# Configuration

Files: `~/.theta/settings.toml` (global), `<project>/.theta/settings.toml` (project). Project values override global ones, key by key (tables merge).
Edit by hand, then `/reload`. Keys set from `/settings` are saved to the global file.
Created on first run with defaults; the file is only written when missing.

## Keys

- `model`: `provider/model`, default `anthropic/claude-sonnet-5-5`.
- `effort`: `low | medium | high | xhigh | max`, default `medium`.
- `verbose`: `full | compact`. `full` shows tool calls and thinking.
- `notify` (default true): bell and terminal notification (OSC 9) when a run that lasted 15 s or more ends or asks a question.
- `[diff]`: `scope = session | turn | unpushed`, `view = popup | split` (defaults of `/diff`, also in `/settings`).
- `tab_orientation`: `horizontal | vertical`. `tab_hidden`: hide the bar manually (`ctrl+x b`). `tab_width`: 14-60, sidebar columns.
- `tab_auto_hide`: `off` (default), `hidden`, or `compact`; vertical bar only. Hover to restore the full sidebar and leave it to collapse. `hidden` frees the whole width (hover the left edge to reveal); `compact` keeps each tab's number and live status (spinner, completion, error). Available in `/settings`.
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
- `enabled` (bool), `threshold` (fraction of window, default 0.8), `keep_recent_tokens` (default 40000), `max_tokens` (ceiling, 0 = none, default 150000), `prune_tokens` (default 90000, 0 = never: above it, tool outputs older than `keep_recent_tokens` are shortened without a model call; each prune rewrites the cached prefix once, so a low value costs more than it saves).

### `[tools]`
- `rtk`: `auto | on | off`, route shell commands through `rtk` when installed.
- `bash_timeout`: seconds, default 120. `max_lines`: lines before middle-truncation, default 300.
- `verify`: command the harness runs after a subagent that can write changed the working tree; its result (`ok` or `FAILED` with the output tail) is appended to the subagent's report. Empty = detect (`cargo test --quiet` with a `Cargo.toml`, `go test ./...` with a `go.mod`), `off` = never.
- `guard` (default true): ask the user before a `bash` command that cannot be undone: `sudo`, `git push --force`, `git reset --hard`, `git clean -f`, `DROP`/`TRUNCATE`, writes to a device, `rm -r` on a path outside the project. In a subagent or with `-p` the command is refused instead. `confirm = ["terraform apply"]` adds regexes that also need a yes.
- `auto_max_lines`: lines of `read`, `grep`, `find`, `ls` and `bash` output the Auto agent itself receives, default 300, 0 = no cap. Sweeps over many files go to `Explore`.
- `auto_nudges`: times the Auto agent is sent back to work when it stops with unfinished todos, default 8, 0 = never. When files changed since its last diff read, it is also sent back to finish whatever the request asked and it left out, and to read `git diff` (3 times per turn at most).
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
- `auth.json`: API keys and OAuth, mode 0600. Each provider stores multiple named API keys or OAuth accounts, an active profile, and rate-limit cooldowns. `/login <provider> [name]` adds and activates a profile; `/accounts` opens the account switcher; `/accounts <provider> <name>` switches directly. HTTP 429 rotates to another available profile, honoring `Retry-After` (60 seconds by default). `/logout <provider> <name>` removes one profile; omitting the name removes all profiles for that provider. Legacy credentials migrate as `default`. CLI equivalents: `theta login <provider> --account <name>`, `theta accounts [provider] [name]`, `theta logout <provider> [name]`.
- `cache/models.json`: model catalog, cached 24h.

## Checks

- Invalid TOML: `/reload` reports the parse error and keeps the previous settings.
- `/linters` shows which linters are configured and whether they are installed.
