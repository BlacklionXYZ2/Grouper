//! Model registry hub glue: `Db` wrappers, orchestrator tools (`grade_thread`,
//! `propose_use_cases`) and HTTP routes (`/v1/registry/*`, `/v1/threads/{id}/rating`).
//!
//! Schema/migrations, stats, routing, proposals and priority rules live in [`core_registry`].

use std::collections::{HashMap, HashSet};
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
pub use core_registry::{
    By, MIN_ROUTING_RUNS, PROPOSAL_HIGH, PROPOSAL_LOW, PROPOSAL_MIN_RUNS, Route, TASK_TYPES, clean_tags,
    format_duration_s, is_local_url, probe_overview_line, stats_line, validate_rule,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::db::Db;
use crate::server::{Hub, err};

impl Db {
    // ---- thread metadata and outcomes

    /// Remember what kind of work a thread is, for its outcome later.
    pub fn set_thread_meta(&self, thread_id: &str, task_type: &str, tags: &[String]) {
        core_registry::set_thread_meta(&self.c(), thread_id, task_type, tags);
    }

    /// (task type, tags) given when the thread started.
    pub fn thread_meta(&self, thread_id: &str) -> Option<(String, Vec<String>)> {
        core_registry::thread_meta(&self.c(), thread_id)
    }

    /// Record how a finished thread went: status, duration, and its cost and tokens
    /// from the usage table. Once per thread; then checks whether the model's use
    /// cases should change. Returns false if the thread isn't finished or was recorded.
    pub fn record_outcome(&self, thread_id: &str) -> bool {
        let Some(mut t) = self.thread(thread_id) else { return false };
        let models = self.models();
        // A free model swapped for another mid-thread: the outcome belongs to the model
        // that wrote most of the thread's output, if it is registered.
        if let Some(answered) = self.main_answering_model(thread_id)
            && let Some(m) = models.iter().find(|m| m.provider_id == answered.0 && m.model == answered.1)
        {
            t.model = m.id.clone();
        }
        let model = models.into_iter().find(|m| m.id == t.model);
        core_registry::record_thread_outcome(&self.c(), &t, model.as_ref())
    }

    /// The (provider, provider model name) with the most output tokens in a thread's usage.
    fn main_answering_model(&self, thread_id: &str) -> Option<(String, String)> {
        self.c()
            .query_row(
                "SELECT provider_id, model FROM usage WHERE thread_id = ?1 GROUP BY provider_id, model
                   ORDER BY SUM(output_tokens) DESC, MAX(id) DESC LIMIT 1",
                [thread_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    }

    /// Record one deterministic probe outcome into `thread_outcomes` with `source = 'probe'`,
    /// distinguishable from real thread outcomes.
    #[allow(clippy::too_many_arguments)]
    pub fn record_probe_outcome(
        &self,
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
        core_registry::record_probe_outcome(
            &self.c(),
            probe_run_id,
            model_id,
            device_id,
            task_type,
            passed,
            latency_ms,
            cost,
            tokens,
            detail,
        )
    }

    /// The (model, task type) of a recorded outcome; an error says why there's none.
    fn outcome_key(&self, thread_id: &str) -> Result<(String, String)> {
        let key = core_registry::outcome_key(&self.c(), thread_id)?;
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
        core_registry::validate_grade(grade)?;
        let (model_id, task_type) = self.outcome_key(thread_id)?;
        let model = self.models().into_iter().find(|m| m.id == model_id);
        core_registry::grade_outcome(&self.c(), thread_id, grade, note, model.as_ref(), &task_type)
    }

    /// Your thumbs up (1), down (-1), or neither (`None`) for a finished thread.
    pub fn rate_thread(&self, thread_id: &str, rating: Option<i64>) -> Result<()> {
        let (model_id, task_type) = self.outcome_key(thread_id)?;
        let model = self.models().into_iter().find(|m| m.id == model_id);
        core_registry::rate_outcome(&self.c(), thread_id, rating, model.as_ref(), &task_type)
    }

    /// Stats per model or device for real thread outcomes: one row per task type, plus
    /// (unless `task_type` is given) one row with an empty task type for everything together.
    pub fn outcome_stats(&self, by: By, subject: Option<&str>, task_type: Option<&str>) -> Vec<OutcomeStats> {
        core_registry::outcome_stats(&self.c(), by, subject, task_type)
    }

    /// Stats per model for deterministic probe runs (`source = 'probe'`).
    pub fn probe_stats(&self, model_id: Option<&str>, task_type: Option<&str>) -> Vec<OutcomeStats> {
        core_registry::probe_stats(&self.c(), model_id, task_type)
    }

    /// Stats per model or device filtered by outcome `source` (`thread` or `probe`).
    pub fn outcome_stats_by_source(
        &self,
        by: By,
        subject: Option<&str>,
        task_type: Option<&str>,
        source: &str,
    ) -> Vec<OutcomeStats> {
        core_registry::outcome_stats_by_source(&self.c(), by, subject, task_type, source)
    }

    pub fn registry_stats(&self) -> RegistryStats {
        core_registry::registry_stats(&self.c())
    }

    // ---- use-case proposals

    pub fn proposals(&self, status: Option<&str>) -> Vec<RegistryProposal> {
        core_registry::proposals(&self.c(), status)
    }

    pub fn proposal(&self, id: i64) -> Option<RegistryProposal> {
        core_registry::proposal(&self.c(), id)
    }

    /// Propose new use-case text for a model. One pending proposal per model.
    pub fn propose_use_cases(&self, model_id: &str, text: &str, reason: &str, source: &str) -> Result<RegistryProposal> {
        let Some(model) = self.models().into_iter().find(|m| m.id == model_id) else {
            bail!("no model {model_id:?}")
        };
        core_registry::propose_use_cases(&self.c(), &model, text, reason, source)
    }

    /// Create or update a pending use-case proposal from a probe run.
    pub fn upsert_probe_proposal(
        &self,
        model_id: &str,
        probe_summary: &str,
        probe_reason: &str,
    ) -> Result<Option<RegistryProposal>> {
        let Some(model) = self.models().into_iter().find(|m| m.id == model_id) else {
            bail!("no model {model_id:?}")
        };
        core_registry::upsert_probe_proposal(&self.c(), &model, probe_summary, probe_reason)
    }

    /// Accept (the model's use cases become the proposed text) or reject a pending proposal.
    pub fn decide_proposal(&self, id: i64, accept: bool) -> Result<RegistryProposal> {
        let mut c = self.c();
        core_registry::decide_proposal(&mut c, id, accept)
    }

    /// Automatic check after an outcome.
    pub fn check_use_cases(&self, model_id: &str, task_type: &str) -> Option<RegistryProposal> {
        let model = self.models().into_iter().find(|m| m.id == model_id)?;
        core_registry::check_use_cases(&self.c(), &model, task_type)
    }

    // ---- priority rules

    pub fn add_rule(&self, text: &str, rule: &RuleSpec) -> Result<PriorityRule> {
        core_registry::add_rule(&self.c(), text, rule)
    }

    /// Every rule, enabled or not, oldest first.
    pub fn rules(&self) -> Vec<PriorityRule> {
        core_registry::rules(&self.c())
    }

    /// Returns false if there's no such rule.
    pub fn set_rule_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        core_registry::set_rule_enabled(&self.c(), id, enabled)
    }

    pub fn delete_rule(&self, id: i64) -> Result<bool> {
        core_registry::delete_rule(&self.c(), id)
    }

    // ---- routing

    /// Provider id → base_url (without decrypting keys).
    fn provider_urls(&self) -> HashMap<String, Option<String>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, base_url FROM providers").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().filter_map(Result::ok).collect()
    }

    /// Provider ids configured to run through a device.
    fn device_providers(&self) -> HashSet<String> {
        self.providers()
            .unwrap_or_default()
            .into_iter()
            .filter(|provider| provider.via_device().is_some())
            .map(|provider| provider.id)
            .collect()
    }

    /// Whether a model counts as local: flagged, hosted on a device, or its provider's URL is local.
    pub fn model_is_local(&self, m: &ModelEntry) -> bool {
        let urls = self.provider_urls();
        let device_providers = self.device_providers();
        core_registry::model_is_local(
            m,
            urls.get(&m.provider_id).and_then(|u| u.as_deref()),
            device_providers.contains(&m.provider_id),
        )
    }

    /// Choose the model for a thread, or check the one the orchestrator named against
    /// the rules. Errors are meant for the orchestrator to relay.
    pub fn route_model(&self, requested: Option<&str>, task_type: Option<&str>, tags: &[String]) -> Result<Route, String> {
        self.route_model_with(requested, task_type, tags, false)
    }

    /// [`Db::route_model`], with `prefer_cheaper` (the thread has a budget, `limits.rs`):
    /// among models the rules and records leave equal, the cheaper one wins.
    pub fn route_model_with(
        &self,
        requested: Option<&str>,
        task_type: Option<&str>,
        tags: &[String],
        prefer_cheaper: bool,
    ) -> Result<Route, String> {
        self.route_model_available(requested, task_type, tags, prefer_cheaper, &HashSet::new())
    }

    /// Route while excluding unavailable providers from automatic selection.
    /// An explicitly requested model is still returned so the caller can surface
    /// its precise availability error instead of silently substituting a model.
    pub fn route_model_available(
        &self,
        requested: Option<&str>,
        task_type: Option<&str>,
        tags: &[String],
        prefer_cheaper: bool,
        unavailable_providers: &HashSet<String>,
    ) -> Result<Route, String> {
        let models = self.models();
        let urls = self.provider_urls();
        let device_providers = self.device_providers();
        let local = core_registry::local_map(&models, &urls, &device_providers);
        core_registry::route_model(
            &self.c(),
            &models,
            &local,
            requested,
            task_type,
            tags,
            prefer_cheaper,
            unavailable_providers,
        )
    }
}

/// What `list_models` shows the orchestrator: each model with its effective
/// `local` flag, stats per task type, brief probe summary and any pending proposal,
/// plus the enabled rules.
pub fn models_overview(db: &Db) -> Value {
    let models = db.models();
    let urls = db.provider_urls();
    let device_providers = db.device_providers();
    let local = core_registry::local_map(&models, &urls, &device_providers);
    core_registry::models_overview(&db.c(), &models, &local)
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
    let rating = match core_registry::norm(&b.rating).as_str() {
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
    use rusqlite::Connection;

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
            modalities: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn outcome_goes_to_the_free_model_that_answered() {
        let db = Db::open(&tmp()).unwrap();
        provider(&db, "or", None);
        let mut asked = model("asked", "or", 0.0, &["thread"]);
        asked.free = true;
        let mut answered = model("answered", "or", 0.0, &[]);
        answered.free = true;
        db.upsert_model(&asked).unwrap();
        db.upsert_model(&answered).unwrap();
        let t = db.create_thread("c1", "t", "hub", "asked", None, "brief").unwrap();
        // The first call was swapped away after a few tokens; the rest came from "answered".
        db.record_usage(Some("c1"), Some(&t.id), "or", "asked-1", 100, 5, 0, 0);
        db.record_usage(Some("c1"), Some(&t.id), "or", "answered-1", 100, 400, 0, 0);
        assert!(db.finish_thread(&t.id, "done", "s"));
        assert!(db.record_outcome(&t.id));
        assert_eq!(db.outcome_key(&t.id).unwrap().0, "answered");
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
    fn device_hosted_models_are_local_and_skipped_automatically_when_offline() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.add_device("Gaming PC", "windows", false).unwrap();
        provider(&db, "cloud", Some("https://example.com/v1"));
        db.upsert_provider(&ProviderConfig {
            id: "device".into(),
            kind: "scripted".into(),
            base_url: Some("http://127.0.0.1:45123/v1".into()),
            api_key: None,
            options: json!({
                "via_device": "Gaming PC",
                "inline": {"default": [{"type": "text", "text": "OK"}]}
            }),
        })
        .unwrap();
        db.upsert_model(&model("cloud-model", "cloud", 1.0, &["thread"])).unwrap();
        db.upsert_model(&model("local-model", "device", 0.0, &["thread"])).unwrap();
        assert!(db.model_is_local(&db.find_model("local-model").unwrap()));

        db.add_rule(
            "Private work stays local",
            &RuleSpec { when_tag: Some("private".into()), require: Some("local".into()), ..Default::default() },
        )
        .unwrap();
        assert_eq!(db.route_model(None, None, &tags(&["private"])).unwrap().model.id, "local-model");

        let unavailable = HashSet::from(["device".to_string()]);
        assert_eq!(
            db.route_model_available(None, None, &[], false, &unavailable).unwrap().model.id,
            "cloud-model"
        );
        let error = db.route_model_available(None, None, &tags(&["private"]), false, &unavailable).unwrap_err();
        assert!(error.contains("device-hosted model is offline"), "{error}");
        assert_eq!(
            db.route_model_available(Some("local-model"), None, &[], false, &unavailable).unwrap().model.id,
            "local-model",
            "an explicit choice is kept so the provider can report its named host as offline"
        );
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
            .chat(core_protocol::ChatRequest {
                conversation_id: None,
                mode: Some("project".into()),
                text: "please summarise my private notes".into(),
                model: None,
            })
            .await
            .unwrap();
        wait_for(&hub, &conv, |e| replies(e, "Graded and saved.") == 1).await;

        // Routed to the local model by the rule, and the reason is on the thread and its start event.
        let threads = hub.db.threads(Some(&conv));
        assert_eq!(threads.len(), 1);
        let t = &threads[0];
        assert_eq!((t.model.as_str(), t.status.as_str()), ("llama", "done"));
        let reason = t.reason.clone().unwrap();
        assert!(reason.starts_with("No device needed; model llama: rule #1 \"Sensitive data: local only\" (require local)"), "{reason}");
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
            model: None,
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
            model: None,
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
        let old = db.find_model("old").unwrap();
        assert!(!old.local);
        assert!(old.modalities.is_empty());
        drop(db);
        Db::open(&dir).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }
}
