//! Google Gemini (generativelanguage v1beta, SSE).

use super::{OnDelta, Request, Response, Sse, fail_on_error_event, send};
use crate::auth::Resolved;
use crate::catalog::Provider;
use crate::types::{Block, Delta, Msg, Role, Usage};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;

fn convert(messages: &[Msg]) -> Vec<Value> {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    for m in messages {
        let mut parts = Vec::new();
        for b in &m.content {
            match b {
                Block::Text { text } if !text.is_empty() => parts.push(json!({"text":text})),
                Block::ToolCall { id, name, args, sig } => {
                    names.insert(id.clone(), name.clone());
                    let mut part = json!({"functionCall":{"name":name,"args":args}});
                    if let Some(s) = sig {
                        part["thoughtSignature"] = json!(s);
                    }
                    parts.push(part);
                }
                Block::ToolResult { id, content, is_error } => {
                    let name = names.get(id).cloned().unwrap_or_default();
                    let key = if *is_error { "error" } else { "output" };
                    parts.push(json!({"functionResponse":{"name":name,"response":{key:content}}}));
                }
                Block::Image { mime, data } => parts.push(json!({"inlineData":{"mimeType":mime,"data":data}})),
                _ => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        let role = if m.role == Role::User { "user" } else { "model" };
        out.push(json!({"role":role,"parts":parts}));
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
    let mut body = json!({
        "contents": convert(req.messages),
        "systemInstruction": {"parts":[{"text":req.system}]},
        "generationConfig": {"maxOutputTokens": req.model.max_output.clamp(1024, 65_536)},
    });
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"name":t.name,"description":t.description,"parametersJsonSchema":t.schema}))
            .collect();
        body["tools"] = json!([{"functionDeclarations": decls}]);
    }
    if req.model.reasoning {
        body["generationConfig"]["thinkingConfig"] = json!({"includeThoughts": true});
    }
    let url = format!("{}/v1beta/models/{}:streamGenerateContent?alt=sse", p.base_url, req.model.id);
    let resp = send(client.post(url).header("x-goog-api-key", &creds.token).json(&body)).await?;
    let mut sse = Sse::new(resp);

    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls = Vec::new();
    let mut usage = Usage::default();
    let mut stop = String::new();
    while let Some(ev) = sse.next().await {
        let (_, data) = ev?;
        let v: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        fail_on_error_event(&v)?;
        if let Some(u) = v.get("usageMetadata") {
            let cached = u["cachedContentTokenCount"].as_u64().unwrap_or(0);
            usage.input = u["promptTokenCount"].as_u64().unwrap_or(0).saturating_sub(cached);
            usage.cache_read = cached;
            usage.output = u["candidatesTokenCount"].as_u64().unwrap_or(0) + u["thoughtsTokenCount"].as_u64().unwrap_or(0);
        }
        let Some(c) = v["candidates"].get(0) else { continue };
        if let Some(r) = c["finishReason"].as_str() {
            stop = r.into();
        }
        for part in c["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(fc) = part.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or("").to_string();
                on(Delta::ToolStart { name: name.clone() });
                calls.push(Block::ToolCall {
                    id: format!("call_{}", rand::random::<u32>()),
                    name,
                    args: fc["args"].clone(),
                    sig: part["thoughtSignature"].as_str().map(String::from),
                });
            } else if let Some(t) = part["text"].as_str() {
                if part["thought"].as_bool() == Some(true) {
                    thinking.push_str(t);
                    on(Delta::Thinking(t.into()));
                } else {
                    text.push_str(t);
                    on(Delta::Text(t.into()));
                }
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
    if !calls.is_empty() {
        stop = "tool_use".into();
    }
    content.extend(calls);
    Ok(Response { msg: Msg { role: Role::Assistant, content, model: None }, usage, stop })
}
