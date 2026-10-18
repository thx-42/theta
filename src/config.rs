//! Paths and layered settings: ~/.theta/settings.toml overridden by <project>/.theta/settings.toml.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Tests that set THETA_HOME hold this, so they do not see each other's folder.
#[cfg(test)]
pub static TEST_HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn home() -> PathBuf {
    if let Ok(dir) = std::env::var("THETA_HOME") {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".theta")
}

/// Project root: nearest ancestor holding `.theta` or `.git`, else cwd.
pub fn project_root(cwd: &Path) -> PathBuf {
    let home_dir = dirs::home_dir();
    for dir in cwd.ancestors() {
        if Some(dir) == home_dir.as_deref() {
            break; // ~/.theta is the global dir, not a project marker
        }
        if dir.join(".theta").is_dir() || dir.join(".git").exists() {
            return dir.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `provider/model`
    pub model: String,
    /// Reasoning effort: low | medium | high | xhigh | max
    pub effort: String,
    /// full: show tool calls and thinking; compact: only final answer + one progress line
    pub verbose: String,
    /// Session tab bar: horizontal (top row) | vertical (left sidebar)
    pub tab_orientation: String,
    /// Color theme id: one of /theme's ids
    pub theme: String,
    /// Paint the whole screen in the theme's background instead of the terminal's
    pub theme_bg: bool,
    /// Tab bar hidden (ctrl+x b); keeps tab_orientation for when it comes back
    pub tab_hidden: bool,
    /// Sidebar width in columns (vertical tab bar only)
    pub tab_width: u16,
    /// Default agent name
    pub agent: String,
    pub models: TaskModels,
    /// Per-agent defaults, keyed by agent name (`[agents.Build]`)
    pub agents: BTreeMap<String, AgentSettings>,
    pub compaction: Compaction,
    pub tools: ToolSettings,
    pub web: WebSettings,
    pub remote: RemoteSettings,
    pub diff: DiffSettings,
    pub skills: SkillSettings,
    /// MCP servers, keyed by name (`[mcp.<name>]`)
    pub mcp: BTreeMap<String, McpServer>,
    pub providers: BTreeMap<String, CustomProvider>,
    pub hooks: Hooks,
    /// Linter command per file extension (`[linters]`, e.g. `py = "ruff check {file}"`).
    pub linters: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            model: "anthropic/claude-sonnet-5-5".into(),
            effort: "medium".into(),
            verbose: "full".into(),
            tab_orientation: "horizontal".into(),
            theme: "theta".into(),
            theme_bg: false,
            tab_hidden: false,
            tab_width: 24,
            agent: "Build".into(),
            models: TaskModels::default(),
            agents: BTreeMap::new(),
            compaction: Compaction::default(),
            tools: ToolSettings::default(),
            web: WebSettings::default(),
            remote: RemoteSettings::default(),
            diff: DiffSettings::default(),
            skills: SkillSettings::default(),
            mcp: BTreeMap::new(),
            providers: BTreeMap::new(),
            hooks: Hooks::default(),
            linters: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// `[agents.<name>]` entry, matched case-insensitively like agent names.
    pub fn agent(&self, name: &str) -> Option<&AgentSettings> {
        self.agents.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }
}

/// Shell commands run around each user message; see `hooks.rs`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Hooks {
    /// Before the message reaches the agent: text on stdin, exit != 0 rejects it, stdout is added as context.
    pub pre_message: Vec<String>,
    /// After the agent finished: its last reply on stdin, output ignored.
    pub post_message: Vec<String>,
}

/// Default model and effort for one agent. Empty string = unset.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// `provider/model`
    pub model: String,
    /// low | medium | high | xhigh | max
    pub effort: String,
}

/// Model per background task. Empty string = use the main model.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskModels {
    pub compaction: String,
    pub title: String,
    pub subagent: String,
    /// Reasoning effort of subagents; empty = the spawning agent's effort
    pub subagent_effort: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub enabled: bool,
    /// Compact when context use exceeds this fraction of the window.
    pub threshold: f64,
    /// Recent tokens kept verbatim after compaction.
    pub keep_recent_tokens: u64,
    /// Absolute ceiling (tokens) for the trigger, so huge windows do not grow unbounded; 0 = none.
    pub max_tokens: u64,
    /// Above this many context tokens, old tool results are shortened (no model call); 0 = never.
    pub prune_tokens: u64,
}

