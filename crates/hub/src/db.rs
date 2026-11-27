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
    /// Google `sub` of the account this device belongs to, once the hub has an owner.
    pub account_sub: Option<String>,
}

/// The app or CLI install a device belongs to: its random `install_id` and the kind of client
/// that registered it (`ios`, `macos` or `cli` for Google sign-in; `node` for a plain device).
#[derive(Clone, Copy, Debug)]
pub struct Install<'a> {
    pub id: &'a str,
    pub kind: &'a str,
}

/// Kinds of [`Install`]: plain (non-admin) devices from `POST /v1/devices` and the device link flow.
pub const NODE_INSTALL: &str = "node";

/// An `install_id` as clients send it: 16 to 64 characters of `[A-Za-z0-9_-]`.
pub fn valid_install_id(id: &str) -> bool {
    (16..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Legacy desktop builds suffixed the node row's `install_id` with `"-node"`.
pub(crate) fn legacy_node_install_id(install_id: &str) -> String {
    let base: String = install_id.chars().take(48).collect();
    format!("{base}-node")
}

/// Canonical `install_id` for grouping: strips the legacy `"-node"` suffix from non-admin node
/// rows when the base itself is a valid `install_id`.
pub(crate) fn canonical_install_id<'a>(install_id: &'a str, install_kind: Option<&str>, admin: bool) -> &'a str {
    if !admin
        && (install_kind == Some(NODE_INSTALL) || install_kind.is_none())
        && let Some(base) = install_id.strip_suffix("-node")
        && valid_install_id(base)
    {
        return base;
    }
    install_id
}

/// A new device would pass the hub's device limit.
#[derive(Debug)]
pub struct DeviceCapReached(pub usize);

impl std::fmt::Display for DeviceCapReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "this hub already has {} devices; revoke one first", self.0)
    }
}

impl std::error::Error for DeviceCapReached {}

/// What [`Db::issue_device`] did.
#[derive(Debug)]
pub struct IssuedDevice {
    pub device: Device,
    /// Shown once; only its hash is kept.
    pub token: String,
    /// An existing device of the same install got this token (its old one is revoked).
    pub reused: bool,
    /// Hashes of the browser sessions that ended with the old token.
    pub ended_sessions: Vec<String>,
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
  modalities TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL DEFAULT 'api', device_id TEXT, can_orchestrate INTEGER NOT NULL DEFAULT 0,
  source TEXT NOT NULL DEFAULT 'manual', hidden INTEGER NOT NULL DEFAULT 0, hidden_reason TEXT,
  missed_syncs INTEGER NOT NULL DEFAULT 0, last_seen INTEGER, display_name TEXT,
  price_source TEXT NOT NULL DEFAULT 'manual', tools TEXT NOT NULL DEFAULT 'unknown');
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

fn device_ref_map(conn: &Connection) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(mut st) = conn.prepare("SELECT id, name FROM devices WHERE revoked = 0") else {
        return map;
    };
    let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else {
        return map;
    };
    for (id, name) in rows.filter_map(Result::ok) {
        map.insert(id.clone(), id.clone());
        map.entry(name.to_ascii_lowercase()).or_insert(id);
    }
    map
}

fn resolve_device_ref(device_map: &HashMap<String, String>, host: &str) -> String {
    let host = host.trim();
    device_map
        .get(host)
        .or_else(|| device_map.get(&host.to_ascii_lowercase()))
        .cloned()
        .unwrap_or_else(|| host.to_string())
}

