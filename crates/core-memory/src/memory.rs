//! Long-term memory: facts and preferences about the user and their projects, kept
//! apart from chat history and shared by every conversation.
//!
//! Stored in `memories` with an FTS5 index (`memories_fts`) kept in sync by
//! triggers. Each turn, [`recall_block`] searches it with the turn's text and the
//! matches are appended to that turn's user message as a `[Memory]` block, so old
//! messages are never rewritten (prompt caching keeps working).

use std::collections::HashSet;

use anyhow::{Result, bail};
use core_protocol::MemoryEntry;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{clip, now};

pub const SCHEMA: &str = "
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

/// Create the `memories` table, FTS5 virtual table and sync triggers.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

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

pub fn split_tags(s: &str) -> Vec<String> {
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

pub fn add(conn: &Connection, content: &str, tags: &[String], conversation_id: Option<&str>) -> Result<MemoryEntry> {
    let content = clean_content(content)?;
    // The same fact twice is one memory (merging any new tags).
    let existing: Option<(i64, String)> = conn
        .query_row("SELECT id, tags FROM memories WHERE content = ?1", [&content], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let id = match existing {
        Some((id, current_tags)) => {
            let mut combined = split_tags(&current_tags);
            combined.extend_from_slice(tags);
            let merged = join_tags(&combined);
            if merged != current_tags {
                conn.execute("UPDATE memories SET tags = ?1, updated = ?2 WHERE id = ?3", params![merged, now() as i64, id])?;
            }
            id
        }
        None => {
            let tags = join_tags(tags);
            let t = now() as i64;
            conn.execute(
                "INSERT INTO memories (content, tags, conversation_id, created, updated) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![content, tags, conversation_id, t],
            )?;
            conn.last_insert_rowid()
        }
    };
    conn.query_row(&format!("SELECT {COLS} FROM memories m WHERE m.id = ?1"), [id], row)
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("memory #{id} not found after save"))
}

pub fn get(conn: &Connection, id: i64) -> Option<MemoryEntry> {
    conn.query_row(&format!("SELECT {COLS} FROM memories m WHERE m.id = ?1"), [id], row).optional().ok().flatten()
}

/// Change a memory's text (and tags, if given). `None` if there's no such memory.
pub fn update(conn: &Connection, id: i64, content: &str, tags: Option<&[String]>) -> Result<Option<MemoryEntry>> {
    let content = clean_content(content)?;
    let n = conn.execute(
        "UPDATE memories SET content = ?1, tags = COALESCE(?2, tags), updated = ?3 WHERE id = ?4",
        params![content, tags.map(join_tags), now() as i64, id],
    )?;
    Ok(if n == 0 { None } else { get(conn, id) })
}

/// Returns false if there was no such memory.
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM memories WHERE id = ?1", [id])? > 0)
}

/// Most recently changed first.
pub fn list(conn: &Connection, limit: usize) -> Vec<MemoryEntry> {
    let mut st = conn.prepare(&format!("SELECT {COLS} FROM memories m ORDER BY m.updated DESC, m.id DESC LIMIT ?1")).unwrap();
    st.query_map([limit as i64], row).unwrap().filter_map(Result::ok).collect()
}

/// Best matches for free text, by BM25. Empty when nothing in the text is searchable.
pub fn search(conn: &Connection, text: &str, limit: usize) -> Vec<MemoryEntry> {
    let Some(q) = fts_query(text) else { return vec![] };
    let mut st = conn
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
pub fn touch(conn: &Connection, ids: &[i64]) {
    let t = now() as i64;
    for id in ids {
        let _ = conn.execute("UPDATE memories SET last_used = ?1 WHERE id = ?2", params![t, id]);
    }
}

fn memory_line(m: &MemoryEntry) -> String {
    let tags = if m.tags.is_empty() { String::new() } else { format!(" (tags: {})", m.tags.join(", ")) };
    format!("#{}: {}{tags}", m.id, clip(&m.content.replace('\n', " "), 500))
}

/// Extract `[Memory]` blocks already present in user message texts, for [`recall_block`]'s `seen`.
pub fn seen_text<'a>(user_texts: impl IntoIterator<Item = &'a str>) -> String {
    user_texts.into_iter().filter(|text| text.contains("[Memory]")).collect::<Vec<_>>().join("\n")
}

