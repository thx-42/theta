//! read / write / edit / ls / find / grep.

use super::{ToolCtx, ToolOut, b, n, resolve, s};
use anyhow::{Context, Result, bail};
use crate::config;
use serde_json::Value;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

const MAX_LINE: usize = 2000;

/// Path as the model should see it: relative to cwd when inside it.
fn rel(ctx: &ToolCtx, p: &std::path::Path) -> String {
    p.strip_prefix(&ctx.cwd).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string())
}

fn hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub fn read(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let raw = s(args, "path").context("path required")?;
    let path = resolve(&ctx.cwd, raw);
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.iter().take(8000).any(|&c| c == 0) {
        return Ok(ToolOut::ok(format!("binary file, {} bytes", bytes.len())));
    }
    let text = String::from_utf8_lossy(&bytes);
    let offset = n(args, "offset").unwrap_or(1).max(1) as usize;
    let max = ctx.settings.tools.max_lines.max(50) * 5;
    let limit = n(args, "limit").map(|l| l as usize).unwrap_or(max).min(max);
    let ranged = args.get("offset").is_some() || args.get("limit").is_some();
    let key = format!("{}:{offset}:{limit}", path.display());
    let h = hash(&text);
    {
        let mut cache = ctx.read_cache.lock().unwrap();
        if cache.get(&key) == Some(&h) {
            return Ok(ToolOut::ok("[unchanged since your last read of this range]"));
        }
        cache.insert(key, h);
    }
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if offset > total.max(1) {
        bail!("offset {offset} beyond end of file ({total} lines)");
    }
    let end = (offset - 1 + limit).min(total);
    let mut out = String::new();
    for l in &lines[offset - 1..end] {
        if l.len() > MAX_LINE {
            out.push_str(&l[..l.floor_char_boundary(MAX_LINE)]);
            out.push_str("…[line truncated]");
        } else {
            out.push_str(l);
        }
        out.push('\n');
    }
    if end < total || ranged {
        out.push_str(&format!("[lines {offset}-{end} of {total}{}]", if end < total { format!("; continue with offset={}", end + 1) } else { String::new() }));
    }
    Ok(ToolOut::ok(out))
}

pub fn write(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let path = resolve(&ctx.cwd, s(args, "path").context("path required")?);
    let content = s(args, "content").context("content required")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let old = std::fs::read_to_string(&path).ok();
    std::fs::write(&path, content)?;
    ctx.read_cache.lock().unwrap().retain(|k, _| !k.starts_with(&path.display().to_string()));
    let lines = content.lines().count();
    let display = old.as_deref().map(|o| diff(o, content));
    Ok(ToolOut {
        content: format!("{} {} ({lines} lines)", if old.is_some() { "overwrote" } else { "created" }, rel(ctx, &path)),
        is_error: false,
        display,
    })
}

/// One plan per session: `~/.theta/plan/<session id>.md`. The model cannot choose the file.
pub fn write_plan(args: &Value, session_id: &str) -> Result<ToolOut> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.starts_with('.') {
        bail!("no session to attach the plan to");
    }
    let content = s(args, "content").context("content required")?;
    let dir = config::home().join("plan");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session_id}.md"));
    std::fs::write(&path, content)?;
    Ok(ToolOut::ok(format!("wrote plan {}", path.display())))
}

