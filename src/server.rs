//! Background daemon: owns sessions, agent runs and MCP connections; any number of TUI clients attach over a unix socket.

use crate::agent::{self, Event, Runtime, Steer, Turn};
use crate::proto::{AgentBrief, ModelBrief, Overview, Push, RemoteState, Req, SessionBrief, Snapshot, Target, socket_path};
use crate::session::{self, Session};
use crate::types::{Block, Msg, Role};
use crate::{agents, config};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel}};

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
    steer: Steer,
}

/// The outbound link to theta-server (see `relay.rs`).
#[derive(Default)]
struct RemoteLink {
    task: Option<(tokio::task::JoinHandle<()>, Arc<Notify>)>,
    state: RemoteState,
    url: String,
}

pub(crate) struct Server {
    /// Owner of the state shared by every session: auth, http client, MCP connections.
    base: Runtime,
    /// One runtime per project directory (settings, agents and skills differ per project).
    runtimes: Mutex<HashMap<PathBuf, Runtime>>,
    lives: Mutex<HashMap<String, Arc<Live>>>,
    remote: Mutex<RemoteLink>,
    /// `Push::Remote` updates for local clients.
    remote_bus: broadcast::Sender<Push>,
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
    let srv = Arc::new(Server {
        runtimes: Mutex::new(HashMap::from([(base.cwd.clone(), base.clone())])),
        base,
        lives: Default::default(),
        remote: Default::default(),
        remote_bus: broadcast::channel(16).0,
    });
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
        rt.jobs = Default::default();
        let jobs_sink = bus.clone();
        rt.jobs.on_change(move |l| {
            let _ = jobs_sink.send(Push::Jobs(l));
        });
        let turn = crate::make_turn(&rt, agent, model)?;
        let live = Arc::new(Live {
            session: Arc::new(Mutex::new(session)),
            bus,
            st: Mutex::new(State { turn, rt, run: None, epoch: 0, replay: vec![], handoff: None }),
            steer: Default::default(),
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
        let (req_tx, reqs) = unbounded_channel::<Req>();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(req) = serde_json::from_str::<Req>(&line) else { continue };
                if req_tx.send(req).is_err() {
                    break;
                }
            }
        });
        self.run_connection(reqs, out, Some(quit)).await;
        reader.abort();
        writer.abort();
    }

    /// Serves one client: a local socket (`quit` set, may control the daemon and the remote link) or a
    /// virtual channel of the relay (`quit` unset, already filtered by `relay::allowed`).
    pub(crate) async fn run_connection(self: Arc<Self>, mut reqs: UnboundedReceiver<Req>, out: UnboundedSender<Push>, quit: Option<UnboundedSender<()>>) {
        let local = quit.is_some();
        let mut remote_fwd = local.then(|| {
            let mut rx = self.remote_bus.subscribe();
            let out = out.clone();
            tokio::spawn(async move {
                while let Ok(p) = rx.recv().await {
                    if out.send(p).is_err() {
                        break;
                    }
                }
            })
        });
        let mut current: Option<Arc<Live>> = None;
        let mut forward: Option<tokio::task::JoinHandle<()>> = None;
        while let Some(req) = reqs.recv().await {
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
                Req::SaveTabs { cwd, open } => update_tabs(&cwd, |t| t.open = open),
                Req::Seen => {
                    if let Some(live) = &current {
                        let (cwd, id) = {
                            let s = live.session.lock().unwrap();
                            (s.cwd.clone(), s.id.clone())
                        };
                        update_tabs(&cwd, |t| {
                            t.unseen.remove(&id);
                        });
                    }
                }
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
                    if let Some(q) = &quit {
                        let _ = q.send(());
                    }
                }
                Req::Overview { cwd } => {
                    let (srv, out) = (self.clone(), out.clone());
                    tokio::spawn(async move {
                        let _ = out.send(match srv.overview(cwd).await {
                            Ok(o) => Push::Overview(Box::new(o)),
                            Err(e) => Push::Err(format!("{e:#}")),
                        });
                    });
                }
                Req::RemoteStart | Req::RemoteStop | Req::RemoteStatus if !local => {
                    let _ = out.send(Push::Err("not allowed remotely".into()));
                }
                Req::RemoteStart => self.remote_start(),
                Req::RemoteStop => self.remote_stop(),
                Req::RemoteStatus => {
                    let (state, url) = {
                        let r = self.remote.lock().unwrap();
                        (r.state.clone(), r.url.clone())
                    };
                    let _ = out.send(Push::Remote { state, url });
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
        if let Some(f) = remote_fwd.take() {
            f.abort();
        }
    }

    async fn overview(&self, cwd: Option<PathBuf>) -> Result<Overview> {
        let cwd = cwd.unwrap_or_else(|| self.base.cwd.clone());
        let rt = self.runtime_for(&cwd).await?;
        let running: Vec<String> = self.lives.lock().unwrap().iter().filter(|(_, l)| l.st.lock().unwrap().run.is_some()).map(|(id, _)| id.clone()).collect();
        let dir = cwd.clone();
        let (sessions, projects) = tokio::task::spawn_blocking(move || (session::list(&dir), session::projects())).await?;
        let sessions = sessions
            .into_iter()
            .map(|i| {
                let id = i.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                SessionBrief { running: running.contains(&id), id, title: i.title, updated: i.updated, messages: i.messages }
            })
            .collect();
        let agents = rt.agents.iter().map(|a| AgentBrief { name: a.name.clone(), description: a.description.clone(), scope: a.scope.label().into() }).collect();
        let mut models = Vec::new();
        for p in rt.catalog.providers.iter() {
            let local = p.env.is_empty() && p.oauth.is_none();
            if rt.auth.status(p).await.is_none() && !(local && !p.models.is_empty()) {
                continue;
            }
            models.extend(rt.catalog.models_of(p).into_iter().map(|m| ModelBrief { key: m.key(), provider: p.name.clone(), context: m.context, reasoning: m.reasoning }));
        }
        let efforts = ["low", "medium", "high", "xhigh", "max"].map(String::from).to_vec();
        Ok(Overview { cwd, version: crate::update::VERSION.into(), projects, sessions, agents, models, efforts })
    }

    /// Records the link state and tells the local clients.
    pub(crate) fn set_remote(&self, state: RemoteState) {
        let url = {
            let mut r = self.remote.lock().unwrap();
            if r.state == state {
                return;
            }
            r.state = state.clone();
            r.url.clone()
        };
        let _ = self.remote_bus.send(Push::Remote { state, url });
    }

    fn remote_start(self: &Arc<Self>) {
        let mut r = self.remote.lock().unwrap();
        if r.task.as_ref().is_some_and(|(h, _)| !h.is_finished()) {
            let _ = self.remote_bus.send(Push::Remote { state: r.state.clone(), url: r.url.clone() });
            return;
        }
        let url = std::env::var("THETA_REMOTE_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .or_else(|| config::load(&self.base.project).ok().map(|s| s.remote.url))
            .unwrap_or_else(|| config::RemoteSettings::default().url);
        r.url = url.clone();
        r.state = RemoteState::Connecting;
        let _ = self.remote_bus.send(Push::Remote { state: RemoteState::Connecting, url: url.clone() });
        let stop = Arc::new(Notify::new());
        let task = tokio::spawn(crate::relay::run(self.clone(), url, stop.clone()));
        r.task = Some((task, stop));
    }

    fn remote_stop(&self) {
        let task = self.remote.lock().unwrap().task.take();
        if let Some((handle, stop)) = task {
            stop.notify_one();
            // The task says goodbye to the server and exits; do not wait on a stuck socket.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                handle.abort();
            });
        }
        self.set_remote(RemoteState::Off);
    }
}

