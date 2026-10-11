//! Credentials (~/.theta/auth.json, mode 0600): API keys and OAuth for Claude Pro/Max, ChatGPT (Codex), GitHub Copilot.

use crate::catalog::{OAuthKind, Provider};
use crate::config;
use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE_NO_PAD, STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Cred {
    ApiKey { key: String },
    Oauth {
        access: String,
        refresh: String,
        /// ms since epoch
        expires: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
}

/// What a provider client needs to authenticate a request.
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    pub token: String,
    pub oauth: bool,
    pub account_id: Option<String>,
    /// Overrides provider base URL (Copilot token proxy endpoint).
    pub base_url: Option<String>,
}

#[derive(Clone, Default)]
pub struct Auth {
    store: Arc<Mutex<BTreeMap<String, Cred>>>,
}

fn path() -> std::path::PathBuf {
    config::home().join("auth.json")
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn save_store(store: &BTreeMap<String, Cred>) -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let p = path();
    std::fs::write(&p, serde_json::to_string_pretty(store)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl Auth {
    pub fn load() -> Auth {
        let store = std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Auth { store: Arc::new(Mutex::new(store)) }
    }

    /// Re-read `auth.json` (another process logged in or out).
    pub async fn reload(&self) {
        let fresh = Auth::load().store.lock().await.clone();
        *self.store.lock().await = fresh;
    }

    pub async fn set(&self, provider: &str, cred: Cred) -> Result<()> {
        let mut s = self.store.lock().await;
        s.insert(provider.to_string(), cred);
        save_store(&s)
    }

    pub async fn remove(&self, provider: &str) -> Result<()> {
        let mut s = self.store.lock().await;
        s.remove(provider);
        save_store(&s)
    }

    /// Human-readable credential source, or None.
    pub async fn status(&self, p: &Provider) -> Option<String> {
        if let Some(c) = self.store.lock().await.get(&p.id) {
            return Some(match c {
                Cred::ApiKey { .. } => "key".into(),
                Cred::Oauth { .. } => "oauth".into(),
            });
        }
        if !p.api_key.is_empty() {
            return Some("settings".into());
        }
        p.env.iter().find(|e| std::env::var(e).is_ok_and(|v| !v.is_empty())).cloned()
    }

    pub async fn resolve(&self, p: &Provider) -> Result<Resolved> {
        let mut store = self.store.lock().await;
        match store.get(&p.id).cloned() {
            Some(Cred::ApiKey { key }) => return Ok(Resolved { token: key, ..Default::default() }),
            Some(Cred::Oauth { access, refresh, expires, account_id }) => {
                let kind = p.oauth.context("provider has no OAuth support")?;
                let (access, account_id) = if now_ms() + 60_000 >= expires {
                    let fresh = refresh_token(kind, &refresh).await.context("OAuth refresh failed, run /login again")?;
                    let out = match &fresh {
                        Cred::Oauth { access, account_id, .. } => (access.clone(), account_id.clone()),
                        _ => unreachable!(),
                    };
                    store.insert(p.id.clone(), fresh);
                    save_store(&store)?;
                    out
                } else {
                    (access, account_id)
                };
                let base_url = (kind == OAuthKind::Copilot).then(|| copilot_base_url(&access));
                return Ok(Resolved { token: access, oauth: true, account_id, base_url });
            }
            None => {}
        }
        if !p.api_key.is_empty() {
            return Ok(Resolved { token: p.api_key.clone(), ..Default::default() });
        }
        for e in &p.env {
            if let Ok(v) = std::env::var(e)
                && !v.is_empty() {
                    return Ok(Resolved { token: v, ..Default::default() });
                }
        }
        if p.env.is_empty() && p.oauth.is_none() {
            return Ok(Resolved::default()); // local provider, no key
        }
        bail!("no credentials for {} — run `theta login {}` or set {}", p.name, p.id, p.env.join("/"))
    }
}

// ---------- OAuth ----------

pub(crate) fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn pkce() -> (String, String) {
    let verifier = b64(&rand::random::<[u8; 32]>());
    let challenge = b64(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn decode(s: &str) -> String {
    String::from_utf8(STANDARD.decode(s).unwrap()).unwrap()
}

const ANTHROPIC_AUTHORIZE: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_TOKEN: &str = "https://platform.claude.com/v1/oauth/token";
const ANTHROPIC_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const ANTHROPIC_PORT: u16 = 53692;

const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const CODEX_PORT: u16 = 1455;

pub const COPILOT_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];

fn anthropic_client_id() -> String {
    decode("OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl")
}

fn copilot_client_id() -> String {
    decode("SXYxLmI1MDdhMDhjODdlY2ZlOTg=")
}

fn copilot_base_url(token: &str) -> String {
    token
        .split(';')
        .find_map(|kv| kv.strip_prefix("proxy-ep="))
        .map(|h| format!("https://{}", h.replacen("proxy.", "api.", 1)))
        .unwrap_or_else(|| "https://api.individual.githubcopilot.com".into())
}

pub(crate) fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Parse `code`/`state` from a redirect URL, `code#state`, or a bare code.
fn parse_code(input: &str) -> (Option<String>, Option<String>) {
    let v = input.trim();
    let query = v.split_once('?').map(|(_, q)| q).unwrap_or(v);
    if query.contains("code=") {
        let mut code = None;
        let mut state = None;
        for (k, val) in url_pairs(query) {
            match k.as_str() {
                "code" => code = Some(val),
                "state" => state = Some(val),
                _ => {}
            }
        }
        return (code, state);
    }
    if let Some((c, s)) = v.split_once('#') {
        return (Some(c.into()), Some(s.into()));
    }
    ((!v.is_empty()).then(|| v.to_string()), None)
}

fn url_pairs(q: &str) -> Vec<(String, String)> {
    q.split(['&', '#'])
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), urldecode(v)))
        .collect()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for c in s.bytes() {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(c as char),
            _ => out.push_str(&format!("%{c:02X}")),
        }
    }
    out
}

