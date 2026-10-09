//! MCP client (tools only): stdio and streamable-HTTP servers from `[mcp.<name>]`,
//! exposed as `mcp__<server>__<tool>`. HTTP servers can log in with OAuth 2.1 (PKCE + dynamic registration).

use crate::auth::{self, Io};
use crate::config::{self, McpServer};
use crate::tools::ToolOut;
use crate::types::ToolDef;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

const PROTOCOL: &str = "2025-06-18";
const OAUTH_PORT: u16 = 53693;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Expand `${VAR}` from the environment.
fn expand(s: &str) -> String {
    regex::Regex::new(r"\$\{(\w+)\}").unwrap().replace_all(s, |c: &regex::Captures| std::env::var(&c[1]).unwrap_or_default()).into_owned()
}

fn full_name(server: &str, tool: &str) -> String {
    let n: String = format!("mcp__{server}__{tool}").chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    n.chars().take(64).collect()
}

// ---------- OAuth token store (~/.theta/mcp-auth.json) ----------

#[derive(Clone, Serialize, Deserialize)]
struct Tok {
    client_id: String,
    token_url: String,
    access: String,
    refresh: String,
    /// ms since epoch
    expires: u64,
}

fn tok_path() -> std::path::PathBuf {
    config::home().join("mcp-auth.json")
}

