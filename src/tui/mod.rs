//! Full-screen ratatui interface.

mod markdown;
mod tree;

use crate::agent::{self, Event};
use crate::auth::{self, Method};
use crate::agents::{self, Agent};
use crate::config;
use crate::session::{self, Kind, Session};
use crate::tools::{self, Kind as ToolKind, Todo};
use crate::types::{Block, Msg, Role};
use crate::App;
use anyhow::Result;
use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, BorderType, Clear, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use unicode_width::UnicodeWidthStr;

pub mod theme {
    use ratatui::style::{Color, Modifier, Style};
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Current agent's color, as a 256-color palette index (works without truecolor).
    static ACCENT: AtomicU8 = AtomicU8::new(51);

    /// Stable color from an agent name: FNV-1a hash → hue, fixed saturation/lightness → 6×6×6 color cube.
    pub fn color_for(name: &str) -> u8 {
        let h = name.to_lowercase().bytes().fold(0x811c9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x01000193));
        let hue = (h % 360) as f32;
        let (s, l) = (0.75, 0.62);
        let c = (1.0 - (2.0 * l - 1.0f32).abs()) * s;
        let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
        let (r, g, b) = match hue as u32 / 60 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let q = |v: f32| (((v + l - c / 2.0) * 5.0).round() as u8).min(5);
        16 + 36 * q(r) + 6 * q(g) + q(b)
    }

    pub fn set_accent(name: &str) {
        ACCENT.store(color_for(name), Ordering::Relaxed);
    }

    fn accent_color() -> Color { Color::Indexed(ACCENT.load(Ordering::Relaxed)) }
    pub fn accent() -> Style { Style::default().fg(accent_color()) }
    pub fn dim() -> Style { Style::default().fg(Color::DarkGray) }
    pub fn code() -> Style { Style::default().fg(Color::Yellow) }
    pub fn link() -> Style { Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED) }
    pub fn ok() -> Style { Style::default().fg(Color::Green) }
    pub fn err() -> Style { Style::default().fg(Color::Red) }
    pub fn user() -> Style { Style::default().add_modifier(Modifier::BOLD) }
    pub fn logo() -> Style { Style::default().fg(Color::Black).bg(accent_color()).add_modifier(Modifier::BOLD) }
    pub fn sel() -> Style { Style::default().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD) }
}

const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Inference indicator glyphs (pulses like Claude Code's).
const PULSE: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const VERBS: &[&str] = &["Thinking", "Pondering", "Reasoning", "Working", "Computing", "Brewing", "Forging", "Weaving", "Crafting", "Mulling"];

const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a new session"),
    ("/resume", "open a previous session"),
    ("/tree", "browse and fork the conversation tree"),
    ("/model", "change model"),
    ("/agent", "change agent"),
    ("/effort", "reasoning effort"),
    ("/settings", "settings (verbosity, task models, compaction, web)"),
    ("/verbose", "toggle full / compact output"),
    ("/compact", "compact context now"),
    ("/title", "rename the session"),
    ("/login", "log in to a provider"),
    ("/logout", "remove provider credentials"),
    ("/copy", "copy the last answer"),
    ("/help", "keys and commands"),
    ("/quit", "exit"),
];

/// Short one-line summary of a tool call's arguments.
pub fn tool_summary(name: &str, args: &Value) -> String {
    let g = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let s = match name {
        "read" => {
            let mut p = g("path");
            if let Some(o) = args.get("offset").and_then(|v| v.as_u64()) {
                p.push_str(&format!(":{o}"));
            }
            p
        }
        "write" | "edit" | "ls" => g("path"),
        "bash" => g("command").lines().next().unwrap_or("").to_string(),
        "grep" => format!("{} {}", g("pattern"), g("path")),
        "find" => g("pattern"),
        "web_search" => g("query"),
        "web_fetch" => g("url"),
        "todo" => format!("{} items", args.get("todos").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0)),
        "task" => g("description"),
        "ask" => g("question"),
        "handoff" => format!("→ {} · {}", args.get("agent").and_then(|v| v.as_str()).unwrap_or("Build"), g("summary")),
        _ => args.to_string(),
    };
    let s = s.trim().to_string();
    if s.chars().count() > 90 { format!("{}…", s.chars().take(90).collect::<String>()) } else { s }
}

#[derive(Default, Clone, Copy)]
struct Counts {
    read: u32,
    write: u32,
    cmd: u32,
    tools: u32,
}

impl Counts {
    fn add(&mut self, name: &str) {
        match tools::kind(name) {
            ToolKind::Read => self.read += 1,
            ToolKind::Write => self.write += 1,
            ToolKind::Command => self.cmd += 1,
            ToolKind::Other => self.tools += 1,
        }
    }
    fn spans(&self) -> Vec<Span<'static>> {
        let mut v = Vec::new();
        for (n, label) in [(self.read, "read"), (self.write, "write"), (self.cmd, "cmd"), (self.tools, "tools")] {
            if !v.is_empty() {
                v.push(Span::styled(" · ", theme::dim()));
            }
            v.push(Span::styled(n.to_string(), if n > 0 { Style::default().add_modifier(Modifier::BOLD) } else { theme::dim() }));
            v.push(Span::styled(format!(" {label}"), theme::dim()));
        }
        v
    }
}

enum ToolState {
    Running,
    Done { ok: bool },
}

enum Item {
    User(String),
    Assistant { text: String, thinking: String, done: bool },
    Tool { id: String, name: String, summary: String, state: ToolState, out: String, display: Option<String>, sub: Counts },
    Info(String),
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Main,
    Compaction,
    Title,
    Subagent,
    /// Index into `rt.agents`
    Agent(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    General,
    Models,
}

#[derive(Clone, Copy)]
struct SettingsView {
    tab: SettingsTab,
    sel: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    Model(Target),
    Agent,
    Session,
    Login,
    LoginMethod,
    Logout,
    Effort,
}

struct PickItem {
    label: String,
    detail: String,
    value: String,
    /// Section header shown above the first item of each group (e.g. provider).
    group: String,
}

fn item(label: impl Into<String>, detail: impl Into<String>, value: impl Into<String>) -> PickItem {
    PickItem { label: label.into(), detail: detail.into(), value: value.into(), group: String::new() }
}

struct Picker {
    title: String,
    items: Vec<PickItem>,
    filter: String,
    sel: usize,
    purpose: Purpose,
}

impl Picker {
    fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| f.is_empty() || f.split_whitespace().all(|w| format!("{} {} {}", it.group, it.label, it.detail).to_lowercase().contains(w)))
            .map(|(i, _)| i)
            .collect()
    }

    /// Visible items plus one header row per group.
    fn rows(&self) -> usize {
        let vis = self.visible();
        let groups = vis.iter().enumerate().filter(|&(n, &i)| !self.items[i].group.is_empty() && (n == 0 || self.items[vis[n - 1]].group != self.items[i].group)).count();
        vis.len() + groups
    }
}

/// A question from the `ask` tool. `sel == options.len()` is the free-text row.
struct AskView {
    question: String,
    options: Vec<String>,
    sel: usize,
    input: Editor,
    reply: agent::Reply,
}

impl AskView {
    fn answer(self, text: String) {
        if let Some(tx) = self.reply.lock().unwrap().take() {
            let _ = tx.send(text);
        }
    }
}

/// Login in progress: API-key entry, or an OAuth flow running in the background.
struct LoginView {
    title: String,
    /// Provider id the credential is saved under.
    provider: String,
    key: bool,
    lines: Vec<String>,
    input: Editor,
    paste: Option<UnboundedSender<String>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

enum LoginMsg {
    Say(String),
    Done(Result<(), String>),
}

enum Overlay {
    Pick(Picker),
    Tree(tree::TreeView),
    Settings(SettingsView),
    Help,
    Ask(AskView),
    Login(LoginView),
}

#[derive(Default)]
struct Editor {
    text: String,
    cur: usize,
}

impl Editor {
    fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cur, s);
        self.cur += s.len();
    }
    fn left(&mut self) {
        if let Some((i, _)) = self.text[..self.cur].char_indices().next_back() {
            self.cur = i;
        }
    }
    fn right(&mut self) {
        if let Some(c) = self.text[self.cur..].chars().next() {
            self.cur += c.len_utf8();
        }
    }
    fn backspace(&mut self) {
        if self.cur > 0 {
            let end = self.cur;
            self.left();
            self.text.replace_range(self.cur..end, "");
        }
    }
    fn delete(&mut self) {
        if self.cur < self.text.len() {
            let start = self.cur;
            self.right();
            self.text.replace_range(start..self.cur, "");
            self.cur = start;
        }
    }
    fn delete_word(&mut self) {
        let before = &self.text[..self.cur];
        let trimmed = before.trim_end();
        let start = trimmed.rfind(|c: char| c.is_whitespace()).map(|i| i + 1).unwrap_or(0);
        self.text.replace_range(start..self.cur, "");
        self.cur = start;
    }
    fn home(&mut self) {
        self.cur = self.text[..self.cur].rfind('\n').map(|i| i + 1).unwrap_or(0);
    }
    fn end(&mut self) {
        self.cur = self.text[self.cur..].find('\n').map(|i| self.cur + i).unwrap_or(self.text.len());
    }
    fn set(&mut self, s: String) {
        self.cur = s.len();
        self.text = s;
    }
    fn take(&mut self) -> String {
        self.cur = 0;
        std::mem::take(&mut self.text)
    }
    /// Wrapped lines (char-level) and cursor (row, col) for `width`.
    fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(4);
        let mut lines = vec![String::new()];
        let mut w = 0;
        let mut cursor = (0, 0);
        for (i, c) in self.text.char_indices() {
            if i == self.cur {
                cursor = (lines.len() - 1, w);
            }
            if c == '\n' {
                lines.push(String::new());
                w = 0;
                continue;
            }
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > width {
                lines.push(String::new());
                w = 0;
            }
            lines.last_mut().unwrap().push(c);
            w += cw;
        }
        if self.cur >= self.text.len() {
            if w >= width {
                lines.push(String::new());
                w = 0;
            }
            cursor = (lines.len() - 1, w);
        }
        (lines, cursor)
    }
}

