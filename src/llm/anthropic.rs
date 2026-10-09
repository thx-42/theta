//! Anthropic Messages API (API key or Claude Pro/Max OAuth).

use super::{OnDelta, Request, Response, Sse, parse_args, send};
use crate::auth::Resolved;
use crate::catalog::Provider;
use crate::types::{Block, Delta, Msg, Role, Usage};
use anyhow::{Result, bail};
use serde_json::{Value, json};

const CLAUDE_CODE_TOOLS: &[&str] = &["Read", "Write", "Edit", "Bash", "Grep", "Glob", "WebFetch", "WebSearch", "Task", "TodoWrite"];

/// (major, minor) parsed from ids like `claude-opus-4-7`, `claude-3-7-sonnet-2025...`.
fn version(id: &str) -> (u32, u32) {
    let parts: Vec<&str> = id.split('-').collect();
    for (i, p) in parts.iter().enumerate() {
        if p.len() == 1 && p.chars().all(|c| c.is_ascii_digit()) {
            let major = p.parse().unwrap_or(0);
            let minor = parts
                .get(i + 1)
                .filter(|n| n.len() <= 2 && n.chars().all(|c| c.is_ascii_digit()))
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            return (major, minor);
        }
    }
    (0, 0)
}

fn safe_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn convert(messages: &[Msg], oauth: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let mut content = Vec::new();
        for b in &m.content {
            match b {
                Block::Text { text } if !text.is_empty() => content.push(json!({"type":"text","text":text})),
                Block::Text { .. } => {}
                Block::Thinking { text, signature: Some(sig) } => {
                    if let Some(data) = sig.strip_prefix("redacted:") {
                        content.push(json!({"type":"redacted_thinking","data":data}));
                    } else {
                        content.push(json!({"type":"thinking","thinking":text,"signature":sig}));
                    }
                }
                Block::Thinking { .. } => {}
                Block::ToolCall { id, name, args, .. } => {
                    let name = if oauth { cc_name(name) } else { name.clone() };
                    content.push(json!({"type":"tool_use","id":safe_id(id),"name":name,"input":args}))
                }
                Block::ToolResult { id, content: c, is_error } => content.push(
                    json!({"type":"tool_result","tool_use_id":safe_id(id),"content":c,"is_error":is_error}),
                ),
                Block::Image { mime, data } => content.push(
                    json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}),
                ),
            }
        }
        if content.is_empty() {
            continue;
        }
        let role = if m.role == Role::User { "user" } else { "assistant" };
        // Merge consecutive same-role messages (API requires alternation).
        if let Some(last) = out.last_mut()
            && last["role"] == role {
                last["content"].as_array_mut().unwrap().extend(content);
                continue;
            }
        out.push(json!({"role":role,"content":content}));
    }
    // Cache breakpoint on the last block of the last two user turns.
    let mut marked = 0;
    for m in out.iter_mut().rev() {
        if m["role"] == "user" && marked < 2
            && let Some(last) = m["content"].as_array_mut().and_then(|c| c.last_mut()) {
                last["cache_control"] = json!({"type":"ephemeral"});
                marked += 1;
            }
    }
    out
}

fn cc_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS.iter().find(|t| t.eq_ignore_ascii_case(name)).map(|t| t.to_string()).unwrap_or(name.into())
}

