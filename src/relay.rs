//! Outbound link to theta-server (`/remote`): the daemon dials out, gets a 6-character code, and a browser
//! that enters it on the server's link page drives sessions through the same `Req`/`Push` protocol as the TUI.
//!
//! Wire (JSON text frames over one WebSocket):
//! - relayed: `{"ch":N,"req":Req}` from the browser, `{"ch":N,"push":Push}` back. One channel per browser tab.
//! - control: `{"type":..}` between daemon and server only (`hello`, `code`, `linked`, `unlinked`,
//!   `web_connected`, `newcode`, `stop`, `error`). See theta-server's README.

use crate::proto::{Push, RemoteState, Req};
use crate::server::Server;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::Notify;
use tokio::time::{Instant, interval, sleep, sleep_until, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const MAX_CHANNELS: usize = 32;

/// What a browser may ask for. An allow-list: new `Req` variants stay local until listed here.
/// The browser can already run shell commands through the agent, but it cannot stop the daemon,
/// reload credentials, start MCP servers or touch the link itself.
pub fn allowed(req: &Req) -> bool {
    matches!(
        req,
        Req::Attach { .. }
            | Req::Send(..)
            | Req::Steer(_)
            | Req::SaveTabs { .. }
            | Req::Seen
            | Req::Interrupt
            | Req::Answer { .. }
            | Req::SetModel(_)
            | Req::SetEffort(_)
            | Req::SetAgent(_)
            | Req::Compact
            | Req::Btw(_)
            | Req::Title(_)
            | Req::Leaf(_)
            | Req::McpStatus
            | Req::Busy
            | Req::Overview { .. }
            | Req::JobKill(_)
            | Req::JobOutput(_)
    )
}

/// `https://host` → `wss://host/ws/daemon`. Plain `http://` is only accepted for loopback.
pub fn ws_url(base: &str) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    let (scheme, rest) = base.split_once("://").ok_or("remote url must start with https://")?;
    let host = rest.split(['/', ':']).next().unwrap_or("");
    let secure = match scheme {
        "https" | "wss" => true,
        "http" | "ws" if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => false,
        "http" | "ws" => return Err("plain http is only allowed for localhost".into()),
        _ => return Err(format!("unsupported scheme `{scheme}`")),
    };
    Ok(format!("{}://{rest}/ws/daemon", if secure { "wss" } else { "ws" }))
}

#[derive(Deserialize)]
struct In {
    r#type: Option<String>,
    ch: Option<u32>,
    req: Option<serde_json::Value>,
    code: Option<String>,
    expires_in: Option<u64>,
    token: Option<String>,
    message: Option<String>,
}

#[derive(Serialize)]
struct Relayed<'a> {
    ch: u32,
    push: &'a Push,
}

