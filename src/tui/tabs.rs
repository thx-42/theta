//! Session tabs. Every tab is its own daemon connection; only the active one is mirrored in `Ui`,
//! the others keep their state parked in a `View` and queue what the daemon pushes.

use super::{AskView, Counts, Editor, Item, SPIN, VERBS, theme};
use crate::agent::{Event, Turn};
use crate::client::Remote;
use crate::proto::Push;
use crate::session::Session;
use crate::tools::Todo;
use ratatui::layout::Rect;
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

/// Where the strip's controls landed on screen, for mouse clicks.
#[derive(Default)]
pub struct Strip {
    /// Per tab: the title (click switches) and the × (click closes).
    pub tabs: Vec<(Rect, Rect)>,
    /// The + button (click opens a new session).
    pub new: Rect,
}

pub fn hit(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.right() && row >= r.y && row < r.bottom()
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// Render the tab strip into `area` (one row, or a column of rows when vertical) and return its clickable parts.
pub fn strip(area: Rect, tabs: &[(String, TabStatus, bool)], spin: usize, vertical: bool) -> (Vec<Line<'static>>, Strip) {
    let mut out = Strip::default();
    let glyph = |status: TabStatus| match status {
        TabStatus::Working => (SPIN[spin % SPIN.len()], theme::accent()),
        TabStatus::Done => ("●", theme::ok()),
        TabStatus::Error => ("!", theme::err()),
        TabStatus::Idle => ("·", theme::dim()),
    };
    let name = |t: &str| if t.trim().is_empty() { "new session".to_string() } else { t.trim().to_string() };
    let style = |active: bool| if active { theme::sel() } else { theme::dim() };
    let mut lines = Vec::new();

    if vertical {
        let inner = area.width.saturating_sub(1) as usize;
        let body_w = inner.saturating_sub(2);
        for (i, (title, status, active)) in tabs.iter().enumerate() {
            let y = area.y + i as u16;
            let (g, c) = glyph(*status);
            let st = style(*active);
            let text = pad(&clip(&format!(" {} {}", i + 1, name(title)), body_w.saturating_sub(2)), body_w.saturating_sub(2));
            lines.push(Line::from(vec![
                Span::styled(" ", st),
                Span::styled(g, st.patch(c)),
                Span::styled(text, st),
                Span::styled(" ", st),
                Span::styled("×", st.patch(theme::dim())),
                Span::styled("│", theme::dim()),
            ]));
            out.tabs.push((Rect { x: area.x, y, width: inner.saturating_sub(1) as u16, height: 1 }, Rect { x: area.x + inner.saturating_sub(1) as u16, y, width: 1, height: 1 }));
        }
        let y = area.y + tabs.len() as u16;
        lines.push(Line::from(vec![Span::styled(pad(" + new session", inner), theme::accent()), Span::styled("│", theme::dim())]));
        out.new = Rect { x: area.x, y, width: inner as u16, height: 1 };
        return (lines, out);
    }

    let max = (area.width as usize / tabs.len().max(1)).saturating_sub(8).clamp(6, 24);
    let mut spans = Vec::new();
    let mut x = area.x;
    for (i, (title, status, active)) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("│", theme::dim()));
            x += 1;
        }
        let (g, c) = glyph(*status);
        let st = style(*active);
        let w = (3 + format!("{}", i + 1).len() + 1 + max.min(name(title).chars().count()) + 1) as u16;
        spans.push(Span::styled(" ", st));
        spans.push(Span::styled(g, st.patch(c)));
        spans.push(Span::styled(format!(" {} {} ", i + 1, clip(&name(title), max)), st));
        let tab = Rect { x, y: area.y, width: w, height: 1 };
        x += w;
        spans.push(Span::styled("×", st.patch(theme::dim())));
        spans.push(Span::styled(" ", st));
        out.tabs.push((tab, Rect { x, y: area.y, width: 1, height: 1 }));
        x += 2;
    }
    spans.push(Span::styled(" + ", theme::accent()));
    out.new = Rect { x, y: area.y, width: 3, height: 1 };
    lines.push(Line::from(spans));
    (lines, out)
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