fn load_toks() -> BTreeMap<String, Tok> {
    std::fs::read_to_string(tok_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_toks(t: &BTreeMap<String, Tok>) -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let p = tok_path();
    std::fs::write(&p, serde_json::to_string_pretty(t)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn logout(server: &str) -> Result<()> {
    let mut t = load_toks();
    t.remove(server);
    save_toks(&t)
}

fn tok_from(v: &Value, client_id: &str, token_url: &str, old_refresh: &str) -> Result<Tok> {
    Ok(Tok {
        client_id: client_id.into(),
        token_url: token_url.into(),
        access: v["access_token"].as_str().context("no access_token in response")?.into(),
        refresh: v["refresh_token"].as_str().unwrap_or(old_refresh).into(),
        expires: auth::now_ms() + v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60) * 1000,
    })
}

/// Stored access token for `server`, refreshed when about to expire.
async fn bearer(server: &str) -> Option<String> {
    let mut all = load_toks();
    let t = all.get(server)?.clone();
    if auth::now_ms() < t.expires || t.refresh.is_empty() {
        return Some(t.access);
    }
    let v = auth::post_form(&t.token_url, &[("grant_type", "refresh_token"), ("refresh_token", &t.refresh), ("client_id", &t.client_id)]).await.ok()?;
    let fresh = tok_from(&v, &t.client_id, &t.token_url, &t.refresh).ok()?;
    all.insert(server.into(), fresh.clone());
    let _ = save_toks(&all);
    Some(fresh.access)
}

async fn get_json(http: &reqwest::Client, url: &str) -> Option<Value> {
    let r = http.get(url).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// OAuth login for an HTTP server: discovery, dynamic client registration, PKCE code flow.
pub async fn login(name: &str, cfg: &McpServer, io: &mut Io) -> Result<()> {
    if cfg.url.is_empty() {
        bail!("`{name}` is a stdio server: no login needed");
    }
    let http = reqwest::Client::new();
    let resource = expand(&cfg.url);
    let origin = reqwest::Url::parse(&resource)?.origin().ascii_serialization();
    let as_url = get_json(&http, &format!("{origin}/.well-known/oauth-protected-resource"))
        .await
        .and_then(|v| v["authorization_servers"][0].as_str().map(|s| s.trim_end_matches('/').to_string()))
        .unwrap_or(origin);
    let meta = get_json(&http, &format!("{as_url}/.well-known/oauth-authorization-server")).await.unwrap_or(json!({}));
    let endpoint = |k: &str, default: &str| meta[k].as_str().map(String::from).unwrap_or(format!("{as_url}{default}"));
    let (authorize, token_url) = (endpoint("authorization_endpoint", "/authorize"), endpoint("token_endpoint", "/token"));
    let redirect = format!("http://localhost:{OAUTH_PORT}/callback");

    let mut all = load_toks();
    let client_id = match all.get(name).filter(|t| t.token_url == token_url) {
        Some(t) => t.client_id.clone(),
        None => {
            let reg = meta["registration_endpoint"].as_str().context("server offers no dynamic client registration")?;
            let r = http
                .post(reg)
                .json(&json!({"client_name":"theta","redirect_uris":[redirect],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"}))
                .send()
                .await?;
            let status = r.status();
            let body: Value = r.json().await.unwrap_or(json!({}));
            if !status.is_success() {
                bail!("client registration → {status}: {body}");
            }
            body["client_id"].as_str().context("registration returned no client_id")?.to_string()
        }
    };

    let (verifier, challenge) = auth::pkce();
    let state = auth::b64(&rand::random::<[u8; 16]>());
    let url = format!(
        "{authorize}{}{}",
        if authorize.contains('?') { '&' } else { '?' },
        auth::query(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &redirect),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("resource", &resource),
        ])
    );
    let _ = io.say.send(format!("Sign in to MCP server `{name}` in your browser. If it does not open, visit this URL, then paste the redirect URL:\n\n{url}"));
    auth::open_browser(&url);
    let (code, got) = auth::wait_code(OAUTH_PORT, "/callback", &mut io.paste).await?;
    if got.as_deref().is_some_and(|s| s != state) {
        bail!("OAuth state mismatch");
    }
    let code = code.context("missing authorization code")?;
    let v = auth::post_form(
        &token_url,
        &[("grant_type", "authorization_code"), ("client_id", &client_id), ("code", &code), ("code_verifier", &verifier), ("redirect_uri", &redirect), ("resource", &resource)],
    )
    .await?;
    all.insert(name.into(), tok_from(&v, &client_id, &token_url, "")?);
    save_toks(&all)
}

// ---------- client ----------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

enum Transport {
    Stdio { stdin: tokio::sync::Mutex<ChildStdin>, pending: Pending, _child: Child },
    Http { http: reqwest::Client, url: String, headers: Vec<(String, String)>, session: Mutex<Option<String>>, server: String },
}

struct ToolInfo {
    name: String,
    description: String,
    schema: Value,
}

struct Client {
    t: Transport,
    next: AtomicU64,
    tools: Vec<ToolInfo>,
}

/// Find the response with `id` in an SSE body.
fn parse_sse(text: &str, id: u64) -> Result<Value> {
    for ev in text.replace("\r\n", "\n").split("\n\n") {
        let data: Vec<&str> = ev.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect();
        if let Ok(v) = serde_json::from_str::<Value>(&data.join("\n"))
            && v["id"].as_u64() == Some(id)
        {
            return Ok(v);
        }
    }
    bail!("no response in event stream")
}

impl Client {
    async fn connect(name: &str, cfg: &McpServer) -> Result<Client> {
        let t = if !cfg.url.is_empty() {
            Transport::Http {
                http: reqwest::Client::new(),
                url: expand(&cfg.url),
                headers: cfg.headers.iter().map(|(k, v)| (k.clone(), expand(v))).collect(),
                session: Mutex::new(None),
                server: name.into(),
            }
        } else if !cfg.command.is_empty() {
            let mut child = tokio::process::Command::new(expand(&cfg.command))
                .args(cfg.args.iter().map(|a| expand(a)))
                .envs(cfg.env.iter().map(|(k, v)| (k, expand(v))))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("cannot start `{}`", cfg.command))?;
            let stdin = child.stdin.take().context("no stdin")?;
            let stdout = child.stdout.take().context("no stdout")?;
            let pending: Pending = Default::default();
            let p = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if let Ok(v) = serde_json::from_str::<Value>(&l)
                        && v.get("method").is_none()
                        && let Some(id) = v["id"].as_u64()
                        && let Some(tx) = p.lock().unwrap().remove(&id)
                    {
                        let _ = tx.send(v);
                    }
                }
                p.lock().unwrap().clear(); // server exited: fail waiting requests
            });
            Transport::Stdio { stdin: tokio::sync::Mutex::new(stdin), pending, _child: child }
        } else {
            bail!("needs `command` or `url`");
        };
        let mut c = Client { t, next: AtomicU64::new(1), tools: vec![] };
        c.request("initialize", json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "theta", "version": env!("CARGO_PKG_VERSION")}})).await?;
        c.notify("notifications/initialized").await?;
        let list = c.request("tools/list", json!({})).await?;
        c.tools = list["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        Some(ToolInfo {
                            name: t["name"].as_str()?.into(),
                            description: t["description"].as_str().unwrap_or("").into(),
                            schema: t.get("inputSchema").cloned().unwrap_or(json!({"type": "object"})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(c)
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        let Transport::Stdio { stdin, .. } = &self.t else { unreachable!() };
        let mut w = stdin.lock().await;
        w.write_all(format!("{msg}\n").as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    async fn http_post(&self, msg: &Value, id: Option<u64>) -> Result<Value> {
        let Transport::Http { http, url, headers, session, server } = &self.t else { unreachable!() };
        let mut rb = http.post(url).header("Accept", "application/json, text/event-stream").json(msg);
        if msg["method"] != "initialize" {
            rb = rb.header("MCP-Protocol-Version", PROTOCOL);
        }
        for (k, v) in headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            && let Some(t) = bearer(server).await
        {
            rb = rb.bearer_auth(t);
        }
        if let Some(s) = session.lock().unwrap().clone() {
            rb = rb.header("Mcp-Session-Id", s);
        }
        let r = rb.send().await?;
        if let Some(s) = r.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().unwrap() = Some(s.into());
        }
        let status = r.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("unauthorized: run `/mcp login {server}`");
        }
        let sse = r.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|c| c.contains("text/event-stream"));
        let text = r.text().await?;
        if !status.is_success() {
            bail!("{status}: {}", text.chars().take(300).collect::<String>());
        }
        let Some(id) = id else { return Ok(Value::Null) };
        if sse { parse_sse(&text, id) } else { Ok(serde_json::from_str(&text)?) }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let v = match &self.t {
            Transport::Stdio { pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id, tx);
                self.send(&msg).await?;
                match tokio::time::timeout(CALL_TIMEOUT, rx).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(_)) => bail!("server closed"),
                    Err(_) => {
                        pending.lock().unwrap().remove(&id);
                        bail!("timeout after {}s", CALL_TIMEOUT.as_secs())
                    }
                }
            }
            Transport::Http { .. } => tokio::time::timeout(CALL_TIMEOUT, self.http_post(&msg, Some(id))).await.map_err(|_| anyhow::anyhow!("timeout"))??,
        };
        if let Some(e) = v.get("error") {
            bail!("{}", e["message"].as_str().unwrap_or("MCP error"));
        }
        Ok(v["result"].clone())
    }

    async fn notify(&self, method: &str) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match &self.t {
            Transport::Stdio { .. } => self.send(&msg).await,
            Transport::Http { .. } => self.http_post(&msg, None).await.map(|_| ()),
        }
    }

    async fn call(&self, tool: &str, args: &Value) -> Result<ToolOut> {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args})).await?;
        let text = r["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| match c["type"].as_str() {
                        Some("text") => c["text"].as_str().unwrap_or("").to_string(),
                        Some("image") => format!("[image: {}]", c["mimeType"].as_str().unwrap_or("?")),
                        Some("resource") => c["resource"]["text"].as_str().map(String::from).unwrap_or_else(|| format!("[resource: {}]", c["resource"]["uri"].as_str().unwrap_or("?"))),
                        _ => c.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(if r["isError"].as_bool().unwrap_or(false) { ToolOut::err(text) } else { ToolOut::ok(text) })
    }
}

