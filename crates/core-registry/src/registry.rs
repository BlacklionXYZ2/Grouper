//! The model registry's learning, per Proposal v3: thread outcomes and the stats
//! drawn from them (per model and per device, by task type), the orchestrator's
//! grades and your thumbs up/down, use-case proposals that wait for your approval
//! so the table doesn't drift, and priority rules ("Sensitive data: local only",
//! "drafts: cheapest") that choose and restrict the model for each thread.
//!
//! Tables: `thread_meta` (task type and tags given at start), `thread_outcomes`
//! (one row per finished thread), `registry_proposals`, `priority_rules`; and the
//! `local` column on `models`.
//!
//! Routing ([`route_model`] / [`route_model_from`]): enabled rules that apply to the
//! task (by tag and task type) filter the models first (`require` / `forbid`); what's
//! left is ordered by the rules' `prefer`s, then by the task type's record (models
//! with at least [`MIN_ROUTING_RUNS`] finished threads and a success rate of 50% or
//! more first, by success rate, grade, then cost; unproven models next; poor records
//! last), then by the registry default for threads. A model the orchestrator names
//! itself must still pass the filters, so privacy rules can't be bypassed.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail};
use core_protocol::{ModelEntry, OutcomeStats, PriorityRule, RegistryProposal, RegistryStats, RuleSpec, ThreadInfo};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::now;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS thread_meta (
  thread_id TEXT PRIMARY KEY, task_type TEXT NOT NULL DEFAULT '', tags TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS thread_outcomes (
  thread_id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, model_id TEXT NOT NULL, device_id TEXT NOT NULL,
  task_type TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, grade INTEGER, grade_note TEXT, user_rating INTEGER,
  duration_s INTEGER NOT NULL DEFAULT 0, cost REAL NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL, source TEXT NOT NULL DEFAULT 'thread', duration_ms INTEGER);
CREATE INDEX IF NOT EXISTS thread_outcomes_model ON thread_outcomes (model_id, task_type);
CREATE TABLE IF NOT EXISTS registry_proposals (
  id INTEGER PRIMARY KEY AUTOINCREMENT, model_id TEXT NOT NULL, field TEXT NOT NULL, text TEXT NOT NULL,
  reason TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', source TEXT NOT NULL, created INTEGER NOT NULL,
  decided INTEGER);
CREATE TABLE IF NOT EXISTS priority_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, rule TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
  created INTEGER NOT NULL);
";

/// Create this module's tables and add newer columns to older databases.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    for (table, column, sql) in [
        ("models", "local", "ALTER TABLE models ADD COLUMN local INTEGER NOT NULL DEFAULT 0;"),
        (
            "thread_outcomes",
            "source",
            "ALTER TABLE thread_outcomes ADD COLUMN source TEXT NOT NULL DEFAULT 'thread';",
        ),
        ("thread_outcomes", "duration_ms", "ALTER TABLE thread_outcomes ADD COLUMN duration_ms INTEGER;"),
    ] {
        let table_exists: bool =
            conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?.exists([table])?;
        if !table_exists {
            continue;
        }
        let has: bool =
            conn.prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?.exists([column])?;
        if !has {
            conn.execute_batch(sql)?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS thread_outcomes_source ON thread_outcomes (source, model_id, task_type);",
    )?;
    Ok(())
}

/// Suggested task types (any short tag works).
pub const TASK_TYPES: &[&str] = &["code", "research", "writing", "data", "ops"];
/// Finished threads of a task type before a model's record steers routing.
pub const MIN_ROUTING_RUNS: u64 = 3;
/// Finished threads of a task type before the hub proposes a use-case change, and
/// the success rates that trigger it.
pub const PROPOSAL_MIN_RUNS: u64 = 5;
pub const PROPOSAL_HIGH: f64 = 0.8;
pub const PROPOSAL_LOW: f64 = 0.3;
pub const MAX_USE_CASES_CHARS: usize = 500;

/// Rule values for `require` / `forbid`, and for `prefer`.
const PROPERTIES: &[&str] = &["local", "remote", "free", "paid", "harness", "api"];
const PREFERENCES: &[&str] = &["cheapest", "local", "free"];

