//! `/ssh`: the file and shell tools run on a remote host. One `ssh` process starts `theta ssh-agent` there;
//! tool calls travel over its stdin/stdout as one JSON value per line, answered out of order by id.

use crate::config::Settings;
use crate::tools::{self, ToolCtx, ToolOut};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

/// Tools that run on the host. The others (todo, web, ask, jobs, plan...) stay local.
const REMOTE_TOOLS: &[&str] = &["read", "write", "edit", "ls", "find", "grep", "bash"];

/// `sh -c '...'` so that the remote login shell (bash, zsh, fish, csh) only has to parse a quoted string.
const AGENT_CMD: &str = r#"sh -c 'PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH" exec theta ssh-agent'"#;

const INSTALL_HINT: &str = "install theta on the server: curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh | bash";

#[derive(Serialize, Deserialize)]
struct Req {
    id: u64,
    /// hello | cd | tool
    op: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: Value,
    #[serde(default)]
    cwd: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Resp {
    id: u64,
    content: String,
    is_error: bool,
    #[serde(default)]
    display: Option<String>,
    /// hello and cd: the directory the host is now in.
    #[serde(default)]
    cwd: String,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Resp>>>>;

/// The host a session works on, when it has one. Shared by the runs of the session and its subagents.
#[derive(Clone, Default)]
pub struct Slot(Arc<Mutex<Option<Arc<Host>>>>);

impl Slot {
    pub fn get(&self) -> Option<Arc<Host>> {
        self.0.lock().unwrap().clone()
    }
    pub fn set(&self, h: Option<Arc<Host>>) {
        *self.0.lock().unwrap() = h;
    }
    /// Forget a connection that died. True when the slot is empty afterwards.
    fn drop_dead(&self) -> bool {
        let mut g = self.0.lock().unwrap();
        g.take_if(|h| !h.alive.load(Ordering::Relaxed));
        g.is_none()
    }
}

pub struct Host {
    /// The `ssh` arguments as typed, for display.
    pub name: String,
    /// Version and platform of the helper.
    pub info: String,
    cwd: Mutex<String>,
    prev: Mutex<String>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next: AtomicU64,
    alive: Arc<AtomicBool>,
    _child: Child,
}

/// Split `/ssh` arguments like a shell would: whitespace, with '...' and "..." grouping.
pub fn split(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None::<char>, false);
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Start `ssh <args>` (so ~/.ssh/config aliases, ports, jump hosts and keys work as in a shell) and the helper behind it.
/// `on_close` runs once if the connection ends later.
pub async fn connect(args: &[String], on_close: impl FnOnce() + Send + 'static) -> Result<Host> {
    if args.is_empty() {
        bail!("usage: /ssh <ssh arguments>, e.g. /ssh user@host or /ssh my-alias");
    }
    // No terminal here: passwords and host-key questions cannot be answered, so fail instead of hanging.
    let mut child = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=30"])
        .args(args)
        .arg(AGENT_CMD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start ssh: {e}"))?;
    let (stdin, stdout, stderr) = (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let err_text = Arc::new(Mutex::new(String::new()));
    let et = err_text.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let mut t = et.lock().unwrap();
            if t.len() < 4000 {
                t.push_str(&l);
                t.push('\n');
            }
        }
    });
    let pending: Pending = Default::default();
    let alive = Arc::new(AtomicBool::new(true));
    // Only a connection that said hello reports its end; a failed attempt reports through `connect`'s error.
    let armed = Arc::new(AtomicBool::new(false));
    {
        let (pending, alive, armed) = (pending.clone(), alive.clone(), armed.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if let Ok(r) = serde_json::from_str::<Resp>(&l)
                    && let Some(tx) = pending.lock().unwrap().remove(&r.id)
                {
                    let _ = tx.send(r);
                }
            }
            alive.store(false, Ordering::Relaxed);
            pending.lock().unwrap().clear(); // wakes every waiting call with an error
            if armed.load(Ordering::Relaxed) {
                on_close();
            }
        });
    }
    let mut host = Host {
        name: args.join(" "),
        info: String::new(),
        cwd: Mutex::new(String::new()),
        prev: Mutex::new(String::new()),
        stdin: tokio::sync::Mutex::new(stdin),
        pending,
        next: AtomicU64::new(1),
        alive,
        _child: child,
    };
    let hello = tokio::time::timeout(Duration::from_secs(30), host.request("hello", "", json!({}), "")).await;
    match hello {
        Ok(Ok(r)) => {
            armed.store(true, Ordering::Relaxed);
            host.info = r.content;
            *host.cwd.lock().unwrap() = r.cwd;
            Ok(host)
        }
        _ => {
            tokio::time::sleep(Duration::from_millis(200)).await; // let stderr drain
            let why = err_text.lock().unwrap().trim().to_string();
            let missing = why.contains("not found") || why.contains("No such file");
            bail!("{}{}", if why.is_empty() { "ssh connection failed or timed out".into() } else { why }, if missing { format!("\n{INSTALL_HINT}") } else { String::new() })
        }
    }
}