impl Default for Compaction {
    fn default() -> Self {
        Compaction { enabled: true, threshold: 0.8, keep_recent_tokens: 20_000, max_tokens: 150_000, prune_tokens: 30_000 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    /// auto | on | off — route shell commands through `rtk` when installed.
    pub rtk: String,
    /// Default bash timeout (seconds).
    pub bash_timeout: u64,
    /// Max lines a tool result may return before middle-truncation.
    pub max_lines: usize,
    /// Tools disabled globally.
    pub disabled: Vec<String>,
    /// Command run after a Build subagent changed the tree: "" = detect (Cargo.toml, go.mod), "off" = never.
    pub verify: String,
    /// Lines of read/grep/find/ls/bash output Auto itself receives (0 = no cap); larger reads belong to subagents.
    pub auto_max_lines: usize,
    /// Model calls after which a subagent must stop and write a handoff report (0 = never).
    pub checkpoint_steps: usize,
}

impl Default for ToolSettings {
    fn default() -> Self {
        ToolSettings { rtk: "auto".into(), bash_timeout: 120, max_lines: 300, disabled: vec![], verify: String::new(), auto_max_lines: 80, checkpoint_steps: 40 }
    }
}

/// One MCP server: `command` (stdio) or `url` (streamable HTTP). `${VAR}` in values is expanded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServer {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub enabled: bool,
}

impl Default for McpServer {
    fn default() -> Self {
        McpServer { command: String::new(), args: vec![], env: BTreeMap::new(), url: String::new(), headers: BTreeMap::new(), enabled: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillSettings {
    /// Skills injected into every system prompt (also: `auto: true` in the skill file).
    pub auto: Vec<String>,
    /// Skills injected into every subagent's system prompt, on top of `auto` (absent skills are ignored).
    pub subagent: Vec<String>,
}

impl Default for SkillSettings {
    fn default() -> Self {
        Self { auto: vec![], subagent: vec!["caveman".into(), "ponytail".into()] }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSettings {
    /// exa | firecrawl | brave | tavily | duckduckgo
    pub backend: String,
}

impl Default for WebSettings {
    fn default() -> Self {
        WebSettings { backend: "exa".into() }
    }
}

/// `/diff`: what to show and where.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffSettings {
    /// session (since the conversation began) | turn (last message) | unpushed (vs upstream, committed or not)
    pub scope: String,
    /// popup | split (side pane next to the chat)
    pub view: String,
}

impl Default for DiffSettings {
    fn default() -> Self {
        DiffSettings { scope: "session".into(), view: "popup".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    /// theta-server base URL used by `/remote`. `THETA_REMOTE_URL` overrides it.
    pub url: String,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        RemoteSettings { url: "https://theta.sputnk.net".into() }
    }
}

/// User-defined OpenAI/Anthropic-compatible provider, or override of a built-in one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomProvider {
    pub name: String,
    /// openai-chat | openai-responses | anthropic | google
    pub api: String,
    pub base_url: String,
    pub api_key_env: String,
    pub api_key: String,
    pub models: Vec<String>,
    pub context: u64,
}

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

fn read_table(path: &Path) -> Result<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(s) => s.parse::<toml::Table>().with_context(|| format!("parse {}", path.display())),
        Err(_) => Ok(toml::Table::new()),
    }
}

pub fn global_settings_path() -> PathBuf {
    home().join("settings.toml")
}

pub fn load(project: &Path) -> Result<Settings> {
    let mut table = read_table(&global_settings_path())?;
    merge(&mut table, read_table(&project.join(".theta/settings.toml"))?);
    Ok(toml::Value::Table(table).try_into()?)
}

/// Set one dotted key (e.g. `models.title`) in the global settings file, keeping other keys.
pub fn set_global(key: &str, value: toml::Value) -> Result<()> {
    let path = global_settings_path();
    let mut table = read_table(&path)?;
    let mut parts: Vec<&str> = key.split('.').collect();
    let last = parts.pop().unwrap();
    let mut cur = &mut table;
    for p in parts {
        cur = cur
            .entry(p.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .context("settings key is not a table")?;
    }
    cur.insert(last.to_string(), value);
    std::fs::create_dir_all(home())?;
    std::fs::write(&path, toml::to_string_pretty(&table)?)?;
    Ok(())
}

/// sha256 of every DEFAULT_AUTO_SOUL shipped before; add the current one's hash here when it changes.
const SHIPPED_AUTO_SOULS: [&str; 4] = [
    "fd0daae85700f041b464bdb696021a63fa4b77db7a22dab9b24c8ffe1aac5bac",
    "7f22cfc138a69fe436daf1e71aa8988b2d4d91c3be111c889dd82573a8306c5a",
    "889663e1048dda6448eb64ff38d2f7d13dc2b3f86890c8512969fb55864e7c72",
    "9312f8bb14a55537d82d6f9ca5f2190705316226971485a1d9aac052151a1e61",
];

/// Overwrite `path` with `new`, first copying an existing different file to `<path>.bak`.
fn replace_with_backup(path: &Path, new: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|cur| cur != new) {
        std::fs::copy(path, format!("{}.bak", path.display()))?;
    }
    std::fs::write(path, new)?;
    Ok(())
}

/// SOUL.md is written once (yours to edit). SOUL.Auto.md follows the binary while it is still a version theta
/// shipped. `force` rewrites both, keeping a `.bak` of what was there.
pub fn sync_souls(force: bool) -> Result<()> {
    let h = home();
    let soul = h.join("SOUL.md");
    let auto_soul = h.join("SOUL.Auto.md");
    let cur = std::fs::read(&auto_soul).ok();
    let stale = cur.as_ref().is_none_or(|c| SHIPPED_AUTO_SOULS.contains(&crate::update::sha256(c).as_str()));
    for (path, new, replace) in [(&soul, crate::agents::DEFAULT_SOUL, force || !soul.exists()), (&auto_soul, crate::agents::DEFAULT_AUTO_SOUL, force || stale)] {
        let had = std::fs::read_to_string(path).ok();
        if replace && had.as_deref() != Some(new) {
            replace_with_backup(path, new)?;
            if had.is_some() {
                eprintln!("theta: {} updated to the new version{}", path.display(), if force { " (yours kept as .bak)" } else { "" });
            }
        }
    }
    Ok(())
}

/// Create ~/.theta with defaults on first run.
pub fn bootstrap() -> Result<()> {
    let h = home();
    std::fs::create_dir_all(h.join("agents"))?;
    std::fs::create_dir_all(h.join("skills"))?;
    std::fs::create_dir_all(h.join("sessions"))?;
    std::fs::create_dir_all(h.join("cache"))?;
    sync_souls(false)?;
    sync_agents(&h.join("agents"), false);
    let changed = write_docs()?;
    if !changed.is_empty() {
        eprintln!("theta: guides updated in {}: {}", h.join("docs").display(), changed.join(", "));
    }
    let settings = global_settings_path();
    if !settings.exists() {
        std::fs::write(&settings, DEFAULT_SETTINGS)?;
    }
    Ok(())
}

/// Guides shipped in the binary; they describe theta to the agent, so they are always replaced.
pub const DOCS: &[(&str, &str)] = &[
    ("README.md", include_str!("../docs/README.md")),
    ("config.md", include_str!("../docs/config.md")),
    ("mcp.md", include_str!("../docs/mcp.md")),
    ("tools.md", include_str!("../docs/tools.md")),
    ("agents.md", include_str!("../docs/agents.md")),
    ("create-agents.md", include_str!("../docs/create-agents.md")),
    ("skills.md", include_str!("../docs/skills.md")),
    ("remote.md", include_str!("../docs/remote.md")),
    ("commands.md", include_str!("../docs/commands.md")),
];

/// Write the shipped guides to ~/.theta/docs (other files there are left alone).
/// Returns the names of the guides that were missing or different, so the caller can say so.
pub fn write_docs() -> Result<Vec<&'static str>> {
    let dir = home().join("docs");
    std::fs::create_dir_all(&dir)?;
    let mut changed = Vec::new();
    for (name, text) in DOCS {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*text) {
            std::fs::write(&path, text)?;
            changed.push(*name);
        }
    }
    Ok(changed)
}

/// Install the shipped agents. `.shipped` records the hash of the version last written (or offered):
/// an agent still equal to it is replaced silently; an edited one is kept, the new version goes
/// next to it as `<name>.md.new` and its diff is shown once on a terminal.
/// `force` rewrites every agent that differs from the shipped one, keeping yours as `<name>.md.bak`.
pub fn sync_agents(dir: &Path, force: bool) {
    use crate::update::sha256;
    use std::io::IsTerminal;
    let manifest = dir.join(".shipped");
    let mut shipped: std::collections::BTreeMap<String, String> = std::fs::read_to_string(&manifest)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(n, h)| (n.to_string(), h.to_string())))
        .collect();
    let before = shipped.clone();
    for (name, new) in [("Build", crate::agents::DEFAULT_BUILD), ("Plan", crate::agents::DEFAULT_PLAN), ("Auto", crate::agents::DEFAULT_AUTO), ("Explore", crate::agents::DEFAULT_EXPLORE)] {
        let (path, new_hash) = (dir.join(format!("{name}.md")), sha256(new.as_bytes()));
        let Ok(cur) = std::fs::read_to_string(&path) else {
            match std::fs::write(&path, new) {
                Ok(()) => {
                    shipped.insert(name.into(), new_hash);
                    eprintln!("theta: agent {name} missing, restored {}", path.display());
                }
                Err(e) => eprintln!("theta: cannot restore agent {name} in {}: {e}", path.display()),
            }
            continue;
        };
        let old = shipped.get(name).cloned();
        let cur_hash = sha256(cur.as_bytes());
        if cur_hash == new_hash {
            shipped.insert(name.into(), new_hash);
        } else if force {
            match replace_with_backup(&path, new) {
                Ok(()) => {
                    shipped.insert(name.into(), new_hash);
                    let _ = std::fs::remove_file(dir.join(format!("{name}.md.new")));
                    eprintln!("theta: agent {name} rewritten (yours kept as {name}.md.bak)");
                }
                Err(e) => eprintln!("theta: cannot rewrite agent {name}: {e:#}"),
            }
        } else if old.as_deref() == Some(&cur_hash) {
            // untouched since we wrote it
            if std::fs::write(&path, new).is_ok() {
                shipped.insert(name.into(), new_hash);
                eprintln!("theta: agent {name} updated to the new version");
            }
        } else if old.as_deref() != Some(&new_hash) {
            // edited (or unknown origin) and the shipped version changed: propose the diff
            let proposed = dir.join(format!("{name}.md.new"));
            if std::fs::write(&proposed, new).is_ok() && std::io::stderr().is_terminal() {
                eprintln!("theta: agent {name} has a new version, but you edited {}. Diff:", path.display());
                let _ = std::process::Command::new("diff").arg("-u").arg(&path).arg(&proposed).stdout(std::process::Stdio::from(std::io::stderr())).status();
                eprintln!("theta: keep yours, or apply with `mv {} {}`", proposed.display(), path.display());
                shipped.insert(name.into(), new_hash);
            }
        }
    }
    if shipped != before {
        let text: String = shipped.iter().map(|(n, h)| format!("{n} {h}\n")).collect();
        let _ = std::fs::write(&manifest, text);
    }
}

