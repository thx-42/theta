//! Session tabs. Every tab is its own daemon connection; only the active one is mirrored in `Ui`,
//! the others keep their state parked in a `View` and queue what the daemon pushes.

use super::{AskView, Counts, Editor, Item, SPIN, VERBS, theme};
use crate::agent::{Event, Turn};
use crate::client::Remote;
use crate::proto::Push;
use crate::session::Session;
use crate::tools::Todo;
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SIDEBAR_W: u16 = 24;

/// idle: nothing to report; working: agent running; done / error: finished (or waiting for an answer) while in the background.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TabStatus {
    Idle,
    Working,
    Done,
    Error,
}

pub struct Tab {
    pub id: u64,
    pub title: String,
    pub status: TabStatus,
    /// The run in progress hit an error.
    pub errored: bool,
    /// Pushes received while in the background, replayed when the tab is activated.
    pub backlog: Vec<Push>,
    /// Parked state; `None` for the active tab, whose state lives in `Ui`.
    pub view: Option<View>,
}

impl Tab {
    pub fn new(id: u64) -> Self {
        Tab { id, title: String::new(), status: TabStatus::Idle, errored: false, backlog: vec![], view: None }
    }

    /// Follow a push meant for a tab that is not on screen.
    pub fn watch(&mut self, p: &Push) {
        match p {
            Push::Snapshot(s) => {
                self.errored = false;
                self.status = if s.running { TabStatus::Working } else { TabStatus::Idle };
            }
            Push::Event(e) => match e {
                Event::Started => {
                    self.errored = false;
                    self.status = TabStatus::Working;
                }
                Event::Answered(_) => self.status = TabStatus::Working,
                // A question needs the user: flag it like a finished run.
                Event::Ask { .. } => self.status = TabStatus::Done,
                Event::Error(_) => self.errored = true,
                Event::Interrupted => self.status = TabStatus::Idle,
                Event::Done if self.status != TabStatus::Idle => self.status = if self.errored { TabStatus::Error } else { TabStatus::Done },
                Event::Title(t) => self.title = t.clone(),
                Event::User(t) if self.title.is_empty() => self.title = t.clone(),
                _ => {}
            },
            _ => {}
        }
    }
}

/// The per-session part of `Ui`, swapped in and out when the active tab changes.
pub struct View {
    pub items: Vec<Item>,
    pub cache: Vec<Option<(usize, bool, Vec<Line<'static>>)>>,
    pub dirty: Vec<bool>,
    pub input: Editor,
    pub scroll_top: Option<usize>,
    pub last_total: usize,
    pub last_view: usize,
    pub remote: Remote,
    pub running: bool,
    pub started: Instant,
    pub elapsed: Duration,
    pub counts: Counts,
    pub status: String,
    pub todos: Vec<Todo>,
    pub cost: f64,
    pub ctx_tokens: u64,
    pub verb: &'static str,
    pub out_tokens: u64,
    pub stream_chars: usize,
    pub asks: VecDeque<AskView>,
    pub session: Arc<Mutex<Session>>,
    pub turn: Turn,
}

impl View {
    /// Empty view for a session the daemon is about to describe with a snapshot.
    pub fn fresh(remote: Remote, turn: Turn) -> Self {
        View {
            items: vec![],
            cache: vec![],
            dirty: vec![],
            input: Editor::default(),
            scroll_top: None,
            last_total: 0,
            last_view: 0,
            remote,
            running: false,
            started: Instant::now(),
            elapsed: Duration::ZERO,
            counts: Counts::default(),
            status: String::new(),
            todos: vec![],
            cost: 0.0,
            ctx_tokens: 0,
            verb: VERBS[0],
            out_tokens: 0,
            stream_chars: 0,
            asks: VecDeque::new(),
            session: Arc::new(Mutex::new(Session::mirror(vec![]))),
            turn,
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>()) }
}

/// One line per tab (vertical sidebar, `width` wide) or a single line (horizontal strip).
pub fn bar(tabs: &[(String, TabStatus, bool)], spin: usize, vertical: bool, width: u16) -> Vec<Line<'static>> {
    let cell = |i: usize, (title, status, active): &(String, TabStatus, bool), max: usize| -> Vec<Span<'static>> {
        let (glyph, color) = match status {
            TabStatus::Working => (SPIN[spin % SPIN.len()], theme::accent()),
            TabStatus::Done => ("●", theme::ok()),
            TabStatus::Error => ("!", theme::err()),
            TabStatus::Idle => ("·", theme::dim()),
        };
        let title = if title.trim().is_empty() { "new session" } else { title.trim() };
        let base = if *active { theme::sel() } else { theme::dim() };
        vec![
            Span::styled(" ", base),
            Span::styled(glyph, base.patch(color)),
            Span::styled(format!(" {} {} ", i + 1, clip(title, max)), base),
        ]
    };
    if vertical {
        let inner = width.saturating_sub(1) as usize;
        return tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut spans = cell(i, t, inner.saturating_sub(6));
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                let pad = inner.saturating_sub(used);
                let base = if t.2 { theme::sel() } else { theme::dim() };
                spans.push(Span::styled(" ".repeat(pad), base));
                spans.push(Span::styled("│", theme::dim()));
                Line::from(spans)
            })
            .collect();
    }
    let max = (width as usize / tabs.len().max(1)).saturating_sub(7).clamp(4, 24);
    let mut spans = Vec::new();
    for (i, t) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("│", theme::dim()));
        }
        spans.extend(cell(i, t, max));
    }
    vec![Line::from(spans)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(e: Event) -> Push {
        Push::Event(e)
    }

    #[test]
    fn background_status_follows_the_run() {
        let mut t = Tab::new(1);
        t.watch(&ev(Event::Started));
        assert_eq!(t.status, TabStatus::Working);
        t.watch(&ev(Event::Done));
        assert_eq!(t.status, TabStatus::Done);
        t.watch(&ev(Event::Started));
        t.watch(&ev(Event::Error("boom".into())));
        t.watch(&ev(Event::Done));
        assert_eq!(t.status, TabStatus::Error);
        t.watch(&ev(Event::Started));
        t.watch(&ev(Event::Interrupted));
        t.watch(&ev(Event::Done));
        assert_eq!(t.status, TabStatus::Idle);
    }

    #[test]
    fn title_comes_from_first_message_then_session_title() {
        let mut t = Tab::new(1);
        t.watch(&ev(Event::User("fix the bug".into())));
        t.watch(&ev(Event::User("second".into())));
        assert_eq!(t.title, "fix the bug");
        t.watch(&ev(Event::Title("Bug fix".into())));
        assert_eq!(t.title, "Bug fix");
    }
}
