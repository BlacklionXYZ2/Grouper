//! Long-term memory hub glue: `Db` wrappers, `providers::Message` seen-text adapter,
//! and orchestrator tools (`remember`, `recall`, `update_memory`, `forget`).
//!
//! Storage, FTS5 search, query sanitising and `[Memory]` block construction live in
//! [`core_memory::memory`].

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use core_agent::{Tool, ToolOutput};
pub use core_memory::memory::{MAX_MEMORY_CHARS, RECALL_MAX_CHARS, RECALL_TOP, SCHEMA, fts_query};
use core_protocol::{MemoryEntry, ToolDef};
use serde_json::{Value, json};

use crate::db::Db;
use crate::server::Hub;

impl Db {
    pub fn add_memory(&self, content: &str, tags: &[String], conversation_id: Option<&str>) -> Result<MemoryEntry> {
        core_memory::memory::add(&self.c(), content, tags, conversation_id)
    }

    pub fn memory(&self, id: i64) -> Option<MemoryEntry> {
        core_memory::memory::get(&self.c(), id)
    }

    /// Change a memory's text (and tags, if given). `None` if there's no such memory.
    pub fn update_memory(&self, id: i64, content: &str, tags: Option<&[String]>) -> Result<Option<MemoryEntry>> {
        core_memory::memory::update(&self.c(), id, content, tags)
    }

    /// Returns false if there was no such memory.
    pub fn delete_memory(&self, id: i64) -> Result<bool> {
        core_memory::memory::delete(&self.c(), id)
    }

    /// Most recently changed first.
    pub fn memories(&self, limit: usize) -> Vec<MemoryEntry> {
        core_memory::memory::list(&self.c(), limit)
    }

    /// Best matches for free text, by BM25. Empty when nothing in the text is searchable.
    pub fn search_memories(&self, text: &str, limit: usize) -> Vec<MemoryEntry> {
        core_memory::memory::search(&self.c(), text, limit)
    }

    /// Record that these memories were just shown to a model.
    pub fn touch_memories(&self, ids: &[i64]) {
        core_memory::memory::touch(&self.c(), ids);
    }
}

/// The `[Memory]` blocks already in a conversation's history, for [`recall_block`]'s `seen`.
pub fn seen_text(history: &[providers::Message]) -> String {
    core_memory::memory::seen_text(
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

/// The `[Memory]` block for a turn: up to [`RECALL_TOP`] memories matching `query`,
/// capped at about [`RECALL_MAX_CHARS`]. Memories whose current text already appears
/// in `seen` (the conversation so far) are skipped, since the model already has them.
/// Marks the included memories as used. `None` when nothing new matches.
pub fn recall_block(db: &Db, query: &str, seen: &str) -> Option<String> {
    core_memory::memory::recall_block(&db.c(), query, seen)
}

// ---- orchestrator tools

pub fn memory_tools(hub: &Arc<Hub>, conversation_id: &str) -> Vec<Arc<dyn Tool>> {
    let ctx = || MemCtx { hub: hub.clone(), conversation_id: conversation_id.to_string() };
    vec![Arc::new(Remember(ctx())), Arc::new(Recall(ctx())), Arc::new(UpdateMemory(ctx())), Arc::new(Forget(ctx()))]
}

struct MemCtx {
    hub: Arc<Hub>,
    conversation_id: String,
}

/// A memory id given as a number or a numeric string (`3`, `"3"`, `"#3"`).
fn id_arg(input: &Value) -> Option<i64> {
    let v = input.get("id")?;
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().trim_start_matches('#').parse().ok()))
}

fn tags_arg(input: &Value) -> Option<Vec<String>> {
    match input.get("tags")? {
        Value::Array(a) => Some(a.iter().filter_map(Value::as_str).map(String::from).collect()),
        Value::String(s) => Some(core_memory::memory::split_tags(s)),
        _ => None,
    }
}

fn content_arg(input: &Value) -> String {
    input.get("content").and_then(Value::as_str).unwrap_or_default().to_string()
}

fn brief(m: &MemoryEntry) -> Value {
    json!({"id": m.id, "content": m.content, "tags": m.tags, "updated": m.updated})
}