pub fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Lower-cased, trimmed, de-duplicated tags.
pub fn clean_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags.iter().flat_map(|t| t.split(',')).map(norm).filter(|t| !t.is_empty()) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The host of a URL such as `http://user@[::1]:8080/v1`, lower-cased.
fn url_host(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://").map(|(_, r)| r).unwrap_or(url.trim());
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => authority.split(':').next()?,
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether a provider URL points at this machine or the local network.
pub fn is_local_url(url: &str) -> bool {
    url_host(url).is_some_and(|h| {
        h == "localhost" || h == "::1" || h.starts_with("127.") || h.ends_with(".local") || h.ends_with(".localhost")
    })
}

/// Whether a model counts as local: flagged, hosted on a device, or its provider's URL is local.
/// Harness models run a CLI on a device but talk to cloud models, so they never count as local.
pub fn model_is_local(model: &ModelEntry, provider_url: Option<&str>, via_device: bool) -> bool {
    if model.is_harness() {
        return false;
    }
    model.local || via_device || provider_url.is_some_and(is_local_url)
}

/// Precompute each model's effective local flag from provider URLs and device-hosted provider ids.
pub fn local_map(
    models: &[ModelEntry],
    provider_urls: &HashMap<String, Option<String>>,
    device_providers: &HashSet<String>,
) -> HashMap<String, bool> {
    models
        .iter()
        .map(|m| {
            let url = provider_urls.get(&m.provider_id).and_then(|u| u.as_deref());
            (m.id.clone(), model_is_local(m, url, device_providers.contains(&m.provider_id)))
        })
        .collect()
}

/// Whether a model may orchestrate a project chat (or hold the `orchestrator` role default):
/// any API model, or a harness that explicitly supports MCP orchestration (`can_orchestrate`).
pub fn can_orchestrate_project(model: &ModelEntry) -> bool {
    !model.is_harness() || model.can_orchestrate
}

/// The role default excluding hidden models and providers that cannot currently serve an
/// automatically selected model (for example, an offline device host). Standard chat prefers a free model.
/// For the `orchestrator` role, harnesses without `can_orchestrate` are dropped before every fallback step.
pub fn default_model_from(
    models: &[ModelEntry],
    role: &str,
    unavailable_providers: &HashSet<String>,
) -> Option<ModelEntry> {
    let available: Vec<&ModelEntry> = models
        .iter()
        .filter(|model| {
            !model.hidden
                && !unavailable_providers.contains(&model.provider_id)
                && (role != "orchestrator" || can_orchestrate_project(model))
        })
        .collect();
    let by_role = |r: &str| available.iter().find(|m| m.roles.iter().any(|x| x == r)).copied().cloned();
    match role {
        "standard" => by_role("standard").or_else(|| available.iter().find(|m| m.free).copied().cloned()),
        "thread" => by_role("thread"),
        _ => None,
    }
    .or_else(|| by_role(role))
    .or_else(|| {
        available
            .iter()
            .find(|m| can_orchestrate_project(m) && m.roles.iter().any(|x| x == "orchestrator"))
            .copied()
            .cloned()
    })
    .or_else(|| available.first().copied().cloned())
}

/// Find a model by registry id, or by the provider's model name (preferring visible rows
/// when matching by provider model name; still resolves hidden rows for history and usage).
pub fn find_model_in(models: &[ModelEntry], key: &str) -> Option<ModelEntry> {
    models
        .iter()
        .find(|m| m.id == key)
        .or_else(|| models.iter().find(|m| !m.hidden && m.model == key))
        .or_else(|| models.iter().find(|m| m.model == key))
        .cloned()
}

/// Rough price for comparing models: USD per million input plus output tokens.
fn price(m: &ModelEntry) -> f64 {
    if m.free { 0.0 } else { m.input_price + m.output_price }
}

fn has_property(m: &ModelEntry, local: bool, property: &str) -> bool {
    match property {
        "local" => local,
        "remote" => !local,
        "free" => m.free,
        "paid" => !m.free,
        "harness" => m.is_harness(),
        "api" => !m.is_harness(),
        _ => false,
    }
}

/// Check and normalise a rule.
pub fn validate_rule(rule: &RuleSpec) -> Result<RuleSpec> {
    let clean = |v: &Option<String>| v.as_deref().map(norm).filter(|s| !s.is_empty());
    let r = RuleSpec {
        when_tag: clean(&rule.when_tag),
        when_task_type: clean(&rule.when_task_type),
        require: clean(&rule.require),
        forbid: clean(&rule.forbid),
        prefer: clean(&rule.prefer),
    };
    for v in [&r.require, &r.forbid].into_iter().flatten() {
        if !PROPERTIES.contains(&v.as_str()) {
            bail!("require and forbid take one of {}; got {v:?}", PROPERTIES.join(", "));
        }
    }
    if let Some(p) = &r.prefer
        && !PREFERENCES.contains(&p.as_str())
    {
        bail!("prefer takes one of {}; got {p:?}", PREFERENCES.join(", "));
    }
    if r.require.is_none() && r.forbid.is_none() && r.prefer.is_none() {
        bail!("a rule needs require, forbid or prefer");
    }
    Ok(r)
}

fn rule_applies(r: &RuleSpec, task_type: &str, tags: &[String]) -> bool {
    r.when_tag.as_ref().is_none_or(|t| tags.contains(t)) && r.when_task_type.as_ref().is_none_or(|t| t == task_type)
}

/// `#1 "Sensitive data: local only" (require local)`
fn describe_rule(r: &PriorityRule) -> String {
    let mut effects = Vec::new();
    for (k, v) in [("require", &r.rule.require), ("forbid", &r.rule.forbid), ("prefer", &r.rule.prefer)] {
        if let Some(v) = v {
            effects.push(format!("{k} {v}"));
        }
    }
    format!("#{} \"{}\" ({})", r.id, r.text, effects.join(", "))
}

/// Which model a thread gets, and why (`None` when the orchestrator named it).
#[derive(Debug, Clone)]
pub struct Route {
    pub model: ModelEntry,
    pub reason: Option<String>,
}

/// Group outcome stats by model or by device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Model,
    Device,
}

#[derive(Debug, Clone)]
pub struct OutcomeRow {
    pub subject: String,
    pub task_type: String,
    pub status: String,
    pub grade: Option<i64>,
    pub rating: Option<i64>,
    pub duration_s: i64,
    pub duration_ms: Option<i64>,
    pub cost: f64,
    pub tokens: i64,
}

pub fn summarize(subject: &str, task_type: &str, source: &str, rows: &[&OutcomeRow]) -> OutcomeStats {
    let mut s = OutcomeStats {
        subject: subject.into(),
        task_type: task_type.into(),
        source: source.into(),
        ..Default::default()
    };
    let mut durations = Vec::new();
    let (mut cost, mut tokens, mut grades, mut graded) = (0.0, 0i64, 0i64, 0u64);
    for r in rows {
        match r.status.as_str() {
            "done" => s.done += 1,
            "stopped" => s.stopped += 1,
            _ => s.failed += 1,
        }
        if r.status == "done" && !r.grade.is_some_and(|g| g <= 2) && r.rating != Some(-1) {
            s.successes += 1;
        }
        if let Some(g) = r.grade {
            grades += g;
            graded += 1;
        }
        s.net_rating += r.rating.unwrap_or(0);
        let dur_s = r.duration_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(r.duration_s as f64);
        durations.push(dur_s);
        cost += r.cost;
        tokens += r.tokens;
    }
    let finished = s.done + s.failed;
    s.success_rate = (finished > 0).then(|| s.successes as f64 / finished as f64);
    s.mean_grade = (graded > 0).then(|| grades as f64 / graded as f64);
    if !rows.is_empty() {
        s.mean_cost = Some(cost / rows.len() as f64);
        if tokens > 0 || source == "probe" {
            s.mean_tokens = Some(tokens as f64 / rows.len() as f64);
        }
        durations.sort_by(f64::total_cmp);
        let n = durations.len();
        s.median_duration_s = Some(if n % 2 == 1 {
            durations[n / 2]
        } else {
            (durations[n / 2 - 1] + durations[n / 2]) / 2.0
        });
    }
    s
}

/// Format latency seconds cleanly: `240ms`, `1.4s`, `41s`.
pub fn format_duration_s(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{secs:.0}s")
    }
}

/// `code: 8/9 ok (89%), grade 4.3, $0.0120/thread, 41s median, rating +2`
pub fn stats_line(s: &OutcomeStats) -> String {
    let what = if s.task_type.is_empty() { "all" } else { s.task_type.as_str() };
    let mut out = format!("{what}: {}/{} ok", s.successes, s.done + s.failed);
    if let Some(r) = s.success_rate {
        out.push_str(&format!(" ({:.0}%)", r * 100.0));
    }
    if s.stopped > 0 {
        out.push_str(&format!(", {} stopped", s.stopped));
    }
    if let Some(g) = s.mean_grade {
        out.push_str(&format!(", grade {g:.1}"));
    }
    if let Some(c) = s.mean_cost {
        out.push_str(&format!(", ${c:.4}/thread"));
    }
    if let Some(d) = s.median_duration_s {
        out.push_str(&format!(", {d:.0}s median"));
    }
    if s.net_rating != 0 {
        out.push_str(&format!(", rating {:+}", s.net_rating));
    }
    out
}