// ---------- registry ----------

#[derive(Clone, Default)]
struct Slot {
    client: Option<Arc<Client>>,
    error: Option<String>,
}

pub struct Registry {
    cfg: BTreeMap<String, McpServer>,
    slots: Mutex<BTreeMap<String, Slot>>,
}

impl Registry {
    /// Nothing is started until `connect_all`.
    pub fn new(cfg: &BTreeMap<String, McpServer>) -> Registry {
        let slots = cfg.keys().map(|k| (k.clone(), Slot { client: None, error: Some("not connected".into()) })).collect();
        Registry { cfg: cfg.clone(), slots: Mutex::new(slots) }
    }

    pub fn has(&self, name: &str) -> bool {
        self.cfg.contains_key(name)
    }

    pub fn config(&self, name: &str) -> Option<&McpServer> {
        self.cfg.get(name)
    }

    pub async fn connect_all(&self) {
        futures::future::join_all(self.cfg.keys().map(|n| self.connect(n))).await;
    }

    /// (Re)connect one server; failure is kept as its status.
    pub async fn connect(&self, name: &str) {
        let Some(cfg) = self.cfg.get(name) else { return };
        let slot = if !cfg.enabled {
            Slot { client: None, error: Some("disabled".into()) }
        } else {
            match tokio::time::timeout(CONNECT_TIMEOUT, Client::connect(name, cfg)).await {
                Ok(Ok(c)) => Slot { client: Some(Arc::new(c)), error: None },
                Ok(Err(e)) => Slot { client: None, error: Some(format!("{e:#}")) },
                Err(_) => Slot { client: None, error: Some("connect timeout".into()) },
            }
        };
        self.slots.lock().unwrap().insert(name.into(), slot);
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        let slots = self.slots.lock().unwrap();
        slots
            .iter()
            .filter_map(|(s, slot)| slot.client.as_ref().map(|c| (s, c)))
            .flat_map(|(s, c)| {
                c.tools.iter().map(move |t| ToolDef { name: full_name(s, &t.name), description: format!("[MCP {s}] {}", t.description), schema: t.schema.clone() })
            })
            .collect()
    }