fn model_row(r: &rusqlite::Row, device_map: &HashMap<String, String>) -> rusqlite::Result<ModelEntry> {
    let roles: String = r.get(10)?;
    let modalities: String = r.get(12)?;
    let stored_kind: String = r.get(13)?;
    let stored_device_id: Option<String> = r.get::<_, Option<String>>(14)?.filter(|s| !s.is_empty());
    let source: String = r.get(15)?;
    let hidden: bool = r.get::<_, i64>(16)? != 0;
    let hidden_reason: Option<String> = if hidden {
        r.get::<_, Option<String>>(17)?.filter(|s| !s.is_empty())
    } else {
        None
    };
    let last_seen: Option<u64> = r.get::<_, Option<i64>>(18)?.map(|v| v as u64);
    let display_name: Option<String> = r.get::<_, Option<String>>(19)?.filter(|s| !s.is_empty());
    let price_source: String = r.get(20)?;
    let tools: String = r.get(21)?;
    let provider_kind: Option<String> = r.get(22)?;
    let provider_options: Option<String> = r.get(23)?;
    let can_orchestrate: bool = r.get::<_, i64>(24)? != 0;
    let is_harness_provider = provider_kind.as_deref() == Some("harness");
    let kind = if is_harness_provider {
        "harness".to_string()
    } else if stored_kind.is_empty() {
        "api".to_string()
    } else {
        stored_kind
    };
    let device_id = stored_device_id.or_else(|| {
        if !is_harness_provider {
            return None;
        }
        let opts: Value = provider_options.as_deref().and_then(|s| serde_json::from_str(s).ok())?;
        let via = opts.get("via_device").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())?;
        Some(resolve_device_ref(device_map, via))
    });
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
        kind,
        device_id,
        can_orchestrate,
        source: if source.is_empty() { "manual".into() } else { source },
        hidden,
        hidden_reason,
        last_seen,
        display_name,
        price_source: if price_source.is_empty() { "manual".into() } else { price_source },
        tools: if tools.is_empty() { "unknown".into() } else { tools },
    })
}
const MODEL_SELECT: &str = "SELECT m.id, m.provider_id, m.model, m.input_price, m.output_price, m.cache_read_price, \
                            m.cache_write_price, m.context_window, m.use_cases, m.free, m.roles, m.local, m.modalities, \
                            m.kind, m.device_id, m.source, m.hidden, m.hidden_reason, m.last_seen, m.display_name, \
                            m.price_source, m.tools, p.kind, p.options, m.can_orchestrate \
                            FROM models m LEFT JOIN providers p ON p.id = m.provider_id";

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
    // Sessions opened by POST /v1/auth/google/return record the Google `sub`. Older rows stay NULL.
    ensure_column(&conn, "sessions", "google_sub", "ALTER TABLE sessions ADD COLUMN google_sub TEXT")?;
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
    ensure_column(&conn, "models", "kind", "ALTER TABLE models ADD COLUMN kind TEXT NOT NULL DEFAULT 'api'")?;
    ensure_column(&conn, "models", "device_id", "ALTER TABLE models ADD COLUMN device_id TEXT")?;
    ensure_column(&conn, "models", "source", "ALTER TABLE models ADD COLUMN source TEXT NOT NULL DEFAULT 'manual'")?;
    ensure_column(&conn, "models", "hidden", "ALTER TABLE models ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(&conn, "models", "hidden_reason", "ALTER TABLE models ADD COLUMN hidden_reason TEXT")?;
    ensure_column(&conn, "models", "missed_syncs", "ALTER TABLE models ADD COLUMN missed_syncs INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(&conn, "models", "last_seen", "ALTER TABLE models ADD COLUMN last_seen INTEGER")?;
    ensure_column(&conn, "models", "display_name", "ALTER TABLE models ADD COLUMN display_name TEXT")?;
    ensure_column(&conn, "models", "price_source", "ALTER TABLE models ADD COLUMN price_source TEXT NOT NULL DEFAULT 'manual'")?;
    ensure_column(&conn, "models", "tools", "ALTER TABLE models ADD COLUMN tools TEXT NOT NULL DEFAULT 'unknown'")?;
    ensure_column(&conn, "models", "can_orchestrate", "ALTER TABLE models ADD COLUMN can_orchestrate INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(&conn, "conversations", "model", "ALTER TABLE conversations ADD COLUMN model TEXT")?;
    ensure_column(&conn, "tools", "available", "ALTER TABLE tools ADD COLUMN available INTEGER NOT NULL DEFAULT 1")?;
    ensure_column(&conn, "tools", "input_schema", "ALTER TABLE tools ADD COLUMN input_schema TEXT NOT NULL DEFAULT '{}'")?;
    ensure_column(&conn, "devices", "folders", "ALTER TABLE devices ADD COLUMN folders TEXT NOT NULL DEFAULT '[]'")?;
    // Which app or CLI install a device belongs to, so signing in again from it reuses the device.
    ensure_column(&conn, "devices", "install_id", "ALTER TABLE devices ADD COLUMN install_id TEXT")?;
    ensure_column(&conn, "devices", "install_kind", "ALTER TABLE devices ADD COLUMN install_kind TEXT")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS devices_install ON devices (install_id);")?;
    // Link devices to the owning Google account (`hub_owner.sub`), and track which device is the hub.
    ensure_column(&conn, "devices", "account_sub", "ALTER TABLE devices ADD COLUMN account_sub TEXT")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS devices_account ON devices (account_sub);
         UPDATE devices SET account_sub = (SELECT sub FROM hub_owner WHERE id = 1)
           WHERE account_sub IS NULL AND EXISTS (SELECT 1 FROM hub_owner WHERE id = 1);
         CREATE TABLE IF NOT EXISTS hub_state (
           id INTEGER PRIMARY KEY CHECK (id = 1),
           hub_device_id TEXT NOT NULL REFERENCES devices(id),
           epoch INTEGER NOT NULL CHECK (epoch >= 1)
         );",
    )?;
    conn.execute_batch(crate::link::SCHEMA)?;
    ensure_column(&conn, "push_subscriptions", "session_id", "ALTER TABLE push_subscriptions ADD COLUMN session_id TEXT")?;
    conn.execute_batch("CREATE INDEX IF NOT EXISTS push_subscriptions_session ON push_subscriptions (session_id);")?;
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

/// Default device name for the machine hosting a fresh hub: its hostname, sanitized, with a safe fallback.
fn machine_device_name() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default();
    let cleaned: String = raw.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("hub") {
        "Grouper host".into()
    } else {
        cleaned.to_string()
    }
}

/// Pick the best live device row to act as the hub for `install_id` (or its legacy `<base>-node`
/// companion id), preferring an exact-match non-admin node row, then any non-admin row, then an admin row.
fn find_install_hub_candidate(
    tx: &rusqlite::Transaction<'_>,
    install_id: &str,
    legacy_node: &str,
) -> Result<Option<(String, i64)>> {
    Ok(tx
        .query_row(
            "SELECT id, admin FROM devices
             WHERE (install_id = ?1 OR (install_id = ?2 AND admin = 0)) AND revoked = 0
             ORDER BY
               CASE
                 WHEN install_id = ?1 AND admin = 0 AND install_kind = ?3 THEN 0
                 WHEN install_id = ?2 AND admin = 0 AND install_kind = ?3 THEN 1
                 WHEN admin = 0 THEN 2
                 ELSE 3
               END,
               created DESC
             LIMIT 1",
            params![install_id, legacy_node, NODE_INSTALL],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
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

    /// Ensure this hub has a device row for the host machine and a `(hub_device_id, epoch)` assignment.
    /// On a fresh hub, registers a non-admin device with the OS platform and machine hostname and sets
    /// `hub_device_id` to it at `epoch = 1`.
    pub fn ensure_hub_device(&self) -> Result<(String, u64)> {
        self.ensure_hub_device_for_install(None)
    }

    /// Ensure this hub has a `(hub_device_id, epoch)` assignment, optionally bound to an embedding
    /// app's `install_id`.
    ///
    /// When `install_id` is `Some`:
    /// - If `hub_state` is absent, adopts an existing live device row for `install_id` (preferring a
    ///   non-admin node row, including legacy `<install_id>-node` rows) or inserts one tagged with
    ///   `(install_id, NODE_INSTALL)` so the hub and its local node share one row.
    /// - If `hub_state` already points to an unattached synthetic hub row (`install_id IS NULL`),
    ///   adopts the install's existing node row (retiring the unused synthetic row) or attaches
    ///   `install_id` to the current hub row without bumping `epoch`.
    /// - If `hub_state` already points to a row of the same install (or a different explicit install),
    ///   reuses it without creating a duplicate row or bumping `epoch`.
    pub fn ensure_hub_device_for_install(&self, install_id: Option<&str>) -> Result<(String, u64)> {
        let install_id = match install_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => {
                if !valid_install_id(id) {
                    bail!("install_id must be 16 to 64 characters of letters, digits, - and _");
                }
                Some(id)
            }
            None => None,
        };

        let mut c = self.c();
        if install_id.is_none()
            && let Some((id, epoch)) = c
                .query_row("SELECT hub_device_id, epoch FROM hub_state WHERE id = 1", [], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
                })
                .optional()?
        {
            return Ok((id, epoch));
        }

        let tx = c.transaction()?;
        let owner_sub: Option<String> = tx
            .query_row("SELECT sub FROM hub_owner WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        let existing_state: Option<(String, u64)> = tx
            .query_row("SELECT hub_device_id, epoch FROM hub_state WHERE id = 1", [], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })
            .optional()?;

        if let Some((cur_id, cur_epoch)) = existing_state {
            let Some(inst) = install_id else {
                return Ok((cur_id, cur_epoch));
            };
            let legacy_node = legacy_node_install_id(inst);
            type HubDeviceRow = (Option<String>, Option<String>, i64, Option<i64>, i64);
            let cur_row: Option<HubDeviceRow> = tx
                .query_row(
                    "SELECT install_id, install_kind, admin, last_seen, revoked FROM devices WHERE id = ?1",
                    [&cur_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;

            if let Some((cur_inst, cur_kind, cur_admin, cur_last_seen, cur_revoked)) = cur_row
                && cur_revoked == 0
            {
                match cur_inst.as_deref().filter(|s| !s.is_empty()) {
                    Some(id) if id == inst => {
                        return Ok((cur_id, cur_epoch));
                    }
                    Some(id) if id == legacy_node => {
                        tx.execute(
                            "UPDATE devices SET install_id = ?1, account_sub = COALESCE(account_sub, ?2) WHERE id = ?3",
                            params![inst, owner_sub, cur_id],
                        )?;
                        tx.commit()?;
                        return Ok((cur_id, cur_epoch));
                    }
                    Some(_) => {
                        return Ok((cur_id, cur_epoch));
                    }
                    None => {
                        if let Some((cand_id, cand_admin)) = find_install_hub_candidate(&tx, inst, &legacy_node)?
                            && cur_admin == 0
                            && cur_kind.is_none()
                            && cur_last_seen.is_none()
                            && cand_admin == 0
                        {
                            tx.execute(
                                "UPDATE devices SET install_id = ?1, account_sub = COALESCE(account_sub, ?2) WHERE id = ?3",
                                params![inst, owner_sub, cand_id],
                            )?;
                            tx.execute("UPDATE hub_state SET hub_device_id = ?1 WHERE id = 1", params![cand_id])?;
                            tx.execute("UPDATE devices SET revoked = 1 WHERE id = ?1", [&cur_id])?;
                            tx.commit()?;
                            return Ok((cand_id, cur_epoch));
                        }
                        if cur_admin == 0 && cur_kind.is_none() {
                            tx.execute(
                                "UPDATE devices SET install_id = ?1, install_kind = ?2, account_sub = COALESCE(account_sub, ?3) WHERE id = ?4",
                                params![inst, NODE_INSTALL, owner_sub, cur_id],
                            )?;
                        } else {
                            tx.execute(
                                "UPDATE devices SET install_id = ?1, account_sub = COALESCE(account_sub, ?2) WHERE id = ?3",
                                params![inst, owner_sub, cur_id],
                            )?;
                        }
                        tx.commit()?;
                        return Ok((cur_id, cur_epoch));
                    }
                }
            }
            return Ok((cur_id, cur_epoch));
        }

        if let Some(inst) = install_id {
            let legacy_node = legacy_node_install_id(inst);
            if let Some((existing_id, _)) = find_install_hub_candidate(&tx, inst, &legacy_node)? {
                tx.execute(
                    "UPDATE devices SET install_id = ?1, account_sub = COALESCE(account_sub, ?2) WHERE id = ?3",
                    params![inst, owner_sub, existing_id],
                )?;
                tx.execute("INSERT INTO hub_state (id, hub_device_id, epoch) VALUES (1, ?1, 1)", params![existing_id])?;
                tx.commit()?;
                return Ok((existing_id, 1));
            }
        }

        let id = random_hex(6);
        let secret = random_hex(32);
        let name = machine_device_name();
        let platform = std::env::consts::OS;
        let install_kind = install_id.map(|_| NODE_INSTALL);
        tx.execute(
            "INSERT INTO devices (id, name, platform, secret_hash, created, admin, install_id, install_kind, account_sub)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8)",
            params![id, name, platform, hash(&secret), ts(), install_id, install_kind, owner_sub],
        )?;
        tx.execute("INSERT INTO hub_state (id, hub_device_id, epoch) VALUES (1, ?1, 1)", params![id])?;
        tx.commit()?;
        Ok((id, 1))
    }

    /// The `install_id` recorded on a device row, if any.
    pub fn device_install_id(&self, id: &str) -> Option<String> {
        self.c()
            .query_row("SELECT install_id FROM devices WHERE id = ?1", [id], |r| r.get::<_, Option<String>>(0))
            .optional()
            .ok()
            .flatten()
            .flatten()
            .filter(|s| !s.is_empty())
    }

    /// The device id currently acting as the hub (registering this machine on first use).
    pub fn hub_device_id(&self) -> String {
        self.ensure_hub_device().expect("hub device registration").0
    }

    /// Hub assignment counter: starts at 1 and rises by 1 whenever `hub_device_id` changes.
    pub fn epoch(&self) -> u64 {
        self.ensure_hub_device().expect("hub device registration").1
    }

    /// Reassign `hub_device_id` to an active device of the account. Increments `epoch` when
    /// `hub_device_id` changes; assigning the current device is a no-op and returns the current `epoch`.
    pub fn set_hub_device_id(&self, device_id: &str) -> Result<u64> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let Some((revoked, dev_account)): Option<(i64, Option<String>)> = tx
            .query_row("SELECT revoked, account_sub FROM devices WHERE id = ?1", [device_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
        else {
            bail!("no such device");
        };
        if revoked != 0 {
            bail!("cannot assign a revoked device as the hub");
        }
        let owner_sub: Option<String> = tx
            .query_row("SELECT sub FROM hub_owner WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        if let Some(sub) = &owner_sub {
            match &dev_account {
                Some(existing) if existing != sub => bail!("device belongs to a different account"),
                None => {
                    tx.execute("UPDATE devices SET account_sub = ?1 WHERE id = ?2", params![sub, device_id])?;
                }
                _ => {}
            }
        }
        let current: Option<(String, i64)> = tx
            .query_row("SELECT hub_device_id, epoch FROM hub_state WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let epoch = match current {
            Some((cur_id, cur_epoch)) if cur_id == device_id => cur_epoch as u64,
            Some((_, cur_epoch)) => {
                let next = (cur_epoch as u64) + 1;
                tx.execute(
                    "UPDATE hub_state SET hub_device_id = ?1, epoch = ?2 WHERE id = 1",
                    params![device_id, next as i64],
                )?;
                next
            }
            None => {
                tx.execute("INSERT INTO hub_state (id, hub_device_id, epoch) VALUES (1, ?1, 1)", params![device_id])?;
                1
            }
        };
        tx.commit()?;
        Ok(epoch)
    }

    /// Hex SHA-256 of the hub's identity key, if one exists. No hub identity key exists yet
    /// (arrives with LAN TLS in package 13), so this returns `None` and `/v1/hub` omits `key_sha256`.
    pub fn identity_key_sha256(&self) -> Option<String> {
        None
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
        let issued = self.issue_device(name, platform, admin, None, None)?;
        Ok((issued.device, issued.token))
    }

    /// A token for a new device, or, when `install` names a live device of the same install and
    /// kind (and admin flag), a fresh token for that device: its old token stops working and its
    /// browser sessions and push subscriptions go, as when it is revoked. `max_live` caps how many
    /// live devices a new one may join (a reused device doesn't count). The caller closes the old
    /// connection and tells live sockets ([`crate::server::Hub::issue_device`]).
    pub fn issue_device(
        &self,
        name: &str,
        platform: &str,
        admin: bool,
        install: Option<Install<'_>>,
        max_live: Option<usize>,
    ) -> Result<IssuedDevice> {
        let name = name.trim();
        if name.is_empty() {
            bail!("device name is empty");
        }
        if name.eq_ignore_ascii_case("hub") {
            bail!("\"hub\" is reserved for the hub itself");
        }
        if let Some(i) = &install
            && !valid_install_id(i.id)
        {
            bail!("install_id must be 16 to 64 characters of letters, digits, - and _");
        }
        let secret = random_hex(32);
        let platform = platform.trim().to_lowercase();
        let mut c = self.c();
        let tx = c.transaction()?;
        let owner_sub: Option<String> = tx
            .query_row("SELECT sub FROM hub_owner WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        let existing: Option<String> = match &install {
            Some(i) => tx
                .query_row(
                    "SELECT id FROM devices WHERE install_id = ?1 AND install_kind = ?2 AND admin = ?3 AND revoked = 0
                     ORDER BY created DESC LIMIT 1",
                    params![i.id, i.kind, admin],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
        let (id, account_sub, ended_sessions) = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE devices SET secret_hash = ?1, name = ?2, platform = ?3,
                       account_sub = COALESCE(?4, account_sub) WHERE id = ?5",
                    params![hash(&secret), name, platform, owner_sub, id],
                )?;
                let account_sub: Option<String> =
                    tx.query_row("SELECT account_sub FROM devices WHERE id = ?1", [&id], |r| r.get(0))?;
                let sessions = {
                    let mut st = tx.prepare("SELECT id_hash FROM sessions WHERE device_id = ?1")?;
                    st.query_map([&id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
                };
                tx.execute("DELETE FROM sessions WHERE device_id = ?1", [&id])?;
                tx.execute("DELETE FROM push_subscriptions WHERE device_id = ?1", [&id])?;
                (id, account_sub, Some(sessions))
            }
            None => {
                if let Some(max) = max_live {
                    let live: i64 = tx.query_row("SELECT COUNT(*) FROM devices WHERE revoked = 0", [], |r| r.get(0))?;
                    if live as usize >= max {
                        return Err(DeviceCapReached(max).into());
                    }
                }
                let id = random_hex(6);
                tx.execute(
                    "INSERT INTO devices (id, name, platform, secret_hash, created, admin, install_id, install_kind, account_sub)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        id,
                        name,
                        platform,
                        hash(&secret),
                        ts(),
                        admin,
                        install.map(|i| i.id),
                        install.map(|i| i.kind),
                        owner_sub,
                    ],
                )?;
                (id, owner_sub, None)
            }
        };
        tx.commit()?;
        Ok(IssuedDevice {
            token: format!("{id}.{secret}"),
            device: Device {
                id,
                name: name.into(),
                platform,
                revoked: false,
                last_seen: None,
                admin,
                account_sub,
            },
            reused: ended_sessions.is_some(),
            ended_sessions: ended_sessions.unwrap_or_default(),
        })
    }

    /// The name of the live device an install would reuse, if any (see [`Db::issue_device`]).
    pub fn install_device_name(&self, install: Install<'_>, admin: bool) -> Option<String> {
        self.c()
            .query_row(
                "SELECT name FROM devices WHERE install_id = ?1 AND install_kind = ?2 AND admin = ?3 AND revoked = 0
                 ORDER BY created DESC LIMIT 1",
                params![install.id, install.kind, admin],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Encrypt a short secret with the master key (as provider keys are).
    pub(crate) fn seal(&self, plaintext: &str) -> Result<String> {
        self.secrets.seal(plaintext)
    }

    /// Decrypt what [`Db::seal`] made.
    pub(crate) fn unseal(&self, sealed: &str) -> Result<String> {
        self.secrets.open(sealed)
    }

    /// Grant or remove admin rights for a device's token.
    pub fn set_admin(&self, id: &str, admin: bool) -> Result<()> {
        let c = self.c();
        if c.execute("UPDATE devices SET admin = ?1 WHERE id = ?2", params![admin, id])? == 0 {
            bail!("no such device");
        }
        if !admin {
            let _ = c.execute("DELETE FROM push_subscriptions WHERE device_id = ?1", [id]);
        }
        Ok(())
    }

    pub fn revoke_device(&self, id: &str) -> Result<()> {
        let c = self.c();
        if c.execute("UPDATE devices SET revoked = 1 WHERE id = ?1", [id])? == 0 {
            bail!("no such device");
        }
        let _ = c.execute("DELETE FROM push_subscriptions WHERE device_id = ?1", [id]);
        Ok(())
    }

    pub fn devices(&self) -> Vec<Device> {
        let c = self.c();
        let mut st = c
            .prepare("SELECT id, name, platform, revoked, last_seen, admin, account_sub FROM devices ORDER BY created")
            .unwrap();
        st.query_map([], |r| {
            Ok(Device {
                id: r.get(0)?,
                name: r.get(1)?,
                platform: r.get(2)?,
                revoked: r.get::<_, i64>(3)? != 0,
                last_seen: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                admin: r.get::<_, i64>(5)? != 0,
                account_sub: r.get(6)?,
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
        if !["anthropic", "openai", "scripted", "harness"].contains(&p.kind.as_str()) {
            bail!("kind must be anthropic, openai, scripted or harness");
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
        let Some(provider) = self.provider(&m.provider_id)? else {
            bail!("no provider {}; add it first", m.provider_id);
        };
        core_registry::cost::check_price("input_price", m.input_price)?;
        core_registry::cost::check_price("output_price", m.output_price)?;
        if let Some(p) = m.cache_read_price {
            core_registry::cost::check_price("cache_read_price", p)?;
        }
        if let Some(p) = m.cache_write_price {
            core_registry::cost::check_price("cache_write_price", p)?;
        }
        let kind = if provider.kind == "harness" {
            "harness"
        } else {
            match m.kind.trim() {
                "" | "api" => "api",
                "harness" => "harness",
                other => bail!("kind must be api or harness; got {other:?}"),
            }
        };
        let source = match m.source.trim() {
            "" | "manual" => "manual",
            "auto" => "auto",
            other => bail!("source must be manual or auto; got {other:?}"),
        };
        let hidden_reason = if !m.hidden {
            None
        } else {
            match m.hidden_reason.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some("gone") => Some("gone"),
                Some("user") => Some("user"),
                Some(other) => bail!("hidden_reason must be gone or user; got {other:?}"),
            }
        };
        let tools = match m.tools.trim() {
            "" | "unknown" => "unknown",
            "yes" => "yes",
            "no" => "no",
            other => bail!("tools must be yes, no or unknown; got {other:?}"),
        };
        let price_source = match m.price_source.trim() {
            "" => "manual".to_string(),
            other => other.to_ascii_lowercase(),
        };
        let display_name = m.display_name.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let c = self.c();
        let explicit_device_id = m.device_id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let device_id = explicit_device_id.or_else(|| {
            if provider.kind != "harness" {
                return None;
            }
            let via = provider.via_device()?;
            let map = device_ref_map(&c);
            Some(resolve_device_ref(&map, via))
        });
        c.execute(
            "INSERT INTO models (id, provider_id, model, input_price, output_price, cache_read_price,
               cache_write_price, context_window, use_cases, free, roles, local, modalities,
               kind, device_id, source, hidden, hidden_reason, last_seen, display_name, price_source, tools, can_orchestrate)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
             ON CONFLICT (id) DO UPDATE SET
               provider_id = excluded.provider_id,
               model = excluded.model,
               input_price = excluded.input_price,
               output_price = excluded.output_price,
               cache_read_price = excluded.cache_read_price,
               cache_write_price = excluded.cache_write_price,
               context_window = excluded.context_window,
               use_cases = excluded.use_cases,
               free = excluded.free,
               roles = excluded.roles,
               local = excluded.local,
               modalities = excluded.modalities,
               kind = excluded.kind,
               device_id = excluded.device_id,
               source = excluded.source,
               hidden = excluded.hidden,
               hidden_reason = excluded.hidden_reason,
               missed_syncs = CASE WHEN excluded.hidden = 0 THEN 0 ELSE models.missed_syncs END,
               last_seen = excluded.last_seen,
               display_name = excluded.display_name,
               price_source = excluded.price_source,
               tools = excluded.tools,
               can_orchestrate = excluded.can_orchestrate",
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
                m.modalities.join(","),
                kind,
                device_id,
                source,
                m.hidden,
                hidden_reason,
                m.last_seen.map(|v| v as i64),
                display_name,
                price_source,
                tools,
                m.can_orchestrate,
            ],
        )?;
        Ok(())
    }

    pub fn set_model_hidden(&self, id: &str, hidden: bool, reason: Option<&str>) -> Result<()> {
        let reason = if !hidden {
            None
        } else {
            match reason.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some("gone") => Some("gone"),
                Some("user") => Some("user"),
                Some(other) => bail!("hidden_reason must be gone or user; got {other:?}"),
            }
        };
        let n = self.c().execute(
            "UPDATE models SET hidden = ?1, hidden_reason = ?2,
               missed_syncs = CASE WHEN ?1 = 0 THEN 0 ELSE missed_syncs END
             WHERE id = ?3",
            params![hidden, reason, id],
        )?;
        if n == 0 {
            bail!("no such model");
        }
        Ok(())
    }

    pub fn delete_model(&self, id: &str) -> Result<()> {
        if self.c().execute("DELETE FROM models WHERE id = ?1", [id])? == 0 {
            bail!("no such model");
        }
        Ok(())
    }

    /// Every model row, including hidden rows (so history, usage and outcome stats still resolve them).
    pub fn models(&self) -> Vec<ModelEntry> {
        let c = self.c();
        let device_map = device_ref_map(&c);
        let mut st = c.prepare(&format!("{MODEL_SELECT} ORDER BY m.id")).unwrap();
        st.query_map([], |r| model_row(r, &device_map)).unwrap().filter_map(Result::ok).collect()
    }

    /// Visible models only (excluding hidden rows).
    pub fn visible_models(&self) -> Vec<ModelEntry> {
        self.models().into_iter().filter(|m| !m.hidden).collect()
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
    /// `Ok(None)` means it uses the role default. An error means the stored id is no longer
    /// registered or is hidden.
    pub fn conversation_model(&self, conversation_id: &str) -> Result<Option<ModelEntry>> {
        let Some(info) = self.conversation(conversation_id) else { bail!("no such conversation") };
        let Some(key) = info.model else { return Ok(None) };
        let Some(model) = self.find_model(&key) else {
            bail!("conversation {conversation_id} is set to model {key}, which is not in the registry");
        };
        if model.hidden {
            if model.hidden_reason.as_deref() == Some("gone") {
                bail!("conversation {conversation_id} is set to model {key}, which the provider no longer lists");
            }
            bail!("conversation {conversation_id} is set to model {key}, which is hidden");
        }
        Ok(Some(model))
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
                if found.hidden {
                    bail!("model {} is hidden and cannot be selected", found.id);
                }
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
            ..Default::default()
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
                ..Default::default()
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
        // The key may live DPAPI-protected (Windows) or in the Keychain (macOS), not as a file.
        crate::secrets::remove_master_key(&dir).unwrap();

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
        // The key may live DPAPI-protected (Windows) or in the Keychain (macOS), not as a file.
        crate::secrets::remove_master_key(&dir).unwrap();
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
            ..Default::default()
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
            ..Default::default()
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

    #[test]
    fn fresh_hub_registers_host_device_and_epoch_rises_on_reassignment() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let hub_dev_id = db.hub_device_id();
        assert!(!hub_dev_id.is_empty());
        assert_eq!(db.epoch(), 1);
        assert_eq!(db.identity_key_sha256(), None);

        let devices = db.devices();
        assert_eq!(devices.len(), 1);
        let host = &devices[0];
        assert_eq!(host.id, hub_dev_id);
        assert_eq!(host.platform, std::env::consts::OS);
        assert_eq!(host.name, machine_device_name());
        assert!(!host.admin);
        assert_eq!(host.account_sub, None);

        // Reopening preserves the same hub_device_id and epoch without duplicating the host device.
        drop(db);
        let db = Db::open(&dir).unwrap();
        assert_eq!(db.hub_device_id(), hub_dev_id);
        assert_eq!(db.epoch(), 1);
        assert_eq!(db.devices().len(), 1);

        // Assigning the same device is a no-op; assigning another rises epoch by 1 each time.
        assert_eq!(db.set_hub_device_id(&hub_dev_id).unwrap(), 1);
        let (mac, _) = db.add_device("MacBook", "macos", false).unwrap();
        assert_eq!(db.set_hub_device_id(&mac.id).unwrap(), 2);
        assert_eq!(db.hub_device_id(), mac.id);
        assert_eq!(db.epoch(), 2);
        assert_eq!(db.set_hub_device_id(&hub_dev_id).unwrap(), 3);
        assert_eq!(db.hub_device_id(), hub_dev_id);
        assert_eq!(db.epoch(), 3);

        // Non-existent and revoked devices cannot become the hub.
        assert!(db.set_hub_device_id("missing").is_err());
        db.revoke_device(&mac.id).unwrap();
        assert!(db.set_hub_device_id(&mac.id).is_err());
        assert_eq!(db.epoch(), 3);

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn migration_links_existing_devices_to_owner_and_tracks_new_devices() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("grouper.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE devices (
               id TEXT PRIMARY KEY, name TEXT NOT NULL, platform TEXT NOT NULL,
               secret_hash TEXT NOT NULL, created INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0,
               last_seen INTEGER, admin INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE hub_owner (
               id INTEGER PRIMARY KEY CHECK (id = 1), provider TEXT NOT NULL, sub TEXT NOT NULL,
               email TEXT NOT NULL, name TEXT NOT NULL DEFAULT '', claimed_at INTEGER NOT NULL);
             INSERT INTO devices (id, name, platform, secret_hash, created, admin)
               VALUES ('d1', 'Old PC', 'windows', 'h', 1, 0);
             INSERT INTO hub_owner (id, provider, sub, email, name, claimed_at)
               VALUES (1, 'google', 'sub-123', 'owner@example.com', 'Owner', 2);",
        )
        .unwrap();
        drop(conn);

        let db = Db::open(&dir).unwrap();
        let devs = db.devices();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].account_sub.as_deref(), Some("sub-123"));

        // Registering the hub device and adding another device both link to the owner's sub.
        let hub_dev_id = db.hub_device_id();
        let (phone, _) = db.add_device("Phone", "ios", true).unwrap();
        assert_eq!(phone.account_sub.as_deref(), Some("sub-123"));
        for d in db.devices() {
            assert_eq!(d.account_sub.as_deref(), Some("sub-123"), "device {}", d.id);
        }
        assert!(db.devices().iter().any(|d| d.id == hub_dev_id));

        // Resetting the owner unlinks devices cleanly; claiming again re-links them.
        db.reset_owner().unwrap();
        assert!(db.devices().iter().all(|d| d.account_sub.is_none()));
        db.set_owner_if_none(&crate::google_auth::Owner {
            provider: "google".into(),
            sub: "sub-456".into(),
            email: "new@example.com".into(),
            name: "New".into(),
            claimed_at: 3,
        })
        .unwrap();
        assert!(db.devices().iter().all(|d| d.account_sub.as_deref() == Some("sub-456")));

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ensure_hub_device_for_install_adopts_existing_and_reuses_on_restart() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let install_id = "pc_install_1234567890abcdef";

        // Invalid install_id is rejected.
        assert!(db.ensure_hub_device_for_install(Some("short")).is_err());

        // Fresh hub creates a single non-admin node row tagged with install_id.
        let (hub_dev_id, epoch) = db.ensure_hub_device_for_install(Some(install_id)).unwrap();
        assert_eq!(epoch, 1);
        assert_eq!(db.device_install_id(&hub_dev_id).as_deref(), Some(install_id));
        assert_eq!(db.devices().len(), 1);

        // Issuing the local node for the same install reuses the hub row instead of creating a second row.
        let node = db
            .issue_device("Oscar-PC", "windows", false, Some(Install { id: install_id, kind: NODE_INSTALL }), None)
            .unwrap();
        assert!(node.reused);
        assert_eq!(node.device.id, hub_dev_id);
        assert_eq!(db.devices().len(), 1);

        // Issuing the admin UI row for the same install keeps two rows total (node/hub + admin).
        let admin = db
            .issue_device("Oscar-PC", "windows", true, Some(Install { id: install_id, kind: "windows" }), None)
            .unwrap();
        assert_ne!(admin.device.id, hub_dev_id);
        assert_eq!(db.devices().len(), 2);

        // Reopening and ensuring with the same install_id reuses the row and keeps epoch = 1.
        drop(db);
        let db = Db::open(&dir).unwrap();
        let (restarted_id, restarted_epoch) = db.ensure_hub_device_for_install(Some(install_id)).unwrap();
        assert_eq!(restarted_id, hub_dev_id);
        assert_eq!(restarted_epoch, 1);
        assert_eq!(db.devices().len(), 2);
        drop(db);
        std::fs::remove_dir_all(dir).ok();

        // Pre-existing node row before hub_state exists is adopted rather than creating a separate hub row.
        let dir2 = tmp();
        let db2 = Db::open(&dir2).unwrap();
        let existing_node = db2
            .issue_device("Existing PC", "windows", false, Some(Install { id: install_id, kind: NODE_INSTALL }), None)
            .unwrap();
        let (adopted_id, adopted_epoch) = db2.ensure_hub_device_for_install(Some(install_id)).unwrap();
        assert_eq!(adopted_id, existing_node.device.id);
        assert_eq!(adopted_epoch, 1);
        assert_eq!(db2.devices().len(), 1);
        drop(db2);
        std::fs::remove_dir_all(dir2).ok();

        // Upgrading a legacy DB with an unattached hub row + `<install_id>-node` row adopts the node row
        // and retires the unused synthetic hub row without bumping epoch.
        let dir3 = tmp();
        let db3 = Db::open(&dir3).unwrap();
        let (orphan_id, _) = db3.ensure_hub_device().unwrap();
        let legacy_id = legacy_node_install_id(install_id);
        let legacy_node = db3
            .issue_device("Legacy PC", "windows", false, Some(Install { id: &legacy_id, kind: NODE_INSTALL }), None)
            .unwrap();
        let (upgraded_id, upgraded_epoch) = db3.ensure_hub_device_for_install(Some(install_id)).unwrap();
        assert_eq!(upgraded_id, legacy_node.device.id);
        assert_eq!(upgraded_epoch, 1);
        assert_eq!(db3.device_install_id(&upgraded_id).as_deref(), Some(install_id));
        let orphan = db3.devices().into_iter().find(|d| d.id == orphan_id).unwrap();
        assert!(orphan.revoked, "unused synthetic hub row should be revoked on adoption");
        drop(db3);
        std::fs::remove_dir_all(dir3).ok();
    }

    #[test]
    fn old_models_table_migrates_with_m1_columns_and_defaults() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("grouper.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE providers (
               id TEXT PRIMARY KEY, kind TEXT NOT NULL, base_url TEXT, api_key TEXT, options TEXT NOT NULL DEFAULT '{}');
             CREATE TABLE models (
               id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, model TEXT NOT NULL,
               input_price REAL NOT NULL DEFAULT 0, output_price REAL NOT NULL DEFAULT 0,
               context_window INTEGER NOT NULL DEFAULT 0, use_cases TEXT NOT NULL DEFAULT '',
               free INTEGER NOT NULL DEFAULT 0, roles TEXT NOT NULL DEFAULT '');
             INSERT INTO providers (id, kind) VALUES ('p1', 'openai');
             INSERT INTO models (id, provider_id, model, input_price, output_price, roles)
               VALUES ('legacy-gpt', 'p1', 'gpt-4o', 2.5, 10.0, 'orchestrator');",
        )
        .unwrap();
        drop(conn);

        // First open migrates; second open is idempotent.
        drop(Db::open(&dir).unwrap());
        let db = Db::open(&dir).unwrap();
        for column in [
            "cache_read_price",
            "cache_write_price",
            "modalities",
            "local",
            "kind",
            "device_id",
            "source",
            "hidden",
            "hidden_reason",
            "missed_syncs",
            "last_seen",
            "display_name",
            "price_source",
            "tools",
            "can_orchestrate",
        ] {
            assert!(
                db.c()
                    .prepare("SELECT 1 FROM pragma_table_info('models') WHERE name = ?1")
                    .unwrap()
                    .exists([column])
                    .unwrap(),
                "missing column {column}"
            );
        }

        let models = db.models();
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "legacy-gpt");
        assert_eq!(m.kind, "api");
        assert_eq!(m.device_id, None);
        assert_eq!(m.source, "manual");
        assert!(!m.hidden);
        assert_eq!(m.hidden_reason, None);
        assert_eq!(m.last_seen, None);
        assert_eq!(m.display_name, None);
        assert_eq!(m.price_source, "manual");
        assert_eq!(m.tools, "unknown");
        assert_eq!(db.default_model("orchestrator").unwrap().id, "legacy-gpt");

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn hidden_models_are_skipped_by_defaults_and_routing_but_resolve_for_history_and_usage() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: Some("k".into()),
            options: Value::Null,
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "old-default".into(),
            provider_id: "p".into(),
            model: "old-default-model".into(),
            input_price: 10.0,
            output_price: 20.0,
            free: false,
            roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
            source: "auto".into(),
            last_seen: Some(1000),
            display_name: Some("Old Default".into()),
            price_source: "table".into(),
            tools: "yes".into(),
            ..Default::default()
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "live-backup".into(),
            provider_id: "p".into(),
            model: "live-backup-model".into(),
            input_price: 1.0,
            output_price: 2.0,
            roles: vec![],
            ..Default::default()
        })
        .unwrap();

        let conv = db.create_conversation("standard", "pinned chat").unwrap();
        db.set_conversation_model(&conv, Some("old-default")).unwrap();
        let thread = db.create_thread(&conv, "Past work", "hub", "old-default", None, "brief").unwrap();

        // Hide the old default because the provider dropped it.
        db.set_model_hidden("old-default", true, Some("gone")).unwrap();

        // Defaults, visible listing, and routing skip the hidden row.
        assert_eq!(db.default_model("orchestrator").unwrap().id, "live-backup");
        assert_eq!(db.default_model("standard").unwrap().id, "live-backup");
        assert_eq!(db.default_model("thread").unwrap().id, "live-backup");
        assert_eq!(db.route_model(None, Some("code"), &[]).unwrap().model.id, "live-backup");
        assert!(db.route_model(Some("old-default"), None, &[]).is_err());
        assert_eq!(db.visible_models().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["live-backup"]);

        // History, usage, and outcome resolution still find the hidden row.
        let resolved = db.find_model("old-default").unwrap();
        assert!(resolved.hidden);
        assert_eq!(resolved.hidden_reason.as_deref(), Some("gone"));
        assert_eq!(db.models().len(), 2);
        let cost = db.record_usage(Some(&conv), Some(&thread.id), "p", "old-default-model", 100_000, 50_000, 0, 0);
        assert!((cost - 2.0).abs() < 1e-9, "usage still prices hidden row: {cost}");
        assert!(db.finish_thread(&thread.id, "done", "finished"));
        assert!(db.record_outcome(&thread.id));
        assert_eq!(db.conversation(&conv).unwrap().model.as_deref(), Some("old-default"));
        let conv_err = db.conversation_model(&conv).unwrap_err().to_string();
        assert!(conv_err.contains("no longer lists"), "{conv_err}");
        assert!(db.set_conversation_model(&conv, Some("old-default")).is_err());

        // Unhiding restores it.
        db.set_model_hidden("old-default", false, None).unwrap();
        assert_eq!(db.default_model("orchestrator").unwrap().id, "old-default");
        assert_eq!(db.conversation_model(&conv).unwrap().unwrap().id, "old-default");

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn harness_models_compute_kind_and_device_id_from_provider() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: format!("claude-code@{}", pc.id),
            kind: "harness".into(),
            base_url: None,
            api_key: None,
            options: serde_json::json!({"harness": "claude-code", "via_device": "Gaming PC", "signed_in": true}),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "cc-opus".into(),
            provider_id: format!("claude-code@{}", pc.id),
            model: "opus".into(),
            source: "auto".into(),
            price_source: "none".into(),
            display_name: Some("Claude Code Opus".into()),
            ..Default::default()
        })
        .unwrap();
        let saved = db.find_model("cc-opus").unwrap();
        assert_eq!(saved.kind, "harness");
        assert_eq!(saved.device_id.as_deref(), Some(pc.id.as_str()));
        assert_eq!(saved.source, "auto");
        assert_eq!(saved.price_source, "none");
        assert_eq!(saved.display_name.as_deref(), Some("Claude Code Opus"));
        assert!(!db.model_is_local(&saved));

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