pub(crate) fn query(params: &[(&str, &str)]) -> String {
    params.iter().map(|(k, v)| format!("{k}={}", urlencode(v))).collect::<Vec<_>>().join("&")
}

/// Wait for the browser redirect on 127.0.0.1:port, or a pasted URL/code.
pub(crate) async fn wait_code(port: u16, path: &str, paste: &mut UnboundedReceiver<String>) -> Result<(Option<String>, Option<String>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.ok();
    loop {
        tokio::select! {
            conn = async { listener.as_ref().unwrap().accept().await }, if listener.is_some() => {
                let (mut sock, _) = conn?;
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let target = req.split_whitespace().nth(1).unwrap_or("").to_string();
                if !target.starts_with(path) {
                    let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
                    continue;
                }
                let body = "<html><body style=\"font-family:sans-serif;text-align:center;margin-top:20vh\"><h1>&theta;</h1><p>Login complete. You can close this tab.</p></body></html>";
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
                return Ok(parse_code(&target));
            }
            line = paste.recv() => {
                let line = line.context("login cancelled")?;
                if !line.trim().is_empty() {
                    return Ok(parse_code(&line));
                }
            }
        }
    }
}

fn expires_in(v: &Value, margin_ms: u64) -> u64 {
    now_ms() + v["expires_in"].as_u64().unwrap_or(3600) * 1000 - margin_ms
}

async fn post_json(url: &str, body: Value) -> Result<Value> {
    let r = reqwest::Client::new().post(url).json(&body).send().await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        bail!("{url} → {status}: {text}");
    }
    Ok(serde_json::from_str(&text)?)
}

pub(crate) async fn post_form(url: &str, form: &[(&str, &str)]) -> Result<Value> {
    let r = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(query(form))
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        bail!("{url} → {status}: {text}");
    }
    Ok(serde_json::from_str(&text)?)
}

fn jwt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v["https://api.openai.com/auth"]["chatgpt_account_id"].as_str().map(String::from)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Oauth(OAuthKind),
    Key,
}

/// One way to log in, shown in the login method picker.
pub struct LoginOption {
    pub label: &'static str,
    /// Provider the credential is stored under (ChatGPT login lives under `openai-codex`).
    pub provider: String,
    pub method: Method,
}

/// Login methods for a provider. OpenAI groups the ChatGPT subscription (Codex) with its API key.
pub fn login_options(p: &Provider) -> Vec<LoginOption> {
    let mut out = Vec::new();
    if p.id == "openai" {
        out.push(LoginOption { label: "ChatGPT Plus/Pro (subscription)", provider: "openai-codex".into(), method: Method::Oauth(OAuthKind::Codex) });
    }
    if let Some(k) = p.oauth {
        let label = match k {
            OAuthKind::Anthropic => "Claude Pro/Max (subscription)",
            OAuthKind::Codex => "ChatGPT Plus/Pro (subscription)",
            OAuthKind::Copilot => "GitHub Copilot (device login)",
        };
        out.push(LoginOption { label, provider: p.id.clone(), method: Method::Oauth(k) });
    }
    if !p.env.is_empty() {
        out.push(LoginOption { label: "API key", provider: p.id.clone(), method: Method::Key });
    }
    out
}

