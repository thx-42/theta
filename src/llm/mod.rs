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
    let creds = auth.resolve(provider).await?;
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
    loop {
        let res = match provider.api_for(&req.model.id) {
            Api::Anthropic => anthropic::stream(client, provider, &creds, &req, on).await,
            Api::OpenAiChat => openai_chat::stream(client, provider, &creds, &req, on).await,
            Api::OpenAiResponses | Api::Codex => openai_resp::stream(client, provider, &creds, &req, on).await,
            Api::Google => google::stream(client, provider, &creds, &req, on).await,
        };
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
