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
}
