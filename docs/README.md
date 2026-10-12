# theta guides: index

theta is a terminal coding agent. These guides describe its own features and configuration.
Read the guide that matches the request before you change anything.

## Layout

- Global dir: `~/.theta/` (override with env `THETA_HOME`). Holds `settings.toml`, `auth.json`, `mcp-auth.json`, `SOUL.md`, `SOUL.Auto.md`, `agents/`, `skills/`, `sessions/`, `plan/`, `cache/`, `docs/`.
- Project dir: `<project>/.theta/` holds `settings.toml`, `agents/`, `skills/`, `SOUL.md`, `SOUL.<Agent>.md`.
- Project root = nearest ancestor with `.theta/` or `.git/`.
- Project settings override global settings. Local agents and skills override global ones with the same name.
- `AGENTS.md` or `CLAUDE.md` in the project root is added to the system prompt.

## Guides

- `config.md`: every `settings.toml` key, hooks, linters, providers, file layout.
- `mcp.md`: MCP servers, config, tool naming (`mcp__<server>__<tool>`), login, reconnect.
- `tools.md`: every built-in tool, opt-in tools, how to allow or deny tools per agent or globally.
- `agents.md`: built-in agents, switching agents, models per agent, SOUL files.
- `create-agents.md`: writing a new agent file, frontmatter, tools, delegation.
- `skills.md`: skills, `$name` injection, auto skills.
- `remote.md`: `/remote`, browser control, server URL.
- `commands.md`: slash commands and keys.

## Reload

`/reload` re-reads settings, agents, skills, MCP config and these guides. No restart needed.
Settings edited by hand, new agents, new skills and MCP changes all take effect after `/reload`.
