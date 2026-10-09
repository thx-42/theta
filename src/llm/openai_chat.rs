//! OpenAI Chat Completions — used by every OpenAI-compatible provider (OpenRouter, Groq, Copilot, Ollama, ...).

use super::{OnDelta, Request, Response, Sse, fail_on_error_event, parse_args, send};
use crate::auth::{COPILOT_HEADERS, Resolved};
use crate::catalog::Provider;
use crate::types::{Block, Delta, Msg, Role, Usage};
use anyhow::Result;
use serde_json::{Value, json};

fn convert(system: &str, messages: &[Msg], echo_reasoning: bool) -> Vec<Value> {
    let mut out = vec![json!({"role":"system","content":system})];
    for m in messages {
        match m.role {
            Role::User => {
                let mut parts = Vec::new();
                for b in &m.content {
                    match b {
                        Block::ToolResult { id, content, .. } => {
                            out.push(json!({"role":"tool","tool_call_id":id,"content":content}))
                        }
                        Block::Text { text } => parts.push(json!({"type":"text","text":text})),
                        Block::Image { mime, data } => parts.push(
                            json!({"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}),
                        ),
                        _ => {}
                    }
                }
                if parts.len() == 1 && parts[0]["type"] == "text" {
                    out.push(json!({"role":"user","content":parts[0]["text"]}));
                } else if !parts.is_empty() {
                    out.push(json!({"role":"user","content":parts}));
                }
            }
            Role::Assistant => {
                let text = m.text();
                let calls: Vec<Value> = m
                    .tool_calls()
                    .into_iter()
                    .map(|(id, name, args)| {
                        json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
                    })
                    .collect();
                let mut msg = json!({"role":"assistant","content": if text.is_empty() { Value::Null } else { json!(text) }});
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                if echo_reasoning {
                    let r: String = m
                        .content
                        .iter()
                        .filter_map(|b| if let Block::Thinking { text, .. } = b { Some(text.as_str()) } else { None })
                        .collect();
                    if !r.is_empty() {
                        msg["reasoning_content"] = json!(r);
                    }
                }
                out.push(msg);
            }
        }
    }
    out
}

pub async fn stream(
    client: &reqwest::Client,
    p: &Provider,
    creds: &Resolved,
    req: &Request<'_>,
    on: OnDelta<'_>,
) -> Result<Response> {
    let echo = matches!(p.id.as_str(), "deepseek" | "moonshotai" | "zai");
    let mut body = json!({
        "model": req.model.id,
        "messages": convert(req.system, req.messages, echo),
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": req.model.max_output.clamp(1024, 32_000),
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.schema}}))
            .collect();
    }
    if req.model.reasoning && p.id == "openrouter" {
        let effort = match req.effort { "xhigh" | "max" => "high", e => e };
        body["reasoning"] = json!({"effort": effort});
    }

    let base = creds.base_url.as_deref().unwrap_or(&p.base_url);
    let mut rb = client.post(format!("{base}/chat/completions")).header("accept", "text/event-stream");
    if !creds.token.is_empty() {
        rb = rb.bearer_auth(&creds.token);
    }
    if p.id == "github-copilot" {
        for (k, v) in COPILOT_HEADERS {
            rb = rb.header(*k, *v);
        }
        let agent_turn = req.messages.last().is_some_and(|m| m.content.iter().any(|b| matches!(b, Block::ToolResult { .. })));
        rb = rb
            .header("Openai-Intent", "conversation-edits")
            .header("X-Initiator", if agent_turn { "agent" } else { "user" });
    }
    if p.id == "openrouter" {
        rb = rb.header("HTTP-Referer", "https://github.com/theta-cli").header("X-Title", "theta");
    }
    if p.id.starts_with("opencode") {
        // OpenCode Go rejects requests without a session id (400 MissingSessionID).
        rb = rb.header("x-opencode-session", req.session_id);
    }
    let resp = send(rb.json(&body)).await?;
    let mut sse = Sse::new(resp);

    let mut text = String::new();
    let mut thinking = String::new();
    // index -> (id, name, args)
    let mut calls: Vec<(String, String, String)> = Vec::new();
    let mut usage = Usage::default();
    let mut stop = String::new();
    while let Some(ev) = sse.next().await {
        let (_, data) = ev?;
        if data == "[DONE]" {
            break;
        }
        let v: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        fail_on_error_event(&v)?;
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            usage.input = u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(cached);
            usage.cache_read = cached;
            usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
        }
        let Some(choice) = v["choices"].get(0) else { continue };
        if let Some(r) = choice["finish_reason"].as_str() {
            stop = r.into();
        }
        let d = &choice["delta"];
        if let Some(t) = d["content"].as_str() {
            text.push_str(t);
            on(Delta::Text(t.into()));
        }
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = d[key].as_str() {
                thinking.push_str(t);
                on(Delta::Thinking(t.into()));
            }
        }
        for tc in d["tool_calls"].as_array().into_iter().flatten() {
            let i = tc["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
            while calls.len() <= i {
                calls.push(Default::default());
            }
            let c = &mut calls[i];
            if let Some(id) = tc["id"].as_str() {
                c.0 = id.into();
            }
            if let Some(n) = tc["function"]["name"].as_str() {
                if c.1.is_empty() {
                    on(Delta::ToolStart { name: n.into() });
                }
                c.1.push_str(n);
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                c.2.push_str(a);
            }
        }
    }
    let mut content = Vec::new();
    if !thinking.is_empty() {
        content.push(Block::Thinking { text: thinking, signature: None });
    }
    if !text.is_empty() {
        content.push(Block::Text { text });
    }
    for (i, (id, name, args)) in calls.into_iter().enumerate() {
        let id = if id.is_empty() { format!("call_{i}_{}", rand::random::<u32>()) } else { id };
        content.push(Block::ToolCall { id, name, args: parse_args(&args), sig: None });
    }
    Ok(Response { msg: Msg { role: Role::Assistant, content, model: None }, usage, stop })
}
