//! Hub state in SQLite: devices, tool registry, providers, models, conversations,
//! model history, threads, events and usage.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use core_protocol::{
    ConversationInfo, Event, EventBody, ModelEntry, Permission, ProviderConfig, ThreadInfo, ToolDef, ToolInfo,
};
use providers::Message;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::secrets::Secrets;

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system randomness");
    hex::encode(buf)
}

/// Unix seconds as SQLite stores them.
fn ts() -> i64 {
    now() as i64
}

fn hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub revoked: bool,
    pub last_seen: Option<u64>,
    /// Admin tokens belong to the user's own clients. Only they may change tool
    /// permissions, answer approvals, manage providers and models, chat or steer
    /// threads. Plain device tokens can connect and read.
    pub admin: bool,
}

/// Who a valid token belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Auth {
    pub device_id: String,
    pub admin: bool,
}

pub struct Db {
    connection: Mutex<Connection>,
    secrets: Secrets,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS devices (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, platform TEXT NOT NULL, secret_hash TEXT NOT NULL,
  created INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0, last_seen INTEGER,
  admin INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS tools (
  device_id TEXT NOT NULL, name TEXT NOT NULL, description TEXT NOT NULL, permission TEXT NOT NULL,
  PRIMARY KEY (device_id, name));
CREATE TABLE IF NOT EXISTS providers (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, base_url TEXT, api_key TEXT, options TEXT NOT NULL DEFAULT '{}');
CREATE TABLE IF NOT EXISTS models (
  id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, model TEXT NOT NULL, input_price REAL NOT NULL DEFAULT 0,
  output_price REAL NOT NULL DEFAULT 0, context_window INTEGER NOT NULL DEFAULT 0,
  use_cases TEXT NOT NULL DEFAULT '', free INTEGER NOT NULL DEFAULT 0, roles TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS conversations (
  id TEXT PRIMARY KEY, mode TEXT NOT NULL, title TEXT NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS history (
  conversation_id TEXT NOT NULL, idx INTEGER NOT NULL, message TEXT NOT NULL,
  PRIMARY KEY (conversation_id, idx));
CREATE TABLE IF NOT EXISTS threads (
  id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, title TEXT NOT NULL, device_id TEXT NOT NULL,
  model TEXT NOT NULL, status TEXT NOT NULL, reason TEXT, brief TEXT NOT NULL, summary TEXT,
  created INTEGER NOT NULL, finished INTEGER);
CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, conversation_id TEXT NOT NULL, thread_id TEXT,
  ts INTEGER NOT NULL, body TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_conv ON events (conversation_id, seq);
CREATE TABLE IF NOT EXISTS usage (
  id INTEGER PRIMARY KEY AUTOINCREMENT, conversation_id TEXT, thread_id TEXT, provider_id TEXT NOT NULL,
  model TEXT NOT NULL, input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
  cache_read_tokens INTEGER NOT NULL, cache_write_tokens INTEGER NOT NULL, cost REAL NOT NULL, ts INTEGER NOT NULL);
";

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
    })
}
const THREAD_COLS: &str = "id, conversation_id, title, device_id, model, status, reason, summary, created, finished";

fn model_row(r: &rusqlite::Row) -> rusqlite::Result<ModelEntry> {
    let roles: String = r.get(8)?;
    Ok(ModelEntry {
        id: r.get(0)?,
        provider_id: r.get(1)?,
        model: r.get(2)?,
        input_price: r.get(3)?,
        output_price: r.get(4)?,
        context_window: r.get::<_, i64>(5)? as u64,
        use_cases: r.get(6)?,
        free: r.get::<_, i64>(7)? != 0,
        roles: roles.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
    })
}
const MODEL_COLS: &str = "id, provider_id, model, input_price, output_price, context_window, use_cases, free, roles";

fn migrate_provider_secrets(conn: &mut Connection, secrets: &Secrets) -> Result<()> {
    let tx = conn.transaction()?;
    let rows = {
        let mut statement = tx.prepare("SELECT id, api_key FROM providers WHERE api_key IS NOT NULL")?;
        statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, api_key) in rows {
        if Secrets::is_sealed(&api_key) {
            secrets.open(&api_key).with_context(|| format!("decrypting provider secret for {id}"))?;
        } else {
            let sealed = secrets.seal(&api_key).with_context(|| format!("encrypting provider secret for {id}"))?;
            tx.execute("UPDATE providers SET api_key = ?1 WHERE id = ?2", params![sealed, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

impl Db {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let secrets = Secrets::load(data_dir)?;
        Self::open_with_secrets(data_dir, secrets)
    }

    fn open_with_secrets(data_dir: &Path, secrets: Secrets) -> Result<Self> {
        std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let mut conn = Connection::open(data_dir.join("grouper.db"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(crate::memory::SCHEMA)?;
        // Databases made before admin tokens: every existing token becomes a plain
        // device token; grant admin with `grouper-hub device admin <id>`.
        let has_admin: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('devices') WHERE name = 'admin'")?
            .exists([])?;
        if !has_admin {
            conn.execute_batch("ALTER TABLE devices ADD COLUMN admin INTEGER NOT NULL DEFAULT 0;")?;
        }
        migrate_provider_secrets(&mut conn, &secrets)?;
        let db = Self { connection: Mutex::new(conn), secrets };
        db.import_devices_json(data_dir)?;
        Ok(db)
    }

    pub(crate) fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Milestone 1 kept devices in devices.json; bring them over once.
    fn import_devices_json(&self, data_dir: &Path) -> Result<()> {
        let path = data_dir.join("devices.json");
        let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
        let v: Value = serde_json::from_str(&text)?;
        let c = self.c();
        for d in v.get("devices").and_then(Value::as_array).into_iter().flatten() {
            let g = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            c.execute(
                "INSERT OR IGNORE INTO devices (id, name, platform, secret_hash, created, revoked, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    g("id"),
                    g("name"),
                    g("platform"),
                    g("secret_hash"),
                    d.get("created").and_then(Value::as_i64).unwrap_or(0),
                    d.get("revoked").and_then(Value::as_bool).unwrap_or(false),
                    d.get("last_seen").and_then(Value::as_i64)
                ],
            )?;
        }
        std::fs::rename(&path, data_dir.join("devices.json.imported"))?;
        Ok(())
    }

    // ---- devices

    pub fn add_device(&self, name: &str, platform: &str, admin: bool) -> Result<(Device, String)> {
        let name = name.trim();
        if name.is_empty() {
            bail!("device name is empty");
        }
        if name.eq_ignore_ascii_case("hub") {
            bail!("\"hub\" is reserved for the hub itself");
        }
        let id = random_hex(6);
        let secret = random_hex(32);
        let platform = platform.trim().to_lowercase();
        self.c().execute(
            "INSERT INTO devices (id, name, platform, secret_hash, created, admin) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, name, platform, hash(&secret), ts(), admin],
        )?;
        Ok((
            Device { id: id.clone(), name: name.into(), platform, revoked: false, last_seen: None, admin },
            format!("{id}.{secret}"),
        ))
    }

    /// Grant or remove admin rights for a device's token.
    pub fn set_admin(&self, id: &str, admin: bool) -> Result<()> {
        if self.c().execute("UPDATE devices SET admin = ?1 WHERE id = ?2", params![admin, id])? == 0 {
            bail!("no such device");
        }
        Ok(())
    }

    pub fn revoke_device(&self, id: &str) -> Result<()> {
        if self.c().execute("UPDATE devices SET revoked = 1 WHERE id = ?1", [id])? == 0 {
            bail!("no such device");
        }
        Ok(())
    }

    pub fn devices(&self) -> Vec<Device> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, name, platform, revoked, last_seen, admin FROM devices ORDER BY created").unwrap();
        st.query_map([], |r| {
            Ok(Device {
                id: r.get(0)?,
                name: r.get(1)?,
                platform: r.get(2)?,
                revoked: r.get::<_, i64>(3)? != 0,
                last_seen: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                admin: r.get::<_, i64>(5)? != 0,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    /// The device a token belongs to, if valid and not revoked.
    pub fn authenticate(&self, token: &str) -> Option<Auth> {
        let (id, secret) = core_protocol::split_token(token)?;
        let (stored, admin): (String, i64) = self
            .c()
            .query_row("SELECT secret_hash, admin FROM devices WHERE id = ?1 AND revoked = 0", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .ok()?;
        ct_eq(&stored, &hash(secret)).then(|| Auth { device_id: id.to_string(), admin: admin != 0 })
    }

    pub fn touch(&self, id: &str) {
        let _ = self.c().execute("UPDATE devices SET last_seen = ?1 WHERE id = ?2", params![ts(), id]);
    }

    // ---- tool registry

    /// Record the tools a device offers. New tools get their default permission;
    /// existing ones keep whatever the user set.
    pub fn upsert_tools(&self, device_id: &str, defs: &[ToolDef]) {
        let c = self.c();
        for d in defs {
            let _ = c.execute(
                "INSERT INTO tools (device_id, name, description, permission) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (device_id, name) DO UPDATE SET description = excluded.description",
                params![device_id, d.name, d.description, Permission::default_for(&d.name).as_str()],
            );
        }
    }

    pub fn tools(&self, device_id: Option<&str>) -> Vec<ToolInfo> {
        let c = self.c();
        let mut st = c
            .prepare(
                "SELECT device_id, name, description, permission FROM tools
                 WHERE ?1 IS NULL OR device_id = ?1 ORDER BY device_id, name",
            )
            .unwrap();
        st.query_map([device_id], |r| {
            let p: String = r.get(3)?;
            Ok(ToolInfo {
                device_id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                permission: Permission::parse(&p).unwrap_or(Permission::Manual),
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn set_permission(&self, device_id: &str, tool: &str, p: Permission) -> Result<()> {
        let n = self.c().execute(
            "UPDATE tools SET permission = ?1 WHERE device_id = ?2 AND name = ?3",
            params![p.as_str(), device_id, tool],
        )?;
        if n == 0 {
            bail!("device {device_id} has no tool {tool}");
        }
        Ok(())
    }

    pub fn permissions(&self, device_id: &str) -> HashMap<String, Permission> {
        self.tools(Some(device_id)).into_iter().map(|t| (t.name, t.permission)).collect()
    }

    // ---- providers and models

    pub fn upsert_provider(&self, p: &ProviderConfig) -> Result<()> {
        if !["anthropic", "openai", "scripted"].contains(&p.kind.as_str()) {
            bail!("kind must be anthropic, openai or scripted");
        }
        let options = if p.options.is_null() { "{}".to_string() } else { p.options.to_string() };
        let api_key = p.api_key.as_deref().map(|key| self.secrets.seal(key)).transpose()?;
        self.c().execute(
            "INSERT INTO providers (id, kind, base_url, api_key, options) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, base_url = excluded.base_url,
               api_key = COALESCE(excluded.api_key, providers.api_key), options = excluded.options",
            params![p.id, p.kind, p.base_url, api_key, options],
        )?;
        Ok(())
    }

    pub fn providers(&self) -> Result<Vec<ProviderConfig>> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, kind, base_url, api_key, options FROM providers ORDER BY id")?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, kind, base_url, api_key, options)| {
                Ok(ProviderConfig {
                    id,
                    kind,
                    base_url,
                    api_key: api_key.map(|key| self.secrets.open(&key)).transpose()?,
                    options: serde_json::from_str(&options).unwrap_or(Value::Null),
                })
            })
            .collect()
    }

    pub fn provider(&self, id: &str) -> Result<Option<ProviderConfig>> {
        Ok(self.providers()?.into_iter().find(|p| p.id == id))
    }

    pub fn upsert_model(&self, m: &ModelEntry) -> Result<()> {
        if self.provider(&m.provider_id)?.is_none() {
            bail!("no provider {}; add it first", m.provider_id);
        }
        self.c().execute(
            "INSERT OR REPLACE INTO models (id, provider_id, model, input_price, output_price, context_window,
               use_cases, free, roles) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                m.id,
                m.provider_id,
                m.model,
                m.input_price,
                m.output_price,
                m.context_window as i64,
                m.use_cases,
                m.free,
                m.roles.join(",")
            ],
        )?;
        Ok(())
    }

    pub fn delete_model(&self, id: &str) -> Result<()> {
        if self.c().execute("DELETE FROM models WHERE id = ?1", [id])? == 0 {
            bail!("no such model");
        }
        Ok(())
    }

    pub fn models(&self) -> Vec<ModelEntry> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {MODEL_COLS} FROM models ORDER BY id")).unwrap();
        st.query_map([], model_row).unwrap().filter_map(Result::ok).collect()
    }

    /// The model to use for a role. Standard chat prefers a free model.
    pub fn default_model(&self, role: &str) -> Option<ModelEntry> {
        let models = self.models();
        let by_role = |r: &str| models.iter().find(|m| m.roles.iter().any(|x| x == r)).cloned();
        match role {
            "standard" => by_role("standard").or_else(|| models.iter().find(|m| m.free).cloned()),
            "thread" => by_role("thread"),
            _ => None,
        }
        .or_else(|| by_role(role))
        .or_else(|| by_role("orchestrator"))
        .or_else(|| models.first().cloned())
    }

    /// A model by registry id, or by the provider's model name.
    pub fn find_model(&self, key: &str) -> Option<ModelEntry> {
        let models = self.models();
        models.iter().find(|m| m.id == key).or_else(|| models.iter().find(|m| m.model == key)).cloned()
    }

    // ---- conversations and history

    pub fn create_conversation(&self, mode: &str, title: &str) -> Result<String> {
        let id = random_hex(8);
        self.c().execute(
            "INSERT INTO conversations (id, mode, title, created) VALUES (?1, ?2, ?3, ?4)",
            params![id, mode, title, ts()],
        )?;
        Ok(id)
    }

    pub fn conversation(&self, id: &str) -> Option<ConversationInfo> {
        self.c()
            .query_row("SELECT id, mode, title, created FROM conversations WHERE id = ?1", [id], |r| {
                Ok(ConversationInfo { id: r.get(0)?, mode: r.get(1)?, title: r.get(2)?, created: r.get::<_, i64>(3)? as u64 })
            })
            .optional()
            .ok()
            .flatten()
    }

    pub fn conversations(&self) -> Vec<ConversationInfo> {
        let c = self.c();
        let mut st = c.prepare("SELECT id, mode, title, created FROM conversations ORDER BY created DESC").unwrap();
        st.query_map([], |r| {
            Ok(ConversationInfo { id: r.get(0)?, mode: r.get(1)?, title: r.get(2)?, created: r.get::<_, i64>(3)? as u64 })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn history(&self, conversation_id: &str) -> Vec<Message> {
        let c = self.c();
        let mut st = c.prepare("SELECT message FROM history WHERE conversation_id = ?1 ORDER BY idx").unwrap();
        st.query_map([conversation_id], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok().and_then(|s| serde_json::from_str(&s).ok()))
            .collect()
    }

    /// Store messages from index `from` on (rewriting only a merged trailing user message).
    pub fn save_history(&self, conversation_id: &str, from: usize, messages: &[Message]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM history WHERE conversation_id = ?1 AND idx >= ?2", params![conversation_id, from as i64])?;
        for (i, m) in messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO history (conversation_id, idx, message) VALUES (?1, ?2, ?3)",
                params![conversation_id, (from + i) as i64, serde_json::to_string(m)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- threads

    #[allow(clippy::too_many_arguments)]
    pub fn create_thread(
        &self,
        conversation_id: &str,
        title: &str,
        device_id: &str,
        model: &str,
        reason: Option<&str>,
        brief: &str,
    ) -> Result<ThreadInfo> {
        let id = format!("t{}", random_hex(4));
        self.c().execute(
            "INSERT INTO threads (id, conversation_id, title, device_id, model, status, reason, brief, created)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8)",
            params![id, conversation_id, title, device_id, model, reason, brief, ts()],
        )?;
        Ok(self.thread(&id).expect("just inserted"))
    }

    pub fn thread(&self, id: &str) -> Option<ThreadInfo> {
        self.c()
            .query_row(&format!("SELECT {THREAD_COLS} FROM threads WHERE id = ?1"), [id], thread_row)
            .optional()
            .ok()
            .flatten()
    }

    pub fn threads(&self, conversation_id: Option<&str>) -> Vec<ThreadInfo> {
        let c = self.c();
        let mut st = c
            .prepare(&format!(
                "SELECT {THREAD_COLS} FROM threads WHERE ?1 IS NULL OR conversation_id = ?1 ORDER BY created"
            ))
            .unwrap();
        st.query_map([conversation_id], thread_row).unwrap().filter_map(Result::ok).collect()
    }

    pub fn running_threads_on(&self, device_id: &str) -> Vec<ThreadInfo> {
        self.threads(None).into_iter().filter(|t| t.device_id == device_id && t.status == "running").collect()
    }

    /// Mark a running thread finished. Returns false if it had already finished.
    pub fn finish_thread(&self, id: &str, status: &str, summary: &str) -> bool {
        self.c()
            .execute(
                "UPDATE threads SET status = ?1, summary = ?2, finished = ?3 WHERE id = ?4 AND status = 'running'",
                params![status, summary, ts(), id],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    // ---- events and usage

    pub fn add_event(&self, conversation_id: &str, thread_id: Option<&str>, body: EventBody) -> Event {
        let ts = now();
        let c = self.c();
        let _ = c.execute(
            "INSERT INTO events (conversation_id, thread_id, ts, body) VALUES (?1, ?2, ?3, ?4)",
            params![conversation_id, thread_id, ts as i64, serde_json::to_string(&body).unwrap()],
        );
        Event {
            seq: c.last_insert_rowid(),
            conversation_id: conversation_id.into(),
            thread_id: thread_id.map(String::from),
            ts,
            body,
        }
    }

    pub fn events(&self, conversation_id: &str, after: i64, thread_id: Option<&str>) -> Vec<Event> {
        let c = self.c();
        let mut st = c
            .prepare(
                "SELECT seq, conversation_id, thread_id, ts, body FROM events
                 WHERE conversation_id = ?1 AND seq > ?2 AND (?3 IS NULL OR thread_id = ?3) ORDER BY seq",
            )
            .unwrap();
        st.query_map(params![conversation_id, after, thread_id], |r| {
            let body: String = r.get(4)?;
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u64, body))
        })
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|(seq, conversation_id, thread_id, ts, body)| {
            Some(Event { seq, conversation_id, thread_id, ts, body: serde_json::from_str(&body).ok()? })
        })
        .collect()
    }

    /// Record token usage; returns the estimated cost in USD.
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage(
        &self,
        conversation_id: Option<&str>,
        thread_id: Option<&str>,
        provider_id: &str,
        model: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> f64 {
        let price = self.models().into_iter().find(|m| m.provider_id == provider_id && m.model == model);
        let cost = price
            .map(|m| {
                (m.input_price * (input as f64 + 1.25 * cache_write as f64 + 0.1 * cache_read as f64)
                    + m.output_price * output as f64)
                    / 1_000_000.0
            })
            .unwrap_or(0.0);
        let _ = self.c().execute(
            "INSERT INTO usage (conversation_id, thread_id, provider_id, model, input_tokens, output_tokens,
               cache_read_tokens, cache_write_tokens, cost, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![conversation_id, thread_id, provider_id, model, input as i64, output as i64, cache_read as i64, cache_write as i64, cost, ts()],
        );
        cost
    }

    /// (input tokens, output tokens, cost) for a conversation.
    pub fn usage_total(&self, conversation_id: &str) -> (u64, u64, f64) {
        self.c()
            .query_row(
                "SELECT COALESCE(SUM(input_tokens + cache_read_tokens + cache_write_tokens), 0),
                        COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cost), 0)
                 FROM usage WHERE conversation_id = ?1",
                [conversation_id],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get(2)?)),
            )
            .unwrap_or((0, 0, 0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-db-test-{}", random_hex(4)))
    }

    #[test]
    fn devices_and_tokens() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (d, token) = db.add_device("MacBook", "macOS", false).unwrap();
        assert_eq!(db.authenticate(&token), Some(Auth { device_id: d.id.clone(), admin: false }));
        assert!(db.authenticate(&format!("{}.wrong", d.id)).is_none());
        assert!(db.add_device("hub", "linux", false).is_err());
        let (phone, phone_token) = db.add_device("Phone", "ios", true).unwrap();
        assert!(db.authenticate(&phone_token).unwrap().admin);
        db.set_admin(&phone.id, false).unwrap();
        assert!(!db.authenticate(&phone_token).unwrap().admin);
        db.set_admin(&d.id, true).unwrap();
        assert!(db.authenticate(&token).unwrap().admin);
        db.revoke_device(&d.id).unwrap();
        assert!(db.authenticate(&token).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn old_databases_get_the_admin_column() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("grouper.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE devices (id TEXT PRIMARY KEY, name TEXT NOT NULL, platform TEXT NOT NULL,
               secret_hash TEXT NOT NULL, created INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0,
               last_seen INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO devices (id, name, platform, secret_hash, created) VALUES ('old', 'Old', 'macos', ?1, 0)",
            [hash("s")],
        )
        .unwrap();
        drop(conn);
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.authenticate("old.s"), Some(Auth { device_id: "old".into(), admin: false }));
        drop(db);
        Db::open(&dir).unwrap(); // reopening doesn't try to add the column again
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tool_permissions_survive_reports() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let def = |n: &str| ToolDef { name: n.into(), description: "d".into(), input_schema: Value::Null };
        db.upsert_tools("dev", &[def("read_file"), def("run_shell")]);
        let p = db.permissions("dev");
        assert_eq!(p["read_file"], Permission::Auto);
        assert_eq!(p["run_shell"], Permission::Manual);
        db.set_permission("dev", "run_shell", Permission::Bypass).unwrap();
        db.upsert_tools("dev", &[def("run_shell")]);
        assert_eq!(db.permissions("dev")["run_shell"], Permission::Bypass);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn default_models() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "or".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: Some("k".into()),
            options: Value::Null,
        })
        .unwrap();
        let m = |id: &str, free: bool, roles: &[&str]| ModelEntry {
            id: id.into(),
            provider_id: "or".into(),
            model: id.into(),
            input_price: 1.0,
            output_price: 2.0,
            context_window: 0,
            use_cases: String::new(),
            free,
            roles: roles.iter().map(|s| s.to_string()).collect(),
        };
        db.upsert_model(&m("big", false, &["orchestrator"])).unwrap();
        db.upsert_model(&m("free", true, &[])).unwrap();
        assert_eq!(db.default_model("orchestrator").unwrap().id, "big");
        assert_eq!(db.default_model("standard").unwrap().id, "free");
        assert_eq!(db.default_model("thread").unwrap().id, "big");
        // keys are kept when an update omits them
        db.upsert_provider(&ProviderConfig {
            id: "or".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
        assert_eq!(db.provider("or").unwrap().unwrap().api_key.as_deref(), Some("k"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plaintext_provider_keys_are_migrated() {
        let dir = tmp();
        let db = Db::open_with_secrets(&dir, Secrets::from_key([3; 32])).unwrap();
        db.c()
            .execute(
                "INSERT INTO providers (id, kind, api_key) VALUES (?1, ?2, ?3)",
                params!["legacy", "openai", "plaintext-key"],
            )
            .unwrap();
        drop(db);

        let db = Db::open_with_secrets(&dir, Secrets::from_key([3; 32])).unwrap();
        let stored: String = db
            .c()
            .query_row("SELECT api_key FROM providers WHERE id = 'legacy'", [], |row| row.get(0))
            .unwrap();
        assert!(Secrets::is_sealed(&stored));
        assert!(!stored.contains("plaintext-key"));
        assert_eq!(db.provider("legacy").unwrap().unwrap().api_key.as_deref(), Some("plaintext-key"));
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn opening_with_the_wrong_master_key_fails_clearly() {
        let dir = tmp();
        let db = Db::open_with_secrets(&dir, Secrets::from_key([4; 32])).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "secured".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: Some("secret".into()),
            options: Value::Null,
        })
        .unwrap();
        drop(db);

        let error = Db::open_with_secrets(&dir, Secrets::from_key([5; 32])).err().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("decrypting provider secret for secured"));
        assert!(message.contains("master key is wrong"));
        std::fs::remove_dir_all(dir).ok();
    }
}
