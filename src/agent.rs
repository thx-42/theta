//! Agent loop: stream a reply, run its tool calls, repeat until the model stops calling tools.

use crate::agents::{self, Agent};
use crate::auth::Auth;
use crate::catalog::{Catalog, Model, Provider};
use crate::compact;
use crate::config::Settings;
use crate::llm::{self, Request};
use crate::session::Session;
use crate::tools::{self, ToolCtx, Todo};
use crate::types::{Block, Delta, Msg, Role, ToolDef, Usage};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Text(String),
    Thinking(String),
    /// Model started emitting a tool call (args not yet known).
    ToolPending(String),
    ToolStart { id: String, name: String, args: Value },
    ToolEnd { id: String, content: String, is_error: bool, display: Option<String> },
    /// One model step finished.
    Step { usage: Usage, model: String },
    Todos(Vec<Todo>),
    Compacting,
    Compacted { before: u64 },
    /// `/btw` answer: shown in the chat, never added to the session.
    Side { question: String, answer: String },
    Title(String),
    /// `ask` tool: the agent waits until someone answers `id` (see `Runtime::asks`).
    Ask { id: String, question: String, options: Vec<String> },
    /// Question `id` was answered (by any attached client).
    Answered(String),
    /// `handoff` accepted: switch to `agent` and continue once this run ends.
    Handoff { agent: String },
    /// Progress from a subagent spawned by tool call `id`.
    Sub { id: String, event: Box<Event> },
    Error(String),
    /// A run (turn, compaction or side question) began.
    Started,
    /// A user message was accepted; shown by every attached client.
    User(String),
    /// The run was aborted on request.
    Interrupted,
    Done,
}

pub type Tx = UnboundedSender<Event>;
/// Pending `ask` questions by id; the answering side removes the sender and sends the text.
pub type Asks = Arc<Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<String>>>>;

/// Everything shared across turns.
#[derive(Clone)]
pub struct Runtime {
    pub catalog: Arc<Catalog>,
    pub auth: Auth,
    pub settings: Arc<Settings>,
    pub http: reqwest::Client,
    pub project: PathBuf,
    pub cwd: PathBuf,
    pub agents: Arc<Vec<Agent>>,
    pub skills: Arc<Vec<crate::skills::Skill>>,
    pub mcp: Arc<crate::mcp::Registry>,
    pub todos: Arc<Mutex<Vec<Todo>>>,
    pub read_cache: Arc<Mutex<std::collections::HashMap<String, u64>>>,
    pub asks: Asks,
}

/// What one run needs: who answers, with which model, and which tools.
#[derive(Clone)]
pub struct Turn {
    pub agent: Agent,
    pub model: String,
    pub effort: String,
    pub depth: u8,
}

impl Runtime {
    pub fn resolve(&self, key: &str) -> Result<(Provider, Model)> {
        self.catalog.resolve(key).with_context(|| format!("unknown model `{key}` (use provider/model)"))
    }

    /// Model for a background task (`compaction`, `title`, `subagent`).
    /// Empty setting, or a provider without credentials, falls back to the main model.
    pub async fn task_model(&self, task: &str, main: &str) -> String {
        let m = match task {
            "compaction" => &self.settings.models.compaction,
            "title" => &self.settings.models.title,
            _ => &self.settings.models.subagent,
        };
        match self.catalog.resolve(m) {
            Some((p, _)) if self.auth.status(&p).await.is_some() || (p.env.is_empty() && p.oauth.is_none()) => m.clone(),
            _ => main.to_string(),
        }
    }

    fn tool_ctx(&self) -> ToolCtx {
        ToolCtx {
            cwd: self.cwd.clone(),
            settings: self.settings.clone(),
            http: self.http.clone(),
            todos: self.todos.clone(),
            read_cache: self.read_cache.clone(),
        }
    }

    pub fn tools_for(&self, agent: &Agent, depth: u8) -> Vec<ToolDef> {
        let mut defs = tools::all_defs();
        if depth == 0 {
            defs.push(tools::task_def(&self.agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>()));
        }
        defs.extend(self.mcp.defs());
        defs.retain(|d| !self.settings.tools.disabled.contains(&d.name));
        match &agent.tools {
            Some(allow) => defs.retain(|d| allow.iter().any(|a| allows(a, &d.name))),
            None => defs.retain(|d| !matches!(d.name.as_str(), "handoff" | "list_agents")), // opt-in: only agents that list it
        }
        if depth > 0 {
            defs.retain(|d| !matches!(d.name.as_str(), "todo" | "ask" | "handoff" | "list_agents")); // the todo panel and the user belong to the main agent
        }
        defs
    }

