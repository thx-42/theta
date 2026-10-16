//! `theta daemon sample`: snapshot of the running daemon (state, threads, open files) for a bug report, copied to the clipboard.

use anyhow::{Result, bail};
use std::process::Command;

/// Stdout+stderr of a command, or None when it is not installed or fails to start.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    Some(text.trim().to_string())
}

/// Stdout only: lsof warns about every foreign filesystem on stderr.
fn run_quiet(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).stderr(std::process::Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn head(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

fn section(out: &mut String, title: &str, body: Option<String>) {
    out.push_str(&format!("\n## {title}\n"));
    out.push_str(body.filter(|b| !b.is_empty()).as_deref().unwrap_or("(unavailable)"));
    out.push('\n');
}

/// Every running theta (the daemon and each TUI): a frozen screen may be the client, not the daemon.
fn theta_pids() -> Vec<String> {
    let me = std::process::id().to_string();
    let mut pids: Vec<(bool, String)> = run_quiet("ps", &["-axo", "pid=,args="])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (pid, args) = l.trim().split_once(' ')?;
            let args = args.trim();
            let exe = args.split(' ').next()?;
            (pid != me && (exe == "theta" || exe.ends_with("/theta"))).then(|| (!args.ends_with(" daemon"), pid.to_string()))
        })
        .collect();
    pids.sort(); // daemon first
    pids.into_iter().map(|(_, p)| p).collect()
}

fn one(out: &mut String, pid: &str) {
    out.push_str(&format!("\n===== pid {pid} =====\n"));
    section(out, "process", run("ps", &["-o", "pid,stat,%cpu,rss,etime,command", "-p", pid]));
    if cfg!(target_os = "macos") {
        section(out, "stacks (sample)", run("sample", &[pid, "2", "-mayDie"]).map(|s| head(&s, 150)));
    } else {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok().map(|s| s.lines().filter(|l| l.starts_with("State") || l.starts_with("Threads")).collect::<Vec<_>>().join("\n"));
        section(out, "status", status);
        // what each thread is blocked in, without needing gdb
        let threads: String = std::fs::read_dir(format!("/proc/{pid}/task"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|t| {
                let p = t.path();
                let name = std::fs::read_to_string(p.join("comm")).unwrap_or_default();
                let wchan = std::fs::read_to_string(p.join("wchan")).unwrap_or_default();
                let state = std::fs::read_to_string(p.join("stat")).ok().and_then(|s| s.rsplit_once(") ").map(|(_, r)| r.chars().next().unwrap_or('?').to_string())).unwrap_or_default();
                format!("{} {} {} {}", t.file_name().to_string_lossy(), state, name.trim(), wchan.trim())
            })
            .collect::<Vec<_>>()
            .join("\n");
        section(out, "threads (tid state name wchan)", Some(threads));
        let stacks = run("eu-stack", &["-p", pid]).filter(|s| !s.contains("not found")).or_else(|| run("gdb", &["-p", pid, "-batch", "-ex", "thread apply all bt 10"]));
        section(out, "stacks (eu-stack/gdb)", stacks.map(|s| head(&s, 150)));
    }
    section(out, "open files", run_quiet("lsof", &["-nP", "-p", pid]).map(|s| head(&s, 25)));
}

pub fn run_sample() -> Result<()> {
    let pids = theta_pids();
    if pids.is_empty() {
        bail!("no theta process running");
    }
    let mut out = format!("theta {} on {} {}\n", crate::update::VERSION, std::env::consts::OS, std::env::consts::ARCH);
    for pid in &pids {
        one(&mut out, pid);
    }
    println!("{out}");
    if crate::tui::clipboard(&out) {
        println!("copied to the clipboard");
    } else {
        // no clipboard tool (server over SSH): the terminal itself can set the local clipboard
        crate::tui::copy_osc52(&out);
        println!("\nsent to the terminal clipboard (OSC 52); if nothing pastes, copy the text above");
    }
    Ok(())
}
