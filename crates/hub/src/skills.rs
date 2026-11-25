//! Skills: procedures that worked, saved by the orchestrator after a successful
//! multi-step task (in the style of Hermes): the steps, the tools used, and which
//! models and devices handled them. Similar tasks reuse a skill instead of
//! re-planning.
//!
//! Stored in `skills` with an FTS5 index (`skills_fts`) kept in sync by triggers,
//! like memory. Each project-chat turn, [`skills_block`] searches them with the
//! turn's text and appends the best [`SKILLS_TOP`] as a `[Skills]` block after the
//! `[Memory]` block, skipping skills already shown in the conversation. The
//! orchestrator gets [`skill_tools`]: save, find and update.

use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use core_agent::{Tool, ToolOutput};
use core_protocol::{Skill, SkillInput, ToolDef};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::db::{Db, now};
use crate::memory::fts_query;
use crate::registry::deny_unless_admin;
use crate::server::{Hub, err};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS skills (
  id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, description TEXT NOT NULL,
  triggers TEXT NOT NULL DEFAULT '', steps TEXT NOT NULL, tools TEXT NOT NULL DEFAULT '',
  models TEXT NOT NULL DEFAULT '', devices TEXT NOT NULL DEFAULT '', success_count INTEGER NOT NULL DEFAULT 0,
  conversation_id TEXT, last_used INTEGER, created INTEGER NOT NULL, updated INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS skills_name ON skills (name COLLATE NOCASE);
CREATE VIRTUAL TABLE IF NOT EXISTS skills_fts USING fts5(
  name, description, triggers, steps, content='skills', content_rowid='id',
  tokenize='porter unicode61 remove_diacritics 2');
CREATE TRIGGER IF NOT EXISTS skills_ai AFTER INSERT ON skills BEGIN
  INSERT INTO skills_fts (rowid, name, description, triggers, steps)
  VALUES (new.id, new.name, new.description, new.triggers, new.steps);
END;
CREATE TRIGGER IF NOT EXISTS skills_ad AFTER DELETE ON skills BEGIN
  INSERT INTO skills_fts (skills_fts, rowid, name, description, triggers, steps)
  VALUES ('delete', old.id, old.name, old.description, old.triggers, old.steps);
END;
CREATE TRIGGER IF NOT EXISTS skills_au AFTER UPDATE OF name, description, triggers, steps ON skills BEGIN
  INSERT INTO skills_fts (skills_fts, rowid, name, description, triggers, steps)
  VALUES ('delete', old.id, old.name, old.description, old.triggers, old.steps);
  INSERT INTO skills_fts (rowid, name, description, triggers, steps)
  VALUES (new.id, new.name, new.description, new.triggers, new.steps);
END;
";

const MAX_NAME: usize = 80;
const MAX_DESCRIPTION: usize = 500;
const MAX_STEPS: usize = 8000;
const MAX_LIST: usize = 30;
/// Skills put in front of the orchestrator each project turn, and how much of their steps.
pub const SKILLS_TOP: usize = 2;
const STEPS_IN_BLOCK: usize = 700;

const COLS: &str = "s.id, s.name, s.description, s.triggers, s.steps, s.tools, s.models, s.devices, s.success_count, \
                    s.last_used, s.created, s.updated";

fn split(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

/// Trimmed, de-duplicated (ignoring case) and capped; commas inside an item split it.
fn clean_list(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in items.iter().flat_map(|t| t.split(',')).map(str::trim).filter(|t| !t.is_empty()) {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(t)) && out.len() < MAX_LIST {
            out.push(t.to_string());
        }
    }
    out
}

fn merge(old: &[String], new: &[String]) -> Vec<String> {
    clean_list(&old.iter().chain(new).cloned().collect::<Vec<_>>())
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Skill> {
    Ok(Skill {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        triggers: split(&r.get::<_, String>(3)?),
        steps: r.get(4)?,
        tools: split(&r.get::<_, String>(5)?),
        models: split(&r.get::<_, String>(6)?),
        devices: split(&r.get::<_, String>(7)?),
        success_count: r.get::<_, i64>(8)? as u64,
        last_used: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        created: r.get::<_, i64>(10)? as u64,
        updated: r.get::<_, i64>(11)? as u64,
    })
}

fn text_field(value: &str, what: &str, max: usize) -> Result<String> {
    let v = value.trim();
    if v.is_empty() {
        bail!("{what} is empty");
    }
    if v.chars().count() > max {
        bail!("{what} is longer than {max} characters");
    }
    Ok(v.to_string())
}

impl Db {
    /// Save a skill after a successful task. A skill with the same name (ignoring
    /// case) is updated instead: new description and steps, lists merged, and one
    /// more success. Returns the skill and whether it was new.
    pub fn save_skill(&self, input: &SkillInput, conversation_id: Option<&str>) -> Result<(Skill, bool)> {
        let name = text_field(input.name.as_deref().unwrap_or_default(), "name", MAX_NAME)?;
        let description = text_field(input.description.as_deref().unwrap_or_default(), "description", MAX_DESCRIPTION)?;
        let steps = text_field(input.steps.as_deref().unwrap_or_default(), "steps", MAX_STEPS)?;
        let list = |l: &Option<Vec<String>>| clean_list(l.as_deref().unwrap_or_default());
        let existing = self.skill_by_name(&name);
        let t = now() as i64;
        let id = {
            let c = self.c();
            match &existing {
                Some(s) => {
                    c.execute(
                        "UPDATE skills SET description = ?1, steps = ?2, triggers = ?3, tools = ?4, models = ?5,
                           devices = ?6, success_count = success_count + 1, updated = ?7 WHERE id = ?8",
                        params![
                            description,
                            steps,
                            merge(&s.triggers, &list(&input.triggers)).join(","),
                            merge(&s.tools, &list(&input.tools)).join(","),
                            merge(&s.models, &list(&input.models)).join(","),
                            merge(&s.devices, &list(&input.devices)).join(","),
                            t,
                            s.id
                        ],
                    )?;
                    s.id
                }
                None => {
                    c.execute(
                        "INSERT INTO skills (name, description, triggers, steps, tools, models, devices, success_count,
                           conversation_id, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?9)",
                        params![
                            name,
                            description,
                            list(&input.triggers).join(","),
                            steps,
                            list(&input.tools).join(","),
                            list(&input.models).join(","),
                            list(&input.devices).join(","),
                            conversation_id,
                            t
                        ],
                    )?;
                    c.last_insert_rowid()
                }
            }
        };
        Ok((self.skill(id).expect("just saved"), existing.is_none()))
    }

    pub fn skill(&self, id: i64) -> Option<Skill> {
        self.c().query_row(&format!("SELECT {COLS} FROM skills s WHERE s.id = ?1"), [id], row).optional().ok().flatten()
    }

    fn skill_by_name(&self, name: &str) -> Option<Skill> {
        self.c()
            .query_row(&format!("SELECT {COLS} FROM skills s WHERE s.name = ?1 COLLATE NOCASE"), [name.trim()], row)
            .optional()
            .ok()
            .flatten()
    }

    /// Change a skill. Given lists replace the old ones, or with `merge_lists` are
    /// added to them; `worked` counts one more success. `None` if there's no such skill.
    pub fn update_skill(&self, id: i64, input: &SkillInput, merge_lists: bool, worked: bool) -> Result<Option<Skill>> {
        let Some(s) = self.skill(id) else { return Ok(None) };
        let name = match &input.name {
            Some(n) => {
                let n = text_field(n, "name", MAX_NAME)?;
                if self.skill_by_name(&n).is_some_and(|o| o.id != id) {
                    bail!("another skill is already called {n:?}");
                }
                n
            }
            None => s.name.clone(),
        };
        let description = match &input.description {
            Some(d) => text_field(d, "description", MAX_DESCRIPTION)?,
            None => s.description.clone(),
        };
        let steps = match &input.steps {
            Some(d) => text_field(d, "steps", MAX_STEPS)?,
            None => s.steps.clone(),
        };
        let list = |old: &[String], new: &Option<Vec<String>>| match new {
            Some(n) if merge_lists => merge(old, n),
            Some(n) => clean_list(n),
            None => old.to_vec(),
        };
        self.c().execute(
            "UPDATE skills SET name = ?1, description = ?2, steps = ?3, triggers = ?4, tools = ?5, models = ?6,
               devices = ?7, success_count = success_count + ?8, updated = ?9, last_used = CASE WHEN ?8 > 0 THEN ?9
               ELSE last_used END WHERE id = ?10",
            params![
                name,
                description,
                steps,
                list(&s.triggers, &input.triggers).join(","),
                list(&s.tools, &input.tools).join(","),
                list(&s.models, &input.models).join(","),
                list(&s.devices, &input.devices).join(","),
                worked as i64,
                now() as i64,
                id
            ],
        )?;
        Ok(self.skill(id))
    }

    pub fn delete_skill(&self, id: i64) -> Result<bool> {
        Ok(self.c().execute("DELETE FROM skills WHERE id = ?1", [id])? > 0)
    }

    /// Most recently changed first.
    pub fn skills(&self, limit: usize) -> Vec<Skill> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {COLS} FROM skills s ORDER BY s.updated DESC, s.id DESC LIMIT ?1")).unwrap();
        st.query_map([limit as i64], row).unwrap().filter_map(Result::ok).collect()
    }

    /// Best matches for free text, by BM25 (then by successes). Empty when nothing
    /// in the text is searchable.
    pub fn search_skills(&self, text: &str, limit: usize) -> Vec<Skill> {
        let Some(q) = fts_query(text) else { return vec![] };
        let c = self.c();
        let mut st = c
            .prepare(&format!(
                "SELECT {COLS} FROM skills_fts JOIN skills s ON s.id = skills_fts.rowid
                 WHERE skills_fts MATCH ?1 ORDER BY bm25(skills_fts, 4.0, 2.0, 3.0, 1.0), s.success_count DESC LIMIT ?2"
            ))
            .unwrap();
        match st.query_map(params![q, limit as i64], row) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(e) => {
                tracing::warn!("skill search failed: {e}");
                vec![]
            }
        }
    }

    /// Record that these skills were just shown to the orchestrator.
    pub fn touch_skills(&self, ids: &[i64]) {
        let c = self.c();
        let t = now() as i64;
        for id in ids {
            let _ = c.execute("UPDATE skills SET last_used = ?1 WHERE id = ?2", params![t, id]);
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max { format!("{}…", s.chars().take(max).collect::<String>()) } else { s.to_string() }
}

/// How a skill starts in a `[Skills]` block; used to spot skills already shown.
fn marker(s: &Skill) -> String {
    format!("Skill #{} \"", s.id)
}

fn skill_entry(s: &Skill) -> String {
    let mut used = vec![format!("worked {}×", s.success_count)];
    for (what, list) in [("tools", &s.tools), ("models", &s.models), ("devices", &s.devices)] {
        if !list.is_empty() {
            used.push(format!("{what}: {}", list.join(", ")));
        }
    }
    let steps = clip(&s.steps.split_whitespace().collect::<Vec<_>>().join(" "), STEPS_IN_BLOCK);
    format!("{}{}\" ({}): {}\n  Steps: {steps}", marker(s), s.name, used.join("; "), s.description.replace('\n', " "))
}

/// The `[Skills]` blocks already in a conversation's history, for [`skills_block`]'s `seen`.
pub fn seen_text(history: &[providers::Message]) -> String {
    history
        .iter()
        .filter(|m| m.role == providers::Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            providers::Block::Text { text } if text.contains("[Skills]") => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `[Skills]` block for a project turn: up to [`SKILLS_TOP`] skills matching
/// `query` that aren't already in `seen` (the conversation so far). Marks them
/// used. `None` when nothing new matches.
pub fn skills_block(db: &Db, query: &str, seen: &str) -> Option<String> {
    let found: Vec<Skill> =
        db.search_skills(query, SKILLS_TOP * 3).into_iter().filter(|s| !seen.contains(&marker(s))).take(SKILLS_TOP).collect();
    if found.is_empty() {
        return None;
    }
    db.touch_skills(&found.iter().map(|s| s.id).collect::<Vec<_>>());
    let mut out = String::from(
        "[Skills] Saved procedures that worked for similar tasks (reuse one instead of re-planning if it fits; \
         find_skills has the full steps):",
    );
    for s in &found {
        out.push('\n');
        out.push_str(&skill_entry(s));
    }
    Some(out)
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
        Value::String(s) => Some(split(s)),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-skills-test-{}", crate::db::random_hex(4)))
    }

    fn v(t: &[&str]) -> Option<Vec<String>> {
        Some(t.iter().map(|s| s.to_string()).collect())
    }

    fn input(name: &str, description: &str, steps: &str) -> SkillInput {
        SkillInput {
            name: Some(name.into()),
            description: Some(description.into()),
            steps: Some(steps.into()),
            ..Default::default()
        }
    }

    #[test]
    fn save_search_update_delete() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let mut blog = input("Publish a blog post", "Draft, review and publish a post to the static site.", "1. Draft on hub\n2. Build on mac");
        blog.tools = v(&["write_file", "run_shell", "write_file"]);
        blog.models = v(&["opus"]);
        blog.devices = v(&["hub, mac"]);
        blog.triggers = v(&["blog", "Hugo"]);
        let (a, new) = db.save_skill(&blog, Some("c1")).unwrap();
        assert!(new);
        assert_eq!((a.success_count, a.tools.len(), a.devices.clone()), (1, 2, vec!["hub".to_string(), "mac".into()]));
        let (b, _) = db.save_skill(&input("Rotate API keys", "Replace provider keys everywhere.", "1. keys set"), None).unwrap();
        assert!(db.save_skill(&input("", "d", "s"), None).is_err());
        assert!(db.save_skill(&input("x", "d", ""), None).is_err());

        // Same name (any case) updates it: lists merge, one more success.
        let mut again = input("publish a BLOG post", "Draft and publish a post.", "1. Draft\n2. Publish");
        again.models = v(&["sonnet", "OPUS"]);
        let (a2, new) = db.save_skill(&again, None).unwrap();
        assert!(!new);
        assert_eq!(a2.id, a.id);
        assert_eq!((a2.success_count, a2.models.clone()), (2, vec!["opus".to_string(), "sonnet".into()]));
        assert_eq!(a2.steps, "1. Draft\n2. Publish");

        // Search: stemming, name and trigger weight, hostile input.
        assert_eq!(db.search_skills("publishing posts", 5)[0].id, a.id);
        assert_eq!(db.search_skills("hugo", 5)[0].id, a.id);
        assert_eq!(db.search_skills("rotate \" OR * NEAR( key:", 5)[0].id, b.id);
        assert!(db.search_skills("the and of", 5).is_empty());

        // Update: merge (tool) or replace (API); worked counts a success.
        let u = db.update_skill(a.id, &SkillInput { devices: v(&["pc"]), ..Default::default() }, true, true).unwrap().unwrap();
        assert_eq!((u.success_count, u.devices.len()), (3, 3));
        assert!(u.last_used.is_some());
        let u = db.update_skill(a.id, &SkillInput { devices: v(&["pc"]), ..Default::default() }, false, false).unwrap().unwrap();
        assert_eq!((u.success_count, u.devices.clone()), (3, vec!["pc".to_string()]));
        let u = db.update_skill(a.id, &SkillInput { steps: Some("1. Use Zola".into()), ..Default::default() }, false, false);
        assert!(u.unwrap().is_some());
        assert_eq!(db.search_skills("zola", 5)[0].id, a.id);
        assert!(db.update_skill(a.id, &SkillInput { name: Some("Rotate API keys".into()), ..Default::default() }, false, false).is_err());
        assert!(db.update_skill(999, &SkillInput::default(), false, false).unwrap().is_none());

        assert!(db.delete_skill(b.id).unwrap());
        assert!(!db.delete_skill(b.id).unwrap());
        assert!(db.search_skills("rotate", 5).is_empty());
        assert_eq!(db.skills(10).len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn skills_blocks() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        assert!(skills_block(&db, "publish my post", "").is_none());
        for (n, d) in [
            ("Publish a blog post", "Publish a post to the blog."),
            ("Blog post images", "Resize images for a blog post."),
            ("Blog post tags", "Tag a blog post."),
            ("Back up photos", "Copy photos to the NAS."),
        ] {
            let mut i = input(n, d, &"Step detail. ".repeat(100));
            i.models = v(&["opus"]);
            db.save_skill(&i, None).unwrap();
        }
        assert!(skills_block(&db, "Tell me a joke", "").is_none());
        let block = skills_block(&db, "Publish my blog post please", "").unwrap();
        assert!(block.starts_with("[Skills]"));
        assert_eq!(block.matches("Skill #").count(), SKILLS_TOP);
        assert!(block.contains("Skill #1 \"Publish a blog post\" (worked 1×; models: opus): Publish a post to the blog."), "{block}");
        assert!(block.len() < 2 * STEPS_IN_BLOCK + 600, "{}", block.len());
        assert!(db.skill(1).unwrap().last_used.is_some());
        // Shown already: the next best are offered instead, then nothing.
        let next = skills_block(&db, "blog post", &block).unwrap();
        assert_eq!(next.matches("Skill #").count(), 1);
        assert!(!next.contains("Skill #1 \""));
        assert!(skills_block(&db, "blog post", &format!("{block}\n{next}")).is_none());
        std::fs::remove_dir_all(dir).ok();
    }
}
