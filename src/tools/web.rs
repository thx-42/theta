//! web_search (Exa MCP by default, no key) and web_fetch.

use super::{ToolCtx, ToolOut, n, s, truncate_middle};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Value, json};
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// Call a tool on Exa's hosted MCP endpoint (JSON-RPC over streamable HTTP).
async fn exa_mcp(ctx: &ToolCtx, tool: &str, arguments: Value) -> Result<String> {
    let mut url = "https://mcp.exa.ai/mcp".to_string();
    if let Some(k) = env("EXA_API_KEY") {
        url.push_str(&format!("?exaApiKey={k}"));
    }
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":tool,"arguments":arguments}});
    let r = ctx
        .http
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .timeout(Duration::from_secs(45))
        .json(&body)
        .send()
        .await?;
    let status = r.status();
    let text = r.text().await?;
    if !status.is_success() {
        bail!("exa {status}: {}", text.chars().take(300).collect::<String>());
    }
    let payload = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|l| l.trim())
        .next_back()
        .unwrap_or(text.trim());
    let v: Value = serde_json::from_str(payload).context("bad exa response")?;
    if let Some(e) = v.get("error") {
        bail!("exa: {e}");
    }
    let parts: Vec<&str> = v["result"]["content"].as_array().into_iter().flatten().filter_map(|c| c["text"].as_str()).collect();
    Ok(parts.join("\n"))
}

async fn brave(ctx: &ToolCtx, q: &str, num: u64, key: &str) -> Result<String> {
    let v: Value = ctx
        .http
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", q), ("count", &num.to_string())])
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(format_results(v["web"]["results"].as_array(), "title", "url", "description"))
}

async fn tavily(ctx: &ToolCtx, q: &str, num: u64, key: &str) -> Result<String> {
    let v: Value = ctx
        .http
        .post("https://api.tavily.com/search")
        .bearer_auth(key)
        .json(&json!({"query":q,"max_results":num}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(format_results(v["results"].as_array(), "title", "url", "content"))
}

async fn firecrawl(ctx: &ToolCtx, q: &str, num: u64, key: &str) -> Result<String> {
    let base = env("FIRECRAWL_API_URL").unwrap_or("https://api.firecrawl.dev".into());
    let v: Value = ctx
        .http
        .post(format!("{base}/v2/search"))
        .bearer_auth(key)
        .json(&json!({"query":q,"limit":num}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let items = v["data"]["web"].as_array().or(v["data"].as_array());
    Ok(format_results(items, "title", "url", "description"))
}

async fn duckduckgo(ctx: &ToolCtx, q: &str, num: u64) -> Result<String> {
    let html = ctx
        .http
        .post("https://html.duckduckgo.com/html/")
        .header("User-Agent", "Mozilla/5.0 (theta)")
        .form(&[("q", q)])
        .send()
        .await?
        .text()
        .await?;
    let link = Regex::new(r#"class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let snip = Regex::new(r#"class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();
    let tags = Regex::new(r"<[^>]+>").unwrap();
    let snippets: Vec<String> = snip.captures_iter(&html).map(|c| tags.replace_all(&c[1], "").to_string()).collect();
    let mut out = String::new();
    for (i, c) in link.captures_iter(&html).take(num as usize).enumerate() {
        let mut url = c[1].to_string();
        if let Some(u) = url.split("uddg=").nth(1) {
            url = u.split('&').next().unwrap_or(u).replace("%3A", ":").replace("%2F", "/");
        }
        let title = tags.replace_all(&c[2], "");
        out.push_str(&format!("{}. {title}\n   {url}\n   {}\n", i + 1, snippets.get(i).map(|s| s.as_str()).unwrap_or("")));
    }
    if out.is_empty() {
        bail!("no results");
    }
    Ok(out)
}

fn format_results(items: Option<&Vec<Value>>, t: &str, u: &str, d: &str) -> String {
    let mut out = String::new();
    for (i, r) in items.into_iter().flatten().enumerate() {
        let desc: String = r[d].as_str().unwrap_or("").chars().take(400).collect();
        out.push_str(&format!("{}. {}\n   {}\n   {}\n", i + 1, r[t].as_str().unwrap_or(""), r[u].as_str().unwrap_or(""), desc));
    }
    if out.is_empty() { "no results".into() } else { out }
}

pub async fn search(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let q = s(args, "query").context("query required")?;
    let num = n(args, "num").unwrap_or(6).clamp(1, 15);
    let backend = ctx.settings.web.backend.as_str();
    let res = match backend {
        "brave" => brave(ctx, q, num, &env("BRAVE_API_KEY").context("BRAVE_API_KEY not set")?).await,
        "tavily" => tavily(ctx, q, num, &env("TAVILY_API_KEY").context("TAVILY_API_KEY not set")?).await,
        "firecrawl" => firecrawl(ctx, q, num, &env("FIRECRAWL_API_KEY").unwrap_or_default()).await,
        "duckduckgo" => duckduckgo(ctx, q, num).await,
        _ => exa_mcp(ctx, "web_search_exa", json!({"query":q,"objective":q,"numResults":num})).await,
    };
    // Any backend failing falls back to DuckDuckGo.
    let text = match res {
        Ok(t) => t,
        Err(e) if backend != "duckduckgo" => duckduckgo(ctx, q, num).await.map_err(|_| e)?,
        Err(e) => return Err(e),
    };
    let (text, _) = truncate_middle(&text, ctx.settings.tools.max_lines);
    Ok(ToolOut::ok(text))
}

pub async fn fetch(args: &Value, ctx: &ToolCtx) -> Result<ToolOut> {
    let url = s(args, "url").context("url required")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("only http(s) URLs");
    }
    let max = n(args, "max_chars").unwrap_or(20_000) as usize;
    let r = ctx
        .http
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (theta)")
        .header("Accept", "text/markdown, text/html;q=0.9, */*;q=0.5")
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    let status = r.status();
    let ctype = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = r.bytes().await?;
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let text = if ctype.contains("html") {
        html2text::from_read(&body[..], 100).unwrap_or_else(|_| String::from_utf8_lossy(&body).to_string())
    } else if ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") || ctype.is_empty() {
        String::from_utf8_lossy(&body).to_string()
    } else {
        return Ok(ToolOut::ok(format!("non-text content ({ctype}, {} bytes)", body.len())));
    };
    let text = super::shell::compact(&text);
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push_str(&format!("\n[truncated at {max} chars]"));
    }
    Ok(ToolOut::ok(out))
}
