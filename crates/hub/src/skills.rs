//! Skills hub glue: `Db` wrappers, `providers::Message` seen-text adapter,
//! orchestrator tools (`save_skill`, `find_skills`, `update_skill`) and HTTP routes.
//!
//! Storage, FTS5 search and `[Skills]` block construction live in [`core_memory::skills`].

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use core_agent::{Tool, ToolOutput};
pub use core_memory::skills::{SCHEMA, SKILLS_TOP};
use core_protocol::{Skill, SkillInput, ToolDef};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::db::Db;
use crate::registry::deny_unless_admin;
use crate::server::{Hub, err};

impl Db {
    /// Save a skill after a successful task. A skill with the same name (ignoring
    /// case) is updated instead: new description and steps, lists merged, and one
    /// more success. Returns the skill and whether it was new.
    pub fn save_skill(&self, input: &SkillInput, conversation_id: Option<&str>) -> Result<(Skill, bool)> {
        core_memory::skills::save(&self.c(), input, conversation_id)
    }

    pub fn skill(&self, id: i64) -> Option<Skill> {
        core_memory::skills::get(&self.c(), id)
    }

    /// Change a skill. Given lists replace the old ones, or with `merge_lists` are
    /// added to them; `worked` counts one more success. `None` if there's no such skill.
    pub fn update_skill(&self, id: i64, input: &SkillInput, merge_lists: bool, worked: bool) -> Result<Option<Skill>> {
        core_memory::skills::update(&self.c(), id, input, merge_lists, worked)
    }

    pub fn delete_skill(&self, id: i64) -> Result<bool> {
        core_memory::skills::delete(&self.c(), id)
    }

    /// Most recently changed first.
    pub fn skills(&self, limit: usize) -> Vec<Skill> {
        core_memory::skills::list(&self.c(), limit)
    }

    /// Best matches for free text, by BM25 (then by successes). Empty when nothing
    /// in the text is searchable.
    pub fn search_skills(&self, text: &str, limit: usize) -> Vec<Skill> {
        core_memory::skills::search(&self.c(), text, limit)
    }

    /// Record that these skills were just shown to the orchestrator.
    pub fn touch_skills(&self, ids: &[i64]) {
        core_memory::skills::touch(&self.c(), ids);
    }
}

/// The `[Skills]` blocks already in a conversation's history, for [`skills_block`]'s `seen`.
pub fn seen_text(history: &[providers::Message]) -> String {
    core_memory::skills::seen_text(
        history
            .iter()
            .filter(|m| m.role == providers::Role::User)
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                providers::Block::Text { text } => Some(text.as_str()),
                _ => None,
            }),
    )
}

/// The `[Skills]` block for a project turn: up to [`SKILLS_TOP`] skills matching
/// `query` that aren't already in `seen` (the conversation so far). Marks them
/// used. `None` when nothing new matches.
pub fn skills_block(db: &Db, query: &str, seen: &str) -> Option<String> {
    core_memory::skills::skills_block(&db.c(), query, seen)
}

// ---- orchestrator tools

pub fn skill_tools(hub: &Arc<Hub>, conversation_id: &str) -> Vec<Arc<dyn Tool>> {
    let ctx = || SkillCtx { hub: hub.clone(), conversation_id: conversation_id.to_string() };
    vec![Arc::new(SaveSkill(ctx())), Arc::new(FindSkills(ctx())), Arc::new(UpdateSkill(ctx()))]
}

struct SkillCtx {
    hub: Arc<Hub>,
    conversation_id: String,
}

fn list_arg(input: &Value, key: &str) -> Option<Vec<String>> {
    match input.get(key)? {
        Value::Array(a) => Some(a.iter().filter_map(Value::as_str).map(String::from).collect()),
        Value::String(s) => Some(core_memory::skills::split_list(s)),
        _ => None,
    }
}

fn input_arg(input: &Value) -> SkillInput {
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(String::from);
    SkillInput {
        name: s("name"),
        description: s("description"),
        triggers: list_arg(input, "triggers"),
        steps: s("steps"),
        tools: list_arg(input, "tools"),
        models: list_arg(input, "models"),
        devices: list_arg(input, "devices"),
    }
}

