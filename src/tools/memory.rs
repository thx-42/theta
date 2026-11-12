//! Project-local notes, loaded only by explicit tool calls. One locked, atomically replaced store.
use super::{ToolOut, n, s};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Note {
    id: String,
    title: String,
    content: String,
    file: String,
    symbol: String,
    updated: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    notes: Vec<Note>,
}

fn field(args: &Value, key: &str, max: usize) -> Result<String> {
    let value = match args.get(key) {
        Some(v) => v.as_str().with_context(|| format!("{key} must be a string"))?,
        None => "",
    }.trim();
    ensure!(value.chars().count() <= max, "{key} exceeds {max} characters");
    Ok(value.into())
}

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn score(note: &Note, query: &[String]) -> usize {
    let text = format!("{} {} {} {}", note.title, note.content, note.file, note.symbol).to_lowercase();
    query.iter().filter(|word| text.contains(word.as_str())).count()
}

fn search(store: &Store, args: &Value) -> Result<ToolOut> {
    let query = field(args, "query", 500)?;
    let terms = tokens(&query);
    ensure!(!terms.is_empty(), "query must contain search words");
    let mut hits: Vec<_> = store.notes.iter().map(|note| (score(note, &terms), note)).filter(|(rank, _)| *rank > 0).collect();
    hits.sort_by(|(a, x), (b, y)| b.cmp(a).then(y.updated.cmp(&x.updated)).then(x.id.cmp(&y.id)));
    let total = hits.len();
    let limit = n(args, "limit").unwrap_or(5).clamp(1, 20) as usize;
    Ok(ToolOut::ok(serde_json::to_string(&json!({"matches": total, "notes": hits.into_iter().take(limit).map(|(_, note)| note).collect::<Vec<_>>()}))?))
}

fn write(store: &mut Store, args: &Value) -> Result<ToolOut> {
    if args.get("id").is_some() { ensure!(s(args, "id").is_some(), "id must be a string"); }
    let existing = if let Some(id) = s(args, "id") {
        Some(store.notes.iter().position(|note| note.id == id).context("unknown memory id")?)
    } else { None };
    let title = field(args, "title", 160)?;
    let content = field(args, "content", 2000)?;
    let file = field(args, "file", 500)?;
    let symbol = field(args, "symbol", 200)?;
    let mut note = existing.map(|i| store.notes[i].clone()).unwrap_or_else(|| Note {
        id: format!("{:032x}", rand::random::<u128>()),
        title: String::new(), content: String::new(), file: String::new(), symbol: String::new(), updated: 0,
    });
    for (key, value, target) in [("title", title, &mut note.title), ("content", content, &mut note.content), ("file", file, &mut note.file), ("symbol", symbol, &mut note.symbol)] {
        if args.get(key).is_some() { *target = value; }
    }
    ensure!(!note.title.is_empty() && !note.content.is_empty(), "title and content are required and must not be empty");
    // Repeated discoveries should not grow the store with identical notes.
    if existing.is_none() && let Some(same) = store.notes.iter().find(|n| n.title == note.title && n.content == note.content && n.file == note.file && n.symbol == note.symbol) {
        return Ok(ToolOut::ok(format!("already remembered: {}", same.id)));
    }
    note.updated = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let out = ToolOut::ok(format!("remembered: {}", note.id));
    if let Some(i) = existing { store.notes[i] = note; } else { store.notes.push(note); }
    Ok(out)
}

fn delete(store: &mut Store, args: &Value) -> Result<ToolOut> {
    let id = s(args, "id").context("id required")?;
    let i = store.notes.iter().position(|note| note.id == id).context("unknown memory id")?;
    store.notes.remove(i);
    Ok(ToolOut::ok(format!("forgotten: {id}")))
}

/// The management command remains usable while agent access is disabled.
fn manage(store: &mut Store, command: &str) -> Result<ToolOut> {
    let (action, rest) = command.trim().split_once(' ').unwrap_or((command.trim(), ""));
    match action {
        "on" | "off" => {
            store.disabled = action == "off";
            Ok(ToolOut::ok(if store.disabled { "Memory disabled; notes preserved." } else { "Memory enabled." }))
        }
        "search" => search(store, &json!({"query": rest})),
        "show" => Ok(ToolOut::ok(serde_json::to_string_pretty(store.notes.iter().find(|note| note.id == rest.trim()).context("unknown memory id")?)?)),
        "delete" => delete(store, &json!({"id": rest.trim()})),
        "edit" => {
            let (id, content) = rest.split_once(' ').context("usage: /memories edit <id> <new content>")?;
            write(store, &json!({"id": id, "content": content}))
        }
        "add" => write(store, &json!({"title": rest.chars().take(80).collect::<String>(), "content": rest})),
        "" | "list" => {
            let page = if rest.trim().is_empty() { 1 } else { rest.trim().parse::<usize>().context("usage: /memories list <page>")? };
            let start = page.saturating_sub(1).saturating_mul(10);
            let mut text = format!("Memory {} · {} notes · page {page}\n", if store.disabled { "off" } else { "on" }, store.notes.len());
            for note in store.notes.iter().skip(start).take(10) {
                text.push_str(&format!("\n{}  {}\n  {} {}", note.id, note.title, note.file, note.symbol));
            }
            text.push_str("\n\n/memories list <page> | search <words> | show <id>\n/memories add <note> | edit <id> <new content> | delete <id>\n/memories on | off (preserves notes)");
            Ok(ToolOut::ok(text))
        }
        _ => bail!("unknown action; use /memories for help"),
    }
}

