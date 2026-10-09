//! `/usage` (subscription rate-limit windows) and `/stats` (token history from stored sessions).

use crate::agent::Runtime;
use crate::catalog::Catalog;
use crate::config;
use crate::session::{Entry, Kind};
use serde_json::Value;
use std::collections::BTreeMap;

const BAR: usize = 24;

fn bar(frac: f64) -> String {
    let n = (frac.clamp(0.0, 1.0) * BAR as f64).round() as usize;
    format!("{}{}", "█".repeat(n), "░".repeat(BAR - n))
}

fn human(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

fn eta(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60);
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {m}m") } else { format!("{m}m") }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Days since 1970-01-01 -> (y, m, d) (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn day_label(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y}-{m:02}-{d:02}")
}

/// Seconds since epoch of an RFC 3339 timestamp (offset ignored: the API returns UTC).
fn parse_iso(s: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    Some(days_from_civil(n(0..4)?, n(5..7)?, n(8..10)?) * 86400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?)
}

fn window(label: &str, used_pct: f64, reset_in: Option<i64>) -> String {
    let reset = reset_in.map(|s| format!("resets in {}", eta(s))).unwrap_or_default();
    format!("  {label:<14}{} {used_pct:>5.1}%  {reset}", bar(used_pct / 100.0))
}

async fn get(rt: &Runtime, url: &str, token: &str, headers: &[(&str, String)]) -> Result<Value, String> {
    let mut req = rt.http.get(url).bearer_auth(token).header("user-agent", "claude-cli/2.1.280");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    res.json().await.map_err(|e| e.to_string())
}

async fn claude(rt: &Runtime) -> Vec<String> {
    let Some(p) = rt.catalog.provider("anthropic") else { return vec!["  unavailable".into()] };
    let creds = match rt.auth.resolve(p).await {
        Ok(c) if c.oauth => c,
        Ok(_) | Err(_) => return vec!["  not logged in with a Claude subscription (/login)".into()],
    };
    let v = match get(rt, "https://api.anthropic.com/api/oauth/usage", &creds.token, &[("anthropic-beta", "oauth-2025-04-20".into())]).await {
        Ok(v) => v,
        Err(e) => return vec![format!("  ✗ {e}")],
    };
    let mut out = Vec::new();
    for (key, label) in [("five_hour", "5-hour"), ("seven_day", "weekly"), ("seven_day_opus", "weekly Opus"), ("seven_day_sonnet", "weekly Sonnet")] {
        let w = &v[key];
        if let Some(used) = w["utilization"].as_f64() {
            out.push(window(label, used, w["resets_at"].as_str().and_then(parse_iso).map(|t| t - now_secs())));
        }
    }
    if out.is_empty() { vec!["  no limits reported".into()] } else { out }
}

async fn codex(rt: &Runtime) -> Vec<String> {
    let Some(p) = rt.catalog.provider("openai-codex") else { return vec!["  unavailable".into()] };
    let creds = match rt.auth.resolve(p).await {
        Ok(c) if c.oauth => c,
        Ok(_) | Err(_) => return vec!["  not logged in with ChatGPT (/login)".into()],
    };
    let v = match get(rt, "https://chatgpt.com/backend-api/wham/usage", &creds.token, &[("chatgpt-account-id", creds.account_id.unwrap_or_default())]).await {
        Ok(v) => v,
        Err(e) => return vec![format!("  ✗ {e}")],
    };
    let mut out = Vec::new();
    if let Some(plan) = v["plan_type"].as_str() {
        out.push(format!("  plan {plan}"));
    }
    for (key, fallback) in [("primary_window", "primary"), ("secondary_window", "secondary")] {
        let w = &v["rate_limit"][key];
        if let Some(used) = w["used_percent"].as_f64() {
            let label = w["limit_window_seconds"].as_i64().map(|s| if s >= 86400 { format!("{}-day", s / 86400) } else { format!("{}-hour", s / 3600) }).unwrap_or_else(|| fallback.into());
            out.push(window(&label, used, w["reset_after_seconds"].as_i64()));
        }
    }
    if out.is_empty() { vec!["  no limits reported".into()] } else { out }
}

pub async fn usage(rt: &Runtime) -> Vec<String> {
    let (c, x) = tokio::join!(claude(rt), codex(rt));
    let mut out = vec!["Claude (Pro/Max)".to_string()];
    out.extend(c);
    out.push(String::new());
    out.push("ChatGPT (Codex)".into());
    out.extend(x);
    out
}

