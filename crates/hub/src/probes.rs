//! Built-in deterministic probe suite for newly added or imported models.
//!
//! Each probe is short (cheap) and graded deterministically — exact output,
//! constrained text structure, JSON shape/values, or a structured tool call —
//! never model-graded. Probes run through the hub's normal provider path (so
//! device-hosted models work), count against usage and the daily budget, record
//! per-probe outcomes in `thread_outcomes` with `source = 'probe'`, and create
//! or update a pending use-case proposal for the user to confirm.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use core_protocol::{ProbeReport, ProbeResult, ToolDef};
use providers::{Block, Message, ProviderError, Request, Response as ModelResponse};
use serde_json::{Value, json};

use crate::db::random_hex;
use crate::registry::{deny_unless_admin, format_duration_s};
use crate::server::{HUB_DEVICE, Hub, err};

const PROBE_SYSTEM: &str =
    "You are being evaluated on a short probe task. Follow formatting and tool instructions strictly and concisely.";

/// Per-probe call timeout so a stalled endpoint cannot hang an entire suite forever.
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// Latency threshold above which a probe is noted as slow in the drafted use-case proposal.
pub const SLOW_PROBE_MS: u64 = 10_000;

/// One built-in probe task with a deterministic grader.
pub struct ProbeTask {
    pub id: &'static str,
    pub task_type: &'static str,
    pub label: &'static str,
    pub prompt: &'static str,
    pub max_tokens: u32,
    pub tools: fn() -> Vec<ToolDef>,
    pub grade: fn(&ModelResponse) -> (bool, String),
}

fn no_tools() -> Vec<ToolDef> {
    Vec::new()
}

fn lookup_service_tools() -> Vec<ToolDef> {
    vec![ToolDef {
        name: "lookup_service".into(),
        description: "Check the health status of a named service in a given environment.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "service": { "type": "string", "description": "Service name" },
                "env": { "type": "string", "enum": ["dev", "staging", "prod"] }
            },
            "required": ["service", "env"]
        }),
    }]
}

/// The built-in 5-task probe suite: code, writing, research/reasoning, data, and tool use.
pub const SUITE: &[ProbeTask] = &[
    ProbeTask {
        id: "code",
        task_type: "code",
        label: "code",
        prompt: "What does this Python snippet print? Reply with ONLY the output.\n\nnums = [1, 2, 3, 4]\nprint(sum(x * x for x in nums if x % 2 == 0))",
        max_tokens: 64,
        tools: no_tools,
        grade: grade_code,
    },
    ProbeTask {
        id: "writing",
        task_type: "writing",
        label: "writing",
        prompt: "Write exactly two short sentences explaining why software backups matter. Do not use the word 'data'. Use at most 40 words.",
        max_tokens: 160,
        tools: no_tools,
        grade: grade_writing,
    },
    ProbeTask {
        id: "research",
        task_type: "research",
        label: "research",
        prompt: "Alice is taller than Bob. Carol is shorter than Bob. Dave is taller than Alice. Who is the second tallest? Reply with ONLY the name.",
        max_tokens: 64,
        tools: no_tools,
        grade: grade_research,
    },
    ProbeTask {
        id: "data",
        task_type: "data",
        label: "data",
        prompt: "Summarize these sales as a JSON object with keys \"total\" (number) and \"top\" (string, highest item). Reply with ONLY valid JSON.\n\napple: 4\nbanana: 9\ncherry: 2",
        max_tokens: 128,
        tools: no_tools,
        grade: grade_data,
    },
    ProbeTask {
        id: "tool_use",
        task_type: "tool_use",
        label: "tool-use",
        prompt: "Check the status of the payments service in prod using the available tool.",
        max_tokens: 128,
        tools: lookup_service_tools,
        grade: grade_tool_use,
    },
];

fn snippet(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 60 {
        format!("{}…", flat.chars().take(60).collect::<String>())
    } else {
        flat
    }
}

/// Strip outer whitespace and optional markdown code fences or inline backticks.
fn strip_code_fence(raw: &str) -> &str {
    let s = raw.trim();
    if let Some(rest) = s.strip_prefix("```")
        && let Some(inner) = rest.strip_suffix("```")
    {
        let body = match inner.find('\n') {
            Some(i) => &inner[i + 1..],
            None => inner,
        };
        return body.trim();
    }
    if let Some(inner) = s.strip_prefix('`').and_then(|t| t.strip_suffix('`'))
        && !inner.contains('`')
    {
        return inner.trim();
    }
    s
}

