//! Swapping a rate-limited free model for another free model on the same provider.
//!
//! Free models (OpenRouter's `:free` ones, for example) are rate limited per model, and
//! the limits are often upstream and short-lived. When the requested model answers 429
//! (or 404/502/503: gone or unavailable), [`Fallback`] puts it on a short cooldown and
//! sends the same request to the next alternate that isn't cooling down, using the same
//! key. Only failures before the first delta swap models: once text reached the caller,
//! another model would repeat or contradict it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::{Delta, OnDelta, Provider, ProviderError, Request, Response};

/// How long a model that hit its limit is skipped.
const COOLDOWN: Duration = Duration::from_secs(60);

/// (provider id, model) → skip until. Shared by every conversation and thread in the process.
fn cooldowns() -> &'static Mutex<HashMap<(String, String), Instant>> {
    static MAP: OnceLock<Mutex<HashMap<(String, String), Instant>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

fn cooling(provider: &str, model: &str, now: Instant) -> bool {
    let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, until| *until > now);
    map.contains_key(&(provider.to_string(), model.to_string()))
}

fn cool(provider: &str, model: &str) {
    cooldowns()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((provider.to_string(), model.to_string()), Instant::now() + COOLDOWN);
}

/// Whether this error means "this model, right now", so another model may work.
/// Key, quota-wide and request errors (400, 401, 402, 403) are not: every model would fail.
/// An account-wide limit (OpenRouter's daily free-model allowance) also answers 429, but
/// every model on the key would fail the same way, so it doesn't swap either.
fn model_unavailable(e: &ProviderError) -> bool {
    match e {
        ProviderError::Http { status: 429, body, .. } => !account_wide_limit(body),
        ProviderError::Http { status: 404 | 502 | 503, .. } => true,
        _ => false,
    }
}

/// Whether an error body is OpenRouter's account-wide daily free-model limit
/// (`free-models-per-day`, `limit_source: openrouter_free_tier_daily`).
pub fn account_wide_limit(body: &str) -> bool {
    body.contains("free-models-per-day") || body.contains("openrouter_free_tier_daily")
}

/// Tries the request's model, then `alternates` in order, skipping models on cooldown.
pub struct Fallback {
    pub(crate) inner: Arc<dyn Provider>,
    pub(crate) provider_id: String,
    pub(crate) alternates: Vec<String>,
}

impl Fallback {
    pub fn new(inner: Arc<dyn Provider>, provider_id: impl Into<String>, alternates: Vec<String>) -> Self {
        Fallback { inner, provider_id: provider_id.into(), alternates }
    }

    /// The models to try, in order: those not cooling down first. If every model is
    /// cooling down, the requested one is still tried (so the caller's retry/backoff applies).
    fn candidates(&self, requested: &str) -> Vec<String> {
        let now = Instant::now();
        let mut all = vec![requested.to_string()];
        for m in &self.alternates {
            if !all.contains(m) {
                all.push(m.clone());
            }
        }
        let ready: Vec<String> = all.iter().filter(|m| !cooling(&self.provider_id, m, now)).cloned().collect();
        if ready.is_empty() { vec![requested.to_string()] } else { ready }
    }
}

