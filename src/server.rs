//! Background daemon: owns sessions, agent runs and MCP connections; any number of TUI clients attach over a unix socket.

use crate::agent::{self, Event, Runtime, Turn};
use crate::proto::{Push, Req, Snapshot, Target, socket_path};
use crate::session::{self, Session};
use crate::types::Msg;
use crate::{agents, config};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc::{UnboundedSender, unbounded_channel}};

const BTW_SYSTEM: &str = "Answer the side question using the conversation as context. Be brief. Do not use tools and do not continue the conversation.";

struct State {
    turn: Turn,
    rt: Runtime,
    run: Option<tokio::task::JoinHandle<()>>,
    /// Bumped on every launch and interrupt; a run's event pump stops when it no longer matches.
    epoch: u64,
    /// Events of the step in flight, for clients that attach mid-run.
    replay: Vec<Event>,
    handoff: Option<String>,
}

struct Live {
    session: Arc<Mutex<Session>>,
    bus: broadcast::Sender<Push>,
    st: Mutex<State>,
}

struct Server {
    /// Owner of the state shared by every session: auth, http client, MCP connections.
    base: Runtime,
    /// One runtime per project directory (settings, agents and skills differ per project).
    runtimes: Mutex<HashMap<PathBuf, Runtime>>,
    lives: Mutex<HashMap<String, Arc<Live>>>,
}

