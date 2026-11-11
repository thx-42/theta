# Agents

An agent is a system prompt plus a tool set and model defaults. Each session runs one agent.

## Built-in agents

- `Build`: default. Reads, edits, writes, runs shell commands, builds and tests. Delegates to subagents with `task`.
- `Plan`: read-only planning. Clarifies the task, asks questions, writes a plan with `write_plan`, then hands off to another agent with `handoff`. Cannot edit files or run commands.
- `Auto`: autonomous agent. Owns the goal from the request to a result that works, in one go. It chooses its own means: by default it goes straight at the code itself (read, edit, `bash` to build and test); it takes a `todo` list, `Explore` searches, `Build` subagents for big or parallel changes and a `Review` subagent only when the task needs them. Every thing the user names is a requirement; it takes the decisions itself (`ask` is the last resort) and, before changing something and before finishing, checks what consumes it and what it relies on, adapting what is affected. For a big task whose design is the hard part it calls `enter_plan`: the conversation switches to `Plan`, whose `handoff` returns to Auto automatically (no confirmation); Auto then checks the plan and executes it or asks for a revision (3 rounds per user message). A Build report that changed files carries a `[verify ...]` line the harness added by running the project's check (`[tools] verify`). Its own `read`, `grep`, `find`, `ls` and `bash` output is capped (`[tools] auto_max_lines`). A step that failed twice can be retried on Auto's own model with `task` `model: "main"`. Its final report covers the work that was asked (what changed, where to see it, what is left), not the review it ran on the way.
- `Review` (hidden from the picker, subagent only): reviews a change against its requirements. Reads the diff itself with `diff`, then the code around it; returns defects only (`path:line: severity: problem. fix.`). Auto uses it for non-trivial diffs.
- `Explore` (hidden from the picker, subagent only): fast read-only search returning `path:line` facts. Runs on `anthropic/claude-haiku-5-5` at low effort; without Anthropic credentials it falls back to the subagent model. Override with `[agents.Explore]`.

Built-in agents are shipped in the binary. If a file is missing from `~/.theta/agents/`, theta restores it on start. An edited file is kept; the new version is written next to it as `<name>.md.new`.

## Switch agent

- `/agent`: pick from a list.
- `settings.toml`: `agent = "Plan"` sets the default for new sessions.
- Plan agents can hand off with `handoff`; subagents are started with `task` (agent name, prompt).

## Models and effort per agent

Order of precedence: CLI flag > `[agents.<Name>]` in settings > frontmatter in the agent file > global `model`/`effort`.

```toml
[agents.Plan]
model = "anthropic/claude-sonnet-5-5"
effort = "high"
```

Subagent defaults: `[models] subagent`, `[models] subagent_effort`.

## Files

- Global: `~/.theta/agents/<Name>.md`. Project: `<project>/.theta/agents/<Name>.md`. Project wins on the same name.
- To change behaviour: edit the file, then `/reload`.
- To add one: see `create-agents.md`.

## SOUL files

`SOUL.md` (global, plus project `.theta/SOUL.md`) is prepended to every agent's prompt. `SOUL.<Name>.md` is added for one agent only; `SOUL.Auto.md` ships for the Auto agent. Edit them to change shared rules.

## Subagents

- Any agent can run `task` to delegate. A read-only agent (Plan) can only spawn read-only agents, unless it sets `delegate: all` (Auto does).
- Nesting is one level deep and read-only: a read-only subagent may start read-only agents (a Plan subagent starts its own Explore agents), always in the foreground; a writing subagent (Build) starts none; those cannot start anything.
- A subagent that saves a plan with `write_plan` gets its own plan file; its report ends with `[plan file: …]` and, if any, `[open questions in that plan: …]`. Auto uses this to run one Plan per sub-task in parallel.
- `background: true` runs a subagent without waiting. `job` waits for or reads results.
- `/jobs` (alt+j) lists background shells, linters and subagents.