struct Ui {
    app: App,
    items: Vec<Item>,
    cache: Vec<Option<(usize, bool, Vec<Line<'static>>)>>,
    dirty: Vec<bool>,
    input: Editor,
    history: Vec<String>,
    hist: Option<usize>,
    scroll_top: Option<usize>,
    last_total: usize,
    last_view: usize,
    run_rx: Option<UnboundedReceiver<Event>>,
    run: Option<tokio::task::JoinHandle<()>>,
    started: Instant,
    elapsed: Duration,
    counts: Counts,
    status: String,
    overlay: Option<Overlay>,
    todos: Vec<Todo>,
    cost: f64,
    ctx_tokens: u64,
    app_tx: UnboundedSender<Event>,
    quit_armed: Option<Instant>,
    notice: Option<(String, Instant)>,
    spin: usize,
    verb: &'static str,
    /// Output tokens of finished steps + streamed chars of the current one (for the live counter).
    out_tokens: u64,
    stream_chars: usize,
    asks: VecDeque<AskView>,
    login_tx: UnboundedSender<LoginMsg>,
    cmd_sel: usize,
    /// Agent to switch to when the current run ends (accepted `handoff`).
    handoff: Option<String>,
    /// Settings view to return to after a model picker opened from it.
    settings_back: Option<SettingsView>,
    quit: bool,
}

fn agent_label(a: &Agent) -> String {
    format!("{} ({})", a.name, a.scope.label())
}

impl Ui {
    fn verbose(&self) -> bool {
        self.app.rt.settings.verbose != "compact"
    }

    fn notify(&mut self, s: impl Into<String>) {
        self.notice = Some((s.into(), Instant::now()));
    }

    fn push(&mut self, it: Item) {
        self.items.push(it);
        self.cache.push(None);
        self.dirty.push(true);
    }

    fn touch(&mut self, i: usize) {
        if let Some(d) = self.dirty.get_mut(i) {
            *d = true;
        }
    }

    fn invalidate_all(&mut self) {
        self.cache.iter_mut().for_each(|c| *c = None);
    }

    /// Rebuild chat items from the active branch of the session.
    fn rebuild(&mut self) {
        self.items.clear();
        self.cache.clear();
        self.dirty.clear();
        self.cost = 0.0;
        let entries: Vec<(Kind, Option<String>)> = {
            let s = self.app.session.lock().unwrap();
            s.path_to(s.leaf.as_deref()).into_iter().map(|e| (e.kind.clone(), Some(e.id.clone()))).collect()
        };
        for (kind, _) in entries {
            match kind {
                Kind::Msg { msg, usage } => {
                    if let (Some(u), Some(m)) = (&usage, &msg.model)
                        && let Some((_, model)) = self.app.rt.catalog.resolve(m) {
                            self.cost += model.cost(u);
                        }
                    self.add_msg_items(&msg);
                }
                Kind::Compaction { tokens_before, .. } => self.push(Item::Info(format!("context compacted ({}k tokens summarized)", tokens_before / 1000))),
                _ => {}
            }
        }
        let s = self.app.session.lock().unwrap();
        self.ctx_tokens = crate::compact::current_tokens(&s);
        drop(s);
        self.scroll_top = None;
    }

    fn add_msg_items(&mut self, msg: &Msg) {
        match msg.role {
            Role::User => {
                for b in &msg.content {
                    match b {
                        Block::Text { text } => self.push(Item::User(text.clone())),
                        Block::ToolResult { id, content, is_error } => {
                            if let Some(i) = self.items.iter().rposition(|it| matches!(it, Item::Tool { id: tid, .. } if tid == id))
                                && let Item::Tool { state, out, .. } = &mut self.items[i] {
                                    *state = ToolState::Done { ok: !is_error };
                                    *out = content.clone();
                                }
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                let thinking: String = msg.content.iter().filter_map(|b| if let Block::Thinking { text, .. } = b { Some(text.as_str()) } else { None }).collect();
                let text = msg.text();
                if !text.is_empty() || !thinking.is_empty() {
                    self.push(Item::Assistant { text, thinking, done: true });
                }
                for (id, name, args) in msg.tool_calls() {
                    let summary = tool_summary(&name, &args);
                    self.push(Item::Tool { id, name, summary, state: ToolState::Running, out: String::new(), display: None, sub: Counts::default() });
                }
            }
        }
    }

    fn model_ctx(&self) -> u64 {
        self.app.rt.catalog.resolve(&self.app.turn.model).map(|(_, m)| m.context).unwrap_or(0)
    }

    // ---------- running ----------

    fn send(&mut self, text: String) {
        if self.run.is_some() {
            self.notify("agent is running — Esc to interrupt");
            self.input.set(text);
            return;
        }
        let added = self.app.session.lock().unwrap().add_msg(Msg::user(text.clone()), None);
        if let Err(e) = added {
            self.push(Item::Error(format!("{e:#}")));
            return;
        }
        self.history.push(text.clone());
        self.hist = None;
        self.push(Item::User(text));
        self.start();
    }

    fn start(&mut self) {
        let (tx, rx) = unbounded_channel();
        let rt = self.app.rt.clone();
        let session = self.app.session.clone();
        let turn = self.app.turn.clone();
        self.run = Some(tokio::spawn(async move {
            if let Err(e) = agent::run(rt, session, turn, tx.clone()).await {
                let _ = tx.send(Event::Error(format!("{e:#}")));
            }
            let _ = tx.send(Event::Done);
        }));
        self.run_rx = Some(rx);
        self.started = Instant::now();
        self.counts = Counts::default();
        self.status = "thinking".into();
        self.verb = VERBS[rand::random_range(0..VERBS.len())];
        self.out_tokens = 0;
        self.stream_chars = 0;
        self.scroll_top = None;
    }

    fn interrupt(&mut self) {
        if let Some(h) = self.run.take() {
            h.abort();
            self.run_rx = None;
            self.elapsed = self.started.elapsed();
            for i in 0..self.items.len() {
                if let Item::Tool { state: state @ ToolState::Running, .. } = &mut self.items[i] {
                    *state = ToolState::Done { ok: false };
                    self.touch(i);
                }
            }
            self.asks.clear();
            self.handoff = None;
            if matches!(self.overlay, Some(Overlay::Ask(_))) {
                self.overlay = None;
            }
            self.push(Item::Info("interrupted".into()));
        }
    }

    fn last_assistant_streaming(&mut self) -> usize {
        if let Some(Item::Assistant { done: false, .. }) = self.items.last() {
            return self.items.len() - 1;
        }
        self.push(Item::Assistant { text: String::new(), thinking: String::new(), done: false });
        self.items.len() - 1
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Text(t) => {
                let i = self.last_assistant_streaming();
                self.stream_chars += t.len();
                if let Item::Assistant { text, .. } = &mut self.items[i] {
                    text.push_str(&t);
                }
                self.touch(i);
                self.status = "writing".into();
            }
            Event::Thinking(t) => {
                let i = self.last_assistant_streaming();
                self.stream_chars += t.len();
                if let Item::Assistant { thinking, .. } = &mut self.items[i] {
                    thinking.push_str(&t);
                }
                self.touch(i);
                self.status = "thinking".into();
            }
            Event::ToolPending(name) => self.status = format!("preparing {name}"),
            Event::ToolStart { id, name, args } => {
                self.close_streaming();
                self.counts.add(&name);
                self.status = format!("{name} {}", tool_summary(&name, &args));
                let summary = tool_summary(&name, &args);
                if let Some(i) = self.items.iter().rposition(|it| matches!(it, Item::Tool { id: tid, .. } if *tid == id)) {
                    self.touch(i);
                } else {
                    self.push(Item::Tool { id, name, summary, state: ToolState::Running, out: String::new(), display: None, sub: Counts::default() });
                }
            }
            Event::ToolEnd { id, content, is_error, display, .. } => {
                if let Some(i) = self.items.iter().rposition(|it| matches!(it, Item::Tool { id: tid, .. } if *tid == id)) {
                    if let Item::Tool { state, out, display: d, .. } = &mut self.items[i] {
                        *state = ToolState::Done { ok: !is_error };
                        *out = content;
                        *d = display;
                    }
                    self.touch(i);
                }
                self.status = "thinking".into();
            }
            Event::Step { usage, model } => {
                self.close_streaming();
                if let Some((_, m)) = self.app.rt.catalog.resolve(&model) {
                    self.cost += m.cost(&usage);
                }
                self.ctx_tokens = usage.context();
                self.out_tokens += usage.output;
                self.stream_chars = 0;
            }
            Event::Handoff { agent } => self.handoff = Some(agent),
            Event::Ask { question, options, reply } => {
                self.status = "waiting for your answer".into();
                self.asks.push_back(AskView { question, options, sel: 0, input: Editor::default(), reply });
            }
            Event::Todos(t) => self.todos = t,
            Event::Compacting => {
                self.status = "compacting context".into();
                self.push(Item::Info("compacting context…".into()));
            }
            Event::Compacted { before } => {
                self.push(Item::Info(format!("context compacted ({}k tokens summarized)", before / 1000)));
                self.ctx_tokens = crate::compact::current_tokens(&self.app.session.lock().unwrap());
            }
            Event::Title(t) => {
                let _ = self.app.session.lock().unwrap().set_title(t.clone());
                self.notify(format!("session: {t}"));
            }
            Event::Sub { id, event } => {
                if let Event::ToolStart { name, .. } = *event {
                    self.counts.add(&name);
                    if let Some(i) = self.items.iter().rposition(|it| matches!(it, Item::Tool { id: tid, .. } if *tid == id)) {
                        if let Item::Tool { sub, .. } = &mut self.items[i] {
                            sub.add(&name);
                        }
                        self.touch(i);
                    }
                }
            }
            Event::Error(e) => {
                self.close_streaming();
                self.push(Item::Error(e));
            }
            Event::Done => {
                self.close_streaming();
                self.run = None;
                self.run_rx = None;
                self.elapsed = self.started.elapsed();
                self.maybe_title();
                if let Some(name) = self.handoff.take()
                    && let Some(a) = crate::agents::find(&self.app.rt.agents, &name).cloned() {
                        self.switch_agent(a);
                        self.send("Plan approved. Implement it now, following the todo list.".into());
                    }
            }
        }
    }

    fn close_streaming(&mut self) {
        if let Some(Item::Assistant { done, .. }) = self.items.last_mut() {
            *done = true;
            let i = self.items.len() - 1;
            self.touch(i);
        }
    }

    fn maybe_title(&mut self) {
        let first = {
            let s = self.app.session.lock().unwrap();
            if s.title.is_some() || !s.has_messages() {
                return;
            }
            s.context().first().map(|m| m.text()).unwrap_or_default()
        };
        let rt = self.app.rt.clone();
        let model = self.app.turn.model.clone();
        let tx = self.app_tx.clone();
        // Mark as titled now so we don't spawn twice.
        let _ = self.app.session.lock().unwrap().set_title(first.chars().take(50).collect());
        tokio::spawn(async move {
            if let Ok(t) = agent::make_title(&rt, &model, &first).await
                && !t.is_empty() {
                    let _ = tx.send(Event::Title(t));
                }
        });
    }

    // ---------- commands ----------

    fn command(&mut self, line: &str) {
        let (cmd, arg) = line.split_once(' ').map(|(c, a)| (c, a.trim())).unwrap_or((line, ""));
        match cmd {
            "/new" => {
                self.interrupt();
                self.app.session = Arc::new(Mutex::new(Session::new(&self.app.rt.cwd)));
                self.todos.clear();
                *self.app.rt.todos.lock().unwrap() = vec![];
                self.app.rt.read_cache.lock().unwrap().clear();
                self.rebuild();
                self.notify("new session");
            }
            "/resume" => self.open_sessions(),
            "/tree" => self.open_tree(),
            "/model" => self.open_models(Target::Main),
            "/agent" => self.open_agents(),
            "/effort" => {
                if arg.is_empty() {
                    let items = ["low", "medium", "high", "xhigh", "max"]
                        .iter()
                        .map(|e| item(*e, if *e == self.app.turn.effort { "current" } else { "" }, *e))
                        .collect();
                    self.overlay = Some(Overlay::Pick(Picker { title: "Reasoning effort".into(), items, filter: String::new(), sel: 0, purpose: Purpose::Effort }));
                } else {
                    self.set_effort(arg);
                }
            }
            "/settings" => self.overlay = Some(Overlay::Settings(SettingsView { tab: SettingsTab::General, sel: 0 })),
            "/verbose" => self.toggle_verbose(),
            "/compact" => self.compact_now(),
            "/title" => {
                if arg.is_empty() {
                    self.notify("usage: /title <name>");
                } else {
                    let _ = self.app.session.lock().unwrap().set_title(arg.to_string());
                    self.notify(format!("session: {arg}"));
                }
            }
            "/login" => {
                if arg.is_empty() {
                    self.open_providers(Purpose::Login);
                } else {
                    self.open_login_methods(arg);
                }
            }
            "/logout" => {
                if arg.is_empty() {
                    self.open_providers(Purpose::Logout);
                } else {
                    self.logout(arg);
                }
            }
            "/copy" => self.copy_last(),
            "/help" => self.overlay = Some(Overlay::Help),
            "/quit" | "/exit" => self.quit = true,
            _ => self.notify(format!("unknown command {cmd}")),
        }
    }

    fn logout(&mut self, id: &str) {
        let auth = self.app.rt.auth.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let _ = auth.remove(&id).await;
        });
        self.notify("credentials removed");
    }