/// Providers listed in the login picker (the Codex entry is reached through OpenAI; local ones need no login).
pub fn shows_in_login(p: &Provider) -> bool {
    p.id != "openai-codex" && !login_options(p).is_empty()
}

/// Channels between an OAuth flow and whoever shows it: `say` gets instructions, `paste` feeds a pasted code/URL.
pub struct Io {
    pub say: UnboundedSender<String>,
    pub paste: UnboundedReceiver<String>,
}

pub async fn oauth(kind: OAuthKind, io: &mut Io) -> Result<Cred> {
    match kind {
        OAuthKind::Anthropic => login_anthropic(io).await,
        OAuthKind::Codex => login_codex(io).await,
        OAuthKind::Copilot => login_copilot(io).await,
    }
}

/// `theta login` on a plain terminal.
pub async fn login_cli(auth: &Auth, p: &Provider) -> Result<()> {
    let opts = login_options(p);
    let opt = match opts.len() {
        0 => bail!("{} needs no login", p.name),
        1 => &opts[0],
        _ => {
            for (i, o) in opts.iter().enumerate() {
                println!("  [{}] {}", i + 1, o.label);
            }
            let n: usize = read_line(&format!("{} > ", p.name)).parse().unwrap_or(1);
            opts.get(n.wrapping_sub(1)).context("invalid choice")?
        }
    };
    let cred = match opt.method {
        Method::Key => {
            let key = read_line(&format!("{} API key: ", p.name));
            if key.is_empty() {
                bail!("empty key");
            }
            Cred::ApiKey { key }
        }
        Method::Oauth(kind) => {
            let (say, mut said) = unbounded_channel();
            let (paste_tx, paste) = unbounded_channel();
            std::thread::spawn(move || {
                let mut s = String::new();
                while std::io::stdin().read_line(&mut s).is_ok_and(|n| n > 0) {
                    if paste_tx.send(std::mem::take(&mut s)).is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                while let Some(m) = said.recv().await {
                    println!("{m}\n");
                }
            });
            oauth(kind, &mut Io { say, paste }).await?
        }
    };
    auth.set(&opt.provider, cred).await?;
    println!("✓ logged in to {}", p.name);
    Ok(())
}

fn read_line(msg: &str) -> String {
    use std::io::Write;
    print!("{msg}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim().to_string()
}

async fn login_anthropic(io: &mut Io) -> Result<Cred> {
    let (verifier, challenge) = pkce();
    let redirect = format!("http://localhost:{ANTHROPIC_PORT}/callback");
    let cid = anthropic_client_id();
    let url = format!(
        "{ANTHROPIC_AUTHORIZE}?{}",
        query(&[
            ("code", "true"),
            ("client_id", &cid),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("scope", ANTHROPIC_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
        ])
    );
    let _ = io.say.send(format!("Sign in in your browser. If it does not open, visit this URL, then paste the code shown:\n\n{url}"));
    open_browser(&url);
    let (code, state) = wait_code(ANTHROPIC_PORT, "/callback", &mut io.paste).await?;
    if state.as_deref().is_some_and(|s| s != verifier) {
        bail!("OAuth state mismatch");
    }
    let code = code.context("missing authorization code")?;
    let v = post_json(
        ANTHROPIC_TOKEN,
        json!({"grant_type":"authorization_code","client_id":cid,"code":code,"state":state.unwrap_or(verifier.clone()),
               "redirect_uri":redirect,"code_verifier":verifier}),
    )
    .await?;
    Ok(Cred::Oauth {
        access: v["access_token"].as_str().context("no access_token")?.into(),
        refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
        expires: expires_in(&v, 300_000),
        account_id: None,
    })
}

async fn login_codex(io: &mut Io) -> Result<Cred> {
    let (verifier, challenge) = pkce();
    let state = b64(&rand::random::<[u8; 16]>());
    let redirect = format!("http://localhost:{CODEX_PORT}/auth/callback");
    let url = format!(
        "{CODEX_AUTHORIZE}?{}",
        query(&[
            ("response_type", "code"),
            ("client_id", CODEX_CLIENT),
            ("redirect_uri", &redirect),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "theta"),
        ])
    );
    let _ = io.say.send(format!("Sign in with ChatGPT in your browser. If it does not open, visit this URL, then paste the redirect URL:\n\n{url}"));
    open_browser(&url);
    let (code, got_state) = wait_code(CODEX_PORT, "/auth/callback", &mut io.paste).await?;
    if got_state.as_deref().is_some_and(|s| s != state) {
        bail!("OAuth state mismatch");
    }
    let code = code.context("missing authorization code")?;
    let v = post_form(
        CODEX_TOKEN,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT),
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
        ],
    )
    .await?;
    codex_cred(&v)
}