fn grade_code(resp: &ModelResponse) -> (bool, String) {
    let text = resp.message.text();
    let cleaned = strip_code_fence(&text).trim_end_matches('.').trim();
    if cleaned == "20" {
        (true, "exact output 20".into())
    } else {
        (false, format!("expected 20, got {:?}", snippet(&text)))
    }
}

/// True if `word` appears as a whole ASCII word (case-insensitive) in `text`.
fn contains_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric()).any(|token| token.eq_ignore_ascii_case(word))
}

/// Count sentences ended by `.`, `!`, or `?` (ignoring decimal points like `3.5`).
/// Returns the word count of each sentence.
fn sentence_word_counts(text: &str) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        let is_decimal = b == b'.'
            && i > 0
            && i + 1 < bytes.len()
            && bytes[i - 1].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit();
        if matches!(b, b'.' | b'!' | b'?') && !is_decimal {
            let segment = text[start..i].trim();
            if !segment.is_empty() {
                counts.push(segment.split_whitespace().count());
            }
            while i < bytes.len() && matches!(bytes[i], b'.' | b'!' | b'?') {
                i += 1;
            }
            start = i;
            continue;
        }
        i += 1;
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        counts.push(tail.split_whitespace().count());
    }
    counts
}

fn grade_writing(resp: &ModelResponse) -> (bool, String) {
    let text = resp.message.text();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return (false, "empty response".into());
    }
    if contains_word(trimmed, "data") {
        return (false, "used forbidden word 'data'".into());
    }
    let words = trimmed.split_whitespace().count();
    if !(6..=40).contains(&words) {
        return (false, format!("expected 6..=40 words, got {words}"));
    }
    let sentences = sentence_word_counts(trimmed);
    if sentences.len() != 2 || sentences.iter().any(|&w| w < 2) {
        return (false, format!("expected 2 sentences, got {}", sentences.len()));
    }
    if !trimmed.ends_with(['.', '!', '?']) {
        return (false, "second sentence missing terminal punctuation".into());
    }
    (true, format!("2 sentences, {words} words, avoided 'data'"))
}

fn grade_research(resp: &ModelResponse) -> (bool, String) {
    let text = resp.message.text();
    let cleaned = strip_code_fence(&text)
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '.'));
    if cleaned.eq_ignore_ascii_case("alice") {
        (true, "exact answer Alice".into())
    } else {
        (false, format!("expected Alice, got {:?}", snippet(&text)))
    }
}

fn grade_data(resp: &ModelResponse) -> (bool, String) {
    let text = resp.message.text();
    let cleaned = strip_code_fence(&text);
    let parsed: Value = match serde_json::from_str(cleaned) {
        Ok(v) => v,
        Err(e) => return (false, format!("invalid JSON: {e}")),
    };
    let Some(obj) = parsed.as_object() else {
        return (false, "expected a JSON object".into());
    };
    let total_ok = obj.get("total").is_some_and(|v| {
        v.as_i64() == Some(15) || v.as_f64().is_some_and(|n| (n - 15.0).abs() < 1e-9)
    });
    if !total_ok {
        return (false, format!("expected total=15, got {:?}", obj.get("total")));
    }
    let top_ok = obj
        .get("top")
        .and_then(Value::as_str)
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("banana"));
    if !top_ok {
        return (false, format!("expected top=\"banana\", got {:?}", obj.get("top")));
    }
    (true, "valid JSON {total: 15, top: \"banana\"}".into())
}