    fn copy_last(&mut self) {
        let text = self.app.session.lock().unwrap().context().iter().rev().find(|m| m.role == Role::Assistant && !m.text().is_empty()).map(|m| m.text());
        let Some(text) = text else { return self.notify("nothing to copy") };
        self.notify(if clipboard(&text) { "copied" } else { "no clipboard tool found" });
    }

    fn compact_now(&mut self) {
        if self.run.is_some() {
            return self.notify("busy");
        }
        let (tx, rx) = unbounded_channel();
        let rt = self.app.rt.clone();
        let session = self.app.session.clone();
        let model = self.app.turn.model.clone();
        self.run = Some(tokio::spawn(async move {
            let _ = tx.send(Event::Compacting);
            match crate::compact::run(&rt, &session, &model).await {
                Ok(before) => {
                    let _ = tx.send(Event::Compacted { before });
                }
                Err(e) => {
                    let _ = tx.send(Event::Error(format!("compaction: {e:#}")));
                }
            }
            let _ = tx.send(Event::Done);
        }));
        self.run_rx = Some(rx);
        self.started = Instant::now();
        self.counts = Counts::default();
    }

    fn update_settings(&mut self, key: &str, value: toml::Value, apply: impl FnOnce(&mut config::Settings)) {
        let mut s = (*self.app.rt.settings).clone();
        apply(&mut s);
        self.app.rt.settings = Arc::new(s);
        if let Err(e) = config::set_global(key, value) {
            self.notify(format!("could not save settings: {e}"));
        }
        self.invalidate_all();
    }

    fn toggle_verbose(&mut self) {
        let v = if self.verbose() { "compact" } else { "full" };
        self.update_settings("verbose", v.into(), |s| s.verbose = v.into());
        self.notify(format!("output: {v}"));
    }

    fn set_effort(&mut self, e: &str) {
        self.app.turn.effort = e.to_string();
        self.update_settings("effort", e.into(), |s| s.effort = e.into());
        self.notify(format!("effort: {e}"));
    }

    /// Write `[agents.<name>].model` or `.effort`, then apply it now if that agent is running.
    fn set_agent_setting(&mut self, idx: usize, field: &str, value: String) {
        let Some(agent) = self.app.rt.agents.get(idx).cloned() else { return };
        let name = agent.name.clone();
        let key = format!("agents.{name}.{field}");
        let (f, v) = (field.to_string(), value.clone());
        self.update_settings(&key, value.into(), |s| {
            let k = s.agents.keys().find(|k| k.eq_ignore_ascii_case(&name)).cloned().unwrap_or_else(|| name.clone());
            let e = s.agents.entry(k).or_default();
            if f == "model" { e.model = v } else { e.effort = v }
        });
        if self.app.turn.agent.name.eq_ignore_ascii_case(&name) {
            let s = &self.app.rt.settings;
            self.app.turn.model = agents::model_of(&agent, s).unwrap_or_else(|| s.model.clone());
            self.app.turn.effort = agents::effort_of(&agent, s).unwrap_or_else(|| s.effort.clone());
        }
    }

    fn back_to_settings(&mut self) {
        if let Some(v) = self.settings_back.take() {
            self.overlay = Some(Overlay::Settings(v));
        }
    }

    fn open_models(&mut self, target: Target) {
        let rt = &self.app.rt;
        let current = match target {
            Target::Main => self.app.turn.model.clone(),
            Target::Compaction => rt.settings.models.compaction.clone(),
            Target::Title => rt.settings.models.title.clone(),
            Target::Subagent => rt.settings.models.subagent.clone(),
            Target::Agent(i) => rt.settings.agent(&rt.agents[i].name).map(|a| a.model.clone()).unwrap_or_default(),
        };
        let mut items = Vec::new();
        match target {
            Target::Main => {}
            Target::Agent(_) => items.push(item("(default)", "from agent file or main model", "")),
            _ => items.push(item("(same as main model)", "", "")),
        }
        // Only providers we can actually call, grouped in catalog order.
        for p in rt.catalog.providers.iter() {
            let local = p.env.is_empty() && p.oauth.is_none();
            let usable = futures::executor::block_on(rt.auth.status(p)).is_some() || (local && !p.models.is_empty());
            if !usable {
                continue;
            }
            for m in rt.catalog.models_of(p) {
                let key = m.key();
                let price = if m.cost_in > 0.0 { format!("  ${}/{}", m.cost_in, m.cost_out) } else { String::new() };
                let detail = format!("{}k{price}{}", m.context / 1000, if key == current { "  ← current" } else { "" });
                items.push(PickItem { label: m.id.clone(), detail, value: key, group: p.name.clone() });
            }
        }
        if items.iter().all(|i| i.value.is_empty()) {
            self.notify("no provider connected — log in first");
            return self.open_providers(Purpose::Login);
        }
        let title = match target {
            Target::Main => "Model".to_string(),
            Target::Compaction => "Compaction model".to_string(),
            Target::Title => "Session-title model".to_string(),
            Target::Subagent => "Subagent model".to_string(),
            Target::Agent(i) => format!("{} model", rt.agents[i].name),
        };
        let sel = items.iter().position(|i| i.value == current).unwrap_or(0);
        self.overlay = Some(Overlay::Pick(Picker { title: format!("{title}  (type to filter)"), items, filter: String::new(), sel, purpose: Purpose::Model(target) }));
    }

    fn open_agents(&mut self) {
        let items = self
            .app
            .rt
            .agents
            .iter()
            .map(|a| item(agent_label(a), a.description.clone(), a.name.clone()))
            .collect();
        self.overlay = Some(Overlay::Pick(Picker { title: "Agent".into(), items, filter: String::new(), sel: 0, purpose: Purpose::Agent }));
    }

    fn open_sessions(&mut self) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let items: Vec<PickItem> = session::list(&self.app.rt.cwd)
            .into_iter()
            .map(|i| item(i.title, format!("{} msgs · {}", i.messages, ago(now.saturating_sub(i.updated))), i.path.display().to_string()))
            .collect();
        if items.is_empty() {
            return self.notify("no sessions yet in this directory");
        }
        self.overlay = Some(Overlay::Pick(Picker { title: "Sessions".into(), items, filter: String::new(), sel: 0, purpose: Purpose::Session }));
    }

    fn open_providers(&mut self, purpose: Purpose) {
        let rt = &self.app.rt;
        let items: Vec<PickItem> = rt
            .catalog
            .providers
            .iter()
            .filter(|p| if purpose == Purpose::Login { auth::shows_in_login(p) } else { futures::executor::block_on(rt.auth.status(p)).is_some() })
            .map(|p| {
                let st = futures::executor::block_on(rt.auth.status(p));
                let kinds: Vec<&str> = auth::login_options(p).iter().map(|o| if o.method == Method::Key { "api key" } else { "subscription" }).collect();
                item(p.name.clone(), format!("{}{}", kinds.join(" / "), st.map(|s| format!("  ● {s}")).unwrap_or_default()), p.id.clone())
            })
            .collect();
        if items.is_empty() {
            return self.notify("no stored credentials");
        }
        let title = if purpose == Purpose::Login { "Log in" } else { "Log out" };
        self.overlay = Some(Overlay::Pick(Picker { title: title.into(), items, filter: String::new(), sel: 0, purpose }));
    }

