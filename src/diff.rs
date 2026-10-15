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
    let r = git(cwd, &["read-tree", "HEAD"], i).and_then(|_| git(cwd, &["add", "-A"], i)).and_then(|_| git(cwd, &["write-tree"], i));
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
        std::fs::write(d.join("b.txt"), "two\n").unwrap();
        let t = diff(&d, "turn", None, Some(&base)).unwrap();
        assert!(t.contains("b.txt") && !t.contains("a.txt"));
        assert!(diff(&d, "session", None, None).unwrap().contains("a.txt"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
