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
use std::collections::VecDeque;
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
    /// A steered message reached the agent (it was appended to the session at its next step).
    Steer(String),
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
    pub jobs: crate::jobs::Jobs,
    pub ssh: crate::ssh::Slot,
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

    fn tool_ctx(&self, session_id: &str) -> ToolCtx {
        ToolCtx {
            cwd: self.cwd.clone(),
            settings: self.settings.clone(),
            http: self.http.clone(),
            todos: self.todos.clone(),
            read_cache: self.read_cache.clone(),
            session_id: session_id.to_string(),
            jobs: self.jobs.clone(),
            ssh: self.ssh.clone(),
        }
    }

    pub fn tools_for(&self, agent: &Agent, depth: u8) -> Vec<ToolDef> {
        let mut defs = tools::all_defs();
        if depth == 0 {
            defs.push(tools::task_def(&self.agents.iter().filter(|a| agents::can_spawn(agent, a)).map(|a| a.name.clone()).collect::<Vec<_>>()));
        }
        defs.extend(self.mcp.defs());
        defs.retain(|d| !self.settings.tools.disabled.contains(&d.name));
        match &agent.tools {
            Some(allow) => defs.retain(|d| allow.iter().any(|a| allows(a, &d.name))),
            // No list: the lean default set. Opt-in tools and, for subagents, MCP tools (their schemas are costly) need an explicit `tools:` entry.
            None => defs.retain(|d| !tools::OPT_IN.contains(&d.name.as_str()) && !(depth > 0 && d.name.starts_with("mcp__"))),
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
    let mut e = entry.to_lowercase();
    if matches!(e.as_str(), "jobs" | "wait" | "job_output" | "job_kill") {
        e = "job".into(); // the four job tools were merged into `job`
    }
    let n = name.to_lowercase();
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

/// Messages the user sent while a run was active; the run moves them into the session at its next safe point.
pub type Steer = Arc<Mutex<VecDeque<String>>>;

/// Append steered messages and the results of background jobs that ended, as user turns. Steers are announced as `Event::Steer`, job results as `Event::User`. Returns true when there were any.
/// Jobs belong to the main agent: subagents leave their messages alone.
fn drain_steer(steer: &Steer, session: &Arc<Mutex<Session>>, tx: &Tx, rt: &Runtime, depth: u8) -> bool {
    let steered: Vec<String> = steer.lock().unwrap().drain(..).collect();
    let jobs: Vec<String> = if depth == 0 { rt.jobs.take_pending() } else { Vec::new() };
    for t in &steered {
        let _ = session.lock().unwrap().add_msg(Msg::user(crate::skills::expand(&rt.skills, t)), None);
        let _ = tx.send(Event::Steer(t.clone()));
    }
    for t in &jobs {
        let _ = session.lock().unwrap().add_msg(Msg::user(crate::skills::expand(&rt.skills, t)), None);
        let _ = tx.send(Event::User(t.clone()));
    }
    !(steered.is_empty() && jobs.is_empty())
}

/// Appended to every subagent's system prompt: the orchestrator reads nothing but the last message.
/// Hard cap (chars) on a subagent report; reviewers get room for a defect list.
const REPORT_CAP: usize = 6000;

const REPORT_RULE: &str = "# Report\nYou are a subagent. Your last message is your report; the orchestrator reads nothing else. Keep it under 600 characters unless the task prompt asks for another report size, terse: what you did, files touched (path:line), verification (command and result), open issues. Never paste code or logs; an investigator answers each question asked, with `path:line` evidence. A reviewer lists each defect on one line as `path:line problem`.";

/// Run until the model answers without tool calls. Appends everything to `session`.
pub async fn run(rt: Runtime, session: Arc<Mutex<Session>>, turn: Turn, tx: Tx, steer: Steer) -> Result<()> {
    let (provider, model) = rt.resolve(&turn.model)?;
    let extra = if turn.depth > 0 { rt.settings.skills.subagent.clone() } else { vec![] };
    let mut system = agents::system_prompt(&turn.agent, &rt.project, &rt.cwd, &crate::skills::auto_section(&rt.skills, &extra), turn.depth == 0);
    if turn.depth > 0 {
        system.push_str("\n\n");
        system.push_str(REPORT_RULE);
    }
    if let Some(h) = rt.ssh.get() {
        system.push_str("\n\n");
        system.push_str(&crate::ssh::prompt_note(&h));
    }
    let tools = rt.tools_for(&turn.agent, turn.depth);
    let session_id = session.lock().unwrap().id.clone();
    let ctx = rt.tool_ctx(&session_id);
    let mut overflow_retry = false;
    let mut nudges = 0u8;
    let mut stalls = 0u8;
    let mut compact_failed = false;
    loop {
        drain_steer(&steer, &session, &tx, &rt, turn.depth);
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
        // Nothing said, or a turn that ends on "...:" (an announced action never taken).
        let empty = resp.msg.content.iter().all(|b| matches!(b, Block::Thinking { .. })) || (calls.is_empty() && resp.msg.text().trim_end().ends_with(':'));
        if !resp.msg.content.is_empty() {
            session.lock().unwrap().add_msg(resp.msg.clone(), Some(resp.usage.clone()))?;
        }
        let _ = tx.send(Event::Step { usage: resp.usage.clone(), model: model.key() });
        if calls.is_empty() {
            if drain_steer(&steer, &session, &tx, &rt, turn.depth) {
                continue;
            }
            // Background subagents still running: sleep until one ends, then read its report (main agent only).
            if turn.depth == 0 {
                let mut woke = false;
                while rt.jobs.running_agents() > 0 {
                    let start = rt.jobs.ended_count();
                    // Short ticks: a message the user sends meanwhile is read at once, not at the next job end.
                    tokio::select! {
                        _ = rt.jobs.wait_since(false, start) => {}
                        _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {}
                    }
                    if drain_steer(&steer, &session, &tx, &rt, turn.depth) {
                        woke = true;
                        break;
                    }
                }
                if woke {
                    continue;
                }
            }
            // A turn that only announced its plan (or said nothing) is not an answer: push the model to act.
            if (resp.stop == "commentary" || empty) && stalls < 3 {
                stalls += 1;
                session.lock().unwrap().add_msg(Msg::user("You stopped without calling a tool or giving a final answer. Continue the task now: call the tools you need, then give your final report."), None)?;
                continue;
            }
            // Auto must finish its plan: unfinished todos send it back to work (3 nudges, then let it stop).
            if turn.depth == 0 && turn.agent.delegate_all && nudges < 3 && ctx.todos.lock().unwrap().iter().any(|t| t.status != "done") {
                nudges += 1;
                session.lock().unwrap().add_msg(Msg::user("The todo list still has unfinished steps. Continue until every step is done and verified. If you are truly blocked on a user decision, call `ask`."), None)?;
                continue;
            }
            if matches!(resp.stop.as_str(), "max_tokens" | "length" | "MAX_TOKENS") {
                let _ = tx.send(Event::Error("reply cut off at the output token limit — say \"continue\" to resume".into()));
            }
            if turn.depth == 0 {
                crate::lint::flush(&ctx);
            }
            break;
        }
        // Parallel subagents of one agent share a prompt prefix: let the first one write the cache, the others then read it.
        let mut seen_tasks: Vec<String> = Vec::new();
        let staggered: Vec<bool> = calls
            .iter()
            .map(|(_, name, args)| {
                if name != "task" {
                    return false;
                }
                let who = args.get("agent").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let again = seen_tasks.contains(&who);
                seen_tasks.push(who);
                again
            })
            .collect();
        let futs = calls.into_iter().zip(staggered).map(|((id, name, args), stagger)| {
            let (rt, ctx, tx, turn, steer) = (rt.clone(), ctx.clone(), tx.clone(), turn.clone(), steer.clone());
            async move {
                if stagger {
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                }
                let _ = tx.send(Event::ToolStart { id: id.clone(), name: name.clone(), args: args.clone() });
                let mut out = if name == "job" && args.get("action").and_then(|v| v.as_str()) == Some("wait") {
                    let pending = || !steer.lock().unwrap().is_empty();
                    tools::wait(&args, &ctx, &pending).await.unwrap_or_else(|e| tools::ToolOut::err(format!("{e:#}")))
                } else if name == "task" && args.get("background").and_then(|v| v.as_bool()).unwrap_or(false) {
                    background_subagent(&rt, &turn, &id, &args, &tx)
                } else if name == "task" {
                    subagent(&rt, &turn, &id, &args, &tx, None).await.unwrap_or_else(|e| tools::ToolOut::err(format!("{e:#}")))
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
                // The orchestrator's context is the expensive one: its own reads stay small, big ones go to a subagent.
                let cap = rt.settings.tools.auto_max_lines;
                if cap > 0 && turn.depth == 0 && turn.agent.delegate_all && matches!(name.as_str(), "read" | "grep" | "find" | "ls" | "bash") {
                    let (t, cut) = tools::truncate_middle(&out.content, cap);
                    if cut {
                        out.content = format!("{t}\n[cut to {cap} lines: you are the orchestrator — have a subagent read or run larger things and report conclusions]");
                    }
                }
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

/// `task` with `background`: run the subagent as a job and return its id at once; the report comes back as a message.
fn background_subagent(rt: &Runtime, parent: &Turn, call_id: &str, args: &Value, tx: &Tx) -> tools::ToolOut {
    let label = args.get("description").and_then(|v| v.as_str()).unwrap_or("subagent").to_string();
    let (rt2, parent, call_id, args, tx) = (rt.clone(), parent.clone(), call_id.to_string(), args.clone(), tx.clone());
    let id = rt.jobs.spawn(crate::jobs::Kind::Agent, label.clone(), false, move |out| {
        Box::pin(async move {
            match subagent(&rt2, &parent, &call_id, &args, &tx, Some(&out)).await {
                Ok(r) => {
                    out.push(&r.content);
                    i32::from(r.is_error)
                }
                Err(e) => {
                    out.push(&format!("{e:#}"));
                    1
                }
            }
        })
    });
    tools::ToolOut::ok(format!("started background subagent {id}: {label}\nIts report arrives in a later message; use `job` (output, kill) to look at it or stop it."))
}

/// stdout of `git <args>` in `cwd`; None outside a repository or on failure.
async fn git(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let o = tokio::process::Command::new("git").args(args).current_dir(cwd).output().await.ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Fingerprint of the working tree (git status + diff size).
async fn tree_state(cwd: &std::path::Path) -> Option<String> {
    Some(git(cwd, &["status", "--porcelain"]).await? + &git(cwd, &["diff", "HEAD", "--shortstat"]).await.unwrap_or_default())
}

/// The check the harness runs itself after a subagent changed files: setting, else detected from the project.
fn verify_command(settings: &crate::config::Settings, cwd: &std::path::Path) -> Option<String> {
    match settings.tools.verify.trim() {
        "off" => None,
        "" if cwd.join("Cargo.toml").exists() => Some("cargo test --quiet".into()),
        "" if cwd.join("go.mod").exists() => Some("go test ./...".into()),
        "" => None,
        c => Some(c.into()),
    }
}

/// Run `cmd` and summarize: "ok", or the failure with the tail of its output. The agent's own word is not proof.
async fn verify(cwd: &std::path::Path, cmd: &str, timeout: u64) -> String {
    let out = crate::jobs::Out::default();
    let code = tools::shell::run_into(cmd, cwd, &out, Some(std::time::Duration::from_secs(timeout))).await;
    if code == 0 {
        return format!("[verify `{cmd}`: ok]");
    }
    let text = out.text();
    let tail: Vec<&str> = text.lines().rev().take(30).collect();
    format!("[verify `{cmd}`: FAILED (exit {code})]\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

/// `task` tool: run another agent in a fresh, unlisted session and return its final answer.
/// `log` (background jobs) receives one line per tool call the subagent makes.
async fn subagent(rt: &Runtime, parent: &Turn, call_id: &str, args: &Value, tx: &Tx, log: Option<&crate::jobs::Out>) -> Result<tools::ToolOut> {
    let prompt = args.get("prompt").and_then(|v| v.as_str()).context("prompt required")?;
    anyhow::ensure!(parent.depth == 0, "subagents cannot spawn subagents");
    let named = args.get("agent").is_some();
    let agent = match args.get("agent").and_then(|v| v.as_str()) {
        Some(name) => {
            let a = agents::find(&rt.agents, name).with_context(|| format!("unknown agent `{name}`"))?;
            anyhow::ensure!(agents::can_spawn(&parent.agent, a), "{} is read-only: it can only spawn read-only agents", parent.agent.name);
            a.clone()
        }
        None => parent.agent.clone(),
    };
    // `model: "main"` escalates to the orchestrator's own model; an unknown name falls back to the defaults.
    let asked = args.get("model").and_then(|v| v.as_str()).and_then(|m| if m == "main" { Some(parent.model.clone()) } else { rt.resolve(m).ok().map(|_| m.to_string()) });
    let model = match asked {
        Some(m) => m,
        None => agents::model_of(&agent, &rt.settings)
            .filter(|_| named)
            .or_else(|| parent.agent.subagent_model.clone())
            .unwrap_or(rt.task_model("subagent", &parent.model).await),
    };
    // `effort` argument > named agent's effort > parent's `subagent_effort` > [models] subagent_effort > parent's effort
    let effort = args.get("effort").and_then(|v| v.as_str()).filter(|e| matches!(*e, "low" | "medium" | "high" | "xhigh" | "max")).map(String::from)
        .or_else(|| agents::effort_of(&agent, &rt.settings).filter(|_| named))
        .or_else(|| parent.agent.subagent_effort.clone())
        .or_else(|| Some(rt.settings.models.subagent_effort.clone()).filter(|e| !e.is_empty()))
        .unwrap_or_else(|| parent.effort.clone());
    let dir = crate::config::home().join("cache/subagents");
    let mut s = match args.get("resume").and_then(|v| v.as_str()) {
        // Only transcripts of this cache: the stem of the given path or id, never an arbitrary file.
        Some(r) => {
            let stem = std::path::Path::new(r).file_stem().and_then(|s| s.to_str()).unwrap_or("");
            Session::open(&dir.join(format!("{stem}.jsonl"))).with_context(|| format!("no subagent transcript `{r}`"))?
        }
        None => {
            let mut s = Session::new(&rt.cwd);
            s.path = dir.join(format!("{}.jsonl", s.id));
            s
        }
    };
    s.add_msg(Msg::user(prompt), None)?;
    let transcript = s.path.display().to_string();
    let session = Arc::new(Mutex::new(s));
    let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel();
    let turn = Turn { agent, model, effort, depth: parent.depth + 1 };
    let fwd_tx = tx.clone();
    let id = call_id.to_string();
    let log = log.cloned();
    let forward = tokio::spawn(async move {
        while let Some(ev) = srx.recv().await {
            if matches!(ev, Event::Text(_) | Event::Thinking(_)) {
                continue;
            }
            if let (Some(log), Event::ToolStart { name, .. }) = (&log, &ev) {
                log.push(&format!("→ {name}\n"));
            }
            let _ = fwd_tx.send(Event::Sub { id: id.clone(), event: Box::new(ev) });
        }
    });
    // A subagent that can write is checked by the harness, not by its own report.
    let writes = !agents::is_readonly(&turn.agent);
    let before = if writes { tree_state(&rt.cwd).await } else { None };
    let res = Box::pin(run(rt.clone(), session.clone(), turn, stx, Steer::default())).await;
    let _ = forward.await;
    res?;
    let answer = session.lock().unwrap().context().last().map(|m| m.text()).unwrap_or_default();
    // The 600-char rule is only a prompt: cap what actually enters the orchestrator's context.
    let answer = match answer.char_indices().nth(REPORT_CAP) {
        _ if answer.is_empty() => "(subagent returned no text)".into(),
        Some((i, _)) => format!("{}… [report cut, see transcript]", &answer[..i]),
        None => answer,
    };
    let mut checks = String::new();
    if let Some(before) = before
        && tree_state(&rt.cwd).await.is_some_and(|after| after != before) {
            if let Some(cmd) = verify_command(&rt.settings, &rt.cwd) {
                checks = format!("\n{}", verify(&rt.cwd, &cmd, rt.settings.tools.bash_timeout.max(600)).await);
            }
            if let Some(stat) = git(&rt.cwd, &["diff", "HEAD", "--shortstat"]).await.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
                checks.push_str(&format!("\n[changed since HEAD: {stat}]"));
            }
        }
    Ok(tools::ToolOut::ok(format!("{answer}{checks}\n[transcript: {transcript}]")))
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
    fn old_job_tool_names_allow_job() {
        assert!(allows("wait", "job") && allows("job_output", "job") && !allows("wait", "bash"));
    }

    #[tokio::test]
    async fn default_tool_set_is_lean() {
        let rt = test_rt().await;
        let build = agents::find(&agents::discover(std::path::Path::new("/nonexistent")), "Build").unwrap().clone();
        let names: Vec<_> = rt.tools_for(&build, 0).into_iter().map(|d| d.name).collect();
        assert!(names.iter().all(|n| !tools::OPT_IN.contains(&n.as_str())), "{names:?}");
        assert!(names.contains(&"bash".to_string()) && names.contains(&"job".to_string()));
    }

    #[tokio::test]
    async fn verify_reports_ok_and_failure_tail() {
        let dir = std::env::temp_dir();
        assert_eq!(verify(&dir, "true", 10).await, "[verify `true`: ok]");
        let bad = verify(&dir, "echo boom; exit 3", 10).await;
        assert!(bad.contains("FAILED (exit 3)") && bad.contains("boom"), "{bad}");
        let mut st = crate::config::Settings::default();
        assert_eq!(verify_command(&st, &dir), None);
        st.tools.verify = "make check".into();
        assert_eq!(verify_command(&st, &dir).as_deref(), Some("make check"));
        st.tools.verify = "off".into();
        assert_eq!(verify_command(&st, std::path::Path::new(env!("CARGO_MANIFEST_DIR"))), None);
    }

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
            jobs: Default::default(),
            ssh: Default::default(),
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
