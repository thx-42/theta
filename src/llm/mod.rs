//! Streaming LLM clients. One module per wire format.

mod anthropic;
mod google;
mod openai_chat;
mod openai_resp;

use crate::auth::Auth;
use crate::catalog::{Api, Model, Provider};
use crate::types::{Block, Delta, Msg, ToolDef, Usage};
use anyhow::{Result, bail};
use futures::StreamExt;
use std::time::Duration;

pub struct Request<'a> {
    pub model: &'a Model,
    pub system: &'a str,
    pub messages: &'a [Msg],
    pub tools: &'a [ToolDef],
    pub effort: &'a str,
    pub session_id: &'a str,
}

pub struct Response {
    pub msg: Msg,
    pub usage: Usage,
    pub stop: String,
}

pub type OnDelta<'a> = &'a mut (dyn FnMut(Delta) + Send);

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(300))
        .build()
        .expect("http client")
}

/// Run one streamed completion, with retries on 429/5xx before the stream starts.
pub async fn complete(
    client: &reqwest::Client,
    auth: &Auth,
    provider: &Provider,
    req: Request<'_>,
    on: OnDelta<'_>,
) -> Result<Response> {
    let t0 = std::time::Instant::now();
    let mut creds = auth.resolve(provider).await?;
    if t0.elapsed() > Duration::from_secs(2) {
        eprintln!("[llm] credential resolve took {:?}", t0.elapsed());
    }
    // Thinking blocks are only valid for the model that produced them.
    let key = req.model.key();
    let messages: Vec<Msg> = req
        .messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if m.model.as_deref() != Some(key.as_str()) {
                m.content.retain(|b| !matches!(b, Block::Thinking { .. }));
            }
            m
        })
        .filter(|m| !m.content.is_empty())
        .collect();
    let req = Request { messages: &messages, ..req };
    let mut attempt = 0u32;
    let mut switched = std::collections::BTreeSet::new();
    loop {
        let res = match provider.api_for(&req.model.id) {
            Api::Anthropic => anthropic::stream(client, provider, &creds, &req, on).await,
            Api::OpenAiChat => openai_chat::stream(client, provider, &creds, &req, on).await,
            Api::OpenAiResponses | Api::Codex => openai_resp::stream(client, provider, &creds, &req, on).await,
            Api::Google => google::stream(client, provider, &creds, &req, on).await,
        };
        // HTTP errors happen before a stream starts. Never replay a partial stream to rotate accounts.
        if let Err(e) = &res
            && let Some(h) = e.downcast_ref::<HttpError>()
            && h.status == 429 {
            if let Some(profile) = &creds.profile { switched.insert(profile.clone()); }
            if let Some(next) = auth.limited(&provider.id, creds.profile.as_deref(), h.retry_after.unwrap_or(60)).await? {
                if !switched.contains(&next) {
                    let reason = format!("{}: rate limited; switching to {next}", provider.name);
                    on(Delta::Retry { attempt: switched.len() as u32, wait: 0, reason });
                    creds = auth.resolve(provider).await?;
                    continue;
                }
            }
            // With a pool, stop when all profiles have been tried instead of looping through exhausted accounts.
            if auth.accounts(Some(&provider.id)).await.len() > 1 { return res; }
        }
        match res {
            // Nothing of a failed reply was acted on (tools run after it is complete), so replaying it is safe.
            Err(e) if attempt < MAX_RETRIES && retry_wait(&e, attempt + 1).is_some() => {
                attempt += 1;
                let wait = retry_wait(&e, attempt).unwrap_or_default();
                let reason: String = format!("{e:#}").chars().take(160).collect();
                eprintln!("[llm] {} retry {attempt}/{MAX_RETRIES} in {}s: {reason}", req.model.key(), wait.as_secs());
                on(Delta::Retry { attempt, wait: wait.as_secs(), reason });
                tokio::time::sleep(wait + Duration::from_millis(rand::random::<u64>() % 1000)).await;
            }
            Err(e) => return Err(e),
            Ok(mut r) => {
                r.msg.model = Some(key);
                return Ok(r);
            }
        }
    }
}

