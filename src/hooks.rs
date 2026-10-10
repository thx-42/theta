//! Shell hooks around each user message (`[hooks]` in settings.toml).

use crate::config::Hooks;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(30);

struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn run_one(cmd: &str, stdin: &str, env: &[(&str, &str)], cwd: &Path) -> Result<Ran, String> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("hook `{cmd}`: {e}"))?;
    let mut input = child.stdin.take().unwrap();
    let text = stdin.to_string();
    // A hook that ignores stdin closes the pipe early; that is not an error.
    tokio::spawn(async move {
        let _ = input.write_all(text.as_bytes()).await;
    });
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("hook `{cmd}` timed out after {}s", TIMEOUT.as_secs()))?
        .map_err(|e| format!("hook `{cmd}`: {e}"))?;
    Ok(Ran { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(), stderr: String::from_utf8_lossy(&out.stderr).trim().to_string() })
}

/// Run the pre-message hooks in order. `Ok(context)`: extra text for the agent (hook stdout). `Err`: the message is rejected.
pub async fn pre(hooks: &Hooks, text: &str, session: &str, cwd: &Path) -> Result<String, String> {
    let env = [("THETA_SESSION", session), ("THETA_CWD", &cwd.display().to_string()), ("THETA_HOOK", "pre_message")];
    let mut extra = Vec::new();
    for cmd in &hooks.pre_message {
        let r = run_one(cmd, text, &env, cwd).await?;
        if r.code != 0 {
            let why = if r.stderr.is_empty() { r.stdout } else { r.stderr };
            return Err(format!("rejected by pre_message hook `{cmd}` (exit {}){}", r.code, if why.is_empty() { String::new() } else { format!(": {why}") }));
        }
        if !r.stdout.is_empty() {
            extra.push(r.stdout);
        }
    }
    Ok(extra.join("\n\n"))
}

/// Run the post-message hooks; returns the failures, one line each.
pub async fn post(hooks: &Hooks, reply: &str, status: &str, session: &str, cwd: &Path) -> Vec<String> {
    let env = [("THETA_SESSION", session), ("THETA_CWD", &cwd.display().to_string()), ("THETA_HOOK", "post_message"), ("THETA_STATUS", status)];
    let mut failed = Vec::new();
    for cmd in &hooks.post_message {
        match run_one(cmd, reply, &env, cwd).await {
            Ok(r) if r.code != 0 => failed.push(format!("post_message hook `{cmd}` exited {}", r.code)),
            Ok(_) => {}
            Err(e) => failed.push(e),
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks(pre: &[&str]) -> Hooks {
        Hooks { pre_message: pre.iter().map(|s| s.to_string()).collect(), post_message: vec![] }
    }

    #[tokio::test]
    async fn pre_adds_context_and_can_reject() {
        let cwd = std::env::temp_dir();
        assert_eq!(pre(&hooks(&["cat", "echo extra"]), "hello", "s1", &cwd).await.unwrap(), "hello\n\nextra");
        let e = pre(&hooks(&["echo no >&2; exit 3"]), "x", "s1", &cwd).await.unwrap_err();
        assert!(e.contains("exit 3") && e.ends_with(": no"));
        assert_eq!(pre(&hooks(&["echo $THETA_SESSION"]), "x", "abc", &cwd).await.unwrap(), "abc");
    }
}
