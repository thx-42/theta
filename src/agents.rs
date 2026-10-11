//! Agents (markdown + frontmatter) from ~/.theta/agents (global) and <project>/.theta/agents (local),
//! plus SOUL.md (global + project) applied to every agent.

use crate::config;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    Local,
    Builtin,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Local => "local",
            Scope::Builtin => "builtin",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub name: String,
    pub description: String,
    /// Short "can do" / "cannot do" notes, shown to an LLM choosing a handoff target
    pub can: String,
    pub cannot: String,
    pub scope: Scope,
    /// Optional `provider/model` override
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Model for subagents spawned by this agent
    pub subagent_model: Option<String>,
    /// Effort for subagents spawned by this agent
    pub subagent_effort: Option<String>,
    /// Allowed tools (None = all)
    pub tools: Option<Vec<String>>,
    /// May spawn any agent as a subagent (`delegate: all`); otherwise only read-only ones, unless it has every tool.
    pub delegate_all: bool,
    pub prompt: String,
}

fn parse(text: &str, fallback_name: &str, scope: Scope) -> Agent {
    let mut a = Agent {
        name: fallback_name.to_string(),
        description: String::new(),
        can: String::new(),
        cannot: String::new(),
        scope,
        model: None,
        effort: None,
        subagent_model: None,
        subagent_effort: None,
        tools: None,
        delegate_all: false,
        prompt: text.trim().to_string(),
    };
    let Some(rest) = text.strip_prefix("---") else { return a };
    let Some((front, body)) = rest.split_once("\n---") else { return a };
    a.prompt = body.trim_start_matches(['-', '\n', '\r']).trim().to_string();
    for line in front.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim().trim_matches('"').to_string();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "name" => a.name = v,
            "description" => a.description = v,
            "can" => a.can = v,
            "cannot" => a.cannot = v,
            "model" => a.model = Some(v),
            "effort" => a.effort = Some(v),
            "subagent_model" => a.subagent_model = Some(v),
            "subagent_effort" => a.subagent_effort = Some(v),
            "delegate" => a.delegate_all = v.eq_ignore_ascii_case("all"),
            "tools" => a.tools = Some(v.trim_matches(['[', ']']).split(',').map(|t| t.trim().trim_matches('"').to_string()).filter(|t| !t.is_empty()).collect()),
            _ => {}
        }
    }
    a
}

fn load_dir(dir: &Path, scope: Scope, out: &mut Vec<Agent>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
    files.sort();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let stem = f.file_stem().unwrap().to_string_lossy().to_string();
        let a = parse(&text, &stem, scope);
        // Local agents shadow global ones with the same name.
        out.retain(|x| !x.name.eq_ignore_ascii_case(&a.name));
        out.push(a);
    }
}

pub fn discover(project: &Path) -> Vec<Agent> {
    let mut out = Vec::new();
    load_dir(&config::home().join("agents"), Scope::Global, &mut out);
    load_dir(&project.join(".theta/agents"), Scope::Local, &mut out);
    ensure_builtins(&mut out);
    out
}

/// The shipped agents must always exist: re-add any missing one from the binary.
fn ensure_builtins(out: &mut Vec<Agent>) {
    for (src, name) in [(DEFAULT_BUILD, "Build"), (DEFAULT_PLAN, "Plan"), (DEFAULT_AUTO, "Auto")] {
        if !out.iter().any(|a| a.name.eq_ignore_ascii_case(name)) {
            out.insert(0, parse(src, name, Scope::Builtin));
        }
    }
}