/// Tries after the first one: 2, 4, 8, 16, 32 and 60 s apart, about two minutes in all.
const MAX_RETRIES: u32 = 6;
/// A provider asking to wait longer than this is not waited for: the user should see the error.
const MAX_RETRY_AFTER: u64 = 120;

#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub body: String,
    /// `retry-after` header, in seconds.
    pub retry_after: Option<u64>,
}

/// The stream broke after it began (provider overloaded, connection cut, reply never finished).
#[derive(Debug)]
pub struct StreamError(pub String);

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for StreamError {}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let body: String = self.body.chars().take(600).collect();
        write!(f, "HTTP {}: {}", self.status, body)
    }
}
impl std::error::Error for HttpError {}

/// How long to wait before try number `attempt` (1-based) after error `e`; None when trying again cannot help.
fn retry_wait(e: &anyhow::Error, attempt: u32) -> Option<Duration> {
    let backoff = Duration::from_secs(2u64.saturating_pow(attempt).min(60));
    if let Some(h) = e.downcast_ref::<HttpError>() {
        if !(h.status == 408 || h.status == 429 || h.status >= 500) {
            return None;
        }
        return match h.retry_after {
            Some(s) if s > MAX_RETRY_AFTER => None,
            Some(s) => Some(Duration::from_secs(s.max(1))),
            None => Some(backoff),
        };
    }
    if e.downcast_ref::<StreamError>().is_some() {
        return Some(backoff);
    }
    e.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_connect() || r.is_timeout() || r.is_body() || r.is_decode() || r.is_request()).then_some(backoff)
}

/// Send and turn non-2xx into HttpError.
pub async fn send(rb: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let r = rb.send().await?;
    if !r.status().is_success() {
        let status = r.status().as_u16();
        let retry_after = r.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
        let body = r.text().await.unwrap_or_default();
        return Err(HttpError { status, body, retry_after }.into());
    }
    Ok(r)
}

/// Minimal SSE reader: yields (event, data) pairs.
pub struct Sse {
    stream: futures::stream::BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    buf: Vec<u8>,
}

impl Sse {
    pub fn new(r: reqwest::Response) -> Sse {
        Sse { stream: r.bytes_stream().boxed(), buf: Vec::new() }
    }

    pub async fn next(&mut self) -> Option<Result<(String, String)>> {
        loop {
            if let Some(pos) = find_event_end(&self.buf) {
                let raw: Vec<u8> = self.buf.drain(..pos.0 + pos.1).collect();
                let text = String::from_utf8_lossy(&raw[..pos.0]).to_string();
                let mut event = String::new();
                let mut data = String::new();
                for line in text.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(v.strip_prefix(' ').unwrap_or(v));
                    }
                }
                if data.is_empty() && event.is_empty() {
                    continue;
                }
                return Some(Ok((event, data)));
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => return Some(Err(e.into())),
                None => {
                    if self.buf.iter().all(|b| b.is_ascii_whitespace()) {
                        return None;
                    }
                    self.buf.extend_from_slice(b"\n\n");
                }
            }
        }
    }
}

/// Returns (end of event, separator length).
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i..].starts_with(b"\n\n") {
            return Some((i, 2));
        }
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
    }
    None
}

/// Parse streamed tool-call JSON; empty means `{}`.
pub fn parse_args(s: &str) -> serde_json::Value {
    if s.trim().is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({ "_invalid_json": s }))
}

