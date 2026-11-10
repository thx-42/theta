//! bash tool: optional rtk rewrite + output compaction.

use super::{ToolCtx, ToolOut, n, s, truncate_middle};
use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

fn rtk_available() -> bool {
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        std::process::Command::new("rtk")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).starts_with("rtk"))
    })
}

/// Ask rtk for a token-optimized equivalent of `cmd` (empty output = no filter applies).
async fn rtk_rewrite(cmd: &str) -> Option<String> {
    let out = tokio::time::timeout(Duration::from_secs(2), Command::new("rtk").arg("rewrite").arg(cmd).output()).await.ok()?.ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty() && s != cmd).then_some(s)
}

pub fn compact(raw: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let ansi = ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][A-Z0-9]").unwrap());
    let clean = ansi.replace_all(raw, "");
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<String> = None;
    let mut reps = 0;
    let flush = |out: &mut Vec<String>, prev: &Option<String>, reps: usize| {
        if let Some(p) = prev {
            out.push(if reps > 1 { format!("{p}  (×{reps})") } else { p.clone() });
        }
    };
    for line in clean.lines() {
        // Progress bars: keep only what follows the last carriage return.
        let line = line.rsplit('\r').next().unwrap_or(line).trim_end().to_string();
        if !line.is_empty() && prev.as_ref() == Some(&line) {
            reps += 1;
            continue;
        }
        flush(&mut out, &prev, reps);
        prev = Some(line);
        reps = 1;
    }
    flush(&mut out, &prev, reps);
    // Collapse runs of blank lines.
    let mut result = String::new();
    let mut blank = false;
    for l in out {
        if l.is_empty() {
            if blank {
                continue;
            }
            blank = true;
        } else {
            blank = false;
        }
        result.push_str(&l);
        result.push('\n');
    }
    result.trim_end().to_string()
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// The command's process group: killed when dropped while armed (timeout, abort, interrupted run), so no child outlives it.
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            // SAFETY: plain syscall; a negative pid addresses the group the child leads (`process_group(0)`).
            unsafe { kill(-(pid as i32), 9) };
        }
    }
}

/// Spawn `cmd` in bash, leader of its own process group, with stderr merged into a piped stdout.
fn spawn_sh(cmd: &str, cwd: &std::path::Path) -> std::io::Result<tokio::process::Child> {
    let shell = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "sh" };
    Command::new(shell)
        .arg("-c")
        .arg(format!("exec 2>&1\n{cmd}"))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
}

/// Run `cmd`, feeding its output to `sink` as it comes. Returns (exit code, timed out); the code is -1 on timeout.
/// Ends when the shell exits, even if a child it left behind (`server &`) still holds the pipe: that child keeps running.
/// On timeout, or when this future is dropped, the whole process group is killed.
async fn run_with(cmd: &str, cwd: &std::path::Path, timeout: Option<Duration>, sink: &mut (dyn FnMut(&[u8]) + Send)) -> std::io::Result<(i32, bool)> {
    use tokio::io::AsyncReadExt;
    let mut child = spawn_sh(cmd, cwd)?;
    let mut group = Group(child.id());
    let mut stdout = child.stdout.take().unwrap();
    let work = async {
        let read = async {
            let mut buf = [0u8; 8192];
            while let Ok(k) = stdout.read(&mut buf).await {
                if k == 0 {
                    break;
                }
                sink(&buf[..k]);
            }
        };
        tokio::pin!(read);
        let mut eof = false;
        let status = loop {
            tokio::select! {
                _ = &mut read, if !eof => eof = true,
                s = child.wait() => break s,
            }
        };
        if !eof {
            // The shell is gone: take what is already in the pipe, do not wait for a background child to close it.
            let _ = tokio::time::timeout(Duration::from_millis(300), &mut read).await;
        }
        status.ok().and_then(|s| s.code()).unwrap_or(-1)
    };
    let res = match timeout {
        Some(t) => tokio::time::timeout(t, work).await.ok(),
        None => Some(work.await),
    };
    match res {
        Some(code) => {
            group.0 = None; // ended by itself: what it left in the background stays
            Ok((code, false))
        }
        None => Ok((-1, true)), // `group` drops here and kills everything
    }
}