fn codex_cred(v: &Value) -> Result<Cred> {
    let access = v["access_token"].as_str().context("no access_token")?.to_string();
    let account_id = Some(jwt_account_id(&access).context("no ChatGPT account id in token")?);
    Ok(Cred::Oauth {
        refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
        expires: expires_in(v, 0),
        access,
        account_id,
    })
}

async fn login_copilot(io: &mut Io) -> Result<Cred> {
    let cid = copilot_client_id();
    let d = post_form("https://github.com/login/device/code", &[("client_id", &cid), ("scope", "read:user")]).await?;
    let device = d["device_code"].as_str().context("bad device response")?.to_string();
    let uri = d["verification_uri"].as_str().unwrap_or("https://github.com/login/device");
    if !uri.starts_with("https://") {
        bail!("untrusted verification uri");
    }
    let mut interval = d["interval"].as_u64().unwrap_or(5);
    let _ = io.say.send(format!("Open {uri} and enter code: {}", d["user_code"].as_str().unwrap_or("?")));
    open_browser(uri);
    let gh_token = loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let r = post_form(
            "https://github.com/login/oauth/access_token",
            &[("client_id", &cid), ("device_code", &device), ("grant_type", "urn:ietf:params:oauth:grant-type:device_code")],
        )
        .await?;
        if let Some(t) = r["access_token"].as_str() {
            break t.to_string();
        }
        match r["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(e) => bail!("device flow failed: {e}"),
            None => bail!("invalid device token response"),
        }
    };
    refresh_token(OAuthKind::Copilot, &gh_token).await
}

async fn refresh_token(kind: OAuthKind, refresh: &str) -> Result<Cred> {
    match kind {
        OAuthKind::Anthropic => {
            let v = post_json(
                ANTHROPIC_TOKEN,
                json!({"grant_type":"refresh_token","client_id":anthropic_client_id(),"refresh_token":refresh}),
            )
            .await?;
            Ok(Cred::Oauth {
                access: v["access_token"].as_str().context("no access_token")?.into(),
                refresh: v["refresh_token"].as_str().unwrap_or(refresh).into(),
                expires: expires_in(&v, 300_000),
                account_id: None,
            })
        }
        OAuthKind::Codex => {
            let v = post_form(
                CODEX_TOKEN,
                &[("grant_type", "refresh_token"), ("refresh_token", refresh), ("client_id", CODEX_CLIENT)],
            )
            .await?;
            codex_cred(&v)
        }
        OAuthKind::Copilot => {
            let mut req = reqwest::Client::new()
                .get("https://api.github.com/copilot_internal/v2/token")
                .header("Accept", "application/json")
                .header("Authorization", format!("Bearer {refresh}"));
            for (k, v) in COPILOT_HEADERS {
                req = req.header(*k, *v);
            }
            let r = req.send().await?;
            if !r.status().is_success() {
                bail!("copilot token: {} {}", r.status(), r.text().await.unwrap_or_default());
            }
            let v: Value = r.json().await?;
            Ok(Cred::Oauth {
                access: v["token"].as_str().context("no copilot token")?.into(),
                refresh: refresh.into(),
                expires: v["expires_at"].as_u64().unwrap_or(0) * 1000 - 300_000,
                account_id: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codes() {
        assert_eq!(parse_code("/callback?code=ab%2Fc&state=xy"), (Some("ab/c".into()), Some("xy".into())));
        assert_eq!(parse_code("abc#def"), (Some("abc".into()), Some("def".into())));
        assert_eq!(parse_code("plain"), (Some("plain".into()), None));
        let c = crate::catalog::providers(&Default::default());
        let labels = |id: &str| login_options(c.iter().find(|p| p.id == id).unwrap()).iter().map(|o| (o.provider.clone(), o.method)).collect::<Vec<_>>();
        assert_eq!(labels("anthropic"), vec![("anthropic".into(), Method::Oauth(OAuthKind::Anthropic)), ("anthropic".into(), Method::Key)]);
        assert_eq!(labels("openai"), vec![("openai-codex".into(), Method::Oauth(OAuthKind::Codex)), ("openai".into(), Method::Key)]);
        assert!(!shows_in_login(c.iter().find(|p| p.id == "openai-codex").unwrap()));
        assert_eq!(copilot_base_url("tid=1;proxy-ep=proxy.individual.githubcopilot.com;x=1"), "https://api.individual.githubcopilot.com");
    }
}