fn skill_schema(required: &[&str], extra: Value) -> Value {
    let list = |d: &str| json!({"type": "array", "items": {"type": "string"}, "description": d});
    let mut props = json!({
        "name": {"type": "string", "description": "Short, reusable name, e.g. \"Publish a blog post\""},
        "description": {"type": "string", "description": "What kind of task it solves, in a sentence"},
        "steps": {"type": "string", "description": "Markdown steps: what each thread did, on which device, with which model"},
        "triggers": list("Keywords for finding it later"),
        "tools": list("Tools the threads used"),
        "models": list("Model ids that handled the steps"),
        "devices": list("Devices the steps ran on"),
    });
    if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
        p.extend(e.clone());
    }
    json!({"type": "object", "properties": props, "required": required})
}

struct SaveSkill(SkillCtx);
#[async_trait]
impl Tool for SaveSkill {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "save_skill".into(),
            description: "After a multi-step task succeeds, save the procedure as a reusable skill: the steps, the tools \
                          used and which models and devices handled each step. Saving under an existing name updates \
                          that skill and counts another success."
                .into(),
            input_schema: skill_schema(&["name", "description", "steps"], json!({})),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        match self.0.hub.db.save_skill(&input_arg(&input), Some(&self.0.conversation_id)) {
            Ok((s, created)) => ToolOutput::ok(
                json!({"skill_id": s.id, "new": created, "success_count": s.success_count}).to_string(),
            ),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

struct FindSkills(SkillCtx);
#[async_trait]
impl Tool for FindSkills {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "find_skills".into(),
            description: "Search saved skills (procedures that worked before) and get their full steps. Matching \
                          skills also arrive in a [Skills] block. An empty query lists the most recent."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "Most results (default 5)"}
            }, "required": ["query"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let query = input.get("query").and_then(Value::as_str).unwrap_or_default();
        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(5).clamp(1, 20) as usize;
        let db = &self.0.hub.db;
        let found = if query.trim().is_empty() { db.skills(limit) } else { db.search_skills(query, limit) };
        if found.is_empty() {
            return ToolOutput::ok("No skills match.");
        }
        db.touch_skills(&found.iter().map(|s| s.id).collect::<Vec<_>>());
        ToolOutput::ok(serde_json::to_string_pretty(&found).unwrap_or_default())
    }
}

struct UpdateSkill(SkillCtx);
#[async_trait]
impl Tool for UpdateSkill {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "update_skill".into(),
            description: "Update a saved skill (ids are shown as Skill #id): after reusing it, set worked to true and \
                          add the models and devices used; fix steps that changed. Lists given are added to the \
                          skill's lists."
                .into(),
            input_schema: skill_schema(
                &["id"],
                json!({
                    "id": {"type": "integer"},
                    "worked": {"type": "boolean", "description": "It was just reused successfully"}
                }),
            ),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let id = input.get("id").and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().trim_start_matches('#').parse().ok()));
        let Some(id) = id else { return ToolOutput::err("id must be a skill id number") };
        let worked = input.get("worked").and_then(Value::as_bool).unwrap_or(false);
        match self.0.hub.db.update_skill(id, &input_arg(&input), true, worked) {
            Ok(Some(s)) => ToolOutput::ok(json!({"updated": s.id, "success_count": s.success_count}).to_string()),
            Ok(None) => ToolOutput::err(format!("no skill #{id}")),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

// ---- HTTP API (admin only, reads included, like memory)

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/skills", get(list))
        .route("/v1/skills/{id}", get(show).put(put).delete(delete))
}

#[derive(Deserialize)]
struct SkillQuery {
    q: Option<String>,
    limit: Option<usize>,
}

/// `GET /v1/skills?q=&limit=`: search, or the most recently changed without `q`.
async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<SkillQuery>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let list = match q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => hub.db.search_skills(text, limit),
        None => hub.db.skills(limit),
    };
    Json(list).into_response()
}

async fn show(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.skill(id) {
        Some(s) => Json(s).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("no skill #{id}")),
    }
}

async fn put(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<i64>, Json(b): Json<SkillInput>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.update_skill(id, &b, false, false) {
        Ok(Some(s)) => Json(s).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, format!("no skill #{id}")),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn delete(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.delete_skill(id) {
        Ok(true) => Json(json!({"deleted": id})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, format!("no skill #{id}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