/// Run `cmd`, stream its output into `out`, return the exit code (-1 on spawn failure or timeout).
pub async fn run_into(cmd: &str, cwd: &std::path::Path, out: &crate::jobs::Out, timeout: Option<Duration>) -> i32 {
    let mut sink = |b: &[u8]| out.push(&String::from_utf8_lossy(b));
    match run_with(cmd, cwd, timeout, &mut sink).await {
        Ok((code, false)) => code,
        Ok((_, true)) => {
            out.push(&format!("\ntimed out after {}s\n", timeout.map_or(0, |t| t.as_secs())));
            -1
        }
        Err(e) => {
            out.push(&format!("cannot start: {e}\n"));
            -1
        }
    }
}

async fn effective(cmd: &str, ctx: &ToolCtx) -> String {
    let use_rtk = match ctx.settings.tools.rtk.as_str() {
        "off" => false,
        _ => rtk_available(),
    };
    if use_rtk { rtk_rewrite(cmd).await.unwrap_or(cmd.to_string()) } else { cmd.to_string() }
}

/// Why `cmd` should not run without the user's yes, if it destroys work or reaches outside the project.
/// A short list of commands whose damage cannot be undone from here; everything else runs unasked.
pub fn risky(cmd: &str, cwd: &std::path::Path, extra: &[String]) -> Option<String> {
    static RULES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        [
            (r"(^|[\s;&|(])(sudo|doas)\s", "runs as root"),
            (r"\bgit\s+push\b[^;&|\n]*(\s--force\b|\s--force-with-lease\b|\s-[a-zA-Z]*f[a-zA-Z]*\b|\s\+\S)", "force push"),
            (r"\bgit\s+reset\b[^;&|\n]*--hard", "discards uncommitted work"),
            (r"\bgit\s+clean\b[^;&|\n]*\s-[a-zA-Z]*f", "deletes untracked files"),
            (r"(?i)\b(drop\s+(table|database|schema)|truncate\s+table)\b", "drops data"),
            (r"\bmkfs(\.\w+)?\s|\bdd\b[^;&|\n]*\bof=/dev/", "writes to a device"),
        ]
        .into_iter()
        .map(|(r, why)| (Regex::new(r).unwrap(), why))
        .collect()
    });
    if let Some((_, why)) = rules.iter().find(|(r, _)| r.is_match(cmd)) {
        return Some((*why).to_string());
    }
    // `rm -r` is routine inside the project (build folders); elsewhere it is not.
    for part in cmd.split([';', '&', '|', '\n']) {
        let mut words = part.split_whitespace().skip_while(|w| w.contains('=') || matches!(*w, "command" | "exec" | "time" | "nohup"));
        if words.next() != Some("rm") {
            continue;
        }
        let (flags, targets): (Vec<&str>, Vec<&str>) = words.partition(|w| w.starts_with('-'));
        if !flags.iter().any(|f| *f == "--recursive" || (!f.starts_with("--") && f.contains(['r', 'R']))) {
            continue;
        }
        for t in targets {
            let t = t.trim_matches(['"', '\'']);
            let inside = std::path::Path::new(t).starts_with(cwd) || t.starts_with("/tmp/") || t.starts_with("/var/tmp/");
            let outside = (t.starts_with('/') && !inside) || t.starts_with('~') || t.starts_with('$') || t.split('/').any(|c| c == "..") || matches!(t, "." | "*" | "./" | "./*");
            if outside {
                return Some(format!("recursive delete of `{t}`"));
            }
        }
    }
    extra.iter().filter_map(|p| Regex::new(p).ok().map(|r| (r, p))).find(|(r, _)| r.is_match(cmd)).map(|(_, p)| format!("matches `{p}` in [tools] confirm"))
}