/// One block per agent for the `list_agents` tool: task, can, cannot.
pub fn catalog(agents: &[Agent]) -> String {
    agents
        .iter()
        .map(|a| {
            let mut s = format!("- {}: {}", a.name, a.description);
            if !a.can.is_empty() {
                s += &format!("\n  can: {}", a.can);
            }
            if !a.cannot.is_empty() {
                s += &format!("\n  cannot: {}", a.cannot);
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read-only = an explicit tool list with nothing that writes files, runs commands or reaches MCP.
pub fn is_readonly(a: &Agent) -> bool {
    a.tools.as_ref().is_some_and(|t| t.iter().all(|e| {
        let e = e.to_lowercase();
        !matches!(e.as_str(), "write" | "edit" | "bash") && !e.starts_with("mcp") && !e.ends_with('*')
    }))
}

/// Subagents never gain write access the parent lacks: a read-only parent (Plan) can only spawn
/// read-only agents, unless it opts in with `delegate: all` (Auto).
pub fn can_spawn(parent: &Agent, target: &Agent) -> bool {
    parent.tools.is_none() || parent.delegate_all || !is_readonly(parent) || is_readonly(target)
}

pub fn find<'a>(agents: &'a [Agent], name: &str) -> Option<&'a Agent> {
    agents.iter().find(|a| a.name.eq_ignore_ascii_case(name))
}

/// Model for an agent: `[agents.<name>]` setting > agent frontmatter. None = main model.
pub fn model_of(agent: &Agent, settings: &config::Settings) -> Option<String> {
    settings.agent(&agent.name).map(|s| s.model.clone()).filter(|m| !m.is_empty()).or_else(|| agent.model.clone())
}

/// Effort for an agent: `[agents.<name>]` setting > agent frontmatter. None = main effort.
pub fn effort_of(agent: &Agent, settings: &config::Settings) -> Option<String> {
    settings.agent(&agent.name).map(|s| s.effort.clone()).filter(|e| !e.is_empty()).or_else(|| agent.effort.clone())
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// SOUL (global, then project) + agent prompt + auto skills + environment + project context file.
pub fn system_prompt(agent: &Agent, project: &Path, cwd: &Path, skills: &str) -> String {
    let mut parts = Vec::new();
    if let Some(s) = read(&config::home().join("SOUL.md")) {
        parts.push(s);
    }
    if let Some(s) = read(&project.join(".theta/SOUL.md")) {
        parts.push(s);
    }
    // Per-agent SOUL: SOUL.<Agent>.md, global then project.
    let own = format!("SOUL.{}.md", agent.name);
    for p in [config::home().join(&own), project.join(".theta").join(&own)] {
        if let Some(s) = read(&p) {
            parts.push(s);
        }
    }
    parts.push(agent.prompt.clone());
    if !skills.is_empty() {
        parts.push(skills.to_string());
    }
    let date = chrono_date();
    parts.push(format!(
        "# Environment\n- cwd: {}\n- project root: {}\n- platform: {} {}\n- date: {date}",
        cwd.display(),
        project.display(),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Some(ctx) = read(&project.join(name)) {
            parts.push(format!("# Project instructions ({name})\n{ctx}"));
            break;
        }
    }
    parts.join("\n\n")
}

fn chrono_date() -> String {
    // Days since epoch -> civil date (Howard Hinnant's algorithm), avoids a date dependency.
    let days = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

pub const DEFAULT_SOUL: &str = r#"# SOUL
Rules for every agent. Edit ~/.theta/SOUL.md (global) or <project>/.theta/SOUL.md (project).

- Answer first, then the reason. No greetings, no recap, no filler. Every token is paid for.
- Keep code, commands, paths and errors exact.
- Do the smallest complete change: reuse what exists, add no speculative features or abstractions.
- Read before you edit. Verify after you change (build, test, run). Report failures honestly.
- Ask only when blocked on a decision that is really the user's.
- Never run destructive or irreversible commands without explicit confirmation.
"#;

pub const DEFAULT_BUILD: &str = r#"---
name: Build
description: Default coding agent — reads, edits, runs and verifies code
can: read, edit and write files, run shell commands, build and test, search the web, delegate to subagents
cannot: ask the user questions mid-task or hand off to another agent
---
You are theta, a coding agent working in the user's terminal on their project.

Work loop:
1. Understand: use grep/find/read to locate the relevant code. Prefer targeted reads (offset/limit) over whole large files.
2. Plan: for multi-step work, keep a `todo` list and update it as you go.
3. Change: use `edit` for surgical changes and `write` only for new files or full rewrites.
4. Verify: run the build/tests with `bash`. Fix what you broke.
5. Report: a short summary of what changed and anything left unverified.

Use `task` to delegate broad searches or independent research to a subagent so your context stays small.
Use `web_search` / `web_fetch` for current docs or facts you are unsure of.
Format answers in Markdown. Reference code as `path:line`.
"#;

pub const DEFAULT_PLAN: &str = r#"---
name: Plan
description: Clarifies the task before coding — rephrases, finds blind spots, asks, plans
can: read and search code, search the web, ask the user, write a todo plan and a plan file for this session, propose a handoff
cannot: edit project files, run shell commands, write code
tools: read, grep, find, ls, todo, write_plan, web_search, web_fetch, ask, task, list_agents, handoff
---
You are theta in planning mode. You do not edit files or run commands. Turn the request into a precise, buildable plan.

1. Understand: read the relevant code; search the web when facts may be outdated. Use `task` for broad exploration.
2. Rephrase goal, scope and expected result so the user can spot a misunderstanding.
3. List what the request leaves vague (edge cases, data, errors, compatibility, tests, what must not change). Settle each point from the code or docs and say how; for decisions only the user can make, `ask` one question per call with concrete options, recommended first.
4. Plan: a `todo` list of small verifiable steps, summarized (files, approach, risks, verification). Save it with `write_plan`.
5. When nothing blocks: `list_agents`, pick the fitting agent, `handoff` with a one-line summary. If declined, keep refining.

Stay brief. Never ask what you can find out yourself. Do not start coding.
"#;

pub const DEFAULT_AUTO: &str = r#"---
name: Auto
description: Autonomous orchestrator — plans with the user, then runs Build and Plan subagents end to end until the task is done and verified
can: read and search code, search the web, ask the user, keep a todo list, delegate to Build (write) and Plan (review) subagents
cannot: edit project files or run shell commands itself
tools: read, grep, find, ls, todo, write_plan, web_search, web_fetch, ask, task, list_agents, job
delegate: all
---
You are theta in auto mode. You never write code: you orchestrate subagents (only you can spawn them).

Phase 1 — Plan with the user (the only phase where you stop for the user):
1. Read the relevant code; settle what you can yourself.
2. `ask` (one question per call, concrete options) only for decisions that are the user's.
3. Write the plan as a `todo` list of small verifiable steps, save it with `write_plan`, summarize it and `ask` (yes_no) for validation. Revise until validated.

Phase 2 — Execute without stopping or asking confirmation between steps; mark todos done as you go.
- Build: `task` with agent `Build` and a self-contained prompt. Independent steps run with `background`, then `job` wait.
- Review: `task` with agent `Plan` on each built step; ask for concrete defects only.
- Fix: send defects back to `Build`, review again until clean.
- Check each report against the code before you trust it. Replan only if reality breaks the plan.

Phase 3 — Final report, only when every todo is done and verified: what changed, what was verified, what is left.
"#;

/// Shipped as ~/.theta/SOUL.Auto.md; applied to the Auto agent only.
pub const DEFAULT_AUTO_SOUL: &str = r#"# SOUL — Auto
Your context is the scarce resource: spend it on decisions, not on subagent chatter.

## Subagent prompts
- Self-contained and short: goal in one sentence, exact files/symbols, approach, what must not change, how to verify.
- Point to `path:line` instead of pasting code or logs. Raise `effort` only via the agent settings.
- Build writes, Plan reviews (read-only). Subagents already run with the terse skills; do not repeat that.
- Their report is ≤ ~600 chars and ends with `[transcript: <path>]`. Open the transcript (a `grep`/`read` slice) only when the report is vague, contradicts the todo or claims a failure you cannot explain.

## Waiting
- Independent steps (no shared files, no ordering): `background: true`. Dependent steps: one after the other.
- `job` wait: `mode: any` when each result can be reviewed alone, `all` when steps must be judged together. Never poll `job` output in a loop.
- If you end your turn with subagents running, theta wakes you when one ends.

Terse output, no narration between tool calls.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let a = parse("---\nname: Review\nmodel: openai/gpt-5\ntools: read, grep\n---\nBe strict.", "x", Scope::Local);
        assert_eq!(a.name, "Review");
        assert_eq!(a.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(a.tools, Some(vec!["read".into(), "grep".into()]));
        assert_eq!(a.prompt, "Be strict.");
        let c = parse("---\nname: R\ndescription: reviews\ncan: read\ncannot: edit\n---\nx", "x", Scope::Local);
        assert_eq!(catalog(&[c]), "- R: reviews\n  can: read\n  cannot: edit");
        let b = parse("plain prompt", "Plain", Scope::Global);
        assert_eq!((b.name.as_str(), b.prompt.as_str()), ("Plain", "plain prompt"));
        assert!(chrono_date().starts_with("20"));
    }

    #[test]
    fn missing_builtins_are_restored() {
        let mut out = vec![parse("---\nname: Plan\n---\nmine", "Plan", Scope::Local)];
        ensure_builtins(&mut out);
        let mut names: Vec<_> = out.iter().map(|a| a.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Auto", "Build", "Plan"]);
        assert_eq!(find(&out, "Plan").unwrap().prompt, "mine");
        assert_eq!(find(&out, "Auto").unwrap().scope, Scope::Builtin);
    }

    #[test]
    fn readonly_parent_spawns_readonly_only() {
        let plan = parse(DEFAULT_PLAN, "Plan", Scope::Global);
        let auto = parse(DEFAULT_AUTO, "Auto", Scope::Global);
        let build = parse(DEFAULT_BUILD, "Build", Scope::Builtin);
        assert!(is_readonly(&plan) && is_readonly(&auto) && !is_readonly(&build));
        assert!(can_spawn(&plan, &plan) && !can_spawn(&plan, &build));
        assert!(can_spawn(&auto, &build) && can_spawn(&build, &build));
    }

    #[test]
    fn settings_override_frontmatter_per_agent() {
        let mut settings = config::Settings::default();
        let mut a = parse("---\nname: Build\nmodel: openai/gpt-5\neffort: low\n---\nx", "x", Scope::Builtin);
        assert_eq!(model_of(&a, &settings).as_deref(), Some("openai/gpt-5"));
        assert_eq!(effort_of(&a, &settings).as_deref(), Some("low"));
        settings.agents.insert("build".into(), config::AgentSettings { model: "anthropic/claude-haiku-5-5".into(), effort: "medium".into() });
        assert_eq!(model_of(&a, &settings).as_deref(), Some("anthropic/claude-haiku-5-5"));
        assert_eq!(effort_of(&a, &settings).as_deref(), Some("medium"));
        a.name = "Plan".into();
        assert_eq!(model_of(&a, &settings).as_deref(), Some("openai/gpt-5"));
    }
}
