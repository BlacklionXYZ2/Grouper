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
  available INTEGER NOT NULL DEFAULT 1, input_schema TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (device_id, name));
CREATE TABLE IF NOT EXISTS providers (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL, base_url TEXT, api_key TEXT, options TEXT NOT NULL DEFAULT '{}');
CREATE TABLE IF NOT EXISTS models (
  id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, model TEXT NOT NULL, input_price REAL NOT NULL DEFAULT 0,
  output_price REAL NOT NULL DEFAULT 0, cache_read_price REAL, cache_write_price REAL,
  context_window INTEGER NOT NULL DEFAULT 0,
  use_cases TEXT NOT NULL DEFAULT '', free INTEGER NOT NULL DEFAULT 0, roles TEXT NOT NULL DEFAULT '',
  modalities TEXT NOT NULL DEFAULT '');
CREATE TABLE IF NOT EXISTS conversations (
  id TEXT PRIMARY KEY, mode TEXT NOT NULL, title TEXT NOT NULL, created INTEGER NOT NULL, model TEXT);
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
CREATE TABLE IF NOT EXISTS usage_snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT, account TEXT NOT NULL, provider TEXT NOT NULL, kind TEXT NOT NULL,
  window TEXT NOT NULL, used_pct REAL, used_amount REAL, limit_amount REAL, currency TEXT, resets_at INTEGER,
  source TEXT NOT NULL, device_id TEXT, fetched_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS usage_snapshots_latest ON usage_snapshots
  (provider, account, kind, window, fetched_at DESC);
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
        rating: r.get(10)?,
    })
}
const THREAD_SELECT: &str = "SELECT t.id, t.conversation_id, t.title, t.device_id, t.model, t.status, t.reason, t.summary, t.created, t.finished, o.user_rating \
                             FROM threads t LEFT JOIN thread_outcomes o ON o.thread_id = t.id";

fn csv_list(value: String) -> Vec<String> {
    value.split(',').filter(|s| !s.is_empty()).map(String::from).collect()
}

fn model_row(r: &rusqlite::Row) -> rusqlite::Result<ModelEntry> {
    let roles: String = r.get(10)?;
    let modalities: String = r.get(12)?;
    Ok(ModelEntry {
        id: r.get(0)?,
        provider_id: r.get(1)?,
        model: r.get(2)?,
        input_price: r.get(3)?,
        output_price: r.get(4)?,
        cache_read_price: r.get(5)?,
        cache_write_price: r.get(6)?,
        context_window: r.get::<_, i64>(7)? as u64,
        use_cases: r.get(8)?,
        free: r.get::<_, i64>(9)? != 0,
        roles: csv_list(roles),
        local: r.get::<_, i64>(11)? != 0,
        modalities: csv_list(modalities),
    })
}
const MODEL_COLS: &str = "id, provider_id, model, input_price, output_price, cache_read_price, cache_write_price, \
                           context_window, use_cases, free, roles, local, modalities";

fn conversation_row(r: &rusqlite::Row) -> rusqlite::Result<ConversationInfo> {
    Ok(ConversationInfo {
        id: r.get(0)?,
        mode: r.get(1)?,
        title: r.get(2)?,
        created: r.get::<_, i64>(3)? as u64,
        model: r.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
    })
}
const CONVERSATION_COLS: &str = "id, mode, title, created, model";

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

/// Whether anything is sealed with the master key (provider keys, the Web Push VAPID key), in
/// which case a missing master key must not be replaced by a fresh one.
fn has_sealed_provider_keys(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare(
            "SELECT 1 FROM providers WHERE api_key LIKE 'enc:v1:%' UNION ALL SELECT 1 FROM hub_meta WHERE value LIKE 'enc:v1:%' LIMIT 1",
        )?
        .exists([])?)
}

