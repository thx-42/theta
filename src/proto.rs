//! Wire protocol between the TUI client and the background daemon: one JSON value per line over a unix socket.

use crate::agent::Event;
use crate::session::Entry;
use crate::tools::Todo;
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
    Send(String),
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
}
