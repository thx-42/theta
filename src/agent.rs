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
    /// The model request failed and is sent again in `wait` seconds; the text streamed for this step is void.
    Retry { attempt: u32, wait: u64, reason: String },
    /// `/btw` answer: shown in the chat, never added to the session.
    Side { question: String, answer: String },
    Title(String),
    /// `ask` tool: the agent waits until someone answers `id` (see `Runtime::asks`).
    Ask { id: String, question: String, options: Vec<String> },
    /// Question `id` was answered (by any attached client).
    Answered(String),
    /// `handoff` accepted: switch to `agent` and continue once this run ends.
    Handoff {
        agent: String,
        /// The message the next agent starts with (None: "Plan approved…").
        #[serde(default)]
        message: Option<String>,
    },
    /// Progress from a subagent spawned by tool call `id`.
    Sub { id: String, event: Box<Event> },
    /// Transcript file of the subagent started by tool call `id` (sent inside `Sub`).
    Transcript(String),
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

/// Plan rounds an agent may start with `enter_plan` per user message.
pub const PLAN_ROUNDS: u8 = 3;

/// `enter_plan` state of a session: who Plan hands back to, and how many rounds ran since the last user message.
#[derive(Default, Debug)]
pub struct PlanState {
    pub return_to: Option<String>,
    pub rounds: u8,
}

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
    pub plan: Arc<Mutex<PlanState>>,
    pub gate: Arc<Gate>,
}

/// Keeps the harness check honest when several writing subagents run side by side: one check at a time,
/// and none while another writer is still editing the tree.
#[derive(Default)]
pub struct Gate {
    writers: std::sync::atomic::AtomicUsize,
    verify: tokio::sync::Mutex<()>,
}

/// One writing subagent at work; leaving (also on error or abort) is counted on drop.
struct Writer(Arc<Gate>);

impl Writer {
    fn enter(gate: &Arc<Gate>) -> Writer {
        gate.writers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Writer(gate.clone())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.0.writers.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
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
        if self.usable(m).await { m.clone() } else { main.to_string() }
    }

