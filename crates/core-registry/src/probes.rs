//! Built-in deterministic probe suite definitions, graders and result summaries.
//!
//! Each probe is short (cheap) and graded deterministically — exact output,
//! constrained text structure, JSON shape/values, or a structured tool call —
//! never model-graded. Provider calls live in the caller (e.g. `hub`).

use core_protocol::{ProbeResult, ToolDef};
use serde_json::{Value, json};

use crate::registry::format_duration_s;

pub const PROBE_SYSTEM: &str =
    "You are being evaluated on a short probe task. Follow formatting and tool instructions strictly and concisely.";

/// Latency threshold above which a probe is noted as slow in the drafted use-case proposal.
pub const SLOW_PROBE_MS: u64 = 10_000;

/// One structured tool call returned by a model during a probe.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeToolCall {
    pub name: String,
    pub input: Value,
}

/// Plain-data view of a model's response for deterministic probe grading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeOutput {
    pub text: String,
    pub tool_calls: Vec<ProbeToolCall>,
}

/// One built-in probe task with a deterministic grader.
pub struct ProbeTask {
    pub id: &'static str,
    pub task_type: &'static str,
    pub label: &'static str,
    pub prompt: &'static str,
    pub max_tokens: u32,
    pub tools: fn() -> Vec<ToolDef>,
    pub grade: fn(&ProbeOutput) -> (bool, String),
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
        max_tokens: 2048,
        tools: no_tools,
        grade: grade_code,
    },
    ProbeTask {
        id: "writing",
        task_type: "writing",
        label: "writing",
        prompt: "Write exactly two short sentences explaining why software backups matter. Do not use the word 'data'. Use at most 40 words.",
        max_tokens: 2048,
        tools: no_tools,
        grade: grade_writing,
    },
    ProbeTask {
        id: "research",
        task_type: "research",
        label: "research",
        prompt: "Alice is taller than Bob. Carol is shorter than Bob. Dave is taller than Alice. Who is the second tallest? Reply with ONLY the name.",
        max_tokens: 2048,
        tools: no_tools,
        grade: grade_research,
    },
    ProbeTask {
        id: "data",
        task_type: "data",
        label: "data",
        prompt: "Summarize these sales as a JSON object with keys \"total\" (number) and \"top\" (string, highest item). Reply with ONLY valid JSON.\n\napple: 4\nbanana: 9\ncherry: 2",
        max_tokens: 2048,
        tools: no_tools,
        grade: grade_data,
    },
    ProbeTask {
        id: "tool_use",
        task_type: "tool_use",
        label: "tool-use",
        prompt: "Check the status of the payments service in prod using the available tool.",
        max_tokens: 2048,
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

pub fn grade_code(resp: &ProbeOutput) -> (bool, String) {
    let cleaned = strip_code_fence(&resp.text).trim_end_matches('.').trim();
    if cleaned == "20" {
        (true, "exact output 20".into())
    } else {
        (false, format!("expected 20, got {:?}", snippet(&resp.text)))
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

pub fn grade_writing(resp: &ProbeOutput) -> (bool, String) {
    let trimmed = resp.text.trim();
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

pub fn grade_research(resp: &ProbeOutput) -> (bool, String) {
    let cleaned = strip_code_fence(&resp.text).trim_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '.'));
    if cleaned.eq_ignore_ascii_case("alice") {
        (true, "exact answer Alice".into())
    } else {
        (false, format!("expected Alice, got {:?}", snippet(&resp.text)))
    }
}

pub fn grade_data(resp: &ProbeOutput) -> (bool, String) {
    let cleaned = strip_code_fence(&resp.text);
    let parsed: Value = match serde_json::from_str(cleaned) {
        Ok(v) => v,
        Err(e) => return (false, format!("invalid JSON: {e}")),
    };
    let Some(obj) = parsed.as_object() else {
        return (false, "expected a JSON object".into());
    };
    let total_ok = obj
        .get("total")
        .is_some_and(|v| v.as_i64() == Some(15) || v.as_f64().is_some_and(|n| (n - 15.0).abs() < 1e-9));
    if !total_ok {
        return (false, format!("expected total=15, got {:?}", obj.get("total")));
    }
    let top_ok = obj.get("top").and_then(Value::as_str).is_some_and(|s| s.trim().eq_ignore_ascii_case("banana"));
    if !top_ok {
        return (false, format!("expected top=\"banana\", got {:?}", obj.get("top")));
    }
    (true, "valid JSON {total: 15, top: \"banana\"}".into())
}

pub fn grade_tool_use(resp: &ProbeOutput) -> (bool, String) {
    if resp.tool_calls.is_empty() {
        return (false, format!("no tool call made (text: {:?})", snippet(&resp.text)));
    }
    for call in &resp.tool_calls {
        if call.name != "lookup_service" {
            continue;
        }
        let service_ok =
            call.input.get("service").and_then(Value::as_str).is_some_and(|s| s.trim().eq_ignore_ascii_case("payments"));
        let env_ok =
            call.input.get("env").and_then(Value::as_str).is_some_and(|s| s.trim().eq_ignore_ascii_case("prod"));
        if service_ok && env_ok {
            return (true, "called lookup_service(service=\"payments\", env=\"prod\")".into());
        }
        return (false, format!("lookup_service called with wrong arguments: {}", call.input));
    }
    let names: Vec<&str> = resp.tool_calls.iter().map(|c| c.name.as_str()).collect();
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
    let slow: Vec<&str> =
        results.iter().filter(|r| r.latency_ms >= slow_threshold_ms).map(|r| label_for(&r.task_type)).collect();

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

#[cfg(test)]
mod tests {
    use super::*;

    fn text_resp(text: &str) -> ProbeOutput {
        ProbeOutput { text: text.into(), tool_calls: Vec::new() }
    }

    fn tool_resp(name: &str, input: Value) -> ProbeOutput {
        ProbeOutput { text: String::new(), tool_calls: vec![ProbeToolCall { name: name.into(), input }] }
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
}