    /// Second step of /login: how to authenticate (subscription or API key). Skipped when there is one way.
    fn open_login_methods(&mut self, id: &str) {
        let Some(p) = self.app.rt.catalog.provider(id).cloned() else { return self.notify(format!("unknown provider {id}")) };
        let opts = auth::login_options(&p);
        match opts.len() {
            0 => self.notify(format!("{} needs no login", p.name)),
            1 => self.start_login(&p.id, 0),
            _ => {
                let items = opts.iter().enumerate().map(|(i, o)| item(o.label, "", format!("{}\t{i}", p.id))).collect();
                self.overlay = Some(Overlay::Pick(Picker { title: format!("Log in to {}", p.name), items, filter: String::new(), sel: 0, purpose: Purpose::LoginMethod }));
            }
        }
    }

    fn start_login(&mut self, id: &str, idx: usize) {
        let Some(p) = self.app.rt.catalog.provider(id).cloned() else { return };
        let Some(o) = auth::login_options(&p).into_iter().nth(idx) else { return };
        let mut v = LoginView { title: format!("{} · {}", p.name, o.label), provider: o.provider.clone(), key: o.method == Method::Key, lines: vec![], input: Editor::default(), paste: None, task: None };
        if let Method::Oauth(kind) = o.method {
            let (say, mut said) = unbounded_channel();
            let (paste_tx, paste) = unbounded_channel();
            let (tx, auth, provider) = (self.login_tx.clone(), self.app.rt.auth.clone(), o.provider);
            v.lines.push("starting…".into());
            v.paste = Some(paste_tx);
            v.task = Some(tokio::spawn(async move {
                let fwd_tx = tx.clone();
                let fwd = tokio::spawn(async move {
                    while let Some(m) = said.recv().await {
                        let _ = fwd_tx.send(LoginMsg::Say(m));
                    }
                });
                let r = async {
                    let cred = auth::oauth(kind, &mut auth::Io { say, paste }).await?;
                    auth.set(&provider, cred).await
                }
                .await;
                let _ = fwd.await;
                let _ = tx.send(LoginMsg::Done(r.map_err(|e| format!("{e:#}"))));
            }));
        }
        self.overlay = Some(Overlay::Login(v));
    }

    fn on_login(&mut self, m: LoginMsg) {
        let Some(Overlay::Login(v)) = &mut self.overlay else { return };
        match m {
            LoginMsg::Say(s) => {
                v.lines.retain(|l| l != "starting…");
                v.lines.push(s);
            }
            LoginMsg::Done(Ok(())) => {
                let t = v.title.clone();
                self.overlay = None;
                self.notify(format!("✓ logged in · {t}"));
                self.invalidate_all();
            }
            LoginMsg::Done(Err(e)) => {
                v.lines.push(format!("✗ {e}"));
                v.task = None;
            }
        }
    }

    fn open_tree(&mut self) {
        let s = self.app.session.lock().unwrap();
        let view = tree::TreeView::new(&s);
        drop(s);
        if view.rows.is_empty() {
            return self.notify("conversation is empty");
        }
        self.overlay = Some(Overlay::Tree(view));
    }

    fn picked(&mut self, purpose: Purpose, value: String) {
        match purpose {
            Purpose::Model(t) => {
                let v = value.clone();
                match t {
                    Target::Main => {
                        self.app.turn.model = value.clone();
                        self.update_settings("model", value.clone().into(), |s| s.model = v);
                        self.ctx_tokens = crate::compact::current_tokens(&self.app.session.lock().unwrap());
                    }
                    Target::Compaction => self.update_settings("models.compaction", value.clone().into(), |s| s.models.compaction = v),
                    Target::Title => self.update_settings("models.title", value.clone().into(), |s| s.models.title = v),
                    Target::Subagent => self.update_settings("models.subagent", value.clone().into(), |s| s.models.subagent = v),
                    Target::Agent(i) => self.set_agent_setting(i, "model", value.clone()),
                }
                self.notify(format!("model: {}", if value.is_empty() { "main model" } else { &value }));
                self.back_to_settings();
            }
            Purpose::Agent => {
                if let Some(a) = crate::agents::find(&self.app.rt.agents, &value).cloned() {
                    self.switch_agent(a);
                }
            }
            Purpose::Session => {
                self.interrupt();
                match Session::open(std::path::Path::new(&value)) {
                    Ok(s) => {
                        self.app.session = Arc::new(Mutex::new(s));
                        self.app.rt.read_cache.lock().unwrap().clear();
                        self.todos.clear();
                        self.rebuild();
                    }
                    Err(e) => self.notify(format!("cannot open session: {e}")),
                }
            }
            Purpose::Login => self.open_login_methods(&value),
            Purpose::LoginMethod => {
                if let Some((id, i)) = value.split_once('\t') {
                    self.start_login(id, i.parse().unwrap_or(0));
                }
            }
            Purpose::Logout => self.logout(&value),
            Purpose::Effort => self.set_effort(&value),
        }
    }

    fn switch_agent(&mut self, a: Agent) {
        let s = &self.app.rt.settings;
        if let Some(m) = agents::model_of(&a, s) {
            self.app.turn.model = m;
        } else if agents::model_of(&self.app.turn.agent, s).is_some() {
            self.app.turn.model = s.model.clone();
        }
        if let Some(e) = agents::effort_of(&a, s) {
            self.app.turn.effort = e;
        } else if agents::effort_of(&self.app.turn.agent, s).is_some() {
            self.app.turn.effort = s.effort.clone();
        }
        self.notify(format!("agent: {}", agent_label(&a)));
        theme::set_accent(&a.name);
        self.invalidate_all();
        self.app.turn.agent = a;
    }

    fn cycle_agent(&mut self) {
        let agents = self.app.rt.agents.clone();
        if agents.len() < 2 {
            return;
        }
        let i = agents.iter().position(|a| a.name == self.app.turn.agent.name).unwrap_or(0);
        self.switch_agent(agents[(i + 1) % agents.len()].clone());
    }

    // ---------- keys ----------

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        if self.overlay.is_some() {
            return self.overlay_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let cmds = self.cmd_matches();
        if matches!(k.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete) {
            self.cmd_sel = 0;
        }
        match k.code {
            KeyCode::Up | KeyCode::Down if !cmds.is_empty() && !k.modifiers.contains(KeyModifiers::SHIFT) => {
                let n = cmds.len();
                self.cmd_sel = if k.code == KeyCode::Up { (self.cmd_sel + n - 1) % n } else { (self.cmd_sel + 1) % n };
            }
            KeyCode::Char('c') if ctrl => {
                if !self.input.text.is_empty() {
                    self.input.take();
                } else if self.run.is_some() {
                    self.interrupt();
                } else if self.quit_armed.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    self.quit = true;
                } else {
                    self.quit_armed = Some(Instant::now());
                    self.notify("press Ctrl+C again to quit");
                }
            }
            KeyCode::Char('d') if ctrl && self.input.text.is_empty() => self.quit = true,
            KeyCode::Esc => {
                if self.run.is_some() {
                    self.interrupt();
                } else {
                    self.scroll_top = None;
                }
            }
            KeyCode::Char('o') if ctrl => self.toggle_verbose(),
            KeyCode::Char('t') if ctrl => self.open_tree(),
            KeyCode::Char('p') if ctrl => self.open_models(Target::Main),
            KeyCode::Char('r') if ctrl => self.open_sessions(),
            KeyCode::Char('l') if ctrl => self.invalidate_all(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Char('u') if ctrl => {
                let end = self.input.cur;
                self.input.home();
                let start = self.input.cur;
                self.input.text.replace_range(start..end, "");
            }
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => self.input.insert("\n"),
            KeyCode::Enter => {
                let text = self.input.text.trim().to_string();
                if text.is_empty() {
                    return;
                }
                if text.starts_with('/') && !text.contains('\n') {
                    // Run the highlighted suggestion, or complete a partial command.
                    let first = text.split(' ').next().unwrap_or("");
                    let cmd = cmds.get(self.cmd_sel).copied().or_else(|| COMMANDS.iter().find(|(c, _)| *c == first)).or_else(|| COMMANDS.iter().find(|(c, _)| c.starts_with(first)));
                    self.cmd_sel = 0;
                    self.input.take();
                    match cmd {
                        Some((c, _)) => {
                            let rest = text[first.len()..].trim();
                            self.command(format!("{c} {rest}").trim());
                        }
                        None => self.notify(format!("unknown command {first}")),
                    }
                    return;
                }
                self.input.take();
                self.send(text);
            }
            KeyCode::Tab => {
                if let Some((c, _)) = cmds.get(self.cmd_sel) {
                    self.input.set(format!("{c} "));
                    self.cmd_sel = 0;
                } else if self.input.text.starts_with('/') {
                } else {
                    self.cycle_agent();
                }
            }
            KeyCode::BackTab => {
                let order = ["low", "medium", "high", "xhigh", "max"];
                let i = order.iter().position(|e| *e == self.app.turn.effort).unwrap_or(1);
                let e = order[(i + 1) % order.len()];
                self.set_effort(e);
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::PageUp => self.scroll(-(self.last_view as i64 / 2).max(3)),
            KeyCode::PageDown => self.scroll((self.last_view as i64 / 2).max(3)),
            KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => self.scroll(-3),
            KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => self.scroll(3),
            KeyCode::Up => {
                if self.input.text.contains('\n') {
                    return self.move_line(-1);
                }
                if self.history.is_empty() {
                    return;
                }
                let i = match self.hist {
                    None => self.history.len() - 1,
                    Some(i) => i.saturating_sub(1),
                };
                self.hist = Some(i);
                self.input.set(self.history[i].clone());
            }
            KeyCode::Down => {
                if self.input.text.contains('\n') {
                    return self.move_line(1);
                }
                match self.hist {
                    Some(i) if i + 1 < self.history.len() => {
                        self.hist = Some(i + 1);
                        self.input.set(self.history[i + 1].clone());
                    }
                    Some(_) => {
                        self.hist = None;
                        self.input.take();
                    }
                    None => {}
                }
            }
            KeyCode::Char(c) => {
                let mut b = [0u8; 4];
                self.input.insert(c.encode_utf8(&mut b));
            }
            _ => {}
        }
    }

    /// Commands matching a `/partial` input (empty when not typing a command).
    fn cmd_matches(&self) -> Vec<&'static (&'static str, &'static str)> {
        let t = self.input.text.as_str();
        if !t.starts_with('/') || t.contains([' ', '\n']) {
            return vec![];
        }
        COMMANDS.iter().filter(|(c, _)| c.starts_with(t)).collect()
    }