    /// A known model whose provider has credentials (or needs none).
    pub async fn usable(&self, key: &str) -> bool {
        match self.catalog.resolve(key) {
            Some((p, _)) => self.auth.status(&p).await.is_some() || (p.env.is_empty() && p.oauth.is_none()),
            None => false,
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
        // Orchestrators (`delegate: all`) are not offered as subagents: they could not delegate from there.
        // One level of nesting, reading only: a read-only subagent (a Plan working on one sub-task) may start read-only agents (Explore), nothing deeper.
        if depth == 0 || (depth == 1 && agents::is_readonly(agent)) {
            let spawnable: Vec<_> = self
                .agents
                .iter()
                .filter(|a| agents::can_spawn(agent, a) && !a.delegate_all && (depth == 0 || agents::is_readonly(a)))
                .map(|a| (a.name.clone(), a.description.clone()))
                .collect();
            if depth == 0 || !spawnable.is_empty() {
                defs.push(tools::task_def(&spawnable));
            }
        }
        defs.extend(self.mcp.defs());
        defs.retain(|d| !self.settings.tools.disabled.contains(&d.name));
        match &agent.tools {
            Some(allow) => defs.retain(|d| allow.iter().any(|a| allows(a, &d.name))),
            // No list: the lean default set. Opt-in tools and, for subagents, MCP tools (their schemas are costly) need an explicit `tools:` entry.
            None => defs.retain(|d| !tools::OPT_IN.contains(&d.name.as_str()) && !(depth > 0 && d.name.starts_with("mcp__"))),
        }
        if depth > 0 {
            defs.retain(|d| !matches!(d.name.as_str(), "ask" | "handoff" | "list_agents" | "enter_plan")); // the user belongs to the main agent; a subagent keeps its own todo list, which the orchestrator reads with `job`
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

/// Subagents (often a cheaper model) drift into workarounds when blocked; the orchestrator can unblock them, they cannot.
const WORK_RULE: &str = "# Work\nYour job is the step in the brief, nothing around it. If a layer the brief needs is missing, list it in the report instead of building it. Verify once and fix at most twice, then report what still fails under `deviations:`. Every tool call must move that step forward: no docs, refactors, cleanup, reviews of unrelated files or git unless the brief asks.\nWhen something stands in the way (a refused command, a missing tool or binary, missing access, credentials or a service, a check you cannot fix in two tries), do not work around it:\n- never build a replacement for a missing tool (your own parser, linter, interpreter binding, test harness, mock runtime);\n- never redo a refused command another way (python, awk, another shell command): use the tool the refusal names;\n- never search for passwords, keys or connection strings, and never change a live database or service the brief did not name (read-only MCP probes are fine);\n- never install packages unless the brief says so.\nInstead: finish what you can, mark the blocked check `not verified` with the reason, and report it as a blocker, or stop and ask (`questions:`). A blocker reported in one line is worth more to the orchestrator than an hour of workaround.";

const REPORT_RULE: &str = "# Report\nYou are a subagent. Your last message is your report; the orchestrator reads nothing else. Keep it under {budget} characters unless the task prompt asks for another report size (a checkpoint handoff report may use up to 3000). Keep your `todo` list current (one item per step, edge cases included): the orchestrator reads it while you run. Terse: what you did, files touched (path:line), verification (command and result), open issues. List every requirement of the brief you did not implement exactly as written under `deviations:` (what you did instead, why); never present a substitute as the requested behaviour. Never paste code or logs; an investigator answers each question asked, with `path:line` evidence. A reviewer lists only the defects, one line each as `path:line problem` (max 15 lines, nothing about what is fine); no defect: `no defects`.\n\n# Questions to the orchestrator\nThe orchestrator wrote your brief and can answer you; it resumes you with your context intact. Before your first change, check the brief against the code. If something you need is missing, ambiguous or contradicts the code (a name, a file, a shape, a requirement), and the code cannot settle it, stop: do not guess and do not build a substitute. The same applies later when the work reveals a conflict with the brief. Your report then starts with `questions:`, one numbered question per line. Each question gives what you found (`path:line`), the options you see, and the one you recommend. After the questions, add what you already did, if anything.";

/// Auto ends a turn that changed files without having read the diff: the strongest model reviews before the user does.
const REVIEW_MSG: &str = "Files changed during this turn and you have not read the diff since your last change. Before your final answer, read it (`diff` tool, new files included; `git diff` if you lack it) as the user's most demanding reviewer: bugs and unhandled failure paths; missing junctions (is everything this change must be wired to actually wired, does it work from the user's entry point?); leftovers (debug code, stale callers, docs, tests). Fix what you find, verify again, then give your final answer.";

const CHECKPOINT_MSG: &str = "Checkpoint: stop calling tools now. Your next message is a handoff report for the orchestrator (up to 3000 characters): what is done, what is not done, files touched (path:line), what was verified and how, blockers, and the exact next step. Do not continue the work in this message.";

/// Run until the model answers without tool calls. Appends everything to `session`.
pub async fn run(rt: Runtime, session: Arc<Mutex<Session>>, turn: Turn, tx: Tx, steer: Steer) -> Result<()> {
    let (provider, model) = rt.resolve(&turn.model)?;
    let extra = if turn.depth > 0 { rt.settings.skills.subagent.clone() } else { vec![] };
    let tools = rt.tools_for(&turn.agent, turn.depth);
    let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    // Facts the agent would otherwise spend a turn finding out. Local only: over ssh the project is elsewhere.
    let mut env = Vec::new();
    if rt.ssh.get().is_none() {
        if let Some(b) = git(&rt.cwd, &["branch", "--show-current"]).await {
            env.push(format!("git branch: {}", if b.trim().is_empty() { "(detached HEAD)" } else { b.trim() }));
        }
        if let Some(c) = verify_command(&rt.settings, &rt.cwd).filter(|_| names.iter().any(|n| n == "bash")) {
            env.push(format!("project check: `{c}`"));
        }
    }
    let mut system = agents::system_prompt(&turn.agent, &rt.project, &rt.cwd, &crate::skills::auto_section(&rt.skills, &extra), turn.depth == 0, &names, &env);
    if turn.depth > 0 {
        system.push_str("\n\n");
        system.push_str(WORK_RULE);
        system.push_str("\n\n");
        // A searcher or planner hands back findings the orchestrator would otherwise look up again; a builder's diff speaks for itself.
        let budget = if agents::is_readonly(&turn.agent) { "2000" } else { "800" };
        system.push_str(&REPORT_RULE.replace("{budget}", budget));
    }
    if let Some(h) = rt.ssh.get() {
        system.push_str("\n\n");
        system.push_str(&crate::ssh::prompt_note(&h));
    }
    let session_id = session.lock().unwrap().id.clone();
    let ctx = rt.tool_ctx(&session_id);
    let mut overflow_retry = false;
    let mut nudges = 0u8;
    let mut stalls = 0u8;
    let mut steps = 0usize;
    let mut checkpoints = 0usize;
    let mut compact_failed = false;
    // A plan-mode switch is pending: the run must end so Plan takes over.
    let mut handed_off = false;
    // Auto reviews its own diff: remember the tree it started from, and whether it looked at the diff since its last change.
    let tree0 = if turn.depth == 0 && turn.agent.delegate_all { tree_state(&rt.cwd).await } else { None };
    let mut diff_read = false;
    let mut reviews = 0u8;
    // Tree at the last diff read or review nudge: a change made by `bash` after it (a formatter, `sed -i`) is unread too.
    let mut tree_read: Option<String> = None;
    loop {
        drain_steer(&steer, &session, &tx, &rt, turn.depth);
        // A subagent that runs long hands its state back so the orchestrator can re-steer (resume or re-split).
        let cp = rt.settings.tools.checkpoint_steps;
        if turn.depth > 0 && cp > 0 && steps / cp > checkpoints {
            checkpoints = steps / cp;
            session.lock().unwrap().add_msg(Msg::user(CHECKPOINT_MSG), None)?;
        }
        if rt.settings.compaction.enabled && compact::prune_needed(&session.lock().unwrap(), &rt.settings) {
            compact::prune(&rt, &session)?;
        }
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
                Delta::Retry { attempt, wait, reason } => Event::Retry { attempt, wait, reason },
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
        steps += 1;
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
            if !handed_off && (resp.stop == "commentary" || empty) && stalls < 3 {
                stalls += 1;
                session.lock().unwrap().add_msg(Msg::user("You stopped without calling a tool or giving a final answer. Continue the task now: call the tools you need, then give your final report."), None)?;
                continue;
            }
            // Auto must finish its plan: unfinished todos send it back to work (3 nudges, then let it stop).
            if turn.depth == 0 && turn.agent.delegate_all && !handed_off && usize::from(nudges) < rt.settings.tools.auto_nudges && ctx.todos.lock().unwrap().iter().any(|t| t.status != "done") {
                nudges += 1;
                session.lock().unwrap().add_msg(Msg::user("The todo list still has unfinished steps. Continue until every step is done and verified. If you are truly blocked on a user decision, call `ask`."), None)?;
                continue;
            }
            // Not when Plan is about to take over: the run must end there.
            // Asked again when files change after a review (a fix is a change too), three times per turn at most.
            if !handed_off && reviews < 3 && let Some(t0) = &tree0 {
                let now = tree_state(&rt.cwd).await;
                if now.as_ref().is_some_and(|t| t != t0) && (!diff_read || now != tree_read) {
                    reviews += 1;
                    diff_read = true;
                    tree_read = now;
                    session.lock().unwrap().add_msg(Msg::user(REVIEW_MSG), None)?;
                    continue;
                }
            }
            if matches!(resp.stop.as_str(), "max_tokens" | "length" | "MAX_TOKENS") {
                let _ = tx.send(Event::Error("reply cut off at the output token limit — say \"continue\" to resume".into()));
            }
            if turn.depth == 0 {
                crate::lint::flush(&ctx);
            }
            break;
        }
        let mut read_now = false;
        for (_, name, args) in &calls {
            match name.as_str() {
                "edit" | "write" | "task" => diff_read = false,
                "bash" if args.get("command").and_then(|v| v.as_str()).is_some_and(|c| c.contains("git diff")) => (diff_read, read_now) = (true, true),
                // A summary is not a read.
                "diff" if !args.get("stat").and_then(|v| v.as_bool()).unwrap_or(false) => (diff_read, read_now) = (true, true),
                _ => {}
            }
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
        let names: Vec<String> = calls.iter().map(|c| c.1.clone()).collect();
        let run_call = |((id, name, mut args), stagger): ((String, String, Value), bool)| {
            let (rt, ctx, tx, turn, steer) = (rt.clone(), ctx.clone(), tx.clone(), turn.clone(), steer.clone());
            async move {
                // The orchestrator's context is the expensive one: its own reads stay small, big ones go to a subagent.
                // A `read` is capped by its `limit`, so the result ends on the `offset=` hint and no part of it is lost.
                let cap = if turn.depth == 0 && turn.agent.delegate_all { rt.settings.tools.auto_max_lines } else { 0 };
                if cap > 0 && name == "read" && args.is_object() {
                    let limit = args.get("limit").and_then(|v| v.as_u64()).map_or(cap as u64, |l| l.min(cap as u64));
                    args["limit"] = json!(limit);
                }
                if stagger {
                    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                }
                let _ = tx.send(Event::ToolStart { id: id.clone(), name: name.clone(), args: args.clone() });
                let mut out = if name == "job" && args.get("action").and_then(|v| v.as_str()) == Some("wait") {
                    let pending = || !steer.lock().unwrap().is_empty();
                    tools::wait(&args, &ctx, &pending).await.unwrap_or_else(|e| tools::ToolOut::err(format!("{e:#}")))
                } else if let Some(open) = (name == "task" && turn.depth == 0 && writes_files(&rt, &turn, &args)).then(|| tools::fs::open_questions(&ctx.session_id)).filter(|q| !q.is_empty()) {
                    // Building on a guess costs more round trips than settling it first.
                    tools::ToolOut::err(format!("The plan still has open questions:\n{}\nSettle each one first (an Explore with that exact question, an MCP test, or `ask` for a user decision), write the answer and its evidence into the plan, empty `## Open questions`, then start the Build.", open.join("\n")))
                } else if name == "task" && turn.depth == 0 && args.get("background").and_then(|v| v.as_bool()).unwrap_or(false) {
                    // Depth 0 only: job reports are delivered to the main agent, a nested background job would leak into its context.
                    background_subagent(&rt, &turn, &id, &args, &tx)
                } else if name == "task" {
                    subagent(&rt, &turn, &id, &args, &tx, None).await.unwrap_or_else(|e| tools::ToolOut::err(format!("{e:#}")))
                } else if name == "ask" {
                    ask(&rt, &args, &tx).await
                } else if name.starts_with("mcp__") {
                    let mut out = rt.mcp.call(&name, &args).await;
                    // A server can answer with megabytes: the model gets the head and tail, the log file the rest.
                    let (text, cut) = tools::truncate_middle(&out.content, rt.settings.tools.max_lines);
                    if cut {
                        let full = tools::save_log(out.content.as_bytes()).map(|f| format!("\n[full output: {}]", f.display())).unwrap_or_default();
                        out.content = format!("{text}{full}");
                    }
                    out
                } else if name == "list_agents" {
                    tools::ToolOut::ok(agents::catalog(&rt.agents))
                } else if name == "handoff" {
                    handoff(&rt, &args, &tx, &ctx.session_id).await
                } else if name == "enter_plan" {
                    enter_plan(&rt, &turn, &args, &tx)
                } else {
                    tools::run(&name, &args, &ctx).await
                };
                if cap > 0 && matches!(name.as_str(), "grep" | "find" | "ls" | "bash") {
                    let (t, cut) = tools::truncate_middle(&out.content, cap);
                    if cut {
                        out.content = format!("{t}\n[cut to {cap} lines: you are the orchestrator — have a subagent read or run larger things and report conclusions]");
                    }
                }
                // Auto's plan is a working document: tell it what the plan's state calls for next, never to stop.
                if name == "write_plan" && !out.is_error && turn.depth == 0 && turn.agent.delegate_all {
                    out.content = format!("{}. {}", out.content, plan_next_step(&tools::fs::open_questions(&ctx.session_id)));
                }
                if name == "todo" {
                    let _ = tx.send(Event::Todos(ctx.todos.lock().unwrap().clone()));
                }
                let _ = tx.send(Event::ToolEnd { id: id.clone(), content: out.content.clone(), is_error: out.is_error, display: out.display });
                Block::ToolResult { id, content: out.content, is_error: out.is_error }
            }
        };
        // Reading tools written side by side run together; a tool with effects (shell, edit, MCP, question) runs alone,
        // in the order the model wrote it: `git add` then `git commit`, an edit then the build.
        let mut results: Vec<Block> = Vec::with_capacity(names.len());
        let mut queue = calls.into_iter().zip(staggered);
        for size in batch_sizes(&names) {
            let batch: Vec<_> = queue.by_ref().take(size).collect();
            results.extend(futures::future::join_all(batch.into_iter().map(&run_call)).await);
        }
        if read_now && diff_read && tree0.is_some() {
            tree_read = tree_state(&rt.cwd).await;
        }
        handed_off |= results.iter().zip(&names).any(|(r, n)| n == "enter_plan" && matches!(r, Block::ToolResult { is_error: false, .. }));
        session.lock().unwrap().add_msg(Msg { role: Role::User, content: results, model: None }, None)?;
    }
    Ok(())
}

/// How the tool calls of one reply are grouped: each group runs together, groups run one after the other.
/// Consecutive reading tools share a group; a tool with effects is a group of its own.
fn batch_sizes(names: &[String]) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut open = false; // the last group may still take reading tools
    for n in names {
        let safe = tools::concurrent_safe(n);
        match out.last_mut() {
            Some(last) if safe && open => *last += 1,
            _ => out.push(1),
        }
        open = safe;
    }
    out
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
/// Plan started by `enter_plan` hands back to its caller at once, without asking.
async fn handoff(rt: &Runtime, args: &Value, tx: &Tx, session_id: &str) -> tools::ToolOut {
    let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    let origin = rt.plan.lock().unwrap().return_to.take();
    if let Some(origin) = origin {
        let plan = crate::config::home().join("plan").join(format!("{session_id}.md"));
        let plan = if plan.exists() { format!("Plan file: {}.", plan.display()) } else { "No plan file was saved.".into() };
        let message = format!(
            "Plan mode finished: {summary}\n{plan} The todo list is loaded. Review the plan against the code and the requirements: if it is sound, execute it now; if it has gaps, guesses or mistakes, call `enter_plan` with precise feedback; `ask` the user only for a choice that is theirs."
        );
        let _ = tx.send(Event::Handoff { agent: origin.clone(), message: Some(message) });
        return tools::ToolOut::ok(format!("Handing back to {origin} for review. End your turn now with one short line."));
    }
    let name = args.get("agent").and_then(|v| v.as_str()).unwrap_or("Build");
    let Some(agent) = agents::find(&rt.agents, name) else { return tools::ToolOut::err(format!("unknown agent `{name}`. Available:\n{}", agents::catalog(&rt.agents))) };
    let q = json!({"question": format!("Switch to {} and start: {summary}?", agent.name), "type": "choice", "options": [format!("Yes, switch to {}", agent.name), "No, keep planning"]});
    let out = ask(rt, &q, tx).await;
    if out.content.contains("Yes, switch") {
        let _ = tx.send(Event::Handoff { agent: agent.name.clone(), message: None });
        return tools::ToolOut::ok(format!("User accepted. {} takes over after your reply: end your turn now with one short line.", agent.name));
    }
    out
}

/// `enter_plan` tool: hand the conversation to Plan, which returns it to the caller once the plan is saved.
fn enter_plan(rt: &Runtime, turn: &Turn, args: &Value, tx: &Tx) -> tools::ToolOut {
    let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("").trim();
    if goal.is_empty() {
        return tools::ToolOut::err("goal required");
    }
    let Some(plan) = agents::find(&rt.agents, "Plan") else { return tools::ToolOut::err("no Plan agent installed: plan with `todo` yourself") };
    let mut st = rt.plan.lock().unwrap();
    if st.rounds >= PLAN_ROUNDS {
        return tools::ToolOut::err(format!("{PLAN_ROUNDS} plan rounds already: decide yourself, or `ask` the user"));
    }
    st.rounds += 1;
    st.return_to = Some(turn.agent.name.clone());
    let message = format!("{} asked for a plan (round {}):\n{goal}\n\nSave it with `todo` and `write_plan`, then call `handoff`.", turn.agent.name, st.rounds);
    let _ = tx.send(Event::Handoff { agent: plan.name.clone(), message: Some(message) });
    tools::ToolOut::ok(format!("{} takes over after your reply and hands back when the plan is saved. End your turn now with one short line.", plan.name))
}

/// Whether this `task` call would start an agent that can change files (resolved like `subagent` does).
fn writes_files(rt: &Runtime, parent: &Turn, args: &Value) -> bool {
    let agent = match args.get("agent").and_then(|v| v.as_str()) {
        Some(n) => agents::find(&rt.agents, n),
        None => Some(&parent.agent),
    };
    agent.is_some_and(|a| !agents::is_readonly(a))
}

/// What Auto does after `write_plan`: settle what is still open, or build, check and review. Never end the turn.
fn plan_next_step(open: &[String]) -> String {
    if open.is_empty() {
        return "The plan is ready, not delivered: start the Builds now, in this turn, follow their reports, then run the Review and Deliver phases.".into();
    }
    format!(
        "The plan is not ready yet. Still open:\n{}\nSettle each one now, in this turn: an Explore or an MCP test for facts, `ask` for what only the user can decide. Then rewrite the plan with the answers.",
        open.join("\n")
    )
}

/// `task` with `background`: run the subagent as a job and return its id at once; the report comes back as a message.
fn background_subagent(rt: &Runtime, parent: &Turn, call_id: &str, args: &Value, tx: &Tx) -> tools::ToolOut {
    let label = args.get("description").and_then(|v| v.as_str()).unwrap_or("subagent").to_string();
    let (rt2, parent, call_id, args, tx) = (rt.clone(), parent.clone(), call_id.to_string(), args.clone(), tx.clone());
    let id = rt.jobs.spawn(crate::jobs::Kind::Agent, label.clone(), false, move |out| {
        Box::pin(async move {
            match subagent(&rt2, &parent, &call_id, &args, &tx, Some(&out)).await {
                Ok(r) => {
                    // The live `→ tool` trace is done with: the job's output becomes the report alone.
                    out.set(&r.content);
                    i32::from(r.is_error)
                }
                Err(e) => {
                    out.set(&format!("{e:#}"));
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
/// Transcript of the subagent started by tool call `id`: derivable from the id, so it stays findable when the run is killed before it reports.
pub fn sub_transcript(id: &str) -> PathBuf {
    let name: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    crate::config::home().join("cache/subagents").join(format!("{name}.jsonl"))
}

/// `log` (background jobs) receives one line per tool call the subagent makes.
async fn subagent(rt: &Runtime, parent: &Turn, call_id: &str, args: &Value, tx: &Tx, log: Option<&crate::jobs::Out>) -> Result<tools::ToolOut> {
    let prompt = args.get("prompt").and_then(|v| v.as_str()).context("prompt required")?;
    anyhow::ensure!(parent.depth <= 1, "a subagent's subagent cannot spawn subagents");
    // Own todo list: the main one belongs to the orchestrator. Background jobs expose theirs through `job todos`.
    let mut rt = rt.clone();
    rt.todos = log.map(|l| l.1.clone()).unwrap_or_default();
    let rt = &rt;
    let named = args.get("agent").is_some();
    let agent = match args.get("agent").and_then(|v| v.as_str()) {
        Some(name) => {
            let a = agents::find(&rt.agents, name).with_context(|| format!("unknown agent `{name}`"))?;
            anyhow::ensure!(agents::can_spawn(&parent.agent, a), "{} is read-only: it can only spawn read-only agents", parent.agent.name);
            a.clone()
        }
        None => parent.agent.clone(),
    };
    anyhow::ensure!(parent.depth == 0 || (named && agents::is_readonly(&parent.agent) && agents::is_readonly(&agent)), "only a read-only subagent can start agents, and only read-only ones (name one with `agent`, e.g. Explore)");
    // `model: "main"` escalates to the orchestrator's own model; an unknown name falls back to the defaults.
    let asked = args.get("model").and_then(|v| v.as_str()).and_then(|m| if m == "main" { Some(parent.model.clone()) } else { rt.resolve(m).ok().map(|_| m.to_string()) });
    let model = match asked {
        Some(m) => m,
        None => {
            // A frontmatter model whose provider has no credentials (Explore's Haiku without an Anthropic key) falls back.
            let own = match agents::model_of(&agent, &rt.settings).filter(|_| named) {
                Some(m) if rt.usable(&m).await => Some(m),
                _ => None,
            };
            match own.or_else(|| parent.agent.subagent_model.clone()) {
                Some(m) => m,
                None => rt.task_model("subagent", &parent.model).await,
            }
        }
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
            s.path = sub_transcript(call_id);
            s
        }
    };
    s.add_msg(Msg::user(prompt), None)?;
    let transcript = s.path.display().to_string();
    let _ = tx.send(Event::Sub { id: call_id.to_string(), event: Box::new(Event::Transcript(transcript.clone())) });
    let session = Arc::new(Mutex::new(s));
    let (stx, mut srx) = tokio::sync::mpsc::unbounded_channel();
    let turn = Turn { agent, model, effort, depth: parent.depth + 1 };
    let fwd_tx = tx.clone();
    let id = call_id.to_string();
    let log = log.cloned();
    let forward = tokio::spawn(async move {
        while let Some(ev) = srx.recv().await {
            if matches!(ev, Event::Text(_) | Event::Thinking(_) | Event::Retry { .. }) {
                continue;
            }
            if let (Some(log), Event::ToolStart { name, .. }) = (&log, &ev) {
                log.push(&format!("→ {name}\n"));
            }
            // A grandchild's activity counts as this subagent's: the UI tracks one level.
            let ev = match ev {
                Event::Sub { event, .. } if matches!(*event, Event::ToolStart { .. }) => *event,
                Event::Sub { .. } => continue,
                ev => ev,
            };
            let _ = fwd_tx.send(Event::Sub { id: id.clone(), event: Box::new(ev) });
        }
    });
    // A subagent that can write is checked by the harness, not by its own report.
    let writes = !agents::is_readonly(&turn.agent);
    let before = if writes { tree_state(&rt.cwd).await } else { None };
    let who = turn.agent.name.clone();
    let writer = writes.then(|| Writer::enter(&rt.gate));
    let res = Box::pin(run(rt.clone(), session.clone(), turn, stx, Steer::default())).await;
    drop(writer);
    let _ = forward.await;
    // A run that died (provider error, overflow) still did work: hand back what it said last, its todos and its transcript.
    let failed = res.err().map(|e| format!("{e:#}"));
    let answer = session.lock().unwrap().context().iter().rev().find(|m| m.role == Role::Assistant).map(|m| m.text()).filter(|t| failed.is_some() || !t.is_empty()).unwrap_or_default();
    // Whole transcript, resumes included: lets the orchestrator see a step that burns tokens without progress.
    let (calls, input) = session.lock().unwrap().entries.iter().fold((0u64, 0u64), |(n, t), e| match &e.kind {
        crate::session::Kind::Msg { usage: Some(u), .. } => (n + 1, t + u.input + u.cache_read + u.cache_write),
        _ => (n, t),
    });
    // The 600-char rule is only a prompt: cap what actually enters the orchestrator's context.
    let answer = match answer.char_indices().nth(REPORT_CAP) {
        _ if answer.is_empty() && failed.is_some() => "(it had not written anything yet)".into(),
        _ if answer.is_empty() => "(subagent returned no text)".into(),
        Some((i, _)) => format!("{}… [report cut, see transcript]", &answer[..i]),
        None => answer,
    };
    let mut checks = String::new();
    // A planning subagent saved its own plan file: point the orchestrator at it, with what it left open.
    let sub_id = session.lock().unwrap().id.clone();
    let plan_file = crate::config::home().join("plan").join(format!("{sub_id}.md"));
    if plan_file.exists() {
        checks.push_str(&format!("\n[plan file: {}]", plan_file.display()));
        let open = tools::fs::open_questions(&sub_id);
        if !open.is_empty() {
            checks.push_str(&format!("\n[open questions in that plan: {}]", open.join("; ")));
        }
    }
    let open: Vec<String> = rt.todos.lock().unwrap().iter().filter(|t| t.status != "done").map(|t| t.content.clone()).collect();
    if !open.is_empty() {
        checks.push_str(&format!("\n[unfinished in its todo: {}]", open.join("; ")));
    }
    // An orchestrator using a writing agent for read-only work: point it at Explore.
    if let Some(b) = &before
        && parent.agent.delegate_all
        && tree_state(&rt.cwd).await.as_ref() == Some(b)
        && agents::find(&rt.agents, "Explore").is_some()
    {
        checks.push_str("\n[note: no files changed — use Explore for read-only work, it is faster and cheaper]");
    }
    if let Some(before) = before
        && tree_state(&rt.cwd).await.is_some_and(|after| after != before) {
            if let Some(cmd) = verify_command(&rt.settings, &rt.cwd).filter(|_| failed.is_none()) {
                // One check at a time, and never on a tree another writer is still editing: the last one to finish runs it.
                let _turn = rt.gate.verify.lock().await;
                let others = rt.gate.writers.load(std::sync::atomic::Ordering::SeqCst);
                if others > 0 {
                    checks.push_str(&format!("\n[verify deferred: {others} other writing subagent(s) still editing; the last one to finish runs `{cmd}` on the whole tree]"));
                } else {
                    checks.push_str(&format!("\n{}", verify(&rt.cwd, &cmd, rt.settings.tools.bash_timeout.max(600)).await));
                }
            }
            if let Some(stat) = git(&rt.cwd, &["diff", "HEAD", "--shortstat"]).await.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
                checks.push_str(&format!("\n[changed since HEAD: {stat}]"));
            }
        }
    // A resume without `agent` would run as the orchestrator itself: name the agent to continue.
    let next = if answer.trim_start().to_lowercase().starts_with("questions:") {
        format!("\n[{who} is waiting for your answers: task agent=\"{who}\" resume=<transcript> prompt=<one answer per question>]")
    } else {
        format!("\n[to continue it: task agent=\"{who}\" resume=<transcript>]")
    };
    let tail = format!("{checks}\n[cost: {calls} model calls, {}k input tokens]{next}\n[transcript: {transcript}]", input / 1000);
    Ok(match failed {
        Some(e) => tools::ToolOut::err(format!("{who} stopped on an error: {e}\nIts work so far is kept; resume it to carry on from where it stopped. Last message:\n{answer}{tail}")),
        None => tools::ToolOut::ok(format!("{answer}{tail}")),
    })
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

    #[tokio::test]
    async fn task_lists_spawnable_agents_with_descriptions() {
        let mut rt = test_rt().await;
        rt.agents = Arc::new(vec![
            agents::parse_builtin(agents::DEFAULT_BUILD, "Build"),
            agents::parse_builtin(agents::DEFAULT_EXPLORE, "Explore"),
            agents::parse_builtin(agents::DEFAULT_AUTO, "Auto"),
        ]);
        let auto = rt.agents[2].clone();
        let defs = rt.tools_for(&auto, 0);
        let task = defs.iter().find(|d| d.name == "task").unwrap();
        assert!(task.description.contains("- Explore: Fast read-only search") && !task.description.contains("- Auto:"), "{}", task.description);
        assert_eq!(task.schema["properties"]["agent"]["enum"], json!(["Build", "Explore"]));
        assert!(task.schema["required"].as_array().unwrap().contains(&json!("agent")));
        assert!(defs.iter().any(|d| d.name == "enter_plan") && defs.iter().any(|d| d.name == "bash"));
        // subagents never get plan mode
        assert!(!rt.tools_for(&auto, 1).iter().any(|d| d.name == "enter_plan"));
    }

    #[tokio::test]
    async fn nested_task_is_read_only_and_one_level_deep() {
        let mut rt = test_rt().await;
        rt.agents = Arc::new(vec![
            agents::parse_builtin(agents::DEFAULT_BUILD, "Build"),
            agents::parse_builtin(agents::DEFAULT_EXPLORE, "Explore"),
            agents::parse_builtin(agents::DEFAULT_PLAN, "Plan"),
            agents::parse_builtin(agents::DEFAULT_AUTO, "Auto"),
        ]);
        let (build, explore, plan) = (rt.agents[0].clone(), rt.agents[1].clone(), rt.agents[2].clone());
        // A Plan subagent may start read-only agents only.
        let defs = rt.tools_for(&plan, 1);
        let task = defs.iter().find(|d| d.name == "task").expect("Plan subagent has task");
        let names: Vec<&str> = task.schema["properties"]["agent"]["enum"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"Explore") && !names.contains(&"Build") && !names.contains(&"Auto"), "{names:?}");
        // A writing subagent delegates nothing; nothing nests deeper.
        assert!(!rt.tools_for(&build, 1).iter().any(|d| d.name == "task"));
        assert!(!rt.tools_for(&plan, 2).iter().any(|d| d.name == "task"));
        assert!(!rt.tools_for(&explore, 1).iter().any(|d| d.name == "task"));
        // The harness refuses what the tool list does not offer.
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sub = Turn { agent: plan.clone(), model: String::new(), effort: String::new(), depth: 1 };
        let err = subagent(&rt, &sub, "t1", &json!({"prompt": "x", "agent": "Build"}), &tx, None).await.err().expect("a writing agent is refused");
        assert!(format!("{err:#}").contains("read-only"), "{err:#}");
        let deep = Turn { agent: plan, model: String::new(), effort: String::new(), depth: 2 };
        assert!(subagent(&rt, &deep, "t2", &json!({"prompt": "x", "agent": "Explore"}), &tx, None).await.is_err());
    }

    #[tokio::test]
    async fn plan_mode_round_trip() {
        let mut rt = test_rt().await;
        rt.agents = Arc::new(vec![agents::parse_builtin(agents::DEFAULT_PLAN, "Plan"), agents::parse_builtin(agents::DEFAULT_AUTO, "Auto")]);
        let turn = Turn { agent: rt.agents[1].clone(), model: String::new(), effort: String::new(), depth: 0 };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(enter_plan(&rt, &turn, &json!({"goal": ""}), &tx).is_error);
        assert!(!enter_plan(&rt, &turn, &json!({"goal": "add x"}), &tx).is_error);
        let Some(Event::Handoff { agent, message: Some(m) }) = rx.recv().await else { panic!("no handoff to Plan") };
        assert!(agent == "Plan" && m.contains("add x"));
        // Plan hands back to Auto without asking the user.
        assert!(!handoff(&rt, &json!({"summary": "plan ready"}), &tx, "s1").await.is_error);
        let Some(Event::Handoff { agent, message: Some(m) }) = rx.recv().await else { panic!("no handoff back") };
        assert!(agent == "Auto" && m.contains("Plan mode finished"));
        assert!(rt.plan.lock().unwrap().return_to.is_none());
        // rounds are capped per user message
        for _ in 1..PLAN_ROUNDS {
            assert!(!enter_plan(&rt, &turn, &json!({"goal": "again"}), &tx).is_error);
        }
        assert!(enter_plan(&rt, &turn, &json!({"goal": "again"}), &tx).is_error);
    }

    #[test]
    fn plan_next_step_follows_open_questions() {
        let open = plan_next_step(&["Q1 does RegisterNpc return a key? → MCP test".into()]);
        assert!(open.contains("not ready") && open.contains("Q1 does RegisterNpc") && open.contains("`ask`"));
        let ready = plan_next_step(&[]);
        assert!(ready.contains("start the Builds") && ready.contains("Review"));
    }

    #[test]
    fn effect_tools_run_alone_and_in_order() {
        let n = |v: &[&str]| batch_sizes(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["read", "grep", "task"]), [3]);
        assert_eq!(n(&["bash", "bash"]), [1, 1]);
        assert_eq!(n(&["read", "edit", "bash", "read", "read", "mcp__db__query", "task", "task"]), [1, 1, 1, 2, 1, 2]);
        assert!(n(&[]).is_empty());
    }

    #[test]
    fn old_job_tool_names_allow_job() {
        assert!(allows("wait", "job") && allows("job_output", "job") && !allows("wait", "bash"));
    }

    #[tokio::test]
    async fn default_tool_set_is_lean() {
        let rt = test_rt().await;
        // An agent file without `tools:` gets the lean set; Build lists its own (with find and ls).
        let mut plain = agents::parse_builtin(agents::DEFAULT_BUILD, "Build");
        plain.tools = None;
        let names: Vec<_> = rt.tools_for(&plain, 0).into_iter().map(|d| d.name).collect();
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
            plan: Default::default(),
            gate: Default::default(),
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
