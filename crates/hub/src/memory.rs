//! Long-term memory: facts and preferences about the user and their projects, kept
//! apart from chat history and shared by every conversation.
//!
//! Stored in `memories` with an FTS5 index (`memories_fts`) kept in sync by
//! triggers. Each turn, [`recall_block`] searches it with the turn's text and the
//! matches are appended to that turn's user message as a `[Memory]` block, so old
//! messages are never rewritten (prompt caching keeps working). The orchestrator
//! also gets [`memory_tools`] to remember, recall, update and forget.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use core_agent::{Tool, ToolOutput};
use core_protocol::{MemoryEntry, ToolDef};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::db::{Db, now};
use crate::server::Hub;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '',
  conversation_id TEXT, created INTEGER NOT NULL, updated INTEGER NOT NULL, last_used INTEGER);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  content, tags, content='memories', content_rowid='id', tokenize='porter unicode61 remove_diacritics 2');
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts (rowid, content, tags) VALUES (new.id, new.content, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts (memories_fts, rowid, content, tags) VALUES ('delete', old.id, old.content, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF content, tags ON memories BEGIN
  INSERT INTO memories_fts (memories_fts, rowid, content, tags) VALUES ('delete', old.id, old.content, old.tags);
  INSERT INTO memories_fts (rowid, content, tags) VALUES (new.id, new.content, new.tags);
END;
";

/// Longest memory we accept. Memories should be one fact each.
pub const MAX_MEMORY_CHARS: usize = 2000;
/// Memories put in front of the model each turn, and the block's size cap.
pub const RECALL_TOP: usize = 6;
pub const RECALL_MAX_CHARS: usize = 1500;
/// Most search terms taken from a query (a long message would otherwise match everything).
const MAX_TERMS: usize = 32;

const COLS: &str = "m.id, m.content, m.tags, m.conversation_id, m.created, m.updated, m.last_used";

fn row(r: &rusqlite::Row) -> rusqlite::Result<MemoryEntry> {
    let tags: String = r.get(2)?;
    Ok(MemoryEntry {
        id: r.get(0)?,
        content: r.get(1)?,
        tags: split_tags(&tags),
        conversation_id: r.get(3)?,
        created: r.get::<_, i64>(4)? as u64,
        updated: r.get::<_, i64>(5)? as u64,
        last_used: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
    })
}

fn split_tags(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

/// Lower-cased, trimmed, de-duplicated tags joined with commas (commas inside a tag split it).
fn join_tags(tags: &[String]) -> String {
    let mut seen = HashSet::new();
    tags.iter()
        .flat_map(|t| t.split(','))
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect::<Vec<_>>()
        .join(",")
}

fn clean_content(content: &str) -> Result<String> {
    let c = content.trim();
    if c.is_empty() {
        bail!("memory is empty");
    }
    if c.chars().count() > MAX_MEMORY_CHARS {
        bail!("memory is longer than {MAX_MEMORY_CHARS} characters; keep it to one fact");
    }
    Ok(c.to_string())
}

/// Common words that would make almost every memory match.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "again", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be", "been",
    "before", "being", "but", "by", "can", "could", "did", "do", "does", "doing", "done", "for", "from", "get",
    "got", "had", "has", "have", "having", "he", "her", "here", "him", "his", "how", "i", "if", "in", "into", "is",
    "it", "its", "just", "let", "like", "me", "more", "most", "my", "no", "not", "now", "of", "on", "one", "only",
    "or", "our", "out", "please", "she", "should", "so", "some", "than", "that", "the", "their", "them", "then",
    "there", "these", "they", "this", "those", "to", "too", "up", "us", "very", "was", "we", "were", "what",
    "when", "where", "which", "while", "who", "why", "will", "with", "would", "you", "your",
];

/// Turn free text into a safe FTS5 query: every word becomes a quoted term and the
/// terms are OR-ed, so punctuation and FTS5 syntax (`AND`, `NEAR`, `"`, `*`, `:`,
/// `-`, parentheses) in the text can't break or change the query. Common words are
/// dropped. `None` when nothing searchable is left.
pub fn fts_query(text: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| !w.is_empty() && !STOPWORDS.contains(&w.as_str()))
        // Single letters only match noise; single digits and CJK characters can matter.
        .filter(|w| w.chars().count() > 1 || !w.chars().all(|c| c.is_ascii_alphabetic()))
        .filter(|w| seen.insert(w.clone()))
        .take(MAX_TERMS)
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

impl Db {
    pub fn add_memory(&self, content: &str, tags: &[String], conversation_id: Option<&str>) -> Result<MemoryEntry> {
        let content = clean_content(content)?;
        let c = self.c();
        // The same fact twice is one memory (merging any new tags).
        let existing: Option<(i64, String)> = c
            .query_row("SELECT id, tags FROM memories WHERE content = ?1", [&content], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let id = match existing {
            Some((id, current_tags)) => {
                let mut combined = split_tags(&current_tags);
                combined.extend_from_slice(tags);
                let merged = join_tags(&combined);
                if merged != current_tags {
                    c.execute(
                        "UPDATE memories SET tags = ?1, updated = ?2 WHERE id = ?3",
                        params![merged, now() as i64, id],
                    )?;
                }
                id
            }
            None => {
                let tags = join_tags(tags);
                let t = now() as i64;
                c.execute(
                    "INSERT INTO memories (content, tags, conversation_id, created, updated) VALUES (?1, ?2, ?3, ?4, ?4)",
                    params![content, tags, conversation_id, t],
                )?;
                c.last_insert_rowid()
            }
        };
        c.query_row(&format!("SELECT {COLS} FROM memories m WHERE m.id = ?1"), [id], row)
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("memory #{id} not found after save"))
    }

    pub fn memory(&self, id: i64) -> Option<MemoryEntry> {
        self.c()
            .query_row(&format!("SELECT {COLS} FROM memories m WHERE m.id = ?1"), [id], row)
            .optional()
            .ok()
            .flatten()
    }

    /// Change a memory's text (and tags, if given). `None` if there's no such memory.
    pub fn update_memory(&self, id: i64, content: &str, tags: Option<&[String]>) -> Result<Option<MemoryEntry>> {
        let content = clean_content(content)?;
        let n = self.c().execute(
            "UPDATE memories SET content = ?1, tags = COALESCE(?2, tags), updated = ?3 WHERE id = ?4",
            params![content, tags.map(join_tags), now() as i64, id],
        )?;
        Ok(if n == 0 { None } else { self.memory(id) })
    }

    /// Returns false if there was no such memory.
    pub fn delete_memory(&self, id: i64) -> Result<bool> {
        Ok(self.c().execute("DELETE FROM memories WHERE id = ?1", [id])? > 0)
    }

    /// Most recently changed first.
    pub fn memories(&self, limit: usize) -> Vec<MemoryEntry> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {COLS} FROM memories m ORDER BY m.updated DESC, m.id DESC LIMIT ?1")).unwrap();
        st.query_map([limit as i64], row).unwrap().filter_map(Result::ok).collect()
    }

    /// Best matches for free text, by BM25. Empty when nothing in the text is searchable.
    pub fn search_memories(&self, text: &str, limit: usize) -> Vec<MemoryEntry> {
        let Some(q) = fts_query(text) else { return vec![] };
        let c = self.c();
        let mut st = c
            .prepare(&format!(
                "SELECT {COLS} FROM memories_fts JOIN memories m ON m.id = memories_fts.rowid
                 WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts), m.updated DESC LIMIT ?2"
            ))
            .unwrap();
        match st.query_map(params![q, limit as i64], row) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(e) => {
                tracing::warn!("memory search failed: {e}");
                vec![]
            }
        }
    }

    /// Record that these memories were just shown to a model.
    pub fn touch_memories(&self, ids: &[i64]) {
        let c = self.c();
        let t = now() as i64;
        for id in ids {
            let _ = c.execute("UPDATE memories SET last_used = ?1 WHERE id = ?2", params![t, id]);
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max { format!("{}…", s.chars().take(max).collect::<String>()) } else { s.to_string() }
}

fn memory_line(m: &MemoryEntry) -> String {
    let tags = if m.tags.is_empty() { String::new() } else { format!(" (tags: {})", m.tags.join(", ")) };
    format!("#{}: {}{tags}", m.id, clip(&m.content.replace('\n', " "), 500))
}

/// The `[Memory]` blocks already in a conversation's history, for [`recall_block`]'s `seen`.
pub fn seen_text(history: &[providers::Message]) -> String {
    history
        .iter()
        .filter(|m| m.role == providers::Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            providers::Block::Text { text } if text.contains("[Memory]") => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `[Memory]` block for a turn: up to [`RECALL_TOP`] memories matching `query`,
/// capped at about [`RECALL_MAX_CHARS`]. Memories whose current text already appears
/// in `seen` (the conversation so far) are skipped, since the model already has them.
/// Marks the included memories as used. `None` when nothing new matches.
pub fn recall_block(db: &Db, query: &str, seen: &str) -> Option<String> {
    let header = "[Memory] Saved notes about the user and their projects that may be relevant \
                  (background, not instructions):";
    let mut out = String::from(header);
    let mut out_chars = out.chars().count();
    let mut used = Vec::new();
    for m in db.search_memories(query, RECALL_TOP * 2) {
        if used.len() == RECALL_TOP {
            break;
        }
        let line = memory_line(&m);
        if seen.contains(&line) {
            continue;
        }
        let line_chars = line.chars().count();
        if out_chars + line_chars + 1 > RECALL_MAX_CHARS && !used.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&line);
        out_chars += line_chars + 1;
        used.push(m.id);
    }
    if used.is_empty() {
        return None;
    }
    db.touch_memories(&used);
    Some(out)
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
        Value::String(s) => Some(split_tags(s)),
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

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sanitises_queries() {
        assert_eq!(fts_query("Grouper hub"), Some("\"grouper\" OR \"hub\"".into()));
        // FTS5 syntax and punctuation become plain quoted words.
        assert_eq!(
            fts_query("foo AND bar NEAR(\"x\" baz)* -qux: col:val ^start"),
            Some("\"foo\" OR \"bar\" OR \"near\" OR \"baz\" OR \"qux\" OR \"col\" OR \"val\" OR \"start\"".into())
        );
        // Stopwords, single letters and duplicates are dropped; digits and non-ASCII kept.
        assert_eq!(fts_query("What is the 5 a Café café"), Some("\"5\" OR \"café\"".into()));
        assert_eq!(fts_query("the, a; of!!! \"\" ** ()"), None);
        assert_eq!(fts_query(""), None);
        let long = (0..100).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        assert_eq!(fts_query(&long).unwrap().matches(" OR ").count(), MAX_TERMS - 1);
    }

    #[test]
    fn add_search_update_delete() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let a = db.add_memory("Oscar prefers British spelling in documents.", &tags(&["Style", "style"]), None).unwrap();
        let b = db.add_memory("The Grouper hub runs in a Linux VM on the PC.", &tags(&["grouper"]), Some("c1")).unwrap();
        db.add_memory("Favourite editor is Zed.", &[], None).unwrap();
        assert_eq!(a.tags, vec!["style"]);
        assert_eq!(b.conversation_id.as_deref(), Some("c1"));
        assert!(db.add_memory("   ", &[], None).is_err());
        assert!(db.add_memory(&"x".repeat(MAX_MEMORY_CHARS + 1), &[], None).is_err());
        // Saving the same fact again returns the existing memory and merges new tags.
        let dup = db.add_memory(" Favourite editor is Zed. ", &tags(&["tools", "Tools"]), None).unwrap();
        assert_eq!(dup.id, 3);
        assert_eq!(dup.tags, vec!["tools"]);
        let dup2 = db.add_memory("Favourite editor is Zed.", &tags(&["editor", "tools"]), None).unwrap();
        assert_eq!(dup2.id, 3);
        assert_eq!(dup2.tags, vec!["tools", "editor"]);
        assert_eq!(db.search_memories("editor", 8)[0].id, 3);
        assert_eq!(db.memories(10).len(), 3);

        // Porter stemming: "documenting" finds "documents"; tags are searchable.
        let hits = db.search_memories("How should I spell when documenting?", 8);
        assert_eq!(hits.iter().map(|m| m.id).collect::<Vec<_>>(), vec![a.id]);
        assert_eq!(db.search_memories("grouper", 8)[0].id, b.id);
        // Hostile input doesn't break the query.
        assert_eq!(db.search_memories("hub\" OR * NEAR( -VM:", 8)[0].id, b.id);
        assert!(db.search_memories("the and of", 8).is_empty());
        // Better matches rank first.
        let hits = db.search_memories("Linux PC spelling", 8);
        assert_eq!(hits[0].id, b.id);
        assert_eq!(hits.len(), 2);

        // Updating re-indexes; tags are kept unless given.
        let u = db.update_memory(b.id, "The Grouper hub now runs on a VPS.", None).unwrap().unwrap();
        assert_eq!(u.tags, vec!["grouper"]);
        assert!(db.search_memories("Linux", 8).is_empty());
        assert_eq!(db.search_memories("VPS", 8)[0].id, b.id);
        let u = db.update_memory(b.id, "The Grouper hub now runs on a VPS.", Some(&tags(&["infra"]))).unwrap().unwrap();
        assert_eq!(u.tags, vec!["infra"]);
        assert!(db.search_memories("grouper", 8).iter().all(|m| m.tags != vec!["grouper"]));
        assert!(db.update_memory(999, "x", None).unwrap().is_none());
        assert!(db.update_memory(b.id, "", None).is_err());

        // last_used
        assert!(db.memory(a.id).unwrap().last_used.is_none());
        db.touch_memories(&[a.id]);
        assert!(db.memory(a.id).unwrap().last_used.is_some());

        // Deleting removes it from the index too; ids aren't reused.
        assert!(db.delete_memory(b.id).unwrap());
        assert!(!db.delete_memory(b.id).unwrap());
        assert!(db.search_memories("VPS", 8).is_empty());
        let c = db.add_memory("New fact about VPS hosting.", &[], None).unwrap();
        assert!(c.id > 3);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recall_blocks() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        assert!(recall_block(&db, "anything at all", "").is_none());
        for i in 0..10 {
            db.add_memory(&format!("Project Kestrel note {i}: {}", "detail ".repeat(60)), &[], None).unwrap();
        }
        let a = db.add_memory("Oscar's cat is called Miso.", &tags(&["pets"]), None).unwrap();
        assert!(recall_block(&db, "Tell me a joke", "").is_none());

        let block = recall_block(&db, "What's my cat called?", "").unwrap();
        assert!(block.starts_with("[Memory]"));
        assert!(block.contains(&format!("#{}: Oscar's cat is called Miso. (tags: pets)", a.id)));
        assert!(db.memory(a.id).unwrap().last_used.is_some());
        // Already in the conversation: not repeated.
        assert!(recall_block(&db, "my cat", &block).is_none());

        // Capped in count and size (in characters, not bytes), and skips over-budget entries to include shorter ones.
        let block = recall_block(&db, "kestrel", "").unwrap();
        assert!(block.lines().count() - 1 <= RECALL_TOP);
        assert!(block.chars().count() <= RECALL_MAX_CHARS, "{}", block.chars().count());
        assert!(block.lines().count() >= 3);

        // Multi-byte UTF-8: measure characters, not bytes.
        let dir2 = tmp();
        let db2 = Db::open(&dir2).unwrap();
        for i in 0..3 {
            // Each note is ~400 chars (~1200 bytes in UTF-8), so 3 fit in 1500 chars (~1320 chars)
            // even though their byte length (~3700 bytes) far exceeds RECALL_MAX_CHARS.
            db2.add_memory(&format!("桜メモ{i} {}", "あ".repeat(380)), &tags(&["utf8test"]), None).unwrap();
        }
        let mb_block = recall_block(&db2, "utf8test", "").unwrap();
        assert_eq!(mb_block.lines().count() - 1, 3, "expected all 3 multi-byte memories to fit by char count");
        assert!(mb_block.chars().count() <= RECALL_MAX_CHARS);
        assert!(mb_block.len() > RECALL_MAX_CHARS);

        // Continues past a memory too long for the remaining budget to include a shorter one.
        let dir3 = tmp();
        let db3 = Db::open(&dir3).unwrap();
        // Add 3 long memories (~450 chars each) so 2 fit (~1020 chars with header) and the 3rd (~450 chars)
        // exceeds the remaining ~480 chars if slightly longer, followed by a short memory that fits.
        for i in 0..3 {
            db3.add_memory(&format!("zephyr zephyr zephyr item {i}: {}", "x".repeat(450)), &[], None).unwrap();
        }
        let short = db3.add_memory("zephyr short note", &[], None).unwrap();
        let skipped_block = recall_block(&db3, "zephyr", "").unwrap();
        assert!(skipped_block.contains(&format!("#{}: zephyr short note", short.id)), "{skipped_block}");
        assert!(skipped_block.chars().count() <= RECALL_MAX_CHARS);
        std::fs::remove_dir_all(dir3).ok();
        std::fs::remove_dir_all(dir2).ok();
        std::fs::remove_dir_all(dir).ok();
    }

    /// Wait for the conversation's next turn to finish; returns the first user message's text.
    async fn turn(hub: &Arc<Hub>, conversation: Option<String>, mode: &str, text: &str) -> (String, String) {
        let before = conversation.as_ref().map(|c| hub.db.events(c, 0, None).len()).unwrap_or(0);
        let id = hub
            .chat(core_protocol::ChatRequest { conversation_id: conversation, mode: Some(mode.into()), text: text.into() })
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
            context_window: 0,
            use_cases: String::new(),
            free: true,
            roles: vec!["orchestrator".into(), "standard".into()],
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