    pub async fn call(&self, full: &str, args: &Value) -> ToolOut {
        let found = {
            let slots = self.slots.lock().unwrap();
            slots.iter().filter_map(|(s, slot)| slot.client.as_ref().map(|c| (s, c))).find_map(|(s, c)| c.tools.iter().find(|t| full_name(s, &t.name) == full).map(|t| (c.clone(), t.name.clone())))
        };
        let Some((client, tool)) = found else { return ToolOut::err(format!("unknown MCP tool `{full}` (server not connected?)")) };
        client.call(&tool, args).await.unwrap_or_else(|e| ToolOut::err(format!("{e:#}")))
    }

    /// One line per server for `/mcp`.
    pub fn status(&self) -> Vec<String> {
        let slots = self.slots.lock().unwrap();
        self.cfg
            .iter()
            .map(|(n, cfg)| {
                let kind = if cfg.url.is_empty() { "stdio" } else { "http" };
                match slots.get(n) {
                    Some(Slot { client: Some(c), .. }) => format!("● {n} ({kind}) · {} tools", c.tools.len()),
                    Some(Slot { error: Some(e), .. }) if e.starts_with("unauthorized") => format!("○ {n} ({kind}) · needs login: /mcp login {n}"),
                    Some(Slot { error: Some(e), .. }) => format!("✗ {n} ({kind}) · {e}"),
                    _ => format!("✗ {n} ({kind})"),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(full_name("my srv", "do.it"), "mcp__my_srv__do_it");
        assert_eq!(full_name("s", &"x".repeat(100)).len(), 64);
        unsafe { std::env::set_var("THETA_T", "v") };
        assert_eq!(expand("a-${THETA_T}-${THETA_NOPE}"), "a-v-");
        let sse = "event: message\ndata: {\"id\":1,\"result\":{}}\n\nevent: message\ndata: {\"id\":2,\"result\":{\"ok\":true}}\n\n";
        assert_eq!(parse_sse(sse, 2).unwrap()["result"]["ok"], true);
        assert!(parse_sse(sse, 9).is_err());
    }

    #[tokio::test]
    async fn stdio_server() {
        let script = r#"while read l; do case "$l" in
*'"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{}}';;
*'"tools/list"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"d","inputSchema":{"type":"object"}}]}}';;
*'"tools/call"'*) echo '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hi"}]}}';;
esac; done"#;
        let mut cfg = BTreeMap::new();
        cfg.insert("fake".to_string(), McpServer { command: "sh".into(), args: vec!["-c".into(), script.into()], ..Default::default() });
        let reg = Registry::new(&cfg);
        reg.connect_all().await;
        assert_eq!(reg.defs().iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["mcp__fake__echo"]);
        let out = reg.call("mcp__fake__echo", &json!({})).await;
        assert_eq!((out.content.as_str(), out.is_error), ("hi", false));
        assert!(reg.status()[0].starts_with("● fake"));
    }
}