    fn move_line(&mut self, dir: i32) {
        let before = &self.input.text[..self.input.cur];
        let col = before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0);
        if dir < 0 {
            let Some(prev_end) = before.rfind('\n') else { return };
            let prev_start = self.input.text[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
            self.input.cur = self.input.text.floor_char_boundary((prev_start + col).min(prev_end));
        } else {
            let Some(off) = self.input.text[self.input.cur..].find('\n') else { return };
            let next_start = self.input.cur + off + 1;
            let next_end = self.input.text[next_start..].find('\n').map(|i| next_start + i).unwrap_or(self.input.text.len());
            self.input.cur = self.input.text.floor_char_boundary((next_start + col).min(next_end));
        }
    }

    fn scroll(&mut self, delta: i64) {
        let max_top = self.last_total.saturating_sub(self.last_view);
        let top = self.scroll_top.unwrap_or(max_top) as i64 + delta;
        self.scroll_top = if top >= max_top as i64 { None } else { Some(top.max(0) as usize) };
    }

    fn overlay_key(&mut self, k: KeyEvent) {
        let Some(ov) = self.overlay.take() else { return };
        match ov {
            Overlay::Help => {}
            Overlay::Ask(mut a) => {
                let free = a.options.len();
                match k.code {
                    KeyCode::Esc => return a.answer(String::new()),
                    KeyCode::Up => a.sel = a.sel.saturating_sub(1),
                    KeyCode::Down => a.sel = (a.sel + 1).min(free),
                    KeyCode::Enter if a.sel < free => {
                        let o = a.options[a.sel].clone();
                        return a.answer(o);
                    }
                    KeyCode::Enter => {
                        let t = a.input.text.trim().to_string();
                        if !t.is_empty() {
                            return a.answer(t);
                        }
                    }
                    KeyCode::Char(c @ '1'..='9') if a.sel < free && (c as usize - '1' as usize) < free => {
                        let o = a.options[c as usize - '1' as usize].clone();
                        return a.answer(o);
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        a.sel = free;
                        let mut b = [0u8; 4];
                        a.input.insert(c.encode_utf8(&mut b));
                    }
                    KeyCode::Backspace => a.input.backspace(),
                    KeyCode::Left => a.input.left(),
                    KeyCode::Right => a.input.right(),
                    _ => {}
                }
                self.overlay = Some(Overlay::Ask(a));
            }
            Overlay::Login(mut v) => {
                match k.code {
                    KeyCode::Esc => {
                        if let Some(t) = v.task.take() {
                            t.abort();
                        }
                        return self.notify("login cancelled");
                    }
                    KeyCode::Enter => {
                        let t = v.input.take().trim().to_string();
                        if t.is_empty() {
                        } else if v.key {
                            let (auth, id, tx) = (self.app.rt.auth.clone(), v.provider.clone(), self.login_tx.clone());
                            tokio::spawn(async move {
                                let r = auth.set(&id, crate::auth::Cred::ApiKey { key: t }).await;
                                let _ = tx.send(LoginMsg::Done(r.map_err(|e| format!("{e:#}"))));
                            });
                        } else if let Some(p) = &v.paste {
                            let _ = p.send(t);
                            v.lines.push("checking code…".into());
                        }
                    }
                    KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        let url = v.lines.iter().flat_map(|l| l.split_whitespace()).find(|w| w.starts_with("https://")).map(String::from);
                        if let Some(u) = url {
                            self.notify(if clipboard(&u) { "URL copied" } else { "no clipboard tool found" });
                        }
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        let mut b = [0u8; 4];
                        v.input.insert(c.encode_utf8(&mut b));
                    }
                    KeyCode::Backspace => v.input.backspace(),
                    KeyCode::Left => v.input.left(),
                    KeyCode::Right => v.input.right(),
                    _ => {}
                }
                self.overlay = Some(Overlay::Login(v));
            }
            Overlay::Pick(mut p) => {
                let vis = p.visible();
                match k.code {
                    KeyCode::Esc => {
                        if matches!(p.purpose, Purpose::Model(_)) {
                            self.back_to_settings();
                        }
                        return;
                    }
                    KeyCode::Up => p.sel = p.sel.saturating_sub(1),
                    KeyCode::Down => p.sel = (p.sel + 1).min(vis.len().saturating_sub(1)),
                    KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
                    KeyCode::PageDown => p.sel = (p.sel + 10).min(vis.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        if let Some(&i) = vis.get(p.sel) {
                            let v = p.items[i].value.clone();
                            return self.picked(p.purpose, v);
                        }
                    }
                    KeyCode::Backspace => {
                        p.filter.pop();
                        p.sel = 0;
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        p.filter.push(c);
                        p.sel = 0;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Pick(p));
            }
            Overlay::Tree(mut t) => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Up | KeyCode::Char('k') => {
                    t.sel = t.sel.saturating_sub(1);
                    self.overlay = Some(Overlay::Tree(t));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    t.sel = (t.sel + 1).min(t.rows.len().saturating_sub(1));
                    self.overlay = Some(Overlay::Tree(t));
                }
                KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char('f') => {
                    if self.run.is_some() {
                        self.notify("interrupt the agent first (Esc)");
                        self.overlay = Some(Overlay::Tree(t));
                        return;
                    }
                    let Some(row) = t.rows.get(t.sel) else { return };
                    let edit = k.code != KeyCode::Enter;
                    let (leaf, text) = {
                        let s = self.app.session.lock().unwrap();
                        if edit { (row.parent.clone(), Some(row.text.clone())) } else { (tree::turn_end(&s, &row.id), None) }
                    };
                    let _ = self.app.session.lock().unwrap().set_leaf(leaf);
                    self.rebuild();
                    if let Some(text) = text {
                        self.input.set(text);
                        self.notify("edit and press Enter to create a new branch");
                    } else {
                        self.notify("switched branch — your next message continues from here");
                    }
                }
                _ => self.overlay = Some(Overlay::Tree(t)),
            },
            Overlay::Settings(v) => {
                let rows = self.settings_rows(v.tab).len();
                let other = match v.tab {
                    SettingsTab::General => SettingsTab::Models,
                    SettingsTab::Models => SettingsTab::General,
                };
                match k.code {
                    KeyCode::Esc => (),
                    KeyCode::Tab | KeyCode::BackTab => self.overlay = Some(Overlay::Settings(SettingsView { tab: other, sel: 0 })),
                    KeyCode::Up => self.overlay = Some(Overlay::Settings(SettingsView { sel: v.sel.saturating_sub(1), ..v })),
                    KeyCode::Down => self.overlay = Some(Overlay::Settings(SettingsView { sel: (v.sel + 1).min(rows - 1), ..v })),
                    KeyCode::Enter | KeyCode::Right | KeyCode::Left | KeyCode::Char(' ') => {
                        let back = k.code == KeyCode::Left;
                        self.overlay = Some(Overlay::Settings(v));
                        self.settings_action(v, back);
                    }
                    _ => self.overlay = Some(Overlay::Settings(v)),
                }
            }
        }
    }

    /// (label, value) rows of one settings tab; row index = `SettingsView::sel`.
    fn settings_rows(&self, tab: SettingsTab) -> Vec<(String, String)> {
        let s = &self.app.rt.settings;
        let or_default = |m: &str| if m.is_empty() { "(default)".to_string() } else { m.to_string() };
        let or_main = |m: &str| if m.is_empty() { "(main model)".to_string() } else { m.to_string() };
        match tab {
            SettingsTab::General => vec![
                ("Output".into(), if s.verbose == "compact" { "compact — final answer + progress line".into() } else { "full — show tools and thinking".into() }),
                ("Auto-compaction".into(), if s.compaction.enabled { "on".into() } else { "off".into() }),
                ("Compact at".into(), format!("{:.0}% of context window", s.compaction.threshold * 100.0)),
                ("Web search".into(), s.web.backend.clone()),
                ("rtk filters".into(), s.tools.rtk.clone()),
            ],
            SettingsTab::Models => {
                let mut rows = vec![
                    ("Main model".into(), self.app.turn.model.clone()),
                    ("Reasoning effort".into(), self.app.turn.effort.clone()),
                    ("Compaction model".into(), or_main(&s.models.compaction)),
                    ("Session-title model".into(), or_main(&s.models.title)),
                    ("Subagent model".into(), or_main(&s.models.subagent)),
                ];
                for a in self.app.rt.agents.iter() {
                    let cfg = s.agent(&a.name).cloned().unwrap_or_default();
                    rows.push((format!("{} model", a.name), or_default(&cfg.model)));
                    rows.push((format!("{} effort", a.name), or_default(&cfg.effort)));
                }
                rows
            }
        }
    }

    fn settings_action(&mut self, view: SettingsView, back: bool) {
        let cycle = |list: &[&str], cur: &str| -> String {
            let i = list.iter().position(|x| *x == cur).unwrap_or(0);
            let j = if back { (i + list.len() - 1) % list.len() } else { (i + 1) % list.len() };
            list[j].to_string()
        };
        let s = self.app.rt.settings.clone();
        if view.tab == SettingsTab::Models {
            let n = view.sel;
            let agent_row = n.checked_sub(5).map(|r| (r / 2, r % 2 == 0));
            match (n, agent_row) {
                (1, _) => {
                    let e = cycle(&["low", "medium", "high", "xhigh", "max"], &self.app.turn.effort);
                    self.set_effort(&e);
                }
                (0, _) | (2..=4, _) | (_, Some((_, true))) => {
                    self.settings_back = Some(view);
                    match n {
                        0 => self.open_models(Target::Main),
                        2 => self.open_models(Target::Compaction),
                        3 => self.open_models(Target::Title),
                        4 => self.open_models(Target::Subagent),
                        _ => self.open_models(Target::Agent((n - 5) / 2)),
                    }
                }
                (_, Some((i, false))) => {
                    let Some(a) = self.app.rt.agents.get(i).cloned() else { return };
                    let cur = s.agent(&a.name).map(|c| c.effort.clone()).unwrap_or_default();
                    let e = cycle(&["", "low", "medium", "high", "xhigh", "max"], &cur);
                    self.set_agent_setting(i, "effort", e);
                }
                _ => {}
            }
            return;
        }
        match view.sel {
            0 => self.toggle_verbose(),
            1 => {
                let v = !s.compaction.enabled;
                self.update_settings("compaction.enabled", v.into(), |s| s.compaction.enabled = v);
            }
            2 => {
                let mut v = s.compaction.threshold + if back { -0.05 } else { 0.05 };
                if v > 0.951 {
                    v = 0.5;
                } else if v < 0.499 {
                    v = 0.95;
                }
                let v = (v * 100.0).round() / 100.0;
                self.update_settings("compaction.threshold", v.into(), |s| s.compaction.threshold = v);
            }
            3 => {
                let v = cycle(&["exa", "firecrawl", "brave", "tavily", "duckduckgo"], &s.web.backend);
                self.update_settings("web.backend", v.clone().into(), |s| s.web.backend = v);
            }
            4 => {
                let v = cycle(&["auto", "off"], &s.tools.rtk);
                self.update_settings("tools.rtk", v.clone().into(), |s| s.tools.rtk = v);
            }
            _ => {}
        }
    }

    // ---------- drawing ----------

    fn item_lines(&self, i: usize, width: usize, full: bool) -> Vec<Line<'static>> {
        let it = &self.items[i];
        let running_turn = self.run.is_some() && !self.items[i + 1..].iter().any(|x| matches!(x, Item::User(_)));
        match it {
            Item::User(t) => {
                let mut out = vec![Line::default()];
                let spans = vec![Span::styled(t.clone(), theme::user())];
                out.extend(markdown::wrap(spans, width, vec![Span::styled("› ", theme::accent().add_modifier(Modifier::BOLD))], vec![Span::raw("  ")]));
                out
            }
            Item::Assistant { text, thinking, done } => {
                // Compact mode: only the final answer of a finished turn.
                let is_final = !self.items[i + 1..].iter().take_while(|x| !matches!(x, Item::User(_))).any(|x| matches!(x, Item::Assistant { .. } | Item::Tool { .. }));
                if !full && (!is_final || running_turn) {
                    return vec![];
                }
                let mut out = vec![Line::default()];
                if full && !thinking.trim().is_empty() {
                    let st = theme::dim().add_modifier(Modifier::ITALIC);
                    if *done {
                        let words = thinking.split_whitespace().count();
                        let first: String = thinking.trim().lines().next().unwrap_or("").chars().take(width.saturating_sub(20)).collect();
                        out.push(Line::from(vec![Span::styled("∴ ", theme::accent()), Span::styled(format!("{first}  ({words} words)"), st)]));
                    } else {
                        let lines: Vec<&str> = thinking.trim().lines().filter(|l| !l.trim().is_empty()).collect();
                        for l in &lines[lines.len().saturating_sub(4)..] {
                            out.extend(markdown::wrap(vec![Span::styled(l.to_string(), st)], width, vec![Span::styled("∴ ", theme::accent())], vec![Span::raw("  ")]));
                        }
                    }
                }
                if !text.trim().is_empty() {
                    if full && !thinking.trim().is_empty() {
                        out.push(Line::default());
                    }
                    out.extend(markdown::render(text, width));
                }
                if out.len() == 1 { vec![] } else { out }
            }
            Item::Tool { name, summary, state, out, display, sub, .. } => {
                if !full {
                    return vec![];
                }
                let (icon, st) = match state {
                    ToolState::Running => (SPIN[self.spin % SPIN.len()], theme::accent()),
                    ToolState::Done { ok: true } => ("✓", theme::ok()),
                    ToolState::Done { ok: false } => ("✗", theme::err()),
                };
                let mut head = vec![Span::styled(format!("  {icon} "), st), Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)), Span::raw(" ")];
                let mut lines = markdown::wrap(vec![Span::styled(summary.clone(), theme::dim())], width, std::mem::take(&mut head), vec![Span::raw("      ")]);
                if lines.len() > 2 {
                    lines.truncate(2);
                }
                if name == "task" && (sub.read + sub.write + sub.cmd + sub.tools) > 0 {
                    let mut l = vec![Span::styled("    ⎿ ", theme::dim())];
                    l.extend(sub.spans());
                    lines.push(Line::from(l));
                }
                let gutter = |s: &str, style: Style| Line::from(vec![Span::styled("    ⎿ ", theme::dim()), Span::styled(s.to_string(), style)]);
                let clip = |s: &str| -> String { s.chars().take(width.saturating_sub(8)).collect() };
                if let Some(d) = display {
                    let dl: Vec<&str> = d.lines().collect();
                    for l in dl.iter().take(14) {
                        let style = if l.starts_with('+') { theme::ok() } else if l.starts_with('-') { theme::err() } else { theme::dim() };
                        lines.push(gutter(&clip(l), style));
                    }
                    if dl.len() > 14 {
                        lines.push(gutter(&format!("… {} more lines", dl.len() - 14), theme::dim()));
                    }
                } else if matches!(state, ToolState::Done { ok: false }) {
                    for l in out.lines().take(4) {
                        lines.push(gutter(&clip(l), theme::err()));
                    }
                } else if matches!(state, ToolState::Done { .. }) {
                    let n = out.lines().count();
                    match name.as_str() {
                        "bash" => {
                            let ls: Vec<&str> = out.lines().collect();
                            for l in &ls[ls.len().saturating_sub(3)..] {
                                lines.push(gutter(&clip(l), theme::dim()));
                            }
                        }
                        "todo" => {}
                        "write" | "edit" | "ask" | "handoff" => lines.push(gutter(&clip(out), theme::dim())),
                        _ => lines.push(gutter(&format!("{n} lines"), theme::dim())),
                    }
                }
                lines
            }
            Item::Info(t) => vec![Line::from(Span::styled(format!("  · {t}"), theme::dim()))],
            Item::Error(t) => {
                let mut out = vec![Line::default()];
                out.extend(markdown::wrap(vec![Span::styled(t.clone(), theme::err())], width, vec![Span::styled("! ", theme::err())], vec![Span::raw("  ")]));
                out
            }
        }
    }

    fn chat_lines(&mut self, width: usize) -> Vec<Line<'static>> {
        let full = self.verbose();
        let mut all = Vec::new();
        for i in 0..self.items.len() {
            let running_tool = matches!(self.items[i], Item::Tool { state: ToolState::Running, .. });
            let reusable = !self.dirty[i] && !running_tool && self.run.is_none();
            let lines = match &self.cache[i] {
                Some((w, f, l)) if reusable && *w == width && *f == full => l.clone(),
                _ => {
                    let l = self.item_lines(i, width, full);
                    self.cache[i] = Some((width, full, l.clone()));
                    self.dirty[i] = false;
                    l
                }
            };
            all.extend(lines);
        }
        all
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let width = area.width as usize;
        let inner_w = width.saturating_sub(2);
        let (input_lines, cursor) = self.input.layout(inner_w.saturating_sub(2));
        let input_h = (input_lines.len().min(8) + 2) as u16;
        let todo_h = if self.todos.is_empty() || self.todos.iter().all(|t| t.status == "done") && self.run.is_none() { 0 } else { (self.todos.len().min(8) + 1) as u16 };
        let show_progress = self.run.is_some() || self.counts.read + self.counts.write + self.counts.cmd + self.counts.tools > 0;
        let [chat, todo_area, progress, input_area, status] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(todo_h),
            Constraint::Length(if show_progress { 1 } else { 0 }),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .areas(area);

        // chat
        let chat_inner = Rect { x: chat.x + 1, width: chat.width.saturating_sub(2), ..chat };
        let lines = if self.items.is_empty() { self.splash(chat_inner.height as usize) } else { self.chat_lines(chat_inner.width as usize) };
        let total = lines.len();
        let view = chat_inner.height as usize;
        self.last_total = total;
        self.last_view = view;
        let max_top = total.saturating_sub(view);
        let top = self.scroll_top.map(|t| t.min(max_top)).unwrap_or(max_top);
        let visible: Vec<Line> = lines.into_iter().skip(top).take(view).collect();
        f.render_widget(Paragraph::new(visible), chat_inner);
        if self.scroll_top.is_some() {
            let tag = format!(" ↓ {} more ", total - top - view.min(total - top));
            let w = tag.width() as u16;
            f.render_widget(Paragraph::new(Span::styled(tag, theme::sel())), Rect { x: chat.right().saturating_sub(w + 1), y: chat.bottom().saturating_sub(1), width: w, height: 1 });
        }

        // todos
        if todo_h > 0 {
            let mut tl = vec![Line::from(Span::styled(" Tasks", theme::dim().add_modifier(Modifier::BOLD)))];
            for t in self.todos.iter().take(8) {
                let (mark, st) = match t.status.as_str() {
                    "done" => ("✓", theme::dim().add_modifier(Modifier::CROSSED_OUT)),
                    "in_progress" => ("▸", theme::accent().add_modifier(Modifier::BOLD)),
                    _ => ("○", Style::default()),
                };
                tl.push(Line::from(vec![Span::styled(format!(" {mark} "), st), Span::styled(t.content.clone(), st)]));
            }
            f.render_widget(Paragraph::new(tl), todo_area);
        }

        // progress line
        if show_progress {
            let mut spans = Vec::new();
            let secs = if self.run.is_some() { self.started.elapsed() } else { self.elapsed }.as_secs();
            let time = if secs >= 60 { format!("{}m{:02}s", secs / 60, secs % 60) } else { format!("{secs}s") };
            if self.run.is_some() {
                // ✻ Pondering… (12s · ↓ 1.2k tokens · esc to interrupt)
                spans.push(Span::styled(format!(" {} ", PULSE[self.spin % PULSE.len()]), theme::accent()));
                let verb = if self.status.starts_with("compacting") { "Compacting" } else if self.status.starts_with("waiting") { "Waiting for you" } else { self.verb };
                let n = verb.chars().count();
                let hi = self.spin % (n + 8);
                for (i, c) in verb.chars().chain("…".chars()).enumerate() {
                    let st = if i + 1 >= hi && i <= hi + 1 { Style::default().add_modifier(Modifier::BOLD) } else { theme::accent() };
                    spans.push(Span::styled(c.to_string(), st));
                }
                let tok = self.out_tokens + self.stream_chars as u64 / 4;
                let tok = if tok >= 1000 { format!("{:.1}k", tok as f64 / 1000.0) } else { tok.to_string() };
                spans.push(Span::styled(format!(" ({time} · ↓ {tok} tokens · esc to interrupt)  "), theme::dim()));
                spans.extend(self.counts.spans());
                if !matches!(self.status.as_str(), "thinking" | "writing") && !self.status.starts_with("compacting") && !self.status.starts_with("waiting") {
                    let st: String = self.status.chars().take(width / 3).collect();
                    spans.push(Span::styled(format!("  ⎿ {st}"), theme::dim()));
                }
            } else {
                spans.push(Span::styled(" ✓ done  ", theme::ok()));
                spans.extend(self.counts.spans());
                spans.push(Span::styled(format!("  · {time}"), theme::dim()));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), progress);
        }

        // input
        let border = if self.run.is_some() { theme::dim() } else { theme::accent() };
        let block = UiBlock::bordered().border_type(BorderType::Rounded).border_style(border);
        let inner = block.inner(input_area);
        f.render_widget(block, input_area);
        let skip = cursor.0.saturating_sub(7);
        let mut il: Vec<Line> = Vec::new();
        for (n, l) in input_lines.iter().enumerate().skip(skip).take(8) {
            let prefix = if n == 0 { Span::styled("› ", theme::accent()) } else { Span::raw("  ") };
            il.push(Line::from(vec![prefix, Span::raw(l.clone())]));
        }
        if self.input.text.is_empty() {
            il = vec![Line::from(vec![Span::styled("› ", theme::accent()), Span::styled("Ask anything — / for commands, tab to switch agent", theme::dim())])];
        }
        f.render_widget(Paragraph::new(il), inner);
        if self.overlay.is_none() {
            f.set_cursor_position((inner.x + 2 + cursor.1 as u16, inner.y + (cursor.0 - skip) as u16));
        }

        // slash-command hints
        let m = self.cmd_matches();
        if self.overlay.is_none() && !m.is_empty() {
            {
                let sel = self.cmd_sel.min(m.len() - 1);
                let max = 10.min(input_area.y.saturating_sub(2) as usize).max(1);
                let start = sel.saturating_sub(max - 1);
                let h = m.len().min(max) as u16 + 2;
                let r = Rect { x: input_area.x + 1, y: input_area.y.saturating_sub(h), width: 60.min(area.width.saturating_sub(2)), height: h };
                f.render_widget(Clear, r);
                let lines: Vec<Line> = m.iter().enumerate().skip(start).take(max).map(|(i, (c, d))| {
                    let st = if i == sel { theme::sel() } else { Style::default() };
                    Line::from(vec![Span::styled(format!(" {c:<10}"), st.patch(theme::accent())), Span::styled(format!(" {d}"), st.patch(theme::dim()))])
                }).collect();
                f.render_widget(Paragraph::new(lines).block(UiBlock::bordered().border_type(BorderType::Rounded).border_style(theme::dim())), r);
            }
        }

        // status bar
        let a = &self.app.turn.agent;
        let mut left = vec![
            Span::styled(" θ ", theme::logo()),
            Span::styled(format!(" {} ", a.name), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!("({})", a.scope.label()), theme::dim()),
            Span::styled("  ", theme::dim()),
            Span::styled(self.app.turn.model.clone(), theme::accent()),
            Span::styled(format!(" · {}", self.app.turn.effort), theme::dim()),
        ];
        if !self.verbose() {
            left.push(Span::styled(" · compact", theme::dim()));
        }
        let ctx_max = self.model_ctx();
        let pct = if ctx_max > 0 { self.ctx_tokens * 100 / ctx_max } else { 0 };
        let title = self.app.session.lock().unwrap().title.clone().unwrap_or_default();
        let right = match &self.notice {
            Some((n, t)) if t.elapsed() < Duration::from_secs(4) => vec![Span::styled(format!("{n} "), theme::accent())],
            _ => vec![
                Span::styled(format!("{} ", title.chars().take(30).collect::<String>()), theme::dim()),
                Span::styled(format!("{}k/{}k {}% ", self.ctx_tokens / 1000, ctx_max / 1000, pct), if pct >= 80 { theme::err() } else { theme::dim() }),
                Span::styled(format!("${:.3} ", self.cost), theme::dim()),
            ],
        };
        let lw: usize = left.iter().map(|s| s.content.width()).sum();
        let rw: usize = right.iter().map(|s| s.content.width()).sum();
        let mut bar = left;
        if lw + rw < width {
            bar.push(Span::raw(" ".repeat(width - lw - rw)));
            bar.extend(right);
        }
        f.render_widget(Paragraph::new(Line::from(bar)), status);

        // overlays
        if let Some(ov) = &self.overlay {
            self.draw_overlay(f, ov, area);
        }
    }

    fn splash(&self, h: usize) -> Vec<Line<'static>> {
        let a = &self.app.turn.agent;
        let mut out = vec![Line::default(); h.saturating_sub(9) / 2];
        out.push(Line::from(vec![Span::styled("  θ", theme::accent().add_modifier(Modifier::BOLD)), Span::styled("  theta", Style::default().add_modifier(Modifier::BOLD))]));
        out.push(Line::default());
        out.push(Line::from(vec![Span::styled("  agent  ", theme::dim()), Span::raw(agent_label(a))]));
        out.push(Line::from(vec![Span::styled("  model  ", theme::dim()), Span::raw(self.app.turn.model.clone())]));
        out.push(Line::from(vec![Span::styled("  cwd    ", theme::dim()), Span::raw(self.app.rt.cwd.display().to_string())]));
        if let Some((p, _)) = self.app.rt.catalog.resolve(&self.app.turn.model) {
            let local = p.env.is_empty() && p.oauth.is_none();
            if !local && futures::executor::block_on(self.app.rt.auth.status(&p)).is_none() {
                out.push(Line::default());
                out.push(Line::from(vec![Span::styled("  ! ", theme::err()), Span::raw(format!("no credentials for {} — run ", p.name)), Span::styled("/login", theme::accent()), Span::raw(" or pick another model with "), Span::styled("ctrl+p", theme::accent())]));
            }
        }
        out.push(Line::default());
        out.push(Line::from(Span::styled("  /help  ·  tab agent  ·  ctrl+p model  ·  ctrl+t tree  ·  ctrl+o compact output", theme::dim())));
        out
    }

    fn draw_overlay(&self, f: &mut Frame, ov: &Overlay, area: Rect) {
        let w = (area.width.saturating_sub(4)).min(100);
        let iw = w.saturating_sub(4) as usize;
        let content = match ov {
            Overlay::Help => COMMANDS.len() + 13,
            Overlay::Pick(p) => p.rows().max(1) + 1,
            Overlay::Tree(t) => t.rows.len(),
            Overlay::Settings(v) => self.settings_rows(v.tab).len() + 3,
            Overlay::Ask(a) => wrapped_rows(&a.question, iw) + a.options.len() + 3,
            Overlay::Login(v) => v.lines.iter().map(|l| wrapped_rows(l, iw) + 1).sum::<usize>() + 2,
        } as u16;
        let min_h = if matches!(ov, Overlay::Pick(_)) { 12 } else { 5 };
        let h = area.height.saturating_sub(4).min(content.max(min_h) + 2);
        let r = Rect { x: (area.width - w) / 2, y: (area.height.saturating_sub(h)) / 3, width: w, height: h };
        f.render_widget(Clear, r);
        let block = |t: &str| UiBlock::bordered().border_type(BorderType::Rounded).border_style(theme::accent()).title(Span::styled(format!(" {t} "), Style::default().add_modifier(Modifier::BOLD)));
        match ov {
            Overlay::Ask(a) => {
                let mut l = vec![Line::from(Span::styled(a.question.clone(), Style::default().add_modifier(Modifier::BOLD))), Line::default()];
                for (i, o) in a.options.iter().enumerate() {
                    let st = if i == a.sel { theme::sel() } else { Style::default() };
                    l.push(Line::from(vec![Span::styled(format!(" {} ", if i == a.sel { "›" } else { " " }), theme::accent()), Span::styled(format!("{}. {o}", i + 1), st)]));
                }
                let on_free = a.sel == a.options.len();
                let text = if a.input.text.is_empty() { Span::styled(if a.options.is_empty() { "type your answer" } else { "or type your own answer" }, theme::dim()) } else { Span::raw(a.input.text.clone()) };
                l.push(Line::from(vec![Span::styled(format!(" {} ✎ ", if on_free { "›" } else { " " }), theme::accent()), text.patch_style(if on_free { theme::sel() } else { Style::default() })]));
                let b = block("θ question").title_bottom(Line::from(Span::styled(" ↑↓ select · enter answer · 1-9 pick · esc skip ", theme::dim())));
                f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(b), r);
                if on_free {
                    let row = wrapped_rows(&a.question, iw) + 1 + a.options.len();
                    let col = 5 + a.input.text[..a.input.cur].width();
                    f.set_cursor_position((r.x + 1 + col as u16, r.y + 1 + row as u16));
                }
            }
            Overlay::Login(v) => {
                let mut l = Vec::new();
                for line in &v.lines {
                    let st = if line.starts_with('✗') { theme::err() } else { Style::default() };
                    l.extend(line.lines().map(|x| Line::from(Span::styled(x.to_string(), if x.starts_with("https://") { theme::link() } else { st }))));
                    l.push(Line::default());
                }
                let (label, shown) = if v.key { ("API key › ", "•".repeat(v.input.text.chars().count())) } else { ("paste code › ", v.input.text.clone()) };
                l.push(Line::from(vec![Span::styled(label, theme::accent()), Span::raw(shown.clone())]));
                let hint = if v.key { " enter save · esc cancel " } else { " waiting for the browser · enter submit pasted code · ctrl+y copy URL · esc cancel " };
                let b = block(&v.title).title_bottom(Line::from(Span::styled(hint, theme::dim())));
                f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(b), r);
                let row: usize = v.lines.iter().map(|x| wrapped_rows(x, iw) + 1).sum();
                f.set_cursor_position((r.x + 1 + (label.width() + shown.width()) as u16, r.y + 1 + row as u16));
            }
            Overlay::Help => {
                let mut l = vec![Line::from(Span::styled("Keys", theme::accent().add_modifier(Modifier::BOLD)))];
                for (k, d) in [
                    ("enter", "send · alt+enter / ctrl+j newline"),
                    ("esc", "interrupt the agent"),
                    ("tab / shift+tab", "next agent / cycle effort"),
                    ("ctrl+p", "model"),
                    ("ctrl+t", "conversation tree (fork)"),
                    ("ctrl+r", "resume a session"),
                    ("ctrl+o", "full / compact output"),
                    ("pgup pgdn shift+↑↓ wheel", "scroll"),
                    ("ctrl+c ×2 / ctrl+d", "quit"),
                ] {
                    l.push(Line::from(vec![Span::styled(format!("  {k:<26}"), theme::accent()), Span::raw(d)]));
                }
                l.push(Line::default());
                l.push(Line::from(Span::styled("Commands", theme::accent().add_modifier(Modifier::BOLD))));
                for (c, d) in COMMANDS {
                    l.push(Line::from(vec![Span::styled(format!("  {c:<26}"), theme::accent()), Span::raw(*d)]));
                }
                f.render_widget(Paragraph::new(l).block(block("θ help")), r);
            }
            Overlay::Pick(p) => {
                let vis = p.visible();
                let rows = h.saturating_sub(4) as usize;
                let mut l = vec![Line::from(vec![Span::styled("filter › ", theme::dim()), Span::raw(p.filter.clone()), Span::styled("▏", theme::accent())])];
                let mut body = Vec::new();
                let mut sel_row = 0;
                for (n, &i) in vis.iter().enumerate() {
                    let it = &p.items[i];
                    if !it.group.is_empty() && (n == 0 || p.items[vis[n - 1]].group != it.group) {
                        body.push(Line::from(Span::styled(it.group.clone(), theme::accent().add_modifier(Modifier::BOLD))));
                    }
                    if n == p.sel {
                        sel_row = body.len();
                    }
                    let st = if n == p.sel { theme::sel() } else { Style::default() };
                    let indent = if it.group.is_empty() { " " } else { "   " };
                    let label: String = it.label.chars().take(iw * 3 / 5).collect();
                    let pad = (iw * 3 / 5).saturating_sub(label.width() + indent.len() - 1) + 1;
                    let detail: String = it.detail.chars().take(iw.saturating_sub(label.width() + pad + 4)).collect();
                    body.push(Line::from(vec![Span::styled(format!("{indent}{label}{}", " ".repeat(pad)), st), Span::styled(detail, st.patch(theme::dim()))]));
                }
                let start = sel_row.saturating_sub(rows.saturating_sub(1));
                l.extend(body.into_iter().skip(start).take(rows));
                if vis.is_empty() {
                    l.push(Line::from(Span::styled("  no match", theme::dim())));
                }
                f.render_widget(Paragraph::new(l).block(block(&p.title)), r);
            }
            Overlay::Tree(t) => {
                let rows = h.saturating_sub(4) as usize;
                let start = t.sel.saturating_sub(rows.saturating_sub(1));
                let mut l = Vec::new();
                for (n, row) in t.rows.iter().enumerate().skip(start).take(rows) {
                    let st = if n == t.sel { theme::sel() } else { Style::default() };
                    let (mark, mst) = if row.current { ("◉ ", theme::accent()) } else if row.on_path { ("● ", theme::accent()) } else { ("○ ", theme::dim()) };
                    let text_st = if row.on_path { st } else { st.patch(theme::dim()) };
                    let budget = iw.saturating_sub(row.prefix.width() + 4);
                    let mut text: String = row.text.lines().next().unwrap_or("").chars().take(budget).collect();
                    let reply_budget = budget.saturating_sub(text.width() + 4);
                    let reply: String = if reply_budget > 8 && !row.reply.is_empty() { format!("  → {}", row.reply.chars().take(reply_budget).collect::<String>()) } else { String::new() };
                    if row.compaction {
                        text = format!("⊟ {text}");
                    }
                    l.push(Line::from(vec![
                        Span::styled(row.prefix.clone(), theme::dim()),
                        Span::styled(mark, mst.patch(if n == t.sel { theme::sel() } else { Style::default() })),
                        Span::styled(text, text_st),
                        Span::styled(reply, st.patch(theme::dim())),
                    ]));
                }
                let mut b = block("Conversation tree");
                b = b.title_bottom(Line::from(Span::styled(" ↑↓ move · enter continue from here · e edit & branch · esc close ", theme::dim())));
                f.render_widget(Paragraph::new(l).block(b), r);
            }
            Overlay::Settings(view) => {
                let tab = |t: SettingsTab, label: &str| Span::styled(format!(" {label} "), if view.tab == t { theme::sel() } else { theme::dim() });
                let mut l = vec![Line::from(vec![tab(SettingsTab::General, "General"), Span::raw(" "), tab(SettingsTab::Models, "Models")]), Line::default()];
                for (i, (k, v)) in self.settings_rows(view.tab).iter().enumerate() {
                    let st = if i == view.sel { theme::sel() } else { Style::default() };
                    l.push(Line::from(vec![Span::styled(format!(" {k:<22}"), st.patch(theme::dim())), Span::styled(v.clone(), st)]));
                }
                l.push(Line::default());
                l.push(Line::from(Span::styled(format!(" saved to {}", config::global_settings_path().display()), theme::dim())));
                let b = block("Settings").title_bottom(Line::from(Span::styled(" tab next tab · ↑↓ select · enter/←→ change · esc close ", theme::dim())));
                f.render_widget(Paragraph::new(l).block(b), r);
            }
        }
    }
}