#[derive(Default)]
struct Totals {
    input: u64,
    output: u64,
    cache: u64,
    msgs: u64,
    /// USD at API list prices (what a subscription would have cost pay-as-you-go)
    cost: f64,
}

impl Totals {
    fn all(&self) -> u64 {
        self.input + self.output + self.cache
    }
}

/// Aggregate every stored session (all projects): per model and per day.
pub fn stats(catalog: &Catalog) -> Vec<String> {
    let mut by_model: BTreeMap<String, Totals> = BTreeMap::new();
    let mut by_day: BTreeMap<i64, Totals> = BTreeMap::new();
    let mut sessions = 0;
    let dirs = std::fs::read_dir(config::home().join("sessions")).into_iter().flatten().flatten();
    for file in dirs.filter_map(|d| std::fs::read_dir(d.path()).ok()).flatten().flatten() {
        if file.path().extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(file.path()) else { continue };
        sessions += 1;
        for e in text.lines().filter_map(|l| serde_json::from_str::<Entry>(l).ok()) {
            let Kind::Msg { msg, usage: Some(u) } = e.kind else { continue };
            let model = msg.model.unwrap_or_else(|| "unknown".into());
            let cost = catalog.resolve(&model).map_or(0.0, |(_, m)| m.cost(&u));
            for t in [by_model.entry(model).or_default(), by_day.entry(e.ts as i64 / 86400).or_default()] {
                t.cost += cost;
                t.input += u.input;
                t.output += u.output;
                t.cache += u.cache_read + u.cache_write;
                t.msgs += 1;
            }
        }
    }
    if by_day.is_empty() {
        return vec!["no usage recorded yet".into()];
    }
    let sum = by_model.values().fold(Totals::default(), |a, t| Totals { input: a.input + t.input, output: a.output + t.output, cache: a.cache + t.cache, msgs: a.msgs + t.msgs, cost: a.cost + t.cost });
    let (peak_day, peak) = by_day.iter().max_by_key(|(_, t)| t.all()).map(|(d, t)| (*d, t.all())).unwrap();
    let first = *by_day.keys().next().unwrap();
    let today = now_secs() / 86400;
    let span = (today - first + 1).max(1) as u64;

    let mut out = vec![
        format!("  total      {}   ({} requests, {sessions} sessions)", human(sum.all()), sum.msgs),
        format!("  input      {}    output {}    cache {}", human(sum.input), human(sum.output), human(sum.cache)),
        format!("  active     {} days of {span}   avg {}/day", by_day.len(), human(sum.all() / by_day.len() as u64)),
        format!("  peak day   {}   {}   ${:.2}", day_label(peak_day), human(peak), by_day[&peak_day].cost),
        format!("  cost       ${:.2}   avg ${:.2}/active day   (API list prices)", sum.cost, sum.cost / by_day.len() as f64),
        String::new(),
        "Models (tokens · input / output / cache · cost)".into(),
    ];
    let mut models: Vec<_> = by_model.iter().collect();
    models.sort_by_key(|(_, t)| std::cmp::Reverse(t.all()));
    let top = models[0].1.all().max(1) as f64;
    for (m, t) in models.iter().take(10) {
        let name: String = m.chars().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect();
        out.push(format!("  {name:<30} {} {:>7}  {} / {} / {}  ${:.2}", bar(t.all() as f64 / top), human(t.all()), human(t.input), human(t.output), human(t.cache), t.cost));
    }
    out.push(String::new());
    out.push("Tokens per day (last 14)".into());
    let days: Vec<_> = (today - 13..=today).collect();
    let top = days.iter().map(|d| by_day.get(d).map_or(0, Totals::all)).max().unwrap_or(0).max(1) as f64;
    for d in days {
        let t = by_day.get(&d).map_or(0, Totals::all);
        out.push(format!("  {} {} {:>7}  {}", &day_label(d)[5..], bar(t as f64 / top), if t == 0 { "-".into() } else { human(t) }, by_day.get(&d).map_or(String::new(), |t| format!("${:.2}", t.cost))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(parse_iso("2025-11-04T04:59:59.9+00:00"), Some(1_762_232_399));
        assert_eq!(day_label(1_762_232_399 / 86400), "2025-11-04");
        assert_eq!(eta(90_000), "1d 1h");
    }

    #[test]
    fn stats_runs() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let c = rt.block_on(Catalog::load(&crate::config::Settings::default()));
        println!("{}", stats(&c).join("\n"));
    }
}