fn open_connection(data_dir: &Path) -> Result<Connection> {
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let conn = Connection::open(data_dir.join("grouper.db"))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(SCHEMA)?;
    core_memory::memory::migrate(&conn)?;
    core_registry::migrate(&conn)?;
    core_memory::skills::migrate(&conn)?;
    conn.execute_batch(crate::offline::SCHEMA)?;
    conn.execute_batch(crate::limits::SCHEMA)?;
    conn.execute_batch(crate::session::SCHEMA)?;
    conn.execute_batch(crate::push::SCHEMA)?;
    conn.execute_batch(crate::google_auth::SCHEMA)?;
    // Databases made before admin tokens: every existing token becomes a plain
    // device token; grant admin with `grouper-hub device admin <id>`.
    let has_admin: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('devices') WHERE name = 'admin'")?
        .exists([])?;
    if !has_admin {
        conn.execute_batch("ALTER TABLE devices ADD COLUMN admin INTEGER NOT NULL DEFAULT 0;")?;
    }
    for column in ["cache_read_price", "cache_write_price"] {
        let exists = conn
            .prepare("SELECT 1 FROM pragma_table_info('models') WHERE name = ?1")?
            .exists([column])?;
        if !exists {
            conn.execute(&format!("ALTER TABLE models ADD COLUMN {column} REAL"), [])?;
        }
    }
    ensure_column(&conn, "models", "modalities", "ALTER TABLE models ADD COLUMN modalities TEXT NOT NULL DEFAULT ''")?;
    ensure_column(&conn, "conversations", "model", "ALTER TABLE conversations ADD COLUMN model TEXT")?;
    ensure_column(&conn, "tools", "available", "ALTER TABLE tools ADD COLUMN available INTEGER NOT NULL DEFAULT 1")?;
    ensure_column(&conn, "tools", "input_schema", "ALTER TABLE tools ADD COLUMN input_schema TEXT NOT NULL DEFAULT '{}'")?;
    ensure_column(&conn, "devices", "folders", "ALTER TABLE devices ADD COLUMN folders TEXT NOT NULL DEFAULT '[]'")?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS hub_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
    Ok(conn)
}

/// Tool names lost their dots (`mac.info` → `mac_info`) when devices became MCP
/// servers. Rename old registry rows, keeping their permission; where both spellings
/// exist, the underscore row (the one devices report now) wins.
fn migrate_dotted_tool_names(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "UPDATE OR IGNORE tools SET name = replace(name, '.', '_') WHERE instr(name, '.') > 0;
         DELETE FROM tools WHERE instr(name, '.') > 0;",
    )?;
    Ok(())
}

/// `ALTER TABLE` only when the column is missing, so opening an existing database is safe to repeat.
fn ensure_column(conn: &Connection, table: &str, column: &str, alter_sql: &str) -> Result<()> {
    let exists = conn
        .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?
        .exists([column])?;
    if !exists {
        conn.execute(alter_sql, [])?;
    }
    Ok(())
}

