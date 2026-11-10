//! `/diff`: what changed in the working tree since a point of the conversation, via throwaway git trees.

use std::path::Path;
use std::process::Command;

pub const SCOPES: [&str; 3] = ["session", "turn", "unpushed"];

fn git(cwd: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut c = Command::new("git");
    c.args(args).current_dir(cwd);
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    let o = c.output().map_err(|e| format!("cannot run git: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

/// Tree object of the whole working directory (tracked, modified and untracked, honoring .gitignore).
pub fn snapshot(cwd: &Path) -> Result<String, String> {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let idx = std::env::temp_dir().join(format!("theta-diff-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let i = Some(idx.as_path());
    // Seeded from the real index, `add -A` reuses its stat cache instead of hashing every file.
    let seeded = git(cwd, &["rev-parse", "--git-path", "index"], None).is_ok_and(|p| std::fs::copy(cwd.join(p), &idx).is_ok());
    let r = if seeded { Ok(String::new()) } else { git(cwd, &["read-tree", "HEAD"], i) }.and_then(|_| git(cwd, &["add", "-A"], i)).and_then(|_| git(cwd, &["write-tree"], i));
    let _ = std::fs::remove_file(&idx);
    r
}

/// Unified diff for `scope`: `session`/`turn` compare against a snapshot taken earlier (`None` = HEAD),
/// `unpushed` against the merge base with the upstream branch, so commits and uncommitted work both count.
pub fn diff(cwd: &Path, scope: &str, session: Option<&str>, turn: Option<&str>) -> Result<String, String> {
    git(cwd, &["rev-parse", "--is-inside-work-tree"], None).map_err(|_| "not a git repository".to_string())?;
    let base = match scope {
        "turn" => turn.map(String::from),
        "session" => session.map(String::from),
        "unpushed" => {
            let up = ["@{upstream}", "origin/HEAD"].into_iter().find(|u| git(cwd, &["rev-parse", "--verify", "--quiet", u], None).is_ok());
            let up = up.ok_or("no upstream branch to compare with")?;
            Some(git(cwd, &["merge-base", "HEAD", up], None)?)
        }
        s => return Err(format!("unknown scope {s}: {}", SCOPES.join(" | "))),
    };
    let base = base.map_or_else(|| git(cwd, &["rev-parse", "HEAD"], None), Ok)?;
    let now = snapshot(cwd)?;
    git(cwd, &["diff", "--no-color", "--no-ext-diff", &base, &now], None)
}

/// Put the working tree back to `tree` (a snapshot taken earlier): files changed or deleted since are rewritten,
/// files created since are removed; ignored files and the index are left alone. The state it replaces is kept under
/// a ref first, so the restore itself can be undone. Returns (files rewritten or removed, that ref).
pub fn restore(cwd: &Path, tree: &str) -> Result<(usize, String), String> {
    // From the top of the repository: the paths git lists are relative to it.
    let top = std::path::PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"], None)?);
    let cwd = top.as_path();
    let now = snapshot(cwd)?;
    if now == tree {
        return Ok((0, String::new()));
    }
    let keep = git(cwd, &["-c", "user.name=theta", "-c", "user.email=theta@localhost", "commit-tree", &now, "-m", "theta: before /undo files"], None)?;
    let name = format!("refs/theta/undo/{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()));
    git(cwd, &["update-ref", &name, &keep], None)?;
    let list = |filter: &str| -> Result<Vec<String>, String> {
        Ok(git(cwd, &["diff", "--name-only", "-z", "--no-renames", &format!("--diff-filter={filter}"), tree, &now], None)?.split('\0').filter(|p| !p.is_empty()).map(String::from).collect())
    };
    let (added, changed) = (list("A")?, list("MD")?);
    let idx = std::env::temp_dir().join(format!("theta-restore-{}", std::process::id()));
    let i = Some(idx.as_path());
    let r = git(cwd, &["read-tree", tree], i).and_then(|_| {
        // Only what differs: untouched files keep their timestamps (no needless rebuild).
        let mut args = vec!["checkout-index", "-f", "--"];
        args.extend(changed.iter().map(String::as_str));
        if changed.is_empty() { Ok(String::new()) } else { git(cwd, &args, i) }
    });
    let _ = std::fs::remove_file(&idx);
    r?;
    for p in &added {
        let _ = std::fs::remove_file(top.join(p));
    }
    Ok((added.len() + changed.len(), name))
}

/// The `diff` tool: the working tree (untracked files included) against `base` (a commit, default HEAD), whole or for
/// one `path`; `stat` gives the per-file summary only.
pub fn against(cwd: &Path, base: Option<&str>, path: Option<&str>, stat: bool) -> Result<String, String> {
    git(cwd, &["rev-parse", "--is-inside-work-tree"], None).map_err(|_| "not a git repository".to_string())?;
    let base = base.unwrap_or("HEAD");
    if base.starts_with('-') {
        return Err("base must be a commit, branch or tag".into());
    }
    let base = git(cwd, &["rev-parse", "--verify", "--quiet", &format!("{base}^{{commit}}")], None).map_err(|_| format!("unknown commit `{base}`"))?;
    let now = snapshot(cwd)?;
    let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
    if stat {
        args.push("--stat");
    }
    args.extend([base.as_str(), now.as_str()]);
    if let Some(p) = path {
        args.extend(["--", p]);
    }
    git(cwd, &args, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_scope_sees_only_changes_after_the_snapshot() {
        let d = std::env::temp_dir().join(format!("theta-diff-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        for a in [&["init", "-q"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "i"]] {
            git(&d, a, None).unwrap();
        }
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        let base = snapshot(&d).unwrap();
        // seeded from a real index (a.txt staged), the tree is still the working tree's
        git(&d, &["add", "a.txt"], None).unwrap();
        assert_eq!(snapshot(&d).unwrap(), base);
        std::fs::write(d.join("b.txt"), "two\n").unwrap();
        let t = diff(&d, "turn", None, Some(&base)).unwrap();
        assert!(t.contains("b.txt") && !t.contains("a.txt"));
        assert!(diff(&d, "session", None, None).unwrap().contains("a.txt"));
        // the tool: untracked files count, a path narrows it, stat summarizes
        let all = against(&d, None, None, false).unwrap();
        assert!(all.contains("+one") && all.contains("+two"));
        assert!(!against(&d, None, Some("b.txt"), false).unwrap().contains("a.txt"));
        assert!(against(&d, None, None, true).unwrap().contains("2 files changed"));
        assert!(against(&d, Some("--output=x"), None, false).is_err() && against(&d, Some("nope"), None, false).is_err());
        // restore: back to `base` (a.txt only), and what was there stays reachable
        std::fs::write(d.join("a.txt"), "changed\n").unwrap();
        let (n, kept) = restore(&d, &base).unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "one\n");
        assert!(!d.join("b.txt").exists());
        assert!(git(&d, &["show", &format!("{kept}:b.txt")], None).unwrap().contains("two"));
        assert_eq!(restore(&d, &base).unwrap().0, 0);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
