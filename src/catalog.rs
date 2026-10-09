//! Built-in providers and the model catalog (models.dev, cached 24h in ~/.theta/cache/models.json).

use crate::config::{self, Settings};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
    Codex,
    Google,
}

impl Api {
    pub fn parse(s: &str) -> Api {
        match s {
            "anthropic" => Api::Anthropic,
            "openai-responses" => Api::OpenAiResponses,
            "google" => Api::Google,
            _ => Api::OpenAiChat,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthKind {
    Anthropic,
    Codex,
    Copilot,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub base_url: String,
    pub env: Vec<String>,
    pub oauth: Option<OAuthKind>,
    /// models.dev provider id
    pub catalog: String,
    /// Explicit model list (custom providers)
    pub models: Vec<String>,
    pub context: u64,
    /// Inline key from settings (custom providers)
    pub api_key: String,
}

impl Provider {
    /// Wire format for one model. OpenCode Go serves MiniMax over Anthropic Messages and the others over chat completions.
    pub fn api_for(&self, model: &str) -> Api {
        if self.id == "opencode-go" && model.starts_with("minimax") { Api::Anthropic } else { self.api }
    }
}

const BUILTIN: &[(&str, &str, Api, &str, &[&str], Option<OAuthKind>, &str)] = &[
    ("anthropic", "Anthropic", Api::Anthropic, "https://api.anthropic.com", &["ANTHROPIC_API_KEY"], Some(OAuthKind::Anthropic), "anthropic"),
    ("openai", "OpenAI", Api::OpenAiResponses, "https://api.openai.com/v1", &["OPENAI_API_KEY"], None, "openai"),
    ("openai-codex", "ChatGPT (Codex)", Api::Codex, "https://chatgpt.com/backend-api", &[], Some(OAuthKind::Codex), "openai"),
    ("github-copilot", "GitHub Copilot", Api::OpenAiChat, "https://api.individual.githubcopilot.com", &[], Some(OAuthKind::Copilot), "github-copilot"),
    ("google", "Google Gemini", Api::Google, "https://generativelanguage.googleapis.com", &["GEMINI_API_KEY", "GOOGLE_API_KEY"], None, "google"),
    ("openrouter", "OpenRouter", Api::OpenAiChat, "https://openrouter.ai/api/v1", &["OPENROUTER_API_KEY"], None, "openrouter"),
    ("groq", "Groq", Api::OpenAiChat, "https://api.groq.com/openai/v1", &["GROQ_API_KEY"], None, "groq"),
    ("xai", "xAI", Api::OpenAiChat, "https://api.x.ai/v1", &["XAI_API_KEY"], None, "xai"),
    ("mistral", "Mistral", Api::OpenAiChat, "https://api.mistral.ai/v1", &["MISTRAL_API_KEY"], None, "mistral"),
    ("deepseek", "DeepSeek", Api::OpenAiChat, "https://api.deepseek.com/v1", &["DEEPSEEK_API_KEY"], None, "deepseek"),
    ("cerebras", "Cerebras", Api::OpenAiChat, "https://api.cerebras.ai/v1", &["CEREBRAS_API_KEY"], None, "cerebras"),
    ("together", "Together", Api::OpenAiChat, "https://api.together.xyz/v1", &["TOGETHER_API_KEY"], None, "togetherai"),
    ("fireworks", "Fireworks", Api::OpenAiChat, "https://api.fireworks.ai/inference/v1", &["FIREWORKS_API_KEY"], None, "fireworks-ai"),
    ("zai", "Z.ai", Api::OpenAiChat, "https://api.z.ai/api/paas/v4", &["ZAI_API_KEY", "ZHIPU_API_KEY"], None, "zai"),
    ("moonshotai", "Moonshot", Api::OpenAiChat, "https://api.moonshot.ai/v1", &["MOONSHOT_API_KEY"], None, "moonshotai"),
    ("huggingface", "Hugging Face", Api::OpenAiChat, "https://router.huggingface.co/v1", &["HF_TOKEN"], None, "huggingface"),
    ("opencode", "OpenCode Zen", Api::OpenAiChat, "https://opencode.ai/zen/v1", &["OPENCODE_API_KEY"], None, "opencode"),
    // One provider for every Go model: MiniMax speaks Anthropic Messages, the rest chat completions (see Provider::api_for).
    ("opencode-go", "OpenCode Go", Api::OpenAiChat, "https://opencode.ai/zen/go/v1", &["OPENCODE_API_KEY"], None, "opencode-go"),
    ("ollama-cloud", "Ollama Cloud", Api::OpenAiChat, "https://ollama.com/v1", &["OLLAMA_API_KEY"], None, "ollama-cloud"),
    ("ollama", "Ollama (local)", Api::OpenAiChat, "http://localhost:11434/v1", &[], None, ""),
    ("lmstudio", "LM Studio (local)", Api::OpenAiChat, "http://127.0.0.1:1234/v1", &[], None, "lmstudio"),
];

pub fn providers(settings: &Settings) -> Vec<Provider> {
    let mut out: Vec<Provider> = BUILTIN
        .iter()
        .map(|(id, name, api, url, env, oauth, cat)| Provider {
            id: id.to_string(),
            name: name.to_string(),
            api: *api,
            base_url: url.to_string(),
            env: env.iter().map(|s| s.to_string()).collect(),
            oauth: *oauth,
            catalog: cat.to_string(),
            models: vec![],
            context: 0,
            api_key: String::new(),
        })
        .collect();
    for (id, c) in &settings.providers {
        let p = match out.iter_mut().find(|p| &p.id == id) {
            Some(p) => p,
            None => {
                out.push(Provider {
                    id: id.clone(),
                    name: id.clone(),
                    api: Api::OpenAiChat,
                    base_url: String::new(),
                    env: vec![],
                    oauth: None,
                    catalog: String::new(),
                    models: vec![],
                    context: 0,
                    api_key: String::new(),
                });
                out.last_mut().unwrap()
            }
        };
        if !c.name.is_empty() { p.name = c.name.clone(); }
        if !c.api.is_empty() { p.api = Api::parse(&c.api); }
        if !c.base_url.is_empty() { p.base_url = c.base_url.trim_end_matches('/').to_string(); }
        if !c.api_key_env.is_empty() { p.env = vec![c.api_key_env.clone()]; }
        if !c.models.is_empty() { p.models = c.models.clone(); }
        if c.context > 0 { p.context = c.context; }
        p.api_key = c.api_key.clone();
    }
    out
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context: u64,
    pub max_output: u64,
    pub reasoning: bool,
    /// $ per 1M tokens
    pub cost_in: f64,
    pub cost_out: f64,
}

impl Model {
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    pub fn cost(&self, u: &crate::types::Usage) -> f64 {
        (u.input as f64 * self.cost_in + u.output as f64 * self.cost_out + u.cache_read as f64 * self.cost_in * 0.1
            + u.cache_write as f64 * self.cost_in * 1.25)
            / 1e6
    }
}

/// models.dev subset: provider catalog id -> model id -> info
type Cache = BTreeMap<String, BTreeMap<String, Model>>;

fn cache_path() -> std::path::PathBuf {
    config::home().join("cache/models.json")
}

fn load_cache() -> Option<(Cache, bool)> {
    let path = cache_path();
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() < Duration::from_secs(86_400))
        .unwrap_or(false);
    let data = std::fs::read_to_string(&path).ok()?;
    Some((serde_json::from_str(&data).ok()?, fresh))
}

/// Download models.dev and keep only fields theta uses.
pub async fn refresh() -> anyhow::Result<()> {
    let raw: serde_json::Value = reqwest::Client::new()
        .get("https://models.dev/api.json")
        .timeout(Duration::from_secs(20))
        .send()
        .await?
        .json()
        .await?;
    let mut cache = Cache::new();
    for (pid, p) in raw.as_object().into_iter().flatten() {
        let mut models = BTreeMap::new();
        for (mid, m) in p["models"].as_object().into_iter().flatten() {
            if m["tool_call"].as_bool() == Some(false) {
                continue;
            }
            models.insert(
                mid.clone(),
                Model {
                    provider: pid.clone(),
                    id: mid.clone(),
                    name: m["name"].as_str().unwrap_or(mid).to_string(),
                    context: m["limit"]["context"].as_u64().unwrap_or(128_000),
                    max_output: m["limit"]["output"].as_u64().unwrap_or(16_384),
                    reasoning: m["reasoning"].as_bool().unwrap_or(false),
                    cost_in: m["cost"]["input"].as_f64().unwrap_or(0.0),
                    cost_out: m["cost"]["output"].as_f64().unwrap_or(0.0),
                },
            );
        }
        cache.insert(pid.clone(), models);
    }
    std::fs::create_dir_all(config::home().join("cache"))?;
    std::fs::write(cache_path(), serde_json::to_string(&cache)?)?;
    Ok(())
}

pub struct Catalog {
    pub providers: Vec<Provider>,
    cache: Cache,
}

impl Catalog {
    pub async fn load(settings: &Settings) -> Catalog {
        let cache = match load_cache() {
            Some((c, true)) => c,
            Some((c, false)) => {
                tokio::spawn(async { let _ = refresh().await; });
                c
            }
            None => {
                let _ = tokio::time::timeout(Duration::from_secs(8), refresh()).await;
                load_cache().map(|(c, _)| c).unwrap_or_default()
            }
        };
        Catalog { providers: providers(settings), cache }
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn models_of(&self, p: &Provider) -> Vec<Model> {
        if !p.models.is_empty() {
            return p.models.iter().map(|m| self.lookup_in(p, m)).collect();
        }
        let mut out: Vec<Model> = self
            .cache
            .get(&p.catalog)
            .map(|ms| ms.values().cloned().collect())
            .unwrap_or_default();
        if p.api == Api::Codex {
            out.retain(|m| m.id.starts_with("gpt-5") || m.id.starts_with("gpt-6") || m.id.contains("codex"));
        }
        for m in &mut out {
            m.provider = p.id.clone();
        }
        out
    }

    fn lookup_in(&self, p: &Provider, id: &str) -> Model {
        let found = self.cache.get(&p.catalog).and_then(|ms| ms.get(id)).cloned();
        let mut m = found.unwrap_or_else(|| Model {
            provider: p.id.clone(),
            id: id.to_string(),
            name: id.to_string(),
            context: if p.context > 0 { p.context } else { 128_000 },
            max_output: 16_384,
            reasoning: false,
            cost_in: 0.0,
            cost_out: 0.0,
        });
        m.provider = p.id.clone();
        if p.context > 0 {
            m.context = p.context;
        }
        m
    }

    /// Resolve `provider/model`. Unknown models are allowed with default limits.
    pub fn resolve(&self, key: &str) -> Option<(Provider, Model)> {
        let (pid, mid) = key.split_once('/')?;
        let p = self.provider(pid)?.clone();
        let m = self.lookup_in(&p, mid);
        Some((p, m))
    }
}