const DEFAULT_SETTINGS: &str = r#"# theta settings. Project overrides: <project>/.theta/settings.toml
model = "anthropic/claude-sonnet-5-5"
effort = "medium"        # low | medium | high | xhigh | max
verbose = "full"         # full | compact
theme_bg = false                # theme paints the background
theme = "theta"                 # color theme: one of /theme's ids
tab_orientation = "horizontal"   # session tab bar: horizontal | vertical
tab_hidden = false               # ctrl+x b toggles; orientation is kept
tab_width = 24                   # sidebar width in columns (14-60)
agent = "Build"

# Model per background task ("" = main model)
[models]
compaction = "anthropic/claude-haiku-5-5"
title = "anthropic/claude-haiku-5-5"
subagent = ""
subagent_effort = ""                   # "" = effort of the spawning agent

# Model and effort per agent (override the agent file's model/effort; CLI flags still win)
# [agents.Build]
# model = "anthropic/claude-haiku-5-5"
# effort = "medium"
# [agents.Plan]
# model = "anthropic/claude-sonnet-5-5"
# effort = "high"

[compaction]
enabled = true
threshold = 0.8
keep_recent_tokens = 20000
max_tokens = 150000      # compact at the lower of threshold x window and this; 0 = no ceiling
prune_tokens = 30000    # above this, old tool outputs are shortened before any summary (no model call); 0 = never

