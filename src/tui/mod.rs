//! Full-screen ratatui interface.

mod markdown;
mod tabs;
mod tree;

use crate::agent::Event;
use crate::auth::{self, Method};
use crate::agents::{self, Agent};
use base64::Engine;
use crate::client::Remote;
use tabs::{Tab, TabStatus, View};
use crate::config;
use crate::jobs::{JobInfo, Kind as JobKind, Status as JobStatus};
use crate::proto::{Push, RemoteState, Req, Snapshot, Target as Where};
use crate::session::{self, Kind, Session};
use crate::tools::{self, Kind as ToolKind, Todo};
use crate::types::{Block, Msg, Role};
use crate::App;
use anyhow::Result;
use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use futures::StreamExt;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, BorderType, Clear, Padding, Paragraph, Wrap};
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

    /// Color index of the agent the UI is being drawn for: nothing in the interface has a color of its own.
    static ACCENT: AtomicU8 = AtomicU8::new(255);
    pub fn accent() -> Style { Style::default().fg(Color::Indexed(ACCENT.load(Ordering::Relaxed))) }
    /// Run `f` with `accent()` set to this agent's color, then put the previous one back.
    pub fn as_agent<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let prev = ACCENT.swap(color_for(name), Ordering::Relaxed);
        let out = f();
        ACCENT.store(prev, Ordering::Relaxed);
        out
    }
    pub fn set_agent(name: &str) { ACCENT.store(color_for(name), Ordering::Relaxed) }
    pub fn agent(name: &str) -> Style { Style::default().fg(Color::Indexed(color_for(name))) }
    pub fn dim() -> Style { Style::default().fg(Color::DarkGray) }
    pub fn code() -> Style { Style::default().fg(Color::Yellow) }
    pub fn link() -> Style { Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED) }
    pub fn ok() -> Style { Style::default().fg(Color::Green) }
    pub fn err() -> Style { Style::default().fg(Color::Red) }
    pub fn user() -> Style { Style::default().add_modifier(Modifier::BOLD) }
    /// The θ badge of the status bar, in the color of the active agent.
    pub fn logo(name: &str) -> Style { Style::default().fg(Color::Black).bg(Color::Indexed(color_for(name))).add_modifier(Modifier::BOLD) }
    pub fn sel() -> Style { Style::default().bg(Color::Indexed(236)).add_modifier(Modifier::BOLD) }
}

const SPIN: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Inference indicator glyphs (pulses like Claude Code's).
const PULSE: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const VERBS: &[&str] = &["Thinking", "Pondering", "Reasoning", "Working", "Computing", "Brewing", "Forging", "Weaving", "Crafting", "Mulling"];


/// Main actions behind ctrl+p, in menu order. Descriptions come from COMMANDS.
const ACTIONS: &[&str] = &["/model", "/effort", "/agent", "/new", "/resume", "/tree", "/undo", "/compact", "/verbose", "/copy", "/usage", "/settings", "/login", "/help", "/quit"];

const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a new session"),
    ("/resume", "open a previous session"),
    ("/session", "session tabs: new | close | hide | show | <n> | width <n>"),
    ("/tree", "browse and fork the conversation tree"),
    ("/linters", "linters per language and whether they are installed"),
    ("/remote", "control theta from a browser: (none) | stop | status"),
    ("/ssh", "run tools on a server: <ssh arguments> | off"),
    ("/cd", "browse the ssh server's folders: (none) opens the explorer | <dir>"),
    ("/jobs", "background shells, linters and subagents (alt+j)"),
    ("/undo", "remove the last turn and put its prompt back in the input"),
    ("/model", "change model"),
    ("/agent", "change agent"),
    ("/skills", "list skills (use $name in a message)"),
    ("/mcp", "MCP servers: view, login, reconnect, diagnose"),
    ("/effort", "reasoning effort"),
    ("/settings", "settings (verbosity, task models, compaction, web)"),
    ("/verbose", "toggle full / compact output"),
    ("/compact", "compact context now"),
    ("/btw", "side question, not kept in context"),
    ("/title", "rename the session"),
    ("/login", "log in to a provider"),
    ("/logout", "remove provider credentials"),
    ("/usage", "subscription limits and resets (Claude, Codex)"),
    ("/stats", "token statistics across all sessions"),
    ("/copy", "copy the last answer"),
    ("/reload", "re-read settings, agents, skills, MCP config and the guides"),
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
    /// Steered message, shown when typed. `delivered`: the agent has taken it into the session.
    Steer { text: String, delivered: bool },
    /// `agent`: the agent that wrote this part (drives its gutter color).
    Assistant { text: String, thinking: String, done: bool, agent: String },
    /// `who`: for `task` calls, the subagent type that runs (empty otherwise).
    Tool { id: String, name: String, summary: String, state: ToolState, out: String, display: Option<String>, sub: Counts, agent: String, who: String },
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
    /// ctrl+p menu: the value is the command to run.
    Actions,
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
    id: String,
}

impl AskView {
    fn answer(self, remote: &Remote, text: String) {
        remote.send(Req::Answer { id: self.id, text });
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
    Report(Vec<String>),
}

/// `/jobs`: background shells, linters and subagents; enter shows one job's live output.
struct JobsView {
    sel: usize,
    /// Job whose output is shown.
    view: Option<String>,
    text: String,
    polled: Instant,
}

/// Left arrow on an empty prompt: subagents and shell commands of this session, newest first.
struct ActivityView {
    sel: usize,
}

/// `/cd`: folders of the ssh host; enter goes in, backspace goes up.
struct DirView {
    cwd: String,
    /// `..` first, then directories (ending in `/`), then files.
    entries: Vec<String>,
    sel: usize,
}

/// Ctrl+x leader keys, shown while the leader is armed.
const LEADER: &[(&str, &str)] = &[
    ("1 … 9", "go to tab"),
    ("n", "new session tab"),
    ("w", "close session tab"),
    ("b", "hide / show tab bar"),
    ("j", "background jobs"),
    ("h / l", "previous / next tab"),
];

enum Overlay {
    Leader,
    Pick(Picker),
    Dir(DirView),
    Jobs(JobsView),
    Activity(ActivityView),
    Tree(tree::TreeView),
    Settings(SettingsView),
    Help,
    Report(String, Vec<String>),
    Ask(AskView),
    Login(LoginView),
    Mcp(McpView),
}

/// `/mcp`: MCP servers; the state is read live from the registry, `sel` is the hovered server.
struct McpView {
    sel: usize,
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
    /// Byte index of the char under visual (row, col) for `width`, same wrap as `layout`.
    fn at(&self, width: usize, row: usize, col: usize) -> usize {
        let width = width.max(4);
        let (mut line, mut w) = (0, 0);
        for (i, c) in self.text.char_indices() {
            if c == '\n' {
                if line == row {
                    return i;
                }
                line += 1;
                w = 0;
                continue;
            }
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw > width {
                line += 1;
                w = 0;
            }
            if line > row || (line == row && w + cw > col) {
                return i;
            }
            w += cw;
        }
        self.text.len()
    }
    fn word_left(&mut self) {
        let t = self.text[..self.cur].trim_end();
        self.cur = t.char_indices().rev().find(|(_, c)| c.is_whitespace()).map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
    }
    fn word_right(&mut self) {
        let rest = &self.text[self.cur..];
        let t = rest.trim_start();
        let lead = rest.len() - t.len();
        self.cur += lead + t.find(char::is_whitespace).unwrap_or(t.len());
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
    remote: Remote,
    /// A run (turn, compaction or side question) is active in the daemon.
    running: bool,
    /// Messages typed during a run; sent together when it ends.
    queued: Vec<(String, Vec<Block>)>,
    /// Images pasted into the next message (`Block::Image`), shown above the text.
    images: Vec<Block>,
    /// First Esc of a double press; the second one within 2s stops the agent.
    esc_at: Option<Instant>,
    /// Ctrl+x was pressed: the next key is a leader command.
    leader: bool,
    /// The last run was stopped by the user.
    interrupted: bool,
    /// Tabs being reopened at start; they must not take focus while this counts down.
    restoring: usize,
    /// Session ids being opened because another client opened them (not yet attached).
    opening: Vec<String>,
    started: Instant,
    elapsed: Duration,
    counts: Counts,
    status: String,
    overlay: Option<Overlay>,
    todos: Vec<Todo>,
    jobs: Vec<JobInfo>,
    /// Link to theta-server, shared by every tab; `remote_asked` = this TUI ran `/remote` and wants the code shown.
    remote_state: RemoteState,
    remote_asked: bool,
    /// Host the tools of this session run on (`/ssh`).
    ssh: Option<crate::proto::SshInfo>,
    cost: f64,
    ctx_tokens: u64,
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
    /// `@path` suggestions for the word being typed: (query, when listed, entries).
    mentions: std::cell::RefCell<(String, Instant, Vec<(String, String)>)>,
    /// Settings view to return to after a model picker opened from it.
    settings_back: Option<SettingsView>,
    quit: bool,
    /// Open session tabs; `tabs[cur]` is the one mirrored in the fields above.
    tabs: Vec<Tab>,
    cur: usize,
    next_tab_id: u64,
    tab_tx: UnboundedSender<(u64, Option<Push>)>,
    opened_tx: UnboundedSender<Opened>,
    /// Output of `!cmd` shell lines, already formatted for display.
    shell_tx: UnboundedSender<String>,
    /// Clickable parts of the tab strip, as drawn last frame.
    strip: tabs::Strip,
    /// Tab under the mouse, and the marquee offset of its title.
    hover: Option<usize>,
    marquee: usize,
    /// Input text area as drawn last frame: (rect, first visible visual row, wrap width).
    input_hit: (Rect, usize, usize),
    /// Sidebar rect as drawn last frame (vertical only; empty otherwise).
    sidebar: Rect,
    /// Live sidebar width while dragging its divider; saved on release.
    drag: Option<u16>,
    /// Mouse text selection: anchor and cursor as screen (col, row).
    sel: Option<((u16, u16), (u16, u16))>,
    /// Last drawn frame, read to copy the selected text.
    frame: Buffer,
    /// Area the selection is limited to: the open popup, else the main area.
    sel_area: Rect,
}

/// A tab opened in the background: its connection, pushes and first snapshot.
type Opened = Result<(Remote, UnboundedReceiver<Push>, Snapshot), String>;

fn agent_label(a: &Agent) -> String {
    format!("{} ({})", a.name, a.scope.label())
}

/// `s` cut to `w` columns, with `…` when it does not fit.
fn fit(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 1 > w {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// Suggestions for `@query`: the folder named by `query` up to its last `/`, narrowed by what follows (folders first, ending in `/`),
/// then deeper files and folders whose name contains it. Honors .gitignore; dot files only when asked for.
fn mention_entries(cwd: &std::path::Path, query: &str) -> Vec<(String, String)> {
    const MAX: usize = 40;
    let (dir, prefix) = match query.rsplit_once('/') {
        Some((d, p)) => (format!("{d}/"), p),
        None => (String::new(), query),
    };
    let base = if dir.starts_with('/') { std::path::PathBuf::from(&dir) } else { cwd.join(&dir) };
    let lower = prefix.to_lowercase();
    let walk = |depth: usize| {
        ignore::WalkBuilder::new(&base).max_depth(Some(depth)).hidden(!prefix.starts_with('.')).require_git(false).build().flatten().skip(1)
    };
    let size = |e: &ignore::DirEntry| match e.metadata() {
        Ok(m) if m.is_dir() => "folder".to_string(),
        Ok(m) if m.len() >= 1 << 20 => format!("{:.1} MB", m.len() as f64 / (1 << 20) as f64),
        Ok(m) if m.len() >= 1 << 10 => format!("{} KB", m.len() >> 10),
        Ok(m) => format!("{} B", m.len()),
        Err(_) => String::new(),
    };
    let entry = |e: &ignore::DirEntry| {
        let rel = match e.path().strip_prefix(cwd) {
            Ok(r) => r.display().to_string(),
            Err(_) => format!("{dir}{}", e.path().strip_prefix(&base).unwrap_or(e.path()).display()),
        };
        let slash = if e.file_type().is_some_and(|t| t.is_dir()) { "/" } else { "" };
        (format!("@{rel}{slash}"), size(e), slash.is_empty())
    };
    let name_of = |e: &ignore::DirEntry| e.file_name().to_string_lossy().to_string();
    let mut direct: Vec<_> = walk(1)
        .filter(|e| name_of(e).to_lowercase().starts_with(&lower) && !name_of(e).contains(char::is_whitespace))
        .map(|e| entry(&e))
        .collect();
    direct.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    let mut out: Vec<(String, String)> = direct.into_iter().take(MAX).map(|(p, d, _)| (p, d)).collect();
    // No folder typed yet: also find it deeper (`@mod` → src/tui/mod.rs); depth 1 is already listed above.
    if !lower.is_empty() && !query.contains('/') && out.len() < MAX {
        for e in walk(6).take(20_000) {
            let n = name_of(&e).to_lowercase();
            if e.depth() == 1 || !n.contains(&lower) || n.contains(char::is_whitespace) {
                continue;
            }
            let (p, d, _) = entry(&e);
            out.push((p, d));
            if out.len() >= MAX {
                break;
            }
        }
    }
    out
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

    /// Mark the oldest pending steer with this text as taken; a steer this client did not send is added.
    fn steer_taken(&mut self, t: String) {
        match self.items.iter().position(|it| matches!(it, Item::Steer { text, delivered: false } if *text == t)) {
            Some(i) => {
                if let Item::Steer { delivered, .. } = &mut self.items[i] {
                    *delivered = true;
                }
                self.touch(i);
            }
            None => self.push(Item::Steer { text: t, delivered: true }),
        }
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
                    self.push(Item::Assistant { text, thinking, done: true, agent: self.app.turn.agent.name.clone() });
                }
                for (id, name, args) in msg.tool_calls() {
                    let it = self.tool_item(id, name, &args);
                    self.push(it);
                }
            }
        }
    }