impl Host {
    pub fn cwd(&self) -> String {
        self.cwd.lock().unwrap().clone()
    }

    async fn request(&self, op: &str, name: &str, args: Value, cwd: &str) -> Result<Resp> {
        if !self.alive.load(Ordering::Relaxed) {
            bail!("ssh connection to {} lost; reconnect with /ssh {}", self.name, self.name);
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_string(&Req { id, op: op.into(), name: name.into(), args, cwd: cwd.into() })?;
        line.push('\n');
        let sent = {
            let mut w = self.stdin.lock().await;
            w.write_all(line.as_bytes()).await.and(w.flush().await)
        };
        if sent.is_err() {
            self.pending.lock().unwrap().remove(&id);
            bail!("ssh connection to {} lost", self.name);
        }
        rx.await.map_err(|_| anyhow::anyhow!("ssh connection to {} lost", self.name))
    }

    /// Run a tool on the host, in its current directory.
    pub async fn call(&self, name: &str, args: &Value) -> ToolOut {
        if name == "bash" && args.get("background").and_then(|v| v.as_bool()).unwrap_or(false) {
            // shortcut: no background jobs over ssh; their end could not be reported, upgrade by streaming job events.
            return ToolOut::err("background jobs are not available over ssh; run it in the foreground (use nohup and a log file for long jobs)");
        }
        match self.request("tool", name, args.clone(), &self.cwd()).await {
            Ok(r) => ToolOut { content: r.content, is_error: r.is_error, display: r.display },
            Err(e) => ToolOut::err(format!("{e:#}")),
        }
    }

    /// Change the host's working directory like `cd`: `~`, `..`, `-`, relative and absolute paths. Returns the new one.
    pub async fn cd(&self, path: &str) -> Result<String> {
        let path = if path == "-" { self.prev.lock().unwrap().clone() } else { path.to_string() };
        let r = self.request("cd", "", json!({ "path": path }), &self.cwd()).await?;
        if r.is_error {
            bail!("{}", r.content);
        }
        let old = std::mem::replace(&mut *self.cwd.lock().unwrap(), r.cwd.clone());
        *self.prev.lock().unwrap() = old;
        Ok(r.cwd)
    }
}

/// Called when a connection ends on its own: forget it, and tell the caller whether the session is back to local.
pub fn closed(slot: &Slot) -> bool {
    slot.drop_dead()
}

/// Environment note for the system prompt.
pub fn prompt_note(h: &Host) -> String {
    format!(
        "# Remote host\nYour read, write, edit, ls, find, grep and bash tools run over SSH on `{}` ({}), not on the machine theta started on. Relative paths and the shell start in `{}`. Project instructions above come from the local machine; todo, web and plan tools stay local.",
        h.name,
        h.info.replace('\n', " "),
        h.cwd()
    )
}

/// Does `name` run on the host?
pub fn is_remote_tool(name: &str) -> bool {
    REMOTE_TOOLS.contains(&name)
}

/// `theta ssh-agent`: serve tool calls from stdin until it closes. Runs on the host.
pub async fn agent() -> Result<()> {
    let ctx = ToolCtx {
        cwd: PathBuf::new(),
        settings: Arc::new(Settings::default()),
        http: reqwest::Client::new(),
        todos: Default::default(),
        read_cache: Default::default(),
        session_id: "ssh".into(),
        jobs: Default::default(),
        ssh: Default::default(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(l) = rx.recv().await {
            if out.write_all(l.as_bytes()).await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut work = tokio::task::JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        let Ok(req) = serde_json::from_str::<Req>(&line) else { continue };
        let (ctx, tx) = (ctx.clone(), tx.clone());
        work.spawn(async move {
            let mut l = serde_json::to_string(&serve(req, ctx).await).unwrap_or_default();
            l.push('\n');
            let _ = tx.send(l);
        });
    }
    // Input closed: finish what is running, then flush the answers.
    while work.join_next().await.is_some() {}
    drop(tx);
    let _ = writer.await;
    Ok(())
}

async fn serve(req: Req, mut ctx: ToolCtx) -> Resp {
    let id = req.id;
    let fail = |m: String| Resp { id, content: m, is_error: true, ..Default::default() };
    ctx.cwd = if req.cwd.is_empty() { std::env::current_dir().unwrap_or_default() } else { PathBuf::from(&req.cwd) };
    match req.op.as_str() {
        "hello" => Resp {
            id,
            content: format!("theta {} on {} {}", crate::update::VERSION, std::env::consts::OS, std::env::consts::ARCH),
            cwd: ctx.cwd.display().to_string(),
            ..Default::default()
        },
        "cd" => {
            let p = req.args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let target = match p {
                "" | "~" => dirs::home_dir().unwrap_or_default(),
                _ => tools::resolve(&ctx.cwd, p),
            };
            match std::fs::canonicalize(&target) {
                Ok(d) if d.is_dir() => Resp { id, cwd: d.display().to_string(), ..Default::default() },
                Ok(d) => fail(format!("not a directory: {}", d.display())),
                Err(e) => fail(format!("cannot cd to {}: {e}", target.display())),
            }
        }
        "tool" if is_remote_tool(&req.name) => {
            let o = tools::run(&req.name, &req.args, &ctx).await;
            Resp { id, content: o.content, is_error: o.is_error, display: o.display, ..Default::default() }
        }
        _ => fail(format!("unsupported request `{}` `{}`", req.op, req.name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_a_shell() {
        assert_eq!(split("-p 2222 me@host"), ["-p", "2222", "me@host"]);
        assert_eq!(split(r#"-o "ProxyCommand ssh jump -W %h:%p" box"#), ["-o", "ProxyCommand ssh jump -W %h:%p", "box"]);
        assert!(split("  ").is_empty());
    }

    /// Whole path with a fake `ssh` that runs the helper locally. Needs `cargo build` first; skipped otherwise.
    #[tokio::test]
    async fn client_talks_to_helper() {
        let bin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/theta");
        if !bin.exists() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("theta-fake-ssh-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("ssh");
        // The real command line is `ssh <options> <host> <command>`: run the helper whatever the arguments.
        std::fs::write(&fake, format!("#!/bin/sh\nexec {} ssh-agent\n", bin.display())).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        unsafe { std::env::set_var("PATH", path) };
        let h = connect(&["me@box".into()], || {}).await.unwrap();
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert_eq!(h.cd(&tmp.display().to_string()).await.unwrap(), tmp.display().to_string());
        assert!(h.cd("/definitely/not/here").await.is_err());
        assert_eq!(h.cwd(), tmp.display().to_string());
        let out = h.call("bash", &json!({ "command": "pwd" })).await;
        assert_eq!(out.content, tmp.display().to_string());
        assert!(h.call("bash", &json!({ "command": "true", "background": true })).await.is_error);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn serves_cd_and_tools() {
        let ctx = ToolCtx {
            cwd: PathBuf::new(),
            settings: Arc::new(Settings::default()),
            http: reqwest::Client::new(),
            todos: Default::default(),
            read_cache: Default::default(),
            session_id: "t".into(),
            jobs: Default::default(),
            ssh: Default::default(),
        };
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let cd = |p: &str| Req { id: 1, op: "cd".into(), name: String::new(), args: json!({ "path": p }), cwd: tmp.display().to_string() };
        let r = serve(cd("."), ctx.clone()).await;
        assert!(!r.is_error && r.cwd == tmp.display().to_string());
        assert!(serve(cd("/definitely/not/here"), ctx.clone()).await.is_error);
        let ls = Req { id: 2, op: "tool".into(), name: "bash".into(), args: json!({ "command": "pwd" }), cwd: tmp.display().to_string() };
        let r = serve(ls, ctx.clone()).await;
        assert_eq!(r.content, tmp.display().to_string());
        let bad = Req { id: 3, op: "tool".into(), name: "web_fetch".into(), args: json!({}), cwd: String::new() };
        assert!(serve(bad, ctx).await.is_error);
    }
}
