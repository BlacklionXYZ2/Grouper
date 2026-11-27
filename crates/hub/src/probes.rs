//! Hub probe runner and HTTP endpoint (`POST /v1/models/{id}/probe`).
//!
//! Probe task definitions, deterministic graders and result summaries live in
//! [`core_registry::probes`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use core_protocol::{ProbeReport, ProbeResult};
pub use core_registry::probes::{
    PROBE_SYSTEM, ProbeOutput, ProbeTask, ProbeToolCall, SLOW_PROBE_MS, SUITE, summarize_probe_results,
};
use providers::{Block, Message, ProviderError, Request, Response as ModelResponse};

use crate::db::random_hex;
use crate::registry::deny_unless_admin;
use crate::server::{HUB_DEVICE, Hub, err};

/// Per-probe call timeout so a stalled endpoint cannot hang an entire suite forever.
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

fn to_probe_output(resp: &ModelResponse) -> ProbeOutput {
    let tool_calls = resp
        .message
        .content
        .iter()
        .filter_map(|b| match b {
            Block::ToolUse { name, input, .. } => Some(ProbeToolCall { name: name.clone(), input: input.clone() }),
            _ => None,
        })
        .collect();
    ProbeOutput { text: resp.message.text(), tool_calls }
}

#[derive(Debug)]
pub enum ProbeError {
    NotFound(String),
    Busy(String),
    BudgetExceeded(String),
    Unavailable(String),
    BadConfig(String),
    Internal(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s)
            | Self::Busy(s)
            | Self::BudgetExceeded(s)
            | Self::Unavailable(s)
            | Self::BadConfig(s)
            | Self::Internal(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ProbeError {}

struct ProbeRunGuard<'a> {
    hub: &'a Hub,
    model_id: String,
}

impl Drop for ProbeRunGuard<'_> {
    fn drop(&mut self) {
        self.hub.probe_runs.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.model_id);
    }
}

fn begin_probe<'a>(hub: &'a Hub, model_id: &str) -> Result<ProbeRunGuard<'a>, ProbeError> {
    let mut running = hub.probe_runs.lock().unwrap_or_else(|e| e.into_inner());
    if !running.insert(model_id.to_string()) {
        return Err(ProbeError::Busy(format!("a probe run for model {model_id:?} is already in progress")));
    }
    Ok(ProbeRunGuard { hub, model_id: model_id.to_string() })
}