[tools]
rtk = "auto"             # route shell commands through rtk when installed
bash_timeout = 120
max_lines = 300
# verify = ""            # run after a subagent changed files; "" = detect (Cargo.toml, go.mod), "off" = never
# auto_max_lines = 80    # lines of read/grep/bash output the Auto agent itself receives; 0 = no cap
# checkpoint_steps = 40  # model calls after which a subagent writes a handoff report for Auto; 0 = never
disabled = []

# Skills (~/.theta/skills, <project>/.theta/skills): `$name` in a message injects one.
[skills]
auto = []                # always injected, e.g. ["caveman", "ponytail"]
subagent = ["caveman", "ponytail"]   # also injected into every subagent (token-lean reports)

# MCP servers: `command` (stdio) or `url` (HTTP, `theta mcp login <name>` for OAuth). Tools appear as mcp__<name>__<tool>.
# [mcp.fs]
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
# [mcp.notion]
# url = "https://mcp.notion.com/mcp"
# headers = { Authorization = "Bearer ${NOTION_TOKEN}" }   # optional static auth

# Shell commands around each message. pre: message on stdin, exit != 0 rejects it, stdout is added as context.
# post: last reply on stdin. Env: THETA_SESSION, THETA_CWD, THETA_STATUS (post).
# [hooks]
# pre_message = ["./scripts/check-message.sh"]
# post_message = ["notify-send theta done"]

