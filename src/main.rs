//! θ theta — a lightweight coding agent harness.

mod agent;
mod agents;
mod auth;
mod catalog;
mod compact;
mod config;
mod llm;
mod session;
mod tools;
mod tui;
mod types;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "theta", version, about = "θ — lightweight coding agent")]
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
    /// Refresh the model catalog from models.dev.
    Refresh,
}

pub struct App {
    pub rt: agent::Runtime,
    pub session: Arc<Mutex<session::Session>>,
    pub turn: agent::Turn,
}

pub async fn build_runtime() -> Result<agent::Runtime> {
    config::bootstrap()?;
    let cwd = std::env::current_dir()?;
    let project = config::project_root(&cwd);
    let settings = config::load(&project)?;
    let catalog = catalog::Catalog::load(&settings).await;
    Ok(agent::Runtime {
        catalog: Arc::new(catalog),
        auth: auth::Auth::load(),
        settings: Arc::new(settings),
        http: llm::http(),
        agents: Arc::new(agents::discover(&project)),
        project,
        cwd,
        todos: Default::default(),
        read_cache: Default::default(),
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
    app.session.lock().unwrap().add_msg(types::Msg::user(prompt), None)?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(agent::run(app.rt.clone(), app.session.clone(), app.turn.clone(), tx));
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
    let rt = build_runtime().await?;
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
        Some(Cmd::Refresh) => return catalog::refresh().await,
        None => {}
    }
    let turn = make_turn(&rt, cli.agent.as_deref(), cli.model.as_deref())?;
    let session = open_session(&rt, cli.r#continue, cli.resume.as_deref())?;
    let app = App { rt, session: Arc::new(Mutex::new(session)), turn };
    let prompt = cli.prompt.join(" ");
    if cli.print {
        let prompt = if prompt.is_empty() {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        } else {
            prompt
        };
        return print_mode(app, prompt).await;
    }
    tui::run(app, (!prompt.is_empty()).then_some(prompt)).await
}
