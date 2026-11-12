//! Progressive slash-command suggestions.
pub(super) const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a new session"),
    ("/resume", "open a previous session"),
    ("/session", "manage session tabs"),
    ("/tree", "browse and fork the conversation tree"),
    ("/linters", "linters per language and whether they are installed"),
    ("/theme", "choose a color theme"),
    ("/remote", "control theta from a browser"),
    ("/ssh", "run tools on an SSH server"),
    ("/cd", "browse the SSH server's folders"),
    ("/jobs", "background shells, linters and subagents (alt+j)"),
    ("/undo", "undo the last turn and restore its prompt"),
    ("/diff", "view working tree changes"),
    ("/model", "change model"),
    ("/agent", "change agent"),
    ("/skills", "list skills (use $name in a message)"),
    ("/memories", "browse, edit and enable or disable project memory"),
    ("/mcp", "MCP servers: view, login, reconnect, diagnose"),
    ("/effort", "reasoning effort"),
    ("/settings", "settings (verbosity, task models, compaction, web)"),
    ("/verbose", "toggle full / compact output"),
    ("/compact", "compact context now"),
    ("/btw", "side question, not kept in context"),
    ("/title", "rename the session"),
    ("/login", "log in to a provider"),
    ("/logout", "remove provider credentials"),
    ("/usage", "subscription limits and resets (Claude, Codex)"),
    ("/stats", "token statistics across all sessions"),
    ("/copy", "copy the last answer"),
    ("/dump", "nightly: copy the whole session and its subagents as Markdown"),
    ("/reload", "re-read settings, agents, skills, MCP config and the guides"),
    ("/help", "keys and commands"),
    ("/quit", "exit"),
];


/// Informational argument rows never insert their placeholder into the prompt.
#[derive(Debug)]
pub(super) struct Hint {
    pub label: String,
    pub description: String,
    pub completion: Option<String>,
}

impl Hint {
    pub fn choice(value: &str, description: &str) -> Self {
        Self { label: value.into(), description: description.into(), completion: Some(value.into()) }
    }
}

pub(super) fn arguments(before: &str, tabs: usize, providers: &[(&str, &str)]) -> Vec<Hint> {
    let Some(command_end) = before.find(char::is_whitespace) else { return vec![] };
    let command = &before[..command_end];
    let start = before.char_indices().rev().find(|(_, c)| c.is_whitespace()).map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
    let prefix = &before[start..];
    let previous: Vec<_> = before[command_end..start].split_whitespace().collect();
    let base = &before[..start];
    let mut hints = vec![];
    let mut choice = |value: &str, description: &str| {
        if value.starts_with(prefix) {
            hints.push(Hint { label: value.into(), description: description.into(), completion: Some(format!("{base}{value}")) });
        }
    };
    match (command, previous.as_slice()) {
        ("/memories", []) => {
            for (value, description) in [("list", "list notes by page"), ("search", "search notes by keywords"), ("show", "read a note by id"), ("add", "save a short note"), ("edit", "replace a note's content"), ("delete", "delete a note by id"), ("on", "enable project memory"), ("off", "disable memory and keep notes")] {
                choice(value, description);
            }
        }
        ("/memories", ["edit"]) => return guide("<id> <new content>", "replace the content of a note"),
        ("/memories", ["show" | "delete"]) => return guide("<id>", "note id from /memories"),
        ("/memories", ["list"]) => return guide("<page>", "ten notes per page"),
        ("/memories", ["search" | "add"]) => return guide("<text>", "keywords or note content"),
        ("/session", []) => {
            for (value, description) in [("new", "open a new tab"), ("close", "close the current tab"), ("hide", "hide the tab sidebar"), ("show", "show the tab sidebar"), ("width", "set the tab sidebar width")] {
                choice(value, description);
            }
            for n in 1..=tabs { choice(&n.to_string(), "switch to this session tab"); }
        }
        ("/session", ["width"]) => return guide("<columns>", &format!("sidebar width, {} to {} columns", super::tabs::WIDTH_MIN, super::tabs::WIDTH_MAX)),
        ("/diff", args) if args.len() < 2 && !args.contains(&"close") => {
            if !args.iter().any(|a| ["session", "turn", "unpushed"].contains(a)) {
                choice("session", "changes since the conversation began");
                choice("turn", "changes since the last message");
                choice("unpushed", "changes compared with upstream");
            }
            if !args.iter().any(|a| ["popup", "split"].contains(a)) {
                choice("popup", "open changes in a popup");
                choice("split", "show changes beside the chat");
            }
            choice("close", "close the changes view");
        }
        ("/remote", []) => {
            choice("stop", "stop browser access");
            choice("status", "show browser access status");
        }
        ("/undo", []) => choice("files", "also restore files to their state before the turn"),
        ("/effort", []) => {
            for effort in ["low", "medium", "high", "xhigh", "max"] { choice(effort, "set reasoning effort"); }
        }
        ("/theme", []) => {
            for theme in super::theme::THEMES { choice(theme.id, "apply this color theme"); }
        }
        ("/login" | "/logout", []) => {
            for (id, name) in providers { choice(id, name); }
        }
        ("/ssh", _) if previous.first() != Some(&"off") => {
            if previous.is_empty() { choice("off", "return to local tools"); }
            hints.extend(guide("<ssh arguments>", "SSH host, user and connection options"));
        }
        ("/cd", _) => return guide("<directory>", "directory on the SSH server"),
        ("/title", _) => return guide("<name>", "new session title"),
        ("/btw", _) => return guide("<question>", "ask a side question outside the conversation context"),
        _ => {}
    }
    hints
}

fn guide(label: &str, description: &str) -> Vec<Hint> {
    vec![Hint { label: label.into(), description: description.into(), completion: None }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_do_not_list_argument_alternatives() {
        assert!(COMMANDS.iter().all(|(_, description)| !description.contains('|')));
    }

    #[test]
    fn suggestions_follow_argument_and_cursor_position() {
        let hints = arguments("/session w", 3, &[]);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].completion.as_deref(), Some("/session width"));
        let hints = arguments("/session width ", 3, &[]);
        assert_eq!(hints[0].label, "<columns>");
        assert!(hints[0].completion.is_none());
        assert!(arguments("/session new ", 3, &[]).is_empty());
        assert_eq!(arguments("/session 3", 3, &[])[0].completion.as_deref(), Some("/session 3"));
    }

    #[test]
    fn diff_suggests_only_remaining_argument_types() {
        let hints = arguments("/diff turn s", 1, &[]);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].completion.as_deref(), Some("/diff turn split"));
        let hints = arguments("/diff split ", 1, &[]);
        assert!(hints.iter().any(|h| h.label == "turn"));
        assert!(!hints.iter().any(|h| h.label == "popup"));
        assert!(arguments("/diff turn split ", 1, &[]).is_empty());
        assert!(arguments("/diff close ", 1, &[]).is_empty());
    }

    #[test]
    fn dynamic_values_and_free_text_guides() {
        assert_eq!(arguments("/login ant", 1, &[("anthropic", "Anthropic")])[0].completion.as_deref(), Some("/login anthropic"));
        assert!(arguments("/theme nord", 1, &[]).iter().any(|h| h.label == "nord"));
        for input in ["/title été à Paris", "/btw explain this", "/cd /srv", "/ssh user@host"] {
            assert!(arguments(input, 1, &[]).iter().any(|h| h.completion.is_none()));
        }
        assert!(arguments("/copy ", 1, &[]).is_empty());
    }
}
