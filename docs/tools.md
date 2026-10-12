# Tools

Tools are what the agent can call. Each agent sees a subset. Names below are the exact strings used in `tools:` (agent file) and `[tools] disabled` (settings), matched case-insensitively.

## Built-in tools

- `read`: read a file. Large files: use `offset` and `limit`. Results are cached per run.
- `write`: create a file or replace it entirely. Use `edit` for changes to existing files.
- `edit`: exact string replacement in a file.
- `bash`: run a shell command. Timeout `[tools] bash_timeout` (default 120 s). Output truncated after `[tools] max_lines` (default 300). Routed through `rtk` when `[tools] rtk` allows it.
- `grep`: regex search in file contents.
- `find`: find files by glob. Opt-in (needs an explicit `tools:` entry).
- `ls`: list a directory. Opt-in.
- `todo`: maintain the todo panel for the session. Main agent only (depth 0).
- `write_plan`: save the plan file of this session (`~/.theta/plan/<session>.md`). Opt-in.
- `ask`: ask the user a question with options (`choice` or `yes_no`). Main agent only. Opt-in.
- `handoff`: pass the session to another agent with a one-line summary. Main agent only. Opt-in.
- `list_agents`: list agents with their description, can and cannot. Opt-in.
- `task`: start a subagent with a fresh context; only its final report comes back. Main agent only; lists the agents it may spawn (see `agents.md`). `background: true` runs it without waiting.
- `job`: wait for or read background jobs (subagents, shells, linters). Old names `wait`, `jobs`, `job_output`, `job_kill` map to `job`.
- `web_search`, `web_fetch`: web search (backend in `[web]`) and page fetch.

Opt-in tools (`ls`, `find`, `write_plan`, `ask`, `handoff`, `list_agents`) are left out of an agent that has no `tools:` list. Add them to `tools:` to enable them.

## Default set

An agent file without `tools:` gets every built-in tool except the opt-in ones, and (main agent only) every connected MCP tool. Subagents get no MCP tools unless named.

## Allow entries

- exact: `read`, `mcp__fs__read_file`
- prefix: `web_*`, `mcp__fs__*`
- server: `mcp__fs` (all tools of that MCP server)
- `job` also accepts `wait`, `jobs`, `job_output`, `job_kill`

## Remove a tool

- One agent: omit it from its `tools:` list. If the list is absent, add one with everything except the tool.
- Everywhere: `[tools] disabled = ["bash"]` in `settings.toml`. Works for MCP names too.

Warning: a `tools:` list that lacks `read` or `grep` leaves the agent unable to inspect code. Check after editing.
