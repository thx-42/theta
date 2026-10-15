# Configuration

Files: `~/.theta/settings.toml` (global), `<project>/.theta/settings.toml` (project). Project values override global ones, key by key (tables merge).
Edit by hand, then `/reload`. Keys set from `/settings` are saved to the global file.
Created on first run with defaults; the file is only written when missing.

## Keys

- `model`: `provider/model`, default `anthropic/claude-sonnet-5-5`.
- `effort`: `low | medium | high | xhigh | max`, default `medium`.
- `verbose`: `full | compact`. `full` shows tool calls and thinking.
- `[diff]`: `scope = session | turn | unpushed`, `view = popup | split` (defaults of `/diff`, also in `/settings`).
- `tab_orientation`: `horizontal | vertical | hidden`. `tab_width`: 14-60, sidebar columns.
- `agent`: default agent name, default `Build`.

### `[models]`: model per background task. `""` = main model.
- `compaction`, `title`, `subagent`: `provider/model`.
- `subagent_effort`: effort for subagents; `""` = effort of the spawning agent.

### `[agents.<Name>]`: default model and effort for one agent (overrides the agent file, CLI flags still win).
```toml
[agents.Plan]
model = "anthropic/claude-sonnet-5-5"
effort = "high"
```

### `[compaction]`
- `enabled` (bool), `threshold` (fraction of window, default 0.8), `keep_recent_tokens` (default 20000), `max_tokens` (ceiling, 0 = none, default 150000).

### `[tools]`
- `rtk`: `auto | on | off`, route shell commands through `rtk` when installed.
- `bash_timeout`: seconds, default 120. `max_lines`: lines before middle-truncation, default 300.
- `verify`: command the harness runs after a subagent that can write changed the working tree; its result (`ok` or `FAILED` with the output tail) is appended to the subagent's report. Empty = detect (`cargo test --quiet` with a `Cargo.toml`, `go test ./...` with a `go.mod`), `off` = never.
- `auto_max_lines`: lines of `read`, `grep`, `find`, `ls` and `bash` output the Auto agent itself receives, default 80, 0 = no cap. Bigger reads go to a subagent.
- `disabled`: tool names turned off for every agent, built-in or `mcp__<server>__<tool>`. Per-agent control: `tools:` in the agent file (see `tools.md`).

### `[skills]`
- `auto`: skill names always injected. `subagent`: skills injected into every subagent, default `["caveman", "ponytail"]`.

### `[web]`
- `backend`: `exa | firecrawl | brave | tavily | duckduckgo`, default `exa`.

### `[remote]`
- `url`: theta-server base URL for `/remote`. Env `THETA_REMOTE_URL` overrides it.

### `[mcp.<name>]`: see `mcp.md`.

### `[providers.<name>]`: custom OpenAI or Anthropic compatible provider, or override of a built-in one.
```toml
[providers.local]
api = "openai-chat"          # openai-chat | openai-responses | anthropic | google
base_url = "http://localhost:8080/v1"
api_key_env = "LOCAL_KEY"    # or api_key = "..."
models = ["qwen3-coder"]
context = 128000
```

### `[hooks]`: shell commands around each user message.
- `pre_message`: runs with the message on stdin. Exit != 0 rejects the message. Stdout is added as context.
- `post_message`: runs with the last reply on stdin. Output ignored.
- Env: `THETA_SESSION`, `THETA_CWD`, `THETA_STATUS` (post only).

### `[linters]`: linter per file extension, run in background after an edit. `{file}` = edited path.
```toml
[linters]
py = "ruff check {file}"
rs = "cargo clippy --quiet"
```

## Other files

- `SOUL.md` (global) and `<project>/.theta/SOUL.md`: rules for every agent.
- `SOUL.Auto.md`: rules for the Auto agent only (global; project variant `SOUL.Auto.md` also works).
- `auth.json`: API keys and OAuth, mode 0600. Managed with `/login` and `/logout`, not by hand.
- `cache/models.json`: model catalog, cached 24h.

## Checks

- Invalid TOML: `/reload` reports the parse error and keeps the previous settings.
- `/linters` shows which linters are configured and whether they are installed.