fn grade_tool_use(resp: &ModelResponse) -> (bool, String) {
    let calls: Vec<(&str, &Value)> = resp
        .message
        .content
        .iter()
        .filter_map(|b| match b {
            Block::ToolUse { name, input, .. } => Some((name.as_str(), input)),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return (false, format!("no tool call made (text: {:?})", snippet(&resp.message.text())));
    }
    for (name, input) in &calls {
        if *name != "lookup_service" {
            continue;
        }
        let service_ok = input
            .get("service")
            .and_then(Value::as_str)
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("payments"));
        let env_ok = input
            .get("env")
            .and_then(Value::as_str)
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("prod"));
        if service_ok && env_ok {
            return (true, "called lookup_service(service=\"payments\", env=\"prod\")".into());
        }
        return (false, format!("lookup_service called with wrong arguments: {input}"));
    }
    let names: Vec<&str> = calls.into_iter().map(|(n, _)| n).collect();
    (false, format!("expected lookup_service call, got {}", names.join(", ")))
}

fn label_for(task_type: &str) -> &str {
    SUITE.iter().find(|p| p.task_type == task_type || p.id == task_type).map(|p| p.label).unwrap_or(task_type)
}

fn join_natural(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [a, b] => format!("{a} and {b}"),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// Build `(proposed_use_cases_summary, proposal_reason)` from probe results.
pub fn summarize_probe_results(results: &[ProbeResult], slow_threshold_ms: u64) -> (String, String) {
    let passed: Vec<&str> = results.iter().filter(|r| r.passed).map(|r| label_for(&r.task_type)).collect();
    let failed: Vec<&str> = results.iter().filter(|r| !r.passed).map(|r| label_for(&r.task_type)).collect();
    let slow: Vec<&str> = results
        .iter()
        .filter(|r| r.latency_ms >= slow_threshold_ms)
        .map(|r| label_for(&r.task_type))
        .collect();

    let mut clauses = Vec::new();
    if passed.len() == results.len() && !results.is_empty() {
        clauses.push(format!("Passed all {} probes ({})", results.len(), join_natural(&passed)));
    } else if !passed.is_empty() {
        let noun = if passed.len() == 1 { "probe" } else { "probes" };
        clauses.push(format!("Passed {} {noun}", join_natural(&passed)));
    } else {
        clauses.push(format!("Failed all {} probes", results.len()));
    }
    if !slow.is_empty() {
        clauses.push(format!("slow on {}", join_natural(&slow)));
    }
    if !passed.is_empty() && !failed.is_empty() {
        clauses.push(format!("failed {}", join_natural(&failed)));
    }
    let summary = clauses.join("; ");

    let total_ms: u64 = results.iter().map(|r| r.latency_ms).sum();
    let total_cost: f64 = results.iter().map(|r| r.cost).sum();
    let per_probe: Vec<String> = results
        .iter()
        .map(|r| {
            format!(
                "{} {} ({})",
                label_for(&r.task_type),
                if r.passed { "pass" } else { "fail" },
                format_duration_s(r.latency_ms as f64 / 1000.0)
            )
        })
        .collect();
    let reason = format!(
        "Probes ({}/{} passed, {} total, ${:.4}): {}.",
        passed.len(),
        results.len(),
        format_duration_s(total_ms as f64 / 1000.0),
        total_cost,
        per_probe.join(", ")
    );
    (summary, reason)
}

#[derive(Debug)]
pub enum ProbeError {
    NotFound(String),
    BudgetExceeded(String),
    Unavailable(String),
    BadConfig(String),
    Internal(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s)
            | Self::BudgetExceeded(s)
            | Self::Unavailable(s)
            | Self::BadConfig(s)
            | Self::Internal(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Run the built-in probe suite against `model_key`, record usage and `source = 'probe'`
/// outcomes, and create or update a pending use-case proposal.
pub async fn run_probes(hub: &Arc<Hub>, model_key: &str) -> Result<ProbeReport, ProbeError> {
    let model = hub
        .db
        .find_model(model_key)
        .ok_or_else(|| ProbeError::NotFound(format!("no model {model_key:?} in the registry")))?;
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
                let (passed, detail) = (probe.grade)(&resp);
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
    use providers::{Role, StopReason, Usage};

    fn text_resp(text: &str) -> ModelResponse {
        ModelResponse {
            message: Message { role: Role::Assistant, content: vec![Block::Text { text: text.into() }], raw: None },
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn tool_resp(name: &str, input: Value) -> ModelResponse {
        ModelResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![Block::ToolUse { id: "c1".into(), name: name.into(), input }],
                raw: None,
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    #[test]
    fn deterministic_graders_accept_valid_and_reject_invalid_answers() {
        assert!(grade_code(&text_resp("20")).0);
        assert!(grade_code(&text_resp("```text\n20\n```")).0);
        assert!(grade_code(&text_resp("`20`")).0);
        assert!(!grade_code(&text_resp("30")).0);
        assert!(!grade_code(&text_resp("The answer is 20")).0);

        assert!(
            grade_writing(&text_resp(
                "Backups protect your work from hardware failure and accidental deletion. Regular copies let you recover quickly without starting over."
            ))
            .0
        );
        assert!(!grade_writing(&text_resp("Backups protect your data from loss. Recover quickly later.")).0);
        assert!(!grade_writing(&text_resp("Backups protect your work from hardware failure.")).0);
        assert!(!grade_writing(&text_resp("One. Two. Three.")).0);

        assert!(grade_research(&text_resp("Alice")).0);
        assert!(grade_research(&text_resp("alice.")).0);
        assert!(!grade_research(&text_resp("Dave")).0);

        assert!(grade_data(&text_resp(r#"{"total": 15, "top": "banana"}"#)).0);
        assert!(grade_data(&text_resp("```json\n{\"total\": 15, \"top\": \"Banana\"}\n```")).0);
        assert!(!grade_data(&text_resp(r#"{"total": 14, "top": "banana"}"#)).0);
        assert!(!grade_data(&text_resp(r#"{"total": 15, "top": "apple"}"#)).0);
        assert!(!grade_data(&text_resp("not json")).0);

        assert!(grade_tool_use(&tool_resp("lookup_service", json!({"service": "payments", "env": "prod"}))).0);
        assert!(!grade_tool_use(&tool_resp("lookup_service", json!({"service": "payments", "env": "staging"}))).0);
        assert!(!grade_tool_use(&tool_resp("other_tool", json!({"service": "payments", "env": "prod"}))).0);
        assert!(!grade_tool_use(&text_resp("Checking payments in prod.")).0);
    }

    #[test]
    fn summary_captures_strengths_slowness_and_failures() {
        let mk = |id: &str, task_type: &str, passed: bool, latency_ms: u64| ProbeResult {
            id: id.into(),
            task_type: task_type.into(),
            passed,
            latency_ms,
            input_tokens: 20,
            output_tokens: 10,
            cost: 0.0001,
            detail: String::new(),
        };
        let results = vec![
            mk("code", "code", true, 200),
            mk("writing", "writing", true, 12_000),
            mk("research", "research", false, 300),
            mk("data", "data", false, 250),
            mk("tool_use", "tool_use", true, 400),
        ];
        let (summary, reason) = summarize_probe_results(&results, SLOW_PROBE_MS);
        assert_eq!(summary, "Passed code, writing and tool-use probes; slow on writing; failed research and data");
        assert!(reason.contains("3/5 passed"), "{reason}");
        assert!(reason.contains("writing pass (12s)"), "{reason}");
    }

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
        })
        .unwrap();
        let hub = Hub::new(db, dir.clone());

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
        let usage = hub.db.usage_totals(0, i64::MAX as u64, "model", None).unwrap();
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
        })
        .unwrap();
        // Budget allows 2 calls ($0.04) before hitting $0.03.
        db.set_limits(&Limits { daily_budget_usd: Some(0.03), ..Default::default() }).unwrap();
        let hub = Hub::new(db, dir.clone());

        let partial = run_probes(&hub, "pricey").await.unwrap();
        assert_eq!(partial.total, 2, "stopped mid-suite once daily budget was spent");
        assert!(partial.stopped_reason.as_deref().unwrap().contains("daily budget of $0.03"));

        // Now daily budget is spent: a fresh probe call is refused before making any model call.
        let calls_before = hub.db.usage_totals(0, i64::MAX as u64, "model", None).unwrap()[0].calls;
        let err = run_probes(&hub, "pricey").await.unwrap_err();
        assert!(matches!(err, ProbeError::BudgetExceeded(_)), "{err:?}");
        assert!(err.to_string().contains("daily budget of $0.03"));
        let calls_after = hub.db.usage_totals(0, i64::MAX as u64, "model", None).unwrap()[0].calls;
        assert_eq!(calls_before, calls_after, "refused probe must not call provider");

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