    /// One-shot completion without tools (titles, summaries).
    pub async fn oneshot(&self, model_key: &str, system: &str, prompt: &str) -> Result<String> {
        let (p, m) = self.resolve(model_key)?;
        let msgs = [Msg::user(prompt)];
        let req = Request { model: &m, system, messages: &msgs, tools: &[], effort: "low", session_id: "oneshot" };
        let r = llm::complete(&self.http, &self.auth, &p, req, &mut |_| {}).await?;
        Ok(r.msg.text().trim().to_string())
    }
}

/// Does an `tools:` entry allow tool `name`? Exact, `prefix*`, or an MCP server name (`mcp__github`).
fn allows(entry: &str, name: &str) -> bool {
    let (e, n) = (entry.to_lowercase(), name.to_lowercase());
    e == n || e.strip_suffix('*').is_some_and(|p| n.starts_with(p)) || (e.starts_with("mcp__") && n.starts_with(&format!("{e}__")))
}

/// Give every dangling tool call a result so the transcript stays valid after an abort.
fn repair(msgs: &mut Vec<Msg>) {
    let mut i = 0;
    while i < msgs.len() {
        if msgs[i].role == Role::Assistant {
            let calls: Vec<String> = msgs[i].tool_calls().into_iter().map(|c| c.0).collect();
            if !calls.is_empty() {
                let answered: Vec<String> = msgs
                    .get(i + 1)
                    .map(|m| m.content.iter().filter_map(|b| if let Block::ToolResult { id, .. } = b { Some(id.clone()) } else { None }).collect())
                    .unwrap_or_default();
                let missing: Vec<Block> = calls
                    .into_iter()
                    .filter(|c| !answered.contains(c))
                    .map(|id| Block::ToolResult { id, content: "[aborted by user]".into(), is_error: true })
                    .collect();
                if !missing.is_empty() {
                    if answered.is_empty() {
                        msgs.insert(i + 1, Msg { role: Role::User, content: missing, model: None });
                    } else {
                        msgs[i + 1].content.splice(0..0, missing);
                    }
                }
            }
        }
        i += 1;
    }
}

fn is_overflow(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("prompt is too long") || s.contains("context_length") || s.contains("maximum context") || s.contains("too many tokens") || s.contains("context window")
}

/// Run until the model answers without tool calls. Appends everything to `session`.
pub async fn run(rt: Runtime, session: Arc<Mutex<Session>>, turn: Turn, tx: Tx) -> Result<()> {
    let (provider, model) = rt.resolve(&turn.model)?;
    let system = agents::system_prompt(&turn.agent, &rt.project, &rt.cwd, &crate::skills::auto_section(&rt.skills));
    let tools = rt.tools_for(&turn.agent, turn.depth);
    let session_id = session.lock().unwrap().id.clone();
    let ctx = rt.tool_ctx();
    let mut overflow_retry = false;
    let mut compact_failed = false;
    loop {
        if rt.settings.compaction.enabled && !compact_failed && compact::needed(&session.lock().unwrap(), &model, &rt.settings) {
            let _ = tx.send(Event::Compacting);
            match compact::run(&rt, &session, &turn.model).await {
                Ok(before) => {
                    let _ = tx.send(Event::Compacted { before });
                }
                Err(e) => {
                    compact_failed = true;
                    let _ = tx.send(Event::Error(format!("compaction failed: {e:#}")));
                }
            }
        }
        let mut msgs = session.lock().unwrap().context();
        repair(&mut msgs);
        let req = Request { model: &model, system: &system, messages: &msgs, tools: &tools, effort: &turn.effort, session_id: &session_id };
        let txd = tx.clone();
        let mut on = move |d: Delta| {
            let _ = txd.send(match d {
                Delta::Text(t) => Event::Text(t),
                Delta::Thinking(t) => Event::Thinking(t),
                Delta::ToolStart { name } => Event::ToolPending(name),
            });
        };
        let resp = match llm::complete(&rt.http, &rt.auth, &provider, req, &mut on).await {
            Ok(r) => r,
            Err(e) if !overflow_retry && is_overflow(&e) => {
                overflow_retry = true;
                let _ = tx.send(Event::Compacting);
                let before = compact::run(&rt, &session, &turn.model).await?;
                let _ = tx.send(Event::Compacted { before });
                continue;
            }
            Err(e) => return Err(e),
        };
        let calls = resp.msg.tool_calls();
        session.lock().unwrap().add_msg(resp.msg.clone(), Some(resp.usage.clone()))?;
        let _ = tx.send(Event::Step { usage: resp.usage.clone(), model: model.key() });
        if calls.is_empty() {
            if matches!(resp.stop.as_str(), "max_tokens" | "length" | "MAX_TOKENS") {
                let _ = tx.send(Event::Error("reply cut off at the output token limit — say \"continue\" to resume".into()));
            }
            break;
        }
        let futs = calls.into_iter().map(|(id, name, args)| {
            let (rt, ctx, tx, turn) = (rt.clone(), ctx.clone(), tx.clone(), turn.clone());
            async move {
                let _ = tx.send(Event::ToolStart { id: id.clone(), name: name.clone(), args: args.clone() });
                let out = if name == "task" {
                    subagent(&rt, &turn, &id, &args, &tx).await.unwrap_or_else(|e| tools::ToolOut::err(format!("{e:#}")))
                } else if name == "ask" {
                    ask(&rt, &args, &tx).await
                } else if name.starts_with("mcp__") {
                    rt.mcp.call(&name, &args).await
                } else if name == "list_agents" {
                    tools::ToolOut::ok(agents::catalog(&rt.agents))
                } else if name == "handoff" {
                    handoff(&rt, &args, &tx).await
                } else {
                    tools::run(&name, &args, &ctx).await
                };
                if name == "todo" {
                    let _ = tx.send(Event::Todos(ctx.todos.lock().unwrap().clone()));
                }
                let _ = tx.send(Event::ToolEnd { id: id.clone(), content: out.content.clone(), is_error: out.is_error, display: out.display });
                Block::ToolResult { id, content: out.content, is_error: out.is_error }
            }
        });
        let results = futures::future::join_all(futs).await;
        session.lock().unwrap().add_msg(Msg { role: Role::User, content: results, model: None }, None)?;
    }
    Ok(())
}

