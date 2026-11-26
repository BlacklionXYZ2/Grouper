//! The cache file: SQLite, versioned with `PRAGMA user_version`.
//!
//! Events are kept exactly as the hub sent them (JSON text), keyed by the hub's
//! `seq`, which is unique across the whole hub. Inserting is `INSERT OR IGNORE`, so
//! catching up twice, or getting the same event from HTTP and the socket, stores it
//! once. Unknown event kinds from a newer hub are stored too and skipped when folding.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use core_protocol::{ConversationInfo, Event, EventBody, MemoryEntry, ThreadInfo};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::Value;

use crate::fold::{self, ActivityItem, MessagePreview, PendingApproval, TranscriptItem};
use crate::{Result, SyncError, now};

/// Migrations, in order. Entry `i` takes the file from version `i` to `i + 1`.
/// Never edit a shipped entry; append a new one.
const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE conversations (
       id TEXT PRIMARY KEY, mode TEXT NOT NULL, title TEXT NOT NULL, created INTEGER NOT NULL, model TEXT,
       read_upto INTEGER NOT NULL DEFAULT 0);
     CREATE TABLE threads (
       id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, title TEXT NOT NULL, device_id TEXT NOT NULL,
       model TEXT NOT NULL, status TEXT NOT NULL, reason TEXT, summary TEXT, created INTEGER NOT NULL,
       finished INTEGER);
     CREATE INDEX threads_conv ON threads (conversation_id, created);
     CREATE TABLE events (
       seq INTEGER PRIMARY KEY, conversation_id TEXT NOT NULL, thread_id TEXT, ts INTEGER NOT NULL,
       kind TEXT NOT NULL, body TEXT NOT NULL);
     CREATE INDEX events_conv ON events (conversation_id, seq);
     CREATE INDEX events_thread ON events (thread_id, seq);
     CREATE VIRTUAL TABLE events_fts USING fts5(text, tokenize='porter unicode61 remove_diacritics 2');
     CREATE TABLE memories (
       id INTEGER PRIMARY KEY, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '', conversation_id TEXT,
       created INTEGER NOT NULL, updated INTEGER NOT NULL, last_used INTEGER);
     CREATE TABLE sync_state (
       conversation_id TEXT PRIMARY KEY, last_seq INTEGER NOT NULL DEFAULT 0, synced_at INTEGER);",
];

/// The schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

const META_HUB: &str = "hub";
const META_HUB_ID: &str = "hub_id";
const META_FULL_REFRESH: &str = "last_full_refresh";
const META_UPDATE: &str = "last_update";
const META_MEMORIES: &str = "memories";

/// One conversation for the list screen.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConversationSummary {
    pub info: ConversationInfo,
    /// Highest stored `seq` in this conversation.
    pub last_seq: i64,
    /// Time of its newest stored event (its creation time when it has none).
    pub last_activity: u64,
    pub preview: Option<MessagePreview>,
    /// The local read marker ([`Store::mark_read`]).
    pub read_upto: i64,
    /// Assistant messages after `read_upto`.
    pub unread: u32,
    pub pending_approvals: u32,
    /// Threads `running` or `waiting_device`.
    pub active_threads: u32,
}

/// A full-text match in cached message text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchHit {
    pub seq: i64,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    /// The event kind, e.g. `assistant_message`.
    pub kind: String,
    pub ts: u64,
    /// The matching text with matches in `[brackets]`.
    pub snippet: String,
}

/// An event as received, before folding: the hub's JSON plus the fields the cache indexes.
#[derive(Clone, Debug)]
pub(crate) struct RawEvent {
    pub seq: i64,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub ts: u64,
    pub kind: String,
    pub json: String,
    /// `None` for a kind this build doesn't know.
    pub event: Option<Event>,
}

