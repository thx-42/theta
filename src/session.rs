//! Sessions: append-only JSONL tree. Each entry has a parent; the leaf marks the active branch.
//! Forking = moving the leaf to an earlier entry and continuing from there.

use crate::config;
use crate::types::{Block, Msg, Role, Usage};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Session { cwd: String },
    Msg { msg: Msg, #[serde(default, skip_serializing_if = "Option::is_none")] usage: Option<Usage> },
    /// Summary replacing everything on the path before `first_kept`.
    Compaction { summary: String, first_kept: String, tokens_before: u64 },
    Title { title: String },
    /// Active branch switched to `target` (empty = root).
    Leaf { target: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub ts: u64,
    #[serde(flatten)]
    pub kind: Kind,
}

pub struct Session {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub entries: Vec<Entry>,
    index: HashMap<String, usize>,
    pub leaf: Option<String>,
    written: bool,
    /// Called with every entry appended after construction (the daemon broadcasts them to attached clients).
    sink: Option<Box<dyn Fn(&Entry) + Send>>,
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn new_id() -> String {
    format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff)
}

pub fn dir_for(cwd: &Path) -> PathBuf {
    let slug: String = cwd.display().to_string().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    config::home().join("sessions").join(slug.trim_matches('-'))
}

pub struct Info {
    pub path: PathBuf,
    pub title: String,
    pub updated: u64,
    pub messages: usize,
}

/// Open tabs and unread results of one project, kept by the daemon in `tabs.json` next to its sessions.
#[derive(Default, Serialize, Deserialize)]
pub struct TabsFile {
    /// Session ids of the open tabs, in tab order.
    #[serde(default)]
    pub open: Vec<String>,
    /// Finished results nobody looked at yet: session id -> done | error | asked.
    #[serde(default)]
    pub unseen: BTreeMap<String, String>,
}

pub fn load_tabs(cwd: &Path) -> TabsFile {
    std::fs::read_to_string(dir_for(cwd).join("tabs.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_tabs(cwd: &Path, f: &TabsFile) {
    let dir = dir_for(cwd);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(s) = serde_json::to_string(f) {
        let _ = std::fs::write(dir.join("tabs.json"), s);
    }
}

/// Tabs that were open last time and whose session file still exists.
pub fn saved_open(cwd: &Path) -> Vec<String> {
    let dir = dir_for(cwd);
    load_tabs(cwd).open.into_iter().filter(|i| dir.join(format!("{i}.jsonl")).exists()).collect()
}

pub fn list(cwd: &Path) -> Vec<Info> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir_for(cwd)) else { return out };
    for e in entries.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let mut title = String::new();
        let mut first_user = String::new();
        let mut messages = 0;
        let mut updated = 0;
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<Entry>(line) else { continue };
            updated = updated.max(entry.ts);
            match entry.kind {
                Kind::Title { title: t } => title = t,
                Kind::Msg { msg, .. } => {
                    messages += 1;
                    if first_user.is_empty() && msg.role == Role::User {
                        first_user = msg.text().lines().next().unwrap_or("").chars().take(80).collect();
                    }
                }
                _ => {}
            }
        }
        if messages == 0 {
            continue;
        }
        out.push(Info { path, title: if title.is_empty() { first_user } else { title }, updated, messages });
    }
    out.sort_by(|a, b| b.updated.cmp(&a.updated));
    out
}

