//! Conversation tree view: one row per user prompt; branches drawn only where the conversation forks.

use crate::session::{Entry, Kind, Session};
use crate::types::{Block, Role};
use std::collections::HashSet;

pub struct Row {
    pub id: String,
    pub parent: Option<String>,
    pub text: String,
    pub reply: String,
    pub prefix: String,
    pub on_path: bool,
    pub current: bool,
    pub compaction: bool,
}

pub struct TreeView {
    pub rows: Vec<Row>,
    pub sel: usize,
}

/// A node shown in the tree: a user text message or a compaction.
fn visible(e: &Entry) -> bool {
    match &e.kind {
        Kind::Msg { msg, .. } => msg.role == Role::User && msg.content.iter().any(|b| matches!(b, Block::Text { .. })),
        Kind::Compaction { .. } => true,
        _ => false,
    }
}

fn turns<'a>(s: &'a Session, from: Option<&str>) -> Vec<&'a Entry> {
    let mut out = Vec::new();
    for c in s.children(from) {
        if visible(c) {
            out.push(c);
        } else {
            out.extend(turns(s, Some(&c.id)));
        }
    }
    out.sort_by_key(|e| e.ts);
    out
}

/// Last entry of the turn started at `id` (following the newest continuation).
pub fn turn_end(s: &Session, id: &str) -> Option<String> {
    let mut cur = id.to_string();
    loop {
        let next = s.children(Some(&cur)).into_iter().filter(|e| !visible(e)).max_by_key(|e| e.ts).map(|e| e.id.clone());
        match next {
            Some(n) => cur = n,
            None => return Some(cur),
        }
    }
}

fn reply_preview(s: &Session, id: &str) -> String {
    let end = turn_end(s, id);
    let mut out = String::new();
    for e in s.path_to(end.as_deref()).iter().rev() {
        if e.id == id {
            break;
        }
        if let Kind::Msg { msg, .. } = &e.kind
            && msg.role == Role::Assistant {
                let t = msg.text();
                if !t.trim().is_empty() {
                    out = t.split_whitespace().collect::<Vec<_>>().join(" ").replace(['*', '`', '#'], "");
                    break;
                }
            }
    }
    out
}

impl TreeView {
    pub fn new(s: &Session) -> TreeView {
        let path: HashSet<String> = s.path_to(s.leaf.as_deref()).iter().map(|e| e.id.clone()).collect();
        let current = s.path_to(s.leaf.as_deref()).iter().rev().find(|e| visible(e)).map(|e| e.id.clone());
        let mut rows = Vec::new();
        walk(s, turns(s, None), String::new(), &path, current.as_deref(), &mut rows);
        let sel = rows.iter().position(|r| r.current).unwrap_or(rows.len().saturating_sub(1));
        TreeView { rows, sel }
    }
}

fn walk(s: &Session, nodes: Vec<&Entry>, prefix: String, path: &HashSet<String>, current: Option<&str>, rows: &mut Vec<Row>) {
    let branching = nodes.len() > 1;
    let n = nodes.len();
    for (i, e) in nodes.into_iter().enumerate() {
        let last = i + 1 == n;
        let connector = if branching { if last { "└─ " } else { "├─ " } } else { "" };
        let (text, compaction) = match &e.kind {
            Kind::Msg { msg, .. } => (msg.text(), false),
            Kind::Compaction { tokens_before, .. } => (format!("compacted {}k tokens", tokens_before / 1000), true),
            _ => (String::new(), false),
        };
        rows.push(Row {
            id: e.id.clone(),
            parent: e.parent.clone(),
            reply: if compaction { String::new() } else { reply_preview(s, &e.id) },
            text,
            prefix: format!("{prefix}{connector}"),
            on_path: path.contains(&e.id),
            current: current == Some(e.id.as_str()),
            compaction,
        });
        let child_prefix = if branching { format!("{prefix}{}", if last { "   " } else { "│  " }) } else { prefix.clone() };
        walk(s, turns(s, Some(&e.id)), child_prefix, path, current, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Msg;

    #[test]
    fn branches_render() {
        let tmp = std::env::temp_dir().join(format!("theta-tree-{}", rand::random::<u32>()));
        unsafe { std::env::set_var("THETA_HOME", &tmp) };
        let mut s = Session::new(std::path::Path::new("/tmp/p"));
        let a = s.add_msg(Msg::user("a"), None).unwrap();
        let r = s.add_msg(Msg { role: Role::Assistant, content: vec![Block::Text { text: "ra".into() }], model: None }, None).unwrap();
        s.add_msg(Msg::user("b"), None).unwrap();
        s.set_leaf(Some(r.clone())).unwrap();
        s.add_msg(Msg::user("c"), None).unwrap();
        let t = TreeView::new(&s);
        let shown: Vec<String> = t.rows.iter().map(|r| format!("{}{}", r.prefix, r.text)).collect();
        assert_eq!(shown, vec!["a", "├─ b", "└─ c"]);
        assert_eq!(t.rows[0].reply, "ra");
        assert!(t.rows[2].current);
        assert_eq!(turn_end(&s, &a), Some(r));
        let _ = std::fs::remove_dir_all(tmp);
    }
}
