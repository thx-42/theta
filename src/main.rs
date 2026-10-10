//! θ theta — a lightweight coding agent harness.

mod agent;
mod agents;
mod skills;
mod mcp;
mod auth;
mod catalog;
mod client;
mod compact;
mod config;
mod hooks;
mod jobs;
mod lint;
mod llm;
mod proto;
mod relay;
mod server;
mod session;
mod tools;
mod tui;
mod types;
mod update;
mod usage;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "theta", version = update::VERSION, about = "θ — lightweight coding agent")]
struct Cli {
    /// Prompt; with -p runs non-interactively and prints the answer.
    prompt: Vec<String>,
    /// Print mode: run once, print the final answer, exit.
    #[arg(short, long)]
    print: bool,
    /// Continue the most recent session in this directory.
    #[arg(short, long)]
    r#continue: bool,
    /// Resume a session file or id.
    #[arg(short, long)]
    resume: Option<String>,
    /// Model as provider/model.
    #[arg(short, long)]
    model: Option<String>,
    /// Agent name.
    #[arg(short, long)]
    agent: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Log in to a provider (OAuth subscription or API key).
    Login { provider: Option<String> },
    /// Remove stored credentials.
    Logout { provider: String },
    /// List models (optionally filtered).
    Models { filter: Option<String> },
    /// List agents (global and local).
    Agents,
    /// MCP servers: list, log in (OAuth) or log out.
    Mcp {
        /// list | login | logout
        action: String,
        server: Option<String>,
    },
    /// Refresh the model catalog from models.dev.
    Refresh,
    /// The background server (started automatically on first use). `theta daemon stop` stops it.
    Daemon { action: Option<String> },
    /// Check for a newer release and install it.
    Update {
        /// Only report whether a newer version exists.
        #[arg(long)]
        check: bool,
        /// Install even when already up to date or on a local build.
        #[arg(long)]
        force: bool,
    },
}

pub struct App {
    pub rt: agent::Runtime,
    pub session: Arc<Mutex<session::Session>>,
    pub turn: agent::Turn,
}

pub async fn build_runtime(cwd: std::path::PathBuf) -> Result<agent::Runtime> {
    config::bootstrap()?;
    let project = config::project_root(&cwd);
    let settings = config::load(&project)?;
    let catalog = catalog::Catalog::load(&settings).await;
    let skills = skills::discover(&project, &settings.skills.auto);
    let mcp = mcp::Registry::new(&settings.mcp);
    Ok(agent::Runtime {
        catalog: Arc::new(catalog),
        auth: auth::Auth::load(),
        settings: Arc::new(settings),
        http: llm::http(),
        agents: Arc::new(agents::discover(&project)),
        skills: Arc::new(skills),
        mcp: Arc::new(mcp),
        project,
        cwd,
        todos: Default::default(),
        read_cache: Default::default(),
        asks: Default::default(),
        jobs: Default::default(),
    })
}

/// Agent + model + effort for a run: CLI flag > `[agents.<name>]` > agent frontmatter > settings.
pub fn make_turn(rt: &agent::Runtime, agent_name: Option<&str>, model: Option<&str>) -> Result<agent::Turn> {
    let name = agent_name.unwrap_or(&rt.settings.agent);
    let agent = agents::find(&rt.agents, name).or_else(|| agents::find(&rt.agents, "build")).context("no agents found")?.clone();
    let model = model.map(String::from).or_else(|| agents::model_of(&agent, &rt.settings)).unwrap_or_else(|| rt.settings.model.clone());
    let effort = agents::effort_of(&agent, &rt.settings).unwrap_or_else(|| rt.settings.effort.clone());
    Ok(agent::Turn { agent, model, effort, depth: 0 })
}

fn open_session(rt: &agent::Runtime, cont: bool, resume: Option<&str>) -> Result<session::Session> {
    if let Some(r) = resume {
        let p = std::path::PathBuf::from(r);
        let path = if p.exists() { p } else { session::dir_for(&rt.cwd).join(format!("{r}.jsonl")) };
        return session::Session::open(&path).with_context(|| format!("cannot open session {}", path.display()));
    }
    if cont
        && let Some(i) = session::list(&rt.cwd).first() {
            return session::Session::open(&i.path);
        }
    Ok(session::Session::new(&rt.cwd))
}