pub async fn serve() -> Result<()> {
    let path = socket_path();
    if UnixStream::connect(&path).await.is_ok() {
        return Ok(()); // another daemon is alive
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("cannot bind {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    // The daemon must outlive the terminal that started it.
    if let Ok(mut hup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        tokio::spawn(async move { while hup.recv().await.is_some() {} });
    }
    let base = crate::build_runtime(std::env::current_dir()?).await?;
    let mcp = base.mcp.clone();
    tokio::spawn(async move { mcp.connect_all().await });
    let srv = Arc::new(Server { runtimes: Mutex::new(HashMap::from([(base.cwd.clone(), base.clone())])), base, lives: Default::default() });
    let (quit_tx, mut quit_rx) = unbounded_channel::<()>();
    loop {
        tokio::select! {
            Ok((sock, _)) = listener.accept() => {
                let (srv, quit_tx) = (srv.clone(), quit_tx.clone());
                tokio::spawn(async move { srv.connection(sock, quit_tx).await });
            }
            _ = quit_rx.recv() => break,
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

impl Server {
    async fn runtime_for(&self, cwd: &Path) -> Result<Runtime> {
        if let Some(r) = self.runtimes.lock().unwrap().get(cwd) {
            return Ok(r.clone());
        }
        let mut rt = crate::build_runtime(cwd.to_path_buf()).await?;
        // shortcut: MCP servers come from the daemon's first project, not per project.
        rt.auth = self.base.auth.clone();
        rt.http = self.base.http.clone();
        rt.mcp = self.base.mcp.clone();
        self.runtimes.lock().unwrap().insert(cwd.to_path_buf(), rt.clone());
        Ok(rt)
    }

    async fn reload(&self) {
        self.base.auth.reload().await;
        let all: Vec<(PathBuf, Runtime)> = self.runtimes.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (cwd, mut rt) in all {
            let Ok(settings) = config::load(&rt.project) else { continue };
            rt.settings = Arc::new(settings);
            for live in self.lives.lock().unwrap().values() {
                let mut st = live.st.lock().unwrap();
                if st.rt.cwd == cwd {
                    st.rt.settings = rt.settings.clone();
                }
            }
            self.runtimes.lock().unwrap().insert(cwd, rt);
        }
    }

    async fn attach(&self, target: Target, cwd: &Path, agent: Option<&str>, model: Option<&str>) -> Result<Arc<Live>> {
        let base = self.runtime_for(cwd).await?;
        let opened = match target {
            Target::New => None,
            Target::Continue => session::list(cwd).first().map(|i| i.path.clone()),
            Target::Resume(r) => {
                let p = PathBuf::from(&r);
                Some(if p.exists() { p } else { session::dir_for(cwd).join(format!("{r}.jsonl")) })
            }
        };
        let session = match opened {
            Some(p) => {
                let id = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if let Some(l) = self.lives.lock().unwrap().get(&id) {
                    let l = l.clone();
                    self.retarget(&l, agent, model);
                    return Ok(l);
                }
                Session::open(&p).with_context(|| format!("cannot open session {}", p.display()))?
            }
            None => Session::new(cwd),
        };
        let id = session.id.clone();
        let (bus, _) = broadcast::channel(4096);
        let sink = bus.clone();
        let mut session = session;
        session.set_sink(move |e| {
            let _ = sink.send(Push::Entry(e.clone()));
        });
        let mut rt = base;
        rt.todos = Default::default();
        rt.read_cache = Default::default();
        rt.asks = Default::default();
        let turn = crate::make_turn(&rt, agent, model)?;
        let live = Arc::new(Live {
            session: Arc::new(Mutex::new(session)),
            bus,
            st: Mutex::new(State { turn, rt, run: None, epoch: 0, replay: vec![], handoff: None }),
        });
        self.lives.lock().unwrap().insert(id, live.clone());
        Ok(live)
    }

    fn retarget(&self, live: &Live, agent: Option<&str>, model: Option<&str>) {
        if agent.is_none() && model.is_none() {
            return;
        }
        let mut st = live.st.lock().unwrap();
        if let Some(a) = agent.and_then(|n| agents::find(&st.rt.agents, n).cloned()) {
            switch_agent(&mut st, a);
        }
        if let Some(m) = model {
            st.turn.model = m.to_string();
        }
        let _ = live.bus.send(meta(&st));
    }

    async fn connection(self: Arc<Self>, sock: UnixStream, quit: UnboundedSender<()>) {
        let (rd, mut wr) = sock.into_split();
        let (out, mut out_rx) = unbounded_channel::<Push>();
        let writer = tokio::spawn(async move {
            while let Some(p) = out_rx.recv().await {
                let Ok(mut line) = serde_json::to_string(&p) else { continue };
                line.push('\n');
                if wr.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let mut lines = BufReader::new(rd).lines();
        let mut current: Option<Arc<Live>> = None;
        let mut forward: Option<tokio::task::JoinHandle<()>> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(req) = serde_json::from_str::<Req>(&line) else { continue };
            match req {
                Req::Attach { target, cwd, agent, model } => {
                    if let Some(f) = forward.take() {
                        f.abort();
                    }
                    match self.attach(target, &cwd, agent.as_deref(), model.as_deref()).await {
                        Ok(live) => {
                            forward = Some(subscribe(&live, &out));
                            current = Some(live);
                        }
                        Err(e) => {
                            let _ = out.send(Push::Err(format!("{e:#}")));
                        }
                    }
                }
                Req::Reload => self.reload().await,
                Req::McpConnect(name) => {
                    let mcp = self.base.mcp.clone();
                    tokio::spawn(async move { mcp.connect(&name).await });
                }
                Req::McpStatus => {
                    let l = self.base.mcp.status();
                    let _ = out.send(Push::Notice(if l.is_empty() { "no MCP servers — add [mcp.<name>] to settings.toml".into() } else { l.join("\n") }));
                }
                Req::Busy => {
                    let lives: Vec<Arc<Live>> = self.lives.lock().unwrap().values().cloned().collect();
                    let busy = lives.iter().any(|l| l.st.lock().unwrap().run.is_some());
                    let _ = out.send(Push::Busy(busy));
                }
                Req::Shutdown => {
                    let _ = quit.send(());
                }
                req => match &current {
                    Some(live) => handle(live, req, &out),
                    None => {
                        let _ = out.send(Push::Err("not attached to a session".into()));
                    }
                },
            }
        }
        if let Some(f) = forward {
            f.abort();
        }
        writer.abort();
    }
}

fn meta(st: &State) -> Push {
    Push::Meta { agent: st.turn.agent.name.clone(), model: st.turn.model.clone(), effort: st.turn.effort.clone() }
}

/// Send the snapshot, then stream everything that happens next. Subscribing and snapshotting happen under the
/// state lock, which also guards every event broadcast, so nothing is lost or doubled.
fn subscribe(live: &Arc<Live>, out: &UnboundedSender<Push>) -> tokio::task::JoinHandle<()> {
    let st = live.st.lock().unwrap();
    let mut rx = live.bus.subscribe();
    let s = live.session.lock().unwrap();
    let snap = Snapshot {
        session_id: s.id.clone(),
        entries: s.entries.clone(),
        agent: st.turn.agent.name.clone(),
        model: st.turn.model.clone(),
        effort: st.turn.effort.clone(),
        running: st.run.is_some(),
        replay: st.replay.clone(),
        todos: st.rt.todos.lock().unwrap().clone(),
    };
    drop(s);
    let _ = out.send(Push::Snapshot(Box::new(snap)));
    drop(st);
    let out = out.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(p) => {
                    if out.send(p).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue, // shortcut: a very slow client misses events; reattach to resync
                Err(_) => break,
            }
        }
    })
}

fn handle(live: &Arc<Live>, req: Req, out: &UnboundedSender<Push>) {
    let err = |m: &str| {
        let _ = out.send(Push::Err(m.into()));
    };
    match req {
        Req::Send(text) => {
            let mut st = live.st.lock().unwrap();
            if st.run.is_some() {
                return err("agent is running");
            }
            let expanded = crate::skills::expand(&st.rt.skills, &text);
            if let Err(e) = live.session.lock().unwrap().add_msg(Msg::user(expanded), None) {
                return err(&format!("{e:#}"));
            }
            let _ = live.bus.send(Push::Event(Event::User(text)));
            launch_turn(live, &mut st);
        }
        Req::Interrupt => interrupt(live),
        Req::Answer { id, text } => {
            let mut st = live.st.lock().unwrap();
            let sender = st.rt.asks.lock().unwrap().remove(&id);
            if let Some(tx) = sender {
                let _ = tx.send(text);
            }
            st.replay.retain(|e| !matches!(e, Event::Ask { id: i, .. } if *i == id));
            let _ = live.bus.send(Push::Event(Event::Answered(id)));
        }
        Req::SetModel(m) => {
            let mut st = live.st.lock().unwrap();
            st.turn.model = m;
            let _ = live.bus.send(meta(&st));
        }
        Req::SetEffort(e) => {
            let mut st = live.st.lock().unwrap();
            st.turn.effort = e;
            let _ = live.bus.send(meta(&st));
        }
        Req::SetAgent(name) => {
            let mut st = live.st.lock().unwrap();
            if let Some(a) = agents::find(&st.rt.agents, &name).cloned() {
                switch_agent(&mut st, a);
                let _ = live.bus.send(meta(&st));
            } else {
                err(&format!("unknown agent `{name}`"));
            }
        }
        Req::Compact => {
            let mut st = live.st.lock().unwrap();
            if st.run.is_some() {
                return err("agent is running");
            }
            let (rt, session, model) = (st.rt.clone(), live.session.clone(), st.turn.model.clone());
            launch(live, &mut st, move |tx| async move {
                let _ = tx.send(Event::Compacting);
                match crate::compact::run(&rt, &session, &model).await {
                    Ok(before) => {
                        let _ = tx.send(Event::Compacted { before });
                    }
                    Err(e) => {
                        let _ = tx.send(Event::Error(format!("compaction: {e:#}")));
                    }
                }
            });
        }
        Req::Btw(question) => {
            let mut st = live.st.lock().unwrap();
            if st.run.is_some() {
                return err("agent is running");
            }
            let (rt, session, model) = (st.rt.clone(), live.session.clone(), st.turn.model.clone());
            launch(live, &mut st, move |tx| async move {
                let context = session.lock().unwrap().context();
                let prompt = format!("# Conversation\n{}\n\n# Side question\n{question}", crate::compact::serialize(&context));
                let _ = tx.send(match rt.oneshot(&model, BTW_SYSTEM, &prompt).await {
                    Ok(answer) => Event::Side { question, answer },
                    Err(e) => Event::Error(format!("btw: {e:#}")),
                });
            });
        }
        Req::Title(t) => {
            if live.session.lock().unwrap().set_title(t.clone()).is_ok() {
                let _ = live.bus.send(Push::Event(Event::Title(t)));
            }
        }
        Req::Leaf(target) => {
            if live.st.lock().unwrap().run.is_some() {
                return err("interrupt the agent first");
            }
            let _ = live.session.lock().unwrap().set_leaf(target);
        }
        Req::Attach { .. } | Req::Reload | Req::McpConnect(_) | Req::McpStatus | Req::Busy | Req::Shutdown => {}
    }
}

/// Agent, plus the model and effort that agent implies (same rules the TUI used before the split).
fn switch_agent(st: &mut State, a: agents::Agent) {
    let s = st.rt.settings.clone();
    if let Some(m) = agents::model_of(&a, &s) {
        st.turn.model = m;
    } else if agents::model_of(&st.turn.agent, &s).is_some() {
        st.turn.model = s.model.clone();
    }
    if let Some(e) = agents::effort_of(&a, &s) {
        st.turn.effort = e;
    } else if agents::effort_of(&st.turn.agent, &s).is_some() {
        st.turn.effort = s.effort.clone();
    }
    st.turn.agent = a;
}

fn interrupt(live: &Arc<Live>) {
    let mut st = live.st.lock().unwrap();
    let Some(h) = st.run.take() else { return };
    h.abort();
    st.epoch += 1;
    st.replay.clear();
    st.handoff = None;
    st.rt.asks.lock().unwrap().clear();
    let _ = live.bus.send(Push::Event(Event::Interrupted));
}

fn launch_turn(live: &Arc<Live>, st: &mut State) {
    let (rt, session, turn) = (st.rt.clone(), live.session.clone(), st.turn.clone());
    launch(live, st, move |tx| async move {
        if let Err(e) = agent::run(rt, session, turn, tx.clone()).await {
            let _ = tx.send(Event::Error(format!("{e:#}")));
        }
    });
}

/// Start `work` as the session's run and pump its events to every attached client.
fn launch<F, Fut>(live: &Arc<Live>, st: &mut State, work: F)
where
    F: FnOnce(UnboundedSender<Event>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    st.epoch += 1;
    let epoch = st.epoch;
    let (tx, mut rx) = unbounded_channel();
    let fut = work(tx.clone());
    st.run = Some(tokio::spawn(async move {
        fut.await;
        let _ = tx.send(Event::Done);
    }));
    let _ = live.bus.send(Push::Event(Event::Started));
    let live = live.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let mut st = live.st.lock().unwrap();
            if st.epoch != epoch {
                return;
            }
            match &ev {
                Event::Step { .. } => st.replay.clear(),
                Event::Handoff { agent } => st.handoff = Some(agent.clone()),
                Event::Done => {
                    st.run = None;
                    st.replay.clear();
                }
                _ => remember(&mut st.replay, &ev),
            }
            let _ = live.bus.send(Push::Event(ev.clone()));
            if matches!(ev, Event::Done) {
                after_run(&live, &mut st);
                return;
            }
        }
    });
}

/// Keep streamed text compact: consecutive deltas merge into one event.
fn remember(replay: &mut Vec<Event>, ev: &Event) {
    match (replay.last_mut(), ev) {
        (Some(Event::Text(a)), Event::Text(b)) | (Some(Event::Thinking(a)), Event::Thinking(b)) => a.push_str(b),
        _ => replay.push(ev.clone()),
    }
}

/// After a run: name the session, and carry out an accepted handoff.
fn after_run(live: &Arc<Live>, st: &mut State) {
    let first = {
        let s = live.session.lock().unwrap();
        (s.title.is_none() && s.has_messages()).then(|| s.context().first().map(|m| m.text()).unwrap_or_default())
    };
    if let Some(first) = first {
        // Mark as titled now so we don't spawn twice.
        let _ = live.session.lock().unwrap().set_title(first.chars().take(50).collect());
        let (rt, model, live) = (st.rt.clone(), st.turn.model.clone(), live.clone());
        tokio::spawn(async move {
            if let Ok(t) = agent::make_title(&rt, &model, &first).await
                && !t.is_empty()
                && live.session.lock().unwrap().set_title(t.clone()).is_ok()
            {
                let _ = live.bus.send(Push::Event(Event::Title(t)));
            }
        });
    }
    if let Some(name) = st.handoff.take()
        && let Some(a) = agents::find(&st.rt.agents, &name).cloned()
    {
        switch_agent(st, a);
        let _ = live.bus.send(meta(st));
        let text = "Plan approved. Implement it now, following the todo list.";
        if live.session.lock().unwrap().add_msg(Msg::user(text), None).is_ok() {
            let _ = live.bus.send(Push::Event(Event::User(text.into())));
            launch_turn(live, st);
        }
    }
}