enum Outcome {
    Stopped,
    Dropped(String),
    /// The server said no (rate limit, full): back off longer.
    Refused(String),
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

pub async fn run(srv: Arc<Server>, base: String, stop: Arc<Notify>) {
    let url = match ws_url(&base) {
        Ok(u) => u,
        Err(e) => return srv.set_remote(RemoteState::Error(e)),
    };
    let host = hostname();
    let mut token: Option<String> = None;
    let mut delay = Duration::from_secs(1);
    loop {
        let outcome = session(&srv, &url, &host, &mut token, &stop, &mut delay).await;
        let wait = match outcome {
            Outcome::Stopped => return srv.set_remote(RemoteState::Off),
            Outcome::Dropped(e) => {
                srv.set_remote(RemoteState::Offline(e));
                delay
            }
            Outcome::Refused(e) => {
                srv.set_remote(RemoteState::Error(e));
                Duration::from_secs(30)
            }
        };
        tokio::select! {
            _ = sleep(wait) => {}
            _ = stop.notified() => return srv.set_remote(RemoteState::Off),
        }
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn session(srv: &Arc<Server>, url: &str, host: &str, token: &mut Option<String>, stop: &Notify, delay: &mut Duration) -> Outcome {
    let ws = match timeout(Duration::from_secs(15), connect_async(url)).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => return Outcome::Dropped(e.to_string()),
        Err(_) => return Outcome::Dropped("connection timed out".into()),
    };
    let (mut sink, mut stream) = ws.split();
    let hello = serde_json::json!({ "type": "hello", "host": host, "version": crate::update::VERSION, "token": token });
    if sink.send(Message::text(hello.to_string())).await.is_err() {
        return Outcome::Dropped("connection closed".into());
    }

    let (out_tx, mut out_rx) = unbounded_channel::<(u32, Push)>();
    let mut chans: HashMap<u32, UnboundedSender<Req>> = HashMap::new();
    let mut ping = interval(Duration::from_secs(30));
    let mut code_deadline: Option<Instant> = None;
    loop {
        let expiry = async {
            match code_deadline {
                Some(d) => sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = stop.notified() => {
                let _ = sink.send(Message::text(r#"{"type":"stop"}"#)).await;
                let _ = sink.close().await;
                return Outcome::Stopped;
            }
            _ = expiry => {
                code_deadline = None;
                if sink.send(Message::text(r#"{"type":"newcode"}"#)).await.is_err() {
                    return Outcome::Dropped("connection closed".into());
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    return Outcome::Dropped("connection closed".into());
                }
            }
            Some((ch, push)) = out_rx.recv() => {
                let Ok(frame) = serde_json::to_string(&Relayed { ch, push: &push }) else { continue };
                if sink.send(Message::text(frame)).await.is_err() {
                    return Outcome::Dropped("connection closed".into());
                }
            }
            msg = stream.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) | None => return Outcome::Dropped("server closed the link".into()),
                    Some(Err(e)) => return Outcome::Dropped(e.to_string()),
                    Some(Ok(_)) => continue,
                };
                let Ok(f) = serde_json::from_str::<In>(&text) else { continue };
                match f.r#type.as_deref() {
                    Some("code") => {
                        *delay = Duration::from_secs(1);
                        if f.token.is_some() {
                            *token = f.token;
                        }
                        let expires_in = f.expires_in.unwrap_or(300);
                        code_deadline = Some(Instant::now() + Duration::from_secs(expires_in));
                        srv.set_remote(RemoteState::Code { code: f.code.unwrap_or_default(), expires_in });
                    }
                    Some("linked") => {
                        *delay = Duration::from_secs(1);
                        if f.token.is_some() {
                            *token = f.token;
                        }
                        code_deadline = None;
                        srv.set_remote(RemoteState::Linked);
                    }
                    Some("unlinked") => {
                        chans.clear();
                        if sink.send(Message::text(r#"{"type":"newcode"}"#)).await.is_err() {
                            return Outcome::Dropped("connection closed".into());
                        }
                    }
                    // A new browser took over: its channel ids start from scratch.
                    Some("web_connected") => chans.clear(),
                    Some("error") => return Outcome::Refused(f.message.unwrap_or_else(|| "refused by server".into())),
                    Some(_) => {}
                    None => {}
                }
                let (Some(ch), Some(req)) = (f.ch, f.req) else { continue };
                let req = match serde_json::from_value::<Req>(req) {
                    Ok(r) if allowed(&r) => r,
                    Ok(_) => {
                        let _ = out_tx.send((ch, Push::Err("not allowed remotely".into())));
                        continue;
                    }
                    Err(e) => {
                        let _ = out_tx.send((ch, Push::Err(format!("bad request: {e}"))));
                        continue;
                    }
                };
                if !chans.contains_key(&ch) {
                    if chans.len() >= MAX_CHANNELS {
                        let _ = out_tx.send((ch, Push::Err("too many open tabs".into())));
                        continue;
                    }
                    chans.insert(ch, open_channel(srv, ch, &out_tx));
                }
                if let Some(tx) = chans.get(&ch) {
                    let _ = tx.send(req);
                }
            }
        }
    }
}

/// A virtual client connection: the daemon serves it exactly like a socket client.
fn open_channel(srv: &Arc<Server>, ch: u32, out: &UnboundedSender<(u32, Push)>) -> UnboundedSender<Req> {
    let (req_tx, req_rx) = unbounded_channel::<Req>();
    let (push_tx, mut push_rx) = unbounded_channel::<Push>();
    tokio::spawn(srv.clone().run_connection(req_rx, push_tx, None));
    let out = out.clone();
    tokio::spawn(async move {
        while let Some(p) = push_rx.recv().await {
            if out.send((ch, p)).is_err() {
                break;
            }
        }
    });
    req_tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_forms() {
        assert_eq!(ws_url("https://theta.sputnk.net/").unwrap(), "wss://theta.sputnk.net/ws/daemon");
        assert_eq!(ws_url("http://localhost:8080").unwrap(), "ws://localhost:8080/ws/daemon");
        assert!(ws_url("http://example.com").is_err());
        assert!(ws_url("theta.sputnk.net").is_err());
    }

    #[test]
    fn allow_list_blocks_control_requests() {
        assert!(allowed(&Req::Interrupt));
        assert!(allowed(&Req::Send("hi".into(), vec![])));
        for r in [Req::Shutdown, Req::Reload, Req::RemoteStart, Req::RemoteStop, Req::RemoteStatus, Req::McpConnect("x".into())] {
            assert!(!allowed(&r), "{r:?}");
        }
    }

    #[test]
    fn frames_keep_ch_first() {
        let f = serde_json::to_string(&Relayed { ch: 3, push: &Push::Busy(true) }).unwrap();
        assert!(f.starts_with(r#"{"ch":3,"#), "{f}");
    }
}
