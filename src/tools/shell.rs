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

/// Spawn `cmd` in bash with stderr merged into a piped stdout. The child dies with its handle.
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
        .kill_on_drop(true)
        .spawn()
}

/// Run `cmd`, stream its output into `out`, return the exit code (-1 on spawn failure or timeout).
pub async fn run_into(cmd: &str, cwd: &std::path::Path, out: &crate::jobs::Out, timeout: Option<Duration>) -> i32 {
    use tokio::io::AsyncReadExt;
    let mut child = match spawn_sh(cmd, cwd) {
        Ok(c) => c,
        Err(e) => {
            out.push(&format!("cannot start: {e}\n"));
            return -1;
        }
    };
    let mut stdout = child.stdout.take().unwrap();
    let work = async {
        let mut buf = [0u8; 4096];
        while let Ok(k) = stdout.read(&mut buf).await {
            if k == 0 {
                break;
            }
            out.push(&String::from_utf8_lossy(&buf[..k]));
        }
        child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1)
    };
    match timeout {
        Some(t) => tokio::time::timeout(t, work).await.unwrap_or_else(|_| {
            out.push(&format!("\ntimed out after {}s\n", t.as_secs()));
            -1
        }),
        None => work.await,
    }
}

async fn effective(cmd: &str, ctx: &ToolCtx) -> String {
    let use_rtk = match ctx.settings.tools.rtk.as_str() {
        "off" => false,
        _ => rtk_available(),
    };
    if use_rtk { rtk_rewrite(cmd).await.unwrap_or(cmd.to_string()) } else { cmd.to_string() }
}

pub async fn bash(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let cmd = s(args, "command").context("command required")?.to_string();
    let effective = effective(&cmd, ctx).await;
    if super::b(args, "background") {
        let (run, cwd, timeout) = (effective.clone(), ctx.cwd.clone(), n(args, "timeout").map(Duration::from_secs));
        let id = ctx.jobs.spawn(crate::jobs::Kind::Shell, cmd.clone(), false, move |out| Box::pin(async move { run_into(&run, &cwd, &out, timeout).await }));
        return Ok(ToolOut::ok(format!("started background job {id}: {cmd}\nIts end is reported in a later message; use job_output to look at it, job_kill to stop it.")));
    }
    let timeout = n(args, "timeout").unwrap_or(ctx.settings.tools.bash_timeout).max(1);
    let mut child = spawn_sh(&effective, &ctx.cwd)?;
    let stdout = child.stdout.take().unwrap();
    let read = async {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut r = stdout;
        let _ = r.read_to_end(&mut buf).await;
        buf
    };
    let (buf, status) = match tokio::time::timeout(Duration::from_secs(timeout), async {
        let buf = read.await;
        (buf, child.wait().await)
    })
    .await
    {
        Ok((buf, status)) => (buf, status.ok()),
        Err(_) => {
            let _ = child.kill().await;
            return Ok(ToolOut::err(format!("timed out after {timeout}s: {cmd}")));
        }
    };
    let raw = String::from_utf8_lossy(&buf);
    let text = compact(&raw);
    let (mut text, cut) = truncate_middle(&text, ctx.settings.tools.max_lines);
    if cut {
        let dir = crate::config::home().join("cache/logs");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(format!("{}.log", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()));
        if std::fs::write(&file, raw.as_bytes()).is_ok() {
            text.push_str(&format!("\n[full output: {}]", file.display()));
        }
    }
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    if text.is_empty() {
        text = "(no output)".into();
    }
    if code != 0 {
        text.push_str(&format!("\n[exit {code}]"));
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
}
