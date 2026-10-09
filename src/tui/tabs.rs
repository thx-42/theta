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

/// Sidebar width bounds, in columns.
pub const WIDTH_MIN: u16 = 14;
pub const WIDTH_MAX: u16 = 60;
/// Rows per tab card in the sidebar: title, agent · model, status, spacer.
const CARD: usize = 4;

/// What the strip shows for one tab.
pub struct TabInfo {
    pub title: String,
    pub status: TabStatus,
    pub agent: String,
    pub model: String,
    /// Live detail: the current step while working, else a status word.
    pub detail: String,
    pub active: bool,
}

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
    pub agent: String,
    pub model: String,
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
        Tab { id, title: String::new(), agent: String::new(), model: String::new(), status: TabStatus::Idle, errored: false, backlog: vec![], view: None }
    }

    /// Follow a push meant for a tab that is not on screen.
    pub fn watch(&mut self, p: &Push) {
        match p {
            Push::Meta { agent, model, .. } => {
                self.agent = agent.clone();
                self.model = model.clone();
            }
            Push::Snapshot(s) => {
                self.agent = s.agent.clone();
                self.model = s.model.clone();
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
    /// Per tab: the whole tab (click switches) and its × (click closes).
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

fn short_model(m: &str) -> &str {
    m.rsplit('/').next().unwrap_or(m)
}

/// Render the tab strip into `area` and return its clickable parts.
/// Horizontal: one row. Vertical: a card of `CARD` rows per tab, then a + row, then the divider.
pub fn strip(area: Rect, tabs: &[TabInfo], spin: usize, vertical: bool) -> (Vec<Line<'static>>, Strip) {
    let mut out = Strip::default();
    let glyph = |status: TabStatus| match status {
        TabStatus::Working => (SPIN[spin % SPIN.len()], theme::accent()),
        TabStatus::Done => ("●", theme::ok()),
        TabStatus::Error => ("!", theme::err()),
        TabStatus::Idle => ("·", theme::dim()),
    };
    let name = |t: &str| if t.trim().is_empty() { "new session".to_string() } else { t.trim().to_string() };
    let style = |active: bool| if active { theme::sel() } else { theme::dim() };

    if vertical {
        let w = area.width.saturating_sub(1) as usize; // last column is the divider
        let divider = || Span::styled("│", theme::dim());
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, t) in tabs.iter().enumerate() {
            let y0 = area.y + (i * CARD) as u16;
            let st = style(t.active);
            let (g, c) = glyph(t.status);
            let idx = format!(" {} ", i + 1);
            // Row 1: glyph, number, title, then " ×" at the right edge.
            let room = w.saturating_sub(1 + 1 + idx.len() + 2);
            let title = clip(&name(&t.title), room);
            lines.push(Line::from(vec![
                Span::styled(" ", st),
                Span::styled(g, st.patch(c)),
                Span::styled(pad(&format!("{idx}{title}"), w.saturating_sub(4)), st),
                Span::styled(" ", st),
                Span::styled("×", st.patch(theme::dim())),
                divider(),
            ]));
            // Row 2: agent · model. Row 3: what it is doing. Row 4: spacer.
            let who = if t.agent.is_empty() { String::new() } else { format!("   {} · {}", t.agent, short_model(&t.model)) };
            lines.push(Line::from(vec![Span::styled(pad(&clip(&who, w), w), theme::dim()), divider()]));
            let detail_style = match t.status {
                TabStatus::Working => theme::accent(),
                TabStatus::Done => theme::ok(),
                TabStatus::Error => theme::err(),
                TabStatus::Idle => theme::dim(),
            };
            lines.push(Line::from(vec![Span::styled(pad(&clip(&format!("   {}", t.detail), w), w), detail_style), divider()]));
            lines.push(Line::from(vec![Span::raw(pad("", w)), divider()]));
            out.tabs.push((
                Rect { x: area.x, y: y0, width: w as u16, height: CARD as u16 },
                Rect { x: area.x + w as u16 - 1, y: y0, width: 1, height: 1 },
            ));
        }
        let y = area.y + (tabs.len() * CARD) as u16;
        lines.push(Line::from(vec![Span::styled(pad(" + new session", w), theme::accent()), divider()]));
        out.new = Rect { x: area.x, y, width: w as u16, height: 1 };
        // Keep the divider running down the empty space below the cards.
        while lines.len() < area.height as usize {
            lines.push(Line::from(vec![Span::raw(pad("", w)), divider()]));
        }
        return (lines, out);
    }

    let max = (area.width as usize / tabs.len().max(1)).saturating_sub(8).clamp(6, 24);
    let mut spans = Vec::new();
    let mut x = area.x;
    for (i, t) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("│", theme::dim()));
            x += 1;
        }
        let (g, c) = glyph(t.status);
        let st = style(t.active);
        let w = (5 + format!("{}", i + 1).len() + max.min(name(&t.title).chars().count())) as u16;
        spans.push(Span::styled(" ", st));
        spans.push(Span::styled(g, st.patch(c)));
        spans.push(Span::styled(format!(" {} {} ", i + 1, clip(&name(&t.title), max)), st));
        let tab = Rect { x, y: area.y, width: w, height: 1 };
        x += w;
        spans.push(Span::styled("×", st.patch(theme::dim())));
        spans.push(Span::styled(" ", st));
        out.tabs.push((tab, Rect { x, y: area.y, width: 1, height: 1 }));
        x += 2;
    }
    spans.push(Span::styled(" + ", theme::accent()));
    out.new = Rect { x, y: area.y, width: 3, height: 1 };
    (vec![Line::from(spans)], out)
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