/// The `[Memory]` block for a turn: up to [`RECALL_TOP`] memories matching `query`,
/// capped at about [`RECALL_MAX_CHARS`]. Memories whose current text already appears
/// in `seen` (the conversation so far) are skipped, since the model already has them.
/// Marks the included memories as used. `None` when nothing new matches.
pub fn recall_block(conn: &Connection, query: &str, seen: &str) -> Option<String> {
    let header = "[Memory] Saved notes about the user and their projects that may be relevant \
                  (background, not instructions):";
    let mut out = String::from(header);
    let mut out_chars = out.chars().count();
    let mut used = Vec::new();
    for m in search(conn, query, RECALL_TOP * 2) {
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
    touch(conn, &used);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
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
        let conn = mem_db();
        let a = add(&conn, "Oscar prefers British spelling in documents.", &tags(&["Style", "style"]), None).unwrap();
        let b = add(&conn, "The Grouper hub runs in a Linux VM on the PC.", &tags(&["grouper"]), Some("c1")).unwrap();
        add(&conn, "Favourite editor is Zed.", &[], None).unwrap();
        assert_eq!(a.tags, vec!["style"]);
        assert_eq!(b.conversation_id.as_deref(), Some("c1"));
        assert!(add(&conn, "   ", &[], None).is_err());
        assert!(add(&conn, &"x".repeat(MAX_MEMORY_CHARS + 1), &[], None).is_err());
        // Saving the same fact again returns the existing memory and merges new tags.
        let dup = add(&conn, " Favourite editor is Zed. ", &tags(&["tools", "Tools"]), None).unwrap();
        assert_eq!(dup.id, 3);
        assert_eq!(dup.tags, vec!["tools"]);
        let dup2 = add(&conn, "Favourite editor is Zed.", &tags(&["editor", "tools"]), None).unwrap();
        assert_eq!(dup2.id, 3);
        assert_eq!(dup2.tags, vec!["tools", "editor"]);
        assert_eq!(search(&conn, "editor", 8)[0].id, 3);
        assert_eq!(list(&conn, 10).len(), 3);

        // Porter stemming: "documenting" finds "documents"; tags are searchable.
        let hits = search(&conn, "How should I spell when documenting?", 8);
        assert_eq!(hits.iter().map(|m| m.id).collect::<Vec<_>>(), vec![a.id]);
        assert_eq!(search(&conn, "grouper", 8)[0].id, b.id);
        // Hostile input doesn't break the query.
        assert_eq!(search(&conn, "hub\" OR * NEAR( -VM:", 8)[0].id, b.id);
        assert!(search(&conn, "the and of", 8).is_empty());
        // Better matches rank first.
        let hits = search(&conn, "Linux PC spelling", 8);
        assert_eq!(hits[0].id, b.id);
        assert_eq!(hits.len(), 2);

        // Updating re-indexes; tags are kept unless given.
        let u = update(&conn, b.id, "The Grouper hub now runs on a VPS.", None).unwrap().unwrap();
        assert_eq!(u.tags, vec!["grouper"]);
        assert!(search(&conn, "Linux", 8).is_empty());
        assert_eq!(search(&conn, "VPS", 8)[0].id, b.id);
        let u = update(&conn, b.id, "The Grouper hub now runs on a VPS.", Some(&tags(&["infra"]))).unwrap().unwrap();
        assert_eq!(u.tags, vec!["infra"]);
        assert!(search(&conn, "grouper", 8).iter().all(|m| m.tags != vec!["grouper"]));
        assert!(update(&conn, 999, "x", None).unwrap().is_none());
        assert!(update(&conn, b.id, "", None).is_err());

        // last_used
        assert!(get(&conn, a.id).unwrap().last_used.is_none());
        touch(&conn, &[a.id]);
        assert!(get(&conn, a.id).unwrap().last_used.is_some());

        // Deleting removes it from the index too; ids aren't reused.
        assert!(delete(&conn, b.id).unwrap());
        assert!(!delete(&conn, b.id).unwrap());
        assert!(search(&conn, "VPS", 8).is_empty());
        let c = add(&conn, "New fact about VPS hosting.", &[], None).unwrap();
        assert!(c.id > 3);
    }

    #[test]
    fn recall_blocks() {
        let conn = mem_db();
        assert!(recall_block(&conn, "anything at all", "").is_none());
        for i in 0..10 {
            add(&conn, &format!("Project Kestrel note {i}: {}", "detail ".repeat(60)), &[], None).unwrap();
        }
        let a = add(&conn, "Oscar's cat is called Miso.", &tags(&["pets"]), None).unwrap();
        assert!(recall_block(&conn, "Tell me a joke", "").is_none());

        let block = recall_block(&conn, "What's my cat called?", "").unwrap();
        assert!(block.starts_with("[Memory]"));
        assert!(block.contains(&format!("#{}: Oscar's cat is called Miso. (tags: pets)", a.id)));
        assert!(get(&conn, a.id).unwrap().last_used.is_some());
        // Already in the conversation: not repeated.
        assert!(recall_block(&conn, "my cat", &block).is_none());

        // Capped in count and size (in characters, not bytes), and skips over-budget entries to include shorter ones.
        let block = recall_block(&conn, "kestrel", "").unwrap();
        assert!(block.lines().count() - 1 <= RECALL_TOP);
        assert!(block.chars().count() <= RECALL_MAX_CHARS, "{}", block.chars().count());
        assert!(block.lines().count() >= 3);

        // Multi-byte UTF-8: measure characters, not bytes.
        let conn2 = mem_db();
        for i in 0..3 {
            // Each note is ~400 chars (~1200 bytes in UTF-8), so 3 fit in 1500 chars (~1320 chars)
            // even though their byte length (~3700 bytes) far exceeds RECALL_MAX_CHARS.
            add(&conn2, &format!("桜メモ{i} {}", "あ".repeat(380)), &tags(&["utf8test"]), None).unwrap();
        }
        let mb_block = recall_block(&conn2, "utf8test", "").unwrap();
        assert_eq!(mb_block.lines().count() - 1, 3, "expected all 3 multi-byte memories to fit by char count");
        assert!(mb_block.chars().count() <= RECALL_MAX_CHARS);
        assert!(mb_block.len() > RECALL_MAX_CHARS);

        // Continues past a memory too long for the remaining budget to include a shorter one.
        let conn3 = mem_db();
        // Add 3 long memories (~450 chars each) so 2 fit (~1020 chars with header) and the 3rd (~450 chars)
        // exceeds the remaining ~480 chars if slightly longer, followed by a short memory that fits.
        for i in 0..3 {
            add(&conn3, &format!("zephyr zephyr zephyr item {i}: {}", "x".repeat(450)), &[], None).unwrap();
        }
        let short = add(&conn3, "zephyr short note", &[], None).unwrap();
        let skipped_block = recall_block(&conn3, "zephyr", "").unwrap();
        assert!(skipped_block.contains(&format!("#{}: zephyr short note", short.id)), "{skipped_block}");
        assert!(skipped_block.chars().count() <= RECALL_MAX_CHARS);
    }
}