impl RawEvent {
    pub(crate) fn parse(v: Value) -> Result<Self> {
        let seq = v.get("seq").and_then(Value::as_i64).ok_or_else(|| SyncError::Protocol("event without seq".into()))?;
        let conversation_id = v
            .get("conversation_id")
            .and_then(Value::as_str)
            .ok_or_else(|| SyncError::Protocol("event without conversation_id".into()))?
            .to_string();
        let thread_id = v.get("thread_id").and_then(Value::as_str).map(String::from);
        let ts = v.get("ts").and_then(Value::as_u64).unwrap_or(0);
        let kind = v.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
        let event = serde_json::from_value::<Event>(v.clone()).ok();
        Ok(Self { seq, conversation_id, thread_id, ts, kind, json: v.to_string(), event })
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl Store {
    /// Open (creating it and its folder if needed) and migrate the cache at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| SyncError::Cache(format!("creating {}: {e}", dir.display())))?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        Self::from_connection(conn, Some(path.to_path_buf()))
    }

    /// Open the cache only if the file exists (for reading offline without creating one).
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Option<Self>> {
        if path.as_ref().is_file() { Self::open(path).map(Some) } else { Ok(None) }
    }

    /// A throwaway cache, for tests and previews.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, None)
    }

    fn from_connection(mut conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn), path })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn c(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.c().pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    // ---- writes (only from the hub, through SyncClient)

    pub(crate) fn upsert_conversations(&self, list: &[ConversationInfo]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for conv in list {
            tx.execute(
                "INSERT INTO conversations (id, mode, title, created, model) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET mode = excluded.mode, title = excluded.title,
                   created = excluded.created, model = excluded.model",
                params![conv.id, conv.mode, conv.title, conv.created as i64, conv.model],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn upsert_threads(&self, list: &[ThreadInfo]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for t in list {
            tx.execute(
                "INSERT INTO threads (id, conversation_id, title, device_id, model, status, reason, summary, created, finished)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, title = excluded.title,
                   device_id = excluded.device_id, model = excluded.model, status = excluded.status,
                   reason = excluded.reason, summary = excluded.summary, created = excluded.created,
                   finished = excluded.finished",
                params![
                    t.id,
                    t.conversation_id,
                    t.title,
                    t.device_id,
                    t.model,
                    t.status,
                    t.reason,
                    t.summary,
                    t.created as i64,
                    t.finished.map(|f| f as i64)
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn replace_memories(&self, list: &[MemoryEntry]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM memories", [])?;
        for m in list {
            tx.execute(
                "INSERT OR REPLACE INTO memories (id, content, tags, conversation_id, created, updated, last_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    m.id,
                    m.content,
                    m.tags.join(","),
                    m.conversation_id,
                    m.created as i64,
                    m.updated as i64,
                    m.last_used.map(|v| v as i64)
                ],
            )?;
        }
        set_meta_tx(&tx, META_MEMORIES, "synced")?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn note_memories_forbidden(&self) -> Result<()> {
        self.set_meta(META_MEMORIES, "forbidden")
    }

    /// Append persisted events (any order; `seq` <= 0 is skipped). Returns how many
    /// were new. Each conversation's sync marker moves to its highest stored seq;
    /// callers only pass events when every earlier event of the conversation is
    /// stored or in the same batch (see `SyncClient`).
    pub(crate) fn append_events(&self, events: &[RawEvent]) -> Result<usize> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let mut added = 0;
        let t = now() as i64;
        for e in events.iter().filter(|e| e.seq > 0) {
            tx.execute(
                "INSERT OR IGNORE INTO conversations (id, mode, title, created) VALUES (?1, '', '', ?2)",
                params![e.conversation_id, e.ts as i64],
            )?;
            let n = tx.execute(
                "INSERT OR IGNORE INTO events (seq, conversation_id, thread_id, ts, kind, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![e.seq, e.conversation_id, e.thread_id, e.ts as i64, e.kind, e.json],
            )?;
            if n == 1 {
                added += 1;
                if let Some(ev) = &e.event {
                    if let Some(text) = fold::searchable_text(&ev.body) {
                        tx.execute("INSERT INTO events_fts (rowid, text) VALUES (?1, ?2)", params![e.seq, text])?;
                    }
                    apply_thread_effects(&tx, ev)?;
                }
            }
            tx.execute(
                "INSERT INTO sync_state (conversation_id, last_seq, synced_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(conversation_id) DO UPDATE SET last_seq = max(last_seq, excluded.last_seq),
                   synced_at = excluded.synced_at",
                params![e.conversation_id, e.seq, t],
            )?;
        }
        set_meta_tx(&tx, META_UPDATE, &t.to_string())?;
        tx.commit()?;
        Ok(added)
    }

    pub(crate) fn note_full_refresh(&self, hub: &str) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let t = now().to_string();
        set_meta_tx(&tx, META_HUB, hub)?;
        set_meta_tx(&tx, META_FULL_REFRESH, &t)?;
        set_meta_tx(&tx, META_UPDATE, &t)?;
        tx.commit()?;
        Ok(())
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let c = self.c();
        c.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Record the hub's id the first time, and refuse a different one afterwards: one
    /// cache holds one hub's data (`seq` numbers from two hubs would collide).
    pub fn check_hub_id(&self, hub_id: &str) -> Result<()> {
        match self.meta(META_HUB_ID)? {
            None => self.set_meta(META_HUB_ID, hub_id),
            Some(known) if known == hub_id => Ok(()),
            Some(known) => Err(crate::SyncError::Protocol(format!(
                "this cache belongs to a different hub (id {known}, this hub is {hub_id});                  use another cache file (--cache) or delete this one"
            ))),
        }
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.c().query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    // ---- local state

    /// Mark a conversation read up to `upto` (its latest stored event when `None`).
    /// The marker never moves backwards.
    pub fn mark_read(&self, conversation_id: &str, upto: Option<i64>) -> Result<()> {
        let c = self.c();
        let upto = match upto {
            Some(s) => s,
            None => c.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE conversation_id = ?1",
                [conversation_id],
                |r| r.get(0),
            )?,
        };
        c.execute(
            "UPDATE conversations SET read_upto = max(read_upto, ?1) WHERE id = ?2",
            params![upto, conversation_id],
        )?;
        Ok(())
    }

    // ---- sync bookkeeping

    /// Highest stored seq across the cache.
    pub fn max_seq(&self) -> Result<i64> {
        Ok(self.c().query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    /// The conversation's sync marker: every event up to it is stored.
    pub fn last_seq(&self, conversation_id: &str) -> Result<i64> {
        Ok(self
            .c()
            .query_row("SELECT last_seq FROM sync_state WHERE conversation_id = ?1", [conversation_id], |r| r.get(0))
            .optional()?
            .unwrap_or(0))
    }

    pub fn has_conversation(&self, id: &str) -> Result<bool> {
        Ok(self.c().prepare("SELECT 1 FROM conversations WHERE id = ?1 AND mode != ''")?.exists([id])?)
    }

    /// Which conversation the stored event `seq` belongs to.
    pub(crate) fn conversation_of(&self, seq: i64) -> Result<Option<String>> {
        Ok(self.c().query_row("SELECT conversation_id FROM events WHERE seq = ?1", [seq], |r| r.get(0)).optional()?)
    }

    /// The hub URL of the last full sync.
    pub fn hub_url(&self) -> Result<Option<String>> {
        self.meta(META_HUB)
    }

    /// Unix seconds of the last completed `sync_once`.
    pub fn last_full_refresh(&self) -> Result<Option<u64>> {
        Ok(self.meta(META_FULL_REFRESH)?.and_then(|v| v.parse().ok()))
    }

    /// Unix seconds the cache last heard from the hub (a full sync or a stored live event).
    pub fn last_update(&self) -> Result<Option<u64>> {
        Ok(self.meta(META_UPDATE)?.and_then(|v| v.parse().ok()))
    }

    /// Whether memories are cached: `Some(true)` synced, `Some(false)` the token isn't
    /// admin, `None` never tried.
    pub fn memories_synced(&self) -> Result<Option<bool>> {
        Ok(self.meta(META_MEMORIES)?.map(|v| v == "synced"))
    }

    // ---- reads for UIs

    /// Conversations as the hub lists them (newest first).
    pub fn conversations(&self) -> Result<Vec<ConversationInfo>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, mode, title, created, model FROM conversations ORDER BY created DESC, id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(ConversationInfo {
                id: r.get(0)?,
                mode: r.get(1)?,
                title: r.get(2)?,
                created: r.get::<_, i64>(3)? as u64,
                model: r.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The conversation list: latest message, unread count, approvals and active
    /// threads, most recently active first.
    pub fn conversation_summaries(&self) -> Result<Vec<ConversationSummary>> {
        let convs = self.conversations()?;
        let mut out = Vec::with_capacity(convs.len());
        for mut info in convs {
            let (read_upto, last_seq, last_ts, unread, active): (i64, i64, Option<i64>, i64, i64) =
                self.c().query_row(
                    "SELECT c.read_upto,
                       (SELECT COALESCE(MAX(seq), 0) FROM events WHERE conversation_id = c.id),
                       (SELECT MAX(ts) FROM events WHERE conversation_id = c.id),
                       (SELECT COUNT(*) FROM events WHERE conversation_id = c.id AND thread_id IS NULL
                          AND kind = 'assistant_message' AND seq > c.read_upto),
                       (SELECT COUNT(*) FROM threads t WHERE t.conversation_id = c.id
                          AND t.status IN ('running', 'waiting_device'))
                     FROM conversations c WHERE c.id = ?1",
                    [&info.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?;
            let preview = self.preview(&info.id)?;
            if info.title.is_empty() {
                info.title = self.first_user_text(&info.id)?.map(|t| t.chars().take(60).collect()).unwrap_or_default();
            }
            let pending = self.pending_approvals(Some(&info.id))?.len() as u32;
            out.push(ConversationSummary {
                last_activity: last_ts.map(|t| t as u64).unwrap_or(info.created).max(info.created),
                info,
                last_seq,
                preview,
                read_upto,
                unread: unread as u32,
                pending_approvals: pending,
                active_threads: active as u32,
            });
        }
        out.sort_by(|a, b| b.last_activity.cmp(&a.last_activity).then(b.last_seq.cmp(&a.last_seq)));
        Ok(out)
    }

    fn preview(&self, conversation_id: &str) -> Result<Option<MessagePreview>> {
        let row: Option<String> = self
            .c()
            .query_row(
                "SELECT body FROM events WHERE conversation_id = ?1 AND thread_id IS NULL
                   AND kind IN ('user_message', 'assistant_message') ORDER BY seq DESC LIMIT 1",
                [conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.and_then(|b| serde_json::from_str::<Event>(&b).ok()).and_then(|e| {
            let (role, text) = match e.body {
                EventBody::UserMessage { text } => ("user", text),
                EventBody::AssistantMessage { text } => ("assistant", text),
                _ => return None,
            };
            let text = if text.chars().count() > 200 {
                format!("{}…", text.chars().take(200).collect::<String>())
            } else {
                text
            };
            Some(MessagePreview { role: role.into(), text, seq: e.seq, ts: e.ts })
        }))
    }

    fn first_user_text(&self, conversation_id: &str) -> Result<Option<String>> {
        let row: Option<String> = self
            .c()
            .query_row(
                "SELECT body FROM events WHERE conversation_id = ?1 AND kind = 'user_message' ORDER BY seq LIMIT 1",
                [conversation_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.and_then(|b| serde_json::from_str::<Event>(&b).ok()).and_then(|e| match e.body {
            EventBody::UserMessage { text } => Some(text),
            _ => None,
        }))
    }

    fn query_events(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Event>> {
        let c = self.c();
        let mut st = c.prepare(sql)?;
        let bodies = st.query_map(params, |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        // Kinds from a newer hub don't parse; they stay stored for a later build.
        Ok(bodies.iter().filter_map(|b| serde_json::from_str::<Event>(b).ok()).collect())
    }

    /// Stored events of a conversation after `after`, optionally one thread's, in seq order.
    pub fn events(&self, conversation_id: &str, after: i64, thread_id: Option<&str>) -> Result<Vec<Event>> {
        self.query_events(
            "SELECT body FROM events WHERE conversation_id = ?1 AND seq > ?2 AND (?3 IS NULL OR thread_id = ?3)
             ORDER BY seq",
            params![conversation_id, after, thread_id],
        )
    }

    /// Threads, oldest first (one conversation's, or all).
    pub fn threads(&self, conversation_id: Option<&str>) -> Result<Vec<ThreadInfo>> {
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, conversation_id, title, device_id, model, status, reason, summary, created, finished
             FROM threads WHERE ?1 IS NULL OR conversation_id = ?1 ORDER BY created, id",
        )?;
        let rows = st.query_map([conversation_id], thread_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn thread(&self, id: &str) -> Result<Option<ThreadInfo>> {
        Ok(self
            .c()
            .query_row(
                "SELECT id, conversation_id, title, device_id, model, status, reason, summary, created, finished
                 FROM threads WHERE id = ?1",
                [id],
                thread_row,
            )
            .optional()?)
    }

    /// A conversation's messages and thread cards.
    pub fn transcript(&self, conversation_id: &str) -> Result<Vec<TranscriptItem>> {
        let events = self.events(conversation_id, 0, None)?;
        Ok(fold::transcript(&events, &self.threads(Some(conversation_id))?))
    }

    /// One thread's steps for its sheet.
    pub fn thread_activity(&self, thread_id: &str) -> Result<Vec<ActivityItem>> {
        let events = self.query_events("SELECT body FROM events WHERE thread_id = ?1 ORDER BY seq", [thread_id])?;
        Ok(fold::thread_activity(&events))
    }

    /// Approvals not yet answered (one conversation's, or all).
    pub fn pending_approvals(&self, conversation_id: Option<&str>) -> Result<Vec<PendingApproval>> {
        let events = self.query_events(
            "SELECT body FROM events WHERE (?1 IS NULL OR conversation_id = ?1)
               AND kind IN ('approval_requested', 'approval_resolved', 'thread_finished') ORDER BY seq",
            [conversation_id],
        )?;
        Ok(fold::pending_approvals(&events, &self.threads(conversation_id)?))
    }

    /// Full-text search over cached message text (messages, thread text, inputs,
    /// checkpoints and summaries). Every word must match (prefixes count).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(q) = fts_query(query) else { return Ok(vec![]) };
        let c = self.c();
        let mut st = c.prepare(
            "SELECT e.seq, e.conversation_id, e.thread_id, e.kind, e.ts,
                    snippet(events_fts, 0, '[', ']', '…', 16)
             FROM events_fts JOIN events e ON e.seq = events_fts.rowid
             WHERE events_fts MATCH ?1 ORDER BY bm25(events_fts), e.seq DESC LIMIT ?2",
        )?;
        let rows = st.query_map(params![q, limit.max(1) as i64], |r| {
            Ok(SearchHit {
                seq: r.get(0)?,
                conversation_id: r.get(1)?,
                thread_id: r.get(2)?,
                kind: r.get(3)?,
                ts: r.get::<_, i64>(4)? as u64,
                snippet: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Cached memories, most recently changed first; with `query`, those whose content
    /// or tags contain every word.
    pub fn memories(&self, query: Option<&str>, limit: usize) -> Result<Vec<MemoryEntry>> {
        let words: Vec<String> = query.unwrap_or_default().split_whitespace().map(str::to_lowercase).collect();
        let c = self.c();
        let mut st = c.prepare(
            "SELECT id, content, tags, conversation_id, created, updated, last_used FROM memories
             ORDER BY updated DESC, id DESC",
        )?;
        let rows = st.query_map([], |r| {
            let tags: String = r.get(2)?;
            Ok(MemoryEntry {
                id: r.get(0)?,
                content: r.get(1)?,
                tags: tags.split(',').filter(|t| !t.is_empty()).map(String::from).collect(),
                conversation_id: r.get(3)?,
                created: r.get::<_, i64>(4)? as u64,
                updated: r.get::<_, i64>(5)? as u64,
                last_used: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            })
        })?;
        let all: Vec<MemoryEntry> = rows.collect::<rusqlite::Result<_>>()?;
        let matches = |m: &MemoryEntry| {
            let hay = format!("{} {}", m.content, m.tags.join(" ")).to_lowercase();
            words.iter().all(|w| hay.contains(w.as_str()))
        };
        Ok(all.into_iter().filter(matches).take(limit.max(1)).collect())
    }
}

fn thread_row(r: &rusqlite::Row) -> rusqlite::Result<ThreadInfo> {
    Ok(ThreadInfo {
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        title: r.get(2)?,
        device_id: r.get(3)?,
        model: r.get(4)?,
        status: r.get(5)?,
        reason: r.get(6)?,
        summary: r.get(7)?,
        created: r.get::<_, i64>(8)? as u64,
        finished: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        rating: None,
    })
}

fn set_meta_tx(tx: &Transaction, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Keep cached thread rows current between list refreshes.
fn apply_thread_effects(tx: &Transaction, e: &Event) -> Result<()> {
    let Some(t) = e.thread_id.as_deref() else { return Ok(()) };
    match &e.body {
        EventBody::ThreadStarted { title, device_id, model, reason, .. } => {
            tx.execute(
                "INSERT OR IGNORE INTO threads (id, conversation_id, title, device_id, model, status, reason, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7)",
                params![t, e.conversation_id, title, device_id, model, reason, e.ts as i64],
            )?;
        }
        EventBody::ThreadWaiting { .. } => {
            tx.execute(
                "UPDATE threads SET status = 'waiting_device' WHERE id = ?1 AND status IN ('running', 'waiting_device')",
                [t],
            )?;
        }
        EventBody::ThreadHandedOver { device_id, .. } => {
            tx.execute(
                "UPDATE threads SET status = 'running', device_id = ?2
                 WHERE id = ?1 AND status IN ('running', 'waiting_device')",
                params![t, device_id],
            )?;
        }
        EventBody::ThreadFinished { status, summary } => {
            tx.execute(
                "UPDATE threads SET status = ?2, summary = ?3, finished = ?4 WHERE id = ?1",
                params![t, status, summary, e.ts as i64],
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(SyncError::Cache(format!(
            "this cache was written by a newer Grouper (schema v{version}, this build knows v{SCHEMA_VERSION}); \
             delete it or use another cache file"
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// Words as quoted FTS5 prefix terms, all required: `"tea"* "pot"*`.
fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(16)
        .map(|w| format!("\"{}\"*", w.to_lowercase()))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_cache_one_hub() {
        let s = Store::open_in_memory().unwrap();
        s.check_hub_id("aaa").unwrap();
        s.check_hub_id("aaa").unwrap();
        let err = s.check_hub_id("bbb").unwrap_err().to_string();
        assert!(err.contains("different hub"), "{err}");
    }

    pub(crate) fn raw(seq: i64, conv: &str, thread: Option<&str>, body: Value) -> RawEvent {
        let mut v = json!({"seq": seq, "conversation_id": conv, "thread_id": thread, "ts": 1000 + seq});
        v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        RawEvent::parse(v).unwrap()
    }

    fn user(seq: i64, conv: &str, text: &str) -> RawEvent {
        raw(seq, conv, None, json!({"kind": "user_message", "text": text}))
    }

    fn assistant(seq: i64, conv: &str, text: &str) -> RawEvent {
        raw(seq, conv, None, json!({"kind": "assistant_message", "text": text}))
    }

    fn conv(id: &str, created: u64) -> ConversationInfo {
        ConversationInfo { id: id.into(), mode: "project".into(), title: format!("{id} title"), created, model: None }
    }

    #[test]
    fn migrations_run_once_and_refuse_newer_files() {
        let dir = std::env::temp_dir().join(format!("grouper-sync-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("cache.db");
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
            s.upsert_conversations(&[conv("c1", 1)]).unwrap();
        }
        // Reopening is a no-op and keeps data.
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
            assert_eq!(s.conversations().unwrap().len(), 1);
        }
        // A file from a newer build is refused, not clobbered.
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        }
        let err = Store::open(&path).err().expect("newer schema refused");
        assert!(err.to_string().contains("newer Grouper"), "{err}");
        assert!(Store::open_existing(dir.join("missing.db")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrating_an_empty_v0_file_creates_every_table() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        for table in ["meta", "conversations", "threads", "events", "events_fts", "memories", "sync_state"] {
            assert!(c.prepare("SELECT 1 FROM sqlite_master WHERE name = ?1").unwrap().exists([table]).unwrap(), "{table}");
        }
        migrate(&mut c).unwrap();
    }

    #[test]
    fn appending_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        let batch = vec![user(1, "c1", "hello tea"), assistant(2, "c1", "Hi"), user(5, "c2", "other")];
        assert_eq!(s.append_events(&batch).unwrap(), 3);
        assert_eq!(s.append_events(&batch).unwrap(), 0);
        // Overlapping batch (HTTP catch-up and the socket both carry seq 2).
        assert_eq!(s.append_events(&[assistant(2, "c1", "Hi"), assistant(3, "c1", "More")]).unwrap(), 1);
        assert_eq!(s.events("c1", 0, None).unwrap().len(), 3);
        assert_eq!(s.last_seq("c1").unwrap(), 3);
        assert_eq!(s.last_seq("c2").unwrap(), 5);
        assert_eq!(s.max_seq().unwrap(), 5);
        // Search sees each message once.
        assert_eq!(s.search("tea", 10).unwrap().len(), 1);
        // Live-only events are never stored.
        assert_eq!(s.append_events(&[raw(0, "c1", None, json!({"kind": "assistant_delta", "text": "x"}))]).unwrap(), 0);
        assert_eq!(s.max_seq().unwrap(), 5);
    }

    #[test]
    fn unknown_kinds_are_kept_but_not_folded() {
        let s = Store::open_in_memory().unwrap();
        s.append_events(&[user(1, "c", "hi"), raw(2, "c", None, json!({"kind": "from_the_future", "x": 1}))]).unwrap();
        assert_eq!(s.max_seq().unwrap(), 2);
        assert_eq!(s.events("c", 0, None).unwrap().len(), 1);
        assert_eq!(s.transcript("c").unwrap().len(), 1);
    }

    #[test]
    fn summaries_previews_unread_and_placeholders() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_conversations(&[conv("c1", 10)]).unwrap();
        s.append_events(&[user(1, "c1", "first"), assistant(2, "c1", "one"), assistant(3, "c1", "two")]).unwrap();
        // c2 is only known from an event (live, before the list refreshed).
        s.append_events(&[user(4, "c2", "a brand new chat about kettles")]).unwrap();
        let list = s.conversation_summaries().unwrap();
        assert_eq!(list.iter().map(|c| c.info.id.as_str()).collect::<Vec<_>>(), ["c2", "c1"]);
        assert_eq!(list[0].info.title, "a brand new chat about kettles");
        assert!(!s.has_conversation("c2").unwrap(), "placeholder rows don't count as known");
        let c1 = &list[1];
        assert_eq!(c1.unread, 2);
        assert_eq!(c1.preview.as_ref().map(|p| (p.role.as_str(), p.text.as_str())), Some(("assistant", "two")));
        s.mark_read("c1", Some(2)).unwrap();
        assert_eq!(s.conversation_summaries().unwrap()[1].unread, 1);
        s.mark_read("c1", Some(1)).unwrap();
        assert_eq!(s.conversation_summaries().unwrap()[1].unread, 1, "marker doesn't move back");
        s.mark_read("c1", None).unwrap();
        assert_eq!(s.conversation_summaries().unwrap()[1].unread, 0);
        // The list refresh fills in the placeholder.
        s.upsert_conversations(&[conv("c2", 20)]).unwrap();
        assert!(s.has_conversation("c2").unwrap());
        assert_eq!(s.conversation_summaries().unwrap()[0].info.title, "c2 title");
    }

    #[test]
    fn thread_rows_follow_events_and_list_refreshes() {
        let s = Store::open_in_memory().unwrap();
        let start = raw(
            1,
            "c",
            Some("t1"),
            json!({"kind": "thread_started", "title": "Build", "device_id": "pc", "model": "m", "reason": null, "brief": "b"}),
        );
        s.append_events(&[start]).unwrap();
        assert_eq!(s.thread("t1").unwrap().unwrap().status, "running");
        s.append_events(&[raw(2, "c", Some("t1"), json!({"kind": "thread_waiting", "device_id": "pc", "reason": "off"}))])
            .unwrap();
        assert_eq!(s.conversation_summaries().unwrap()[0].active_threads, 1);
        assert_eq!(s.thread("t1").unwrap().unwrap().status, "waiting_device");
        let approval = raw(
            3,
            "c",
            Some("t1"),
            json!({"kind": "approval_requested", "approval_id": "a1", "device_id": "pc", "tool": "run_shell",
                   "input": {"command": "ls"}, "reason": "shell"}),
        );
        s.append_events(&[approval]).unwrap();
        assert_eq!(s.pending_approvals(None).unwrap().len(), 1);
        assert_eq!(s.conversation_summaries().unwrap()[0].pending_approvals, 1);
        s.append_events(&[raw(4, "c", Some("t1"), json!({"kind": "thread_finished", "status": "done", "summary": "ok"}))])
            .unwrap();
        let t = s.thread("t1").unwrap().unwrap();
        assert_eq!((t.status.as_str(), t.summary.as_deref(), t.finished), ("done", Some("ok"), Some(1004)));
        assert!(s.pending_approvals(Some("c")).unwrap().is_empty());
        assert_eq!(s.thread_activity("t1").unwrap().len(), 4);
        assert_eq!(s.search("ok", 5).unwrap()[0].kind, "thread_finished");
    }

    #[test]
    fn memories_replace_and_filter() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.memories_synced().unwrap(), None);
        let m = |id: i64, content: &str, tags: &[&str], updated: u64| MemoryEntry {
            id,
            content: content.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            conversation_id: None,
            created: 1,
            updated,
            last_used: None,
        };
        s.replace_memories(&[m(1, "Likes green tea", &["food"], 5), m(2, "Uses a 100% wool rug", &[], 9)]).unwrap();
        assert_eq!(s.memories_synced().unwrap(), Some(true));
        assert_eq!(s.memories(None, 10).unwrap().iter().map(|m| m.id).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(s.memories(Some("TEA food"), 10).unwrap().len(), 1);
        assert_eq!(s.memories(Some("100%"), 10).unwrap().len(), 1);
        s.replace_memories(&[m(3, "New", &[], 1)]).unwrap();
        assert_eq!(s.memories(None, 10).unwrap().len(), 1);
        s.note_memories_forbidden().unwrap();
        assert_eq!(s.memories_synced().unwrap(), Some(false));
    }

    #[test]
    fn fts_queries_are_quoted_prefixes() {
        assert_eq!(fts_query("Tea pot!").as_deref(), Some("\"tea\"* \"pot\"*"));
        assert_eq!(fts_query(" \"  "), None);
        let s = Store::open_in_memory().unwrap();
        s.append_events(&[user(1, "c", "The teapot is \"blue\""), user(2, "c", "a red kettle")]).unwrap();
        let hits = s.search("teap", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("[teapot]"), "{}", hits[0].snippet);
        assert!(s.search("OR AND NOT \"", 10).unwrap().is_empty());
    }
}