    fn model_ctx(&self) -> u64 {
        self.app.rt.catalog.resolve(&self.app.turn.model).map(|(_, m)| m.context).unwrap_or(0)
    }

    // ---------- running ----------

    fn send(&mut self, text: String, images: Vec<Block>) {
        if self.running {
            self.queued.push((text, images));
            self.notify(format!("queued until the agent finishes ({} waiting) — ctrl+enter sends it now", self.queued.len()));
            return;
        }
        if !text.is_empty() {
            self.history.push(text.clone());
        }
        self.hist = None;
        self.save_tabs();
        self.remote.send(Req::Send(text, images));
    }

    /// Ctrl+V: an image on the clipboard is attached; otherwise its text is pasted.
    fn paste_clipboard(&mut self) {
        if let Some(png) = clipboard_image() {
            if png.len() > MAX_IMAGE_BYTES {
                return self.notify("image too large (max 5 MB)");
            }
            let data = base64::engine::general_purpose::STANDARD.encode(&png);
            self.images.push(Block::Image { mime: "image/png".into(), data });
        } else if let Some(text) = clipboard_text() {
            self.input.insert(&normalize_paste(&text));
        }
    }

    /// `!cmd`: run in the user's shell, in this project's directory, without the agent.
    fn shell(&mut self, cmd: String) {
        if cmd.is_empty() {
            return self.notify("usage: !<shell command>");
        }
        self.push(Item::User(format!("!{cmd}")));
        let (tx, cwd) = (self.shell_tx.clone(), self.app.rt.cwd.clone());
        tokio::spawn(async move {
            let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let run = tokio::process::Command::new(sh).arg("-c").arg(&cmd).current_dir(&cwd).stdin(std::process::Stdio::null()).output();
            let text = match tokio::time::timeout(Duration::from_secs(120), run).await {
                Ok(Ok(o)) => {
                    let raw = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
                    let out = tools::shell::compact(&raw);
                    let code = o.status.code().filter(|c| *c != 0).map(|c| format!("\n\nexit {c}")).unwrap_or_default();
                    format!("```\n{out}\n```{code}")
                }
                Ok(Err(e)) => format!("cannot run `{cmd}`: {e}"),
                Err(_) => format!("`{cmd}` timed out after 120s"),
            };
            let _ = tx.send(text);
        });
    }

    /// Ctrl+Enter during a run: the message joins the conversation at the agent's next step.
    fn steer(&mut self) {
        let text = self.input.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.images.is_empty() {
            return self.notify("images can't be steered: press enter to queue them");
        }
        self.input.take();
        self.history.push(text.clone());
        self.hist = None;
        self.push(Item::Steer { text: text.clone(), delivered: false });
        self.remote.send(Req::Steer(text));
    }

    /// Send what was typed during the run, as one message.
    fn flush_queued(&mut self) {
        if self.queued.is_empty() {
            return;
        }
        let (texts, images): (Vec<String>, Vec<Vec<Block>>) = self.queued.drain(..).unzip();
        self.send(texts.join("\n\n"), images.into_iter().flatten().collect());
    }

    /// A run began (here or in another client): reset the progress display.
    fn begin_run(&mut self) {
        self.interrupted = false;
        self.running = true;
        self.started = Instant::now();
        self.counts = Counts::default();
        self.status = "thinking".into();
        self.verb = VERBS[rand::random_range(0..VERBS.len())];
        self.out_tokens = 0;
        self.stream_chars = 0;
        self.scroll_top = None;
    }

    fn interrupt(&mut self) {
        if self.running {
            self.remote.send(Req::Interrupt);
        }
    }

    fn on_interrupted(&mut self) {
        self.running = false;
        self.interrupted = true;
        self.flush_queued();
        self.elapsed = self.started.elapsed();
        for i in 0..self.items.len() {
            if let Item::Tool { state: state @ ToolState::Running, .. } = &mut self.items[i] {
                *state = ToolState::Done { ok: false };
                self.touch(i);
            }
        }
        self.asks.clear();
        if matches!(self.overlay, Some(Overlay::Ask(_))) {
            self.overlay = None;
        }
        self.close_streaming();
        self.push(Item::Info("interrupted".into()));
    }

    fn last_assistant_streaming(&mut self) -> usize {
        if let Some(Item::Assistant { done: false, .. }) = self.items.last() {
            return self.items.len() - 1;
        }
        self.push(Item::Assistant { text: String::new(), thinking: String::new(), done: false, agent: self.app.turn.agent.name.clone() });
        self.items.len() - 1
    }

    /// New running tool row, made by the current agent.
    fn tool_item(&self, id: String, name: String, args: &Value) -> Item {
        let summary = tool_summary(&name, args);
        let who = if name == "task" {
            match args.get("agent").and_then(|v| v.as_str()) {
                Some(n) => agents::find(&self.app.rt.agents, n).map(|a| a.name.clone()).unwrap_or_else(|| n.to_string()),
                None => self.app.turn.agent.name.clone(),
            }
        } else {
            String::new()
        };
        Item::Tool { id, name, summary, state: ToolState::Running, out: String::new(), display: None, sub: Counts::default(), agent: self.app.turn.agent.name.clone(), who }
    }

