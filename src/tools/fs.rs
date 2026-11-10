//! read / write / edit / ls / find / grep.

use super::{ToolCtx, ToolOut, b, n, resolve, s};
use anyhow::{Context, Result, bail};
use crate::config;
use serde_json::Value;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

const MAX_LINE: usize = 2000;
/// A read stops here even when lines remain: it ends with the `offset=` hint to continue.
const MAX_READ_BYTES: usize = 24_000;

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
    let max = ctx.settings.tools.max_lines.max(50) * 3 / 2;
    let limit = n(args, "limit").map(|l| l as usize).unwrap_or(max).min(max);
    let ranged = args.get("offset").is_some() || args.get("limit").is_some();
    // Per session: a subagent never read what its parent read, so it must get the content, not the "unchanged" stub.
    let key = format!("{}:{offset}:{limit}:{}", path.display(), ctx.session_id);
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
    let mut end = (offset - 1 + limit).min(total);
    let mut out = String::new();
    for (i, l) in lines[offset - 1..end].iter().enumerate() {
        if out.len() > MAX_READ_BYTES {
            end = offset - 1 + i;
            break;
        }
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
    crate::lint::edited(ctx, &path);
    let lines = content.lines().count();
    let display = old.as_deref().map(|o| diff(o, content));
    Ok(ToolOut {
        content: format!("{} {} ({lines} lines)", if old.is_some() { "overwrote" } else { "created" }, rel(ctx, &path)),
        is_error: false,
        display,
    })
}

/// One plan per session: `~/.theta/plan/<session id>.md`. The model cannot choose the file.
/// Lines of the plan's `## Open questions` section that are still open ("none" or empty = settled).
pub fn open_questions(session_id: &str) -> Vec<String> {
    let Ok(plan) = std::fs::read_to_string(config::home().join("plan").join(format!("{session_id}.md"))) else { return vec![] };
    plan.lines()
        .skip_while(|l| !l.trim_start().to_lowercase().starts_with("## open questions"))
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with("## "))
        .map(|l| l.trim().trim_start_matches(['-', '*']).trim())
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("none") && !l.eq_ignore_ascii_case("(none)"))
        .map(String::from)
        .collect()
}

pub fn write_plan(args: &Value, session_id: &str) -> Result<ToolOut> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id.starts_with('.') {
        bail!("no session to attach the plan to");
    }
    let content = s(args, "content").context("content required")?;
    let dir = config::home().join("plan");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{session_id}.md"));
    std::fs::write(&path, content)?;
    let mut out = ToolOut::ok(format!("wrote plan {}", path.display()));
    out.display = Some(content.to_string());
    Ok(out)
}

pub fn edit(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let path = resolve(&ctx.cwd, s(args, "path").context("path required")?);
    let original = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let edits = args.get("edits").and_then(|e| e.as_array()).context("edits required")?;
    // The model's picture of the file is the last read: say so when the file moved since (a formatter, the user, `sed`).
    let stale = {
        let (prefix, suffix, now) = (format!("{}:", path.display()), format!(":{}", ctx.session_id), hash(&original));
        let cache = ctx.read_cache.lock().unwrap();
        let mut seen = cache.iter().filter(|(k, _)| k.starts_with(&prefix) && k.ends_with(&suffix)).map(|(_, h)| *h).peekable();
        seen.peek().is_some() && seen.all(|h| h != now)
    };
    let mut notes: Vec<String> = Vec::new();
    let mut text = original.clone();
    for (i, e) in edits.iter().enumerate() {
        let old = e.get("old").and_then(|v| v.as_str()).context("edit.old required")?;
        let new = e.get("new").and_then(|v| v.as_str()).context("edit.new required")?;
        if old.is_empty() {
            bail!("edit {}: `old` is empty", i + 1);
        }
        let all = e.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
        let count = text.matches(old).count();
        match count {
            1 => text = text.replacen(old, new, 1),
            0 => {
                // Fallbacks: trailing whitespace, then indentation (the replacement is re-indented to the file's).
                if let Some((start, end)) = fuzzy_find(&text, old, false) {
                    text.replace_range(start..end, new);
                } else if let Some((start, end)) = fuzzy_find(&text, old, true) {
                    let fixed = reindent(new, indent_of(old), indent_of(&text[start..end]));
                    text.replace_range(start..end, &fixed);
                    notes.push(format!("edit {}: matched ignoring indentation; `new` was re-indented to the file's", i + 1));
                } else {
                    bail!("edit {}: `old` not found in {}. {}", i + 1, path.display(), near_miss(&text, old));
                }
            }
            c if all => {
                text = text.replace(old, new);
                notes.push(format!("edit {}: {c} occurrences replaced", i + 1));
            }
            c => bail!("edit {}: `old` matches {c} places (lines {}); add surrounding context to make it unique, or set `all` to replace every one", i + 1, match_lines(&text, old)),
        }
    }
    if stale {
        notes.push("note: the file had changed on disk since you last read it; read it again before further edits".into());
    }
    if text == original {
        bail!("edits produced no change");
    }
    std::fs::write(&path, &text)?;
    ctx.read_cache.lock().unwrap().retain(|k, _| !k.starts_with(&path.display().to_string()));
    crate::lint::edited(ctx, &path);
    let d = diff(&original, &text);
    let (plus, minus) = d.lines().fold((0, 0), |(p, m), l| {
        if l.starts_with('+') { (p + 1, m) } else if l.starts_with('-') { (p, m + 1) } else { (p, m) }
    });
    let notes = if notes.is_empty() { String::new() } else { format!("\n{}", notes.join("\n")) };
    Ok(ToolOut { content: format!("edited {} (+{plus} -{minus}){notes}", rel(ctx, &path)), is_error: false, display: Some(d) })
}