pub fn edit(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let path = resolve(&ctx.cwd, s(args, "path").context("path required")?);
    let original = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let edits = args.get("edits").and_then(|e| e.as_array()).context("edits required")?;
    let mut text = original.clone();
    for (i, e) in edits.iter().enumerate() {
        let old = e.get("old").and_then(|v| v.as_str()).context("edit.old required")?;
        let new = e.get("new").and_then(|v| v.as_str()).context("edit.new required")?;
        if old.is_empty() {
            bail!("edit {}: `old` is empty", i + 1);
        }
        let count = text.matches(old).count();
        match count {
            1 => text = text.replacen(old, new, 1),
            0 => {
                // Fallback: tolerate trailing-whitespace differences line by line.
                match fuzzy_find(&text, old) {
                    Some((start, end)) => text.replace_range(start..end, new),
                    None => bail!("edit {}: `old` not found in {}. Re-read the file and copy the text exactly.", i + 1, path.display()),
                }
            }
            c => bail!("edit {}: `old` matches {c} places; add surrounding context to make it unique", i + 1),
        }
    }
    if text == original {
        bail!("edits produced no change");
    }
    std::fs::write(&path, &text)?;
    ctx.read_cache.lock().unwrap().retain(|k, _| !k.starts_with(&path.display().to_string()));
    let d = diff(&original, &text);
    let (plus, minus) = d.lines().fold((0, 0), |(p, m), l| {
        if l.starts_with('+') { (p + 1, m) } else if l.starts_with('-') { (p, m + 1) } else { (p, m) }
    });
    Ok(ToolOut { content: format!("edited {} (+{plus} -{minus})", rel(ctx, &path)), is_error: false, display: Some(d) })
}

/// Match `needle` ignoring trailing whitespace on each line. Returns byte range in `hay`.
fn fuzzy_find(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let want: Vec<&str> = needle.lines().map(|l| l.trim_end()).collect();
    if want.is_empty() {
        return None;
    }
    let mut starts = vec![0];
    for (i, c) in hay.char_indices() {
        if c == '\n' {
            starts.push(i + 1);
        }
    }
    let lines: Vec<&str> = hay.lines().collect();
    let mut found = None;
    for i in 0..lines.len().saturating_sub(want.len() - 1) {
        if (0..want.len()).all(|j| lines[i + j].trim_end() == want[j]) {
            if found.is_some() {
                return None; // ambiguous
            }
            let end_line = i + want.len() - 1;
            found = Some((starts[i], starts[end_line] + lines[end_line].len()));
        }
    }
    found
}

/// Compact unified diff for the UI.
pub fn diff(old: &str, new: &str) -> String {
    let d = similar::TextDiff::from_lines(old, new);
    let mut out = String::new();
    for group in d.grouped_ops(2) {
        for op in group {
            for change in d.iter_changes(&op) {
                let sign = match change.tag() {
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Insert => '+',
                    similar::ChangeTag::Equal => ' ',
                };
                out.push(sign);
                out.push_str(change.value().trim_end_matches('\n'));
                out.push('\n');
            }
        }
        out.push_str("…\n");
    }
    out
}

pub fn ls(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let path = resolve(&ctx.cwd, s(args, "path").unwrap_or("."));
    let mut entries: Vec<String> = std::fs::read_dir(&path)
        .with_context(|| format!("cannot list {}", path.display()))?
        .filter_map(|e| e.ok())
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if e.file_type().is_ok_and(|t| t.is_dir()) { format!("{name}/") } else { name }
        })
        .collect();
    entries.sort();
    let total = entries.len();
    entries.truncate(500);
    let mut out = entries.join("\n");
    if total > 500 {
        out.push_str(&format!("\n[{} more entries]", total - 500));
    }
    Ok(ToolOut::ok(if out.is_empty() { "(empty)".into() } else { out }))
}

fn walker(root: &std::path::Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root).hidden(false).filter_entry(|e| e.file_name() != ".git").build()
}

pub fn find(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let pattern = s(args, "pattern").context("pattern required")?;
    let root = resolve(&ctx.cwd, s(args, "path").unwrap_or("."));
    let limit = n(args, "limit").unwrap_or(300) as usize;
    let pat = if pattern.contains('/') || pattern.starts_with("**") { pattern.to_string() } else { format!("**/{pattern}") };
    let glob = globset::Glob::new(&pat)?.compile_matcher();
    let mut out = Vec::new();
    let mut total = 0;
    for e in walker(&root).filter_map(|e| e.ok()) {
        let Ok(rel) = e.path().strip_prefix(&root) else { continue };
        if rel.as_os_str().is_empty() || !glob.is_match(rel) {
            continue;
        }
        total += 1;
        if out.len() < limit {
            out.push(rel.display().to_string());
        }
    }
    out.sort();
    let mut text = out.join("\n");
    if total > limit {
        text.push_str(&format!("\n[{} more; narrow the pattern]", total - limit));
    }
    Ok(ToolOut::ok(if text.is_empty() { "no matches".into() } else { text }))
}

