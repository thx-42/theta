//! Context compaction with a (small) dedicated model: summarize old turns, keep recent ones verbatim.

use crate::agent::Runtime;
use crate::catalog::Model;
use crate::config::Settings;
use crate::session::{Kind, Session};
use crate::types::{Block, Msg, Role, estimate_tokens};
use anyhow::{Result, bail};
use std::sync::{Arc, Mutex};

const SYSTEM: &str = "You are a context summarization assistant. Read a conversation between a user and an AI coding agent and produce a structured summary in the exact format requested. Do NOT continue the conversation or answer its questions. ONLY output the summary.";

const FORMAT: &str = "Create a structured context checkpoint that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What the user is trying to accomplish]

## Constraints & Preferences
- [Requirements and preferences the user stated, or (none)]

## Progress
### Done
- [x] [Completed work]
### In Progress
- [ ] [Current work]
### Blocked
- [Issues, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next]

## Critical Context
- [Data, file paths, function names, error messages needed to continue]

Keep each section concise. Preserve exact file paths, function names and error messages.";

/// Tokens the active branch currently occupies (last reported usage + estimate of what came after).
pub fn current_tokens(s: &Session) -> u64 {
    let path = s.path_to(s.leaf.as_deref());
    let mut tail = Vec::new();
    for e in path.iter().rev() {
        match &e.kind {
            Kind::Msg { usage: Some(u), .. } => return u.context() + estimate_tokens(&tail),
            Kind::Msg { msg, .. } => tail.push(msg.clone()),
            Kind::Compaction { .. } => break,
            _ => {}
        }
    }
    estimate_tokens(&s.context())
}

pub fn needed(s: &Session, model: &Model, settings: &Settings) -> bool {
    model.context > 0 && current_tokens(s) as f64 > model.context as f64 * settings.compaction.threshold
}

fn serialize(msgs: &[Msg]) -> String {
    let mut out = String::new();
    for m in msgs {
        for b in &m.content {
            match (m.role, b) {
                (Role::User, Block::Text { text }) => out.push_str(&format!("[User]: {text}\n\n")),
                (Role::Assistant, Block::Text { text }) => out.push_str(&format!("[Assistant]: {text}\n\n")),
                (_, Block::ToolCall { name, args, .. }) => {
                    let a: String = args.to_string().chars().take(400).collect();
                    out.push_str(&format!("[Tool call] {name} {a}\n"));
                }
                (_, Block::ToolResult { content, is_error, .. }) => {
                    let c: String = content.chars().take(800).collect();
                    out.push_str(&format!("[Tool result{}]: {c}\n\n", if *is_error { " (error)" } else { "" }));
                }
                _ => {}
            }
        }
    }
    out
}

/// Summarize everything before the kept tail. Returns tokens before compaction.
pub async fn run(rt: &Runtime, session: &Arc<Mutex<Session>>, main_model: &str) -> Result<u64> {
    let (to_summarize, previous, first_kept, before) = {
        let s = session.lock().unwrap();
        let before = current_tokens(&s);
        let path = s.path_to(s.leaf.as_deref());
        let start = path.iter().rposition(|e| matches!(e.kind, Kind::Compaction { .. })).map(|i| i + 1).unwrap_or(0);
        let previous = path[..start].iter().rev().find_map(|e| match &e.kind {
            Kind::Compaction { summary, .. } => Some(summary.clone()),
            _ => None,
        });
        let entries = &path[start..];
        // Cut before a user text or assistant message (never a tool result), so tool calls stay paired with their results.
        // The summary is a user message, so the kept tail may start with an assistant turn.
        let keep = rt.settings.compaction.keep_recent_tokens;
        let mut acc = 0;
        let mut cut = None;
        for (i, e) in entries.iter().enumerate().rev() {
            let Kind::Msg { msg, .. } = &e.kind else { continue };
            acc += estimate_tokens(std::slice::from_ref(msg));
            let boundary = msg.role == Role::Assistant || msg.content.iter().all(|b| !matches!(b, Block::ToolResult { .. }));
            if boundary && i > 0 {
                cut = Some(i);
                if acc >= keep {
                    break;
                }
            }
        }
        let Some(cut) = cut else { bail!("nothing to compact yet") };
        let msgs: Vec<Msg> = entries[..cut].iter().filter_map(|e| if let Kind::Msg { msg, .. } = &e.kind { Some(msg.clone()) } else { None }).collect();
        (msgs, previous, entries[cut].id.clone(), before)
    };
    let mut prompt = String::new();
    if let Some(p) = &previous {
        prompt.push_str(&format!("<previous-summary>\n{p}\n</previous-summary>\n\nMerge the previous summary with the new messages below.\n\n"));
    }
    prompt.push_str(&format!("# Conversation\n{}\n\n# Instructions\n{FORMAT}", serialize(&to_summarize)));
    let model = rt.task_model("compaction", main_model).await;
    let summary = rt.oneshot(&model, SYSTEM, &prompt).await?;
    if summary.is_empty() {
        bail!("empty summary");
    }
    session.lock().unwrap().add_compaction(summary, first_kept, before)?;
    rt.read_cache.lock().unwrap().clear();
    Ok(before)
}