/// Bytes of one foreground command's output kept in memory.
const MAX_CAPTURE: usize = 8 * 1024 * 1024;

/// A command that only views files (cat, sed -n, grep…): `read`/`grep` do it in fewer, capped, cache-aware calls.
fn viewing_only(cmd: &str) -> bool {
    if cmd.contains('>') || cmd.contains("$(") || cmd.contains('`') || cmd.contains('\n') {
        return false;
    }
    let mut parts: Vec<&str> = cmd.split(['|', ';', '&']).map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.first().is_some_and(|p| p.starts_with("cd ")) {
        parts.remove(0);
    }
    !parts.is_empty()
        && parts.iter().all(|p| {
            let mut w = p.split_whitespace();
            match w.next() {
                Some("cat" | "head" | "tail" | "grep" | "rg" | "nl") => true,
                Some("sed") => w.next() == Some("-n") && !p.contains(" -i"),
                _ => false,
            }
        })
}

#[cfg(test)]
pub async fn bash(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    bash_live(args, ctx, &mut |_| {}).await
}

/// `bash`, calling `live` with the last lines printed so far (a few times per second) while the command runs.
pub async fn bash_live(args: &Value, ctx: &ToolCtx, live: &mut (dyn FnMut(String) + Send)) -> Result<ToolOut> {
    let cmd = s(args, "command").context("command required")?.to_string();
    let viewing = viewing_only(&cmd);
    let effective = effective(&cmd, ctx).await;
    if super::b(args, "background") {
        let (run, cwd, timeout) = (effective.clone(), ctx.cwd.clone(), n(args, "timeout").map(Duration::from_secs));
        let id = ctx.jobs.spawn(crate::jobs::Kind::Shell, cmd.clone(), false, move |out| Box::pin(async move { run_into(&run, &cwd, &out, timeout).await }));
        return Ok(ToolOut::ok(format!("started background job {id}: {cmd}\nIts end is reported in a later message; use `job` (output, kill) to look at it or stop it.")));
    }
    let timeout = n(args, "timeout").unwrap_or(ctx.settings.tools.bash_timeout).max(1);
    // Everything the command printed, up to MAX_CAPTURE: the model gets a truncated view, the log file the rest.
    let mut buf: Vec<u8> = Vec::new();
    let mut dropped = 0usize;
    let mut shown = std::time::Instant::now();
    let mut sink = |b: &[u8]| {
        let room = MAX_CAPTURE.saturating_sub(buf.len()).min(b.len());
        buf.extend_from_slice(&b[..room]);
        dropped += b.len() - room;
        if shown.elapsed() >= Duration::from_millis(250) {
            shown = std::time::Instant::now();
            let recent = compact(&String::from_utf8_lossy(&buf[buf.len().saturating_sub(2000)..]));
            let lines: Vec<&str> = recent.lines().filter(|l| !l.trim().is_empty()).collect();
            live(lines[lines.len().saturating_sub(5)..].join("\n"));
        }
    };
    let (code, timed_out) = run_with(&effective, &ctx.cwd, Some(Duration::from_secs(timeout)), &mut sink).await?;
    let raw = String::from_utf8_lossy(&buf);
    let text = compact(&raw);
    let (mut text, cut) = truncate_middle(&text, ctx.settings.tools.max_lines);
    if cut && let Some(file) = super::save_log(raw.as_bytes()) {
        text.push_str(&format!("\n[full output: {}]", file.display()));
    }
    if dropped > 0 {
        text.push_str(&format!("\n[{dropped} more bytes of output were not kept]"));
    }
    if text.is_empty() {
        text = "(no output)".into();
    }
    if timed_out {
        text.push_str(&format!("\n[timed out after {timeout}s: the command and its children were killed; output so far is above. For servers and long runs use `background: true`]"));
    } else if code != 0 {
        text.push_str(&format!("\n[exit {code}]"));
    }
    // A refusal cost a whole extra turn (full context resent) and the model kept retrying: run it, then point to the right tool.
    if viewing {
        text.push_str("\n[next time: `read` (path, offset, limit) to view files and `grep` to search, several in one turn]");
    }
    Ok(ToolOut { content: text, is_error: code != 0, display: (effective != cmd).then(|| format!("$ {effective}")) })
}