fn proposal_row(r: &rusqlite::Row) -> rusqlite::Result<RegistryProposal> {
    Ok(RegistryProposal {
        id: r.get(0)?,
        model_id: r.get(1)?,
        field: r.get(2)?,
        text: r.get(3)?,
        reason: r.get(4)?,
        status: r.get(5)?,
        source: r.get(6)?,
        created: r.get::<_, i64>(7)? as u64,
        decided: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
    })
}
const PROPOSAL_COLS: &str = "id, model_id, field, text, reason, status, source, created, decided";

// ---- thread metadata and outcomes over Connection

/// Remember what kind of work a thread is, for its outcome later.
pub fn set_thread_meta(conn: &Connection, thread_id: &str, task_type: &str, tags: &[String]) {
    let _ = conn.execute(
        "INSERT OR REPLACE INTO thread_meta (thread_id, task_type, tags) VALUES (?1, ?2, ?3)",
        params![thread_id, norm(task_type), clean_tags(tags).join(",")],
    );
}

/// `(task type, tags)` given when the thread started.
pub fn thread_meta(conn: &Connection, thread_id: &str) -> Option<(String, Vec<String>)> {
    conn.query_row("SELECT task_type, tags FROM thread_meta WHERE thread_id = ?1", [thread_id], |r| {
        let tags: String = r.get(1)?;
        Ok((r.get(0)?, tags.split(',').filter(|t| !t.is_empty()).map(String::from).collect()))
    })
    .optional()
    .ok()
    .flatten()
}

/// Record how a finished thread went: status, duration, and its cost and tokens
/// from the usage table (if present). Once per thread; then checks whether the
/// model's use cases should change. Returns false if the thread isn't finished
/// or was already recorded.
pub fn record_thread_outcome(conn: &Connection, t: &ThreadInfo, model: Option<&ModelEntry>) -> bool {
    if t.status == "running" {
        return false;
    }
    let task_type = thread_meta(conn, &t.id).map(|(tt, _)| tt).unwrap_or_default();
    let (cost, tokens): (f64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(cost), 0), COALESCE(SUM(input_tokens + output_tokens + cache_read_tokens
               + cache_write_tokens), 0) FROM usage WHERE thread_id = ?1",
            [&t.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0.0, 0));
    let duration = t.finished.unwrap_or_else(now).saturating_sub(t.created) as i64;
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO thread_outcomes (thread_id, conversation_id, model_id, device_id, task_type,
               status, duration_s, cost, tokens, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![t.id, t.conversation_id, t.model, t.device_id, task_type, t.status, duration, cost, tokens, now() as i64],
        )
        .map(|n| n > 0)
        .unwrap_or(false);
    if inserted
        && let Some(m) = model
    {
        check_use_cases(conn, m, &task_type);
    }
    inserted
}

/// Record one deterministic probe outcome into `thread_outcomes` with `source = 'probe'`,
/// distinguishable from real thread outcomes.
#[allow(clippy::too_many_arguments)]
pub fn record_probe_outcome(
    conn: &Connection,
    probe_run_id: &str,
    model_id: &str,
    device_id: &str,
    task_type: &str,
    passed: bool,
    latency_ms: u64,
    cost: f64,
    tokens: u64,
    detail: &str,
) -> Result<()> {
    let status = if passed { "done" } else { "failed" };
    let duration_s = ((latency_ms + 500) / 1000) as i64;
    let note: String = detail.trim().chars().take(500).collect();
    conn.execute(
        "INSERT OR REPLACE INTO thread_outcomes
           (thread_id, conversation_id, model_id, device_id, task_type, status, grade_note,
            duration_s, duration_ms, cost, tokens, created, source)
         VALUES (?1, '', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'probe')",
        params![
            probe_run_id,
            model_id,
            device_id,
            norm(task_type),
            status,
            (!note.is_empty()).then_some(note),
            duration_s,
            latency_ms as i64,
            cost,
            tokens as i64,
            now() as i64,
        ],
    )?;
    Ok(())
}

/// The `(model_id, task_type)` of a recorded real-thread outcome, if present.
pub fn outcome_key(conn: &Connection, thread_id: &str) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT model_id, task_type FROM thread_outcomes WHERE thread_id = ?1 AND source = 'thread'",
            [thread_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Validate a 1–5 thread grade.
pub fn validate_grade(grade: i64) -> Result<()> {
    if !(1..=5).contains(&grade) {
        bail!("grade must be 1 to 5");
    }
    Ok(())
}

/// Store the orchestrator's 1–5 grade for an already-recorded thread outcome and
/// re-check automatic use-case proposals.
pub fn grade_outcome(
    conn: &Connection,
    thread_id: &str,
    grade: i64,
    note: &str,
    model: Option<&ModelEntry>,
    task_type: &str,
) -> Result<()> {
    validate_grade(grade)?;
    let note: String = note.trim().chars().take(500).collect();
    conn.execute(
        "UPDATE thread_outcomes SET grade = ?1, grade_note = ?2 WHERE thread_id = ?3",
        params![grade, (!note.is_empty()).then_some(note), thread_id],
    )?;
    if let Some(m) = model {
        check_use_cases(conn, m, task_type);
    }
    Ok(())
}

/// Store the user's thumbs up (1), down (-1), or cleared (`None`) rating for an
/// already-recorded thread outcome and re-check automatic use-case proposals.
pub fn rate_outcome(
    conn: &Connection,
    thread_id: &str,
    rating: Option<i64>,
    model: Option<&ModelEntry>,
    task_type: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE thread_outcomes SET user_rating = ?1 WHERE thread_id = ?2",
        params![rating.map(|r| r.signum()), thread_id],
    )?;
    if let Some(m) = model {
        check_use_cases(conn, m, task_type);
    }
    Ok(())
}

/// Stats per model or device for real thread outcomes: one row per task type, plus
/// (unless `task_type` is given) one row with an empty task type for everything together.
pub fn outcome_stats(conn: &Connection, by: By, subject: Option<&str>, task_type: Option<&str>) -> Vec<OutcomeStats> {
    outcome_stats_by_source(conn, by, subject, task_type, "thread")
}

/// Stats per model for deterministic probe runs (`source = 'probe'`).
pub fn probe_stats(conn: &Connection, model_id: Option<&str>, task_type: Option<&str>) -> Vec<OutcomeStats> {
    outcome_stats_by_source(conn, By::Model, model_id, task_type, "probe")
}

