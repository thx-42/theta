# Slash commands

- `/new`: new session. Refused when the current one is empty.
- `/resume`: open a previous session.
- `/session`: manage session tabs. Choose `new`, `close`, `hide`, `show`, a tab number, or `width` followed by a column count.
- `/tree`: browse and fork the conversation tree.
- `/undo [files]`: remove the last turn, put its prompt back in the input. `files` also puts the working tree back to what it was when that run started (git projects, same client); the state it replaces is kept under `refs/theta/undo/<time>`.
- `/diff [scope] [view]` (scopes: `session`, `turn`, `unpushed`; views: `popup`, `split`, `close`): what changed in the working tree: since the conversation began, since the last message, or not yet pushed (vs upstream, committed or not). No argument uses `[diff]` in settings. `split` shows a pane beside the chat (wheel scrolls it, refreshed when a run ends; needs 100+ columns); in the popup `tab` cycles the scope.
- `/model`: pick model. `/effort [level]`: reasoning effort (`low`, `medium`, `high`, `xhigh`, `max`).
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
- `/remote`, `/remote stop`, `/remote status`: see `remote.md`.
- `/ssh <args>`, `/ssh off`, `/cd [dir]`: run tools on a remote server.
- `/linters`: configured linters and whether installed.
- `/jobs` (alt+j): background shells, linters, subagents.
- `/reload`: re-read settings, agents, skills, MCP config and these guides.
- `/help`: keys and commands.
- `/quit`: exit.

In the input, besides `/`: `$name` suggests skills, `@path` suggests files and folders (tab goes into a folder, `.gitignore` is honored, `@mod` also finds `src/tui/mod.rs`). The list is as wide as the input.

Slash-command descriptions stay short. After a command and a space, suggestions show only the current argument, filtered as you type. For example, `/session w` suggests `width`, then `/session width ` shows the expected column count. `/diff turn ` suggests the remaining view options. Use ↑/↓ to select and Tab to complete; Enter runs a complete command or completes a partial argument. Free-text placeholders are guidance only and are never inserted. Suggestions follow the cursor and preserve arguments after it.