impl Db {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let conn = open_connection(data_dir)?;
        let has_sealed = has_sealed_provider_keys(&conn)?;
        let secrets = Secrets::load_with_create(data_dir, !has_sealed)?;
        Self::finish_open(data_dir, conn, secrets)
    }

    #[cfg(test)]
    fn open_with_secrets(data_dir: &Path, secrets: Secrets) -> Result<Self> {
        let conn = open_connection(data_dir)?;
        Self::finish_open(data_dir, conn, secrets)
    }

    fn finish_open(data_dir: &Path, mut conn: Connection, secrets: Secrets) -> Result<Self> {
        migrate_provider_secrets(&mut conn, &secrets)?;
        migrate_dotted_tool_names(&conn)?;
        let db = Self { connection: Mutex::new(conn), secrets };
        db.import_devices_json(data_dir)?;
        Ok(db)
    }

    pub(crate) fn c(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This hub's random id, made on first use and kept in the database.
    pub fn hub_id(&self) -> String {
        let c = self.c();
        let existing: Option<String> = c
            .query_row("SELECT value FROM hub_meta WHERE key = 'hub_id'", [], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        if let Some(id) = existing {
            return id;
        }
        let id = random_hex(16);
        let _ = c.execute("INSERT OR IGNORE INTO hub_meta (key, value) VALUES ('hub_id', ?1)", [&id]);
        c.query_row("SELECT value FROM hub_meta WHERE key = 'hub_id'", [], |r| r.get(0)).unwrap_or(id)
    }

    /// The Web Push (VAPID) private key: a P-256 scalar, made on first use and kept in
    /// `hub_meta` sealed with the master key like provider keys. `generate` makes a new one.
    pub(crate) fn vapid_private_key(&self, generate: impl FnOnce() -> Result<Vec<u8>>) -> Result<Vec<u8>> {
        use base64::Engine as _;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        const KEY: &str = "vapid_private_key";
        let read = |c: &Connection| -> Result<Option<String>> {
            Ok(c.query_row("SELECT value FROM hub_meta WHERE key = ?1", [KEY], |r| r.get(0)).optional()?)
        };
        let c = self.c();
        let sealed = match read(&c)? {
            Some(sealed) => sealed,
            None => {
                let key = generate()?;
                let sealed = self.secrets.seal(&URL_SAFE_NO_PAD.encode(&key))?;
                c.execute("INSERT OR IGNORE INTO hub_meta (key, value) VALUES (?1, ?2)", params![KEY, sealed])?;
                read(&c)?.context("VAPID key missing right after it was stored")?
            }
        };
        drop(c);
        let encoded = self
            .secrets
            .open(&sealed)
            .context("cannot decrypt the Web Push VAPID key; is this the master key that sealed it?")?;
        URL_SAFE_NO_PAD.decode(encoded.trim()).context("stored VAPID key is not valid base64url")
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

    /// The scoped folders a device reported on its latest connection (names and modes).
    pub fn set_device_folders(&self, id: &str, folders: &[core_protocol::FolderInfo]) {
        let json = serde_json::to_string(folders).unwrap_or_else(|_| "[]".into());
        let _ = self.c().execute("UPDATE devices SET folders = ?1 WHERE id = ?2", params![json, id]);
    }

    pub fn device_folders(&self, id: &str) -> Vec<core_protocol::FolderInfo> {
        self.c()
            .query_row("SELECT folders FROM devices WHERE id = ?1", [id], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    // ---- tool registry

    /// Record the tools a device currently offers. Rows for this device not in `defs`
    /// are marked unavailable (keeping their saved permission level so returning tools
    /// keep it); listed tools are marked available and get their default permission if new.
    pub fn upsert_tools(&self, device_id: &str, defs: &[ToolDef]) {
        let mut c = self.c();
        let Ok(tx) = c.transaction() else { return };
        let _ = tx.execute("UPDATE tools SET available = 0 WHERE device_id = ?1", [device_id]);
        for d in defs {
            let schema = if d.input_schema.is_object() {
                d.input_schema.to_string()
            } else {
                core_protocol::default_input_schema().to_string()
            };
            let _ = tx.execute(
                "INSERT INTO tools (device_id, name, description, permission, available, input_schema)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5)
                 ON CONFLICT (device_id, name) DO UPDATE SET
                   description = excluded.description,
                   available = 1,
                   input_schema = excluded.input_schema",
                params![device_id, d.name, d.description, Permission::default_for(&d.name).as_str(), schema],
            );
        }
        let _ = tx.commit();
    }

    fn query_tools(&self, device_id: Option<&str>, include_unavailable: bool) -> Vec<ToolInfo> {
        let c = self.c();
        let sql = if include_unavailable {
            "SELECT device_id, name, description, permission, available, input_schema FROM tools
             WHERE ?1 IS NULL OR device_id = ?1 ORDER BY device_id, name"
        } else {
            "SELECT device_id, name, description, permission, available, input_schema FROM tools
             WHERE available != 0 AND (?1 IS NULL OR device_id = ?1) ORDER BY device_id, name"
        };
        let mut st = c.prepare(sql).unwrap();
        st.query_map([device_id], |r| {
            let p: String = r.get(3)?;
            let available: i64 = r.get(4)?;
            let schema_str: String = r.get(5)?;
            let input_schema = serde_json::from_str::<Value>(&schema_str)
                .ok()
                .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
                .unwrap_or_else(core_protocol::default_input_schema);
            Ok(ToolInfo {
                device_id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                permission: Permission::parse(&p).unwrap_or(Permission::Manual),
                available: available != 0,
                input_schema,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    /// Currently available tools (offered in each device's latest `tools/list`).
    pub fn tools(&self, device_id: Option<&str>) -> Vec<ToolInfo> {
        self.query_tools(device_id, false)
    }

    /// Every tool in the registry, including ones a device no longer offers (`available = false`).
    pub fn all_tools(&self, device_id: Option<&str>) -> Vec<ToolInfo> {
        self.query_tools(device_id, true)
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

    /// Permission levels of a device's currently available tools.
    pub fn permissions(&self, device_id: &str) -> HashMap<String, Permission> {
        self.tools(Some(device_id)).into_iter().map(|t| (t.name, t.permission)).collect()
    }

    // ---- providers and models

    pub fn upsert_provider(&self, p: &ProviderConfig) -> Result<()> {
        if !["anthropic", "openai", "scripted"].contains(&p.kind.as_str()) {
            bail!("kind must be anthropic, openai or scripted");
        }
        if let Some(value) = p.options.get("via_device") {
            let Some(host) = value.as_str().map(str::trim).filter(|host| !host.is_empty()) else {
                bail!("options.via_device must be a non-empty device id or name");
            };
            let devices: Vec<Device> = self
                .devices()
                .into_iter()
                .filter(|device| !device.revoked && (device.id == host || device.name.eq_ignore_ascii_case(host)))
                .collect();
            if devices.is_empty() {
                bail!("options.via_device names no device {host:?}");
            }
            if devices.len() > 1 && !devices.iter().any(|device| device.id == host) {
                bail!("options.via_device name {host:?} is ambiguous; use a device id");
            }
        }
        let options = if p.options.is_null() { "{}".to_string() } else { p.options.to_string() };
        let api_key = p
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .map(|key| self.secrets.seal(key))
            .transpose()?;
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
        core_registry::cost::check_price("input_price", m.input_price)?;
        core_registry::cost::check_price("output_price", m.output_price)?;
        if let Some(p) = m.cache_read_price {
            core_registry::cost::check_price("cache_read_price", p)?;
        }
        if let Some(p) = m.cache_write_price {
            core_registry::cost::check_price("cache_write_price", p)?;
        }
        self.c().execute(
            "INSERT OR REPLACE INTO models (id, provider_id, model, input_price, output_price, cache_read_price,
               cache_write_price, context_window, use_cases, free, roles, local, modalities)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                m.id,
                m.provider_id,
                m.model,
                m.input_price,
                m.output_price,
                m.cache_read_price,
                m.cache_write_price,
                m.context_window as i64,
                m.use_cases,
                m.free,
                m.roles.join(","),
                m.local,
                m.modalities.join(",")
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
        self.default_model_available(role, &std::collections::HashSet::new())
    }

    /// The role default excluding providers that cannot currently serve an
    /// automatically selected model (for example, an offline device host).
    pub fn default_model_available(
        &self,
        role: &str,
        unavailable_providers: &std::collections::HashSet<String>,
    ) -> Option<ModelEntry> {
        core_registry::default_model_from(&self.models(), role, unavailable_providers)
    }

    /// A model by registry id, or by the provider's model name.
    pub fn find_model(&self, key: &str) -> Option<ModelEntry> {
        core_registry::find_model_in(&self.models(), key)
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
            .query_row(&format!("SELECT {CONVERSATION_COLS} FROM conversations WHERE id = ?1"), [id], conversation_row)
            .optional()
            .ok()
            .flatten()
    }

    pub fn conversations(&self) -> Vec<ConversationInfo> {
        let c = self.c();
        let mut st = c.prepare(&format!("SELECT {CONVERSATION_COLS} FROM conversations ORDER BY created DESC")).unwrap();
        st.query_map([], conversation_row).unwrap().filter_map(Result::ok).collect()
    }

    /// Registry id this conversation uses instead of the role default, resolved to a model.
    /// `Ok(None)` means it uses the role default. An error means the stored id is no longer registered.
    pub fn conversation_model(&self, conversation_id: &str) -> Result<Option<ModelEntry>> {
        let Some(info) = self.conversation(conversation_id) else { bail!("no such conversation") };
        let Some(key) = info.model else { return Ok(None) };
        self.find_model(&key).map(Some).with_context(|| {
            format!("conversation {conversation_id} is set to model {key}, which is not in the registry")
        })
    }

    /// Set or clear the conversation's model. `model` is a registry id (or the provider's model
    /// name); null uses the role default again. The stored value is the registry id.
    pub fn set_conversation_model(&self, id: &str, model: Option<&str>) -> Result<ConversationInfo> {
        if self.conversation(id).is_none() {
            bail!("no such conversation");
        }
        let stored = match model.map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(key) => {
                let Some(found) = self.find_model(key) else { bail!("no model {key:?} in the registry") };
                Some(found.id)
            }
        };
        self.c().execute("UPDATE conversations SET model = ?1 WHERE id = ?2", params![stored, id])?;
        Ok(self.conversation(id).expect("conversation exists"))
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
            .query_row(&format!("{THREAD_SELECT} WHERE t.id = ?1"), [id], thread_row)
            .optional()
            .ok()
            .flatten()
    }

    pub fn threads(&self, conversation_id: Option<&str>) -> Vec<ThreadInfo> {
        let c = self.c();
        let mut st = c
            .prepare(&format!(
                "{THREAD_SELECT} WHERE ?1 IS NULL OR t.conversation_id = ?1 ORDER BY t.created"
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

    /// Finish a thread from either live state in one conditional update. This is used
    /// by stop paths that can race a device disconnect or hand-over transition.
    pub(crate) fn finish_active_thread(&self, id: &str, status: &str, summary: &str) -> bool {
        let c = self.c();
        let done = c
            .execute(
                "UPDATE threads SET status = ?1, summary = ?2, finished = ?3
                 WHERE id = ?4 AND status IN ('running', 'waiting_device')",
                params![status, summary, ts(), id],
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if done {
            let _ = c.execute("DELETE FROM offline_threads WHERE thread_id = ?1", [id]);
        }
        done
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

    pub fn last_event_seq(&self) -> i64 {
        self.c()
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn events_since(&self, after: i64, conversation: Option<&str>) -> Vec<Event> {
        let c = self.c();
        let mut st = c
            .prepare(
                "SELECT seq, conversation_id, thread_id, ts, body FROM events
                 WHERE seq > ?1 AND (?2 IS NULL OR conversation_id = ?2) ORDER BY seq",
            )
            .unwrap();
        st.query_map(params![after, conversation], |r| {
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
        let provider_kind = self.provider(provider_id).ok().flatten().map(|p| p.kind);
        let cost = price
            .map(|m| core_registry::cost::estimate_cost(&m, provider_kind.as_deref(), input, output, cache_read, cache_write))
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
        core_registry::cost::usage_total(&self.c(), conversation_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-db-test-{}", random_hex(4)))
    }

    #[test]
    fn hub_id_is_stable() {
        let dir = tmp();
        let id = Db::open(&dir).unwrap().hub_id();
        assert_eq!(id.len(), 32);
        assert_eq!(Db::open(&dir).unwrap().hub_id(), id, "same database, same id");
        let other = tmp();
        assert_ne!(Db::open(&other).unwrap().hub_id(), id, "another hub, another id");
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(other).ok();
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
    fn device_hosted_provider_requires_a_real_device_id_or_name() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (device, _) = db.add_device("Gaming PC", "windows", false).unwrap();
        let provider = |via: Value| ProviderConfig {
            id: "local".into(),
            kind: "openai".into(),
            base_url: Some("http://127.0.0.1:45123/v1".into()),
            api_key: None,
            options: serde_json::json!({"via_device": via}),
        };

        db.upsert_provider(&provider(Value::String("Gaming PC".into()))).unwrap();
        assert_eq!(db.provider("local").unwrap().unwrap().via_device(), Some("Gaming PC"));
        db.upsert_provider(&provider(Value::String(device.id.clone()))).unwrap();
        assert_eq!(db.provider("local").unwrap().unwrap().via_device(), Some(device.id.as_str()));

        for bad in [Value::String(String::new()), Value::String("Missing PC".into()), Value::Bool(true), Value::Null] {
            let error = db.upsert_provider(&provider(bad)).unwrap_err().to_string();
            assert!(error.contains("via_device"), "{error}");
        }
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
    fn old_databases_get_cache_price_columns_idempotently() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("grouper.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE models (
               id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, model TEXT NOT NULL,
               input_price REAL NOT NULL DEFAULT 0, output_price REAL NOT NULL DEFAULT 0,
               context_window INTEGER NOT NULL DEFAULT 0, use_cases TEXT NOT NULL DEFAULT '',
               free INTEGER NOT NULL DEFAULT 0, roles TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        drop(conn);
        drop(Db::open(&dir).unwrap());
        let db = Db::open(&dir).unwrap();
        for column in ["cache_read_price", "cache_write_price", "modalities"] {
            assert!(
                db.c()
                    .prepare("SELECT 1 FROM pragma_table_info('models') WHERE name = ?1")
                    .unwrap()
                    .exists([column])
                    .unwrap(),
                "missing column {column}"
            );
        }
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tool_permissions_survive_reports() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let def = |n: &str| ToolDef { name: n.into(), description: "d".into(), input_schema: Value::Null };
        let with_schema = |n: &str, schema: Value| ToolDef { name: n.into(), description: "d".into(), input_schema: schema };
        db.upsert_tools(
            "dev",
            &[
                def("read_file"),
                with_schema("run_shell", serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string"}}})),
            ],
        );
        db.upsert_tools("other", &[def("echo")]);
        let p = db.permissions("dev");
        assert_eq!(p["read_file"], Permission::Auto);
        assert_eq!(p["run_shell"], Permission::Manual);
        db.set_permission("dev", "run_shell", Permission::Bypass).unwrap();

        // Removing `read_file` marks it unavailable without touching `other` or losing permissions.
        db.upsert_tools("dev", &[def("run_shell")]);
        assert_eq!(db.permissions("dev").get("run_shell"), Some(&Permission::Bypass));
        assert!(!db.permissions("dev").contains_key("read_file"));
        assert_eq!(db.tools(Some("dev")).len(), 1);
        assert_eq!(db.tools(Some("other")).len(), 1);
        let all = db.all_tools(Some("dev"));
        assert_eq!(all.len(), 2);
        let gone = all.iter().find(|t| t.name == "read_file").unwrap();
        assert!(!gone.available);
        assert_eq!(gone.permission, Permission::Auto);

        // Removing `run_shell` keeps its Bypass permission; returning restores it as available.
        db.upsert_tools("dev", &[def("read_file")]);
        assert!(!db.permissions("dev").contains_key("run_shell"));
        let gone_shell = db.all_tools(Some("dev")).into_iter().find(|t| t.name == "run_shell").unwrap();
        assert!(!gone_shell.available);
        assert_eq!(gone_shell.permission, Permission::Bypass);

        let schema = serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string"}}});
        db.upsert_tools("dev", &[def("read_file"), with_schema("run_shell", schema.clone())]);
        assert_eq!(db.permissions("dev")["run_shell"], Permission::Bypass);
        let back = db.tools(Some("dev")).into_iter().find(|t| t.name == "run_shell").unwrap();
        assert!(back.available);
        assert_eq!(back.input_schema, schema);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn old_tools_table_gets_available_and_schema_columns_idempotently() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("grouper.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE tools (
               device_id TEXT NOT NULL, name TEXT NOT NULL, description TEXT NOT NULL, permission TEXT NOT NULL,
               PRIMARY KEY (device_id, name));
             INSERT INTO tools (device_id, name, description, permission) VALUES ('dev', 'echo', 'd', 'bypass');",
        )
        .unwrap();
        drop(conn);
        drop(Db::open(&dir).unwrap());
        let db = Db::open(&dir).unwrap();
        let tools = db.tools(Some("dev"));
        assert_eq!(tools.len(), 1);
        assert!(tools[0].available);
        assert_eq!(tools[0].permission, Permission::Bypass);
        assert_eq!(tools[0].input_schema, core_protocol::default_input_schema());
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dotted_tool_names_are_migrated() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        {
            let c = db.c();
            for (dev, name, perm) in [
                ("mac", "mac.info", "bypass"),
                ("pc", "system.info", "auto"),
                ("pc", "system_info", "manual"),
            ] {
                c.execute(
                    "INSERT INTO tools (device_id, name, description, permission) VALUES (?1, ?2, 'd', ?3)",
                    params![dev, name, perm],
                )
                .unwrap();
            }
        }
        drop(db);
        let db = Db::open(&dir).unwrap();
        let names: Vec<(String, String, Permission)> =
            db.tools(None).into_iter().map(|t| (t.device_id, t.name, t.permission)).collect();
        assert_eq!(
            names,
            vec![
                ("mac".into(), "mac_info".into(), Permission::Bypass),
                ("pc".into(), "system_info".into(), Permission::Manual),
            ]
        );
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
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free,
            roles: roles.iter().map(|s| s.to_string()).collect(),
            local: false,
            modalities: vec![],
        };
        db.upsert_model(&m("big", false, &["orchestrator"])).unwrap();
        db.upsert_model(&m("free", true, &[])).unwrap();
        assert_eq!(db.default_model("orchestrator").unwrap().id, "big");
        assert_eq!(db.default_model("standard").unwrap().id, "free");
        assert_eq!(db.default_model("thread").unwrap().id, "big");
        // keys are kept when an update omits them or passes an empty string
        db.upsert_provider(&ProviderConfig {
            id: "or".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
        assert_eq!(db.provider("or").unwrap().unwrap().api_key.as_deref(), Some("k"));
        db.upsert_provider(&ProviderConfig {
            id: "or".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: Some(String::new()),
            options: Value::Null,
        })
        .unwrap();
        assert_eq!(db.provider("or").unwrap().unwrap().api_key.as_deref(), Some("k"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cache_cost_uses_provider_defaults_and_registry_overrides() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        for (id, kind) in [("anthropic", "anthropic"), ("openai", "openai"), ("override", "openai")] {
            db.upsert_provider(&ProviderConfig {
                id: id.into(),
                kind: kind.into(),
                base_url: None,
                api_key: Some("k".into()),
                options: Value::Null,
            })
            .unwrap();
            db.upsert_model(&ModelEntry {
                id: id.into(),
                provider_id: id.into(),
                model: id.into(),
                input_price: 10.0,
                output_price: 20.0,
                cache_read_price: (id == "override").then_some(3.0),
                cache_write_price: (id == "override").then_some(4.0),
                context_window: 0,
                use_cases: String::new(),
                free: false,
                roles: vec![],
                local: false,
                modalities: vec![],
            })
            .unwrap();
        }
        let cost = |provider: &str| db.record_usage(None, None, provider, provider, 100, 100, 100, 100);
        assert!((cost("anthropic") - 0.00435).abs() < 1e-12);
        assert!((cost("openai") - 0.0045).abs() < 1e-12);
        assert!((cost("override") - 0.0037).abs() < 1e-12);
        drop(db);
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

    #[test]
    fn missing_master_key_is_not_autogenerated_when_sealed_rows_exist() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "secured".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: Some("secret".into()),
            options: Value::Null,
        })
        .unwrap();
        drop(db);

        let key_path = dir.join("master.key");
        std::fs::remove_file(&key_path).unwrap();

        let error = Db::open(&dir).err().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("master key missing"), "{message}");
        assert!(message.contains("GROUPER_MASTER_KEY"), "{message}");
        assert!(!key_path.exists(), "must not auto-generate a new master key");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_vapid_key_is_sealed_stable_and_guards_the_master_key() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let first = db.vapid_private_key(|| Ok(vec![5; 32])).unwrap();
        assert_eq!(first, vec![5; 32]);
        let second = db.vapid_private_key(|| panic!("must not make a second key")).unwrap();
        assert_eq!(second, first);
        let stored: String =
            db.c().query_row("SELECT value FROM hub_meta WHERE key = 'vapid_private_key'", [], |r| r.get(0)).unwrap();
        assert!(Secrets::is_sealed(&stored), "{stored}");
        drop(db);

        // A sealed VAPID key alone is enough to refuse making a fresh master key.
        let key_path = dir.join("master.key");
        std::fs::remove_file(&key_path).unwrap();
        let message = format!("{:#}", Db::open(&dir).err().unwrap());
        assert!(message.contains("master key missing"), "{message}");
        assert!(!key_path.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn events_since_filters_by_seq_and_conversation() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.last_event_seq(), 0);
        let e1 = db.add_event("c1", None, EventBody::UserMessage { text: "a".into() });
        let e2 = db.add_event("c2", None, EventBody::UserMessage { text: "b".into() });
        let e3 = db.add_event("c1", None, EventBody::AssistantMessage { text: "c".into() });
        assert_eq!(db.last_event_seq(), e3.seq);

        let all_after_0: Vec<i64> = db.events_since(0, None).into_iter().map(|e| e.seq).collect();
        assert_eq!(all_after_0, vec![e1.seq, e2.seq, e3.seq]);

        let all_after_1: Vec<i64> = db.events_since(e1.seq, None).into_iter().map(|e| e.seq).collect();
        assert_eq!(all_after_1, vec![e2.seq, e3.seq]);

        let c1_after_1: Vec<i64> = db.events_since(e1.seq, Some("c1")).into_iter().map(|e| e.seq).collect();
        assert_eq!(c1_after_1, vec![e3.seq]);

        let c2_after_2: Vec<i64> = db.events_since(e2.seq, Some("c2")).into_iter().map(|e| e.seq).collect();
        assert!(c2_after_2.is_empty());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn conversation_model_override_is_stored_and_cleared() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "picked".into(),
            provider_id: "p".into(),
            model: "picked-model".into(),
            input_price: 1.0,
            output_price: 2.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: vec![],
            local: false,
            modalities: vec!["text".into()],
        })
        .unwrap();
        let id = db.create_conversation("project", "notes").unwrap();
        assert_eq!(db.conversation(&id).unwrap().model, None);
        assert!(db.conversation_model(&id).unwrap().is_none());
        let info = db.set_conversation_model(&id, Some("picked-model")).unwrap();
        assert_eq!(info.model.as_deref(), Some("picked"));
        assert_eq!(db.conversation_model(&id).unwrap().unwrap().id, "picked");
        assert_eq!(db.conversations()[0].model.as_deref(), Some("picked"));
        assert!(db.set_conversation_model(&id, Some("missing")).is_err());
        assert_eq!(db.conversation(&id).unwrap().model.as_deref(), Some("picked"));
        let cleared = db.set_conversation_model(&id, None).unwrap();
        assert_eq!(cleared.model, None);
        assert!(db.set_conversation_model("nope", None).is_err());
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn old_conversations_gain_a_nullable_model_column() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("grouper.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
               id TEXT PRIMARY KEY, mode TEXT NOT NULL, title TEXT NOT NULL, created INTEGER NOT NULL);
             INSERT INTO conversations (id, mode, title, created) VALUES ('c1', 'standard', 'old', 1);",
        )
        .unwrap();
        drop(conn);
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.conversation("c1").unwrap().model, None);
        drop(db);
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.conversation("c1").unwrap().model, None);
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn upsert_model_rejects_negative_and_non_finite_prices() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
        let base = ModelEntry {
            id: "m".into(),
            provider_id: "p".into(),
            model: "m".into(),
            input_price: 1.0,
            output_price: 2.0,
            cache_read_price: Some(0.5),
            cache_write_price: Some(1.5),
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: vec![],
            local: false,
            modalities: vec![],
        };
        db.upsert_model(&base).unwrap();
        for bad in [-0.01, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(db.upsert_model(&ModelEntry { input_price: bad, ..base.clone() }).is_err(), "input {bad}");
            assert!(db.upsert_model(&ModelEntry { output_price: bad, ..base.clone() }).is_err(), "output {bad}");
            assert!(db.upsert_model(&ModelEntry { cache_read_price: Some(bad), ..base.clone() }).is_err(), "cache_read {bad}");
            assert!(db.upsert_model(&ModelEntry { cache_write_price: Some(bad), ..base.clone() }).is_err(), "cache_write {bad}");
        }
        // Zero prices remain valid.
        db.upsert_model(&ModelEntry {
            input_price: 0.0,
            output_price: 0.0,
            cache_read_price: Some(0.0),
            cache_write_price: Some(0.0),
            ..base
        })
        .unwrap();
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