# Linter per file extension, run in the background once the agent moves on to another file ({file} = edited path).
# [linters]
# py = "ruff check {file}"
# rs = "cargo clippy --quiet"

[diff]
scope = "session"        # session | turn | unpushed
view = "popup"           # popup | split

[web]
backend = "exa"          # exa | firecrawl | brave | tavily | duckduckgo

# Server behind `/remote` (browser control). THETA_REMOTE_URL overrides it.
# [remote]
# url = "https://theta.sputnk.net"

# Custom OpenAI-compatible provider:
# [providers.local]
# api = "openai-chat"
# base_url = "http://localhost:8080/v1"
# models = ["qwen3-coder"]
# context = 128000
"#;

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn shipped_agents_update_unless_edited() {
        let dir = std::env::temp_dir().join(format!("theta-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sync_agents(&dir, false);
        assert_eq!(std::fs::read_to_string(dir.join("Plan.md")).unwrap(), crate::agents::DEFAULT_PLAN);
        // Pretend the shipped Plan was an older text we wrote: untouched => replaced.
        let old = "old plan";
        std::fs::write(dir.join("Plan.md"), old).unwrap();
        let hash = crate::update::sha256(old.as_bytes());
        let m = std::fs::read_to_string(dir.join(".shipped")).unwrap();
        let line = m.lines().find(|l| l.starts_with("Plan ")).unwrap().to_string();
        std::fs::write(dir.join(".shipped"), m.replace(&line, &format!("Plan {hash}"))).unwrap();
        sync_agents(&dir, false);
        assert_eq!(std::fs::read_to_string(dir.join("Plan.md")).unwrap(), crate::agents::DEFAULT_PLAN);
        // Edited by the user and shipped version changed: kept, proposal written aside.
        std::fs::write(dir.join("Plan.md"), "mine").unwrap();
        std::fs::write(dir.join(".shipped"), m.replace(&line, "Plan 0000")).unwrap();
        sync_agents(&dir, false);
        assert_eq!(std::fs::read_to_string(dir.join("Plan.md")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(dir.join("Plan.md.new")).unwrap(), crate::agents::DEFAULT_PLAN);
        // Forced: rewritten, the user's copy kept as .bak, the stale proposal gone.
        sync_agents(&dir, true);
        assert_eq!(std::fs::read_to_string(dir.join("Plan.md")).unwrap(), crate::agents::DEFAULT_PLAN);
        assert_eq!(std::fs::read_to_string(dir.join("Plan.md.bak")).unwrap(), "mine");
        assert!(!dir.join("Plan.md.new").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
