# Create an agent

An agent is one markdown file: `<Name>.md`. Put it in `~/.theta/agents/` (global) or `<project>/.theta/agents/` (this project only). Then `/reload`, and it appears in `/agent`.

The file name is the agent name unless frontmatter `name:` is set. A local agent with the same name replaces the global one.

## Format

```markdown
---
name: Review
description: Strict code reviewer, read-only
can: read and search code, run grep, report findings with path:line
cannot: edit files, run shell commands
tools: read, grep, find, ls, todo
model: anthropic/claude-sonnet-5-5
effort: high
---
You are a strict code reviewer. Report only concrete defects, with path:line.
Do not suggest style changes.
```

## Frontmatter keys

- `name`: display name, also used by `/agent`, `task` and `[agents.<name>]`.
- `description`: one line, shown to the agent when it picks a subagent with `list_agents`.
- `can`, `cannot`: short notes shown to the agent choosing a handoff or subagent target.
- `tools`: allowed tools, comma list or `[a, b]`. Omit it for the default set (see `tools.md`: opt-in tools and MCP rules apply).
  - Entries: exact name, `prefix*`, or an MCP server (`mcp__<server>`). Full list and rules in `tools.md` and `mcp.md`.
  - Example to make a no-shell reader: `tools: read, grep, find, ls` (no `bash`, no `write`, no `edit`).
- `model`: `provider/model` for this agent.
- `effort`: `low | medium | high | xhigh | max`.
- `subagent_model`, `subagent_effort`: defaults for subagents this agent spawns.
- `delegate: all`: may spawn any agent. Otherwise a read-only agent can only spawn read-only agents.

Read-only = a `tools` list with no `write`, `edit`, `bash`, no `mcp*` and no `*` wildcard.

## Body

Plain markdown, becomes the agent's system prompt after the SOUL files. Write the job, the steps, the output format. Keep it short: it is sent on every turn.

## Check it works

1. `/reload`.
2. `/agent`, pick it. Or `settings.toml`: `agent = "Review"`.
3. Ask a task. If a tool is missing, check `tools:` spelling.

## Pitfalls

- Missing `---` lines: the whole file is treated as prompt, with the file name as agent name.
- `tools:` with a typo: that tool silently disappears.
- Do not name a new agent `Build`, `Plan` or `Auto` unless you mean to override the shipped one.
