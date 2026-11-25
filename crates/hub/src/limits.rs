//! Limits on running threads and on estimated spending. All of them are off until
//! the user sets them (`grouper limits set …`, `PUT /v1/limits`).
//!
//! - **Running threads**: `max_running_threads` (account) and
//!   `max_running_threads_per_device`. Running threads count, and so do the hub parts
//!   of threads waiting for their device (on the hub). `start_thread` is refused with
//!   a message the orchestrator can relay; checks and creation are serialized so
//!   parallel `start_thread` calls can't both take the last slot.
//! - **Budgets** (USD, estimated from registry prices as usage is recorded):
//!   `thread_budget_usd` (default per thread; `start_thread`'s `budget_usd` can only
//!   lower it), `conversation_budget_usd` (orchestrator plus threads) and
//!   `daily_budget_usd` (all usage in a UTC day). Spend reaching a budget stops the
//!   threads it covers through the normal stop path (by `budget`) and wakes their
//!   orchestrator; a spent conversation or daily budget also refuses new turns and
//!   new threads with an error saying how to raise it. A raised limit applies at once.
//! - **Warnings**: spend reaching `warn_at_fraction` (default 0.8) of a budget emits
//!   one [`EventBody::BudgetWarning`] per scope and period (thread, conversation,
//!   UTC day; again if the limit changes).
//! - **Routing**: a thread with a budget prefers cheaper models among otherwise equal
//!   ones when the orchestrator doesn't name a model (`Db::route_model_with`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use core_protocol::{DEFAULT_WARN_AT_FRACTION, EventBody, LIMIT_KEYS, Limits, LimitsReport, ThreadInfo};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::db::{Db, now};
use crate::offline::WAITING_DEVICE;
use crate::orchestrator::Wake;
use crate::registry::deny_unless_admin;
use crate::server::{HUB_DEVICE, Hub, err};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS limits (key TEXT PRIMARY KEY, value REAL NOT NULL);
CREATE TABLE IF NOT EXISTS thread_budgets (thread_id TEXT PRIMARY KEY, budget REAL NOT NULL);
CREATE TABLE IF NOT EXISTS budget_warnings (
  scope TEXT NOT NULL, subject TEXT NOT NULL, period TEXT NOT NULL, limit_usd REAL NOT NULL, created INTEGER NOT NULL,
  PRIMARY KEY (scope, subject, period, limit_usd));
CREATE INDEX IF NOT EXISTS usage_thread ON usage (thread_id);
CREATE INDEX IF NOT EXISTS usage_conversation ON usage (conversation_id);
CREATE INDEX IF NOT EXISTS usage_ts ON usage (ts);
";

pub const DAY: u64 = 86_400;

/// Start of the UTC day `ts` falls in.
pub fn day_start(ts: u64) -> u64 {
    ts - ts % DAY
}

/// The next 00:00 UTC after `ts`.
pub fn next_day(ts: u64) -> u64 {
    day_start(ts) + DAY
}