/// `ask` tool: show the question to the user and wait. A free-text answer is always allowed.
async fn ask(rt: &Runtime, args: &Value, tx: &Tx) -> tools::ToolOut {
    let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if question.is_empty() {
        return tools::ToolOut::err("question required");
    }
    let mut options: Vec<String> = args.get("options").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|o| o.as_str().map(String::from)).collect()).unwrap_or_default();
    if options.is_empty() && args.get("type").and_then(|v| v.as_str()) == Some("yes_no") {
        options = vec!["Yes".into(), "No".into()];
    }
    let (rtx, rrx) = tokio::sync::oneshot::channel();
    let id = format!("{:08x}", rand::random::<u32>());
    rt.asks.lock().unwrap().insert(id.clone(), rtx);
    if tx.send(Event::Ask { id: id.clone(), question, options }).is_err() {
        rt.asks.lock().unwrap().remove(&id);
    }
    match rrx.await {
        Ok(a) if a.is_empty() => tools::ToolOut::ok("[the user dismissed the question without answering]"),
        Ok(a) => tools::ToolOut::ok(format!("User answered: {a}")),
        Err(_) => tools::ToolOut::err("no user available (non-interactive run); decide yourself and state your assumption"),
    }
}

/// `handoff` tool: ask the user to switch agents. On yes, the UI switches and starts the new agent.
async fn handoff(rt: &Runtime, args: &Value, tx: &Tx) -> tools::ToolOut {
    let name = args.get("agent").and_then(|v| v.as_str()).unwrap_or("Build");
    let Some(agent) = agents::find(&rt.agents, name) else { return tools::ToolOut::err(format!("unknown agent `{name}`. Available:\n{}", agents::catalog(&rt.agents))) };
    let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    let q = json!({"question": format!("Switch to {} and start: {summary}?", agent.name), "type": "choice", "options": [format!("Yes, switch to {}", agent.name), "No, keep planning"]});
    let out = ask(rt, &q, tx).await;
    if out.content.contains("Yes, switch") {
        let _ = tx.send(Event::Handoff { agent: agent.name.clone() });
        return tools::ToolOut::ok(format!("User accepted. {} takes over after your reply: end your turn now with one short line.", agent.name));
    }
    out
}