/// Directories that have sessions, most recently used first. Read from each session file's first entry,
/// since the directory slug is lossy.
pub fn projects() -> Vec<PathBuf> {
    let Ok(dirs) = std::fs::read_dir(config::home().join("sessions")) else { return vec![] };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for d in dirs.filter_map(|e| e.ok()) {
        let Ok(files) = std::fs::read_dir(d.path()) else { continue };
        let newest = files.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "jsonl")).filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).max();
        let Some((mtime, file)) = newest else { continue };
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let cwd = text.lines().next().and_then(|l| serde_json::from_str::<Entry>(l).ok()).and_then(|e| match e.kind {
            Kind::Session { cwd } => Some(PathBuf::from(cwd)),
            _ => None,
        });
        if let Some(cwd) = cwd.filter(|c| c.is_dir()) {
            found.push((mtime, cwd));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out: Vec<PathBuf> = Vec::new();
    for (_, c) in found {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

impl Session {
    pub fn new(cwd: &Path) -> Session {
        let id = new_id();
        let path = dir_for(cwd).join(format!("{id}.jsonl"));
        let mut s = Session { id: id.clone(), path, cwd: cwd.to_path_buf(), title: None, entries: vec![], index: HashMap::new(), leaf: None, written: false, sink: None };
        s.push_entry(Entry { id, parent: None, ts: now(), kind: Kind::Session { cwd: cwd.display().to_string() } });
        s.leaf = None;
        s
    }

    pub fn open(path: &Path) -> Result<Session> {
        let text = std::fs::read_to_string(path)?;
        let mut s = Session::blank(path);
        for line in text.lines() {
            let Ok(e) = serde_json::from_str::<Entry>(line) else { continue };
            s.apply(e);
        }
        Ok(s)
    }

    /// Read-only copy of a session held elsewhere (a client mirroring the daemon): never writes to disk.
    pub fn mirror(entries: Vec<Entry>) -> Session {
        let mut s = Session::blank(Path::new(""));
        entries.into_iter().for_each(|e| s.apply(e));
        s
    }

    fn blank(path: &Path) -> Session {
        Session { id: String::new(), path: path.to_path_buf(), cwd: PathBuf::new(), title: None, entries: vec![], index: HashMap::new(), leaf: None, written: true, sink: None }
    }

    /// Replay one stored entry into the in-memory state. Idempotent per entry id.
    pub fn apply(&mut self, e: Entry) {
        if self.index.contains_key(&e.id) {
            return;
        }
        match &e.kind {
            Kind::Session { cwd } => {
                self.id = e.id.clone();
                self.cwd = PathBuf::from(cwd);
            }
            Kind::Title { title } => self.title = Some(title.clone()),
            Kind::Leaf { target } => self.leaf = (!target.is_empty()).then(|| target.clone()),
            _ => self.leaf = Some(e.id.clone()),
        }
        self.push_entry(e);
    }

    pub fn set_sink(&mut self, f: impl Fn(&Entry) + Send + 'static) {
        self.sink = Some(Box::new(f));
    }

    fn push_entry(&mut self, e: Entry) {
        self.index.insert(e.id.clone(), self.entries.len());
        self.entries.push(e);
    }

    fn persist(&mut self, e: &Entry) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        if !self.written {
            // Write the header (and anything buffered) on first real content.
            for prev in &self.entries[..self.entries.len() - 1] {
                writeln!(f, "{}", serde_json::to_string(prev)?)?;
            }
            self.written = true;
        }
        writeln!(f, "{}", serde_json::to_string(e)?)?;
        Ok(())
    }

    fn append(&mut self, kind: Kind, moves_leaf: bool) -> Result<String> {
        let id = new_id();
        let e = Entry { id: id.clone(), parent: self.leaf.clone(), ts: now(), kind };
        self.push_entry(e.clone());
        self.persist(&e)?;
        if let Some(f) = &self.sink {
            f(&e);
        }
        if moves_leaf {
            self.leaf = Some(id.clone());
        }
        Ok(id)
    }

    pub fn add_msg(&mut self, msg: Msg, usage: Option<Usage>) -> Result<String> {
        self.append(Kind::Msg { msg, usage }, true)
    }

    pub fn add_compaction(&mut self, summary: String, first_kept: String, tokens_before: u64) -> Result<String> {
        self.append(Kind::Compaction { summary, first_kept, tokens_before }, true)
    }

    pub fn set_title(&mut self, title: String) -> Result<()> {
        self.title = Some(title.clone());
        if self.written {
            let leaf = self.leaf.clone();
            self.append(Kind::Title { title }, false)?;
            self.leaf = leaf;
        }
        Ok(())
    }

    /// Move the active branch. `None` = before the first message.
    pub fn set_leaf(&mut self, target: Option<String>) -> Result<()> {
        self.leaf = target.clone();
        if self.written {
            let keep = self.leaf.clone();
            self.append(Kind::Leaf { target: target.unwrap_or_default() }, false)?;
            self.leaf = keep;
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    /// Entries from root to `leaf` (message and compaction entries only).
    pub fn path_to(&self, leaf: Option<&str>) -> Vec<&Entry> {
        let mut out = Vec::new();
        let mut cur = leaf.map(String::from);
        while let Some(id) = cur {
            let Some(e) = self.get(&id) else { break };
            if matches!(e.kind, Kind::Msg { .. } | Kind::Compaction { .. }) {
                out.push(e);
            }
            cur = e.parent.clone();
        }
        out.reverse();
        out
    }

    /// Messages sent to the model for the active branch, with the latest compaction applied.
    pub fn context(&self) -> Vec<Msg> {
        let path = self.path_to(self.leaf.as_deref());
        let last_c = path.iter().rposition(|e| matches!(e.kind, Kind::Compaction { .. }));
        let mut out = Vec::new();
        let mut start = 0;
        if let Some(c) = last_c
            && let Kind::Compaction { summary, first_kept, .. } = &path[c].kind {
                out.push(Msg::user(format!("<context-summary>\nEarlier conversation was compacted. Summary:\n\n{summary}\n</context-summary>")));
                let kept_from = path.iter().position(|e| &e.id == first_kept).unwrap_or(c);
                for e in &path[kept_from..c] {
                    if let Kind::Msg { msg, .. } = &e.kind {
                        out.push(strip_thinking(msg));
                    }
                }
                start = c + 1;
            }
        for e in &path[start..] {
            if let Kind::Msg { msg, .. } = &e.kind {
                out.push(msg.clone());
            }
        }
        out
    }

    /// Last reported usage on the active branch (for context-size display).
    pub fn last_usage(&self) -> Option<Usage> {
        self.path_to(self.leaf.as_deref()).iter().rev().find_map(|e| match &e.kind {
            Kind::Msg { usage: Some(u), .. } => Some(u.clone()),
            Kind::Compaction { .. } => Some(Usage::default()),
            _ => None,
        })
    }

    pub fn children(&self, id: Option<&str>) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| e.parent.as_deref() == id && matches!(e.kind, Kind::Msg { .. } | Kind::Compaction { .. }))
            .collect()
    }

    pub fn has_messages(&self) -> bool {
        self.entries.iter().any(|e| matches!(e.kind, Kind::Msg { .. }))
    }
}

/// Kept-after-compaction messages lose their thinking blocks: their prefix changed, so signatures no longer verify.
fn strip_thinking(m: &Msg) -> Msg {
    let mut m = m.clone();
    m.content.retain(|b| !matches!(b, Block::Thinking { .. }));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_and_fork() {
        let tmp = std::env::temp_dir().join(format!("theta-test-{}", rand::random::<u32>()));
        let _g = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("THETA_HOME", &tmp) };
        let mut s = Session::new(Path::new("/tmp/proj"));
        let u1 = s.add_msg(Msg::user("one"), None).unwrap();
        s.add_msg(Msg { role: Role::Assistant, content: vec![Block::Text { text: "r1".into() }], model: None }, None).unwrap();
        s.add_msg(Msg::user("two"), None).unwrap();
        assert_eq!(s.context().len(), 3);
        // Fork: go back to after u1's reply... here to u1 itself and branch.
        s.set_leaf(Some(u1.clone())).unwrap();
        s.add_msg(Msg::user("alt"), None).unwrap();
        let ctx = s.context();
        assert_eq!(ctx.iter().map(|m| m.text()).collect::<Vec<_>>(), vec!["one", "alt"]);
        assert_eq!(s.children(Some(&u1)).len(), 2);
        // Reload keeps the active branch.
        let r = Session::open(&s.path).unwrap();
        assert_eq!(r.context().len(), 2);
        // Compaction keeps first_kept onwards.
        let mut s2 = r;
        let last = s2.leaf.clone().unwrap();
        s2.add_compaction("sum".into(), last, 100).unwrap();
        let c = s2.context();
        assert!(c[0].text().contains("sum"));
        assert_eq!(c[1].text(), "alt");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn tabs_round_trip_and_skip_missing_sessions() {
        let home = std::env::temp_dir().join(format!("theta-tabs-test-{}", std::process::id()));
        let _g = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("THETA_HOME", &home) };
        let cwd = Path::new("/tmp/theta-tabs-project");
        std::fs::create_dir_all(dir_for(cwd)).unwrap();
        std::fs::write(dir_for(cwd).join("keep.jsonl"), "").unwrap();
        let mut f = TabsFile::default();
        f.open = vec!["keep".into(), "gone".into()];
        f.unseen.insert("keep".into(), "done".into());
        save_tabs(cwd, &f);
        let back = load_tabs(cwd);
        assert_eq!(back.unseen.get("keep").map(String::as_str), Some("done"));
        assert_eq!(saved_open(cwd), vec!["keep".to_string()]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