pub async fn stream(
    client: &reqwest::Client,
    p: &Provider,
    creds: &Resolved,
    req: &Request<'_>,
    on: OnDelta<'_>,
) -> Result<Response> {
    let oauth = creds.oauth || creds.token.contains("sk-ant-oat");
    let mut system = Vec::new();
    if oauth {
        system.push(json!({"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}));
    }
    system.push(json!({"type":"text","text":req.system,"cache_control":{"type":"ephemeral"}}));

    let mut tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            let name = if oauth { cc_name(&t.name) } else { t.name.clone() };
            json!({"name":name,"description":t.description,"input_schema":t.schema})
        })
        .collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type":"ephemeral"});
    }

    let max_tokens = req.model.max_output.clamp(4096, 64_000);
    let mut body = json!({
        "model": req.model.id,
        "max_tokens": max_tokens,
        "stream": true,
        "system": system,
        "messages": convert(req.messages, oauth),
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if req.model.reasoning {
        let (major, minor) = version(&req.model.id);
        if (major, minor) >= (4, 6) {
            let effort = if (major, minor) < (4, 7) && req.effort == "xhigh" { "high" } else { req.effort };
            body["thinking"] = json!({"type":"adaptive","display":"summarized"});
            body["output_config"] = json!({"effort": effort});
        } else {
            let budget = match req.effort { "low" => 4_000, "medium" => 10_000, _ => 24_000 };
            body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
            body["max_tokens"] = json!(max_tokens.max(budget + 8_000));
        }
    }

    let base = creds.base_url.as_deref().unwrap_or(&p.base_url);
    // Some bases already carry the /v1 of their API (OpenCode Go: .../zen/go/v1/messages).
    let url = if base.ends_with("/v1") { format!("{base}/messages") } else { format!("{base}/v1/messages") };
    let mut rb = client
        .post(url)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    rb = if oauth {
        rb.header("authorization", format!("Bearer {}", creds.token))
            .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
            .header("user-agent", "claude-cli/2.1.280")
            .header("x-app", "cli")
    } else {
        rb.header("x-api-key", &creds.token)
    };
    if p.id.starts_with("opencode") {
        rb = rb.header("x-opencode-session", req.session_id);
    }
    let resp = send(rb.json(&body)).await?;
    let mut sse = Sse::new(resp);

    let mut blocks: Vec<Block> = Vec::new();
    let mut json_bufs: Vec<String> = Vec::new();
    let mut usage = Usage::default();
    let mut stop = String::new();
    while let Some(ev) = sse.next().await {
        let (_, data) = ev?;
        let v: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &v["message"]["usage"];
                usage.input = u["input_tokens"].as_u64().unwrap_or(0);
                usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let cb = &v["content_block"];
                let block = match cb["type"].as_str().unwrap_or("") {
                    "text" => Block::Text { text: String::new() },
                    "thinking" => Block::Thinking { text: String::new(), signature: None },
                    "redacted_thinking" => Block::Thinking {
                        text: String::new(),
                        signature: Some(format!("redacted:{}", cb["data"].as_str().unwrap_or(""))),
                    },
                    "tool_use" => {
                        let raw = cb["name"].as_str().unwrap_or("").to_string();
                        let name = req
                            .tools
                            .iter()
                            .find(|t| t.name.eq_ignore_ascii_case(&raw))
                            .map(|t| t.name.clone())
                            .unwrap_or(raw);
                        on(Delta::ToolStart { name: name.clone() });
                        Block::ToolCall { id: cb["id"].as_str().unwrap_or("").into(), name, args: json!({}), sig: None }
                    }
                    _ => Block::Text { text: String::new() },
                };
                blocks.push(block);
                json_bufs.push(String::new());
            }
            "content_block_delta" => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                let d = &v["delta"];
                let Some(block) = blocks.get_mut(i) else { continue };
                match (d["type"].as_str().unwrap_or(""), block) {
                    ("text_delta", Block::Text { text }) => {
                        let t = d["text"].as_str().unwrap_or("");
                        text.push_str(t);
                        on(Delta::Text(t.into()));
                    }
                    ("thinking_delta", Block::Thinking { text, .. }) => {
                        let t = d["thinking"].as_str().unwrap_or("");
                        text.push_str(t);
                        on(Delta::Thinking(t.into()));
                    }
                    ("signature_delta", Block::Thinking { signature, .. }) => {
                        signature.get_or_insert_with(String::new).push_str(d["signature"].as_str().unwrap_or(""));
                    }
                    ("input_json_delta", Block::ToolCall { .. }) => {
                        json_bufs[i].push_str(d["partial_json"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = v["index"].as_u64().unwrap_or(0) as usize;
                if let Some(Block::ToolCall { args, .. }) = blocks.get_mut(i) {
                    *args = parse_args(&json_bufs[i]);
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    stop = s.into();
                }
                if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                    usage.output = o;
                }
            }
            "error" => bail!("anthropic: {}", v["error"]),
            _ => {}
        }
    }
    if stop == "refusal" {
        blocks.push(Block::Text { text: "\n[model declined this request]".into() });
    }
    Ok(Response { msg: Msg { role: Role::Assistant, content: blocks, model: None }, usage, stop })
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions() {
        assert_eq!(super::version("claude-opus-4-7"), (4, 7));
        assert_eq!(super::version("claude-sonnet-4-5-20250929"), (4, 5));
        assert_eq!(super::version("claude-3-7-sonnet-20250219"), (3, 7));
        assert_eq!(super::version("claude-haiku-5-5"), (5, 5));
        assert_eq!(super::version("claude-opus-5"), (5, 0));
    }
}