static TABS_LOCK: Mutex<()> = Mutex::new(());

/// Read-modify-write of a project's `tabs.json`; the daemon's connections and runs share it.
fn update_tabs(cwd: &Path, f: impl FnOnce(&mut session::TabsFile)) {
    let _g = TABS_LOCK.lock().unwrap();
    let mut t = session::load_tabs(cwd);
    f(&mut t);
    session::save_tabs(cwd, &t);
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
        jobs: st.rt.jobs.list(),
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
        Req::Send(text, images) => {
            let (hooks, cwd) = {
                let st = live.st.lock().unwrap();
                if st.run.is_some() {
                    return err("agent is running");
                }
                (st.rt.settings.hooks.clone(), st.rt.cwd.clone())
            };
            if hooks.pre_message.is_empty() {
                return send(live, text, String::new(), images, out);
            }
            // Hooks are slow and must not hold the state lock: send once they have answered.
            let (live, out) = (live.clone(), out.clone());
            let session = live.session.lock().unwrap().id.clone();
            tokio::spawn(async move {
                match crate::hooks::pre(&hooks, &text, &session, &cwd).await {
                    Ok(extra) => send(&live, text, extra, images, &out),
                    Err(e) => {
                        let _ = out.send(Push::Err(e));
                    }
                }
            });
        }
        Req::JobKill(id) => {
            if !live.st.lock().unwrap().rt.jobs.kill(&id) {
                err(&format!("no running job `{id}`"));
            }
        }
        Req::JobOutput(id) => {
            let found = live.st.lock().unwrap().rt.jobs.output(&id, None);
            if let Some((text, _)) = found {
                let _ = out.send(Push::JobOutput { id, text });
            }
        }
        Req::Steer(text) => {
            if live.st.lock().unwrap().run.is_some() {
                live.steer.lock().unwrap().push_back(text);
            } else {
                handle(live, Req::Send(text, vec![]), out);
            }
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
        Req::Attach { .. } | Req::Reload | Req::McpConnect(_) | Req::McpStatus | Req::Busy | Req::Shutdown | Req::SaveTabs { .. } | Req::Seen => {}
        Req::RemoteStart | Req::RemoteStop | Req::RemoteStatus | Req::Overview { .. } => {}
    }
}