#[cfg(test)]
mod tests {
    #[test]
    fn compacts() {
        let raw = "\x1b[32mok\x1b[0m\nsame\nsame\nsame\n\n\n\n10%\r50%\r100%\nend";
        assert_eq!(super::compact(raw), "ok\nsame  (×3)\n\n100%\nend");
    }

    fn ctx() -> crate::tools::ToolCtx {
        crate::tools::ToolCtx {
            cwd: std::env::temp_dir(),
            settings: Default::default(),
            http: reqwest::Client::new(),
            todos: Default::default(),
            read_cache: Default::default(),
            session_id: String::new(),
            jobs: Default::default(),
            ssh: Default::default(),
        }
    }

    #[tokio::test]
    async fn timeout_keeps_output_and_kills_the_group() {
        let t0 = std::time::Instant::now();
        let out = super::bash(&serde_json::json!({"command": "echo before; sleep 30 & echo pid=$!; wait", "timeout": 1}), &ctx()).await.unwrap();
        assert!(out.is_error && out.content.contains("before") && out.content.contains("timed out after 1s"), "{}", out.content);
        assert!(t0.elapsed() < std::time::Duration::from_secs(10));
        let pid = out.content.lines().find_map(|l| l.strip_prefix("pid=")).unwrap().trim().to_string();
        let mut alive = true;
        for _ in 0..100 {
            // A killed child is a zombie until reaped: its state letter is Z.
            alive = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "));
            if !alive {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!alive || !std::path::Path::new("/proc").exists(), "child {pid} survived the timeout");
    }

    #[tokio::test]
    async fn returns_when_the_shell_exits_not_when_the_pipe_closes() {
        let t0 = std::time::Instant::now();
        let out = super::bash(&serde_json::json!({"command": "sleep 4 & echo started", "timeout": 30}), &ctx()).await.unwrap();
        assert!(!out.is_error && out.content.contains("started"), "{}", out.content);
        assert!(t0.elapsed() < std::time::Duration::from_secs(3), "waited for the background child: {:?}", t0.elapsed());
    }

    #[test]
    fn guard_asks_for_what_cannot_be_undone() {
        let cwd = std::path::Path::new("/home/u/proj");
        let risky = |c: &str| super::risky(c, cwd, &["terraform apply".to_string()]);
        for c in ["sudo apt install x", "git push --force origin main", "git push -f", "git push origin +main", "cd x && git reset --hard HEAD~1", "git clean -fdx", "psql -c 'DROP TABLE users'", "rm -rf ~/work", "rm -rf /etc/x", "rm -fr ../other", "FOO=1 rm -r $DIR", "rm -rf *", "terraform apply -auto-approve", "dd if=a of=/dev/sda"] {
            assert!(risky(c).is_some(), "{c}");
        }
        for c in ["rm -rf target", "rm -rf ./node_modules build/", "rm -rf /home/u/proj/dist", "rm -rf /tmp/x", "rm file.txt", "git push origin dev", "git push -u origin dev", "git reset HEAD~1", "git clean -n", "cargo test", "echo pseudo code", "grep -r dropdown ."] {
            assert!(risky(c).is_none(), "{c}: {:?}", risky(c));
        }
    }

    #[test]
    fn viewing_commands_go_to_read_and_grep() {
        for c in ["cat a.rs", "cd x && sed -n '1,30p' f.lua", "grep -rn foo . | head -20", "head -50 a; tail b"] {
            assert!(super::viewing_only(c), "{c}");
        }
        for c in ["cargo test", "sed -i 's/a/b/' f", "grep foo f > out", "ls", "git grep foo", "find . -name x", "cat a | python3 x.py", "cd x && cargo build"] {
            assert!(!super::viewing_only(c), "{c}");
        }
    }
}
