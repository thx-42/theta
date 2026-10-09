//! OpenAI Responses API — OpenAI (API key) and ChatGPT subscription (Codex backend).

use super::{OnDelta, Request, Response, Sse, parse_args, send};
use crate::auth::Resolved;
use crate::catalog::{Api, Provider};
use crate::types::{Block, Delta, Msg, Role, Usage};
use anyhow::{Result, bail};
use serde_json::{Value, json};

fn convert(messages: &[Msg]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::User => {
                let mut parts = Vec::new();
                for b in &m.content {
                    match b {
                        Block::ToolResult { id, content, .. } => {
                            out.push(json!({"type":"function_call_output","call_id":id,"output":content}))
                        }
                        Block::Text { text } => parts.push(json!({"type":"input_text","text":text})),
                        Block::Image { mime, data } => parts.push(
                            json!({"type":"input_image","image_url":format!("data:{mime};base64,{data}")}),
                        ),
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(json!({"role":"user","content":parts}));
                }
            }
            Role::Assistant => {
                for b in &m.content {
                    match b {
                        Block::Text { text } if !text.is_empty() => out.push(
                            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
                        ),
                        Block::ToolCall { id, name, args, .. } => out.push(
                            json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()}),
                        ),
                        _ => {}
                    }
                }
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
    let codex = p.api == Api::Codex;
    let mut body = json!({
        "model": req.model.id,
        "instructions": req.system,
        "input": convert(req.messages),
        "stream": true,
        "store": false,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "prompt_cache_key": req.session_id,
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type":"function","name":t.name,"description":t.description,"parameters":t.schema}))
            .collect();
    }
    if req.model.reasoning {
        let effort = match req.effort { "max" => "xhigh", e => e };
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    let url = if codex { format!("{}/codex/responses", p.base_url) } else { format!("{}/responses", p.base_url) };
    let mut rb = client.post(url).bearer_auth(&creds.token).header("accept", "text/event-stream");
    if codex {
        rb = rb
            .header("chatgpt-account-id", creds.account_id.clone().unwrap_or_default())
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "theta")
            .header("session_id", req.session_id);
    }
    let resp = send(rb.json(&body)).await?;
    let mut sse = Sse::new(resp);

    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls: Vec<Block> = Vec::new();
    let mut usage = Usage::default();
    let mut stop = "stop".to_string();
    while let Some(ev) = sse.next().await {
        let (_, data) = ev?;
        let v: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                let t = v["delta"].as_str().unwrap_or("");
                text.push_str(t);
                on(Delta::Text(t.into()));
            }
            "response.reasoning_summary_text.delta" => {
                let t = v["delta"].as_str().unwrap_or("");
                thinking.push_str(t);
                on(Delta::Thinking(t.into()));
            }
            "response.reasoning_summary_part.added" if !thinking.is_empty() => {
                thinking.push_str("\n\n");
                on(Delta::Thinking("\n\n".into()));
            }
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                on(Delta::ToolStart { name: v["item"]["name"].as_str().unwrap_or("").into() });
            }
            "response.output_item.done" if v["item"]["type"] == "function_call" => {
                let it = &v["item"];
                calls.push(Block::ToolCall {
                    id: it["call_id"].as_str().unwrap_or("").into(),
                    name: it["name"].as_str().unwrap_or("").into(),
                    args: parse_args(it["arguments"].as_str().unwrap_or("")),
                    sig: None,
                });
            }
            "response.completed" | "response.incomplete" => {
                let u = &v["response"]["usage"];
                let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
                usage.input = u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached);
                usage.cache_read = cached;
                usage.output = u["output_tokens"].as_u64().unwrap_or(0);
                if v["type"] == "response.incomplete" {
                    stop = "length".into();
                }
            }
            "response.failed" => bail!("openai: {}", v["response"]["error"]),
            "error" => bail!("openai: {}", v),
            _ => {}
        }
    }
    let mut content = Vec::new();
    if !thinking.is_empty() {
        content.push(Block::Thinking { text: thinking, signature: None });
    }
    if !text.is_empty() {
        content.push(Block::Text { text });
    }
    if !calls.is_empty() {
        stop = "tool_use".into();
    }
    content.extend(calls);
    Ok(Response { msg: Msg { role: Role::Assistant, content, model: None }, usage, stop })
}