/// Add a user message (plus `extra` context from the pre-message hooks, hidden from the chat) and start the run.
fn send(live: &Arc<Live>, text: String, extra: String, images: Vec<Block>, out: &UnboundedSender<Push>) {
    let mut st = live.st.lock().unwrap();
    if st.run.is_some() {
        let _ = out.send(Push::Err("agent is running".into()));
        return;
    }
    let mut expanded = crate::skills::expand(&st.rt.skills, &text);
    if !extra.is_empty() {
        expanded = format!("{expanded}\n\n{extra}");
    }
    let mut content: Vec<Block> = if expanded.is_empty() { vec![] } else { vec![Block::Text { text: expanded }] };
    content.extend(images);
    let msg = Msg { role: Role::User, content, model: None };
    if let Err(e) = live.session.lock().unwrap().add_msg(msg, None) {
        let _ = out.send(Push::Err(format!("{e:#}")));
        return;
    }
    let _ = live.bus.send(Push::Event(Event::User(text)));
    launch_turn(live, &mut st);
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
    let (rt, session, turn, steer) = (st.rt.clone(), live.session.clone(), st.turn.clone(), live.steer.clone());
    launch(live, st, move |tx| async move {
        if let Err(e) = agent::run(rt, session, turn, tx.clone(), steer).await {
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
        let mut errored = false;
        while let Some(ev) = rx.recv().await {
            let mut st = live.st.lock().unwrap();
            if st.epoch != epoch {
                return;
            }
            errored |= matches!(ev, Event::Error(_));
            // Results are remembered on disk, so a finished run still shows once the client is gone.
            let unread = match &ev {
                Event::Done => Some(if errored { "error" } else { "done" }),
                Event::Ask { .. } => Some("asked"),
                _ => None,
            };
            if let Some(u) = unread {
                let (cwd, id) = {
                    let s = live.session.lock().unwrap();
                    (s.cwd.clone(), s.id.clone())
                };
                update_tabs(&cwd, |t| {
                    t.unseen.insert(id, u.into());
                });
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
                if !st.rt.settings.hooks.post_message.is_empty() {
                    let reply = live.session.lock().unwrap().context().last().map(|m| m.text()).unwrap_or_default();
                    let (hooks, cwd, session) = (st.rt.settings.hooks.clone(), st.rt.cwd.clone(), live.session.lock().unwrap().id.clone());
                    let (bus, status) = (live.bus.clone(), if errored { "error" } else { "done" });
                    tokio::spawn(async move {
                        for e in crate::hooks::post(&hooks, &reply, status, &session, &cwd).await {
                            let _ = bus.send(Push::Notice(e));
                        }
                    });
                }
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