/// `task` tool: run another agent in a fresh, unlisted session and return its final answer.
async fn subagent(rt: &Runtime, parent: &Turn, call_id: &str, args: &Value, tx: &Tx) -> Result<tools::ToolOut> {
    let prompt = args.get("prompt").and_then(|v| v.as_str()).context("prompt required")?;
    let named = args.get("agent").is_some();
    let agent = match args.get("agent").and_then(|v| v.as_str()) {
        Some(name) => agents::find(&rt.agents, name).with_context(|| format!("unknown agent `{name}`"))?.clone(),
        None => parent.agent.clone(),
    };
    let model = agents::model_of(&agent, &rt.settings)
        .filter(|_| named)
        .or_else(|| parent.agent.subagent_model.clone())
        .unwrap_or(rt.task_model("subagent", &parent.model).await);
    let effort = agents::effort_of(&agent, &rt.settings).filter(|_| named).unwrap_or_else(|| parent.effort.clone());
    let mut s = Session::new(&rt.cwd);
    s.path = crate::config::home().join("cache/subagents").join(format!("{}.jsonl", s.id));
    s.add_msg(Msg::user(prompt), None)?;
    let session = Arc::new(Mutex::new(s));
    let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel();
    let turn = Turn { agent, model, effort, depth: parent.depth + 1 };
    let fwd_tx = tx.clone();
    let id = call_id.to_string();
    let forward = tokio::spawn(async move {
        while let Some(ev) = srx.recv().await {
            if matches!(ev, Event::Text(_) | Event::Thinking(_)) {
                continue;
            }
            let _ = fwd_tx.send(Event::Sub { id: id.clone(), event: Box::new(ev) });
        }
    });
    let res = Box::pin(run(rt.clone(), session.clone(), turn, stx)).await;
    let _ = forward.await;
    res?;
    let answer = session.lock().unwrap().context().last().map(|m| m.text()).unwrap_or_default();
    Ok(tools::ToolOut::ok(if answer.is_empty() { "(subagent returned no text)".into() } else { answer }))
}

/// Name the session from its first exchange using the title model.
pub async fn make_title(rt: &Runtime, main_model: &str, first_user: &str) -> Result<String> {
    let model = rt.task_model("title", main_model).await;
    let prompt: String = first_user.chars().take(2000).collect();
    let t = rt
        .oneshot(&model, "You name coding sessions. Reply with a 3-6 word title in the user's language. No quotes, no punctuation at the end.", &prompt)
        .await?;
    Ok(t.lines().next().unwrap_or("").trim_matches(['"', '\'', '.', ' ']).chars().take(60).collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_allow_entries() {
        assert!(super::allows("mcp__*", "mcp__gh__x") && super::allows("mcp__gh", "mcp__gh__x") && super::allows("READ", "read"));
        assert!(!super::allows("mcp__g", "mcp__gh__x") && !super::allows("read", "write"));
    }

    use super::*;

    #[test]
    fn repairs_dangling_calls() {
        let mut msgs = vec![
            Msg::user("hi"),
            Msg { role: Role::Assistant, content: vec![Block::ToolCall { id: "a".into(), name: "ls".into(), args: json!({}), sig: None }], model: None },
            Msg::user("next"),
        ];
        repair(&mut msgs);
        assert_eq!(msgs.len(), 4);
        assert!(matches!(&msgs[2].content[0], Block::ToolResult { id, .. } if id == "a"));
    }

    async fn test_rt() -> Runtime {
        Runtime {
            catalog: Arc::new(Catalog::load(&Settings::default()).await),
            auth: Auth::load(),
            settings: Arc::new(Settings::default()),
            http: reqwest::Client::new(),
            project: PathBuf::new(),
            cwd: PathBuf::new(),
            agents: Default::default(),
            skills: Default::default(),
            mcp: Arc::new(crate::mcp::Registry::new(&Default::default())),
            todos: Default::default(),
            read_cache: Default::default(),
            asks: Default::default(),
        }
    }

    #[tokio::test]
    async fn ask_waits_for_answer() {
        let rt = test_rt().await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (rt2, tx2) = (rt.clone(), tx.clone());
        let h = tokio::spawn(async move { ask(&rt2, &json!({"question":"Go?","type":"yes_no"}), &tx2).await });
        let Some(Event::Ask { id, options, .. }) = rx.recv().await else { panic!("no ask event") };
        assert_eq!(options, vec!["Yes", "No"]);
        rt.asks.lock().unwrap().remove(&id).unwrap().send("No".into()).unwrap();
        assert_eq!(h.await.unwrap().content, "User answered: No");
        // Nobody listening: the sender is dropped with the registry entry, so the tool errors instead of hanging.
        drop(rx);
        drop(tx);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let rt3 = rt.clone();
        let h = tokio::spawn(async move { ask(&rt3, &json!({"question":"Go?","type":"text"}), &tx).await });
        assert!(h.await.unwrap().is_error);
    }
}
