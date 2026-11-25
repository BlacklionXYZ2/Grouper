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
//! Routing ([`Db::route_model`]): enabled rules that apply to the task (by tag and
//! task type) filter the models first (`require` / `forbid`); what's left is
//! ordered by the rules' `prefer`s, then by the task type's record (models with at
//! least [`MIN_ROUTING_RUNS`] finished threads and a success rate of 50% or more
//! first, by success rate, grade, then cost; unproven models next; poor records
//! last), then by the registry default for threads. A model the orchestrator names
//! itself must still pass the filters, so privacy rules can't be bypassed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use core_agent::{Tool, ToolOutput};
use core_protocol::{ModelEntry, OutcomeStats, PriorityRule, RegistryProposal, RegistryStats, RuleInput, RuleSpec, ToolDef};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::db::{Db, now};
use crate::server::{Hub, err};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS thread_meta (
  thread_id TEXT PRIMARY KEY, task_type TEXT NOT NULL DEFAULT '', tags TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS thread_outcomes (
  thread_id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, model_id TEXT NOT NULL, device_id TEXT NOT NULL,
  task_type TEXT NOT NULL DEFAULT '', status TEXT NOT NULL, grade INTEGER, grade_note TEXT, user_rating INTEGER,
  duration_s INTEGER NOT NULL DEFAULT 0, cost REAL NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0,
  created INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS thread_outcomes_model ON thread_outcomes (model_id, task_type);
CREATE TABLE IF NOT EXISTS registry_proposals (
  id INTEGER PRIMARY KEY AUTOINCREMENT, model_id TEXT NOT NULL, field TEXT NOT NULL, text TEXT NOT NULL,
  reason TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', source TEXT NOT NULL, created INTEGER NOT NULL,
  decided INTEGER);
CREATE TABLE IF NOT EXISTS priority_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, rule TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
  created INTEGER NOT NULL);
";

/// Create this module's tables and add `models.local` to older databases.
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    let has_local: bool = conn.prepare("SELECT 1 FROM pragma_table_info('models') WHERE name = 'local'")?.exists([])?;
    if !has_local {
        conn.execute_batch("ALTER TABLE models ADD COLUMN local INTEGER NOT NULL DEFAULT 0;")?;
    }
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
const MAX_USE_CASES_CHARS: usize = 500;

/// Rule values for `require` / `forbid`, and for `prefer`.
const PROPERTIES: &[&str] = &["local", "remote", "free", "paid"];
const PREFERENCES: &[&str] = &["cheapest", "local", "free"];

fn norm(s: &str) -> String {
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

struct OutcomeRow {
    subject: String,
    task_type: String,
    status: String,
    grade: Option<i64>,
    rating: Option<i64>,
    duration_s: i64,
    cost: f64,
}

fn summarize(subject: &str, task_type: &str, rows: &[&OutcomeRow]) -> OutcomeStats {
    let mut s = OutcomeStats { subject: subject.into(), task_type: task_type.into(), ..Default::default() };
    let mut durations = Vec::new();
    let (mut cost, mut grades, mut graded) = (0.0, 0i64, 0u64);
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
        durations.push(r.duration_s as f64);
        cost += r.cost;
    }
    let finished = s.done + s.failed;
    s.success_rate = (finished > 0).then(|| s.successes as f64 / finished as f64);
    s.mean_grade = (graded > 0).then(|| grades as f64 / graded as f64);
    if !rows.is_empty() {
        s.mean_cost = Some(cost / rows.len() as f64);
        durations.sort_by(f64::total_cmp);
        let n = durations.len();
        s.median_duration_s = Some(if n % 2 == 1 { durations[n / 2] } else { (durations[n / 2 - 1] + durations[n / 2]) / 2.0 });
    }
    s
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

impl Db {
    // ---- thread metadata and outcomes

    /// Remember what kind of work a thread is, for its outcome later.
    pub fn set_thread_meta(&self, thread_id: &str, task_type: &str, tags: &[String]) {
        let _ = self.c().execute(
            "INSERT OR REPLACE INTO thread_meta (thread_id, task_type, tags) VALUES (?1, ?2, ?3)",
            params![thread_id, norm(task_type), clean_tags(tags).join(",")],
        );
    }

    /// (task type, tags) given when the thread started.
    pub fn thread_meta(&self, thread_id: &str) -> Option<(String, Vec<String>)> {
        self.c()
            .query_row("SELECT task_type, tags FROM thread_meta WHERE thread_id = ?1", [thread_id], |r| {
                let tags: String = r.get(1)?;
                Ok((r.get(0)?, tags.split(',').filter(|t| !t.is_empty()).map(String::from).collect()))
            })
            .optional()
            .ok()
            .flatten()
    }

    /// Record how a finished thread went: status, duration, and its cost and tokens
    /// from the usage table. Once per thread; then checks whether the model's use
    /// cases should change. Returns false if the thread isn't finished or was recorded.
    pub fn record_outcome(&self, thread_id: &str) -> bool {
        let Some(t) = self.thread(thread_id) else { return false };
        if t.status == "running" {
            return false;
        }
        let task_type = self.thread_meta(thread_id).map(|(t, _)| t).unwrap_or_default();
        let inserted = {
            let c = self.c();
            let (cost, tokens): (f64, i64) = c
                .query_row(
                    "SELECT COALESCE(SUM(cost), 0), COALESCE(SUM(input_tokens + output_tokens + cache_read_tokens
                       + cache_write_tokens), 0) FROM usage WHERE thread_id = ?1",
                    [thread_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap_or((0.0, 0));
            let duration = t.finished.unwrap_or_else(now).saturating_sub(t.created) as i64;
            c.execute(
                "INSERT OR IGNORE INTO thread_outcomes (thread_id, conversation_id, model_id, device_id, task_type,
                   status, duration_s, cost, tokens, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![t.id, t.conversation_id, t.model, t.device_id, task_type, t.status, duration, cost, tokens, now() as i64],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
        };
        if inserted {
            self.check_use_cases(&t.model, &task_type);
        }
        inserted
    }

    /// The (model, task type) of a recorded outcome; an error says why there's none.
    fn outcome_key(&self, thread_id: &str) -> Result<(String, String)> {
        let key = self
            .c()
            .query_row("SELECT model_id, task_type FROM thread_outcomes WHERE thread_id = ?1", [thread_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match (key, self.thread(thread_id)) {
            (Some(k), _) => Ok(k),
            (None, Some(t)) if t.status == "running" => {
                bail!("thread {thread_id} is still running; rate it once its result is in")
            }
            (None, Some(_)) => bail!("thread {thread_id} has no recorded outcome"),
            (None, None) => bail!("no thread {thread_id}"),
        }
    }

    /// The orchestrator's 1–5 grade for a finished thread's result.
    pub fn grade_thread(&self, thread_id: &str, grade: i64, note: &str) -> Result<()> {
        if !(1..=5).contains(&grade) {
            bail!("grade must be 1 to 5");
        }
        let (model, task_type) = self.outcome_key(thread_id)?;
        let note: String = note.trim().chars().take(500).collect();
        self.c().execute(
            "UPDATE thread_outcomes SET grade = ?1, grade_note = ?2 WHERE thread_id = ?3",
            params![grade, (!note.is_empty()).then_some(note), thread_id],
        )?;
        self.check_use_cases(&model, &task_type);
        Ok(())
    }

    /// Your thumbs up (1), down (-1), or neither (`None`) for a finished thread.
    pub fn rate_thread(&self, thread_id: &str, rating: Option<i64>) -> Result<()> {
        let (model, task_type) = self.outcome_key(thread_id)?;
        self.c().execute(
            "UPDATE thread_outcomes SET user_rating = ?1 WHERE thread_id = ?2",
            params![rating.map(|r| r.signum()), thread_id],
        )?;
        self.check_use_cases(&model, &task_type);
        Ok(())
    }

    /// Stats per model or device: one row per task type, plus (unless `task_type`
    /// is given) one row with an empty task type for everything together.
    pub fn outcome_stats(&self, by: By, subject: Option<&str>, task_type: Option<&str>) -> Vec<OutcomeStats> {
        let col = match by {
            By::Model => "model_id",
            By::Device => "device_id",
        };
        let rows: Vec<OutcomeRow> = {
            let c = self.c();
            let mut st = c
                .prepare(&format!(
                    "SELECT {col}, task_type, status, grade, user_rating, duration_s, cost FROM thread_outcomes
                     WHERE (?1 IS NULL OR {col} = ?1) AND (?2 IS NULL OR task_type = ?2)"
                ))
                .unwrap();
            st.query_map(params![subject, task_type.map(norm)], |r| {
                Ok(OutcomeRow {
                    subject: r.get(0)?,
                    task_type: r.get(1)?,
                    status: r.get(2)?,
                    grade: r.get(3)?,
                    rating: r.get(4)?,
                    duration_s: r.get(5)?,
                    cost: r.get(6)?,
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
        groups.iter().map(|((s, t), rows)| summarize(s, t, rows)).collect()
    }

    pub fn registry_stats(&self) -> RegistryStats {
        RegistryStats { models: self.outcome_stats(By::Model, None, None), devices: self.outcome_stats(By::Device, None, None) }
    }

    // ---- use-case proposals

    pub fn proposals(&self, status: Option<&str>) -> Vec<RegistryProposal> {
        let c = self.c();
        let mut st = c
            .prepare(&format!(
                "SELECT {PROPOSAL_COLS} FROM registry_proposals WHERE ?1 IS NULL OR status = ?1 ORDER BY id DESC"
            ))
            .unwrap();
        st.query_map([status], proposal_row).unwrap().filter_map(Result::ok).collect()
    }

    pub fn proposal(&self, id: i64) -> Option<RegistryProposal> {
        self.c()
            .query_row(&format!("SELECT {PROPOSAL_COLS} FROM registry_proposals WHERE id = ?1"), [id], proposal_row)
            .optional()
            .ok()
            .flatten()
    }

    /// Propose new use-case text for a model. One pending proposal per model.
    pub fn propose_use_cases(&self, model_id: &str, text: &str, reason: &str, source: &str) -> Result<RegistryProposal> {
        let Some(model) = self.models().into_iter().find(|m| m.id == model_id) else { bail!("no model {model_id:?}") };
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
        let id = {
            let c = self.c();
            let pending: Option<i64> = c
                .query_row(
                    "SELECT id FROM registry_proposals WHERE model_id = ?1 AND status = 'pending'",
                    [model_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(p) = pending {
                bail!("{model_id} already has a pending proposal (#{p}); the user decides on it first");
            }
            c.execute(
                "INSERT INTO registry_proposals (model_id, field, text, reason, status, source, created)
                 VALUES (?1, 'use_cases', ?2, ?3, 'pending', ?4, ?5)",
                params![model_id, text, reason.trim(), source, now() as i64],
            )?;
            c.last_insert_rowid()
        };
        Ok(self.proposal(id).expect("just inserted"))
    }

    /// Accept (the model's use cases become the proposed text) or reject a pending proposal.
    pub fn decide_proposal(&self, id: i64, accept: bool) -> Result<RegistryProposal> {
        let Some(p) = self.proposal(id) else { bail!("no proposal #{id}") };
        if p.status != "pending" {
            bail!("proposal #{id} was already {}", p.status);
        }
        let mut c = self.c();
        let tx = c.transaction()?;
        if accept && tx.execute("UPDATE models SET use_cases = ?1 WHERE id = ?2", params![p.text, p.model_id])? == 0 {
            bail!("model {} isn't in the registry any more", p.model_id);
        }
        tx.execute(
            "UPDATE registry_proposals SET status = ?1, decided = ?2 WHERE id = ?3",
            params![if accept { "accepted" } else { "rejected" }, now() as i64, id],
        )?;
        tx.commit()?;
        drop(c);
        Ok(self.proposal(id).expect("exists"))
    }

    /// The automatic check after an outcome: when a model has at least
    /// [`PROPOSAL_MIN_RUNS`] finished threads of a task type, succeeds at
    /// [`PROPOSAL_HIGH`] or more (or [`PROPOSAL_LOW`] or less), and its use cases don't
    /// mention the task type, propose adding "<type> tasks" (or "not for <type>
    /// tasks"). Never repeats a proposal made before (so a rejection sticks) and
    /// waits while the model has one pending.
    pub fn check_use_cases(&self, model_id: &str, task_type: &str) -> Option<RegistryProposal> {
        if task_type.is_empty() {
            return None;
        }
        let model = self.models().into_iter().find(|m| m.id == model_id)?;
        if model.use_cases.to_lowercase().contains(task_type) {
            return None;
        }
        let s = self.outcome_stats(By::Model, Some(model_id), Some(task_type)).into_iter().next()?;
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
        let already: bool = self
            .c()
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
        match self.propose_use_cases(model_id, &text, &reason, "stats") {
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

    pub fn add_rule(&self, text: &str, rule: &RuleSpec) -> Result<PriorityRule> {
        let text = text.trim();
        if text.is_empty() {
            bail!("describe the rule, e.g. \"Sensitive data: local only\"");
        }
        let rule = validate_rule(rule)?;
        let id = {
            let c = self.c();
            c.execute(
                "INSERT INTO priority_rules (text, rule, enabled, created) VALUES (?1, ?2, 1, ?3)",
                params![text, serde_json::to_string(&rule)?, now() as i64],
            )?;
            c.last_insert_rowid()
        };
        Ok(self.rules().into_iter().find(|r| r.id == id).expect("just inserted"))
    }

    /// Every rule, enabled or not, oldest first.
    pub fn rules(&self) -> Vec<PriorityRule> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, text, rule, enabled, created FROM priority_rules ORDER BY id").unwrap();
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
    pub fn set_rule_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        Ok(self.c().execute("UPDATE priority_rules SET enabled = ?1 WHERE id = ?2", params![enabled, id])? > 0)
    }

    pub fn delete_rule(&self, id: i64) -> Result<bool> {
        Ok(self.c().execute("DELETE FROM priority_rules WHERE id = ?1", [id])? > 0)
    }

    // ---- routing

    /// Provider id → base_url (without decrypting keys).
    fn provider_urls(&self) -> HashMap<String, Option<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, base_url FROM providers").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().filter_map(Result::ok).collect()
    }

    /// Whether a model counts as local: flagged, or its provider's URL is local.
    pub fn model_is_local(&self, m: &ModelEntry) -> bool {
        m.local || self.provider_urls().get(&m.provider_id).cloned().flatten().is_some_and(|u| is_local_url(&u))
    }

    /// Choose the model for a thread (see the module docs), or check the one the
    /// orchestrator named against the rules. Errors are meant for the orchestrator
    /// to relay.
    pub fn route_model(&self, requested: Option<&str>, task_type: Option<&str>, tags: &[String]) -> Result<Route, String> {
        let models = self.models();
        if models.is_empty() {
            return Err("no models registered".into());
        }
        let task_type = task_type.map(norm).unwrap_or_default();
        let tags = clean_tags(tags);
        let urls = self.provider_urls();
        let local: HashMap<String, bool> = models
            .iter()
            .map(|m| {
                let url_local = urls.get(&m.provider_id).cloned().flatten().is_some_and(|u| is_local_url(&u));
                (m.id.clone(), m.local || url_local)
            })
            .collect();
        let rules: Vec<PriorityRule> =
            self.rules().into_iter().filter(|r| r.enabled && rule_applies(&r.rule, &task_type, &tags)).collect();
        let filters: Vec<&PriorityRule> = rules.iter().filter(|r| r.rule.require.is_some() || r.rule.forbid.is_some()).collect();
        // The first filter rule a model fails, described, if any.
        let blocked_by = |m: &ModelEntry| -> Option<String> {
            let l = local[&m.id];
            filters
                .iter()
                .find(|r| {
                    r.rule.require.as_deref().is_some_and(|p| !has_property(m, l, p))
                        || r.rule.forbid.as_deref().is_some_and(|p| has_property(m, l, p))
                })
                .map(|r| describe_rule(r))
        };

        if let Some(key) = requested {
            let m = self.find_model(key).ok_or_else(|| format!("no model {key:?} in the registry"))?;
            if let Some(r) = blocked_by(&m) {
                return Err(format!(
                    "model {} is ruled out for this task by priority rule {}. Leave model out to let the registry \
                     choose, or pick one that fits from list_models.",
                    m.id, r
                ));
            }
            return Ok(Route { model: m, reason: None });
        }

        let mut allowed: Vec<ModelEntry> = models.iter().filter(|m| blocked_by(m).is_none()).cloned().collect();
        if allowed.is_empty() {
            let why: Vec<String> = filters.iter().map(|r| describe_rule(r)).collect();
            return Err(format!(
                "No registered model satisfies the priority rules for this task ({}). Tell the user: they can add a \
                 model that fits (`grouper models add … --local` for a local one) or change the rules (`grouper rules`).",
                why.join("; ")
            ));
        }

        let stats: HashMap<String, OutcomeStats> = if task_type.is_empty() {
            HashMap::new()
        } else {
            self.outcome_stats(By::Model, None, Some(&task_type)).into_iter().map(|s| (s.subject.clone(), s)).collect()
        };
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
                    "local" => local[&b.id].cmp(&local[&a.id]),
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
                .then(role_rank(a).cmp(&role_rank(b)))
                .then(a.id.cmp(&b.id))
        });
        let model = allowed.swap_remove(0);

        let mut why: Vec<String> = filters.iter().map(|r| format!("rule {}", describe_rule(r))).collect();
        if !prefs.is_empty() {
            why.push(format!("preferring {}", prefs.join(", then ")));
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
            let default = self.default_model("thread").map(|m| m.id);
            why.push(if default.as_deref() == Some(model.id.as_str()) {
                "registry default for threads".into()
            } else {
                "the default among the models the rules allow".into()
            });
        }
        Ok(Route { reason: Some(format!("model {}: {}", model.id, why.join("; "))), model })
    }
}

/// What `list_models` shows the orchestrator: each model with its effective
/// `local` flag, stats per task type and any pending proposal, plus the enabled rules.
pub fn models_overview(db: &Db) -> Value {
    let stats = db.outcome_stats(By::Model, None, None);
    let pending = db.proposals(Some("pending"));
    let models: Vec<Value> = db
        .models()
        .into_iter()
        .map(|m| {
            let local = db.model_is_local(&m);
            let mut v = serde_json::to_value(&m).unwrap_or_default();
            v["local"] = json!(local);
            let lines: Vec<String> = stats.iter().filter(|s| s.subject == m.id).map(stats_line).collect();
            if !lines.is_empty() {
                v["stats"] = json!(lines);
            }
            if let Some(p) = pending.iter().find(|p| p.model_id == m.id) {
                v["pending_use_cases_proposal"] = json!({"id": p.id, "text": p.text, "reason": p.reason});
            }
            v
        })
        .collect();
    let rules: Vec<Value> = db
        .rules()
        .into_iter()
        .filter(|r| r.enabled)
        .map(|r| json!({"id": r.id, "text": r.text, "rule": r.rule}))
        .collect();
    json!({
        "models": models,
        "priority_rules": rules,
        "task_types": TASK_TYPES,
        "note": "Prices are USD per million tokens. Stats are from finished threads by task type."
    })
}

// ---- orchestrator tools

pub fn registry_tools(hub: &Arc<Hub>, conversation_id: &str) -> Vec<Arc<dyn Tool>> {
    let ctx = || RegCtx { hub: hub.clone(), conversation_id: conversation_id.to_string() };
    vec![Arc::new(GradeThread(ctx())), Arc::new(ProposeUseCases(ctx()))]
}

struct RegCtx {
    hub: Arc<Hub>,
    conversation_id: String,
}

fn str_arg<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or_default()
}

struct GradeThread(RegCtx);
#[async_trait]
impl Tool for GradeThread {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "grade_thread".into(),
            description: "Grade a finished thread's result from 1 (useless) to 5 (excellent), with a short note. \
                          Grades feed the model registry's stats, which choose models for future threads."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "thread_id": {"type": "string"},
                "grade": {"type": "integer", "minimum": 1, "maximum": 5},
                "note": {"type": "string", "description": "Why, in a few words"}
            }, "required": ["thread_id", "grade"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let id = str_arg(&input, "thread_id");
        if !self.0.hub.db.thread(id).is_some_and(|t| t.conversation_id == self.0.conversation_id) {
            return ToolOutput::err("no such thread in this conversation");
        }
        let grade = input.get("grade").and_then(|g| g.as_i64().or_else(|| g.as_str()?.trim().parse().ok()));
        let Some(grade) = grade else { return ToolOutput::err("grade must be a number from 1 to 5") };
        match self.0.hub.db.grade_thread(id, grade, str_arg(&input, "note")) {
            Ok(()) => ToolOutput::ok(json!({"graded": id, "grade": grade}).to_string()),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

struct ProposeUseCases(RegCtx);
#[async_trait]
impl Tool for ProposeUseCases {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "propose_use_cases".into(),
            description: "Propose new use-case text for a model when its stats clearly contradict what the registry \
                          says it's for. The user approves or rejects it; nothing changes until then."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "model_id": {"type": "string"},
                "text": {"type": "string", "description": "The full new use cases, e.g. \"Long refactors, architecture review\""},
                "reason": {"type": "string", "description": "The evidence, e.g. \"9 of 10 code threads succeeded\""}
            }, "required": ["model_id", "text", "reason"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let db = &self.0.hub.db;
        match db.propose_use_cases(str_arg(&input, "model_id"), str_arg(&input, "text"), str_arg(&input, "reason"), "orchestrator") {
            Ok(p) => ToolOutput::ok(
                json!({"proposed": p.id, "note": "Waiting for the user (grouper models proposals)."}).to_string(),
            ),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

// ---- HTTP API (merged into the hub's router)

/// A response refusing the request unless it has a valid token.
pub(crate) fn deny_unless_authed(hub: &Hub, headers: &HeaderMap) -> Option<Response> {
    hub.auth(headers).is_none().then(|| err(StatusCode::UNAUTHORIZED, "invalid or missing device token"))
}

/// A response refusing the request unless it has an admin token, as every change
/// to the registry needs.
pub(crate) fn deny_unless_admin(hub: &Hub, headers: &HeaderMap) -> Option<Response> {
    match hub.auth(headers) {
        None => Some(err(StatusCode::UNAUTHORIZED, "invalid or missing device token")),
        Some(a) if !a.admin => Some(err(StatusCode::FORBIDDEN, "this needs an admin token (grouper-hub device add --admin)")),
        Some(_) => None,
    }
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/threads/{id}/rating", post(rate))
        .route("/v1/registry/stats", get(stats))
        .route("/v1/registry/proposals", get(list_proposals))
        .route("/v1/registry/proposals/{id}", post(decide))
        .route("/v1/registry/rules", get(list_rules).post(add_rule))
        .route("/v1/registry/rules/{id}", post(set_rule).delete(delete_rule))
}

#[derive(Deserialize)]
struct RatingBody {
    /// `up`, `down` or `clear`.
    rating: String,
}

async fn rate(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>, Json(b): Json<RatingBody>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let rating = match norm(&b.rating).as_str() {
        "up" | "+1" | "1" => Some(1),
        "down" | "-1" => Some(-1),
        "clear" | "none" | "0" => None,
        _ => return err(StatusCode::BAD_REQUEST, "rating must be up, down or clear"),
    };
    if hub.db.thread(&id).is_none() {
        return err(StatusCode::NOT_FOUND, format!("no thread {id}"));
    }
    match hub.db.rate_thread(&id, rating) {
        Ok(()) => Json(json!({"thread_id": id, "rating": rating})).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn stats(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_authed(&hub, &headers) {
        return r;
    }
    Json(hub.db.registry_stats()).into_response()
}

#[derive(Deserialize)]
struct ProposalQuery {
    /// `pending`, `accepted`, `rejected` or `all` (default `pending`).
    status: Option<String>,
}

async fn list_proposals(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<ProposalQuery>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let status = q.status.unwrap_or_else(|| "pending".into());
    Json(hub.db.proposals((status != "all").then_some(status.as_str()))).into_response()
}

#[derive(Deserialize)]
struct Decision {
    accept: bool,
}

async fn decide(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<i64>, Json(d): Json<Decision>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.decide_proposal(id, d.accept) {
        Ok(p) => Json(p).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn list_rules(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    Json(hub.db.rules()).into_response()
}

async fn add_rule(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(b): Json<RuleInput>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.add_rule(&b.text, &b.rule) {
        Ok(r) => Json(r).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct Enabled {
    enabled: bool,
}

async fn set_rule(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<i64>, Json(b): Json<Enabled>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.set_rule_enabled(id, b.enabled) {
        Ok(true) => Json(json!({"id": id, "enabled": b.enabled})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, format!("no rule #{id}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn delete_rule(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.delete_rule(id) {
        Ok(true) => Json(json!({"deleted": id})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, format!("no rule #{id}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_protocol::ProviderConfig;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-registry-test-{}", crate::db::random_hex(4)))
    }

    fn provider(db: &Db, id: &str, base_url: Option<&str>) {
        db.upsert_provider(&ProviderConfig {
            id: id.into(),
            kind: "scripted".into(),
            base_url: base_url.map(String::from),
            api_key: None,
            options: json!({"inline": {"default": [{"type": "text", "text": "OK"}]}}),
        })
        .unwrap();
    }

    fn model(id: &str, provider: &str, price: f64, roles: &[&str]) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            provider_id: provider.into(),
            model: format!("{id}-1"),
            input_price: price,
            output_price: price,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: roles.iter().map(|s| s.to_string()).collect(),
            local: false,
        }
    }

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    /// Insert a finished thread with an outcome (status done/failed/stopped).
    fn outcome(db: &Db, model: &str, device: &str, task_type: &str, status: &str, grade: Option<i64>, rating: Option<i64>) {
        let t = db.create_thread("c1", "t", device, model, None, "brief").unwrap();
        db.set_thread_meta(&t.id, task_type, &[]);
        let m = db.find_model(model).unwrap();
        db.record_usage(Some("c1"), Some(&t.id), &m.provider_id, &m.model, 100, 50, 0, 0);
        assert!(db.finish_thread(&t.id, status, "s"));
        assert!(db.record_outcome(&t.id));
        assert!(!db.record_outcome(&t.id), "recorded once");
        if let Some(g) = grade {
            db.grade_thread(&t.id, g, "note").unwrap();
        }
        if rating.is_some() {
            db.rate_thread(&t.id, rating).unwrap();
        }
    }

    #[test]
    fn local_urls() {
        for u in ["http://localhost:11434/v1", "http://127.0.0.1:8080", "http://[::1]:1234/x", "https://gpu-box.local/v1",
            "http://user@LOCALHOST", "localhost:11434"]
        {
            assert!(is_local_url(u), "{u}");
        }
        for u in ["https://openrouter.ai/api/v1", "https://api.anthropic.com", "http://localhost.example.com", "", "http://"] {
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
            cost,
        };
        let rows = [r("done", 10, 0.1), r("failed", 1, 0.3), r("done", 5, 0.2), r("stopped", 3, 0.2)];
        let s = summarize("m", "code", &rows.iter().collect::<Vec<_>>());
        assert_eq!(s.median_duration_s, Some(4.0));
        assert!((s.mean_cost.unwrap() - 0.2).abs() < 1e-9);
        assert_eq!((s.successes, s.success_rate.map(|x| (x * 3.0).round())), (2, Some(2.0)));
        let s = summarize("m", "code", &rows[..3].iter().collect::<Vec<_>>());
        assert_eq!((s.median_duration_s, s.mean_grade), (Some(5.0), None));
        assert_eq!(summarize("m", "", &[]).success_rate, None);
    }

    #[test]
    fn rules_are_validated() {
        let r = validate_rule(&RuleSpec { when_tag: Some(" Sensitive ".into()), require: Some("LOCAL".into()), ..Default::default() })
            .unwrap();
        assert_eq!((r.when_tag.as_deref(), r.require.as_deref()), (Some("sensitive"), Some("local")));
        assert!(validate_rule(&RuleSpec { require: Some("fast".into()), ..Default::default() }).is_err());
        assert!(validate_rule(&RuleSpec { prefer: Some("best".into()), ..Default::default() }).is_err());
        assert!(validate_rule(&RuleSpec { when_tag: Some("x".into()), ..Default::default() }).is_err());
        assert!(serde_json::from_str::<RuleSpec>(r#"{"when_tags": "x", "require": "local"}"#).is_err(), "typos are refused");
    }

    #[test]
    fn stats_per_model_device_and_task_type() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        provider(&db, "p", None);
        db.upsert_model(&model("a", "p", 1.0, &["thread"])).unwrap();
        outcome(&db, "a", "mac", "code", "done", Some(5), Some(1));
        outcome(&db, "a", "mac", "code", "done", Some(2), None); // graded poorly: not a success
        outcome(&db, "a", "pc", "code", "failed", None, None);
        outcome(&db, "a", "pc", "code", "stopped", None, None); // not in the success rate
        outcome(&db, "a", "pc", "writing", "done", None, Some(-1)); // rated down: not a success
        outcome(&db, "a", "pc", "", "done", None, None); // untyped: only in the overall row

        let s = db.outcome_stats(By::Model, Some("a"), None);
        assert_eq!(s.iter().map(|s| s.task_type.as_str()).collect::<Vec<_>>(), vec!["", "code", "writing"]);
        let code = &s[1];
        assert_eq!((code.done, code.failed, code.stopped, code.successes), (2, 1, 1, 1));
        assert!((code.success_rate.unwrap() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(code.mean_grade, Some(3.5));
        assert_eq!(code.net_rating, 1);
        assert!(code.mean_cost.unwrap() > 0.0);
        assert!(code.median_duration_s.is_some());
        let all = &s[0];
        assert_eq!((all.done, all.successes, all.net_rating), (4, 2, 0));
        assert!(stats_line(code).starts_with("code: 1/3 ok (33%), 1 stopped, grade 3.5, $"), "{}", stats_line(code));

        let d = db.outcome_stats(By::Device, None, Some("code"));
        assert_eq!(d.iter().map(|s| (s.subject.as_str(), s.done + s.failed)).collect::<Vec<_>>(), vec![("mac", 2), ("pc", 1)]);
        assert_eq!(db.registry_stats().devices.len(), 5); // mac: all, code; pc: all, code, writing

        // Ratings and grades need a finished thread.
        let running = db.create_thread("c1", "r", "mac", "a", None, "b").unwrap();
        assert!(db.grade_thread(&running.id, 4, "").unwrap_err().to_string().contains("still running"));
        assert!(db.rate_thread("nope", Some(1)).is_err());
        assert!(db.grade_thread(&running.id, 9, "").is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn routing_by_rules_then_stats_then_default() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        provider(&db, "cloud", Some("https://openrouter.ai/api/v1"));
        provider(&db, "ollama", Some("http://localhost:11434/v1"));
        db.upsert_model(&model("big", "cloud", 10.0, &["orchestrator", "thread"])).unwrap();
        db.upsert_model(&model("cheap", "cloud", 0.5, &[])).unwrap();
        let mut flagged = model("box", "cloud", 2.0, &[]);
        flagged.local = true; // runs on the user's own machine behind a tunnel
        db.upsert_model(&flagged).unwrap();
        db.upsert_model(&model("llama", "ollama", 0.0, &[])).unwrap();
        assert!(db.model_is_local(&db.find_model("llama").unwrap()));
        assert!(db.model_is_local(&db.find_model("box").unwrap()));
        assert!(!db.model_is_local(&db.find_model("big").unwrap()));

        // No rules, no stats: the registry default for threads.
        let r = db.route_model(None, Some("code"), &[]).unwrap();
        assert_eq!(r.model.id, "big");
        assert_eq!(r.reason.as_deref(), Some("model big: registry default for threads"));

        // Sensitive → local only.
        let sensitive = db
            .add_rule("Sensitive data: local only", &RuleSpec { when_tag: Some("sensitive".into()), require: Some("local".into()), ..Default::default() })
            .unwrap();
        db.add_rule("Drafts: cheapest", &RuleSpec { when_tag: Some("draft".into()), prefer: Some("cheapest".into()), ..Default::default() })
            .unwrap();
        let r = db.route_model(None, Some("writing"), &tags(&["Sensitive"])).unwrap();
        assert_eq!(r.model.id, "box"); // local ones: box and llama; neither has a record, so by id
        assert!(r.reason.as_deref().unwrap().contains("rule #1 \"Sensitive data: local only\" (require local)"), "{r:?}");
        // Drafts: cheapest (free beats 0.5).
        assert_eq!(db.route_model(None, None, &tags(&["draft"])).unwrap().model.id, "llama");
        // Both: local and cheapest.
        let r = db.route_model(None, None, &tags(&["draft", "sensitive"])).unwrap();
        assert_eq!(r.model.id, "llama");
        assert!(r.reason.unwrap().contains("preferring cheapest"));
        // An explicit model must pass the filters too.
        assert_eq!(db.route_model(Some("big"), None, &[]).unwrap().reason, None);
        let e = db.route_model(Some("big"), None, &tags(&["sensitive"])).unwrap_err();
        assert!(e.contains("ruled out") && e.contains("Sensitive data"), "{e}");
        assert!(db.route_model(Some("nope"), None, &[]).is_err());

        // Stats: a proven model beats the default for its task type, but not for others.
        for _ in 0..3 {
            outcome(&db, "cheap", "hub", "code", "done", Some(5), None);
        }
        for _ in 0..3 {
            outcome(&db, "big", "hub", "code", "done", Some(3), None);
        }
        let r = db.route_model(None, Some("code"), &[]).unwrap();
        assert_eq!(r.model.id, "cheap");
        assert!(r.reason.unwrap().contains("best code record: 3/3 succeeded, grade 5.0"));
        assert_eq!(db.route_model(None, Some("research"), &[]).unwrap().model.id, "big");
        // A poor record ranks below models without one.
        db.upsert_model(&model("big", "cloud", 10.0, &["orchestrator"])).unwrap();
        db.upsert_model(&model("cheap", "cloud", 0.5, &["thread"])).unwrap();
        assert_eq!(db.route_model(None, Some("data"), &[]).unwrap().model.id, "cheap");
        for _ in 0..4 {
            outcome(&db, "cheap", "hub", "data", "failed", None, None);
        }
        assert_eq!(db.route_model(None, Some("data"), &[]).unwrap().model.id, "big");

        // A rule that leaves nothing: a clear refusal. Disabled rules don't count.
        db.delete_model("llama").unwrap();
        db.delete_model("box").unwrap();
        let e = db.route_model(None, Some("writing"), &tags(&["sensitive"])).unwrap_err();
        assert!(e.contains("No registered model satisfies") && e.contains("Sensitive data: local only"), "{e}");
        assert!(db.set_rule_enabled(sensitive.id, false).unwrap());
        assert!(db.route_model(None, None, &tags(&["sensitive"])).is_ok());
        assert!(db.delete_rule(sensitive.id).unwrap());
        assert!(!db.delete_rule(sensitive.id).unwrap());
        assert_eq!(db.rules().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn proposals_trigger_once_and_wait_for_the_user() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        provider(&db, "p", None);
        let mut m = model("m", "p", 1.0, &["thread"]);
        m.use_cases = "Long refactors.".into();
        db.upsert_model(&m).unwrap();
        // Four good code results: not enough yet.
        for _ in 0..4 {
            outcome(&db, "m", "hub", "code", "done", None, None);
        }
        assert!(db.proposals(None).is_empty());
        // The fifth triggers one proposal.
        outcome(&db, "m", "hub", "code", "done", Some(4), None);
        let p = db.proposals(Some("pending"));
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].text.as_str(), p[0].source.as_str()), ("Long refactors; code tasks", "stats"));
        assert_eq!(p[0].reason, "5 of 5 finished code threads succeeded (100%)");
        // One pending per model, for automatic and orchestrator proposals alike.
        for _ in 0..5 {
            outcome(&db, "m", "hub", "writing", "failed", None, None);
        }
        assert_eq!(db.proposals(None).len(), 1);
        assert!(db.propose_use_cases("m", "anything", "x", "orchestrator").unwrap_err().to_string().contains("pending"));

        // Rejected: never proposed again; the writing one comes next.
        let r = db.decide_proposal(p[0].id, false).unwrap();
        assert_eq!(r.status, "rejected");
        assert!(db.decide_proposal(p[0].id, true).is_err());
        outcome(&db, "m", "hub", "code", "done", None, None);
        let p = db.proposals(Some("pending"));
        assert_eq!(p.len(), 0, "the code proposal isn't repeated; writing waits for its own next outcome");
        outcome(&db, "m", "hub", "writing", "failed", None, None);
        let p = db.proposals(Some("pending"));
        assert_eq!(p[0].text, "Long refactors; not for writing tasks");
        // Accepted: the registry changes, and the mention stops further proposals.
        db.decide_proposal(p[0].id, true).unwrap();
        assert_eq!(db.find_model("m").unwrap().use_cases, "Long refactors; not for writing tasks");
        outcome(&db, "m", "hub", "writing", "failed", None, None);
        assert!(db.proposals(Some("pending")).is_empty());
        // The orchestrator's own proposal.
        let o = db.propose_use_cases("m", "Refactors and code review", "graded 5 on reviews", "orchestrator").unwrap();
        assert_eq!(o.source, "orchestrator");
        assert!(db.propose_use_cases("nope", "x", "y", "orchestrator").is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Wait until the conversation's events satisfy `done`.
    async fn wait_for(hub: &Arc<Hub>, conversation: &str, done: impl Fn(&[core_protocol::Event]) -> bool) {
        for _ in 0..400 {
            if done(&hub.db.events(conversation, 0, None)) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("timed out: {:#?}", hub.db.events(conversation, 0, None));
    }

    fn replies(events: &[core_protocol::Event], text: &str) -> usize {
        events.iter().filter(|e| matches!(&e.body, core_protocol::EventBody::AssistantMessage { text: t } if t == text)).count()
    }

    /// The user message holding `needle`, as stored in the conversation's history.
    fn user_text(hub: &Hub, conversation: &str, needle: &str) -> String {
        hub.db
            .history(conversation)
            .iter()
            .filter(|m| m.role == providers::Role::User)
            .flat_map(|m| &m.content)
            .filter_map(|b| if let providers::Block::Text { text } = b { Some(text.clone()) } else { None })
            .find(|t| t.contains(needle))
            .unwrap()
    }

    /// A scripted orchestrator starts a sensitive writing thread without naming a
    /// model: the "sensitive → local" rule picks the local model, the outcome is
    /// recorded, the orchestrator grades it and saves a skill, the skill shows up in
    /// the next similar turn, and with no local model left the rule refuses clearly.
    #[tokio::test]
    async fn orchestrator_routes_grades_and_saves_skills() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let script = json!({"inline": {
            "rules": [
                {"system": "orchestrator of Grouper", "last": "please summarise my private notes", "reply": [
                    {"type": "tool_use", "name": "start_thread", "input": {
                        "title": "Summarise notes", "device": "hub", "goal": "Summarise the notes.",
                        "task_type": "Writing", "tags": ["sensitive"], "hub_reason": "No device needed."}}]},
                {"system": "orchestrator of Grouper", "last": "No registered model", "reply": [
                    {"type": "text", "text": "There's no local model for sensitive work."}]},
                {"system": "orchestrator of Grouper", "last": "\"thread_id\"", "reply": [
                    {"type": "text", "text": "Started a thread."}]},
                {"system": "orchestrator of Grouper", "last": " done]", "reply": [
                    {"type": "tool_use", "name": "grade_thread",
                     "input": {"thread_id": "$THREAD_ID", "grade": 4, "note": "Clear summary"}},
                    {"type": "tool_use", "name": "save_skill", "input": {
                        "name": "Summarise notes locally",
                        "description": "Summarise a set of confidential notes on a local model.",
                        "steps": "1. Hub thread, task_type writing, tags [sensitive]\n2. Grade the summary",
                        "triggers": ["notes", "summary"], "tools": ["write_file"],
                        "models": ["llama"], "devices": ["hub"]}}]},
                {"system": "orchestrator of Grouper", "last": "\"skill_id\"", "reply": [
                    {"type": "text", "text": "Graded and saved."}]},
                {"system": "subagent in Grouper", "reply": [{"type": "text", "text": "Summary written."}]}
            ],
            "default": [{"type": "text", "text": "OK"}]
        }});
        for (id, url) in [("cloud", None), ("ollama", Some("http://127.0.0.1:11434/v1"))] {
            db.upsert_provider(&ProviderConfig {
                id: id.into(),
                kind: "scripted".into(),
                base_url: url.map(String::from),
                api_key: None,
                options: script.clone(),
            })
            .unwrap();
        }
        db.upsert_model(&model("big", "cloud", 3.0, &["orchestrator", "standard", "thread"])).unwrap();
        db.upsert_model(&model("llama", "ollama", 0.0, &[])).unwrap();
        db.add_rule("Sensitive data: local only", &RuleSpec { when_tag: Some("sensitive".into()), require: Some("local".into()), ..Default::default() })
            .unwrap();
        let hub = Hub::new(db, dir.clone());

        let conv = hub
            .chat(core_protocol::ChatRequest { conversation_id: None, mode: Some("project".into()), text: "please summarise my private notes".into() })
            .await
            .unwrap();
        wait_for(&hub, &conv, |e| replies(e, "Graded and saved.") == 1).await;

        // Routed to the local model by the rule, and the reason is on the thread and its start event.
        let threads = hub.db.threads(Some(&conv));
        assert_eq!(threads.len(), 1);
        let t = &threads[0];
        assert_eq!((t.model.as_str(), t.status.as_str()), ("llama", "done"));
        let reason = t.reason.clone().unwrap();
        assert!(reason.starts_with("No device needed.; model llama: rule #1 \"Sensitive data: local only\" (require local)"), "{reason}");
        let events = hub.db.events(&conv, 0, None);
        assert!(events.iter().any(|e| matches!(&e.body,
            core_protocol::EventBody::ThreadStarted { model, reason: Some(r), .. } if model == "llama" && *r == reason)));
        assert_eq!(hub.db.thread_meta(&t.id), Some(("writing".into(), vec!["sensitive".into()])));

        // The outcome is recorded and graded.
        let s = hub.db.outcome_stats(By::Model, Some("llama"), Some("writing"));
        assert_eq!((s[0].done, s[0].successes, s[0].mean_grade), (1, 1, Some(4.0)));
        let note: Option<String> = hub
            .db
            .c()
            .query_row("SELECT grade_note FROM thread_outcomes WHERE thread_id = ?1", [&t.id], |r| r.get(0))
            .unwrap();
        assert_eq!(note.as_deref(), Some("Clear summary"));
        assert_eq!(hub.db.outcome_stats(By::Device, Some("hub"), None)[0].done, 1);
        // list_models shows it.
        let overview = models_overview(&hub.db);
        let llama = overview["models"].as_array().unwrap().iter().find(|m| m["id"] == "llama").unwrap();
        assert_eq!(llama["local"], json!(true));
        assert!(llama["stats"][0].as_str().unwrap().starts_with("all: 1/1 ok (100%), grade 4.0"), "{llama}");
        assert_eq!(overview["priority_rules"][0]["rule"], json!({"when_tag": "sensitive", "require": "local"}));

        // The skill is saved, and offered in the next similar turn.
        let skills = hub.db.search_skills("notes", 5);
        assert_eq!(skills.len(), 1);
        assert_eq!((skills[0].models.clone(), skills[0].devices.clone(), skills[0].success_count), (vec!["llama".into()], vec!["hub".into()], 1));
        hub.chat(core_protocol::ChatRequest {
            conversation_id: Some(conv.clone()),
            mode: None,
            text: "please summarise my private notes again".into(),
        })
        .await
        .unwrap();
        wait_for(&hub, &conv, |e| replies(e, "Graded and saved.") == 2).await;
        let text = user_text(&hub, &conv, "notes again");
        assert!(text.contains("[Skills]") && text.contains("Skill #1 \"Summarise notes locally\" (worked 1×"), "{text}");
        assert_eq!(hub.db.skill(1).unwrap().success_count, 2, "saved again under the same name");

        // No local model left: the rule refuses and the orchestrator relays it. The
        // skill isn't repeated in the same conversation.
        hub.db.delete_model("llama").unwrap();
        hub.chat(core_protocol::ChatRequest {
            conversation_id: Some(conv.clone()),
            mode: None,
            text: "please summarise my private notes once more".into(),
        })
        .await
        .unwrap();
        wait_for(&hub, &conv, |e| replies(e, "There's no local model for sensitive work.") == 1).await;
        assert!(!user_text(&hub, &conv, "once more").contains("[Skills]"));
        assert_eq!(hub.db.threads(Some(&conv)).len(), 2);
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn old_models_tables_get_the_local_column() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("grouper.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE models (id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, model TEXT NOT NULL,
               input_price REAL NOT NULL DEFAULT 0, output_price REAL NOT NULL DEFAULT 0,
               context_window INTEGER NOT NULL DEFAULT 0, use_cases TEXT NOT NULL DEFAULT '',
               free INTEGER NOT NULL DEFAULT 0, roles TEXT NOT NULL DEFAULT '');
             INSERT INTO models (id, provider_id, model) VALUES ('old', 'p', 'old-1');",
        )
        .unwrap();
        drop(conn);
        let db = Db::open(&dir).unwrap();
        assert!(!db.find_model("old").unwrap().local);
        drop(db);
        Db::open(&dir).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }
}