pub fn fail_on_error_event(data: &serde_json::Value) -> Result<()> {
    if let Some(err) = data.get("error")
        && !err.is_null() {
            bail!("provider error: {}", err);
        }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rotates_on_429_and_stops_when_the_pool_is_exhausted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _guard = crate::config::TEST_HOME.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("theta-rotation-{}", rand::random::<u64>()));
        let previous = std::env::var_os("THETA_HOME");
        unsafe { std::env::set_var("THETA_HOME", &dir) };
        for exhausted in [false, true] {
            let auth = Auth::default();
            for name in ["a", "b", "c"] {
                auth.set_named("opencode-go", Some(name), crate::auth::Cred::ApiKey { key: format!("test-{name}") }).await.unwrap();
            }
            auth.select("opencode-go", "a").await.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let mut tokens = Vec::new();
                for i in 0..3 {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut data = Vec::new();
                    let headers_end = loop {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        assert!(n > 0);
                        data.extend_from_slice(&buf[..n]);
                        if let Some(pos) = data.windows(4).position(|b| b == b"\r\n\r\n") { break pos + 4; }
                    };
                    let headers = String::from_utf8_lossy(&data[..headers_end]).to_ascii_lowercase();
                    tokens.push(headers.lines().find_map(|l| l.strip_prefix("authorization: bearer ")).unwrap().to_string());
                    let size: usize = headers.lines().find_map(|l| l.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                    while data.len() < headers_end + size {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        assert!(n > 0);
                        data.extend_from_slice(&buf[..n]);
                    }
                    let limited = exhausted || i < 2;
                    let body = if limited { "rate limited" } else {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                    };
                    let status = if limited { "429 Too Many Requests" } else { "200 OK" };
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nRetry-After: 3600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
                tokens
            });
            let mut provider = crate::catalog::providers(&Default::default()).into_iter().find(|p| p.id == "opencode-go").unwrap();
            provider.base_url = format!("http://{address}");
            let model = Model { provider: provider.id.clone(), id: "test-model".into(), name: "Test".into(), context: 4096,
                max_output: 1024, reasoning: false, cost_in: 0.0, cost_out: 0.0 };
            let request = Request { model: &model, system: "", messages: &[], tools: &[], effort: "low", session_id: "test" };
            let mut retries = Vec::new();
            let result = tokio::time::timeout(Duration::from_secs(5), complete(&http(), &auth, &provider, request, &mut |d| {
                if let Delta::Retry { wait, .. } = d { retries.push(wait); }
            })).await.unwrap();
            if exhausted {
                assert_eq!(result.err().unwrap().downcast_ref::<HttpError>().unwrap().status, 429);
                assert!(auth.resolve(&provider).await.is_err());
            } else {
                assert_eq!(result.unwrap().msg.text(), "ok");
                assert_eq!(Auth::load().resolve(&provider).await.unwrap().profile.as_deref(), Some("c"));
            }
            assert_eq!(retries, vec![0, 0], "rotation must not sleep on another account's reset");
            assert_eq!(server.await.unwrap(), vec!["test-a", "test-b", "test-c"]);
        }
        unsafe {
            match previous {
                Some(value) => std::env::set_var("THETA_HOME", value),
                None => std::env::remove_var("THETA_HOME"),
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retries_what_can_recover() {
        let http = |status, retry_after| anyhow::Error::from(HttpError { status, body: String::new(), retry_after });
        assert_eq!(retry_wait(&http(529, None), 1), Some(Duration::from_secs(2)));
        assert_eq!(retry_wait(&http(500, None), 6), Some(Duration::from_secs(60)));
        assert_eq!(retry_wait(&http(429, Some(7)), 1), Some(Duration::from_secs(7)));
        // a limit that resets in an hour is the user's to see, and a bad request never gets better
        assert_eq!(retry_wait(&http(429, Some(3600)), 1), None);
        assert_eq!(retry_wait(&http(400, None), 1), None);
        assert_eq!(retry_wait(&http(401, None), 1), None);
        let cut = anyhow::Error::from(StreamError("overloaded".into())).context("anthropic");
        assert_eq!(retry_wait(&cut, 3), Some(Duration::from_secs(8)));
        assert_eq!(retry_wait(&anyhow::anyhow!("prompt is too long"), 1), None);
    }
}
