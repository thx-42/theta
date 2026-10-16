//! `theta daemon sample`: snapshot of the running daemon (state, threads, open files) for a bug report, copied to the clipboard.

use anyhow::{Result, bail};
use std::process::Command;

/// Stdout+stderr of a command, or None when it is not installed or fails to start.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    Some(text.trim().to_string())
}

fn head(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

fn section(out: &mut String, title: &str, body: Option<String>) {
    out.push_str(&format!("\n## {title}\n"));
    out.push_str(body.filter(|b| !b.is_empty()).as_deref().unwrap_or("(unavailable)"));
    out.push('\n');
}

pub fn run_sample() -> Result<()> {
    // `$` keeps this very process (`theta daemon sample`) out of the match.
    let Some(pid) = run("pgrep", &["-f", "theta daemon$"]).and_then(|s| s.lines().next().map(str::to_string)).filter(|p| !p.is_empty()) else {
        bail!("no daemon running");
    };
    let mut out = format!("theta {} on {} {}\ndaemon pid {pid}\n", crate::update::VERSION, std::env::consts::OS, std::env::consts::ARCH);
    section(&mut out, "process", run("ps", &["-o", "pid,stat,%cpu,rss,etime,command", "-p", &pid]));
    if cfg!(target_os = "macos") {
        section(&mut out, "stacks (sample)", run("sample", &[&pid, "2", "-mayDie"]).map(|s| head(&s, 150)));
    } else {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok().map(|s| s.lines().filter(|l| l.starts_with("State") || l.starts_with("Threads")).collect::<Vec<_>>().join("\n"));
        section(&mut out, "status", status);
        section(&mut out, "wchan", std::fs::read_to_string(format!("/proc/{pid}/wchan")).ok());
        let stacks = run("eu-stack", &["-p", &pid]).filter(|s| !s.contains("not found")).or_else(|| run("gdb", &["-p", &pid, "-batch", "-ex", "thread apply all bt 10"]));
        section(&mut out, "stacks (eu-stack/gdb)", stacks.map(|s| head(&s, 150)));
    }
    section(&mut out, "open files", run("lsof", &["-nP", "-p", &pid]).map(|s| head(&s, 40)));
    println!("{out}");
    println!("{}", if crate::tui::clipboard(&out) { "copied to the clipboard" } else { "no clipboard tool found: copy the text above" });
    Ok(())
}
