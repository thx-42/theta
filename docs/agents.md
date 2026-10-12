# Agents

An agent is a system prompt plus a tool set and model defaults. Each session runs one agent.

## Built-in agents

- `Build`: default. Reads, edits, writes, runs shell commands, builds and tests. Delegates to subagents with `task`.
- `Plan`: read-only planning. Clarifies the task, asks questions, writes a plan with `write_plan`, then hands off to another agent with `handoff`. Cannot edit files or run commands.
- `Auto`: orchestrator. Plans with the user, then runs Build and Plan subagents until the task is verified. Never writes code itself.

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
- `background: true` runs a subagent without waiting. `job` waits for or reads results.
- `/jobs` (alt+j) lists background shells, linters and subagents.