pub fn grep(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let pattern = s(args, "pattern").context("pattern required")?;
    let root = resolve(&ctx.cwd, s(args, "path").unwrap_or("."));
    let limit = n(args, "limit").unwrap_or(100) as usize;
    let context = n(args, "context").unwrap_or(0) as usize;
    let pat = if b(args, "literal") { regex::escape(pattern) } else { pattern.to_string() };
    let re = regex::RegexBuilder::new(&pat).case_insensitive(b(args, "ignore_case")).build()?;
    let glob = match s(args, "glob") {
        Some(g) => {
            let g = if g.contains('/') { g.to_string() } else { format!("**/{g}") };
            Some(globset::Glob::new(&g)?.compile_matcher())
        }
        None => None,
    };
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut hits = 0;
    let files: Box<dyn Iterator<Item = std::path::PathBuf>> = if root.is_file() {
        Box::new(std::iter::once(root.clone()))
    } else {
        Box::new(walker(&root).filter_map(|e| e.ok()).filter(|e| e.file_type().is_some_and(|t| t.is_file())).map(|e| e.into_path()))
    };
    'files: for path in files {
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
        if let Some(g) = &glob
            && !g.is_match(&rel) {
                continue;
            }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        let mut last_printed: Option<usize> = None;
        for (i, l) in lines.iter().enumerate() {
            if !re.is_match(l) {
                continue;
            }
            hits += 1;
            if hits > limit {
                break 'files;
            }
            let entry = groups.entry(if rel.as_os_str().is_empty() { path.display().to_string() } else { rel.display().to_string() }).or_default();
            let from = i.saturating_sub(context);
            let to = (i + context).min(lines.len() - 1);
            for j in from..=to {
                if last_printed.is_some_and(|p| j <= p) {
                    continue;
                }
                if context > 0 && last_printed.is_some_and(|p| j > p + 1) {
                    entry.push("  --".into());
                }
                let line = lines[j].trim();
                let line = if line.len() > 300 { format!("{}…", &line[..line.floor_char_boundary(300)]) } else { line.to_string() };
                let sep = if j == i { ':' } else { '-' };
                entry.push(format!("  {}{sep} {line}", j + 1));
                last_printed = Some(j);
            }
        }
    }
    if groups.is_empty() {
        return Ok(ToolOut::ok("no matches"));
    }
    let mut out = String::new();
    for (file, lines) in groups {
        out.push_str(&file);
        out.push('\n');
        for l in lines {
            out.push_str(&l);
            out.push('\n');
        }
    }
    if hits > limit {
        out.push_str(&format!("[stopped at {limit} matches; narrow the search]"));
    }
    Ok(ToolOut::ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ignores_trailing_ws() {
        let hay = "fn a() {  \n    x();\n}\n";
        let (s, e) = fuzzy_find(hay, "fn a() {\n    x();").unwrap();
        assert_eq!(&hay[s..e], "fn a() {  \n    x();");
        assert!(fuzzy_find("a\nb\na\nb\n", "a\nb").is_none());
    }

    #[test]
    fn plan_is_written_per_session() {
        let home = std::env::temp_dir().join(format!("theta-plan-test-{}", std::process::id()));
        let _g = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("THETA_HOME", &home) };
        let out = write_plan(&serde_json::json!({"content": "# plan"}), "abc123").unwrap();
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(home.join("plan/abc123.md")).unwrap(), "# plan");
        assert!(write_plan(&serde_json::json!({"content": "x"}), "../evil").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
