//! Client side of the daemon socket: connects (starting the daemon if needed), reconnects after a drop,
//! and turns the line protocol into two channels.

use crate::proto::{Push, Req, Snapshot, Target, socket_path};
use anyhow::{Result, bail};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Clone)]
pub struct Remote {
    tx: UnboundedSender<Req>,
}

impl Remote {
    pub fn send(&self, r: Req) {
        let _ = self.tx.send(r);
    }
}

unsafe extern "C" {
    fn setsid() -> i32;
}

fn spawn_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let logs = crate::config::home().join("cache/logs");
    std::fs::create_dir_all(&logs)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(logs.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // New session: closing the terminal must not take the daemon down with it.
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// Connect to the daemon, starting it on first use.
pub async fn connect() -> Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path).await {
        return Ok(s);
    }
    spawn_daemon()?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(s) = UnixStream::connect(&path).await {
            return Ok(s);
        }
    }
    bail!("cannot reach the theta daemon (see {})", crate::config::home().join("cache/logs/daemon.log").display())
}

async fn write(wr: &mut tokio::net::unix::OwnedWriteHalf, r: &Req) -> bool {
    let Ok(mut line) = serde_json::to_string(r) else { return true };
    line.push('\n');
    wr.write_all(line.as_bytes()).await.is_ok()
}

/// Attach and wait for the snapshot. Returns the channels the UI runs on.
pub async fn open(target: Target, cwd: PathBuf, agent: Option<String>, model: Option<String>) -> Result<(Remote, UnboundedReceiver<Push>, Snapshot)> {
    let attach = Req::Attach { target, cwd: cwd.clone(), agent, model };
    let sock = connect().await?;
    let (rd, mut wr) = sock.into_split();
    if !write(&mut wr, &attach).await {
        bail!("daemon closed the connection");
    }
    let mut lines = BufReader::new(rd).lines();
    let snap = loop {
        let Some(line) = lines.next_line().await? else { bail!("daemon closed the connection") };
        match serde_json::from_str::<Push>(&line) {
            Ok(Push::Snapshot(s)) => break *s,
            Ok(Push::Err(e)) => bail!("{e}"),
            _ => {}
        }
    };
    let (req_tx, req_rx) = unbounded_channel();
    let (push_tx, push_rx) = unbounded_channel();
    tokio::spawn(pump(lines, wr, req_rx, push_tx, cwd, snap.session_id.clone()));
    Ok((Remote { tx: req_tx }, push_rx, snap))
}

/// Shuttle messages both ways; on a dropped connection, reconnect and re-attach to the same session.
async fn pump(
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut wr: tokio::net::unix::OwnedWriteHalf,
    mut reqs: UnboundedReceiver<Req>,
    pushes: UnboundedSender<Push>,
    cwd: PathBuf,
    mut session: String,
) {
    loop {
        let alive = loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        let Ok(p) = serde_json::from_str::<Push>(&l) else { continue };
                        if let Push::Snapshot(s) = &p {
                            session = s.session_id.clone();
                        }
                        if pushes.send(p).is_err() {
                            return;
                        }
                    }
                    _ => break false,
                },
                r = reqs.recv() => match r {
                    Some(r) => {
                        if !write(&mut wr, &r).await {
                            break false;
                        }
                    }
                    None => return,
                },
            }
        };
        debug_assert!(!alive);
        let _ = pushes.send(Push::Err("disconnected from the daemon — reconnecting…".into()));
        let mut back = None;
        for _ in 0..20 {
            if let Ok(s) = connect().await {
                back = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let Some(sock) = back else {
            let _ = pushes.send(Push::Err("daemon unreachable".into()));
            return;
        };
        let (rd, w) = sock.into_split();
        lines = BufReader::new(rd).lines();
        wr = w;
        // Anything typed while disconnected is dropped; the snapshot restores the real state.
        while reqs.try_recv().is_ok() {}
        if !write(&mut wr, &Req::Attach { target: Target::Resume(session.clone()), cwd: cwd.clone(), agent: None, model: None }).await {
            let _ = pushes.send(Push::Err("daemon unreachable".into()));
            return;
        }
    }
}
