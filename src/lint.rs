//! Background linter: once the agent leaves an edited file for another one, lint the first in the background.
//! The linter for a file type is the `[linters]` command of its extension; a missing binary means no lint.

use crate::jobs::Kind;
use crate::tools::{ToolCtx, shell};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Called after a successful write/edit of `path`.
pub fn edited(ctx: &ToolCtx, path: &Path) {
    if let Some(prev) = ctx.jobs.swap_last_edit(Some(path.to_path_buf()))
        && prev != path
    {
        spawn(ctx, &prev);
    }
}

/// The agent stopped: lint the file it ended on.
pub fn flush(ctx: &ToolCtx) {
    if let Some(prev) = ctx.jobs.swap_last_edit(None) {
        spawn(ctx, &prev);
    }
}

/// Linters used when `[linters]` has no entry for an extension (`""` there disables it).
const DEFAULTS: &[(&str, &str)] = &[
    ("py", "ruff check {file}"),
    ("js", "eslint {file}"),
    ("jsx", "eslint {file}"),
    ("mjs", "eslint {file}"),
    ("cjs", "eslint {file}"),
    ("ts", "eslint {file}"),
    ("tsx", "eslint {file}"),
    ("rs", "cargo clippy --quiet"),
    ("go", "go vet {file}"),
    ("sh", "shellcheck {file}"),
    ("bash", "shellcheck {file}"),
    ("rb", "rubocop {file}"),
    ("php", "php -l {file}"),
    ("lua", "luacheck {file}"),
    ("c", "cppcheck --quiet {file}"),
    ("cc", "cppcheck --quiet {file}"),
    ("cpp", "cppcheck --quiet {file}"),
    ("h", "cppcheck --quiet {file}"),
    ("hpp", "cppcheck --quiet {file}"),
    ("kt", "ktlint {file}"),
    ("swift", "swiftlint lint {file}"),
    ("md", "markdownlint {file}"),
    ("yml", "yamllint {file}"),
    ("yaml", "yamllint {file}"),
    ("toml", "taplo check {file}"),
    ("css", "stylelint {file}"),
    ("scss", "stylelint {file}"),
    ("sql", "sqlfluff lint {file}"),
    ("zig", "zig fmt --check {file}"),
];

/// Built-in linters overridden by the user's `[linters]`; empty commands are dropped.
pub fn table(settings: &crate::config::Settings) -> std::collections::BTreeMap<String, String> {
    let mut t: std::collections::BTreeMap<String, String> = DEFAULTS.iter().map(|(e, c)| (e.to_string(), c.to_string())).collect();
    t.extend(settings.linters.clone());
    t.retain(|_, c| !c.trim().is_empty());
    t
}

/// First word of a linter command: the binary to look for.
pub fn binary(cmd: &str) -> &str {
    cmd.split_whitespace().next().unwrap_or("")
}

/// One line per distinct linter: `✓ ruff  py` or `✗ eslint  js jsx  — absent`.
pub fn report(settings: &crate::config::Settings) -> Vec<String> {
    let mut by_cmd: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (ext, cmd) in table(settings) {
        by_cmd.entry(cmd).or_default().push(ext);
    }
    let mut lines: Vec<(bool, String)> = by_cmd
        .into_iter()
        .map(|(cmd, exts)| {
            let ok = on_path(binary(&cmd));
            (ok, format!("  {} {:<22} {}{}", if ok { "✓" } else { "✗" }, binary(&cmd), exts.join(" "), if ok { "" } else { "  — absent" }))
        })
        .collect();
    lines.sort_by_key(|(ok, _)| *ok);
    let mut out = vec!["Linters (override with [linters] in settings.toml)".to_string()];
    out.extend(lines.into_iter().map(|(_, l)| l));
    out
}

/// The command for `path`, if its extension has a linter and the binary exists.
/// A missing binary is noted once in the job list as absent.
fn command(ctx: &ToolCtx, path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    let tpl = table(&ctx.settings).remove(ext)?;
    let bin = binary(&tpl);
    if !on_path(bin) {
        ctx.jobs.note_missing(bin, ext);
        return None;
    }
    let file = format!("'{}'", path.display().to_string().replace('\'', r"'\''"));
    Some(if tpl.contains("{file}") { tpl.replace("{file}", &file) } else { tpl })
}

pub fn on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return PathBuf::from(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn spawn(ctx: &ToolCtx, path: &Path) {
    let Some(cmd) = command(ctx, path) else { return };
    let label = path.strip_prefix(&ctx.cwd).unwrap_or(path).display().to_string();
    let (cwd, timeout) = (ctx.cwd.clone(), Duration::from_secs(ctx.settings.tools.bash_timeout));
    ctx.jobs.spawn(Kind::Lint, label, true, move |out| Box::pin(async move { shell::run_into(&cmd, &cwd, &out, Some(timeout)).await }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use std::sync::Arc;

    fn ctx(linters: &[(&str, &str)]) -> ToolCtx {
        let mut s = Settings::default();
        s.linters = linters.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ToolCtx {
            cwd: std::env::temp_dir(),
            settings: Arc::new(s),
            http: reqwest::Client::new(),
            todos: Default::default(),
            read_cache: Default::default(),
            session_id: String::new(),
            jobs: Default::default(),
        }
    }

    #[test]
    fn picks_linter_by_extension_and_binary() {
        let c = ctx(&[("py", "sh -n {file}"), ("rs", "no-such-linter-bin {file}")]);
        assert_eq!(command(&c, Path::new("/t/a b.py")).unwrap(), "sh -n '/t/a b.py'");
        assert!(command(&c, Path::new("/t/a.rs")).is_none() && command(&c, Path::new("/t/a.txt")).is_none());
        let jobs = c.jobs.list();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, crate::jobs::Status::Missing);
        assert!(jobs[0].label.contains("no-such-linter-bin") && jobs[0].label.contains("rs"));
        command(&c, Path::new("/t/b.rs"));
        assert_eq!(c.jobs.list().len(), 1, "absent linter is reported once");
        let off = ctx(&[("py", "")]);
        assert!(table(&off.settings).get("py").is_none() && table(&off.settings).contains_key("go"));
    }

    #[tokio::test]
    async fn lints_the_previous_file_only() {
        let c = ctx(&[("py", "echo linting {file}")]);
        edited(&c, Path::new("/t/a.py"));
        edited(&c, Path::new("/t/a.py"));
        assert!(c.jobs.list().is_empty());
        edited(&c, Path::new("/t/b.py"));
        assert_eq!(c.jobs.list().len(), 1);
        flush(&c);
        assert_eq!(c.jobs.list().len(), 2);
        for _ in 0..100 {
            if c.jobs.list().iter().all(|j| j.status != crate::jobs::Status::Running) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let notes = c.jobs.take_pending();
        assert_eq!(notes.len(), 2);
        assert!(notes.iter().any(|n| n.contains("linting /t/a.py")) && notes.iter().any(|n| n.contains("linting /t/b.py")));
    }
}
