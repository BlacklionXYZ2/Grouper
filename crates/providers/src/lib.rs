//! One interface over every model provider.
//!
//! Messages are kept in a provider-neutral form ([`Message`], [`Block`]). Assistant
//! messages also keep the provider's raw content (`raw`), which is sent back
//! unchanged to the same provider kind. That keeps history append-only, which
//! prompt caching and Claude's preserved thinking rely on.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod anthropic;
mod openai;
mod scripted;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, #[serde(default)] is_error: bool },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
    /// Provider-native content, replayed verbatim to the same provider kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<RawContent>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RawContent {
    pub kind: String,
    pub value: Value,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: Role::User, content: vec![Block::Text { text: text.into() }], raw: None }
    }

    /// All text blocks joined.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub use core_protocol::{ProviderConfig, ToolDef};

#[derive(Clone, Debug)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other(String),
}

#[derive(Clone, Debug)]
pub struct Response {
    pub message: Message,
    pub stop: StopReason,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("no API key for provider {0}")]
    NoKey(String),
    #[error("{provider} returned HTTP {status}: {body}")]
    Http { provider: String, status: u16, body: String },
    #[error("network error talking to {provider}: {message}")]
    Network { provider: String, message: String },
    #[error("unexpected response from {provider}: {message}")]
    BadResponse { provider: String, message: String },
    #[error("bad provider config: {0}")]
    Config(String),
}

impl ProviderError {
    fn retryable(&self) -> bool {
        match self {
            ProviderError::Http { status, .. } => *status == 429 || *status == 408 || *status >= 500,
            ProviderError::Network { .. } => true,
            _ => false,
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError>;
}

pub fn build(cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> {
    let inner: Arc<dyn Provider> = match cfg.kind.as_str() {
        "anthropic" => Arc::new(anthropic::Anthropic::new(cfg)?),
        "openai" => Arc::new(openai::OpenAi::new(cfg)?),
        "scripted" => Arc::new(scripted::Scripted::new(cfg)?),
        other => return Err(ProviderError::Config(format!("unknown provider kind {other:?}"))),
    };
    Ok(Arc::new(Retrying(inner)))
}

/// Retries rate limits, server errors and network failures with backoff.
struct Retrying(Arc<dyn Provider>);

#[async_trait]
impl Provider for Retrying {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let mut delay = std::time::Duration::from_secs(1);
        let mut attempt = 0;
        loop {
            match self.0.complete(req).await {
                Err(e) if e.retryable() && attempt < 3 => {
                    attempt += 1;
                    tracing::warn!("provider error, retry {attempt}/3 in {delay:?}: {e}");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                other => return other,
            }
        }
    }
}

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .expect("http client")
}

pub(crate) async fn post_json(
    provider: &str,
    req: reqwest::RequestBuilder,
    body: &Value,
) -> Result<Value, ProviderError> {
    let resp = req
        .json(body)
        .send()
        .await
        .map_err(|e| ProviderError::Network { provider: provider.into(), message: e.to_string() })?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| ProviderError::Network { provider: provider.into(), message: e.to_string() })?;
    if !(200..300).contains(&status) {
        let mut body = text;
        body.truncate(2000);
        return Err(ProviderError::Http { provider: provider.into(), status, body });
    }
    serde_json::from_str(&text)
        .map_err(|e| ProviderError::BadResponse { provider: provider.into(), message: e.to_string() })
}

pub(crate) fn opt_bool(options: &Value, key: &str, default: bool) -> bool {
    options.get(key).and_then(Value::as_bool).unwrap_or(default)
}

pub(crate) fn opt_str<'a>(options: &'a Value, key: &str) -> Option<&'a str> {
    options.get(key).and_then(Value::as_str)
}
