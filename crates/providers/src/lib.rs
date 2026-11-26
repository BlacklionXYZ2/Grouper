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
mod fallback;
mod openai;
mod scripted;
mod sse;

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
pub use fallback::{Fallback, account_wide_limit};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Response {
    pub message: Message,
    pub stop: StopReason,
    pub usage: Usage,
    /// The model that answered, when it isn't the requested one ([`Fallback`] swapped it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
    #[error("{0}")]
    Unavailable(String),
}

impl ProviderError {
    fn retryable(&self) -> bool {
        match self {
            ProviderError::Http { status: 429, body, .. } => !fallback::account_wide_limit(body),
            ProviderError::Http { status, .. } => *status == 408 || *status >= 500,
            ProviderError::Network { .. } => true,
            _ => false,
        }
    }
}

/// A piece of a response as the model generates it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", content = "text", rename_all = "snake_case")]
pub enum Delta {
    /// Visible text. Concatenated, a response's text deltas equal [`Message::text`]
    /// of the final message (separate text blocks are joined with `\n`).
    Text(String),
    /// Reasoning text, when the provider shows it.
    Thinking(String),
}

/// Receives [`Delta`]s while a response streams.
pub type OnDelta<'a> = &'a (dyn Fn(Delta) + Send + Sync);

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError>;

    /// Like [`complete`](Provider::complete), calling `on_delta` as text arrives. The
    /// returned [`Response`] is the same as `complete` would give. The default makes
    /// one non-streaming call and emits the whole text once.
    async fn complete_stream(&self, req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
        let resp = self.complete(req).await?;
        let text = resp.message.text();
        if !text.is_empty() {
            on_delta(Delta::Text(text));
        }
        Ok(resp)
    }
}

/// The provider itself, without retries.
fn build_raw(cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> {
    Ok(match cfg.kind.as_str() {
        "anthropic" => Arc::new(anthropic::Anthropic::new(cfg)?),
        "openai" => Arc::new(openai::OpenAi::new(cfg)?),
        "scripted" => Arc::new(scripted::Scripted::new(cfg)?),
        other => return Err(ProviderError::Config(format!("unknown provider kind {other:?}"))),
    })
}

pub fn build(cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, ProviderError> {
    let inner = build_raw(cfg)?;
    Ok(Arc::new(Retrying(inner)))
}

/// Like [`build`], but a model that is rate limited or unavailable is swapped for the
/// next of `alternates` (same provider, same key) before backing off; see [`Fallback`].
/// The backoff then retries the whole list.
pub fn build_with_fallbacks(cfg: &ProviderConfig, alternates: Vec<String>) -> Result<Arc<dyn Provider>, ProviderError> {
    if alternates.is_empty() {
        return build(cfg);
    }
    let inner = build_raw(cfg)?;
    Ok(Arc::new(Retrying(Arc::new(Fallback::new(inner, cfg.id.clone(), alternates)))))
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

    /// Retries only failures before the first delta: once text has reached the
    /// caller, a retry would repeat it.
    async fn complete_stream(&self, req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
        let mut delay = std::time::Duration::from_secs(1);
        let mut attempt = 0;
        loop {
            let emitted = std::sync::atomic::AtomicBool::new(false);
            let forward = |d: Delta| {
                emitted.store(true, std::sync::atomic::Ordering::Relaxed);
                on_delta(d);
            };
            match self.0.complete_stream(req, &forward).await {
                Err(e) if e.retryable() && attempt < 3 && !emitted.load(std::sync::atomic::Ordering::Relaxed) => {
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
        // Cut on a character boundary: `String::truncate` panics inside a UTF-8 sequence.
        let body: String = text.chars().take(2000).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Fails with a 503 on every call; emits `before_failing` deltas first.
    struct Flaky {
        calls: AtomicU32,
        before_failing: usize,
    }

    #[async_trait]
    impl Provider for Flaky {
        async fn complete(&self, _req: &Request) -> Result<Response, ProviderError> {
            unreachable!()
        }
        async fn complete_stream(&self, _req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            for _ in 0..self.before_failing {
                on_delta(Delta::Text(format!("call {n} ")));
            }
            Err(ProviderError::Http { provider: "p".into(), status: 503, body: String::new() })
        }
    }

    fn req() -> Request {
        Request { model: "m".into(), system: String::new(), messages: vec![], tools: vec![], max_tokens: 1 }
    }

    #[tokio::test]
    async fn no_retry_after_a_delta() {
        let inner = Arc::new(Flaky { calls: AtomicU32::new(0), before_failing: 1 });
        let seen = Mutex::new(Vec::new());
        let r = Retrying(inner.clone()).complete_stream(&req(), &|d| seen.lock().unwrap().push(d)).await;
        assert!(matches!(r, Err(ProviderError::Http { status: 503, .. })));
        assert_eq!(inner.calls.load(Ordering::Relaxed), 1);
        assert_eq!(seen.into_inner().unwrap(), vec![Delta::Text("call 0 ".into())]);
    }

    #[tokio::test]
    async fn retries_before_the_first_delta() {
        let inner = Arc::new(Flaky { calls: AtomicU32::new(0), before_failing: 0 });
        // Three retries with 1 s, 2 s, 4 s backoff would make this test slow; stop after the first.
        let r = tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            Retrying(inner.clone()).complete_stream(&req(), &|_| {}),
        )
        .await;
        assert!(r.is_err(), "still retrying");
        assert_eq!(inner.calls.load(Ordering::Relaxed), 2);
    }
}