#[async_trait]
impl Provider for Fallback {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        self.complete_stream(req, &|_| {}).await
    }

    async fn complete_stream(&self, req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
        let candidates = self.candidates(&req.model);
        let mut last_err = None;
        for (i, model) in candidates.iter().enumerate() {
            let emitted = std::sync::atomic::AtomicBool::new(false);
            let forward = |d: Delta| {
                emitted.store(true, std::sync::atomic::Ordering::Relaxed);
                on_delta(d);
            };
            let attempt = Request { model: model.clone(), ..req.clone() };
            match self.inner.complete_stream(&attempt, &forward).await {
                Err(e) if model_unavailable(&e) && !emitted.load(std::sync::atomic::Ordering::Relaxed) => {
                    cool(&self.provider_id, model);
                    if let Some(next) = candidates.get(i + 1) {
                        tracing::warn!("{}/{model} unavailable ({e}); trying {next}", self.provider_id);
                    }
                    last_err = Some(e);
                }
                Ok(mut resp) => {
                    if i > 0 {
                        tracing::info!("{} answered with {model} instead of {}", self.provider_id, req.model);
                        resp.model = Some(model.clone());
                    }
                    return Ok(resp);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.expect("at least one candidate"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, Message, Role, StopReason, Usage};
    use std::sync::Mutex as StdMutex;

    /// Answers 429 for models in `limited`, otherwise replies with the model's name.
    struct PerModel {
        limited: Vec<&'static str>,
        status: u16,
        calls: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl Provider for PerModel {
        async fn complete(&self, _req: &Request) -> Result<Response, ProviderError> {
            unreachable!()
        }
        async fn complete_stream(&self, req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
            self.calls.lock().unwrap().push(req.model.clone());
            if self.limited.contains(&req.model.as_str()) {
                return Err(ProviderError::Http { provider: "p".into(), status: self.status, body: String::new() });
            }
            on_delta(Delta::Text(req.model.clone()));
            Ok(Response {
                model: None,
                message: Message { role: Role::Assistant, content: vec![Block::Text { text: req.model.clone() }], raw: None },
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    fn req(model: &str) -> Request {
        Request { model: model.into(), system: String::new(), messages: vec![], tools: vec![], max_tokens: 1 }
    }

    fn provider(limited: Vec<&'static str>, status: u16) -> Arc<PerModel> {
        Arc::new(PerModel { limited, status, calls: StdMutex::new(vec![]) })
    }

    #[tokio::test]
    async fn swaps_a_rate_limited_model_and_skips_it_while_cooling() {
        // A unique provider id keeps the shared cooldown map from leaking between tests.
        let inner = provider(vec!["a"], 429);
        let fb = Fallback::new(inner.clone(), "swap-test", vec!["b".into(), "c".into()]);
        let r = fb.complete_stream(&req("a"), &|_| {}).await.unwrap();
        assert_eq!(r.message.text(), "b");
        assert_eq!(r.model.as_deref(), Some("b"), "the response names the model that answered");
        // "a" is cooling down now: the next request goes straight to "b".
        let r = fb.complete(&req("a")).await.unwrap();
        assert_eq!(r.message.text(), "b");
        assert_eq!(*inner.calls.lock().unwrap(), vec!["a", "b", "b"]);
    }

    #[tokio::test]
    async fn all_limited_returns_the_last_error_then_still_tries_the_requested_model() {
        let inner = provider(vec!["x", "y"], 429);
        let fb = Fallback::new(inner.clone(), "all-limited-test", vec!["y".into()]);
        let r = fb.complete(&req("x")).await;
        assert!(matches!(r, Err(ProviderError::Http { status: 429, .. })));
        // Everything is cooling: only the requested model is tried, once.
        let _ = fb.complete(&req("x")).await;
        assert_eq!(*inner.calls.lock().unwrap(), vec!["x", "y", "x"]);
    }

    #[test]
    fn the_daily_free_allowance_does_not_swap() {
        let daily = ProviderError::Http {
            provider: "openrouter".into(),
            status: 429,
            body: r#"{"error":{"message":"Rate limit exceeded: free-models-per-day.","code":429,"metadata":{"limit_source":"openrouter_free_tier_daily"}}}"#.into(),
        };
        assert!(!model_unavailable(&daily));
        let upstream = ProviderError::Http {
            provider: "openrouter".into(),
            status: 429,
            body: r#"{"error":{"message":"Provider returned error","code":429}}"#.into(),
        };
        assert!(model_unavailable(&upstream));
    }

    #[tokio::test]
    async fn key_errors_do_not_swap() {
        let inner = provider(vec!["k"], 401);
        let fb = Fallback::new(inner.clone(), "key-test", vec!["other".into()]);
        let r = fb.complete(&req("k")).await;
        assert!(matches!(r, Err(ProviderError::Http { status: 401, .. })));
        assert_eq!(*inner.calls.lock().unwrap(), vec!["k"]);
    }
}
