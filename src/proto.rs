//! Wire protocol between the TUI client and the background daemon: one JSON value per line over a unix socket.

use crate::agent::Event;
use crate::jobs::JobInfo;
use crate::session::Entry;
use crate::tools::Todo;
use crate::types::Block;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    crate::config::home().join("theta.sock")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Target {
    New,
    /// Most recent session in `cwd`.
    Continue,
    /// A session id or file path.
    Resume(String),
}

/// Client → daemon. Answers come back as `Push` values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Req {
    /// Subscribe this connection to a session (replaces the previous one) and get a `Push::Snapshot`.
    Attach { target: Target, cwd: PathBuf, agent: Option<String>, model: Option<String> },
    /// Text and attached images (`Block::Image`) of a new user message.
    Send(String, Vec<Block>),
    /// Message for a run in progress: it joins the conversation at the agent's next step. Idle: same as `Send`.
    Steer(String),
    /// Open tabs of `cwd`, in tab order, so the next start can reopen them.
    SaveTabs { cwd: PathBuf, open: Vec<String> },
    /// The user looked at this session: drop its unread result.
    Seen,
    Interrupt,
    Answer { id: String, text: String },
    SetModel(String),
    SetEffort(String),
    SetAgent(String),
    Compact,
    Btw(String),
    Title(String),
    /// Move the active branch (`None` = before the first message).
    Leaf(Option<String>),
    /// Settings or credentials changed on disk: re-read them.
    Reload,
    McpConnect(String),
    McpStatus,
    /// Is an agent run active in any session? Answered with `Push::Busy`.
    Busy,
    /// Stop a background job of the attached session.
    JobKill(String),
    /// Ask for a job's output; answered with `Push::JobOutput`.
    JobOutput(String),
    Shutdown,
    /// Link this daemon to theta-server so a browser can drive it (answered with `Push::Remote`). Local clients only.
    /// `cwd` is the project the browser opens first: the one the user is working in.
    RemoteStart { cwd: PathBuf },
    /// Drop the link to theta-server. Local clients only.
    RemoteStop,
    /// Ask for the current link state. Local clients only.
    RemoteStatus,
    /// What a client needs to start: projects, the sessions of `cwd` (default: the daemon's), agents, models.
    /// Answered with `Push::Overview`.
    Overview { cwd: Option<PathBuf> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionBrief {
    pub id: String,
    pub title: String,
    pub updated: u64,
    pub messages: usize,
    /// An agent run is active in it right now.
    pub running: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentBrief {
    pub name: String,
    pub description: String,
    pub scope: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelBrief {
    /// `provider/model`, the value `Req::SetModel` takes.
    pub key: String,
    pub provider: String,
    pub context: u64,
    pub reasoning: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overview {
    pub cwd: PathBuf,
    pub version: String,
    /// Session ids of the tabs open in `cwd` (shared with the TUI), in tab order.
    pub open: Vec<String>,
    pub projects: Vec<PathBuf>,
    pub sessions: Vec<SessionBrief>,
    pub agents: Vec<AgentBrief>,
    /// Models of the providers that are logged in.
    pub models: Vec<ModelBrief>,
    pub efforts: Vec<String>,
}

/// State of the outbound link to theta-server.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum RemoteState {
    #[default]
    Off,
    Connecting,
    /// Waiting for the user to type `code` on the server's link page.
    Code { code: String, expires_in: u64 },
    Linked,
    /// Connection lost, retrying.
    Offline(String),
    /// The server refused the link.
    Error(String),
}

/// Everything a client needs to show a session it just attached to.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub session_id: String,
    pub entries: Vec<Entry>,
    pub agent: String,
    pub model: String,
    pub effort: String,
    pub running: bool,
    /// Events of the step in flight (streamed text, tool progress, open questions).
    pub replay: Vec<Event>,
    pub todos: Vec<Todo>,
    #[serde(default)]
    pub jobs: Vec<JobInfo>,
}

/// Daemon → client.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Push {
    Snapshot(Box<Snapshot>),
    /// A new session entry; clients keep a read-only mirror of the session.
    Entry(Entry),
    Event(Event),
    Meta { agent: String, model: String, effort: String },
    Notice(String),
    Err(String),
    Busy(bool),
    /// Background jobs of the session changed (started, ended or killed).
    Jobs(Vec<JobInfo>),
    JobOutput { id: String, text: String },
    Remote { state: RemoteState, url: String },
    Overview(Box<Overview>),
    /// Tabs of `cwd` were opened or closed by another client: mirror it. Sent to every client.
    Tabs { cwd: PathBuf, opened: Vec<String>, closed: Vec<String> },
}
