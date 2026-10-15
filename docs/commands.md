# Slash commands

- `/new`: new session. Refused when the current one is empty.
- `/resume`: open a previous session.
- `/session [new | close | hide | show | <n> | width <n>]`: session tabs.
- `/tree`: browse and fork the conversation tree.
- `/undo`: remove the last turn, put its prompt back in the input.
- `/diff [session | turn | unpushed] [popup | split | close]`: what changed in the working tree: since the conversation began, since the last message, or not yet pushed (vs upstream, committed or not). No argument uses `[diff]` in settings. `split` shows a pane beside the chat (wheel scrolls it, refreshed when a run ends; needs 100+ columns); in the popup `tab` cycles the scope.
- `/model`: pick model. `/effort [low|medium|high|xhigh|max]`: reasoning effort.
- `/theme [name]`: color theme (theta, light, opencode, tokyonight, catppuccin, gruvbox, nord, dracula, ... run `/theme` for the full list), saved as `theme` in settings. Each theme reshuffles agent colors.
- `/agent`: pick agent. See `agents.md`.
- `/skills`: list skills. See `skills.md`.
- `/mcp`: pop-up of MCP servers (reconnect, login, logout, diagnose). See `mcp.md`.
- `/settings`: verbosity, task models, compaction, web.
- `/verbose`: toggle full and compact output.
- `/compact`: compact context now.
- `/btw <question>`: side question, not kept in context.
- `/title <name>`: rename session.
- `/login [provider]`, `/logout [provider]`: provider credentials.
- `/usage`: subscription limits (Claude, Codex).
- `/stats`: token statistics across sessions.
- `/copy`: copy last answer.
- `/remote [stop|status]`: see `remote.md`.
- `/ssh <args> | off`, `/cd [dir]`: run tools on a remote server.
- `/linters`: configured linters and whether installed.
- `/jobs` (alt+j): background shells, linters, subagents.
- `/reload`: re-read settings, agents, skills, MCP config and these guides.
- `/help`: keys and commands.
- `/quit`: exit.

In the input, besides `/`: `$name` suggests skills, `@path` suggests files and folders (tab goes into a folder, `.gitignore` is honored, `@mod` also finds `src/tui/mod.rs`). The list is as wide as the input.