    /// Agents that wrote the parts of the turn started by user item `i`, first-seen order.
    fn turn_agents(&self, i: usize) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for it in &self.items[i + 1..] {
            match it {
                Item::User(_) => break,
                Item::Assistant { agent, .. } | Item::Tool { agent, .. } => {
                    if !names.contains(&agent.as_str()) {
                        names.push(agent);
                    }
                }
                _ => {}
            }
        }
        names
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
                if let Some(i) = self.items.iter().rposition(|it| matches!(it, Item::Tool { id: tid, .. } if *tid == id)) {
                    self.touch(i);
                } else {
                    let it = self.tool_item(id, name, &args);
                    self.push(it);
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
            Event::Handoff { agent } => self.notify(format!("switching to {agent} after this reply")),
            Event::Ask { id, question, options } => {
                self.status = "waiting for your answer".into();
                self.asks.push_back(AskView { question, options, sel: 0, input: Editor::default(), id });
            }
            Event::Answered(id) => {
                self.asks.retain(|a| a.id != id);
                if matches!(&self.overlay, Some(Overlay::Ask(a)) if a.id == id) {
                    self.overlay = None;
                }
            }
            Event::Started => self.begin_run(),
            Event::User(t) => self.push(Item::User(t)),
            // The steer was shown when typed; this marks it as taken by the agent (or shows one from another client).
            Event::Steer(t) => self.steer_taken(t),
            Event::Interrupted => self.on_interrupted(),
            Event::Todos(t) => self.todos = t,
            Event::Compacting => {
                self.status = "compacting context".into();
                self.push(Item::Info("compacting context…".into()));
            }
            Event::Compacted { before } => {
                self.push(Item::Info(format!("context compacted ({}k tokens summarized)", before / 1000)));
                self.ctx_tokens = crate::compact::current_tokens(&self.app.session.lock().unwrap());
            }
            Event::Side { question, answer } => {
                self.push(Item::Info(format!("btw: {question}")));
                self.push(Item::Assistant { text: answer, thinking: String::new(), done: true, agent: self.app.turn.agent.name.clone() });
            }
            Event::Title(t) => self.notify(format!("session: {t}")),
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
                self.mark_seen();
                self.running = false;
                self.elapsed = self.started.elapsed();
                self.flush_queued();
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

    // ---------- commands ----------

    fn command(&mut self, line: &str) {
        let (cmd, arg) = line.split_once(' ').map(|(c, a)| (c, a.trim())).unwrap_or((line, ""));
        match cmd {
            "/new" => {
                if self.is_blank() {
                    return self.notify("this session is still empty");
                }
                self.interrupt();
                self.attach(Where::New);
            }
            "/resume" => self.open_sessions(),
            "/session" => match arg {
                "" | "new" => self.new_tab(),
                "close" => self.close_tab(),
                "hide" => self.set_tab_hidden(true),
                "show" => self.set_tab_hidden(false),
                n if n.starts_with("width") => match n["width".len()..].trim().parse::<u16>() {
                    Ok(w) => self.set_tab_width(w),
                    Err(_) => self.notify(format!("usage: /session width <{}-{}>", tabs::WIDTH_MIN, tabs::WIDTH_MAX)),
                },
                n => match n.parse::<usize>() {
                    Ok(n) if (1..=self.tabs.len()).contains(&n) => self.switch_to(n - 1),
                    _ => self.notify("usage: /session [new | close | hide | show | <n> | width <n>]"),
                },
            },
            "/tree" => self.open_tree(),
            "/undo" => self.undo_turn(),
            "/model" => self.open_models(Target::Main),
            "/agent" => self.open_agents(),
            "/mcp" => self.mcp_command(),
            "/skills" => {
                let l: Vec<_> = self.app.rt.skills.iter().map(|s| format!("${}{} ({})", s.name, if s.auto { "*" } else { "" }, s.scope.label())).collect();
                self.notify(if l.is_empty() { "no skills in ~/.theta/skills".into() } else { format!("{} — * = auto", l.join("  ")) });
            }
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
            "/btw" => self.btw(arg),
            "/title" => {
                if arg.is_empty() {
                    self.notify("usage: /title <name>");
                } else {
                    self.remote.send(Req::Title(arg.to_string()));
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
            "/usage" => self.report("Usage", |rt| Box::pin(async move { crate::usage::usage(&rt).await })),
            "/stats" => self.report("Stats", |rt| Box::pin(async move { tokio::task::spawn_blocking(move || crate::usage::stats(&rt.catalog)).await.unwrap_or_default() })),
            "/copy" => self.copy_last(),
            "/jobs" => self.open_jobs(),
            "/remote" => {
                self.remote_asked = true;
                match arg {
                    "" => self.remote.send(Req::RemoteStart { cwd: self.app.rt.cwd.clone() }),
                    "stop" => self.remote.send(Req::RemoteStop),
                    "status" => self.remote.send(Req::RemoteStatus),
                    _ => self.notify("usage: /remote [stop|status]"),
                }
            }
            "/ssh" => self.remote.send(Req::Ssh(arg.to_string())),
            "/cd" if arg.is_empty() => self.remote.send(Req::Ls(String::new())),
            "/cd" => self.remote.send(Req::Cd(arg.to_string())),
            "/linters" => self.overlay = Some(Overlay::Report("θ linters".into(), crate::lint::report(&self.app.rt.settings))),
            "/reload" => self.reload(),
            "/help" => self.overlay = Some(Overlay::Help),
            "/quit" | "/exit" => self.quit = true,
            _ => self.notify(format!("unknown command {cmd}")),
        }
    }

    fn open_jobs(&mut self) {
        self.overlay = Some(Overlay::Jobs(JobsView { sel: 0, view: None, text: String::new(), polled: Instant::now() }));
    }

    /// Subagent and shell tool calls, newest first: (kind, state (None = running, Some(ok)), label, output).
    fn activity(&self) -> Vec<(&'static str, Option<bool>, String, String)> {
        self.items
            .iter()
            .rev()
            .filter_map(|it| match it {
                Item::Tool { name, summary, state, out, .. } if name == "task" || name == "bash" => {
                    let kind = if name == "task" { "agent" } else { "shell" };
                    let done = match state {
                        ToolState::Running => None,
                        ToolState::Done { ok } => Some(*ok),
                    };
                    Some((kind, done, summary.clone(), out.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Refresh the output shown in `/jobs` about once a second.
    fn poll_job(&mut self) {
        if let Some(Overlay::Jobs(v)) = &mut self.overlay
            && let Some(id) = &v.view
            && v.polled.elapsed() >= Duration::from_secs(1)
        {
            v.polled = Instant::now();
            self.remote.send(Req::JobOutput(id.clone()));
        }
    }

    /// Open a text overlay and fill it when `job` finishes.
    fn report(&mut self, title: &str, job: impl FnOnce(crate::agent::Runtime) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send>>) {
        self.overlay = Some(Overlay::Report(title.into(), vec!["loading…".into()]));
        let (tx, fut) = (self.login_tx.clone(), job(self.app.rt.clone()));
        tokio::spawn(async move {
            let _ = tx.send(LoginMsg::Report(fut.await));
        });
    }

    fn logout(&mut self, id: &str) {
        let auth = self.app.rt.auth.clone();
        let id = id.to_string();
        let remote = self.remote.clone();
        tokio::spawn(async move {
            let _ = auth.remove(&id).await;
            remote.send(Req::Reload);
        });
        self.notify("credentials removed");
    }

    fn copy_last(&mut self) {
        let text = self.app.session.lock().unwrap().context().iter().rev().find(|m| m.role == Role::Assistant && !m.text().is_empty()).map(|m| m.text());
        let Some(text) = text else { return self.notify("nothing to copy") };
        self.notify(if clipboard(&text) { "copied" } else { "no clipboard tool found" });
    }

    fn compact_now(&mut self) {
        if self.running {
            return self.notify("busy");
        }
        self.remote.send(Req::Compact);
    }

    /// Answer a side question from the current context. The daemon never writes it to the session.
    fn btw(&mut self, question: &str) {
        if question.is_empty() {
            return self.notify("usage: /btw <question>");
        }
        if self.running {
            return self.notify("busy");
        }
        self.remote.send(Req::Btw(question.to_string()));
    }

    /// Join (or create) a session in the daemon; the snapshot that comes back resets the view.
    fn attach(&mut self, target: Where) {
        self.remote.send(Req::Attach { target, cwd: self.app.rt.cwd.clone(), agent: None, model: None });
    }

    /// Re-read everything from disk here and in the daemon, which also reconnects changed MCP servers.
    fn reload(&mut self) {
        let project = self.app.rt.project.clone();
        let settings = match config::load(&project) {
            Ok(s) => s,
            Err(e) => return self.notify(format!("reload failed: {e:#}")),
        };
        self.app.rt.skills = Arc::new(crate::skills::discover(&project, &settings.skills.auto));
        self.app.rt.agents = Arc::new(agents::discover(&project));
        self.app.rt.settings = Arc::new(settings);
        self.remote.send(Req::Reload);
        self.invalidate_all();
        self.notify("reloaded settings, agents, skills, MCP config and guides");
    }

    fn update_settings(&mut self, key: &str, value: toml::Value, apply: impl FnOnce(&mut config::Settings)) {
        let mut s = (*self.app.rt.settings).clone();
        apply(&mut s);
        self.app.rt.settings = Arc::new(s);
        if let Err(e) = config::set_global(key, value) {
            self.notify(format!("could not save settings: {e}"));
        }
        self.remote.send(Req::Reload);
        self.invalidate_all();
    }

    fn toggle_verbose(&mut self) {
        let v = if self.verbose() { "compact" } else { "full" };
        self.update_settings("verbose", v.into(), |s| s.verbose = v.into());
        self.notify(format!("output: {v}"));
    }

    fn set_effort(&mut self, e: &str) {
        self.app.turn.effort = e.to_string();
        self.remote.send(Req::SetEffort(e.to_string()));
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
            self.remote.send(Req::SetModel(self.app.turn.model.clone()));
            self.remote.send(Req::SetEffort(self.app.turn.effort.clone()));
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

    fn open_actions(&mut self) {
        let items = ACTIONS.iter().map(|c| item(*c, COMMANDS.iter().find(|(n, _)| n == c).map_or("", |(_, d)| *d), *c)).collect();
        self.overlay = Some(Overlay::Pick(Picker { title: "Actions  (type to filter)".into(), items, filter: String::new(), sel: 0, purpose: Purpose::Actions }));
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

    /// `/mcp`: pop-up listing the MCP servers; keys act on the selected one.
    fn mcp_command(&mut self) {
        self.overlay = Some(Overlay::Mcp(McpView { sel: 0 }));
    }

    fn mcp_login(&mut self, n: String) {
        let Some(cfg) = self.app.rt.mcp.config(&n) else { return };
        let (say, mut said) = unbounded_channel();
        let (paste_tx, paste) = unbounded_channel();
        let tx = self.login_tx.clone();
        let remote = self.remote.clone();
        let mut v = LoginView { title: format!("MCP · {n}"), provider: String::new(), key: false, lines: vec!["starting…".into()], input: Editor::default(), paste: Some(paste_tx), task: None };
        v.task = Some(tokio::spawn(async move {
            let fwd_tx = tx.clone();
            let fwd = tokio::spawn(async move {
                while let Some(m) = said.recv().await {
                    let _ = fwd_tx.send(LoginMsg::Say(m));
                }
            });
            let r = crate::mcp::login(&n, &cfg, &mut auth::Io { say, paste }).await;
            if r.is_ok() {
                remote.send(Req::McpConnect(n.clone()));
            }
            let _ = fwd.await;
            let _ = tx.send(LoginMsg::Done(r.map_err(|e| format!("{e:#}"))));
        }));
        self.overlay = Some(Overlay::Login(v));
    }

    /// Prompt that asks theta to diagnose one MCP server. Goes in the input bar; the user sends it.
    fn mcp_diagnose_prompt(&self, name: &str) -> String {
        let mcp = &self.app.rt.mcp;
        let state = match mcp.rows().into_iter().find(|r| r.name == name).and_then(|r| r.error) {
            Some(e) => format!("Error: {e}"),
            None => "Error: none (no connection error reported)".into(),
        };
        let cfg = mcp.config(name).unwrap_or_default();
        let target = if cfg.url.is_empty() { format!("stdio command `{} {}`", cfg.command, cfg.args.join(" ")) } else { format!("HTTP url `{}`", cfg.url) };
        let env = if cfg.env.is_empty() { "none".to_string() } else { cfg.env.keys().cloned().collect::<Vec<_>>().join(", ") };
        format!(
            "The MCP server `{name}` in theta is not working.\n{state}\nTransport: {target}. Env vars (values hidden): {env}.\n\
             Diagnose the cause and fix the config so it connects. Check that the command exists and starts, its args, the url, \
             auth (`theta mcp login {name}` for OAuth) and the `[mcp.{name}]` table in ~/.theta/settings.toml. \
             Then tell me what you changed so I can reconnect it from /mcp."
        )
    }

    fn on_login(&mut self, m: LoginMsg) {
        if let (LoginMsg::Report(lines), Some(Overlay::Report(_, l))) = (&m, &mut self.overlay) {
            *l = lines.clone();
            return;
        }
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
                self.remote.send(Req::Reload);
                self.invalidate_all();
            }
            LoginMsg::Done(Err(e)) => {
                v.lines.push(format!("✗ {e}"));
                v.task = None;
            }
            LoginMsg::Report(_) => {}
        }
    }

    /// Drop the last turn from the active branch (kept in the tree) and edit its prompt.
    fn undo_turn(&mut self) {
        if self.running {
            return self.notify("interrupt the agent first (Esc)");
        }
        let last = tree::last_prompt(&self.app.session.lock().unwrap());
        let Some((parent, text)) = last else { return self.notify("nothing to undo") };
        self.remote.send(Req::Leaf(parent));
        self.input.set(text);
        self.notify("undone: edit and press Enter to resend");
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
                        self.remote.send(Req::SetModel(value.clone()));
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
                self.attach(Where::Resume(value));
            }
            Purpose::Login => self.open_login_methods(&value),
            Purpose::LoginMethod => {
                if let Some((id, i)) = value.split_once('\t') {
                    self.start_login(id, i.parse().unwrap_or(0));
                }
            }
            Purpose::Logout => self.logout(&value),
            Purpose::Effort => self.set_effort(&value),
            Purpose::Actions => self.command(&value),
        }
    }

    /// The daemon applies the agent's model and effort rules and answers with `Push::Meta`.
    fn switch_agent(&mut self, a: Agent) {
        self.remote.send(Req::SetAgent(a.name));
    }

    fn cycle_agent(&mut self) {
        let agents: Vec<_> = self.app.rt.agents.iter().filter(|a| !a.hidden).cloned().collect();
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
        if self.overlay.is_some() && !self.leader {
            return self.overlay_key(k);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let cmds = self.cmd_matches();
        let hints = self.hints();
        if matches!(k.code, KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete) {
            self.cmd_sel = 0;
        }
        // Leader (ctrl+x) then one key: vim-like, no Option/Alt needed.
        if std::mem::take(&mut self.leader) {
            self.overlay = None;
            match k.code {
                KeyCode::Char(c @ '1'..='9') => {
                    let n = c as usize - '1' as usize;
                    if n < self.tabs.len() { self.switch_to(n) } else { self.notify(format!("no tab {c}")) }
                }
                KeyCode::Char('n') => self.new_tab(),
                KeyCode::Char('w') => self.close_tab(),
                KeyCode::Char('b') => self.toggle_tab_bar(),
                KeyCode::Char('j') => self.open_jobs(),
                KeyCode::Char('h') => self.step_tab(-1),
                KeyCode::Char('l') => self.step_tab(1),
                KeyCode::Esc => {}
                _ => self.notify("leader: unknown key"),
            }
            return;
        }
        match k.code {
            KeyCode::Char('x') if ctrl => {
                self.leader = true;
                self.overlay = Some(Overlay::Leader);
            }
            // Tab keys: alt, because ctrl+tab and ctrl+shift+t belong to terminal emulators such as Warp.
            KeyCode::Char(c @ '1'..='9') if alt => {
                let n = c as usize - '1' as usize;
                if n < self.tabs.len() { self.switch_to(n) } else { self.notify(format!("no tab {c}")) }
            }
            KeyCode::Char('n') if alt => self.new_tab(),
            KeyCode::Char('w') if alt => self.close_tab(),
            // Word moves: Option+Left/Right send ESC b / ESC f on macOS terminals, so alt+b/f must be word moves.
            KeyCode::Backspace if alt => self.input.delete_word(),
            KeyCode::Left if alt => self.input.word_left(),
            KeyCode::Right if alt => self.input.word_right(),
            KeyCode::Char('b') if alt => self.input.word_left(),
            KeyCode::Char('f') if alt => self.input.word_right(),
            KeyCode::Char('j') if alt => self.open_jobs(),
            KeyCode::PageDown if alt => self.step_tab(1),
            KeyCode::PageUp if alt => self.step_tab(-1),
            KeyCode::Up | KeyCode::Down if !hints.is_empty() && !k.modifiers.contains(KeyModifiers::SHIFT) => {
                let n = hints.len();
                self.cmd_sel = if k.code == KeyCode::Up { (self.cmd_sel + n - 1) % n } else { (self.cmd_sel + 1) % n };
            }
            KeyCode::Char('c') if ctrl => {
                if !self.input.text.is_empty() || !self.images.is_empty() {
                    self.input.take();
                    self.images.clear();
                } else if self.running {
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
                if self.running {
                    if self.esc_at.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                        self.esc_at = None;
                        self.interrupt();
                    } else {
                        self.esc_at = Some(Instant::now());
                        self.notify("press esc again to stop the agent");
                    }
                } else {
                    self.scroll_top = None;
                }
            }
            KeyCode::Left if self.input.text.is_empty() && self.images.is_empty() => {
                self.overlay = Some(Overlay::Activity(ActivityView { sel: 0 }));
            }
            KeyCode::Char('o') if ctrl => self.toggle_verbose(),
            KeyCode::Char('t') if ctrl => self.open_tree(),
            KeyCode::Char('p') if ctrl => self.open_actions(),
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
            KeyCode::Char('v') if ctrl => self.paste_clipboard(),
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => self.input.insert("\n"),
            KeyCode::Enter | KeyCode::Char('j') if ctrl && self.running => self.steer(),
            KeyCode::Enter => {
                let text = self.input.text.trim().to_string();
                if text.is_empty() && self.images.is_empty() {
                    return;
                }
                // A half-typed $skill completes first; a full name sends.
                if let Some(tok) = self.skill_token().map(str::to_string)
                    && let Some((h, _)) = hints.get(self.cmd_sel).cloned()
                    && h != tok
                {
                    self.complete(&h);
                    self.cmd_sel = 0;
                    return;
                }
                if let Some(cmd) = text.strip_prefix('!') {
                    self.input.take();
                    self.shell(cmd.trim().to_string());
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
                let images = std::mem::take(&mut self.images);
                self.send(text, images);
            }
            KeyCode::Tab => {
                if let Some((c, _)) = hints.get(self.cmd_sel).cloned() {
                    self.complete(&c);
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
            KeyCode::Backspace if self.input.text.is_empty() => {
                self.images.pop();
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

    /// The word the cursor is in (up to the cursor), with its byte offset.
    fn token(&self) -> (usize, &str) {
        let before = &self.input.text[..self.input.cur];
        let start = before.char_indices().rev().find(|(_, c)| c.is_whitespace()).map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
        (start, &before[start..])
    }

    /// The `$skill` or `@path` being typed at the cursor, if any.
    fn skill_token(&self) -> Option<&str> {
        Some(self.token().1).filter(|t| t.starts_with(['$', '@']))
    }

    /// Suggestions under the input: slash commands, `$prefix` skills, `@path` files and folders.
    fn hints(&self) -> Vec<(String, String)> {
        let slash = self.cmd_matches();
        if !slash.is_empty() {
            return slash.iter().map(|(c, d)| (c.to_string(), d.to_string())).collect();
        }
        let Some(tok) = self.skill_token() else { return vec![] };
        if let Some(query) = tok.strip_prefix('@') {
            // Tools run on the ssh host: its files are not on this disk.
            if self.ssh.is_some() {
                return vec![];
            }
            let mut cache = self.mentions.borrow_mut();
            if cache.0 != query || cache.1.elapsed() > Duration::from_secs(2) {
                *cache = (query.to_string(), Instant::now(), mention_entries(&self.app.rt.cwd, query));
            }
            return cache.2.clone();
        }
        let prefix = tok[1..].to_lowercase();
        self.app
            .rt
            .skills
            .iter()
            .filter(|s| s.name.to_lowercase().starts_with(&prefix))
            .map(|s| {
                let d = if s.description.is_empty() { s.scope.label().to_string() } else { s.description.clone() };
                (format!("${}", s.name), d)
            })
            .collect()
    }

    /// Put a picked hint into the input: `$skill` and `@path` replace the word at the cursor (a folder keeps the cursor inside it, to go deeper), `/cmd` replaces the line.
    fn complete(&mut self, hint: &str) {
        let (start, word) = self.token();
        let (start, end) = if word.starts_with(['$', '@']) { (start, self.input.cur) } else { (0, self.input.text.len()) };
        let tail = if hint.ends_with('/') { "" } else { " " };
        self.input.text.replace_range(start..end, &format!("{hint}{tail}"));
        self.input.cur = start + hint.len() + tail.len();
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
            Overlay::Help | Overlay::Leader => {}
            Overlay::Mcp(mut v) => {
                let rows = self.app.rt.mcp.rows();
                let name = rows.get(v.sel).map(|r| r.name.clone());
                match (k.code, name) {
                    (KeyCode::Esc | KeyCode::Char('q'), _) => return,
                    (KeyCode::Up, _) => v.sel = v.sel.saturating_sub(1),
                    (KeyCode::Down, _) => v.sel = (v.sel + 1).min(rows.len().saturating_sub(1)),
                    (KeyCode::Enter | KeyCode::Char('r'), Some(n)) => {
                        self.notify(format!("reconnecting {n}…"));
                        self.remote.send(Req::McpConnect(n));
                        return;
                    }
                    (KeyCode::Char('l'), Some(n)) => return self.mcp_login(n),
                    (KeyCode::Char('o'), Some(n)) => match crate::mcp::logout(&n) {
                        Ok(()) => {
                            self.notify(format!("logged out of {n}"));
                            self.remote.send(Req::McpConnect(n));
                        }
                        Err(e) => self.push(Item::Error(format!("{e:#}"))),
                    },
                    (KeyCode::Char('d'), Some(n)) => {
                        self.input.set(self.mcp_diagnose_prompt(&n));
                        return;
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Mcp(v));
            }
            Overlay::Report(title, lines) => {
                // The link code is typed on another device: `c` puts it on the clipboard and keeps the overlay.
                if title == "θ remote" && matches!(k.code, KeyCode::Char('c')) {
                    if let RemoteState::Code { code, .. } = &self.remote_state {
                        let ok = clipboard(code);
                        self.notify(if ok { "code copied" } else { "no clipboard tool found" });
                    }
                    self.overlay = Some(Overlay::Report(title, lines));
                }
            }
            Overlay::Dir(mut v) => {
                let last = v.entries.len().saturating_sub(1);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => return,
                    KeyCode::Up => v.sel = v.sel.saturating_sub(1),
                    KeyCode::Down => v.sel = (v.sel + 1).min(last),
                    KeyCode::PageUp => v.sel = v.sel.saturating_sub(10),
                    KeyCode::PageDown => v.sel = (v.sel + 10).min(last),
                    KeyCode::Home => v.sel = 0,
                    KeyCode::End => v.sel = last,
                    KeyCode::Backspace | KeyCode::Left => self.remote.send(Req::Cd("..".into())),
                    KeyCode::Char('~') => self.remote.send(Req::Cd("~".into())),
                    KeyCode::Enter | KeyCode::Right => {
                        if let Some(e) = v.entries.get(v.sel)
                            && let Some(dir) = e.strip_suffix('/').or((e == "..").then_some(".."))
                        {
                            self.remote.send(Req::Cd(dir.to_string()));
                        }
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Dir(v));
            }
            Overlay::Jobs(mut v) => {
                let viewing = v.view.is_some();
                match (viewing, k.code) {
                    (_, KeyCode::Char('k')) => {
                        if let Some(id) = v.view.clone().or_else(|| self.jobs.get(v.sel).map(|j| j.id.clone())) {
                            self.remote.send(Req::JobKill(id));
                        }
                    }
                    (true, KeyCode::Esc | KeyCode::Char('q')) => v.view = None,
                    (false, KeyCode::Esc | KeyCode::Char('q')) => return,
                    (false, KeyCode::Up) => v.sel = v.sel.saturating_sub(1),
                    (false, KeyCode::Down) => v.sel = (v.sel + 1).min(self.jobs.len().saturating_sub(1)),
                    (false, KeyCode::Enter) => {
                        if let Some(j) = self.jobs.get(v.sel) {
                            v.view = Some(j.id.clone());
                            v.text.clear();
                            v.polled = Instant::now();
                            self.remote.send(Req::JobOutput(j.id.clone()));
                        }
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Jobs(v));
            }
            Overlay::Activity(mut v) => {
                let rows = self.activity();
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => return,
                    KeyCode::Up => v.sel = v.sel.saturating_sub(1),
                    KeyCode::Down => v.sel = (v.sel + 1).min(rows.len().saturating_sub(1)),
                    KeyCode::Enter => {
                        if let Some((kind, _, label, out)) = rows.into_iter().nth(v.sel) {
                            let lines = if out.is_empty() { vec!["(no output yet)".to_string()] } else { out.lines().map(String::from).collect() };
                            self.overlay = Some(Overlay::Report(format!("θ {kind} · {label}"), lines));
                            return;
                        }
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Activity(v));
            }
            Overlay::Ask(mut a) => {
                let free = a.options.len();
                match k.code {
                    KeyCode::Esc => return a.answer(&self.remote, String::new()),
                    KeyCode::Up => a.sel = a.sel.saturating_sub(1),
                    KeyCode::Down => a.sel = (a.sel + 1).min(free),
                    KeyCode::Enter if a.sel < free => {
                        let o = a.options[a.sel].clone();
                        return a.answer(&self.remote, o);
                    }
                    KeyCode::Enter => {
                        let t = a.input.text.trim().to_string();
                        if !t.is_empty() {
                            return a.answer(&self.remote, t);
                        }
                    }
                    KeyCode::Char(c @ '1'..='9') if a.sel < free && (c as usize - '1' as usize) < free => {
                        let o = a.options[c as usize - '1' as usize].clone();
                        return a.answer(&self.remote, o);
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
                    if self.running {
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
                    self.remote.send(Req::Leaf(leaf));
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
                ("Tab bar".into(), s.tab_orientation.clone()),
                ("Tab bar width".into(), format!("{} cols", s.tab_width)),
            ],
            SettingsTab::Models => {
                let mut rows = vec![
                    ("Main model".into(), self.app.turn.model.clone()),
                    ("Reasoning effort".into(), self.app.turn.effort.clone()),
                    ("Compaction model".into(), or_main(&s.models.compaction)),
                    ("Session-title model".into(), or_main(&s.models.title)),
                    ("Subagent model".into(), or_main(&s.models.subagent)),
                    ("Subagent effort".into(), if s.models.subagent_effort.is_empty() { "(same as agent)".into() } else { s.models.subagent_effort.clone() }),
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
            let agent_row = n.checked_sub(6).map(|r| (r / 2, r % 2 == 0));
            match (n, agent_row) {
                (1, _) => {
                    let e = cycle(&["low", "medium", "high", "xhigh", "max"], &self.app.turn.effort);
                    self.set_effort(&e);
                }
                (5, _) => {
                    let e = cycle(&["", "low", "medium", "high", "xhigh", "max"], &s.models.subagent_effort);
                    self.update_settings("models.subagent_effort", e.clone().into(), |s| s.models.subagent_effort = e);
                }
                (0, _) | (2..=4, _) | (_, Some((_, true))) => {
                    self.settings_back = Some(view);
                    match n {
                        0 => self.open_models(Target::Main),
                        2 => self.open_models(Target::Compaction),
                        3 => self.open_models(Target::Title),
                        4 => self.open_models(Target::Subagent),
                        _ => self.open_models(Target::Agent((n - 6) / 2)),
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
            5 => {
                let v = cycle(&["horizontal", "vertical"], &s.tab_orientation);
                self.update_settings("tab_orientation", v.clone().into(), |s| s.tab_orientation = v);
            }
            6 => {
                let w = if back {
                    if s.tab_width >= tabs::WIDTH_MIN + 2 { s.tab_width - 2 } else { tabs::WIDTH_MAX }
                } else if s.tab_width + 2 > tabs::WIDTH_MAX {
                    tabs::WIDTH_MIN
                } else {
                    s.tab_width + 2
                };
                self.set_tab_width(w);
            }
            _ => {}
        }
    }

    // ---------- drawing ----------

    /// Assistant and tool parts get a gutter bar in the color of the agent that wrote them.
    /// Everything accented inside an item (bullet, headings, spinner) takes the color of its agent;
    /// a prompt keeps its white `›`, its agent label names who answered.
    fn item_lines(&self, i: usize, width: usize, full: bool) -> Vec<Line<'static>> {
        match &self.items[i] {
            Item::Assistant { agent, .. } | Item::Tool { agent, .. } => theme::as_agent(agent, || self.item_body(i, width, full)),
            Item::User(_) | Item::Steer { .. } => {
                let who = self.turn_agents(i).first().map(|a| a.to_string()).unwrap_or_else(|| self.app.turn.agent.name.clone());
                theme::as_agent(&who, || self.item_body(i, width, full))
            }
            _ => self.item_body(i, width, full),
        }
    }

    /// User prompts and steers share one band, one blank row above and below, full width.
    /// The top row names the agents that answered, right-aligned; a steer also labels itself on the left (`steer`: delivered?).
    fn user_lines(&self, i: usize, t: &str, width: usize, steer: Option<bool>) -> Vec<Line<'static>> {
        let band = Style::default().bg(Color::Indexed(237));
        let spans = vec![Span::styled(t.to_string(), theme::user())];
        let mut label: Vec<Span> = Vec::new();
        for (k, name) in self.turn_agents(i).into_iter().enumerate() {
            if k > 0 {
                label.push(Span::styled(" → ", theme::dim()));
            }
            label.push(Span::styled(name.to_string(), theme::agent(name).add_modifier(Modifier::BOLD)));
        }
        let left = match steer {
            Some(true) => Span::styled("Steering", theme::accent().add_modifier(Modifier::BOLD)),
            Some(false) => Span::styled("Steering", theme::dim()),
            None => Span::raw(""),
        };
        let used: usize = left.width() + label.iter().map(|s| s.width()).sum::<usize>();
        let mut top = vec![left, Span::raw(" ".repeat(width.saturating_sub(used)))];
        top.extend(label);
        let mut out = vec![Line::from(top).style(band)];
        for l in markdown::wrap(spans, width, vec![Span::styled("› ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))], vec![Span::raw("  ")]) {
            let fill = width.saturating_sub(l.width());
            let mut s = l.spans;
            s.push(Span::raw(" ".repeat(fill)));
            out.push(Line::from(s).style(band));
        }
        out.push(Line::from(" ".repeat(width)).style(band));
        out
    }

    fn item_body(&self, i: usize, width: usize, full: bool) -> Vec<Line<'static>> {
        let it = &self.items[i];
        let running_turn = self.running && !self.items[i + 1..].iter().any(|x| matches!(x, Item::User(_)));
        match it {
            Item::User(t) => self.user_lines(i, t, width, None),
            Item::Steer { text, delivered } => self.user_lines(i, text, width, Some(*delivered)),
            Item::Assistant { text, thinking, done, .. } => {
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
                    // A bullet on the first line, the rest indented under it.
                    for (n, l) in markdown::render(text, width.saturating_sub(2)).into_iter().enumerate() {
                        let mark = if n == 0 { Span::styled("● ", theme::accent()) } else { Span::raw("  ") };
                        let mut spans = vec![mark];
                        spans.extend(l.spans);
                        out.push(Line::from(spans).style(l.style));
                    }
                }
                if out.len() == 1 { vec![] } else { out }
            }
            Item::Tool { name, summary, state, out, display, sub, who, .. } => {
                if !full {
                    return vec![];
                }
                let (icon, st) = match state {
                    ToolState::Running => (SPIN[self.spin % SPIN.len()], theme::accent()),
                    ToolState::Done { ok: true } => ("✓", theme::ok()),
                    ToolState::Done { ok: false } => ("✗", theme::err()),
                };
                let mut head = vec![Span::styled(format!("  {icon} "), st), Span::styled(name.clone(), theme::accent().add_modifier(Modifier::BOLD)), Span::raw(" ")];
                if !who.is_empty() {
                    head.push(Span::styled(format!("({who}) "), theme::agent(who).add_modifier(Modifier::BOLD)));
                }
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
            let reusable = !self.dirty[i] && !running_tool && !self.running;
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
        theme::set_agent(&self.app.turn.agent.name);
        let orientation = self.app.rt.settings.tab_orientation.as_str();
        let hidden = self.app.rt.settings.tab_hidden;
        let vertical = !hidden && orientation == "vertical";
        let infos = self.tab_infos();
        let width = self.drag.unwrap_or(self.app.rt.settings.tab_width.clamp(tabs::WIDTH_MIN, tabs::WIDTH_MAX));
        let (strip_area, area) = if hidden {
            (Rect::default(), f.area())
        } else if vertical {
            let [a, b] = Layout::horizontal([Constraint::Length(width), Constraint::Min(20)]).areas(f.area());
            (a, b)
        } else {
            let [a, b] = Layout::vertical([Constraint::Length(1), Constraint::Min(5)]).areas(f.area());
            (a, b)
        };
        self.sidebar = if vertical { strip_area } else { Rect::default() };
        let hover = self.hover.map(|i| (i, self.marquee));
        let (lines, strip) = if hidden { (Vec::new(), tabs::Strip::default()) } else { tabs::strip(strip_area, &infos, self.spin, vertical, hover) };
        if !hidden {
            f.render_widget(Paragraph::new(lines), strip_area);
        }
        self.strip = strip;
        // Chat, input and status sit between two blank columns (popups and overlays keep the full area).
        let margin = if area.width >= 60 { 2 } else { 1 };
        let main = Rect { x: area.x + margin, width: area.width.saturating_sub(2 * margin), ..area };
        let width = main.width as usize;
        let (input_lines, cursor) = self.input.layout(width.saturating_sub(2));
        let chips = u16::from(!self.images.is_empty());
        let input_h = input_lines.len().min(8) as u16 + chips + 2;
        let todo_h = if self.todos.is_empty() || self.todos.iter().all(|t| t.status == "done") && !self.running { 0 } else { (self.todos.len().min(8) + 1) as u16 };
        let show_progress = self.running || self.counts.read + self.counts.write + self.counts.cmd + self.counts.tools > 0;
        let [chat, todo_area, progress, _gap, input_area, _gap2, status] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(todo_h),
            Constraint::Length(if show_progress { 1 } else { 0 }),
            Constraint::Length(1),
            Constraint::Length(input_h),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .areas(main);

        // chat
        let chat_inner = chat;
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
            let secs = if self.running { self.started.elapsed() } else { self.elapsed }.as_secs();
            let time = if secs >= 60 { format!("{}m{:02}s", secs / 60, secs % 60) } else { format!("{secs}s") };
            if self.running {
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
                spans.push(Span::styled(format!(" ({time} · ↓ {tok} tokens · "), theme::dim()));
                spans.push(Span::styled("esc", Style::default().fg(Color::White)));
                spans.push(Span::styled(" to interrupt)  ", theme::dim()));
                spans.extend(self.counts.spans());
                if !matches!(self.status.as_str(), "thinking" | "writing") && !self.status.starts_with("compacting") && !self.status.starts_with("waiting") {
                    let st: String = self.status.chars().take(width / 3).collect();
                    spans.push(Span::styled(format!("  ⎿ {st}"), theme::dim()));
                }
            } else {
                let (mark, style) = if self.interrupted { (" ■ interrupted  ", theme::err()) } else { (" ✓ done  ", theme::ok()) };
                spans.push(Span::styled(mark, style));
                spans.extend(self.counts.spans());
                spans.push(Span::styled(format!("  · {time}"), theme::dim()));
            }
            f.render_widget(Paragraph::new(Line::from(spans)), progress);
        }

        // input
        // Lighter band with one blank row above and below the text.
        let band = Style::default().bg(Color::Indexed(237));
        f.render_widget(UiBlock::default().style(band), input_area);
        let inner = Rect { y: input_area.y + 1, height: input_area.height.saturating_sub(2), ..input_area };
        let skip = cursor.0.saturating_sub(7);
        let prompt = Style::default().fg(Color::White);
        let mut il: Vec<Line> = Vec::new();
        if !self.images.is_empty() {
            let mut spans = vec![Span::raw("  ")];
            spans.extend((1..=self.images.len()).map(|n| Span::styled(format!("[image {n}] "), theme::accent())));
            il.push(Line::from(spans));
        }
        for (n, l) in input_lines.iter().enumerate().skip(skip).take(8) {
            let prefix = if n == 0 { Span::styled("› ", prompt) } else { Span::raw("  ") };
            il.push(Line::from(vec![prefix, Span::raw(l.clone())]));
        }
        if self.input.text.is_empty() && self.images.is_empty() {
            il = vec![Line::from(vec![Span::styled("› ", prompt), Span::styled("Ask anything — / for commands, tab to switch agent", theme::dim())])];
        }
        f.render_widget(Paragraph::new(il).style(band), inner);
        let text_area = Rect { x: inner.x + 2, y: inner.y + chips, width: inner.width.saturating_sub(2), height: inner.height.saturating_sub(chips) };
        self.input_hit = (text_area, skip, width.saturating_sub(2));
        if self.overlay.is_none() {
            f.set_cursor_position((inner.x + 2 + cursor.1 as u16, inner.y + chips + (cursor.0 - skip) as u16));
        }

        // slash-command hints
        let m = self.hints();
        if self.overlay.is_none() && !m.is_empty() {
            {
                let sel = self.cmd_sel.min(m.len() - 1);
                let max = 10.min(input_area.y as usize).max(1);
                let start = sel.saturating_sub(max - 1);
                let h = m.len().min(max) as u16;
                // As wide as the input, not the terminal (the tab sidebar is not covered).
                let w = input_area.width as usize;
                let r = Rect { x: input_area.x, y: input_area.y.saturating_sub(h), width: input_area.width, height: h };
                f.render_widget(Clear, r);
                let name_w = m.iter().map(|(c, _)| c.width()).max().unwrap_or(0).max(10).min(w / 2);
                let lines: Vec<Line> = m.iter().enumerate().skip(start).take(max).map(|(i, (c, d))| {
                    let st = if i == sel { theme::sel() } else { Style::default() };
                    let name = fit(c, name_w);
                    let desc = fit(d, w.saturating_sub(name_w + 2));
                    let pad = " ".repeat(w.saturating_sub(name_w + 2 + desc.width()));
                    Line::from(vec![
                        Span::styled(format!(" {name:<name_w$}"), st.patch(Style::default().fg(Color::White))),
                        Span::styled(format!(" {desc}{pad}"), st.patch(theme::dim())),
                    ])
                }).collect();
                f.render_widget(Paragraph::new(lines), r);
            }
        }

        // status bar: who and what model on the first row, the rest (ssh, jobs, remote, session, context, cost) on the second
        let a = &self.app.turn.agent;
        let ag = theme::agent(&a.name);
        let mut top = vec![
            Span::styled(" θ ", theme::logo(&a.name)),
            Span::styled(format!(" {} ", a.name), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!("({})", a.scope.label()), theme::dim()),
            Span::styled("  ", theme::dim()),
            Span::styled(self.app.turn.model.clone(), ag),
            Span::styled(format!(" · {}", self.app.turn.effort), theme::dim()),
        ];
        if !self.verbose() {
            top.push(Span::styled(" · compact", theme::dim()));
        }
        let mut left = vec![Span::raw(" ")];
        let sep = |left: &mut Vec<Span<'static>>, s: Span<'static>| {
            if left.len() > 1 {
                left.push(Span::styled(" · ", theme::dim()));
            }
            left.push(s);
        };
        if let Some(s) = &self.ssh {
            sep(&mut left, Span::styled(format!("⇢ {}:{}", s.host, s.cwd), ag));
        }
        let live = self.jobs.iter().filter(|j| j.status == JobStatus::Running).count();
        if live > 0 {
            sep(&mut left, Span::styled(format!("⚙ {live}"), ag));
        }
        match &self.remote_state {
            RemoteState::Linked => sep(&mut left, Span::styled("⇄ remote", ag)),
            RemoteState::Code { .. } => sep(&mut left, Span::styled("⇄ waiting for browser", theme::dim())),
            _ => {}
        }
        let ctx_max = self.model_ctx();
        let pct = if ctx_max > 0 { self.ctx_tokens * 100 / ctx_max } else { 0 };
        let right = match &self.notice {
            Some((n, t)) if t.elapsed() < Duration::from_secs(4) => vec![Span::styled(format!("{n} "), ag)],
            _ => vec![
                Span::styled(format!("{}k/{}k {}% ", self.ctx_tokens / 1000, ctx_max / 1000, pct), if pct >= 80 { theme::err() } else { theme::dim() }),
                Span::styled(format!("${:.3} ", self.cost), theme::dim()),
            ],
        };
        let lw: usize = left.iter().map(|s| s.content.width()).sum();
        let rw: usize = right.iter().map(|s| s.content.width()).sum();
        let mut second = left;
        if lw + rw < width {
            second.push(Span::raw(" ".repeat(width - lw - rw)));
            second.extend(right);
        }
        f.render_widget(Paragraph::new(vec![Line::from(top), Line::from(second)]), status);

        // overlays
        let prev = self.sel_area;
        self.sel_area = match &self.overlay {
            Some(ov) => self.draw_overlay(f, ov, area).inner(Margin::new(2, 2)),
            None => area,
        };
        // popup opened or closed: the old selection no longer lives in this area
        if prev != self.sel_area {
            self.sel = None;
        }
    }

    fn splash(&self, h: usize) -> Vec<Line<'static>> {
        const LOGO: [&str; 4] = [r" _   _        _", r"| |_| |_  ___| |_ __ _", r"|  _| ' \/ -_)  _/ _` |", r" \__|_||_\___|\__\__,_|"];
        let a = &self.app.turn.agent;
        let accent = theme::agent(&a.name);
        let white = Style::default().fg(Color::White);
        let mut out: Vec<Line<'static>> = LOGO
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let mut spans = vec![Span::styled(format!("  {l}"), accent)];
                if i == LOGO.len() - 1 && crate::update::is_dev() {
                    spans.push(Span::styled(" (dev)", theme::dim()));
                }
                Line::from(spans)
            })
            .collect();
        out.push(Line::from(Span::styled(format!("  θ v{}  ·  terminal coding agent", env!("CARGO_PKG_VERSION")), theme::dim())));
        out.push(Line::from(Span::styled(format!("  {}", "─".repeat(44)), theme::dim())));
        out.push(Line::from(vec![Span::styled("  agent  ", theme::dim()), Span::raw(agent_label(a))]));
        out.push(Line::from(vec![Span::styled("  model  ", theme::dim()), Span::raw(self.app.turn.model.clone())]));
        out.push(Line::from(vec![Span::styled("  cwd    ", theme::dim()), Span::raw(self.app.rt.cwd.display().to_string())]));
        if let Some(s) = &self.ssh {
            out.push(Line::from(vec![Span::styled("  ssh    ", theme::dim()), Span::raw(format!("{}:{}", s.host, s.cwd))]));
        }
        if let Some((p, _)) = self.app.rt.catalog.resolve(&self.app.turn.model) {
            let local = p.env.is_empty() && p.oauth.is_none();
            if !local && futures::executor::block_on(self.app.rt.auth.status(&p)).is_none() {
                out.push(Line::default());
                out.push(Line::from(vec![Span::styled("  ! ", theme::err()), Span::raw(format!("no credentials for {} — run ", p.name)), Span::styled("/login", theme::accent()), Span::raw(" or pick another model with "), Span::styled("ctrl+p", theme::accent())]));
            }
        }
        out.push(Line::from(Span::styled(format!("  {}", "─".repeat(44)), theme::dim())));
        out.push(Line::from(vec![
            Span::styled("  /help", white),
            Span::styled(" commands  ·  ", theme::dim()),
            Span::styled("tab", white),
            Span::styled(" agent  ·  ", theme::dim()),
            Span::styled("ctrl+p", white),
            Span::styled(" model  ·  ", theme::dim()),
            Span::styled("ctrl+t", white),
            Span::styled(" tree", theme::dim()),
        ]));
        let mut top = vec![Line::default(); h.saturating_sub(out.len()) / 2];
        top.extend(out);
        top
    }

    /// Draws the popup and returns its rect, the only area a mouse selection may cover.
    fn draw_overlay(&self, f: &mut Frame, ov: &Overlay, area: Rect) -> Rect {
        let w = (area.width.saturating_sub(4)).min(100);
        let iw = w.saturating_sub(6) as usize;
        let content = match ov {
            Overlay::Help => COMMANDS.len() + 18,
            Overlay::Leader => LEADER.len() + 2,
            Overlay::Dir(v) => v.entries.len().min(18) + 3,
            Overlay::Jobs(v) => if v.view.is_some() { 24 } else { self.jobs.len().max(1) + 3 },
            Overlay::Report(_, l) => l.len() + 2,
            Overlay::Mcp(_) => self.app.rt.mcp.rows().len().max(1) + 6,
            Overlay::Activity(_) => self.activity().len().max(1) + 3,
            Overlay::Pick(p) => p.rows().max(1) + 1,
            Overlay::Tree(t) => t.rows.len() + 2,
            Overlay::Settings(v) => self.settings_rows(v.tab).len() + 6,
            Overlay::Ask(a) => wrapped_rows(&a.question, iw) + a.options.len() + 4,
            Overlay::Login(v) => v.lines.iter().map(|l| wrapped_rows(l, iw) + 1).sum::<usize>() + 4,
        } as u16;
        let min_h = if matches!(ov, Overlay::Pick(_)) { 12 } else { 5 };
        let h = area.height.saturating_sub(4).min(content.max(min_h) + 4);
        let r = Rect { x: (area.width - w) / 2, y: (area.height.saturating_sub(h)) / 3, width: w, height: h };
        f.render_widget(Clear, r);
        let block = |t: &str| UiBlock::bordered().padding(Padding::uniform(1)).border_type(BorderType::Rounded).border_style(theme::accent()).title(Span::styled(format!(" {t} "), Style::default().add_modifier(Modifier::BOLD)));
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
                l.push(Line::default());
                l.push(hint(" ↑↓ select · enter answer · 1-9 pick · esc skip "));
                f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(block("θ question")), r);
                if on_free {
                    let row = wrapped_rows(&a.question, iw) + 1 + a.options.len();
                    let col = 5 + a.input.text[..a.input.cur].width();
                    f.set_cursor_position((r.x + 1 + col as u16, r.y + 1 + row as u16));
                }
            }
            Overlay::Dir(v) => {
                let rows = h.saturating_sub(4) as usize;
                let top = (v.sel + 1).saturating_sub(rows);
                let l: Vec<Line> = v.entries.iter().enumerate().skip(top).take(rows).map(|(i, e)| {
                    let dir = e.ends_with('/') || e == "..";
                    let st = if i == v.sel { theme::sel() } else if dir { theme::accent() } else { theme::dim() };
                    Line::from(vec![Span::styled(format!(" {} ", if i == v.sel { "›" } else { " " }), theme::accent()), Span::styled(e.clone(), st)])
                }).collect();
                let name: String = v.cwd.chars().rev().take(iw.saturating_sub(10)).collect::<Vec<_>>().into_iter().rev().collect();
                f.render_widget(Paragraph::new(l).block(block(&format!("θ {name}")).title_bottom(hint(" ↑↓ select · enter open · ⌫ up · ~ home · esc close "))), r);
            }
            Overlay::Activity(v) => {
                let rows = self.activity();
                let vis = h.saturating_sub(3) as usize;
                let top = (v.sel + 1).saturating_sub(vis);
                let mut l: Vec<Line> = rows.iter().enumerate().skip(top).take(vis).map(|(i, (kind, state, label, _))| {
                    let (mark, st) = match state {
                        None => ("●", theme::accent()),
                        Some(true) => ("✓", theme::dim()),
                        Some(false) => ("✗", theme::err()),
                    };
                    let sel = if i == v.sel { theme::sel() } else { Style::default() };
                    let label: String = label.chars().take(iw.saturating_sub(14)).collect();
                    Line::from(vec![
                        Span::styled(format!(" {} ", if i == v.sel { "›" } else { " " }), theme::accent()),
                        Span::styled(format!("{mark} "), st),
                        Span::styled(format!("{kind:<6} {label}"), sel),
                    ])
                }).collect();
                if l.is_empty() {
                    l.push(Line::from(Span::styled(" nothing yet", theme::dim())));
                }
                f.render_widget(Paragraph::new(l).block(block("θ activity").title_bottom(hint(" ↑↓ select · enter output · esc close "))), r);
            }
            Overlay::Jobs(v) => {
                let mut l = Vec::new();
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                if let Some(id) = &v.view {
                    let rows = h.saturating_sub(4) as usize;
                    let lines: Vec<&str> = v.text.lines().collect();
                    l.extend(lines[lines.len().saturating_sub(rows)..].iter().map(|x| Line::from(x.to_string())));
                    let label = self.jobs.iter().find(|j| &j.id == id).map(|j| j.label.chars().take(60).collect::<String>()).unwrap_or_default();
                    f.render_widget(Paragraph::new(l).block(block(&format!("θ job {id} · {label}")).title_bottom(hint(" k stop · esc back "))), r);
                } else {
                    for (i, j) in self.jobs.iter().enumerate() {
                        let (mark, st) = match j.status {
                            JobStatus::Running => ("●", theme::accent()),
                            JobStatus::Done(0) => ("✓", theme::dim()),
                            JobStatus::Done(_) => ("✗", theme::err()),
                            JobStatus::Killed => ("■", theme::dim()),
                            JobStatus::Missing => ("✗", theme::err()),
                        };
                        let kind = match j.kind {
                            JobKind::Shell => "shell",
                            JobKind::Lint => "lint ",
                            JobKind::Agent => "agent",
                        };
                        let secs = j.ended.unwrap_or(now).saturating_sub(j.started);
                        let sel = if i == v.sel { theme::sel() } else { Style::default() };
                        let label: String = j.label.chars().take(iw.saturating_sub(24)).collect();
                        l.push(Line::from(vec![
                            Span::styled(format!(" {} ", if i == v.sel { "›" } else { " " }), theme::accent()),
                            Span::styled(format!("{mark} "), st),
                            Span::styled(format!("{}  {kind}  {secs:>4}s  {label}", j.id), sel),
                        ]));
                    }
                    if l.is_empty() {
                        l.push(Line::from(Span::styled(" no background jobs", theme::dim())));
                    }
                    f.render_widget(Paragraph::new(l).block(block("θ jobs").title_bottom(hint(" ↑↓ select · enter output · k stop · esc close "))), r);
                }
            }
            Overlay::Mcp(v) => {
                let rows = self.app.rt.mcp.rows();
                let mut l = Vec::new();
                if rows.is_empty() {
                    l.push(Line::from(Span::styled(" no MCP servers — add [mcp.<name>] to settings.toml", theme::dim())));
                }
                for (i, r) in rows.iter().enumerate() {
                    let (mark, st, state) = if !r.enabled {
                        ("○", theme::dim(), "disabled".to_string())
                    } else if let Some(t) = r.tools {
                        ("●", theme::accent(), format!("{t} tools"))
                    } else if r.error.as_deref().is_some_and(|e| e.starts_with("unauthorized")) {
                        ("○", theme::accent(), "needs login".to_string())
                    } else if let Some(e) = &r.error {
                        ("✗", theme::err(), e.clone())
                    } else {
                        ("…", theme::dim(), "connecting".to_string())
                    };
                    let kind = if r.http { "http" } else { "stdio" };
                    let text: String = format!(" {mark} {}  {kind} · {state}", r.name).chars().take(iw + 1).collect();
                    let sel = if i == v.sel { theme::sel() } else { st };
                    l.push(Line::from(vec![Span::styled(if i == v.sel { " › " } else { "   " }, theme::accent()), Span::styled(text, sel)]));
                }
                if let Some(r) = rows.get(v.sel) {
                    let c = self.app.rt.mcp.config(&r.name).unwrap_or_default();
                    let target = if c.url.is_empty() { format!("{} {}", c.command, c.args.join(" ")) } else { c.url.clone() };
                    l.push(Line::default());
                    l.push(Line::from(Span::styled(format!(" {target}"), theme::dim())));
                    if let Some(e) = &r.error {
                        l.push(Line::from(Span::styled(format!(" {e}"), theme::err())));
                    }
                }
                f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(block("θ mcp").title_bottom(hint(" ↑↓ select · enter reconnect · l login · o logout · d diagnose · esc close "))), r);
            }
            Overlay::Report(title, lines) => {
                let mut l = lines.iter().map(|x| Line::from(Span::styled(x.clone(), if x.starts_with(' ') || x.starts_with("loading") { Style::default() } else { theme::accent().add_modifier(Modifier::BOLD) }))).collect::<Vec<_>>();
                l.push(Line::default());
                l.push(hint(" any key to close "));
                f.render_widget(Paragraph::new(l).block(block(title)), r);
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
                let keys = if v.key { " enter save · esc cancel " } else { " waiting for the browser · enter submit pasted code · ctrl+y copy URL · esc cancel " };
                l.push(Line::default());
                l.push(hint(keys));
                f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(block(&v.title)), r);
                let row: usize = v.lines.iter().map(|x| wrapped_rows(x, iw) + 1).sum();
                f.set_cursor_position((r.x + 1 + (label.width() + shown.width()) as u16, r.y + 1 + row as u16));
            }
            Overlay::Help => {
                let mut l = vec![Line::from(Span::styled("Keys", theme::accent().add_modifier(Modifier::BOLD)))];
                for (k, d) in [
                    ("enter", "send · alt+enter / ctrl+j newline"),
                    ("esc", "interrupt the agent"),
                    ("tab / shift+tab", "next agent / cycle effort"),
                    ("ctrl+p", "actions menu"),
                    ("ctrl+t", "conversation tree (fork)"),
                    ("ctrl+r", "resume a session"),
                    ("alt+j", "background jobs (shells, linters, subagents)"),
                    ("alt+n / alt+w", "new / close session tab"),
                    ("alt+1 … alt+9", "go to tab"),
                    ("alt+pgdn / alt+pgup", "next / previous tab"),
                    ("click tab / × / +", "switch / close / new tab"),
                    ("ctrl+o", "full / compact output"),
                    ("pgup pgdn shift+↑↓ wheel", "scroll"),
                    ("ctrl+c ×2 / ctrl+d", "quit"),
                ] {
                    l.push(Line::from(vec![Span::styled(format!("  {k:<26}"), Style::default().fg(Color::White)), Span::styled(d, theme::dim())]));
                }
                l.push(Line::default());
                l.push(Line::from(Span::styled("Commands", theme::accent().add_modifier(Modifier::BOLD))));
                for (c, d) in COMMANDS {
                    l.push(Line::from(vec![Span::styled(format!("  {c:<26}"), Style::default().fg(Color::White)), Span::styled(*d, theme::dim())]));
                }
                f.render_widget(Paragraph::new(l).block(block("θ help")), r);
            }
            Overlay::Leader => {
                let mut l: Vec<Line> = LEADER.iter().map(|(k, d)| Line::from(vec![Span::styled(format!("  {k:<12}"), Style::default().fg(Color::White)), Span::styled(*d, theme::dim())])).collect();
                l.push(Line::default());
                l.push(hint(" a key to run · esc cancel "));
                f.render_widget(Paragraph::new(l).block(block("θ ctrl+x")), r);
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
                l.push(Line::default());
                l.push(hint(" ↑↓ move · enter continue from here · e edit & branch · esc close "));
                f.render_widget(Paragraph::new(l).block(block("Conversation tree")), r);
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
                l.push(Line::default());
                l.push(hint(" tab next tab · ↑↓ select · enter/←→ change · esc close "));
                f.render_widget(Paragraph::new(l).block(block("Settings")), r);
            }
        }
        r
    }
}

/// Key hint drawn as the last line of an overlay body: each "key rest" segment has the key in white, the rest dimmed.
fn hint(keys: &'static str) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, seg) in keys.split(" · ").enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        let (lead, body) = seg.split_at(seg.len() - seg.trim_start().len());
        let (key, rest) = body.split_once(' ').map_or((body, ""), |(k, r)| (k, r));
        spans.push(Span::raw(lead));
        spans.push(Span::styled(key.to_string(), Style::default().fg(Color::White)));
        if !rest.is_empty() {
            spans.push(Span::styled(format!(" {rest}"), theme::dim()));
        }
    }
    Line::from(spans)
}

/// Rows `text` takes when wrapped at `width` (approximate: by display width, per line).
fn wrapped_rows(text: &str, width: usize) -> usize {
    text.lines().map(|l| l.width().div_ceil(width.max(1)).max(1)).sum::<usize>().max(1)
}

/// Largest image attached from the clipboard (Anthropic's limit).
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Stdout of the first command that succeeds with output.
fn clipboard_out(cmds: &[&[&str]]) -> Option<Vec<u8>> {
    cmds.iter().find_map(|c| {
        let o = std::process::Command::new(c[0]).args(&c[1..]).stderr(std::process::Stdio::null()).output().ok()?;
        (o.status.success() && !o.stdout.is_empty()).then_some(o.stdout)
    })
}

/// PNG bytes on the clipboard, if any.
fn clipboard_image() -> Option<Vec<u8>> {
    if cfg!(target_os = "macos") {
        if let Some(png) = clipboard_out(&[&["pngpaste", "-"]]) {
            return Some(png);
        }
        // osascript prints the PNG as hex: «data PNGf89504E47…»
        let s = String::from_utf8(clipboard_out(&[&["osascript", "-e", "the clipboard as «class PNGf»"]])?).ok()?;
        let hex = s.trim().strip_prefix("«data PNGf")?.strip_suffix('»')?;
        return (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect();
    }
    clipboard_out(&[&["wl-paste", "--type", "image/png"], &["xclip", "-selection", "clipboard", "-t", "image/png", "-o"]])
}

fn clipboard_text() -> Option<String> {
    let cmds: &[&[&str]] = if cfg!(target_os = "macos") { &[&["pbpaste"]] } else { &[&["wl-paste", "-n"], &["xclip", "-selection", "clipboard", "-o"], &["xsel", "-b", "-o"]] };
    String::from_utf8(clipboard_out(cmds)?).ok()
}

/// Pasted text as the input shows it: newlines only, tabs as spaces, no other control characters.
fn normalize_paste(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ").chars().filter(|c| *c == '\n' || !c.is_control()).collect()
}

fn clipboard(text: &str) -> bool {
    let cmds: &[&[&str]] = if cfg!(target_os = "macos") { &[&["pbcopy"]] } else { &[&["wl-copy"], &["xclip", "-selection", "clipboard"], &["xsel", "-b", "-i"]] };
    for c in cmds {
        // stderr silenced: xclip's "Can't open display" would print over the TUI and stick
        if let Ok(mut child) = std::process::Command::new(c[0]).args(&c[1..]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            use std::io::Write;
            let _ = child.stdin.take().map(|mut s| s.write_all(text.as_bytes()));
            if child.wait().is_ok_and(|s| s.success()) {
                return true;
            }
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

impl Ui {
    fn title_now(&self) -> String {
        let t = self.app.session.lock().unwrap().title.clone();
        t.or_else(|| self.items.iter().find_map(|i| if let Item::User(t) = i { Some(t.clone()) } else { None })).unwrap_or_default()
    }

    fn swap_view(&mut self, v: &mut View) {
        use std::mem::swap;
        swap(&mut self.items, &mut v.items);
        swap(&mut self.cache, &mut v.cache);
        swap(&mut self.dirty, &mut v.dirty);
        swap(&mut self.input, &mut v.input);
        swap(&mut self.scroll_top, &mut v.scroll_top);
        swap(&mut self.last_total, &mut v.last_total);
        swap(&mut self.last_view, &mut v.last_view);
        swap(&mut self.remote, &mut v.remote);
        swap(&mut self.running, &mut v.running);
        swap(&mut self.started, &mut v.started);
        swap(&mut self.elapsed, &mut v.elapsed);
        swap(&mut self.counts, &mut v.counts);
        swap(&mut self.status, &mut v.status);
        swap(&mut self.todos, &mut v.todos);
        swap(&mut self.jobs, &mut v.jobs);
        swap(&mut self.cost, &mut v.cost);
        swap(&mut self.ctx_tokens, &mut v.ctx_tokens);
        swap(&mut self.verb, &mut v.verb);
        swap(&mut self.out_tokens, &mut v.out_tokens);
        swap(&mut self.stream_chars, &mut v.stream_chars);
        swap(&mut self.asks, &mut v.asks);
        swap(&mut self.app.session, &mut v.session);
        swap(&mut self.app.turn, &mut v.turn);
    }

    /// An open question belongs to its tab: put it back in the queue before the view changes.
    fn park_overlay(&mut self) {
        if let Some(Overlay::Ask(a)) = self.overlay.take_if(|o| matches!(o, Overlay::Ask(_))) {
            self.asks.push_front(a);
        }
    }

    /// Make `view` the active one and park the current state in the tab being left.
    fn install(&mut self, mut view: View, to: usize) {
        self.park_overlay();
        let title = self.title_now();
        let (agent, model) = (self.app.turn.agent.name.clone(), self.app.turn.model.clone());
        self.swap_view(&mut view);
        let from = &mut self.tabs[self.cur];
        from.status = if view.running { TabStatus::Working } else { TabStatus::Idle };
        from.title = title;
        from.agent = agent;
        from.model = model;
        from.view = Some(view);
        self.cur = to;
    }

    fn switch_to(&mut self, to: usize) {
        if to == self.cur || to >= self.tabs.len() {
            return;
        }
        let view = self.tabs[to].view.take().expect("background tab keeps its view");
        self.install(view, to);
        let t = &mut self.tabs[to];
        (t.status, t.errored) = (TabStatus::Idle, false);
        for p in std::mem::take(&mut t.backlog) {
            self.on_push(p);
        }
        self.mark_seen();
        self.invalidate_all();
    }

    fn step_tab(&mut self, d: isize) {
        let n = self.tabs.len() as isize;
        if n > 1 {
            self.switch_to((self.cur as isize + d).rem_euclid(n) as usize);
        }
    }

    /// The active session has no message yet: starting another one would only add an empty tab.
    fn is_blank(&self) -> bool {
        !self.app.session.lock().unwrap().has_messages()
    }

    /// Write the open tabs (sessions with messages) so the next start reopens them.
    fn save_tabs(&self) {
        if self.restoring > 0 {
            return;
        }
        let open: Vec<String> = self
            .tabs
            .iter()
            .filter_map(|t| {
                let s = match &t.view {
                    Some(v) => v.session.clone(),
                    None => self.app.session.clone(),
                };
                let s = s.lock().unwrap();
                s.has_messages().then(|| s.id.clone())
            })
            .collect();
        self.remote.send(Req::SaveTabs { cwd: self.app.rt.cwd.clone(), open });
    }

    /// The active tab's result has been seen: the daemon drops its unread mark.
    fn mark_seen(&self) {
        self.remote.send(Req::Seen);
    }

    /// Reopen the tabs that were open at last exit, after the first one.
    fn restore(&mut self, first: String) {
        let ids: Vec<String> = session::saved_open(&self.app.rt.cwd).into_iter().filter(|i| *i != first).collect();
        if ids.is_empty() {
            return;
        }
        self.restoring = ids.len();
        let (tx, cwd) = (self.opened_tx.clone(), self.app.rt.cwd.clone());
        // One task, in order: tabs appear in the order they were saved.
        tokio::spawn(async move {
            for id in ids {
                let r = crate::client::open(Where::Resume(id), cwd.clone(), None, None).await.map_err(|e| format!("{e:#}"));
                if tx.send(r).is_err() {
                    break;
                }
            }
        });
    }

    /// Last restored tab is in: focus the first tab again and show the unread marks saved for the parked ones.
    fn finish_restore(&mut self) {
        self.switch_to(0);
        let f = session::load_tabs(&self.app.rt.cwd);
        for t in self.tabs.iter_mut().skip(1) {
            let Some(v) = &t.view else { continue };
            let id = v.session.lock().unwrap().id.clone();
            if let Some(u) = f.unseen.get(&id).filter(|_| !v.running) {
                t.status = if u == "error" { TabStatus::Error } else { TabStatus::Done };
            }
        }
    }

    /// Open a fresh session in a new tab, with the current agent and model.
    fn new_tab(&mut self) {
        if self.is_blank() {
            return self.notify("this session is still empty");
        }
        let (tx, cwd) = (self.opened_tx.clone(), self.app.rt.cwd.clone());
        let (agent, model) = (Some(self.app.turn.agent.name.clone()), Some(self.app.turn.model.clone()));
        tokio::spawn(async move {
            let _ = tx.send(crate::client::open(Where::New, cwd, agent, model).await.map_err(|e| format!("{e:#}")));
        });
    }

    /// Session id of tab `i`: the active tab's state lives in `Ui`, the others are parked in their view.
    fn tab_session_id(&self, i: usize) -> Option<String> {
        let s = match &self.tabs[i].view {
            Some(v) => v.session.clone(),
            None => self.app.session.clone(),
        };
        let s = s.lock().unwrap();
        s.has_messages().then(|| s.id.clone())
    }

    /// Another client (the web UI) opened or closed tabs of this project: do the same here.
    fn on_tabs(&mut self, cwd: &std::path::Path, opened: Vec<String>, closed: Vec<String>) {
        if cwd != self.app.rt.cwd || self.restoring > 0 {
            return;
        }
        for id in closed {
            if let Some(i) = (0..self.tabs.len()).find(|&i| self.tab_session_id(i).as_deref() == Some(&id)) {
                self.close_at(i);
            }
        }
        for id in opened {
            let have = (0..self.tabs.len()).any(|i| self.tab_session_id(i).as_deref() == Some(&id));
            if have || self.opening.contains(&id) {
                continue;
            }
            self.opening.push(id.clone());
            let (tx, cwd) = (self.opened_tx.clone(), self.app.rt.cwd.clone());
            tokio::spawn(async move {
                let _ = tx.send(crate::client::open(Where::Resume(id), cwd, None, None).await.map_err(|e| format!("{e:#}")));
            });
        }
    }

    fn tab_opened(&mut self, opened: Opened) {
        match &opened {
            Ok((_, _, snap)) => self.opening.retain(|i| *i != snap.session_id),
            Err(_) => self.opening.clear(),
        }
        let restored = self.restoring > 0;
        self.restoring = self.restoring.saturating_sub(1);
        self.open_tab(opened);
        if restored && self.restoring == 0 {
            self.finish_restore();
        }
        self.save_tabs();
    }

    fn open_tab(&mut self, opened: Opened) {
        let (remote, rx, snap) = match opened {
            Ok(o) => o,
            Err(e) => return self.push(Item::Error(format!("could not open a tab: {e}"))),
        };
        let turn = match crate::make_turn(&self.app.rt, Some(&snap.agent), Some(&snap.model)) {
            Ok(t) => t,
            Err(e) => return self.push(Item::Error(format!("could not open a tab: {e:#}"))),
        };
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.forward(id, rx);
        self.tabs.push(Tab::new(id));
        self.install(View::fresh(remote, turn), self.tabs.len() - 1);
        self.on_snapshot(snap);
    }

    fn close_tab(&mut self) {
        self.close_at(self.cur);
    }

    /// Close tab `dead`; a run still going in it is interrupted. Closing the active tab moves to its neighbour.
    fn close_at(&mut self, dead: usize) {
        if self.tabs.len() < 2 {
            return self.notify("last tab — /quit to exit");
        }
        if dead == self.cur {
            self.interrupt();
            self.switch_to(if dead > 0 { dead - 1 } else { 1 });
        } else if self.tabs[dead].view.as_ref().is_some_and(|v| v.running) {
            if let Some(v) = &self.tabs[dead].view {
                v.remote.send(Req::Interrupt);
            }
        }
        self.tabs.remove(dead);
        self.save_tabs();
        if dead < self.cur {
            self.cur -= 1;
        }
    }

    /// What the strip shows: the active tab from live state, the others from what they last reported.
    fn tab_infos(&self) -> Vec<tabs::TabInfo> {
        (0..self.tabs.len())
            .map(|i| {
                if i == self.cur {
                    let working = self.running;
                    let detail = match (working, self.status.is_empty()) {
                        (false, _) => "idle".to_string(),
                        (true, true) => "working".to_string(),
                        (true, false) => self.status.clone(),
                    };
                    tabs::TabInfo {
                        title: self.title_now(),
                        status: if working { TabStatus::Working } else { TabStatus::Idle },
                        agent: self.app.turn.agent.name.clone(),
                        model: self.app.turn.model.clone(),
                        detail,
                        active: true,
                    }
                } else {
                    let t = &self.tabs[i];
                    let detail = match t.status {
                        TabStatus::Working => "working",
                        TabStatus::Done => "finished",
                        TabStatus::Error => "error",
                        TabStatus::Idle => "idle",
                    };
                    tabs::TabInfo { title: t.title.clone(), status: t.status, agent: t.agent.clone(), model: t.model.clone(), detail: detail.into(), active: false }
                }
            })
            .collect()
    }

    fn set_tab_hidden(&mut self, hidden: bool) {
        self.update_settings("tab_hidden", hidden.into(), |s| s.tab_hidden = hidden);
    }

    /// ctrl+x b: hide the tab bar, or bring it back in its saved orientation.
    fn toggle_tab_bar(&mut self) {
        let hidden = !self.app.rt.settings.tab_hidden;
        self.set_tab_hidden(hidden);
    }

    fn set_tab_width(&mut self, w: u16) {
        let v = w.clamp(tabs::WIDTH_MIN, tabs::WIDTH_MAX);
        self.update_settings("tab_width", (v as i64).into(), |s| s.tab_width = v);
    }

    /// The sidebar's right edge: grab it and drag to resize.
    fn on_divider(&self, col: u16, row: u16) -> bool {
        self.sidebar.width > 0 && col == self.sidebar.right() - 1 && row >= self.sidebar.y && row < self.sidebar.bottom()
    }

    /// Release after a drag: save the new width once.
    fn end_drag(&mut self) {
        if let Some(w) = self.drag.take() {
            self.set_tab_width(w);
        }
    }

    /// A press outside the selection area (e.g. outside the popup) selects nothing.
    fn sel_start(&mut self, col: u16, row: u16) {
        let r = self.sel_area;
        let inside = col >= r.x && col < r.right() && row >= r.y && row < r.bottom();
        self.sel = inside.then_some(((col, row), (col, row)));
    }

    /// Drag stays inside the selection area: past its edge the selection stops at the edge.
    fn sel_drag(&mut self, col: u16, row: u16) {
        let r = self.sel_area;
        if r.is_empty() {
            return;
        }
        if let Some((_, end)) = &mut self.sel {
            *end = (col.clamp(r.x, r.right() - 1), row.clamp(r.y, r.bottom() - 1));
        }
    }

    /// Selected cells in reading order, empty when nothing was dragged.
    fn sel_range(&self) -> Option<((u16, u16), (u16, u16))> {
        let (a, b) = self.sel?;
        let (s, e) = if (a.1, a.0) <= (b.1, b.0) { (a, b) } else { (b, a) };
        (s != e).then_some((s, e))
    }

    /// Mouse release: copy the selection to the clipboard via OSC 52 (the local terminal's clipboard, also over SSH).
    fn sel_end(&mut self) {
        let Some(text) = self.sel_text() else { return };
        let b64 = base64::engine::general_purpose::STANDARD.encode(text);
        let mut out = std::io::stdout();
        let _ = std::io::Write::write_all(&mut out, format!("\x1b]52;c;{b64}\x07").as_bytes());
        let _ = std::io::Write::flush(&mut out);
    }

    /// Per selected row: (row, first col, last col), limited to `sel_area`.
    fn sel_rows(&self) -> Vec<(u16, u16, u16)> {
        let Some((s, e)) = self.sel_range() else { return vec![] };
        let r = self.sel_area;
        (s.1..=e.1)
            .map(|row| {
                let x0 = if row == s.1 { s.0 } else { r.x };
                let x1 = if row == e.1 { e.0 } else { r.right().saturating_sub(1) };
                (row, x0, x1)
            })
            .collect()
    }

    fn sel_text(&self) -> Option<String> {
        let rows = self.sel_rows();
        if rows.is_empty() {
            return None;
        }
        let lines: Vec<String> = rows
            .iter()
            .map(|&(row, x0, x1)| {
                let line: String = (x0..=x1).filter_map(|x| self.frame.cell((x, row))).filter(|c| c.underline_color != markdown::NO_SELECT).map(|c| c.symbol()).collect();
                line.trim_end().to_string()
            })
            .collect();
        Some(lines.join("\n"))
    }

    /// Reverse the selected cells so the selection shows. Table cells stay unselected.
    fn sel_highlight(&self, buf: &mut Buffer) {
        for (row, x0, x1) in self.sel_rows() {
            for x in x0..=x1 {
                if let Some(c) = buf.cell_mut((x, row)).filter(|c| c.underline_color != markdown::NO_SELECT) {
                    c.set_style(Style::default().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }

    /// Mouse moved: the tab under it scrolls its title from the start.
    fn on_hover(&mut self, col: u16, row: u16) {
        let hover = self.strip.tabs.iter().position(|(tab, _)| tabs::hit(*tab, col, row));
        if hover != self.hover {
            self.hover = hover;
            self.marquee = 0;
        }
    }

    /// Mouse on the tab strip: × closes, the title switches, + opens a new session.
    fn on_click(&mut self, col: u16, row: u16) {
        if self.overlay.is_some() {
            return;
        }
        let (r, skip, w) = self.input_hit;
        if col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height && !self.input.text.is_empty() {
            self.input.cur = self.input.at(w, skip + (row - r.y) as usize, (col - r.x) as usize);
            return;
        }
        let mut hit = None;
        for (i, (tab, close)) in self.strip.tabs.iter().enumerate() {
            if tabs::hit(*close, col, row) {
                hit = Some((i, true));
                break;
            }
            if tabs::hit(*tab, col, row) {
                hit = Some((i, false));
                break;
            }
        }
        match hit {
            Some((i, true)) => self.close_at(i),
            Some((i, false)) => self.switch_to(i),
            None if tabs::hit(self.strip.new, col, row) => self.new_tab(),
            None => {}
        }
    }

    /// Feed one tab's pushes into the shared channel the main loop reads.
    fn forward(&self, id: u64, mut rx: UnboundedReceiver<Push>) {
        let tx = self.tab_tx.clone();
        tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                if tx.send((id, Some(p))).is_err() {
                    return;
                }
            }
            let _ = tx.send((id, None));
        });
    }

    fn on_tab_push(&mut self, id: u64, p: Option<Push>) {
        // Link and tab changes concern the whole TUI, whichever tab's connection carried them.
        match p {
            Some(Push::Tabs { cwd, opened, closed }) => return self.on_tabs(&cwd, opened, closed),
            Some(Push::Remote { state, url }) => return self.on_remote(state, &url),
            _ => {}
        }
        let Some(i) = self.tabs.iter().position(|t| t.id == id) else { return };
        match (i == self.cur, p) {
            (true, Some(p)) => self.on_push(p),
            (true, None) => {
                self.push(Item::Error("connection to the daemon lost".into()));
                self.running = false;
            }
            (false, Some(p)) => {
                self.tabs[i].watch(&p);
                self.tabs[i].backlog.push(p);
            }
            (false, None) => self.tabs[i].status = TabStatus::Error,
        }
    }

    /// Replace the whole view with a session the daemon just handed us (first attach, /new, /resume, reconnect).
    fn on_snapshot(&mut self, snap: Snapshot) {
        *self.app.session.lock().unwrap() = Session::mirror(snap.entries);
        self.apply_meta(&snap.agent, snap.model, snap.effort);
        self.todos = snap.todos;
        self.jobs = snap.jobs;
        self.ssh = snap.ssh;
        self.asks.clear();
        self.overlay = None;
        self.running = false;
        self.rebuild();
        if snap.running {
            self.begin_run();
            for e in snap.replay {
                self.on_event(e);
            }
        }
    }

    fn apply_meta(&mut self, agent: &str, model: String, effort: String) {
        if let Some(a) = agents::find(&self.app.rt.agents, agent).cloned() {
            if a.name != self.app.turn.agent.name {
                self.notify(format!("agent: {}", agent_label(&a)));
            }
            self.app.turn.agent = a;
        }
        self.app.turn.model = model;
        self.app.turn.effort = effort;
        self.ctx_tokens = crate::compact::current_tokens(&self.app.session.lock().unwrap());
        self.invalidate_all();
    }

    fn on_remote(&mut self, state: RemoteState, url: &str) {
        let had_code = matches!(self.remote_state, RemoteState::Code { .. });
        self.remote_state = state.clone();
        let is_code_overlay = matches!(&self.overlay, Some(Overlay::Report(t, _)) if t == "θ remote");
        if is_code_overlay && !matches!(state, RemoteState::Code { .. }) {
            self.overlay = None;
        }
        if !self.remote_asked {
            return;
        }
        match state {
            RemoteState::Code { code, expires_in } => {
                let lines = vec![
                    format!("  code   {}", code),
                    format!("  open   {}/link", url.trim_end_matches('/')),
                    String::new(),
                    "  press c to copy the code".into(),
                    format!("  Valid for {} min. Anyone with the code can run tools on this machine:", expires_in / 60),
                    "  only enter it yourself. `/remote stop` cuts the link.".into(),
                ];
                self.overlay = Some(Overlay::Report("θ remote".into(), lines));
            }
            RemoteState::Linked => {
                self.remote_asked = false;
                self.notify("remote: browser connected (/remote stop to disconnect)");
            }
            RemoteState::Off => {
                self.remote_asked = false;
                self.notify("remote: off");
            }
            RemoteState::Offline(e) if !had_code => self.notify(format!("remote: link lost ({e}), retrying")),
            RemoteState::Error(e) => {
                self.remote_asked = false;
                self.notify(format!("remote: {e}"));
            }
            RemoteState::Offline(_) | RemoteState::Connecting => {}
        }
    }

    fn on_push(&mut self, p: Push) {
        match p {
            Push::Snapshot(s) => self.on_snapshot(*s),
            Push::Event(e) => self.on_event(e),
            Push::Entry(e) => {
                let moved = matches!(e.kind, Kind::Leaf { .. });
                self.app.session.lock().unwrap().apply(e);
                if moved && !self.running {
                    self.rebuild();
                }
            }
            Push::Meta { agent, model, effort } => self.apply_meta(&agent, model, effort),
            Push::Notice(n) => self.push(Item::Info(n)),
            Push::Err(e) => self.push(Item::Error(e)),
            Push::Busy(_) => {}
            Push::Remote { state, url } => self.on_remote(state, &url),
            Push::Overview(_) | Push::Tabs { .. } => {} // web clients / handled in on_tab_push
            Push::Jobs(l) => self.jobs = l,
            Push::Ssh(i) => self.ssh = i,
            Push::Dir { cwd, mut entries } => {
                entries.sort_by_key(|e| (!e.ends_with('/'), e.to_lowercase()));
                entries.insert(0, "..".into());
                let sel = if entries.len() > 1 { 1 } else { 0 };
                self.overlay = Some(Overlay::Dir(DirView { cwd, entries, sel }));
            }
            Push::JobOutput { id, text } => {
                if let Some(Overlay::Jobs(v)) = &mut self.overlay
                    && v.view.as_deref() == Some(&id)
                {
                    v.text = text;
                }
            }
        }
    }
}

pub async fn run(rt: crate::agent::Runtime, remote: Remote, pushes: UnboundedReceiver<Push>, snap: Snapshot, initial: Option<String>) -> Result<()> {
    let (login_tx, mut login_rx) = unbounded_channel();
    let (tab_tx, mut tab_rx) = unbounded_channel();
    let (opened_tx, mut opened_rx) = unbounded_channel();
    let (shell_tx, mut shell_rx) = unbounded_channel::<String>();
    let turn = crate::make_turn(&rt, Some(&snap.agent), Some(&snap.model))?;
    let app = App { rt, session: Arc::new(Mutex::new(Session::mirror(vec![]))), turn };
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
        remote,
        running: false,
        queued: vec![],
        images: vec![],
        esc_at: None,
        leader: false,
        interrupted: false,
        restoring: 0,
        opening: vec![],
        started: Instant::now(),
        elapsed: Duration::ZERO,
        counts: Counts::default(),
        status: String::new(),
        overlay: None,
        todos: vec![],
        jobs: snap.jobs.clone(),
        remote_state: RemoteState::Off,
        remote_asked: false,
        ssh: None,
        cost: 0.0,
        ctx_tokens: 0,
        quit_armed: None,
        notice: None,
        spin: 0,
        verb: VERBS[0],
        out_tokens: 0,
        stream_chars: 0,
        asks: VecDeque::new(),
        login_tx,
        cmd_sel: 0,
        mentions: std::cell::RefCell::new((String::new(), Instant::now(), Vec::new())),
        settings_back: None,
        quit: false,
        tabs: vec![Tab::new(0)],
        cur: 0,
        next_tab_id: 1,
        tab_tx,
        opened_tx,
        shell_tx,
        strip: tabs::Strip::default(),
        hover: None,
        marquee: 0,
        input_hit: (Rect::default(), 0, 0),
        sidebar: Rect::default(),
        drag: None,
        sel: None,
        frame: Buffer::default(),
        sel_area: Rect::default(),
    };
    ui.forward(0, pushes);
    let first = snap.session_id.clone();
    ui.on_snapshot(snap);
    ui.mark_seen();
    ui.restore(first);
    // Prompt history from previous sessions in this directory.
    if let Some(i) = session::list(&ui.app.rt.cwd).first()
        && let Ok(s) = Session::open(&i.path) {
            ui.history = s.context().iter().filter(|m| m.role == Role::User && !m.text().is_empty()).map(|m| m.text()).collect();
        }
    let mut term = setup();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    if let Some(p) = initial {
        ui.send(p, vec![]);
    }
    loop {
        let done = term.draw(|f| {
            ui.draw(f);
            ui.sel_highlight(f.buffer_mut());
        })?;
        ui.frame = done.buffer.clone();
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(CEvent::Key(k))) => {
                    ui.sel = None;
                    ui.on_key(k);
                }
                Some(Ok(CEvent::Paste(s))) => {
                    let s = normalize_paste(&s);
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
                    MouseEventKind::Moved => ui.on_hover(m.column, m.row),
                    MouseEventKind::Down(MouseButton::Left) if ui.on_divider(m.column, m.row) => ui.drag = Some(ui.sidebar.width),
                    MouseEventKind::Drag(MouseButton::Left) if ui.drag.is_some() => ui.drag = Some(m.column.saturating_sub(ui.sidebar.x) + 1),
                    MouseEventKind::Drag(MouseButton::Left) => ui.sel_drag(m.column, m.row),
                    MouseEventKind::Up(MouseButton::Left) if ui.drag.is_some() => ui.end_drag(),
                    MouseEventKind::Up(MouseButton::Left) => ui.sel_end(),
                    MouseEventKind::Down(MouseButton::Left) => {
                        ui.sel_start(m.column, m.row);
                        ui.on_click(m.column, m.row);
                    }
                    _ => {}
                },
                Some(Ok(CEvent::Resize(..))) => ui.invalidate_all(),
                Some(Err(_)) | None => break,
                _ => {}
            },
            Some((id, p)) = tab_rx.recv() => {
                ui.on_tab_push(id, p);
                // Drain whatever else is queued before redrawing.
                while let Ok((id, p)) = tab_rx.try_recv() {
                    ui.on_tab_push(id, p);
                }
            },
            Some(o) = opened_rx.recv() => ui.tab_opened(o),
            Some(text) = shell_rx.recv() => ui.push(Item::Assistant { text, thinking: String::new(), done: true, agent: ui.app.turn.agent.name.clone() }),
            Some(m) = login_rx.recv() => ui.on_login(m),
            _ = tick.tick() => {
                if ui.running || ui.tabs.iter().any(|t| t.status == TabStatus::Working) {
                    ui.spin += 1;
                }
                ui.marquee += 1;
                ui.poll_job();
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
    ui.save_tabs();
    teardown();
    let s = ui.app.session.lock().unwrap();
    if s.has_messages() {
        let live = if ui.running { " · still running in the background" } else { "" };
        println!("θ session saved{live} · resume with: theta -r {}", s.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn mentions_navigate_to_a_file() {
        let root = std::env::temp_dir().join(format!("theta-mention-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/tui")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/tui/mod.rs"), "x").unwrap();
        std::fs::write(root.join("src/main.rs"), "x").unwrap();
        std::fs::write(root.join("README.md"), "x").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("target/junk.rs"), "x").unwrap();
        let names = |q: &str| super::mention_entries(&root, q).into_iter().map(|(p, _)| p).collect::<Vec<_>>();
        // folders first, ignored and hidden entries left out
        assert_eq!(names(""), ["@src/", "@README.md"]);
        assert_eq!(names("sr"), ["@src/"]);
        assert_eq!(names("src/"), ["@src/tui/", "@src/main.rs"]);
        assert_eq!(names("src/t"), ["@src/tui/"]);
        // no folder typed: also found deeper
        assert_eq!(names("mod"), ["@src/tui/mod.rs"]);
        assert!(names("junk").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fit_cuts_by_width() {
        assert_eq!(super::fit("abcdef", 6), "abcdef");
        assert_eq!(super::fit("abcdef", 4), "abc…");
    }

    #[test]
    fn editor_word_moves_and_click() {
        let mut e = super::Editor { text: "ab cd  ef".into(), cur: 0 };
        e.word_right();
        assert_eq!(e.cur, 2);
        e.word_right();
        assert_eq!(e.cur, 5);
        e.word_right();
        assert_eq!(e.cur, 9);
        e.word_left();
        assert_eq!(e.cur, 7);
        e.word_left();
        assert_eq!(e.cur, 3);
        e.word_left();
        assert_eq!(e.cur, 0);
        // click on col 4 of a 3-wide-wrapped line lands in the second visual row
        let e = super::Editor { text: "abcdefgh".into(), cur: 0 };
        assert_eq!(e.at(4, 0, 2), 2);
        assert_eq!(e.at(4, 1, 1), 5);
    }

    #[test]
    fn agent_colors_are_stable_and_distinct() {
        use super::theme::color_for;
        assert_eq!(color_for("Build"), color_for("build"));
        assert_ne!(color_for("Build"), color_for("Plan"));
        assert!((16..232).contains(&color_for("Review")));
    }

    #[test]
    fn paste_keeps_newlines_drops_control_chars() {
        use super::normalize_paste;
        assert_eq!(normalize_paste("a\r\nb\rc\td\x1b[0m"), "a\nb\nc    d[0m");
    }
}