pub fn run(name: &str, args: &Value, cwd: &Path) -> Result<ToolOut> {
    let dir = crate::config::project_root(cwd).join(".theta/memory");
    fs::create_dir_all(&dir).context("create project memory directory")?;
    // OS lock works across sessions/processes and is released even on failure or crash.
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("store.lock"))?;
    lock.lock()?;
    let path = dir.join("notes.json");
    let mut store: Store = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid memory store; repair notes.json before writing")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(e) => return Err(e.into()),
    };
    if name != "memory_manage" && store.disabled {
        bail!("Memory is disabled for this project. The user can enable it with /memories on.");
    }
    let out = match name {
        "memory_search" => search(&store, args)?,
        "memory_write" => write(&mut store, args)?,
        "memory_delete" => delete(&mut store, args)?,
        "memory_manage" => manage(&mut store, s(args, "command").unwrap_or(""))?,
        _ => bail!("unknown memory tool"),
    };
    if matches!(name, "memory_write" | "memory_delete") || (name == "memory_manage" && matches!(s(args, "command").unwrap_or("").split_whitespace().next(), Some("on" | "off" | "add" | "edit" | "delete"))) {
        let temp = dir.join("notes.json.tmp");
        let mut file = fs::File::create(&temp)?;
        use std::io::Write;
        file.write_all(&serde_json::to_vec_pretty(&store)?)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_search_update_disable_and_isolation() {
        let root = std::env::temp_dir().join(format!("theta-memory-{:x}", rand::random::<u64>()));
        let other = root.join("other");
        let project = root.join("project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::create_dir_all(project.join("src")).unwrap();
        fs::create_dir_all(&other).unwrap();
        let call = |name, args: Value| run(name, &args, &project).unwrap().content;
        call("memory_write", json!({"title":"Authentication", "content":"Réutiliser la validation des jetons", "file":"src/auth.rs", "symbol":"validate_token"}));
        let found: Value = serde_json::from_str(&run("memory_search", &json!({"query":"VALIDATE jetons"}), &project.join("src")).unwrap().content).unwrap();
        assert_eq!(found["matches"], 1);
        let id = found["notes"][0]["id"].as_str().unwrap();
        call("memory_write", json!({"id":id, "content":"Validate and refresh sessions"}));
        assert!(call("memory_manage", json!({"command":format!("show {id}")})).contains("refresh"));
        assert_eq!(serde_json::from_str::<Value>(&run("memory_search", &json!({"query":"jetons"}), &other).unwrap().content).unwrap()["matches"], 0);
        call("memory_manage", json!({"command":"off"}));
        for name in ["memory_search", "memory_write", "memory_delete"] {
            assert!(run(name, &json!({"query":"auth", "id":id}), &project).is_err());
        }
        assert!(call("memory_manage", json!({"command":""})).contains("1 notes"));
        call("memory_manage", json!({"command":"on"}));
        call("memory_delete", json!({"id":id}));
        assert!(call("memory_manage", json!({"command":""})).contains("0 notes"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_writes_deduplication_and_corruption() {
        let root = std::env::temp_dir().join(format!("theta-memory-{:x}", rand::random::<u64>()));
        fs::create_dir_all(root.join(".git")).unwrap();
        std::thread::scope(|scope| {
            for i in 0..8 {
                let root = &root;
                scope.spawn(move || { run("memory_write", &json!({"title":format!("note {i}"), "content":"useful finding"}), root).unwrap(); });
            }
        });
        run("memory_write", &json!({"title":"note 0", "content":"useful finding"}), &root).unwrap();
        assert!(run("memory_manage", &json!({"command":""}), &root).unwrap().content.contains("8 notes"));
        assert!(run("memory_write", &json!({"title":"bad", "content":"x".repeat(2001)}), &root).is_err());
        let path = root.join(".theta/memory/notes.json");
        fs::write(&path, "broken").unwrap();
        assert!(run("memory_write", &json!({"title":"test", "content":"test"}), &root).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "broken");
        fs::remove_dir_all(root).unwrap();
    }
}