/// `5h 12m`, `12m`, `under a minute`.
pub fn duration_text(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    match (h, m) {
        (0, 0) => "under a minute".into(),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// Dollars for messages: `5.00`, `0.05`, `0.0012`.
pub fn usd(amount: f64) -> String {
    let mut s = format!("{amount:.4}");
    while s.ends_with('0') && s.len() - s.find('.').unwrap_or(s.len()) > 3 {
        s.pop();
    }
    s
}

/// Spend at or over `limit`, allowing for float rounding in summed costs.
fn reached(spent: f64, limit: f64) -> bool {
    spent + 1e-9 >= limit
}

fn has_budgets(l: &Limits) -> bool {
    l.thread_budget_usd.is_some() || l.conversation_budget_usd.is_some() || l.daily_budget_usd.is_some()
}

fn has_caps(l: &Limits) -> bool {
    l.max_running_threads.is_some() || l.max_running_threads_per_device.is_some()
}

pub fn warn_fraction(l: &Limits) -> f64 {
    l.warn_at_fraction.unwrap_or(DEFAULT_WARN_AT_FRACTION)
}

/// A thread's budget: what `start_thread` asked for, never above the account's default.
pub fn effective_thread_budget(asked: Option<f64>, account: Option<f64>) -> Option<f64> {
    match (asked, account) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Apply `PUT /v1/limits` changes (key → number, or null to turn it off) to `current`.
pub fn apply_changes(current: &Limits, changes: &Map<String, Value>) -> Result<Limits, String> {
    let mut l = current.clone();
    for (key, value) in changes {
        let n = match value {
            Value::Null => None,
            v => Some(v.as_f64().ok_or_else(|| format!("{key} must be a number, or null to turn it off"))?),
        };
        match key.as_str() {
            "max_running_threads" | "max_running_threads_per_device" => {
                let cap = n
                    .map(|n| {
                        if n >= 1.0 && n.fract() == 0.0 && n <= f64::from(u32::MAX) {
                            Ok(n as u32)
                        } else {
                            Err(format!("{key} must be a whole number of threads, 1 or more (or off)"))
                        }
                    })
                    .transpose()?;
                if key == "max_running_threads" {
                    l.max_running_threads = cap;
                } else {
                    l.max_running_threads_per_device = cap;
                }
            }
            "thread_budget_usd" | "conversation_budget_usd" | "daily_budget_usd" => {
                let budget = n
                    .map(|n| {
                        if n.is_finite() && n > 0.0 {
                            Ok(n)
                        } else {
                            Err(format!("{key} must be more than 0 US dollars (or off)"))
                        }
                    })
                    .transpose()?;
                match key.as_str() {
                    "thread_budget_usd" => l.thread_budget_usd = budget,
                    "conversation_budget_usd" => l.conversation_budget_usd = budget,
                    _ => l.daily_budget_usd = budget,
                }
            }
            "warn_at_fraction" => {
                l.warn_at_fraction = n
                    .map(|n| {
                        if n > 0.0 && n <= 1.0 {
                            Ok(n)
                        } else {
                            Err("warn_at_fraction must be more than 0 and at most 1 (off restores 0.8)".to_string())
                        }
                    })
                    .transpose()?;
            }
            _ => return Err(format!("unknown limit {key:?}; the limits are {}", LIMIT_KEYS.join(", "))),
        }
    }
    Ok(l)
}

/// Why a spent budget stops new work, if one is spent. Daily first.
pub fn budget_refusal(l: &Limits, spent_today: f64, spent_conversation: f64, at: u64) -> Option<String> {
    if let Some(limit) = l.daily_budget_usd
        && reached(spent_today, limit)
    {
        return Some(format!(
            "The daily budget of ${} is spent (estimated ${} so far today, UTC). New work resumes at 00:00 UTC \
             (in {}), or sooner if the user raises it: `grouper limits set daily_budget_usd <amount>` (or `off`).",
            usd(limit),
            usd(spent_today),
            duration_text(next_day(at) - at)
        ));
    }
    if let Some(limit) = l.conversation_budget_usd
        && reached(spent_conversation, limit)
    {
        return Some(format!(
            "This conversation's budget of ${} is spent (estimated ${}). To continue, the user can raise it: \
             `grouper limits set conversation_budget_usd <amount>` (or `off`).",
            usd(limit),
            usd(spent_conversation)
        ));
    }
    None
}

/// Why `start_thread` is refused, if it is: a running-thread cap or a spent budget.
#[allow(clippy::too_many_arguments)]
pub fn start_refusal(
    l: &Limits,
    running: u32,
    running_on_device: u32,
    device_name: &str,
    spent_today: f64,
    spent_conversation: f64,
    at: u64,
) -> Option<String> {
    if let Some(cap) = l.max_running_threads
        && running >= cap
    {
        return Some(format!(
            "Not started: {running} thread(s) already running, the user's limit (max_running_threads = {cap}). Wait for \
             one to finish (you're woken when it does) or stop one; only the user can raise the limit \
             (`grouper limits set max_running_threads <n>`)."
        ));
    }
    if let Some(cap) = l.max_running_threads_per_device
        && running_on_device >= cap
    {
        return Some(format!(
            "Not started: {device_name} already runs {running_on_device} thread(s), the user's per-device limit \
             (max_running_threads_per_device = {cap}). Use another device, wait for one to finish, or stop one; only \
             the user can raise the limit."
        ));
    }
    budget_refusal(l, spent_today, spent_conversation, at).map(|why| format!("Not started: {why}"))
}

/// A thread that may still spend: running, or waiting for its device.
fn active(status: &str) -> bool {
    status == "running" || status == WAITING_DEVICE
}

struct ActiveThread {
    id: String,
    conversation_id: String,
    device_id: String,
    status: String,
}

impl Db {
    /// The user's limits (none set by default).
    pub fn limits(&self) -> Limits {
        let rows: Vec<(String, f64)> = {
            let c = self.c();
            let Ok(mut st) = c.prepare("SELECT key, value FROM limits") else { return Limits::default() };
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default()
        };
        let mut l = Limits::default();
        for (key, value) in rows {
            match key.as_str() {
                "max_running_threads" => l.max_running_threads = Some(value as u32),
                "max_running_threads_per_device" => l.max_running_threads_per_device = Some(value as u32),
                "thread_budget_usd" => l.thread_budget_usd = Some(value),
                "conversation_budget_usd" => l.conversation_budget_usd = Some(value),
                "daily_budget_usd" => l.daily_budget_usd = Some(value),
                "warn_at_fraction" => l.warn_at_fraction = Some(value),
                _ => {}
            }
        }
        l
    }

    /// Replace the limits. Validate with [`apply_changes`] first.
    pub fn set_limits(&self, l: &Limits) -> anyhow::Result<()> {
        let values = [
            ("max_running_threads", l.max_running_threads.map(f64::from)),
            ("max_running_threads_per_device", l.max_running_threads_per_device.map(f64::from)),
            ("thread_budget_usd", l.thread_budget_usd),
            ("conversation_budget_usd", l.conversation_budget_usd),
            ("daily_budget_usd", l.daily_budget_usd),
            ("warn_at_fraction", l.warn_at_fraction),
        ];
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM limits", [])?;
        for (key, value) in values {
            if let Some(v) = value {
                tx.execute("INSERT INTO limits (key, value) VALUES (?1, ?2)", params![key, v])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The budget `start_thread` asked for this thread, if any.
    pub fn asked_thread_budget(&self, thread_id: &str) -> Option<f64> {
        self.c()
            .query_row("SELECT budget FROM thread_budgets WHERE thread_id = ?1", [thread_id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    fn set_asked_thread_budget(&self, thread_id: &str, budget: f64) {
        let _ = self.c().execute(
            "INSERT OR REPLACE INTO thread_budgets (thread_id, budget) VALUES (?1, ?2)",
            params![thread_id, budget],
        );
    }

    /// The thread's budget in force: its own, never above the account default.
    pub fn thread_budget(&self, thread_id: &str, l: &Limits) -> Option<f64> {
        effective_thread_budget(self.asked_thread_budget(thread_id), l.thread_budget_usd)
    }

    fn spent(&self, sql: &str, arg: impl rusqlite::ToSql) -> f64 {
        self.c().query_row(sql, [arg], |r| r.get(0)).unwrap_or(0.0)
    }

    /// Estimated USD a thread has used.
    pub fn spent_by_thread(&self, thread_id: &str) -> f64 {
        self.spent("SELECT COALESCE(SUM(cost), 0) FROM usage WHERE thread_id = ?1", thread_id)
    }

    /// Estimated USD a conversation has used (its orchestrator and threads).
    pub fn spent_by_conversation(&self, conversation_id: &str) -> f64 {
        self.spent("SELECT COALESCE(SUM(cost), 0) FROM usage WHERE conversation_id = ?1", conversation_id)
    }

    /// Estimated USD used since `since` (Unix seconds), everything included.
    pub fn spent_since(&self, since: u64) -> f64 {
        self.spent("SELECT COALESCE(SUM(cost), 0) FROM usage WHERE ts >= ?1", since as i64)
    }

    fn active_threads(&self) -> Vec<ActiveThread> {
        let c = self.c();
        let Ok(mut st) = c.prepare("SELECT id, conversation_id, device_id, status FROM threads WHERE status IN ('running', ?1)")
        else {
            return Vec::new();
        };
        st.query_map([WAITING_DEVICE], |r| {
            Ok(ActiveThread { id: r.get(0)?, conversation_id: r.get(1)?, device_id: r.get(2)?, status: r.get(3)? })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    /// True the first time a warning for this scope, subject, period and limit is recorded.
    fn first_warning(&self, scope: &str, subject: &str, period: &str, limit: f64) -> bool {
        self.c()
            .execute(
                "INSERT OR IGNORE INTO budget_warnings (scope, subject, period, limit_usd, created)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![scope, subject, period, limit, now() as i64],
            )
            .is_ok_and(|n| n > 0)
    }
}

/// Hub-side state for limits.
#[derive(Default)]
pub struct LimitState {
    /// Held from the running-thread check until the new thread is recorded.
    start: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Running {
    total: u32,
    per_device: HashMap<String, u32>,
}

/// Threads counted against the caps: running ones on their device, and hub parts of
/// waiting ones on the hub.
async fn running(hub: &Hub) -> Running {
    let threads = hub.db.active_threads();
    let parts = hub.hub_threads.lock().await;
    let mut r = Running::default();
    for t in threads {
        let device = if t.status == "running" {
            t.device_id
        } else if parts.contains_key(&t.id) {
            HUB_DEVICE.to_string()
        } else {
            continue;
        };
        r.total += 1;
        *r.per_device.entry(device).or_default() += 1;
    }
    r
}

/// Permission to start a thread: holds the start lock (when a cap is set) until dropped.
pub(crate) struct Admission<'a> {
    _slot: Option<tokio::sync::MutexGuard<'a, ()>>,
    /// The new thread's budget in force.
    pub budget: Option<f64>,
    asked: Option<f64>,
}

impl Admission<'_> {
    /// Remember the budget `start_thread` asked for.
    pub(crate) fn record(&self, db: &Db, thread_id: &str) {
        if let Some(b) = self.asked {
            db.set_asked_thread_budget(thread_id, b);
        }
    }
}

/// Check the caps and budgets before a thread starts on `device_id` (the hub, when
/// the target device is offline and the hub prepares). Errors are for the orchestrator.
pub(crate) async fn admit<'a>(
    hub: &'a Hub,
    conversation_id: &str,
    device_id: &str,
    device_name: &str,
    asked: Option<f64>,
) -> Result<Admission<'a>, String> {
    if let Some(b) = asked
        && !(b.is_finite() && b > 0.0)
    {
        return Err("budget_usd must be more than 0 US dollars".into());
    }
    let l = hub.db.limits();
    let budget = effective_thread_budget(asked, l.thread_budget_usd);
    let slot = if has_caps(&l) { Some(hub.limits.start.lock().await) } else { None };
    if has_caps(&l) || has_budgets(&l) {
        let r = if has_caps(&l) { running(hub).await } else { Running::default() };
        let at = now();
        let today = if l.daily_budget_usd.is_some() { hub.db.spent_since(day_start(at)) } else { 0.0 };
        let conversation =
            if l.conversation_budget_usd.is_some() { hub.db.spent_by_conversation(conversation_id) } else { 0.0 };
        let on_device = r.per_device.get(device_id).copied().unwrap_or(0);
        if let Some(why) = start_refusal(&l, r.total, on_device, device_name, today, conversation, at) {
            return Err(why);
        }
    }
    Ok(Admission { _slot: slot, budget, asked })
}

/// Why a conversation's turn can't run now (a spent daily or conversation budget).
pub fn turn_refusal(db: &Db, conversation_id: &str) -> Option<String> {
    let l = db.limits();
    if l.daily_budget_usd.is_none() && l.conversation_budget_usd.is_none() {
        return None;
    }
    let at = now();
    let today = if l.daily_budget_usd.is_some() { db.spent_since(day_start(at)) } else { 0.0 };
    let conversation = if l.conversation_budget_usd.is_some() { db.spent_by_conversation(conversation_id) } else { 0.0 };
    budget_refusal(&l, today, conversation, at)
}

/// Refuse a turn when a budget is spent: what woke the conversation is kept in its
/// history (the next turn sees it), and an error says why and how to raise the limit.
pub(crate) fn refuse_turn(hub: &Arc<Hub>, conversation_id: &str, wakes: &[Wake]) -> bool {
    let Some(message) = turn_refusal(&hub.db, conversation_id) else { return false };
    let mut history = hub.db.history(conversation_id);
    let merged = history.last().is_some_and(|m| m.role == providers::Role::User);
    let from = if merged { history.len() - 1 } else { history.len() };
    let text = wakes.iter().map(Wake::render).collect::<Vec<_>>().join("\n\n");
    core_agent::push_user_text(&mut history, text);
    if let Err(e) = hub.db.save_history(conversation_id, from, &history[from..]) {
        tracing::error!("saving history: {e:#}");
    }
    hub.emit(conversation_id, None, EventBody::Error { message });
    let running = hub.db.threads(Some(conversation_id)).iter().filter(|t| t.status == "running").count() as u32;
    hub.emit(conversation_id, None, EventBody::TurnFinished { running_threads: running });
    true
}

/// After a model call's usage was recorded: warn, and stop what a reached budget covers.
pub(crate) fn after_usage(hub: &Arc<Hub>, conversation_id: &str, thread: Option<&ThreadInfo>, cost: f64) {
    if cost <= 0.0 {
        return;
    }
    let l = hub.db.limits();
    if !has_budgets(&l) {
        return;
    }
    let warn = warn_fraction(&l);
    let at = now();
    let thread_id = thread.map(|t| t.id.as_str());
    let check = |scope: &str, subject: &str, period: &str, spent: f64, limit: f64| -> bool {
        if reached(spent, limit) {
            return true;
        }
        if reached(spent, warn * limit) && hub.db.first_warning(scope, subject, period, limit) {
            hub.emit(conversation_id, thread_id, EventBody::BudgetWarning { scope: scope.into(), spent, limit });
        }
        false
    };

    if let Some(t) = thread.filter(|t| active(&t.status))
        && let Some(limit) = hub.db.thread_budget(&t.id, &l)
    {
        let spent = hub.db.spent_by_thread(&t.id);
        if check("thread", &t.id, "", spent, limit) {
            let summary =
                format!("Stopped: budget of ${} reached (estimated cost ${}).", usd(limit), usd(spent));
            stop_for_budget(hub, vec![t.id.clone()], summary);
        }
    }
    if let Some(limit) = l.conversation_budget_usd {
        let spent = hub.db.spent_by_conversation(conversation_id);
        if check("conversation", conversation_id, "", spent, limit) {
            let ids = hub
                .db
                .active_threads()
                .into_iter()
                .filter(|t| t.conversation_id == conversation_id)
                .map(|t| t.id)
                .collect();
            let summary = format!(
                "Stopped: this conversation's budget of ${} was reached (estimated ${}).",
                usd(limit),
                usd(spent)
            );
            stop_for_budget(hub, ids, summary);
        }
    }
    if let Some(limit) = l.daily_budget_usd {
        let spent = hub.db.spent_since(day_start(at));
        let day = (at / DAY).to_string();
        if check("daily", "", &day, spent, limit) {
            let ids = hub.db.active_threads().into_iter().map(|t| t.id).collect();
            let summary = format!(
                "Stopped: the daily budget of ${} was reached (estimated ${} today, UTC).",
                usd(limit),
                usd(spent)
            );
            stop_for_budget(hub, ids, summary);
        }
    }
}

/// Stop threads through the normal stop path (by `budget`) and wake their orchestrators.
fn stop_for_budget(hub: &Arc<Hub>, ids: Vec<String>, summary: String) {
    if ids.is_empty() {
        return;
    }
    let hub = hub.clone();
    tokio::spawn(async move {
        for id in ids {
            // Already stopped (an earlier usage report got there first): nothing to do.
            if hub.stop_thread_as(&id, "budget", &summary).await.is_err() {
                continue;
            }
            tracing::info!(thread = %id, "{summary}");
            if let Some(t) = hub.db.thread(&id) {
                let wake = Wake::ThreadDone {
                    thread_id: t.id,
                    title: t.title,
                    status: "stopped".into(),
                    summary: summary.clone(),
                };
                hub.wake(&t.conversation_id, wake).await;
            }
        }
    });
}

/// For the orchestrator's `[Status]` line, only when limits are set.
pub(crate) async fn status_note(hub: &Hub, conversation_id: &str) -> Option<String> {
    let l = hub.db.limits();
    if !has_budgets(&l) && !has_caps(&l) {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(limit) = l.daily_budget_usd {
        parts.push(format!("today ${} of ${}", usd(hub.db.spent_since(day_start(now()))), usd(limit)));
    }
    if let Some(limit) = l.conversation_budget_usd {
        parts.push(format!("this conversation ${} of ${}", usd(hub.db.spent_by_conversation(conversation_id)), usd(limit)));
    }
    if let Some(limit) = l.thread_budget_usd {
        parts.push(format!("${} per thread", usd(limit)));
    }
    if has_caps(&l) {
        let r = running(hub).await;
        if let Some(cap) = l.max_running_threads {
            parts.push(format!("{} of {cap} threads running", r.total));
        }
        if let Some(cap) = l.max_running_threads_per_device {
            parts.push(format!("at most {cap} per device"));
        }
    }
    Some(format!(" Limits (spend is estimated, USD): {}.", parts.join("; ")))
}

/// For `list_usage`: what's left of each budget in force, only when limits are set.
pub(crate) async fn overview(hub: &Hub, conversation_id: &str) -> Option<Value> {
    let l = hub.db.limits();
    if !has_budgets(&l) && !has_caps(&l) {
        return None;
    }
    let at = now();
    let mut v = json!({"note": "Spend is estimated from registry prices, in USD."});
    let budget = |spent: f64, limit: f64| json!({"limit": limit, "spent": spent, "remaining": (limit - spent).max(0.0)});
    if let Some(limit) = l.daily_budget_usd {
        v["daily"] = budget(hub.db.spent_since(day_start(at)), limit);
        v["daily"]["resets_at"] = json!(next_day(at));
    }
    if let Some(limit) = l.conversation_budget_usd {
        v["conversation"] = budget(hub.db.spent_by_conversation(conversation_id), limit);
    }
    if let Some(limit) = l.thread_budget_usd {
        v["thread_budget_usd"] = json!(limit);
    }
    if has_caps(&l) {
        let r = running(hub).await;
        v["running_threads"] = json!(r.total);
        if let Some(cap) = l.max_running_threads {
            v["max_running_threads"] = json!(cap);
        }
        if let Some(cap) = l.max_running_threads_per_device {
            v["max_running_threads_per_device"] = json!(cap);
        }
    }
    Some(v)
}

pub async fn report(hub: &Hub) -> LimitsReport {
    let limits = hub.db.limits();
    let at = now();
    LimitsReport {
        warn_at_fraction: warn_fraction(&limits),
        limits,
        spent_today_usd: hub.db.spent_since(day_start(at)),
        day_resets_at: next_day(at),
        running_threads: running(hub).await.total,
    }
}

// ---- HTTP API (merged into the hub's router; admin only)

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new().route("/v1/limits", get(get_limits).put(put_limits))
}

async fn get_limits(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    Json(report(&hub).await).into_response()
}

/// Body: the keys to change, e.g. `{"thread_budget_usd": 0.5, "daily_budget_usd": null}`.
async fn put_limits(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some(changes) = body.as_object() else {
        return err(StatusCode::BAD_REQUEST, "send a JSON object of limits to change, null to turn one off");
    };
    let limits = match apply_changes(&hub.db.limits(), changes) {
        Ok(l) => l,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    if let Err(e) = hub.db.set_limits(&limits) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    tracing::info!(?limits, "limits changed");
    Json(report(&hub).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn period_maths() {
        // 2026-11-25 00:00:00 UTC
        let midnight = 1_795_564_800;
        assert_eq!(day_start(midnight), midnight);
        assert_eq!(day_start(midnight + 1), midnight);
        assert_eq!(day_start(midnight + DAY - 1), midnight);
        assert_eq!(day_start(midnight + DAY), midnight + DAY);
        assert_eq!(next_day(midnight), midnight + DAY);
        assert_eq!(next_day(midnight + DAY - 1), midnight + DAY);
        assert_eq!(duration_text(0), "under a minute");
        assert_eq!(duration_text(59), "under a minute");
        assert_eq!(duration_text(61), "1m");
        assert_eq!(duration_text(5 * 3600 + 12 * 60 + 30), "5h 12m");
        assert_eq!(usd(5.0), "5.00");
        assert_eq!(usd(0.05), "0.05");
        assert_eq!(usd(0.0012), "0.0012");
        assert_eq!(usd(0.06000001), "0.06");
    }

    #[test]
    fn changes_are_validated_and_off_clears() {
        let l = apply_changes(
            &Limits::default(),
            &changes(json!({"max_running_threads": 3, "thread_budget_usd": 0.5, "warn_at_fraction": 0.9})),
        )
        .unwrap();
        assert_eq!(l.max_running_threads, Some(3));
        assert_eq!(l.thread_budget_usd, Some(0.5));
        assert_eq!(warn_fraction(&l), 0.9);
        let l = apply_changes(&l, &changes(json!({"thread_budget_usd": null, "warn_at_fraction": null}))).unwrap();
        assert_eq!((l.thread_budget_usd, l.max_running_threads), (None, Some(3)));
        assert_eq!(warn_fraction(&l), DEFAULT_WARN_AT_FRACTION);
        for bad in [
            json!({"max_running_threads": 0}),
            json!({"max_running_threads": 1.5}),
            json!({"max_running_threads_per_device": -1}),
            json!({"daily_budget_usd": 0}),
            json!({"conversation_budget_usd": "5"}),
            json!({"warn_at_fraction": 1.5}),
            json!({"warn_at_fraction": 0}),
            json!({"monthly_budget_usd": 5}),
        ] {
            assert!(apply_changes(&Limits::default(), &changes(bad.clone())).is_err(), "{bad}");
        }
    }

    #[test]
    fn thread_budget_never_exceeds_the_account_default() {
        assert_eq!(effective_thread_budget(None, None), None);
        assert_eq!(effective_thread_budget(Some(0.2), None), Some(0.2));
        assert_eq!(effective_thread_budget(None, Some(0.5)), Some(0.5));
        assert_eq!(effective_thread_budget(Some(0.2), Some(0.5)), Some(0.2));
        assert_eq!(effective_thread_budget(Some(2.0), Some(0.5)), Some(0.5));
    }

    #[test]
    fn start_checks() {
        let at = 1_795_564_800 + 3600;
        let none = Limits::default();
        assert_eq!(start_refusal(&none, 99, 99, "Mac", 1e9, 1e9, at), None, "nothing set: nothing refused");

        let caps = Limits { max_running_threads: Some(2), max_running_threads_per_device: Some(1), ..Default::default() };
        assert_eq!(start_refusal(&caps, 1, 0, "Mac", 0.0, 0.0, at), None);
        let e = start_refusal(&caps, 2, 0, "Mac", 0.0, 0.0, at).unwrap();
        assert!(e.contains("max_running_threads = 2"), "{e}");
        let e = start_refusal(&caps, 1, 1, "Mac", 0.0, 0.0, at).unwrap();
        assert!(e.contains("Mac already runs 1") && e.contains("per-device"), "{e}");

        let budgets = Limits { daily_budget_usd: Some(1.0), conversation_budget_usd: Some(0.5), ..Default::default() };
        assert_eq!(start_refusal(&budgets, 0, 0, "Mac", 0.99, 0.49, at), None);
        let e = start_refusal(&budgets, 0, 0, "Mac", 1.0, 0.0, at).unwrap();
        assert!(e.starts_with("Not started: The daily budget of $1.00 is spent"), "{e}");
        assert!(e.contains("(in 23h 0m)") && e.contains("estimated"), "{e}");
        let e = start_refusal(&budgets, 0, 0, "Mac", 0.2, 0.5, at).unwrap();
        assert!(e.contains("conversation's budget of $0.50 is spent") && e.contains("conversation_budget_usd"), "{e}");
        assert!(budget_refusal(&budgets, 0.0, 0.0, at).is_none());
    }

    #[test]
    fn a_budget_prefers_cheaper_models_among_equals() {
        use core_protocol::{ModelEntry, ProviderConfig};
        let dir = std::env::temp_dir().join(format!("grouper-limits-route-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
        let model = |id: &str, price: f64, roles: &[&str]| ModelEntry {
            id: id.into(),
            provider_id: "p".into(),
            model: id.into(),
            input_price: price,
            output_price: price,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: roles.iter().map(|r| r.to_string()).collect(),
            local: false,
            modalities: vec![],
        };
        db.upsert_model(&model("big", 10.0, &["orchestrator", "thread"])).unwrap();
        db.upsert_model(&model("cheap", 0.5, &[])).unwrap();
        assert_eq!(db.route_model_with(None, Some("code"), &[], false).unwrap().model.id, "big");
        let r = db.route_model_with(None, Some("code"), &[], true).unwrap();
        assert_eq!(r.model.id, "cheap");
        assert!(r.reason.unwrap().contains("cheaper among equals"));
        // A soft preference: a model the orchestrator names is used as is.
        assert_eq!(db.route_model_with(Some("big"), None, &[], true).unwrap().model.id, "big");
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn settings_round_trip_and_spend_queries() {
        let dir = std::env::temp_dir().join(format!("grouper-limits-test-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.limits(), Limits::default());
        let l = Limits {
            max_running_threads: Some(4),
            max_running_threads_per_device: Some(2),
            thread_budget_usd: Some(0.25),
            conversation_budget_usd: Some(2.0),
            daily_budget_usd: Some(10.0),
            warn_at_fraction: Some(0.5),
        };
        db.set_limits(&l).unwrap();
        assert_eq!(db.limits(), l);

        let t = db.create_thread("c1", "t", "hub", "m", None, "b").unwrap();
        assert_eq!(db.thread_budget(&t.id, &l), Some(0.25));
        db.set_asked_thread_budget(&t.id, 0.1);
        assert_eq!(db.thread_budget(&t.id, &l), Some(0.1));
        assert_eq!(db.thread_budget(&t.id, &Limits::default()), Some(0.1));

        {
            let c = db.c();
            for (conv, thread, cost, ts) in
                [("c1", Some(t.id.as_str()), 0.1, 1_000), ("c1", None, 0.2, DAY + 5), ("c2", None, 0.4, DAY + 10)]
            {
                c.execute(
                    "INSERT INTO usage (conversation_id, thread_id, provider_id, model, input_tokens, output_tokens,
                       cache_read_tokens, cache_write_tokens, cost, ts) VALUES (?1, ?2, 'p', 'm', 0, 0, 0, 0, ?3, ?4)",
                    params![conv, thread, cost, ts as i64],
                )
                .unwrap();
            }
        }
        assert!((db.spent_by_thread(&t.id) - 0.1).abs() < 1e-9);
        assert!((db.spent_by_conversation("c1") - 0.3).abs() < 1e-9);
        assert!((db.spent_since(day_start(DAY + 100)) - 0.6).abs() < 1e-9);
        assert_eq!(db.active_threads().len(), 1);

        assert!(db.first_warning("daily", "", "1", 10.0));
        assert!(!db.first_warning("daily", "", "1", 10.0), "once per period");
        assert!(db.first_warning("daily", "", "2", 10.0), "again the next day");
        assert!(db.first_warning("daily", "", "2", 20.0), "again when the limit changes");

        db.set_limits(&Limits::default()).unwrap();
        assert_eq!(db.limits(), Limits::default());
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