/// Rows `text` takes when wrapped at `width` (approximate: by display width, per line).
fn wrapped_rows(text: &str, width: usize) -> usize {
    text.lines().map(|l| l.width().div_ceil(width.max(1)).max(1)).sum::<usize>().max(1)
}

fn clipboard(text: &str) -> bool {
    let cmds: &[&[&str]] = if cfg!(target_os = "macos") { &[&["pbcopy"]] } else { &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "-b", "-i"]] };
    for c in cmds {
        if let Ok(mut child) = std::process::Command::new(c[0]).args(&c[1..]).stdin(std::process::Stdio::piped()).spawn() {
            use std::io::Write;
            let _ = child.stdin.take().map(|mut s| s.write_all(text.as_bytes()));
            let _ = child.wait();
            return true;
        }
    }
    false
}

fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn setup() -> ratatui::DefaultTerminal {
    let t = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture, crossterm::event::EnableBracketedPaste);
    t
}

fn teardown() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture, crossterm::event::DisableBracketedPaste);
    ratatui::restore();
}

pub async fn run(app: App, initial: Option<String>) -> Result<()> {
    let (app_tx, mut app_rx) = unbounded_channel();
    let (login_tx, mut login_rx) = unbounded_channel();
    let mut ui = Ui {
        app,
        items: vec![],
        cache: vec![],
        dirty: vec![],
        input: Editor::default(),
        history: vec![],
        hist: None,
        scroll_top: None,
        last_total: 0,
        last_view: 0,
        run_rx: None,
        run: None,
        started: Instant::now(),
        elapsed: Duration::ZERO,
        counts: Counts::default(),
        status: String::new(),
        overlay: None,
        todos: vec![],
        cost: 0.0,
        ctx_tokens: 0,
        app_tx,
        quit_armed: None,
        notice: None,
        spin: 0,
        verb: VERBS[0],
        out_tokens: 0,
        stream_chars: 0,
        asks: VecDeque::new(),
        login_tx,
        cmd_sel: 0,
        handoff: None,
        settings_back: None,
        quit: false,
    };
    theme::set_accent(&ui.app.turn.agent.name);
    ui.rebuild();
    // Prompt history from previous sessions in this directory.
    if let Some(i) = session::list(&ui.app.rt.cwd).first()
        && let Ok(s) = Session::open(&i.path) {
            ui.history = s.context().iter().filter(|m| m.role == Role::User && !m.text().is_empty()).map(|m| m.text()).collect();
        }
    let mut term = setup();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    if let Some(p) = initial {
        ui.send(p);
    }
    loop {
        term.draw(|f| ui.draw(f))?;
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(CEvent::Key(k))) => ui.on_key(k),
                Some(Ok(CEvent::Paste(s))) => {
                    let s = s.replace("\r\n", "\n").replace('\r', "\n");
                    match &mut ui.overlay {
                        None => ui.input.insert(&s),
                        Some(Overlay::Ask(a)) => {
                            a.sel = a.options.len();
                            a.input.insert(&s);
                        }
                        Some(Overlay::Login(v)) => v.input.insert(s.trim()),
                        _ => {}
                    }
                }
                Some(Ok(CEvent::Mouse(m))) => match m.kind {
                    MouseEventKind::ScrollUp => ui.scroll(-3),
                    MouseEventKind::ScrollDown => ui.scroll(3),
                    _ => {}
                },
                Some(Ok(CEvent::Resize(..))) => ui.invalidate_all(),
                Some(Err(_)) | None => break,
                _ => {}
            },
            ev = async { match ui.run_rx.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => {
                match ev {
                    Some(e) => {
                        ui.on_event(e);
                        // Drain whatever else is queued before redrawing.
                        while let Some(e) = ui.run_rx.as_mut().and_then(|r| r.try_recv().ok()) {
                            ui.on_event(e);
                        }
                    }
                    None => { ui.run_rx = None; ui.run = None; }
                }
            }
            Some(e) = app_rx.recv() => ui.on_event(e),
            Some(m) = login_rx.recv() => ui.on_login(m),
            _ = tick.tick() => {
                if ui.run.is_some() {
                    ui.spin += 1;
                }
            }
        }
        if ui.overlay.is_none()
            && let Some(a) = ui.asks.pop_front() {
                ui.overlay = Some(Overlay::Ask(a));
            }
        if ui.quit {
            break;
        }
    }
    if let Some(h) = ui.run.take() {
        h.abort();
    }
    teardown();
    let s = ui.app.session.lock().unwrap();
    if s.has_messages() {
        println!("θ session saved · resume with: theta -r {}", s.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn agent_colors_are_stable_and_distinct() {
        use super::theme::color_for;
        assert_eq!(color_for("Build"), color_for("build"));
        assert_ne!(color_for("Build"), color_for("Plan"));
        assert!((16..232).contains(&color_for("Review")));
    }
}