struct Remember(MemCtx);
#[async_trait]
impl Tool for Remember {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "remember".into(),
            description: "Save a lasting fact or preference about the user or their projects to long-term memory, \
                          shared by every conversation. One self-contained fact per call. Not for transient task \
                          details or secrets."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "content": {"type": "string", "description": "The fact, written so it makes sense on its own later"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional short keywords, e.g. a project name"}
            }, "required": ["content"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let tags = tags_arg(&input).unwrap_or_default();
        match self.0.hub.db.add_memory(&content_arg(&input), &tags, Some(&self.0.conversation_id)) {
            Ok(m) => ToolOutput::ok(json!({"saved": true, "id": m.id}).to_string()),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

struct Recall(MemCtx);
#[async_trait]
impl Tool for Recall {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "recall".into(),
            description: "Search long-term memory. Relevant memories already arrive each turn in a [Memory] block; \
                          use this to look for something specific. An empty query lists the most recent memories."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "Words to search for"},
                "limit": {"type": "integer", "description": "Most results to return (default 8)"}
            }, "required": ["query"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let query = input.get("query").and_then(Value::as_str).unwrap_or_default();
        let limit = input.get("limit").and_then(Value::as_u64).unwrap_or(8).clamp(1, 50) as usize;
        let db = &self.0.hub.db;
        let found = if query.trim().is_empty() { db.memories(limit) } else { db.search_memories(query, limit) };
        db.touch_memories(&found.iter().map(|m| m.id).collect::<Vec<_>>());
        if found.is_empty() {
            return ToolOutput::ok("No memories match.");
        }
        ToolOutput::ok(serde_json::to_string_pretty(&found.iter().map(brief).collect::<Vec<_>>()).unwrap_or_default())
    }
}

struct UpdateMemory(MemCtx);
#[async_trait]
impl Tool for UpdateMemory {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "update_memory".into(),
            description: "Replace the text of a memory that's wrong or out of date (ids are shown as #id).".into(),
            input_schema: json!({"type": "object", "properties": {
                "id": {"type": "integer"},
                "content": {"type": "string", "description": "The full new text"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional; replaces the tags"}
            }, "required": ["id", "content"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let Some(id) = id_arg(&input) else { return ToolOutput::err("id must be a memory id number") };
        let tags = tags_arg(&input);
        match self.0.hub.db.update_memory(id, &content_arg(&input), tags.as_deref()) {
            Ok(Some(m)) => ToolOutput::ok(json!({"updated": true, "id": m.id}).to_string()),
            Ok(None) => ToolOutput::err(format!("no memory #{id}")),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

struct Forget(MemCtx);
#[async_trait]
impl Tool for Forget {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "forget".into(),
            description: "Delete a memory (ids are shown as #id), e.g. when the user asks you to forget it.".into(),
            input_schema: json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let Some(id) = id_arg(&input) else { return ToolOutput::err("id must be a memory id number") };
        match self.0.hub.db.delete_memory(id) {
            Ok(true) => ToolOutput::ok(format!("forgot #{id}")),
            Ok(false) => ToolOutput::err(format!("no memory #{id}")),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-memory-test-{}", crate::db::random_hex(4)))
    }

    /// Wait for the conversation's next turn to finish; returns the first user message's text.
    async fn turn(hub: &Arc<Hub>, conversation: Option<String>, mode: &str, text: &str) -> (String, String) {
        let before = conversation.as_ref().map(|c| hub.db.events(c, 0, None).len()).unwrap_or(0);
        let id = hub
            .chat(core_protocol::ChatRequest {
                conversation_id: conversation,
                mode: Some(mode.into()),
                text: text.into(),
                model: None,
            })
            .await
            .unwrap();
        for _ in 0..200 {
            let events = hub.db.events(&id, 0, None);
            if events[before..].iter().any(|e| matches!(e.body, core_protocol::EventBody::TurnFinished { .. })) {
                let history = hub.db.history(&id);
                // The last user message with text (tool results are user messages too).
                let text = history
                    .iter()
                    .rev()
                    .filter(|m| m.role == providers::Role::User)
                    .map(|m| {
                        m.content
                            .iter()
                            .filter_map(|b| if let providers::Block::Text { text } = b { Some(text.clone()) } else { None })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .find(|t| !t.is_empty())
                    .unwrap();
                return (id, text);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("turn didn't finish");
    }

    #[tokio::test]
    async fn turns_recall_and_the_orchestrator_remembers() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&core_protocol::ProviderConfig {
            id: "fake".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {
                "rules": [
                    {"system": "orchestrator of Grouper", "last": "remember that", "reply": [
                        {"type": "tool_use", "name": "remember",
                         "input": {"content": "Oscar's favourite colour is green.", "tags": ["prefs"]}}]},
                    {"system": "orchestrator of Grouper", "last": "\"saved\"", "reply": [{"type": "text", "text": "Saved."}]}
                ],
                "default": [{"type": "text", "text": "OK"}]
            }}),
        })
        .unwrap();
        db.upsert_model(&core_protocol::ModelEntry {
            id: "fake".into(),
            provider_id: "fake".into(),
            model: "fake-1".into(),
            input_price: 0.0,
            output_price: 0.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: true,
            roles: vec!["orchestrator".into(), "standard".into()],
            local: false,
            modalities: vec![],
            ..Default::default()
        })
        .unwrap();
        let cat = db.add_memory("Oscar's cat is called Miso.", &[], None).unwrap();
        let hub = Hub::new(db, dir.clone());

        // Project chat: the matching memory is in this turn's user message; the
        // orchestrator's remember call saves a new one.
        let (project, text) = turn(&hub, None, "project", "Please remember that I like green. What's my cat called?").await;
        assert!(text.contains("[Memory]"), "{text}");
        assert!(text.contains(&format!("#{}: Oscar's cat is called Miso.", cat.id)), "{text}");
        assert!(text.contains("[Status]"));
        let saved = hub.db.search_memories("favourite colour", 8);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].conversation_id.as_deref(), Some(project.as_str()));
        assert!(hub.db.memory(cat.id).unwrap().last_used.is_some());

        // Standard chat shares the memory; no block when nothing matches, and a memory
        // already in the conversation isn't repeated.
        let (standard, text) = turn(&hub, None, "standard", "Which colour is my favourite?").await;
        assert!(text.contains(&format!("#{}: Oscar's favourite colour is green. (tags: prefs)", saved[0].id)), "{text}");
        let (_, text) = turn(&hub, Some(standard.clone()), "standard", "Tell me a joke").await;
        assert!(!text.contains("[Memory]"), "{text}");
        let (_, text) = turn(&hub, Some(standard), "standard", "Green is my favourite colour, right?").await;
        assert!(!text.contains("[Memory]"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }
}