/// 1-based line numbers where `needle` starts, at most ten.
fn match_lines(hay: &str, needle: &str) -> String {
    let lines: Vec<String> = hay.match_indices(needle).take(10).map(|(i, _)| (hay[..i].matches('\n').count() + 1).to_string()).collect();
    lines.join(", ")
}

/// Why `old` did not match, in a form that saves the model a re-read: where its first line is and where the text departs.
fn near_miss(hay: &str, old: &str) -> String {
    let want: Vec<&str> = old.lines().map(str::trim).collect();
    let Some(first) = want.iter().position(|l| !l.is_empty()) else { return "Re-read the file and copy the text exactly.".into() };
    let lines: Vec<&str> = hay.lines().collect();
    let cut = |l: &str| l.chars().take(160).collect::<String>();
    // The candidate that agrees with `old` the longest.
    let best = (0..lines.len())
        .filter(|&i| lines[i].trim() == want[first])
        .map(|i| (i, (first..want.len()).take_while(|&j| lines.get(i + j - first).is_some_and(|l| l.trim() == want[j])).count()))
        .max_by_key(|&(_, n)| n);
    match best {
        Some((i, n)) => {
            let at = i + n;
            let theirs = want.get(first + n).copied().unwrap_or("");
            format!("Its first line is at line {}, but line {} differs: the file has `{}`, `old` has `{}`. Copy the text from the file exactly.", i + 1, at + 1, cut(lines.get(at).copied().unwrap_or("(end of file)").trim()), cut(theirs))
        }
        None => format!("No line of the file matches its first line `{}`: the code moved or was never there. `grep` for it, then read that part.", cut(want[first])),
    }
}

/// Leading whitespace of the first non-blank line.
fn indent_of(s: &str) -> &str {
    let l = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    &l[..l.len() - l.trim_start().len()]
}

/// Move `text` from the indentation the model wrote (`from`) to the file's (`to`): the first-line indent is swapped on
/// every line that carries it, and spaces deeper than it become tabs when the file indents with tabs.
fn reindent(text: &str, from: &str, to: &str) -> String {
    if from == to {
        return text.to_string();
    }
    // Spaces per level the model used, when the file uses tabs: guessed from the first-line indents.
    let unit = (to.starts_with('\t') && !from.is_empty() && from.chars().all(|c| c == ' ') && from.len() % to.len() == 0).then(|| from.len() / to.len());
    let mut out: Vec<String> = Vec::new();
    for l in text.split('\n') {
        let Some(rest) = l.strip_prefix(from) else {
            out.push(l.to_string());
            continue;
        };
        let deeper = rest.len() - rest.trim_start_matches(' ').len();
        match unit {
            Some(u) if u > 0 && deeper % u == 0 => out.push(format!("{to}{}{}", "\t".repeat(deeper / u), &rest[deeper..])),
            _ => out.push(format!("{to}{rest}")),
        }
    }
    out.join("\n")
}

