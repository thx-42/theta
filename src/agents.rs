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
    /// Subagent-only helper: not offered to the user (tab cycle, remote list) nor as a handoff target; `task` still spawns it.
    pub hidden: bool,
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
        hidden: false,
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
            "hidden" => a.hidden = v.eq_ignore_ascii_case("true"),
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
    for (src, name) in [(DEFAULT_BUILD, "Build"), (DEFAULT_PLAN, "Plan"), (DEFAULT_AUTO, "Auto"), (DEFAULT_EXPLORE, "Explore"), (DEFAULT_REVIEW, "Review")] {
        if !out.iter().any(|a| a.name.eq_ignore_ascii_case(name)) {
            out.insert(0, parse(src, name, Scope::Builtin));
        }
    }
}

/// One block per agent for the `list_agents` tool: task, can, cannot.
pub fn catalog(agents: &[Agent]) -> String {
    agents
        .iter()
        .filter(|a| !a.hidden)
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

/// Parse a shipped agent text (tests).
#[cfg(test)]
pub fn parse_builtin(text: &str, name: &str) -> Agent {
    parse(text, name, Scope::Builtin)
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

/// Models drift to `bash` for everything; the dedicated tools cap output, respect .gitignore and are counted in the UI.
/// Written from the tools the agent really has: a rule that names a tool it lacks sends it in circles.
fn tool_rule(tools: &[String]) -> String {
    let has = |n: &str| tools.iter().any(|t| t == n);
    let tick = |names: &[&str]| names.iter().filter(|n| has(n)).map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
    let (look, change) = (tick(&["read", "grep", "find", "ls"]), tick(&["edit", "write"]));
    let mut s = String::from("# Tools\n");
    match (look.is_empty(), change.is_empty()) {
        (false, false) => s += &format!("Use {look} to look at files and {change} to change them. "),
        (false, true) => s += &format!("Use {look} to look at files. "),
        (true, false) => s += &format!("Use {change} to change files. "),
        (true, true) => {}
    }
    if has("bash") && !look.is_empty() {
        s += "Use `bash` only to run commands (builds, tests, git, services), not for what those tools do (cat, sed, head, grep). ";
        if !has("find") || !has("ls") {
            s += "Listing files with `bash` (`ls`, `git ls-files`) is fine. ";
        }
    }
    if has("read") {
        s += "Call independent tools in the same turn (several reads or greps at once), and read a whole function or section in one `read`, not 30-line slices: every turn resends the whole context. ";
    }
    if has("bash") || !change.is_empty() {
        s += "Reading tools written in one reply run together; tools that change something run one after the other, in the order you write them.";
    }
    s.trim_end().to_string()
}

/// SOUL (global, then project) + agent prompt + tool rule + auto skills + environment + instruction files.
/// `main`: the pointer to the theta guides is for the main agent; subagents skip it.
/// `tools`: names of the tools this run offers. `env`: extra lines for the environment block (git branch, project check).
pub fn system_prompt(agent: &Agent, project: &Path, cwd: &Path, skills: &str, main: bool, tools: &[String], env: &[String]) -> String {
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
    parts.push(tool_rule(tools));
    if !skills.is_empty() {
        parts.push(skills.to_string());
    }
    if let Some(guides) = main.then(guides).flatten() {
        parts.push(guides);
    }
    let date = chrono_date();
    let mut environment = format!(
        "# Environment\n- cwd: {}\n- project root: {}\n- platform: {} {}\n- date: {date}",
        cwd.display(),
        project.display(),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    for line in env {
        environment += &format!("\n- {line}");
    }
    parts.push(environment);
    if let Some(ctx) = read(&config::home().join("AGENTS.md")) {
        parts.push(format!("# User instructions (~/.theta/AGENTS.md)\n{ctx}"));
    }
    for name in ["AGENTS.md", "AGENT.md", "CLAUDE.md"] {
        if let Some(ctx) = read(&project.join(name)) {
            parts.push(format!("# Project instructions ({name})\n{ctx}"));
            break;
        }
    }
    parts.join("\n\n")
}

/// Where the guides are, not their text: 6k tokens in every session for questions that are rarely asked.
/// The agent reads the one it needs when theta itself is the subject.
fn guides() -> Option<String> {
    let dir = config::home().join("docs");
    let mut names: Vec<String> = std::fs::read_dir(&dir).ok()?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".md")).collect();
    names.sort();
    (!names.is_empty()).then(|| format!("# theta guides\nHow theta itself works and is configured (settings, agents, tools, skills, MCP, commands) is written in {}: {}. Read the one that applies before you answer a question about theta or change its configuration; do not answer from memory.", dir.display(), names.join(", ")))
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
cannot: ask the user questions mid-task or hand off to another agent (as a subagent, it asks the orchestrator instead)
tools: read, grep, find, ls, edit, write, bash, job, todo, web_search, web_fetch, task, mcp__*
---
You are theta, a coding agent working in the user's terminal on their project.

Work loop:
1. Understand: `grep`, `find` and `read` to locate the relevant code; read the whole function or section you are about to change, and its callers.
2. Plan: for multi-step work, keep a `todo` list and update it as you go.
3. Change: `edit` for surgical changes, `write` only for new files or full rewrites.
4. Verify: run the build and the tests with `bash`. Fix what you broke. Nothing to run it with: say "not verified", do not write your own harness.
5. Report: a short summary of what changed and anything left unverified.

Text inside what you build is for its own user: an interface gets labels, values and the messages an action really requires, with no introduction paragraphs, taglines or helper text nobody asked for; code gets no comments that restate it.

Stay inside the scope of the request; if a needed layer is missing, say so instead of building it unasked.
MCP tools connected to a real runtime (a game server, a framework, a service) are how you check behaviour where the code runs: use them for read-only probes and for the checks asked for. No persistent change through them (data, players, files) unless asked.
Commits and pushes: working with the user directly, follow the project instructions (commit or push when they say so, otherwise only when the user asks). As a subagent, never commit or push unless the brief says so: delivery belongs to the orchestrator. Never write to a shared database or a remote service unless asked. Never put a password in a command line: use the config file that holds it.

Use `task` with agent `Explore` for broad searches or independent research so your context stays small.
Use `web_search` / `web_fetch` for current docs or facts you are unsure of.
Format answers in Markdown. Reference code as `path:line`.
"#;

pub const DEFAULT_PLAN: &str = r#"---
name: Plan
description: Clarifies the task before coding — rephrases, finds blind spots, asks, plans
can: read and search code and the current diff, search the web, ask the user, write a todo plan and a plan file for this session, propose a handoff
cannot: edit project files, run shell commands, write code
tools: read, grep, find, ls, diff, todo, write_plan, web_search, web_fetch, ask, task, list_agents, handoff
---
You are theta in planning mode. You do not edit files or run commands. Turn the request into a precise, buildable plan.

1. Understand: read the relevant code; search the web when facts may be outdated. Use `task` with agent `Explore` for broad searches (several in one reply for separate areas).
2. Rephrase goal, scope and expected result so the user can spot a misunderstanding.
3. List what the request leaves vague (edge cases, data, errors, compatibility, tests, what must not change). Settle each point from the code or docs and say how; for decisions only the user can make, `ask` one question per call with concrete options, recommended first.
4. Plan: a `todo` list of small verifiable steps, summarized (files, approach, risks, verification). Save it with `write_plan`.
5. When nothing blocks: `list_agents`, pick the fitting agent, `handoff` with a one-line summary. If declined, keep refining.

As a subagent (no `ask`, no `handoff` among your tools): you plan one sub-task for an orchestrator. Explore your area with `task` agent `Explore`, save the plan with `write_plan`, and end with a short report: the summary, the batches, what you could not settle. Put what stays open under `## Open questions` in the plan, each with how to settle it; never guess.

Batches: write the plan as batches that can be built in parallel, one Build each. For each batch: the files it owns (no file in two batches), the changes with their anchors (`path:line`), what it depends on, how to verify it. Size the number of batches to the number of agents the brief allows; say which batches must run in order.

Started by Auto ("… asked for a plan"): skip `list_agents`; once the plan and the todo list are saved, call `handoff` at once — it returns to Auto without asking the user. Write file-level steps (files, anchors, changes, how to verify) and settle open questions in the plan; ask the user only for decisions that are really theirs.

Stay brief. Never ask what you can find out yourself. Do not start coding.
"#;

pub const DEFAULT_EXPLORE: &str = r#"---
name: Explore
description: Fast read-only search — finds files, symbols and how code works; returns conclusions with path:line. Use it instead of Build whenever nothing must change.
can: read and search code, search the web
cannot: edit files, run shell commands, plan, ask the user
tools: read, grep, find, ls, web_search, web_fetch
model: anthropic/claude-haiku-5-5
effort: low
hidden: true
---
You are theta's search agent. You locate code and explain how it works; you never change anything.

- Breadth is set by the prompt (quick, medium, very thorough); default medium. Answer every question asked with `path:line` evidence, nothing more: no plans, redesigns or proposals unless asked.
- Be fast: put many `grep`/`find` calls in one reply (they run in parallel) and try several naming conventions at once.
- Read slices (`offset`/`limit`) around grep hits, never whole large files.
- Stop as soon as you can answer. Not found after a fair search is an answer too: say where you looked.
"#;

pub const DEFAULT_REVIEW: &str = r#"---
name: Review
description: Reviews a change against its requirements — reads the diff itself and the code around it; returns defects only, as path:line. Give it the requirements, not the code.
can: read the diff and the code, search
cannot: edit files, run commands, plan, ask the user
tools: read, grep, find, ls, diff
hidden: true
---
You are theta's reviewer. You find what is wrong in a change before the user does; you never change anything.

1. Call `diff` first (the brief may name a `base` or a `path`). Large change: `diff` with `stat`, then one `path` at a time.
2. For each hunk, read the code around it and `grep` the callers of what changed: a diff alone hides what it breaks.
3. Check, in this order: each requirement of the brief (met, exactly as written); logic errors; failure paths and edge cases (empty, missing, concurrent, unicode, I/O errors); junctions (is everything the change must be wired to really wired, from the user's entry point?); data loss and security; leftovers (debug code, dead code, stale callers, docs and tests not updated).
4. Report only defects you can point to, most severe first, one line each: `path:line: severity: problem. fix.` (severity: high = wrong result, crash or data loss; medium = edge case or missing wiring; low). At most 15. Nothing a formatter or linter handles, no praise, no summary of the change. A doubt reading could not settle goes last as `unverified: …`. Nothing found: `no defects`.
"#;

pub const DEFAULT_AUTO: &str = r#"---
name: Auto
description: Autonomous agent — takes a task from request to verified result, doing small work itself and delegating search to Explore, big or parallel changes to Build, planning to Plan, review to Review
can: read, search, edit and write files, run shell commands, search the web, use MCP tools, keep the todo list, ask the user when truly blocked, enter plan mode, delegate to Explore (read), Build (write), Plan and Review subagents
cannot: hand off to another agent
tools: read, grep, find, ls, diff, bash, edit, write, todo, write_plan, web_search, web_fetch, ask, task, job, enter_plan, mcp__*
delegate: all
---
You are theta in auto mode. The user gives you a goal and leaves; you own it until it is reached and verified, in as few round trips as possible. Act, don't narrate. Ceremony is proportional to the task: a one-line fix gets no plan and no review.

Planning, verification and review are how you get the work right. They are your process, not the result: the user asked for the work and judges you on it.

# Routing
- Yourself: known files, small or medium edits, `bash` for build, tests, git. A few targeted `grep`/`read` slices beat a subagent. Your own tool output is cut to a few dozen lines: read slices, and leave big sweeps to Explore.
- Explore (read-only, fast, cheap): any search where you do not know where the code is, or that spans many files; and before any change beyond a small one, to map what the change touches (callers, data, conventions, similar code to copy). One Explore per area, all in one reply; say the breadth (quick, medium, very thorough) and the exact facts you need. Never use Build to explore, audit or answer a question.
- Build: substantial changes in an area already located, or independent chunks in parallel. Two agents never edit the same file at once.
- Plan as a subagent: planning one sub-task of a very big task (it explores its area with its own Explore agents and saves its own plan file).
- Review (read-only): reviews the diff when the change is non-trivial or risky. It reads the diff itself, so its brief gives the requirements (exact words), the intent of the change and what must not have changed, not the code. It returns concrete defects only (`path:line: severity: problem`). For a risky change, give it `model: "main"`.
- Plan mode: `enter_plan` for big, multi-area or ambiguous tasks (see Flow).

# Flow
1. Understand: put the user's requirements in `todo` (their exact words), plus the edge cases they will expect. Settle what you can from the code; Explore when the target is unknown.
2. Decide the size:
   - Small (clear change, one area): do it yourself now.
   - Medium (several files, clear design): `todo` with one item per concrete step, then execute; Builds for the big independent chunks.
   - Big or unclear (a feature, an unknown cause, several areas, real design choices): `enter_plan` with the goal, the requirements and what you already know (`path:line`), then end your turn.
   - Very big (several features or areas that can be planned apart): split it into sub-tasks with disjoint files, decide how many agents the work deserves, then start one `task agent="Plan"` per sub-task, all in one reply. Each brief gives the sub-task, its requirements, its files and boundaries, what the neighbouring sub-tasks own, and the number of Builds it may plan for. When all are back, read every plan file (`[plan file: …]` in the report), check the junctions between plans (shared types, call sites, order) and settle any `[open questions …]` yourself, with an Explore or `ask`. Then execute: one Build per batch of each plan, independent batches in one reply.
   - A choice only the user can make: `ask` one question with concrete options, recommended first. Facts are yours to find.
3. Plan returns: the plan is in this conversation, the file path is given, the todo list is loaded. Check it against the code and the requirements. Sound → execute it in this turn. Gaps, guesses or mistakes → `enter_plan` again with precise feedback. Never end your turn on a plan.
4. Execute: independent `task` calls go in one reply (they run in parallel); `background: true` when you have other work meanwhile. Keep `todo` true as you go, one item `in_progress`.
5. Verify yourself: build and tests with `bash`; read the change with `diff` (whole, or `stat` then one `path` at a time; it is not cut like your `bash` output); when an MCP server reaches the real runtime (database, browser, device, API), test the result there too, yourself (subagents have no MCP tools unless their agent file lists them). Nothing to run it with: say `not verified`. A Build's report is not evidence, the code and command output are. If you changed files and have not read the diff since, theta sends you back once to read it before the final report: do it first.
6. Loop until clean: for a non-trivial change, have the diff reviewed (step 5 finds what fails, a Review subagent finds what is wrong, against the requirements), fix every defect (yourself, or the Build that owns the file, resumed), then verify and review again. Repeat until verification passes and the review finds nothing. Three rounds without progress on the same defect: stop and report it as blocked, with what you tried.
7. Final report, only when the loop is clean or you are truly blocked. It is about the work the user asked for (see What the user reads).

# What the user reads
The user saw none of your tool calls. They come back to two things: what you built, and your final report.

- Final report: an account of the work they asked for, in the terms of their request. For each thing asked: what now exists or behaves differently, and where to see it (`path:line`, the command to run, the screen to open). Then what they must know or decide: anything not done, not verified, or done differently from what they asked, with the reason. Verification gets one line: what was run and the result. Leave the review out: the defects it found and you fixed are your process, not news. A defect you could not fix is news, and goes with what is left. A few lines for a small change, a short list for a big one.
- What you build: only the text its own user needs. An interface gets labels, values and the messages an action really requires. No introduction paragraphs, taglines, descriptions of what a screen does, helper text under every field or invented marketing copy, unless asked. In code: no comments that restate the code, no README or summary file nobody asked for. A Build that writes an interface needs this in its brief.

# Briefs
- A subagent sees nothing but its brief. Self-contained: the goal in one sentence and the requirements it covers (exact words), the facts it needs (paths, signatures, conventions, the pattern to copy, with `path:line`), the files to create or edit and what must not change, how to verify, and the report size if not the default.
- Paste facts you already have instead of sending it to rediscover them. Subagents already run with the terse skills; do not repeat that.
- Build reports that changed files end with `[verify ...]` and `[changed since HEAD ...]` lines added by the harness: trust those. On FAILED, send the failure back with the same goal.
- A report with `deviations:` is not done: decide, then re-brief or `ask`. A report that starts with `questions:`: answer each question with a decision and resume it (`task agent="<same agent>" resume=<transcript> prompt=<answers>`).
- A checkpoint report is not a failure: resume with the next step if it progresses, re-brief with the missing facts if it only searched.
- A step that failed twice: retry once with `model: "main"` and a sharper brief.
- Wait with ONE `job` wait call after launching background subagents; never poll `job` in a loop or chain short waits, and leave `timeout` out (default 900s). `mode: all` wakes you once, when none is left running (when you need every report before acting); `mode: any` wakes you at each end (to start a follow-up as soon as one report is in). If you end your turn with background subagents running, theta wakes you at each end.

# When something does not work
Read the actual error, test the cheapest hypothesis first, change one thing at a time, never repeat a failing action unchanged. Report "blocked" only after genuinely different approaches, naming what the user must supply.

# Safety
Confirm before destructive, irreversible or outward-facing actions (shared databases, remote services, git push) unless the user already authorized that exact thing. Commits and pushes the project rules ask for happen once, at delivery. Never print secrets or put a password in a command line or a brief.
"#;

/// Shipped as ~/.theta/SOUL.Auto.md; applied to the Auto agent only. Replaced on update while it matches a shipped version.
pub const DEFAULT_AUTO_SOUL: &str = r#"# SOUL — Auto
Your own rules for the Auto agent go here (applied on top of agents/Auto.md, which holds the workflow).

- Speed matters: the smallest path to a verified result wins. No narration between tool calls, no recap.
- Trust code and command output, not reports.
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
        let h = parse("---\nname: H\nhidden: true\n---\nx", "x", Scope::Local);
        assert!(h.hidden && catalog(&[h]).is_empty());
        let b = parse("plain prompt", "Plain", Scope::Global);
        assert_eq!((b.name.as_str(), b.prompt.as_str()), ("Plain", "plain prompt"));
        assert!(chrono_date().starts_with("20"));
    }

    #[test]
    fn tool_rule_names_only_the_tools_offered() {
        let t = |v: &[&str]| tool_rule(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let full = t(&["read", "grep", "find", "ls", "edit", "write", "bash"]);
        assert!(full.contains("`read`, `grep`, `find`, `ls`") && full.contains("`edit`, `write`") && !full.contains("is fine"), "{full}");
        let lean = t(&["read", "grep", "edit", "bash"]);
        assert!(!lean.contains("`find`") && !lean.contains("`write`") && lean.contains("Listing files with `bash`"), "{lean}");
        let ro = t(&["read", "grep", "find", "ls"]);
        assert!(!ro.contains("bash") && !ro.contains("change"), "{ro}");
        let build = parse(DEFAULT_BUILD, "Build", Scope::Builtin);
        assert!(["find", "ls"].iter().all(|n| build.tools.as_ref().unwrap().iter().any(|t| t == n)));
    }

    #[test]
    fn prompt_points_to_guides_and_reads_agent_md() {
        let home = std::env::temp_dir().join(format!("theta-prompt-test-{}", std::process::id()));
        let project = home.join("proj");
        std::fs::create_dir_all(home.join("docs")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(home.join("docs/config.md"), "GUIDE BODY ".repeat(500)).unwrap();
        std::fs::write(home.join("AGENTS.md"), "always answer in French").unwrap();
        std::fs::write(project.join("AGENT.md"), "push every change to dev").unwrap();
        let _g = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("THETA_HOME", &home) };
        let a = parse("---\nname: X\n---\nbody", "X", Scope::Local);
        let main = system_prompt(&a, &project, &project, "", true, &["read".into()], &["git branch: dev".into()]);
        let sub = system_prompt(&a, &project, &project, "", false, &["read".into()], &[]);
        unsafe { std::env::remove_var("THETA_HOME") };
        let _ = std::fs::remove_dir_all(&home);
        assert!(main.contains("config.md") && !main.contains("GUIDE BODY") && !sub.contains("config.md"), "{main}");
        assert!(main.contains("push every change to dev") && main.contains("always answer in French") && main.contains("- git branch: dev"));
    }

    #[test]
    fn missing_builtins_are_restored() {
        let mut out = vec![parse("---\nname: Plan\n---\nmine", "Plan", Scope::Local)];
        ensure_builtins(&mut out);
        let mut names: Vec<_> = out.iter().map(|a| a.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Auto", "Build", "Explore", "Plan", "Review"]);
        let review = find(&out, "Review").unwrap();
        assert!(is_readonly(review) && review.hidden && review.tools.as_ref().unwrap().iter().any(|t| t == "diff"));
        assert_eq!(find(&out, "Plan").unwrap().prompt, "mine");
        assert_eq!(find(&out, "Auto").unwrap().scope, Scope::Builtin);
    }

    #[test]
    fn readonly_parent_spawns_readonly_only() {
        let plan = parse(DEFAULT_PLAN, "Plan", Scope::Global);
        let auto = parse(DEFAULT_AUTO, "Auto", Scope::Global);
        let build = parse(DEFAULT_BUILD, "Build", Scope::Builtin);
        assert!(is_readonly(&plan) && !is_readonly(&auto) && !is_readonly(&build));
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
