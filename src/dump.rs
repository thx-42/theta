//! `theta dump` (nightly only): print a whole session as Markdown, subagent transcripts nested under the `task` that spawned them.

use crate::session::{self, Kind, Session};
use crate::types::{Block, Role};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fmt::Write;
use std::path::PathBuf;

/// `target`: session file or id; none = the latest session of the current directory.
pub fn run(target: Option<&str>) -> Result<()> {
    if !crate::update::is_dev() {
        bail!("`theta dump` is only available on the nightly (dev) channel");
    }
    let cwd = std::env::current_dir()?;
    let path = match target {
        Some(t) if PathBuf::from(t).exists() => PathBuf::from(t),
        Some(t) => session::dir_for(&cwd).join(format!("{t}.jsonl")),
        None => session::list(&cwd).first().map(|i| i.path.clone()).context("no session in this directory")?,
    };
    let mut out = String::new();
    render(&path, 0, &mut HashSet::new(), &mut out)?;
    crate::tui::copy_osc52(&out);
    eprintln!("session copied to clipboard ({} bytes)", out.len());
    Ok(())
}

fn indent(depth: usize, s: &str) -> String {
    let pad = "> ".repeat(depth);
    s.lines().map(|l| format!("{pad}{l}\n")).collect()
}

fn fence(s: &str) -> String {
    format!("````\n{s}\n````")
}

fn render(path: &std::path::Path, depth: usize, seen: &mut HashSet<PathBuf>, out: &mut String) -> Result<()> {
    if !seen.insert(path.to_path_buf()) {
        return Ok(());
    }
    let s = Session::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buf = String::new();
    let _ = writeln!(buf, "# {} — {}\n{}\n", if depth == 0 { "session" } else { "subagent" }, s.title.as_deref().unwrap_or(&s.id), path.display());
    let mut nested: Vec<PathBuf> = vec![];
    for e in &s.entries {
        match &e.kind {
            Kind::Session { cwd } => {
                let _ = writeln!(buf, "cwd: `{cwd}`\n");
            }
            Kind::Title { title } => {
                let _ = writeln!(buf, "_title: {title}_\n");
            }
            Kind::Leaf { target } => {
                let _ = writeln!(buf, "_branch switched to `{target}`_\n");
            }
            Kind::Model { agent, model } => {
                let _ = writeln!(buf, "_model for {agent}: {}_\n", if model.is_empty() { "default" } else { model });
            }
            Kind::Compaction { summary, tokens_before, .. } => {
                let _ = writeln!(buf, "## compaction ({tokens_before} tokens before)\n{}\n", fence(summary));
            }
            Kind::Msg { msg, usage } => {
                let who = match msg.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let u = usage.as_ref().map(|u| format!(" · in {} out {} cache r{} w{}", u.input, u.output, u.cache_read, u.cache_write)).unwrap_or_default();
                let _ = writeln!(buf, "## {who} `{}` ts={}{}{u}\n", msg.model.as_deref().unwrap_or(""), e.ts, if e.parent.is_some() { "" } else { " (root)" });
                for b in &msg.content {
                    let text = match b {
                        Block::Text { text } => format!("{}\n", fence(text)),
                        Block::Thinking { text, .. } => format!("**thinking**\n{}\n", fence(text)),
                        Block::ToolCall { id, name, args, .. } => {
                            format!("**tool_call** `{name}` `{id}`\n{}\n", fence(&serde_json::to_string_pretty(args).unwrap_or_default()))
                        }
                        Block::ToolResult { id, content, is_error } => {
                            format!("**tool_result** `{id}`{}\n{}\n", if *is_error { " (error)" } else { "" }, fence(content))
                        }
                        Block::Image { mime, data } => format!("**image** {mime} ({} bytes base64)\n", data.len()),
                    };
                    buf.push_str(&text);
                    if let Block::ToolResult { content: t, .. } | Block::Text { text: t } = b {
                        for p in transcripts(t) {
                            nested.push(p);
                        }
                    }
                    buf.push('\n');
                }
            }
        }
    }
    out.push_str(&indent(depth, &buf));
    for p in nested {
        render(&p, depth + 1, seen, out)?;
    }
    Ok(())
}

/// Paths in `[transcript: <path>]` markers (the report footer of a `task` result).
fn transcripts(text: &str) -> Vec<PathBuf> {
    text.split("[transcript: ").skip(1).filter_map(|r| r.split(']').next()).map(PathBuf::from).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_transcript_markers() {
        let t = "report\n[transcript: /a/b.jsonl]\nmore [transcript: /c.jsonl]";
        assert_eq!(super::transcripts(t), vec![std::path::PathBuf::from("/a/b.jsonl"), "/c.jsonl".into()]);
    }
}