/// Match `needle` line by line, ignoring trailing whitespace (and leading too with `any_indent`). Returns byte range in `hay`.
fn fuzzy_find(hay: &str, needle: &str, any_indent: bool) -> Option<(usize, usize)> {
    let norm = |l: &str| if any_indent { l.trim().to_string() } else { l.trim_end().to_string() };
    let want: Vec<String> = needle.lines().map(norm).collect();
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
        if (0..want.len()).all(|j| norm(lines[i + j]) == want[j]) {
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
        let (s, e) = fuzzy_find(hay, "fn a() {\n    x();", false).unwrap();
        assert_eq!(&hay[s..e], "fn a() {  \n    x();");
        assert!(fuzzy_find("a\nb\na\nb\n", "a\nb", false).is_none());
    }

    fn ctx_in(dir: &std::path::Path) -> crate::tools::ToolCtx {
        crate::tools::ToolCtx {
            cwd: dir.to_path_buf(),
            settings: Default::default(),
            http: reqwest::Client::new(),
            todos: Default::default(),
            read_cache: Default::default(),
            session_id: "s".into(),
            jobs: Default::default(),
            ssh: Default::default(),
        }
    }

    #[test]
    fn edit_explains_failures_and_tolerates_indentation() {
        let dir = std::env::temp_dir().join(format!("theta-edit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.go");
        let c = ctx_in(&dir);
        let run = |edits: serde_json::Value| edit(&serde_json::json!({"path": "a.go", "edits": edits}), &c);
        std::fs::write(&f, "func a() {\n\tif x {\n\t\ty()\n\t}\n}\nfoo()\nfoo()\n").unwrap();
        // several matches: the lines are named, `all` replaces them
        let e = format!("{:#}", run(serde_json::json!([{"old": "foo()", "new": "bar()"}])).err().unwrap());
        assert!(e.contains("2 places") && e.contains("lines 6, 7") && e.contains("`all`"), "{e}");
        let ok = run(serde_json::json!([{"old": "foo()", "new": "bar()", "all": true}])).unwrap();
        assert!(ok.content.contains("2 occurrences replaced"), "{}", ok.content);
        // the model wrote spaces where the file has tabs: matched, and the replacement gets tabs
        let ok = run(serde_json::json!([{"old": "    if x {\n        y()\n    }", "new": "    if x {\n        y()\n        z()\n    }"}])).unwrap();
        assert!(ok.content.contains("ignoring indentation"), "{}", ok.content);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "func a() {\n\tif x {\n\t\ty()\n\t\tz()\n\t}\n}\nbar()\nbar()\n");
        // not found: where the text departs
        let e = format!("{:#}", run(serde_json::json!([{"old": "if x {\n    w()", "new": ""}])).err().unwrap());
        assert!(e.contains("first line is at line 2") && e.contains("line 3 differs") && e.contains("y()"), "{e}");
        let e = format!("{:#}", run(serde_json::json!([{"old": "nothing like this", "new": ""}])).err().unwrap());
        assert!(e.contains("No line of the file matches"), "{e}");
        // changed behind the model's back since its read
        read(&serde_json::json!({"path": "a.go"}), &c).unwrap();
        std::fs::write(&f, "one\ntwo\n").unwrap();
        let ok = run(serde_json::json!([{"old": "two", "new": "2"}])).unwrap();
        assert!(ok.content.contains("changed on disk"), "{}", ok.content);
        assert!(!run(serde_json::json!([{"old": "one", "new": "1"}])).unwrap().content.contains("changed on disk"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_stub_is_per_session() {
        let f = std::env::temp_dir().join(format!("theta-read-test-{}", std::process::id()));
        std::fs::write(&f, "hello\n").unwrap();
        let args = serde_json::json!({"path": f.display().to_string()});
        let parent = crate::tools::ToolCtx {
            cwd: std::env::temp_dir(),
            settings: Default::default(),
            http: reqwest::Client::new(),
            todos: Default::default(),
            read_cache: Default::default(),
            session_id: "parent".into(),
            jobs: Default::default(),
            ssh: Default::default(),
        };
        let mut sub = parent.clone();
        sub.session_id = "sub".into();
        assert!(read(&args, &parent).unwrap().content.contains("hello"));
        assert!(read(&args, &parent).unwrap().content.contains("unchanged"));
        assert!(read(&args, &sub).unwrap().content.contains("hello"));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn plan_is_written_per_session() {
        let home = std::env::temp_dir().join(format!("theta-plan-test-{}", std::process::id()));
        let _g = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("THETA_HOME", &home) };
        let out = write_plan(&serde_json::json!({"content": "# plan"}), "abc123").unwrap();
        assert!(!out.is_error);
        assert_eq!(out.display.as_deref(), Some("# plan"));
        assert_eq!(std::fs::read_to_string(home.join("plan/abc123.md")).unwrap(), "# plan");
        assert!(write_plan(&serde_json::json!({"content": "x"}), "../evil").is_err());
        assert!(open_questions("abc123").is_empty() && open_questions("missing").is_empty());
        let draft = "# p\n## Open questions\n- Q1 does RegisterNpc return a key? → MCP test\n\n## Steps\nS1 x";
        write_plan(&serde_json::json!({"content": draft}), "abc123").unwrap();
        assert_eq!(open_questions("abc123"), vec!["Q1 does RegisterNpc return a key? → MCP test"]);
        write_plan(&serde_json::json!({"content": "## Open questions\n- none\n## Steps"}), "abc123").unwrap();
        assert!(open_questions("abc123").is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }
}