/// Run the built-in probe suite against `model_key`, record usage and `source = 'probe'`
/// outcomes, and create or update a pending use-case proposal.
pub async fn run_probes(hub: &Arc<Hub>, model_key: &str) -> Result<ProbeReport, ProbeError> {
    let model = hub
        .db
        .find_model(model_key)
        .ok_or_else(|| ProbeError::NotFound(format!("no model {model_key:?} in the registry")))?;
    // The guard covers both the budget check and the complete suite, so concurrent
    // requests for one model cannot both spend against the same remaining budget.
    let _run = begin_probe(hub, &model.id)?;
    if let Some(why) = crate::limits::daily_budget_refusal(&hub.db) {
        return Err(ProbeError::BudgetExceeded(why));
    }
    let provider_cfg = hub
        .db
        .provider(&model.provider_id)
        .map_err(|e| ProbeError::Internal(e.to_string()))?
        .ok_or_else(|| ProbeError::BadConfig(format!("no provider {}", model.provider_id)))?;

    // Check device host availability up front so an offline host fails cleanly
    // instead of recording bogus failed probe rows.
    let outcome_device_id = if let Some(host) = provider_cfg.via_device() {
        let (device_id, device_name) = hub.resolve_device(host).ok_or_else(|| {
            ProbeError::BadConfig(format!("provider {} names unknown hosting device {host:?}", provider_cfg.id))
        })?;
        if !hub.is_online(&device_id).await {
            return Err(ProbeError::Unavailable(format!("the model lives on {device_name}, which is offline")));
        }
        device_id
    } else {
        HUB_DEVICE.to_string()
    };

    let provider = hub.build_provider(&provider_cfg).map_err(|e| match e {
        ProviderError::NoKey(_) | ProviderError::Config(_) => ProbeError::BadConfig(e.to_string()),
        ProviderError::Unavailable(msg) => ProbeError::Unavailable(msg),
        other => ProbeError::Internal(other.to_string()),
    })?;

    let run_id = random_hex(4);
    let mut results = Vec::with_capacity(SUITE.len());
    let mut stopped_reason = None;

    for (idx, probe) in SUITE.iter().enumerate() {
        if idx > 0
            && let Some(why) = crate::limits::daily_budget_refusal(&hub.db)
        {
            stopped_reason = Some(why);
            break;
        }
        let item_id = format!("probe:{run_id}:{}", probe.id);
        let req = Request {
            model: model.model.clone(),
            system: PROBE_SYSTEM.into(),
            messages: vec![Message::user_text(probe.prompt)],
            tools: (probe.tools)(),
            max_tokens: probe.max_tokens,
        };
        let started = Instant::now();
        let outcome = tokio::time::timeout(PROBE_TIMEOUT, provider.complete(&req)).await;
        let latency_ms = started.elapsed().as_millis() as u64;

        let result = match outcome {
            Ok(Ok(resp)) => {
                let cost = hub.db.record_usage(
                    None,
                    Some(&item_id),
                    &model.provider_id,
                    &model.model,
                    resp.usage.input_tokens,
                    resp.usage.output_tokens,
                    resp.usage.cache_read_tokens,
                    resp.usage.cache_write_tokens,
                );
                crate::limits::after_daily_usage(hub, cost);
                let (passed, mut detail) = (probe.grade)(&to_probe_output(&resp));
                if !passed && matches!(resp.stop, providers::StopReason::MaxTokens) {
                    detail.push_str(" (stopped at the token limit)");
                }
                let tokens = resp.usage.input_tokens
                    + resp.usage.output_tokens
                    + resp.usage.cache_read_tokens
                    + resp.usage.cache_write_tokens;
                if let Err(e) = hub.db.record_probe_outcome(
                    &item_id,
                    &model.id,
                    &outcome_device_id,
                    probe.task_type,
                    passed,
                    latency_ms,
                    cost,
                    tokens,
                    &detail,
                ) {
                    tracing::warn!(model = %model.id, probe = probe.id, "recording probe outcome: {e:#}");
                }
                ProbeResult {
                    id: probe.id.into(),
                    task_type: probe.task_type.into(),
                    passed,
                    latency_ms,
                    input_tokens: resp.usage.input_tokens,
                    output_tokens: resp.usage.output_tokens,
                    cost,
                    detail,
                }
            }
            Ok(Err(ProviderError::Unavailable(msg))) if idx == 0 => {
                return Err(ProbeError::Unavailable(msg));
            }
            Ok(Err(ProviderError::NoKey(p))) if idx == 0 => {
                return Err(ProbeError::BadConfig(format!("no API key for provider {p}")));
            }
            Ok(Err(e)) => {
                let detail = format!("provider error: {e}");
                let _ = hub.db.record_probe_outcome(
                    &item_id,
                    &model.id,
                    &outcome_device_id,
                    probe.task_type,
                    false,
                    latency_ms,
                    0.0,
                    0,
                    &detail,
                );
                ProbeResult {
                    id: probe.id.into(),
                    task_type: probe.task_type.into(),
                    passed: false,
                    latency_ms,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost: 0.0,
                    detail,
                }
            }
            Err(_) => {
                let detail = format!("probe timed out after {}s", PROBE_TIMEOUT.as_secs());
                let _ = hub.db.record_probe_outcome(
                    &item_id,
                    &model.id,
                    &outcome_device_id,
                    probe.task_type,
                    false,
                    latency_ms,
                    0.0,
                    0,
                    &detail,
                );
                ProbeResult {
                    id: probe.id.into(),
                    task_type: probe.task_type.into(),
                    passed: false,
                    latency_ms,
                    input_tokens: 0,
                    output_tokens: 0,
                    cost: 0.0,
                    detail,
                }
            }
        };
        results.push(result);
    }

    let (proposal_id, proposal_text) = if !results.is_empty() {
        let (summary, reason) = summarize_probe_results(&results, SLOW_PROBE_MS);
        match hub.db.upsert_probe_proposal(&model.id, &summary, &reason) {
            Ok(Some(p)) => (Some(p.id), Some(p.text)),
            Ok(None) => (None, None),
            Err(e) => {
                tracing::warn!(model = %model.id, "updating probe use-case proposal: {e:#}");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let passed = results.iter().filter(|r| r.passed).count();
    let total = results.len();
    let total_latency_ms = results.iter().map(|r| r.latency_ms).sum();
    let total_tokens = results.iter().map(|r| r.input_tokens + r.output_tokens).sum();
    let total_cost = results.iter().map(|r| r.cost).sum();

    Ok(ProbeReport {
        model_id: model.id,
        passed,
        total,
        total_latency_ms,
        total_tokens,
        total_cost,
        results,
        proposal_id,
        proposal_text,
        stopped_reason,
    })
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new().route("/v1/models/{id}/probe", post(probe_model))
}

async fn probe_model(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match run_probes(&hub, &id).await {
        Ok(report) => Json(report).into_response(),
        Err(ProbeError::NotFound(msg)) => err(StatusCode::NOT_FOUND, msg),
        Err(ProbeError::Busy(msg)) => err(StatusCode::CONFLICT, msg),
        Err(ProbeError::BudgetExceeded(msg)) => err(StatusCode::TOO_MANY_REQUESTS, msg),
        Err(ProbeError::Unavailable(msg)) => err(StatusCode::SERVICE_UNAVAILABLE, msg),
        Err(ProbeError::BadConfig(msg)) => err(StatusCode::BAD_REQUEST, msg),
        Err(ProbeError::Internal(msg)) => err(StatusCode::INTERNAL_SERVER_ERROR, msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_protocol::{Limits, ModelEntry, ProviderConfig};
    use serde_json::{Value, json};

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-probes-{tag}-{}", random_hex(4)))
    }

    fn partial_script() -> Value {
        json!({
            "rules": [
                {"last": "Python snippet", "reply": [{"type": "text", "text": "20"}]},
                {"last": "software backups", "reply": [{"type": "text", "text": "Backups keep your data safe. Keep copies." }]},
                {"last": "second tallest", "reply": [{"type": "text", "text": "Alice"}]},
                {"last": "banana: 9", "reply": [{"type": "text", "text": "not json"}]},
                {"last": "payments service", "reply": [{"type": "tool_use", "name": "lookup_service", "input": {"service": "payments", "env": "prod"}}]}
            ],
            "default": [{"type": "text", "text": "fallback"}]
        })
    }

    #[tokio::test]
    async fn probes_record_distinct_stats_usage_and_update_proposals() {
        let dir = tmp("partial");
        let db = crate::db::Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "s".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": partial_script()}),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "test-model".into(),
            provider_id: "s".into(),
            model: "fake-1".into(),
            input_price: 10.0,
            output_price: 20.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: vec!["thread".into()],
            local: false,
            modalities: vec!["text".into()],
            ..Default::default()
        })
        .unwrap();
        let hub = Hub::new(db, dir.clone());

        let concurrent = begin_probe(&hub, "test-model").unwrap();
        let busy = run_probes(&hub, "test-model").await.unwrap_err();
        assert!(matches!(busy, ProbeError::Busy(_)), "{busy:?}");
        drop(concurrent);

        let report = run_probes(&hub, "test-model").await.unwrap();
        assert_eq!((report.passed, report.total), (3, 5));
        assert!(report.total_tokens > 0);
        assert!(report.total_cost > 0.0);
        assert_eq!(report.stopped_reason, None);
        let by_id = |id: &str| report.results.iter().find(|r| r.id == id).unwrap().passed;
        assert!(by_id("code"));
        assert!(!by_id("writing"));
        assert!(by_id("research"));
        assert!(!by_id("data"));
        assert!(by_id("tool_use"));

        // Outcomes are recorded with source='probe' and kept separate from thread outcomes.
        let reg = hub.db.registry_stats();
        assert!(reg.models.is_empty(), "probes must not pollute thread outcome stats");
        assert!(reg.devices.is_empty(), "probes must not pollute device thread stats");
        assert_eq!(reg.probes.len(), 6); // (all) + 5 task types
        let all = reg.probes.iter().find(|s| s.task_type.is_empty()).unwrap();
        assert_eq!(all.source, "probe");
        assert_eq!((all.done, all.failed, all.successes), (3, 2, 3));
        assert!(all.mean_cost.unwrap() > 0.0);
        assert!(all.mean_tokens.unwrap() > 0.0);
        assert!(all.median_duration_s.is_some());

        // Usage was recorded and counts toward daily spend.
        let usage = hub.db.usage_totals(0, i64::MAX as u64, "model", None, None).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].calls, 5);
        assert!(usage[0].cost > 0.0);

        // Pending proposal was created and summarizes strengths/failures.
        let proposal_id = report.proposal_id.unwrap();
        let p = hub.db.proposal(proposal_id).unwrap();
        assert_eq!(p.status, "pending");
        assert_eq!(p.source, "probe");
        assert_eq!(p.text, "Passed code, research and tool-use probes; failed writing and data");
        assert!(p.reason.contains("3/5 passed"), "{}", p.reason);

        // Orchestrator list_models briefly summarizes probe results.
        let overview = crate::registry::models_overview(&hub.db);
        let entry = &overview["models"][0];
        let summary = entry["probes"].as_str().unwrap();
        assert!(summary.contains("3/5 passed (60%)"), "{summary}");
        assert!(summary.contains("code pass") && summary.contains("writing fail"), "{summary}");

        // Re-running probes updates the same pending proposal in place without duplicating text.
        let second = run_probes(&hub, "test-model").await.unwrap();
        assert_eq!(second.proposal_id, Some(proposal_id));
        let p2 = hub.db.proposal(proposal_id).unwrap();
        assert_eq!(p2.text, "Passed code, research and tool-use probes; failed writing and data");
        assert_eq!(hub.db.proposals(Some("pending")).len(), 1);

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn probes_respect_daily_budget_before_and_during_suite() {
        let dir = tmp("budget");
        let db = crate::db::Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "s".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": partial_script()}),
        })
        .unwrap();
        // 20 output tokens at $1,000 / 1M = $0.02 per scripted call.
        db.upsert_model(&ModelEntry {
            id: "pricey".into(),
            provider_id: "s".into(),
            model: "fake-1".into(),
            input_price: 0.0,
            output_price: 1000.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: vec![],
            local: false,
            modalities: vec![],
            ..Default::default()
        })
        .unwrap();
        // Budget allows 2 calls ($0.04) before hitting $0.03.
        db.set_limits(&Limits { daily_budget_usd: Some(0.03), ..Default::default() }).unwrap();
        let hub = Hub::new(db, dir.clone());

        let partial = run_probes(&hub, "pricey").await.unwrap();
        assert_eq!(partial.total, 2, "stopped mid-suite once daily budget was spent");
        assert!(partial.stopped_reason.as_deref().unwrap().contains("daily budget of $0.03"));

        // Now daily budget is spent: a fresh probe call is refused before making any model call.
        let calls_before = hub.db.usage_totals(0, i64::MAX as u64, "model", None, None).unwrap()[0].calls;
        let err = run_probes(&hub, "pricey").await.unwrap_err();
        assert!(matches!(err, ProbeError::BudgetExceeded(_)), "{err:?}");
        assert!(err.to_string().contains("daily budget of $0.03"));
        let calls_after = hub.db.usage_totals(0, i64::MAX as u64, "model", None, None).unwrap()[0].calls;
        assert_eq!(calls_before, calls_after, "refused probe must not call provider");

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