/// Stats per model or device filtered by outcome `source` (`thread` or `probe`).
pub fn outcome_stats_by_source(
    conn: &Connection,
    by: By,
    subject: Option<&str>,
    task_type: Option<&str>,
    source: &str,
) -> Vec<OutcomeStats> {
    let col = match by {
        By::Model => "model_id",
        By::Device => "device_id",
    };
    let rows: Vec<OutcomeRow> = {
        let mut st = conn
            .prepare(&format!(
                "SELECT {col}, task_type, status, grade, user_rating, duration_s, duration_ms, cost, tokens
                 FROM thread_outcomes
                 WHERE source = ?1 AND (?2 IS NULL OR {col} = ?2) AND (?3 IS NULL OR task_type = ?3)"
            ))
            .unwrap();
        st.query_map(params![source, subject, task_type.map(norm)], |r| {
            Ok(OutcomeRow {
                subject: r.get(0)?,
                task_type: r.get(1)?,
                status: r.get(2)?,
                grade: r.get(3)?,
                rating: r.get(4)?,
                duration_s: r.get(5)?,
                duration_ms: r.get(6)?,
                cost: r.get(7)?,
                tokens: r.get(8)?,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    };
    let mut groups: BTreeMap<(String, String), Vec<&OutcomeRow>> = BTreeMap::new();
    for r in &rows {
        if task_type.is_none() {
            groups.entry((r.subject.clone(), String::new())).or_default().push(r);
        }
        if !r.task_type.is_empty() {
            groups.entry((r.subject.clone(), r.task_type.clone())).or_default().push(r);
        }
    }
    groups.iter().map(|((s, t), rows)| summarize(s, t, source, rows)).collect()
}

pub fn registry_stats(conn: &Connection) -> RegistryStats {
    RegistryStats {
        models: outcome_stats(conn, By::Model, None, None),
        devices: outcome_stats(conn, By::Device, None, None),
        probes: probe_stats(conn, None, None),
    }
}

// ---- use-case proposals

pub fn proposals(conn: &Connection, status: Option<&str>) -> Vec<RegistryProposal> {
    let mut st = conn
        .prepare(&format!(
            "SELECT {PROPOSAL_COLS} FROM registry_proposals WHERE ?1 IS NULL OR status = ?1 ORDER BY id DESC"
        ))
        .unwrap();
    st.query_map([status], proposal_row).unwrap().filter_map(Result::ok).collect()
}

pub fn proposal(conn: &Connection, id: i64) -> Option<RegistryProposal> {
    conn.query_row(&format!("SELECT {PROPOSAL_COLS} FROM registry_proposals WHERE id = ?1"), [id], proposal_row)
        .optional()
        .ok()
        .flatten()
}

/// Propose new use-case text for a model. One pending proposal per model.
pub fn propose_use_cases(
    conn: &Connection,
    model: &ModelEntry,
    text: &str,
    reason: &str,
    source: &str,
) -> Result<RegistryProposal> {
    let model_id = &model.id;
    let text = text.trim();
    if text.is_empty() {
        bail!("the proposed use cases are empty");
    }
    if text.chars().count() > MAX_USE_CASES_CHARS {
        bail!("keep use cases under {MAX_USE_CASES_CHARS} characters");
    }
    if text == model.use_cases.trim() {
        bail!("that's already {model_id}'s use cases");
    }
    let pending: Option<i64> = conn
        .query_row(
            "SELECT id FROM registry_proposals WHERE model_id = ?1 AND status = 'pending'",
            [model_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(p) = pending {
        bail!("{model_id} already has a pending proposal (#{p}); the user decides on it first");
    }
    conn.execute(
        "INSERT INTO registry_proposals (model_id, field, text, reason, status, source, created)
         VALUES (?1, 'use_cases', ?2, ?3, 'pending', ?4, ?5)",
        params![model_id, text, reason.trim(), source, now() as i64],
    )?;
    let id = conn.last_insert_rowid();
    Ok(proposal(conn, id).expect("just inserted"))
}

/// Create or update a pending use-case proposal from a probe run.
///
/// If the model already has a pending proposal (for example, drafted from a model card on
/// import or from an earlier probe run), updates it in place while preserving any non-probe
/// prefix. Returns `Ok(None)` if the resulting text already matches the model's current
/// `use_cases` and there is no pending proposal to update.
pub fn upsert_probe_proposal(
    conn: &Connection,
    model: &ModelEntry,
    probe_summary: &str,
    probe_reason: &str,
) -> Result<Option<RegistryProposal>> {
    let model_id = &model.id;
    let summary = probe_summary.trim();
    if summary.is_empty() {
        return Ok(None);
    }
    let reason = probe_reason.trim();
    let existing: Option<RegistryProposal> = conn
        .query_row(
            &format!("SELECT {PROPOSAL_COLS} FROM registry_proposals WHERE model_id = ?1 AND status = 'pending'"),
            [model_id],
            proposal_row,
        )
        .optional()?;
    let base = match &existing {
        Some(p) => strip_probe_clause(&p.text),
        None => strip_probe_clause(&model.use_cases),
    };
    let combined = combine_use_cases(base, summary);
    if let Some(p) = existing {
        let merged_reason = if p.source == "catalog" && !p.reason.trim().is_empty() {
            format!("{} {reason}", p.reason.trim())
        } else {
            reason.to_string()
        };
        conn.execute(
            "UPDATE registry_proposals SET text = ?1, reason = ?2, source = 'probe', created = ?3 WHERE id = ?4",
            params![combined, merged_reason, now() as i64, p.id],
        )?;
        return Ok(proposal(conn, p.id));
    }
    if combined == model.use_cases.trim() {
        return Ok(None);
    }
    propose_use_cases(conn, model, &combined, reason, "probe").map(Some)
}

/// Accept (the model's use cases become the proposed text) or reject a pending proposal.
pub fn decide_proposal(conn: &mut Connection, id: i64, accept: bool) -> Result<RegistryProposal> {
    let Some(p) = proposal(conn, id) else { bail!("no proposal #{id}") };
    if p.status != "pending" {
        bail!("proposal #{id} was already {}", p.status);
    }
    let tx = conn.transaction()?;
    if accept && tx.execute("UPDATE models SET use_cases = ?1 WHERE id = ?2", params![p.text, p.model_id])? == 0 {
        bail!("model {} isn't in the registry any more", p.model_id);
    }
    tx.execute(
        "UPDATE registry_proposals SET status = ?1, decided = ?2 WHERE id = ?3",
        params![if accept { "accepted" } else { "rejected" }, now() as i64, id],
    )?;
    tx.commit()?;
    Ok(proposal(conn, id).expect("exists"))
}

/// The automatic check after an outcome: when a model has at least
/// [`PROPOSAL_MIN_RUNS`] finished threads of a task type, succeeds at
/// [`PROPOSAL_HIGH`] or more (or [`PROPOSAL_LOW`] or less), and its use cases don't
/// mention the task type, propose adding "<type> tasks" (or "not for <type>
/// tasks"). Never repeats a proposal made before (so a rejection sticks) and
/// waits while the model has one pending.
pub fn check_use_cases(conn: &Connection, model: &ModelEntry, task_type: &str) -> Option<RegistryProposal> {
    if task_type.is_empty() {
        return None;
    }
    let model_id = &model.id;
    if model.use_cases.to_lowercase().contains(task_type) {
        return None;
    }
    let s = outcome_stats(conn, By::Model, Some(model_id), Some(task_type)).into_iter().next()?;
    let finished = s.done + s.failed;
    let rate = s.success_rate?;
    if finished < PROPOSAL_MIN_RUNS {
        return None;
    }
    let addition = if rate >= PROPOSAL_HIGH {
        format!("{task_type} tasks")
    } else if rate <= PROPOSAL_LOW {
        format!("not for {task_type} tasks")
    } else {
        return None;
    };
    let base = model.use_cases.trim().trim_end_matches(['.', ';', ',', ' ']);
    let text = if base.is_empty() { addition } else { format!("{base}; {addition}") };
    let already: bool = conn
        .query_row(
            "SELECT 1 FROM registry_proposals WHERE model_id = ?1 AND (text = ?2 OR status = 'pending')",
            params![model_id, text],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some();
    if already {
        return None;
    }
    let reason = format!(
        "{} of {finished} finished {task_type} threads succeeded ({:.0}%)",
        s.successes,
        rate * 100.0
    );
    match propose_use_cases(conn, model, &text, &reason, "stats") {
        Ok(p) => {
            tracing::info!(model = model_id, proposal = p.id, "proposed new use cases: {}", p.text);
            Some(p)
        }
        Err(e) => {
            tracing::warn!("use-case proposal for {model_id}: {e:#}");
            None
        }
    }
}

// ---- priority rules

pub fn add_rule(conn: &Connection, text: &str, rule: &RuleSpec) -> Result<PriorityRule> {
    let text = text.trim();
    if text.is_empty() {
        bail!("describe the rule, e.g. \"Sensitive data: local only\"");
    }
    let rule = validate_rule(rule)?;
    conn.execute(
        "INSERT INTO priority_rules (text, rule, enabled, created) VALUES (?1, ?2, 1, ?3)",
        params![text, serde_json::to_string(&rule)?, now() as i64],
    )?;
    let id = conn.last_insert_rowid();
    Ok(rules(conn).into_iter().find(|r| r.id == id).expect("just inserted"))
}

/// Every rule, enabled or not, oldest first.
pub fn rules(conn: &Connection) -> Vec<PriorityRule> {
    let mut st = conn.prepare("SELECT id, text, rule, enabled, created FROM priority_rules ORDER BY id").unwrap();
    st.query_map([], |r| {
        let rule: String = r.get(2)?;
        Ok(PriorityRule {
            id: r.get(0)?,
            text: r.get(1)?,
            rule: serde_json::from_str(&rule).unwrap_or_default(),
            enabled: r.get::<_, i64>(3)? != 0,
            created: r.get::<_, i64>(4)? as u64,
        })
    })
    .unwrap()
    .filter_map(Result::ok)
    .collect()
}

/// Returns false if there's no such rule.
pub fn set_rule_enabled(conn: &Connection, id: i64, enabled: bool) -> Result<bool> {
    Ok(conn.execute("UPDATE priority_rules SET enabled = ?1 WHERE id = ?2", params![enabled, id])? > 0)
}

pub fn delete_rule(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM priority_rules WHERE id = ?1", [id])? > 0)
}

// ---- routing

/// Choose the model for a thread from plain data (no database access), or check the
/// model the orchestrator named against the priority rules.
///
/// `stats` should hold per-model [`OutcomeStats`] for `task_type` (keyed by model id),
/// or be empty when `task_type` is unset. Errors are meant for the orchestrator to relay.
#[allow(clippy::too_many_arguments)]
pub fn route_model_from(
    models: &[ModelEntry],
    local: &HashMap<String, bool>,
    all_rules: &[PriorityRule],
    stats: &HashMap<String, OutcomeStats>,
    requested: Option<&str>,
    task_type: Option<&str>,
    tags: &[String],
    prefer_cheaper: bool,
    unavailable_providers: &HashSet<String>,
) -> Result<Route, String> {
    if models.is_empty() {
        return Err("no models registered".into());
    }
    let task_type = task_type.map(norm).unwrap_or_default();
    let tags = clean_tags(tags);
    let is_local = |m: &ModelEntry| local.get(&m.id).copied().unwrap_or(m.local);
    let rules: Vec<&PriorityRule> =
        all_rules.iter().filter(|r| r.enabled && rule_applies(&r.rule, &task_type, &tags)).collect();
    let filters: Vec<&PriorityRule> =
        rules.iter().copied().filter(|r| r.rule.require.is_some() || r.rule.forbid.is_some()).collect();
    // The first filter rule a model fails, described, if any.
    let blocked_by = |m: &ModelEntry| -> Option<String> {
        let l = is_local(m);
        filters
            .iter()
            .find(|r| {
                r.rule.require.as_deref().is_some_and(|p| !has_property(m, l, p))
                    || r.rule.forbid.as_deref().is_some_and(|p| has_property(m, l, p))
            })
            .map(|r| describe_rule(r))
    };

    if let Some(key) = requested {
        let m = find_model_in(models, key).ok_or_else(|| format!("no model {key:?} in the registry"))?;
        if m.hidden {
            let why = match m.hidden_reason.as_deref() {
                Some("gone") => "the provider no longer lists it",
                Some("user") => "it is hidden by the user",
                _ => "it is hidden",
            };
            return Err(format!(
                "model {} is hidden ({why}). Leave model out to let the registry choose, or pick one from list_models.",
                m.id
            ));
        }
        if let Some(r) = blocked_by(&m) {
            return Err(format!(
                "model {} is ruled out for this task by priority rule {}. Leave model out to let the registry \
                 choose, or pick one that fits from list_models.",
                m.id, r
            ));
        }
        return Ok(Route { model: m, reason: None });
    }

    if !models.iter().any(|m| !m.hidden) {
        return Err("no visible models registered".into());
    }

    let mut allowed: Vec<ModelEntry> = models
        .iter()
        .filter(|m| !m.hidden && blocked_by(m).is_none() && !unavailable_providers.contains(&m.provider_id))
        .cloned()
        .collect();
    if allowed.is_empty() {
        if models.iter().any(|m| !m.hidden && blocked_by(m).is_none() && unavailable_providers.contains(&m.provider_id)) {
            return Err("No automatically selectable model is currently available; the matching device-hosted model is offline. Connect its hosting device or choose another model.".into());
        }
        let why: Vec<String> = filters.iter().map(|r| describe_rule(r)).collect();
        return Err(format!(
            "No registered model satisfies the priority rules for this task ({}). Tell the user: they can add a \
             model that fits (`grouper models add … --local` for a local one) or change the rules (`grouper rules`).",
            why.join("; ")
        ));
    }

    // 0: proven for this task type, 1: not enough record, 2: poor record.
    let tier = |m: &ModelEntry| match stats.get(&m.id) {
        Some(s) if s.done + s.failed >= MIN_ROUTING_RUNS => {
            if s.success_rate.unwrap_or(0.0) >= 0.5 { 0 } else { 2 }
        }
        _ => 1,
    };
    let role_rank = |m: &ModelEntry| {
        if m.roles.iter().any(|r| r == "thread") {
            0
        } else if m.roles.iter().any(|r| r == "orchestrator") {
            1
        } else {
            2
        }
    };
    let prefs: Vec<&str> = rules.iter().filter_map(|r| r.rule.prefer.as_deref()).collect();
    allowed.sort_by(|a, b| {
        use std::cmp::Ordering::Equal;
        for p in &prefs {
            let o = match *p {
                "cheapest" => price(a).total_cmp(&price(b)),
                "local" => is_local(b).cmp(&is_local(a)),
                "free" => b.free.cmp(&a.free),
                _ => Equal,
            };
            if o != Equal {
                return o;
            }
        }
        tier(a)
            .cmp(&tier(b))
            .then_with(|| match (stats.get(&a.id), stats.get(&b.id)) {
                (Some(x), Some(y)) if tier(a) == 0 => y
                    .success_rate
                    .unwrap_or(0.0)
                    .total_cmp(&x.success_rate.unwrap_or(0.0))
                    .then(y.mean_grade.unwrap_or(0.0).total_cmp(&x.mean_grade.unwrap_or(0.0)))
                    .then(x.mean_cost.unwrap_or(0.0).total_cmp(&y.mean_cost.unwrap_or(0.0))),
                _ => Equal,
            })
            .then_with(|| if prefer_cheaper { price(a).total_cmp(&price(b)) } else { Equal })
            .then(role_rank(a).cmp(&role_rank(b)))
            .then(a.id.cmp(&b.id))
    });
    let several = allowed.len() > 1;
    let model = allowed.swap_remove(0);

    let mut why: Vec<String> = filters.iter().map(|r| format!("rule {}", describe_rule(r))).collect();
    if !prefs.is_empty() {
        why.push(format!("preferring {}", prefs.join(", then ")));
    }
    if prefer_cheaper && several {
        why.push("cheaper among equals for the thread's budget".into());
    }
    if tier(&model) == 0 {
        let s = &stats[&model.id];
        why.push(format!(
            "best {task_type} record: {}/{} succeeded{}",
            s.successes,
            s.done + s.failed,
            s.mean_grade.map(|g| format!(", grade {g:.1}")).unwrap_or_default()
        ));
    } else if prefs.is_empty() {
        let default = default_model_from(models, "thread", &HashSet::new()).map(|m| m.id);
        why.push(if default.as_deref() == Some(model.id.as_str()) {
            "registry default for threads".into()
        } else {
            "the default among the models the rules allow".into()
        });
    }
    Ok(Route { reason: Some(format!("model {}: {}", model.id, why.join("; "))), model })
}

/// Route a model using priority rules and outcome stats loaded from `conn`.
#[allow(clippy::too_many_arguments)]
pub fn route_model(
    conn: &Connection,
    models: &[ModelEntry],
    local: &HashMap<String, bool>,
    requested: Option<&str>,
    task_type: Option<&str>,
    tags: &[String],
    prefer_cheaper: bool,
    unavailable_providers: &HashSet<String>,
) -> Result<Route, String> {
    let all_rules = rules(conn);
    let normalized_task = task_type.map(norm).unwrap_or_default();
    let stats: HashMap<String, OutcomeStats> = if normalized_task.is_empty() {
        HashMap::new()
    } else {
        outcome_stats(conn, By::Model, None, Some(&normalized_task))
            .into_iter()
            .map(|s| (s.subject.clone(), s))
            .collect()
    };
    route_model_from(
        models,
        local,
        &all_rules,
        &stats,
        requested,
        task_type,
        tags,
        prefer_cheaper,
        unavailable_providers,
    )
}

/// Strip any previous probe-generated summary clause so re-probing replaces rather than duplicates it.
pub fn strip_probe_clause(text: &str) -> &str {
    let trimmed = text.trim();
    for marker in ["; passed ", "; failed ", "; Passed ", "; Failed "] {
        if let Some(idx) = trimmed.find(marker)
            && trimmed[idx..].to_ascii_lowercase().contains("probe")
        {
            return trimmed[..idx].trim().trim_end_matches(['.', ';', ',', ' ']);
        }
    }
    let lower = trimmed.to_ascii_lowercase();
    if (lower.starts_with("passed ") || lower.starts_with("failed ")) && lower.contains("probe") {
        return "";
    }
    trimmed.trim_end_matches(['.', ';', ',', ' '])
}

/// Join an optional base use-case description with a probe summary, fitting within [`MAX_USE_CASES_CHARS`].
pub fn combine_use_cases(base: &str, summary: &str) -> String {
    let full = if base.is_empty() { summary.to_string() } else { format!("{base}; {summary}") };
    if full.chars().count() <= MAX_USE_CASES_CHARS {
        return full;
    }
    let mut out: String = full.chars().take(MAX_USE_CASES_CHARS.saturating_sub(1)).collect();
    out = out.trim_end().to_string();
    out.push('…');
    out
}

/// One brief summary line for a model's probe results, for the orchestrator's `list_models`.
pub fn probe_overview_line(probe_stats: &[OutcomeStats], model_id: &str) -> Option<String> {
    let overall = probe_stats.iter().find(|s| s.subject == model_id && s.task_type.is_empty())?;
    let total = overall.done + overall.failed;
    if total == 0 {
        return None;
    }
    let pct = overall.success_rate.unwrap_or(0.0) * 100.0;
    let parts: Vec<String> = probe_stats
        .iter()
        .filter(|s| s.subject == model_id && !s.task_type.is_empty())
        .map(|s| {
            let n = s.done + s.failed;
            if n == 1 {
                format!("{} {}", s.task_type, if s.successes == 1 { "pass" } else { "fail" })
            } else {
                format!("{} {}/{}", s.task_type, s.successes, n)
            }
        })
        .collect();
    let mut line = format!("{}/{total} passed ({pct:.0}%)", overall.successes);
    if !parts.is_empty() {
        line.push_str(&format!(": {}", parts.join(", ")));
    }
    if let Some(d) = overall.median_duration_s {
        line.push_str(&format!("; median {}", format_duration_s(d)));
    }
    Some(line)
}

/// What `list_models` shows the orchestrator: each model with its effective
/// `local` flag, stats per task type, brief probe summary and any pending proposal,
/// plus the enabled rules.
pub fn models_overview(
    conn: &Connection,
    models: &[ModelEntry],
    local: &HashMap<String, bool>,
) -> Value {
    let stats = outcome_stats(conn, By::Model, None, None);
    let probes = probe_stats(conn, None, None);
    let pending = proposals(conn, Some("pending"));
    let model_rows: Vec<Value> = models
        .iter()
        .filter(|m| !m.hidden)
        .map(|m| {
            let is_local = local.get(&m.id).copied().unwrap_or(m.local);
            let mut v = serde_json::to_value(m).unwrap_or_default();
            v["local"] = json!(is_local);
            let lines: Vec<String> = stats.iter().filter(|s| s.subject == m.id).map(stats_line).collect();
            if !lines.is_empty() {
                v["stats"] = json!(lines);
            }
            if let Some(summary) = probe_overview_line(&probes, &m.id) {
                v["probes"] = json!(summary);
            }
            if let Some(p) = pending.iter().find(|p| p.model_id == m.id) {
                v["pending_use_cases_proposal"] = json!({"id": p.id, "text": p.text, "reason": p.reason});
            }
            v
        })
        .collect();
    let active_rules: Vec<Value> = rules(conn)
        .into_iter()
        .filter(|r| r.enabled)
        .map(|r| json!({"id": r.id, "text": r.text, "rule": r.rule}))
        .collect();
    json!({
        "models": model_rows,
        "priority_rules": active_rules,
        "task_types": TASK_TYPES,
        "note": "Prices are USD per million tokens. Stats are from finished threads by task type; probes are from deterministic probe suites."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls() {
        for u in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:1234/x",
            "https://gpu-box.local/v1",
            "http://user@LOCALHOST",
            "localhost:11434",
        ] {
            assert!(is_local_url(u), "{u}");
        }
        for u in [
            "https://openrouter.ai/api/v1",
            "https://api.anthropic.com",
            "http://localhost.example.com",
            "",
            "http://",
        ] {
            assert!(!is_local_url(u), "{u}");
        }
    }

    #[test]
    fn summaries() {
        let r = |status: &str, d: i64, cost: f64| OutcomeRow {
            subject: "m".into(),
            task_type: "code".into(),
            status: status.into(),
            grade: None,
            rating: None,
            duration_s: d,
            duration_ms: None,
            cost,
            tokens: 0,
        };
        let rows = [r("done", 10, 0.1), r("failed", 1, 0.3), r("done", 5, 0.2), r("stopped", 3, 0.2)];
        let s = summarize("m", "code", "thread", &rows.iter().collect::<Vec<_>>());
        assert_eq!(s.median_duration_s, Some(4.0));
        assert!((s.mean_cost.unwrap() - 0.2).abs() < 1e-9);
        assert_eq!((s.successes, s.success_rate.map(|x| (x * 3.0).round())), (2, Some(2.0)));
        let s = summarize("m", "code", "thread", &rows[..3].iter().collect::<Vec<_>>());
        assert_eq!((s.median_duration_s, s.mean_grade), (Some(5.0), None));
        assert_eq!(summarize("m", "", "thread", &[]).success_rate, None);
    }

    #[test]
    fn rules_are_validated() {
        let r = validate_rule(&RuleSpec {
            when_tag: Some(" Sensitive ".into()),
            require: Some("LOCAL".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!((r.when_tag.as_deref(), r.require.as_deref()), (Some("sensitive"), Some("local")));
        assert!(validate_rule(&RuleSpec { require: Some("fast".into()), ..Default::default() }).is_err());
        assert!(validate_rule(&RuleSpec { prefer: Some("best".into()), ..Default::default() }).is_err());
        assert!(validate_rule(&RuleSpec { when_tag: Some("x".into()), ..Default::default() }).is_err());
        assert!(
            serde_json::from_str::<RuleSpec>(r#"{"when_tags": "x", "require": "local"}"#).is_err(),
            "typos are refused"
        );
    }

    #[test]
    fn hidden_models_are_skipped_by_defaults_routing_and_overview() {
        let mut hidden_orch = ModelEntry {
            id: "old-orch".into(),
            provider_id: "p".into(),
            model: "shared-model".into(),
            input_price: 0.1,
            output_price: 0.1,
            free: true,
            roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
            local: true,
            hidden: true,
            hidden_reason: Some("gone".into()),
            ..Default::default()
        };
        let visible_api = ModelEntry {
            id: "live-api".into(),
            provider_id: "p".into(),
            model: "shared-model".into(),
            input_price: 5.0,
            output_price: 10.0,
            roles: vec![],
            ..Default::default()
        };
        let models = vec![hidden_orch.clone(), visible_api.clone()];
        let none_unavail = HashSet::new();

        // Defaults skip hidden rows for every role (even when the hidden row holds the role and is free).
        assert_eq!(default_model_from(&models, "orchestrator", &none_unavail).unwrap().id, "live-api");
        assert_eq!(default_model_from(&models, "standard", &none_unavail).unwrap().id, "live-api");
        assert_eq!(default_model_from(&models, "thread", &none_unavail).unwrap().id, "live-api");
        assert!(default_model_from(std::slice::from_ref(&hidden_orch), "orchestrator", &none_unavail).is_none());

        // find_model_in resolves hidden rows by exact id, and prefers visible rows when matching by provider model name.
        assert_eq!(find_model_in(&models, "old-orch").unwrap().id, "old-orch");
        assert_eq!(find_model_in(&models, "shared-model").unwrap().id, "live-api");
        assert_eq!(find_model_in(std::slice::from_ref(&hidden_orch), "shared-model").unwrap().id, "old-orch");

        // Automatic routing skips hidden rows even when they have better stats, lower price, or match a rule.
        let local = HashMap::from([("old-orch".into(), true), ("live-api".into(), false)]);
        let stats = HashMap::from([(
            "old-orch".into(),
            OutcomeStats {
                subject: "old-orch".into(),
                task_type: "code".into(),
                done: 10,
                successes: 10,
                success_rate: Some(1.0),
                mean_grade: Some(5.0),
                ..Default::default()
            },
        )]);
        let routed = route_model_from(&models, &local, &[], &stats, None, Some("code"), &[], true, &none_unavail)
            .unwrap();
        assert_eq!(routed.model.id, "live-api");

        // A require:local rule does not pick a hidden local model.
        let local_rule = vec![PriorityRule {
            id: 1,
            text: "Sensitive: local only".into(),
            rule: RuleSpec { when_tag: Some("sensitive".into()), require: Some("local".into()), ..Default::default() },
            enabled: true,
            created: 1,
        }];
        let err = route_model_from(
            &models,
            &local,
            &local_rule,
            &stats,
            None,
            None,
            &["sensitive".into()],
            false,
            &none_unavail,
        )
        .unwrap_err();
        assert!(err.contains("No registered model satisfies"), "{err}");

        // Explicit request for a hidden model is refused with the reason.
        let err = route_model_from(&models, &local, &[], &stats, Some("old-orch"), None, &[], false, &none_unavail)
            .unwrap_err();
        assert!(err.contains("hidden") && err.contains("no longer lists"), "{err}");

        // Only hidden models -> clear error, not an empty rules error.
        hidden_orch.hidden_reason = Some("user".into());
        let err = route_model_from(
            std::slice::from_ref(&hidden_orch),
            &local,
            &[],
            &stats,
            None,
            None,
            &[],
            false,
            &none_unavail,
        )
        .unwrap_err();
        assert!(err.contains("no visible models"), "{err}");

        // models_overview omits hidden rows.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let overview = models_overview(&conn, &models, &local);
        let listed = overview["models"].as_array().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], "live-api");
    }

    #[test]
    fn harness_models_never_count_as_local() {
        let harness = ModelEntry {
            id: "cc-opus".into(),
            provider_id: "claude-code@pc".into(),
            model: "opus".into(),
            kind: "harness".into(),
            device_id: Some("pc".into()),
            local: true,
            ..Default::default()
        };
        assert!(!model_is_local(&harness, Some("http://localhost:1234"), true));
    }

    #[test]
    fn default_model_and_routing_enforce_harness_orchestration_rules() {
        let non_orch_harness = ModelEntry {
            id: "a-codex-o3".into(),
            provider_id: "codex@pc".into(),
            model: "o3".into(),
            kind: "harness".into(),
            device_id: Some("pc".into()),
            can_orchestrate: false,
            roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
            ..Default::default()
        };
        let orch_harness = ModelEntry {
            id: "b-cc-opus".into(),
            provider_id: "claude-code@pc".into(),
            model: "opus".into(),
            kind: "harness".into(),
            device_id: Some("pc".into()),
            can_orchestrate: true,
            roles: vec!["orchestrator".into()],
            ..Default::default()
        };
        let api_model = ModelEntry {
            id: "z-gpt-4o".into(),
            provider_id: "openai".into(),
            model: "gpt-4o".into(),
            kind: "api".into(),
            roles: vec![],
            ..Default::default()
        };
        let none_unavail = HashSet::new();

        // Only a non-orchestrating harness: never auto-picked for orchestrator (even if it carries the role
        // or would be the final "first available" fallback), but allowed for standard and thread.
        let only_non_orch = vec![non_orch_harness.clone()];
        assert!(default_model_from(&only_non_orch, "orchestrator", &none_unavail).is_none());
        assert_eq!(default_model_from(&only_non_orch, "standard", &none_unavail).unwrap().id, "a-codex-o3");
        assert_eq!(default_model_from(&only_non_orch, "thread", &none_unavail).unwrap().id, "a-codex-o3");

        // Even with no roles at all, only a non-orchestrating harness is never auto-picked for orchestrator.
        let unroled_non_orch = vec![ModelEntry { roles: vec![], ..non_orch_harness.clone() }];
        assert!(default_model_from(&unroled_non_orch, "orchestrator", &none_unavail).is_none());
        assert_eq!(default_model_from(&unroled_non_orch, "standard", &none_unavail).unwrap().id, "a-codex-o3");
        assert_eq!(default_model_from(&unroled_non_orch, "thread", &none_unavail).unwrap().id, "a-codex-o3");

        // Non-orchestrating harness listed before an API model: orchestrator skips it and picks the API model.
        let non_orch_and_api = vec![non_orch_harness.clone(), api_model.clone()];
        assert_eq!(default_model_from(&non_orch_and_api, "orchestrator", &none_unavail).unwrap().id, "z-gpt-4o");
        assert_eq!(default_model_from(&non_orch_and_api, "standard", &none_unavail).unwrap().id, "a-codex-o3");
        assert_eq!(default_model_from(&non_orch_and_api, "thread", &none_unavail).unwrap().id, "a-codex-o3");

        // A harness with can_orchestrate = true IS allowed as the orchestrator default.
        let all = vec![non_orch_harness.clone(), orch_harness.clone(), api_model.clone()];
        assert_eq!(default_model_from(&all, "orchestrator", &none_unavail).unwrap().id, "b-cc-opus");

        // Any harness (with or without can_orchestrate) may be picked for threads in route_model_from.
        let local = HashMap::new();
        let stats = HashMap::new();
        let routed_non_orch =
            route_model_from(&all, &local, &[], &stats, Some("a-codex-o3"), Some("code"), &[], false, &none_unavail)
                .unwrap();
        assert_eq!(routed_non_orch.model.id, "a-codex-o3");
        let routed_orch =
            route_model_from(&all, &local, &[], &stats, Some("b-cc-opus"), Some("code"), &[], false, &none_unavail)
                .unwrap();
        assert_eq!(routed_orch.model.id, "b-cc-opus");

        // Priority rules understand `api` and `harness` properties.
        let api_only = validate_rule(&RuleSpec { require: Some("api".into()), ..Default::default() }).unwrap();
        let forbid_harness = validate_rule(&RuleSpec { forbid: Some("harness".into()), ..Default::default() }).unwrap();
        let rules = vec![
            PriorityRule { id: 1, text: "API only".into(), rule: api_only, enabled: true, created: 1 },
            PriorityRule { id: 2, text: "No harnesses".into(), rule: forbid_harness, enabled: true, created: 2 },
        ];
        let routed_api =
            route_model_from(&all, &local, &rules, &stats, None, Some("code"), &[], false, &none_unavail).unwrap();
        assert_eq!(routed_api.model.id, "z-gpt-4o");
        assert!(
            route_model_from(&all, &local, &rules, &stats, Some("a-codex-o3"), None, &[], false, &none_unavail)
                .is_err()
        );
    }
}
