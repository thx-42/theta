# MCP servers

theta connects to MCP servers and gives their tools to the agent. Tools of server `<s>` named `<t>` appear as `mcp__<s>__<t>`.

## Configure

Global `~/.theta/settings.toml` or project `<project>/.theta/settings.toml`. Tables merge: a project can add or override one `[mcp.<name>]` key without repeating the rest.

```toml
# stdio: theta starts the process and talks JSON-RPC over stdin/stdout
[mcp.fs]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = { DEBUG = "1" }          # extra env vars for the process
enabled = true                 # false = listed, not started, status "disabled"

# HTTP: streamable HTTP endpoint
[mcp.notion]
url = "https://mcp.notion.com/mcp"
headers = { Authorization = "Bearer ${NOTION_TOKEN}" }   # optional static auth
```

Rules:
- One transport per server. `command` set = stdio. `url` set = HTTP. If both are set, `url` wins.
- `${VAR}` in `command`, `args`, `env` values, `url` and `headers` is replaced by the env var. Unset var = empty string, no error.
- Server name is the key after `mcp.`. It becomes part of the tool name: keep it short, letters, digits, `_`, `-`.
- Connect timeout: 20 s. A server that does not answer in time shows `connect timeout`.

## Tool names

Rule: `mcp__<server>__<tool>`. Characters other than letters, digits, `_` and `-` become `_`. Total length cut at 64 characters.

Examples:
- server `fs`, tool `read_file` → `mcp__fs__read_file`
- server `my.server`, tool `list items` → `mcp__my_server__list_items`

The name the model sees is the one used in `tools:` and in `[tools] disabled`. Check it with the server's own tool list if unsure. Tool description is prefixed `[MCP <server>]`.

## Who gets MCP tools

- Main agent (`task` depth 0): with no `tools:` in its file, it gets every connected MCP tool.
- Subagents (started with `task`): they get MCP tools only if the agent file names them explicitly in `tools:`. Reason: MCP schemas are large.
- Disconnected or failed server: its tools are absent. No error at startup.

## Restrict tools per agent

Set `tools:` in the agent file (see `create-agents.md`). Only listed tools are offered. Nothing is denied by a separate list: whatever is not in `tools:` is not available.

Entries accepted in `tools:`:
- exact name: `mcp__fs__read_file`
- prefix with `*`: `mcp__fs__*` (all tools of server `fs`), also `mcp__*` (all MCP tools)
- server name without suffix: `mcp__fs` (same as `mcp__fs__*`)
- built-in name: `read`, `bash`, `web_search`, …
- matching is case-insensitive

```markdown
---
name: Reader
tools: read, grep, mcp__fs__read_file, mcp__notion
---
```

Notes:
- An agent with `tools:` containing no `write`, `edit`, `bash`, no `mcp*` entry and no `*` is read-only. Read-only agents cannot spawn writing subagents (unless `delegate: all`). Adding an MCP entry makes the agent non-read-only.
- A `tools:` entry that matches nothing is silently ignored. Typos give no error: check `/mcp` names and the agent's behaviour.

## Disable a tool everywhere

```toml
[tools]
disabled = ["mcp__fs__write_file", "bash"]
```

Applies to all agents and all projects, MCP and built-in names alike. Use it to hide a dangerous tool without editing agent files. There is no per-agent deny list: to deny per agent, leave the tool out of that agent's `tools:`.

## Login (HTTP servers with OAuth)

For an HTTP server that asks for OAuth (no static `Authorization` header needed):

1. Add `[mcp.<name>]` with `url`, then `/reload`.
2. `theta mcp login <name>` (or `l` in `/mcp`): browser opens the provider's login page. theta listens on local port 53693 for the callback.
3. Tokens: `~/.theta/mcp-auth.json`.
4. `o` in `/mcp` (or `theta mcp logout <name>`): deletes stored tokens for that server.

Status `needs login` means the server rejected the token.

## Commands

`/mcp` opens a pop-up listing every server. Up/down selects one; the keys act on it:

- `enter` or `r`: reconnect the server now.
- `l`: log in (OAuth). `o`: log out.
- `d`: write a diagnosis prompt into the input bar (error, transport, env var names, what to check). Send it and theta looks into the config and fixes it.
- `esc`: close.

Each row shows `● name (kind) · N tools` when connected, `○ … needs login` when OAuth is missing, `✗ … <error>` on failure, `○ … disabled` when `enabled = false`.

The CLI keeps `theta mcp list | login | logout <name>`.

`~T tokens/request` is the cost of sending that server's tool definitions on every request. Use it to decide whether a server is worth its cost on a subagent.

## Reload

`/reload` diffs `[mcp.*]` against the running config:
- server removed → stopped and dropped
- server added, or any field changed → connected (or restarted)
- unchanged → keeps running, not restarted

MCP servers belong to the theta daemon. Config is read from the daemon's first project, so a project-level `[mcp.*]` only applies if that project was opened first. Put shared servers in the global file.

## Troubleshooting

- `cannot start <command>`: binary not in PATH of the daemon. Use an absolute path, or check the env set in `env`.
- `connect timeout`: server does not complete the handshake in 20 s. Run the command by hand to see what it prints.
- `unauthorized`: run `theta mcp login <name>`.
- Tool missing from the model: check it is connected (`/mcp`), that the agent's `tools:` matches, and that it is not in `[tools] disabled`.
- Tool name too long: names are cut at 64 characters; a long server name shortens the tool part.