async fn print_mode(app: App, prompt: String) -> Result<()> {
    let compact = app.rt.settings.verbose == "compact";
    app.rt.mcp.connect_all().await;
    app.session.lock().unwrap().add_msg(types::Msg::user(skills::expand(&app.rt.skills, &prompt)), None)?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(agent::run(app.rt.clone(), app.session.clone(), app.turn.clone(), tx, Default::default()));
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    while let Some(ev) = rx.recv().await {
        match ev {
            agent::Event::Text(t) if !compact => {
                write!(out, "{t}")?;
                out.flush()?;
            }
            agent::Event::ToolStart { name, args, .. } if !compact => {
                writeln!(err, "\n▸ {name} {}", tui::tool_summary(&name, &args))?;
            }
            agent::Event::Ask { id, .. } => {
                app.rt.asks.lock().unwrap().remove(&id); // nobody to answer: the tool reports it and the agent decides
            }
            agent::Event::Error(e) => writeln!(err, "error: {e}")?,
            _ => {}
        }
    }
    handle.await??;
    if compact {
        let s = app.session.lock().unwrap();
        writeln!(out, "{}", s.context().last().map(|m| m.text()).unwrap_or_default())?;
    } else {
        writeln!(out)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Cmd::Daemon { action }) = &cli.cmd {
        return match action.as_deref() {
            None => server::serve().await,
            Some("stop") => {
                if let Ok(sock) = tokio::net::UnixStream::connect(proto::socket_path()).await {
                    let mut line = serde_json::to_string(&proto::Req::Shutdown)?;
                    line.push('\n');
                    tokio::io::AsyncWriteExt::write_all(&mut { sock }, line.as_bytes()).await?;
                }
                Ok(())
            }
            Some(a) => bail!("unknown action `{a}` (stop)"),
        };
    }
    if let Some(Cmd::Update { check, force }) = &cli.cmd {
        return update::run(*check, *force).await;
    }
    let cwd = std::env::current_dir()?;
    let rt = build_runtime(cwd.clone()).await?;
    match cli.cmd {
        Some(Cmd::Login { provider }) => {
            let id = match provider {
                Some(p) => p,
                None => {
                    let list: Vec<_> = rt.catalog.providers.iter().filter(|p| auth::shows_in_login(p)).collect();
                    for (i, p) in list.iter().enumerate() {
                        println!("{:>2}. {:<16} {}", i + 1, p.id, p.name);
                    }
                    print!("provider > ");
                    std::io::stdout().flush()?;
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s)?;
                    let s = s.trim();
                    s.parse::<usize>().ok().and_then(|i| list.get(i.wrapping_sub(1))).map(|p| p.id.clone()).unwrap_or(s.to_string())
                }
            };
            let p = rt.catalog.provider(&id).with_context(|| format!("unknown provider `{id}`"))?;
            return auth::login_cli(&rt.auth, p).await;
        }
        Some(Cmd::Logout { provider }) => return rt.auth.remove(&provider).await,
        Some(Cmd::Models { filter }) => {
            for p in &rt.catalog.providers {
                let ok = rt.auth.status(p).await.is_some();
                for m in rt.catalog.models_of(p) {
                    let key = m.key();
                    if filter.as_ref().is_some_and(|f| !key.contains(f.as_str())) {
                        continue;
                    }
                    if writeln!(std::io::stdout(), "{} {key:<50} {:>6}k ctx  ${}/{}", if ok { "●" } else { "○" }, m.context / 1000, m.cost_in, m.cost_out).is_err() {
                        return Ok(()); // stdout closed (e.g. piped to head)
                    }
                }
            }
            return Ok(());
        }
        Some(Cmd::Agents) => {
            for a in rt.agents.iter() {
                println!("{:<16} ({})  {}", a.name, a.scope.label(), a.description);
            }
            return Ok(());
        }
        Some(Cmd::Mcp { action, server }) => {
            let named = || -> Result<(String, config::McpServer)> {
                let n = server.clone().context("server name required")?;
                let c = rt.mcp.config(&n).with_context(|| format!("unknown MCP server `{n}`"))?.clone();
                Ok((n, c))
            };
            match action.as_str() {
                "list" => {
                    rt.mcp.connect_all().await;
                    rt.mcp.status().iter().for_each(|l| println!("{l}"));
                }
                "login" => {
                    let (n, c) = named()?;
                    let (say, mut said) = tokio::sync::mpsc::unbounded_channel();
                    let (paste_tx, paste) = tokio::sync::mpsc::unbounded_channel();
                    std::thread::spawn(move || {
                        let mut s = String::new();
                        while std::io::stdin().read_line(&mut s).is_ok_and(|k| k > 0) {
                            if paste_tx.send(std::mem::take(&mut s)).is_err() {
                                break;
                            }
                        }
                    });
                    tokio::spawn(async move {
                        while let Some(m) = said.recv().await {
                            println!("{m}\n");
                        }
                    });
                    mcp::login(&n, &c, &mut auth::Io { say, paste }).await?;
                    println!("✓ logged in to {n}");
                }
                "logout" => mcp::logout(&named()?.0)?,
                _ => bail!("unknown action `{action}` (list | login | logout)"),
            }
            return Ok(());
        }
        Some(Cmd::Refresh) => return catalog::refresh().await,
        Some(Cmd::Daemon { .. } | Cmd::Update { .. }) => unreachable!(),
        None => {}
    }
    let prompt = cli.prompt.join(" ");
    if cli.print {
        let turn = make_turn(&rt, cli.agent.as_deref(), cli.model.as_deref())?;
        let session = open_session(&rt, cli.r#continue, cli.resume.as_deref())?;
        let app = App { rt, session: Arc::new(Mutex::new(session)), turn };
        let prompt = if prompt.is_empty() {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        } else {
            prompt
        };
        return print_mode(app, prompt).await;
    }
    let target = match (&cli.resume, cli.r#continue) {
        (Some(r), _) => {
            // A path is resolved here: the daemon's working directory differs from ours.
            let p = std::path::Path::new(r);
            proto::Target::Resume(if p.exists() { p.canonicalize()?.display().to_string() } else { r.clone() })
        }
        (None, true) => proto::Target::Continue,
        (None, false) => match session::saved_open(&cwd).into_iter().next() {
            Some(first) => proto::Target::Resume(first),
            None => proto::Target::New,
        },
    };
    let (remote, pushes, snap) = client::open(target, cwd, cli.agent, cli.model).await?;
    tui::run(rt, remote, pushes, snap, (!prompt.is_empty()).then_some(prompt)).await
}
