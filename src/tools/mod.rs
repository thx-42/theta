//! Built-in tools. Outputs are compacted for token efficiency (rtk-style).

mod fs;
pub mod shell;
mod web;

use crate::config::Settings;
use crate::types::ToolDef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Todo {
    pub content: String,
    /// pending | in_progress | done
    pub status: String,
}

/// Shared state tools may touch.
#[derive(Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub settings: Arc<Settings>,
    pub http: reqwest::Client,
    pub todos: Arc<Mutex<Vec<Todo>>>,
    /// path -> content hash of the last full read, to answer "unchanged" on re-reads.
    pub read_cache: Arc<Mutex<HashMap<String, u64>>>,
    /// Session the tools run for; `write_plan` names its file after it.
    pub session_id: String,
    pub jobs: crate::jobs::Jobs,
    /// Host the file and shell tools run on, when the session works over ssh.
    pub ssh: crate::ssh::Slot,
}

pub struct ToolOut {
    pub content: String,
    pub is_error: bool,
    /// Richer text for the UI (e.g. a diff); the model only sees `content`.
    pub display: Option<String>,
}

impl ToolOut {
    pub fn ok(s: impl Into<String>) -> Self {
        ToolOut { content: s.into(), is_error: false, display: None }
    }
    pub fn err(s: impl Into<String>) -> Self {
        ToolOut { content: s.into(), is_error: true, display: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    Command,
    Other,
}

pub fn kind(name: &str) -> Kind {
    match name {
        "read" | "ls" | "find" | "grep" => Kind::Read,
        "write" | "edit" | "write_plan" => Kind::Write,
        "bash" => Kind::Command,
        _ => Kind::Other,
    }
}

fn def(name: &str, description: &str, schema: Value) -> ToolDef {
    ToolDef { name: name.into(), description: description.into(), schema }
}

/// All built-in tool definitions. `task` is added by the agent when subagents are allowed.
pub fn all_defs() -> Vec<ToolDef> {
    vec![
        def("read", "Read a text file. Output is capped: use offset/limit (1-indexed lines) for large files.",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]})),
        def("write", "Create or overwrite a file. Creates parent directories.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})),
        def("edit", "Edit a file by exact string replacement. Each `old` must match exactly one place (add context). Edits apply atomically.",
            json!({"type":"object","properties":{"path":{"type":"string"},"edits":{"type":"array","items":{"type":"object","properties":{"old":{"type":"string"},"new":{"type":"string"}},"required":["old","new"]}}},"required":["path","edits"]})),
        def("bash", "Run a shell command in the project directory. Long output is truncated; the full log is saved to a file.",
            json!({"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"integer","description":"seconds"},"background":{"type":"boolean","description":"Return a job id at once and keep running (servers, watchers, long builds); its end is reported later"}},"required":["command"]})),
        def("job", "Manage background jobs (shell, lint, subagent). `action`: list; output (+ `id`, optional `tail` lines); kill (+ `id`); wait for subagent reports (`mode`: any = first end, default; all = none left).",
            json!({"type":"object","properties":{"action":{"type":"string","enum":["list","output","kill","wait"]},"id":{"type":"string"},"tail":{"type":"integer"},"mode":{"type":"string","enum":["any","all"]},"timeout":{"type":"integer","description":"wait, seconds"}},"required":["action"]})),
        def("grep", "Search file contents with a regex (respects .gitignore). Results grouped by file.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"ignore_case":{"type":"boolean"},"literal":{"type":"boolean"},"context":{"type":"integer"},"limit":{"type":"integer"}},"required":["pattern"]})),
        def("find", "Find files by glob pattern (respects .gitignore), e.g. `**/*.rs`.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"limit":{"type":"integer"}},"required":["pattern"]})),
        def("ls", "List a directory (dirs end with /).",
            json!({"type":"object","properties":{"path":{"type":"string"}}})),
        def("write_plan", "Save this session's plan as markdown (one file per session, rewritten on each call).",
            json!({"type":"object","properties":{"content":{"type":"string"}},"required":["content"]})),
        def("todo", "Replace the task list for multi-step work: one item in_progress, mark items done as they finish.",
            json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","done"]}},"required":["content","status"]}}},"required":["todos"]})),
        def("ask", "Ask the user a question and wait. Only when blocked on a decision that is the user's. `type`: choice (2-6 `options`), yes_no, or text.",
            json!({"type":"object","properties":{"question":{"type":"string"},"type":{"type":"string","enum":["choice","yes_no","text"]},"options":{"type":"array","items":{"type":"string"}}},"required":["question","type"]})),
        def("list_agents", "List the available agents with what each can and cannot do. Call before `handoff`.",
            json!({"type":"object","properties":{}})),
        def("handoff", "Ask the user to switch to another agent (from `list_agents`) to carry out the plan. If accepted, that agent continues this conversation.",
            json!({"type":"object","properties":{"agent":{"type":"string","description":"default Build"},"summary":{"type":"string","description":"one line: what will be built"}},"required":["summary"]})),
        def("web_search", "Search the web. Returns titles, URLs and short highlights.",
            json!({"type":"object","properties":{"query":{"type":"string"},"num":{"type":"integer"}},"required":["query"]})),
        def("web_fetch", "Fetch a URL and return its main content as compact text.",
            json!({"type":"object","properties":{"url":{"type":"string"},"max_chars":{"type":"integer"}},"required":["url"]})),
    ]
}

/// Tools only agents that list them get: the default set stays close to read/write/edit/bash.
pub const OPT_IN: &[&str] = &["ls", "find", "write_plan", "ask", "handoff", "list_agents"];

pub fn task_def(agents: &[String]) -> ToolDef {
    def("task", &format!("Delegate a self-contained task to a subagent with a fresh context; only its final report returns. Use for broad searches or independent work. Agents: {}.", agents.join(", ")),
        json!({"type":"object","properties":{"description":{"type":"string","description":"3-6 words"},"prompt":{"type":"string","description":"Full instructions; the subagent sees nothing else"},"agent":{"type":"string"},"background":{"type":"boolean","description":"Return a job id at once; the report arrives later"},"resume":{"type":"string","description":"Transcript path or id from an earlier report: continue that subagent"},"model":{"type":"string","description":"Override the subagent model: `main` = your own (escalate a step that failed twice) or provider/model"},"effort":{"type":"string","enum":["low","medium","high","xhigh","max"]}},"required":["description","prompt"]}))
}

pub fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let p = if let Some(rest) = p.strip_prefix("~/") { dirs::home_dir().unwrap_or_default().join(rest) } else { PathBuf::from(p) };
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn s<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(|v| v.as_str())
}

fn n(args: &Value, k: &str) -> Option<u64> {
    args.get(k).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)).or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn b(args: &Value, k: &str) -> bool {
    args.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Keep head and tail of long output; mark what was cut.
pub fn truncate_middle(text: &str, max_lines: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let max_bytes = max_lines * 60; // 300 lines of dense text stay near 5k tokens
    if lines.len() <= max_lines && text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let head = max_lines * 2 / 5;
    let tail = max_lines - head;
    let mut out: Vec<String> = Vec::new();
    let cut = |l: &str| if l.len() > 500 { format!("{}…", &l[..l.floor_char_boundary(500)]) } else { l.to_string() };
    if lines.len() > max_lines {
        out.extend(lines[..head].iter().map(|l| cut(l)));
        out.push(format!("… [{} lines omitted] …", lines.len() - head - tail));
        out.extend(lines[lines.len() - tail..].iter().map(|l| cut(l)));
    } else {
        out.extend(lines.iter().map(|l| cut(l)));
    }
    let mut joined = out.join("\n");
    if joined.len() > max_bytes {
        let h = joined.floor_char_boundary(max_bytes * 2 / 5);
        let t = joined.ceil_char_boundary(joined.len() - max_bytes * 3 / 5);
        joined = format!("{}\n… [output truncated] …\n{}", &joined[..h], &joined[t..]);
    }
    (joined, true)
}

/// Subagent transcripts live on this machine: reading them must not go to the ssh host.
fn is_transcript(name: &str, args: &Value, ctx: &ToolCtx) -> bool {
    matches!(name, "read" | "grep")
        && s(args, "path").is_some_and(|p| !p.contains("..") && resolve(&ctx.cwd, p).starts_with(crate::config::home().join("cache/subagents")))
}

pub async fn run(name: &str, args: &Value, ctx: &ToolCtx) -> ToolOut {
    if let Some(bad) = args.get("_invalid_json") {
        return ToolOut::err(format!("invalid JSON arguments: {bad}"));
    }
    if crate::ssh::is_remote_tool(name)
        && !is_transcript(name, args, ctx)
        && let Some(host) = ctx.ssh.get()
    {
        return host.call(name, args).await;
    }
    let res = match name {
        "read" => fs::read(args, ctx),
        "write" => fs::write(args, ctx),
        "write_plan" => fs::write_plan(args, &ctx.session_id),
        "edit" => fs::edit(args, ctx),
        "ls" => fs::ls(args, ctx),
        "find" => fs::find(args, ctx),
        "grep" => fs::grep(args, ctx),
        "bash" => shell::bash(args, ctx).await,
        "job" => job(args, ctx).await,
        "todo" => todo(args, ctx),
        "web_search" => web::search(args, ctx).await,
        "web_fetch" => web::fetch(args, ctx).await,
        _ => Err(anyhow::anyhow!("unknown tool `{name}`")),
    };
    res.unwrap_or_else(|e| ToolOut::err(format!("{e:#}")))
}

/// `job` tool: list | output | kill | wait. The agent loop handles `wait` itself so a user message can interrupt it.
async fn job(args: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOut> {
    match s(args, "action") {
        Some("list") => Ok(jobs_list(ctx)),
        Some("output") => job_output(args, ctx),
        Some("kill") => job_kill(args, ctx),
        Some("wait") => wait(args, ctx, &|| false).await,
        _ => anyhow::bail!("action must be list, output, kill or wait"),
    }
}

fn job_output(args: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOut> {
    let id = s(args, "id").ok_or_else(|| anyhow::anyhow!("id required"))?;
    let (text, info) = ctx.jobs.output(id, n(args, "tail").map(|t| t as usize)).ok_or_else(|| anyhow::anyhow!("no job `{id}`"))?;
    let (text, _) = truncate_middle(&text, ctx.settings.tools.max_lines);
    Ok(ToolOut::ok(format!("[{}] {}\n{text}", state_of(info.status), info.label)))
}

fn state_of(s: crate::jobs::Status) -> String {
    match s {
        crate::jobs::Status::Running => "running".to_string(),
        crate::jobs::Status::Done(c) => format!("exit {c}"),
        crate::jobs::Status::Killed => "killed".into(),
        crate::jobs::Status::Missing => "absent".into(),
    }
}

fn jobs_list(ctx: &ToolCtx) -> ToolOut {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let l = ctx.jobs.list();
    if l.is_empty() {
        return ToolOut::ok("no jobs");
    }
    ToolOut::ok(l.iter().map(|j| format!("{} {:?} [{}] {}s {}", j.id, j.kind, state_of(j.status), j.ended.unwrap_or(now).saturating_sub(j.started), j.label)).collect::<Vec<_>>().join("\n"))
}

/// `wait`: block on subagent jobs, then hand over the reports of the ones that ended.
/// `interrupt` is polled while waiting (a user message is pending): the wait ends early.
pub async fn wait(args: &Value, ctx: &ToolCtx, interrupt: &(dyn Fn() -> bool + Sync)) -> anyhow::Result<ToolOut> {
    let all = s(args, "mode") == Some("all");
    let secs = n(args, "timeout").unwrap_or(900).clamp(1, 3600);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let start = ctx.jobs.ended_count();
    let mut why = "";
    loop {
        tokio::select! {
            _ = ctx.jobs.wait_since(all, start) => break,
            _ = tokio::time::sleep(std::time::Duration::from_millis(300)) => {}
        }
        if interrupt() {
            why = "[interrupted: the user sent a message]";
            break;
        }
        if std::time::Instant::now() >= deadline {
            why = "[timeout: nothing ended]";
            break;
        }
    }
    let mut out = ctx.jobs.take_pending().join("\n\n");
    if out.is_empty() {
        out = if !why.is_empty() { why.into() } else { "[no report: no subagent running, or it was stopped]".into() };
    }
    let running = ctx.jobs.running_agents();
    if running > 0 {
        out.push_str(&format!("\n\n[{running} subagent(s) still running; see `job` list]"));
    }
    Ok(ToolOut::ok(out))
}

fn job_kill(args: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOut> {
    let id = s(args, "id").ok_or_else(|| anyhow::anyhow!("id required"))?;
    Ok(if ctx.jobs.kill(id) { ToolOut::ok(format!("stopped job {id}")) } else { ToolOut::err(format!("no running job `{id}`")) })
}

fn todo(args: &Value, ctx: &ToolCtx) -> anyhow::Result<ToolOut> {
    let todos: Vec<Todo> = serde_json::from_value(args.get("todos").cloned().unwrap_or(json!([])))?;
    let rendered = render_todos(&todos);
    *ctx.todos.lock().unwrap() = todos;
    Ok(ToolOut::ok(rendered))
}

pub fn render_todos(todos: &[Todo]) -> String {
    todos
        .iter()
        .map(|t| {
            let mark = match t.status.as_str() { "done" => "[x]", "in_progress" => "[>]", _ => "[ ]" };
            format!("{mark} {}", t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_middle() {
        let text: String = (0..100).map(|i| format!("line{i}\n")).collect();
        let (out, cut) = truncate_middle(&text, 10);
        assert!(cut);
        assert!(out.starts_with("line0\nline1\nline2\nline3\n… [90 lines omitted]"));
        assert!(out.ends_with("line99"));
        assert_eq!(truncate_middle("a\nb", 10), ("a\nb".to_string(), false));
    }

    #[test]
    fn transcripts_stay_local_under_ssh() {
        let ctx = ToolCtx {
            cwd: PathBuf::from("/proj"),
            settings: Default::default(),
            http: reqwest::Client::new(),
            todos: Default::default(),
            read_cache: Default::default(),
            session_id: String::new(),
            jobs: Default::default(),
            ssh: Default::default(),
        };
        let t = crate::config::home().join("cache/subagents/ab.jsonl").display().to_string();
        assert!(is_transcript("read", &json!({"path": t}), &ctx));
        assert!(!is_transcript("write", &json!({"path": t}), &ctx));
        assert!(!is_transcript("read", &json!({"path": "src/main.rs"}), &ctx));
        assert!(!is_transcript("read", &json!({"path": format!("{t}/../../../x")}), &ctx));
    }
}
