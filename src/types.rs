//! Provider-neutral conversation types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        args: Value,
        /// Opaque provider token that must be echoed back (Gemini thought signature).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
    },
    ToolResult {
        id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        mime: String,
        data: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Msg {
    pub role: Role,
    pub content: Vec<Block>,
    /// `provider/model` that produced an assistant message; thinking blocks are only replayed to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Self {
        Msg { role: Role::User, content: vec![Block::Text { text: text.into() }], model: None }
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.content {
            if let Block::Text { text } = b {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> Vec<(String, String, Value)> {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall { id, name, args, .. } => Some((id.clone(), name.clone(), args.clone())),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

impl Usage {
    pub fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// Streaming events from a provider.
#[derive(Clone, Debug)]
pub enum Delta {
    Text(String),
    Thinking(String),
    ToolStart { name: String },
}

/// Rough token estimate (4 chars per token) for messages without provider usage.
pub fn estimate_tokens(msgs: &[Msg]) -> u64 {
    let mut chars = 0usize;
    for m in msgs {
        for b in &m.content {
            chars += match b {
                Block::Text { text } | Block::Thinking { text, .. } => text.len(),
                Block::ToolCall { args, .. } => args.to_string().len(),
                Block::ToolResult { content, .. } => content.len(),
                Block::Image { .. } => 4800,
            };
        }
    }
    (chars / 4) as u64
}
