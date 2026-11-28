//! Relay connector: the hub's side of the Grouper relay (`docs/relay-design.md`).
//!
//! The hub dials **out** to the relay over one WebSocket (`<relay>/relay/v1/hub/connect`, with
//! `Authorization: Bearer <hub secret>` and `Grouper-Hub-Id`). The relay forwards phones' and
//! browsers' HTTP requests and WebSockets down that socket as JSON frames; this module serves them
//! by calling the hub's own loopback listener, exactly as a reverse proxy would. The hub still checks
//! every request itself: the relay is a router, not an authority.
//!
//! - Settings live in `hub_meta`: `relay_url`, `relay_enabled`, `relay_hub_secret` (sealed with the
//!   master key like the tunnel token; no endpoint ever returns it) and `relay_account_email`.
//! - Relayed requests must look **remote** to the hub. The connector replaces `x-forwarded-for` with
//!   the relay's client IP (never loopback; `0.0.0.0` when missing), sets `cf-connecting-ip` to the
//!   same, sets `x-forwarded-proto` and adds `x-grouper-relayed: 1`, so the hub's loopback-only rules
//!   (token sign-in in local mode, the orchestrator MCP endpoint) don't apply and rate limits count
//!   the real client.
//! - The supervisor runs while relaying is enabled, the hub is linked and it has not stepped down. It
//!   reconnects with a 1 s to 60 s backoff. Close code 4001 means another hub replaced this one on
//!   the relay: that is a failure to show, not something to retry in a loop.
//!
//! Admin endpoints (admin token or admin web session; browser cookies need the CSRF header like any
//! other admin write):
//! - `GET /v1/relay` — `{enabled, url, linked, status, last_error, account_email}`.
//! - `PUT /v1/relay` — `{url?, enabled?}`.
//! - `POST /v1/relay/nonce` — asks the relay for a nonce (the app runs Google sign-in with it).
//! - `POST /v1/relay/link` — `{id_token}`: the owner's Google ID token; claims the hub on the relay.
//! - `DELETE /v1/relay` — forgets the hub on the relay and clears the secret.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};

use crate::db::Db;
use crate::registry::deny_unless_admin;
use crate::server::{Hub, err};

const META_URL: &str = "relay_url";
const META_ENABLED: &str = "relay_enabled";
const META_SECRET: &str = "relay_hub_secret";
const META_EMAIL: &str = "relay_account_email";

/// Response and WebSocket data frames carry at most this many raw bytes.
const CHUNK_BYTES: usize = 48 * 1024;
/// Request bodies reach 16 MiB and are sent whole (base64 in one frame).
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_REQUESTS: usize = 128;
const MAX_WEBSOCKETS: usize = 64;
const RELAY_CALL_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ERROR_CHARS: usize = 300;

/// Lifecycle of the connector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayStatus {
    /// Not running: relaying is off, the hub isn't linked, or it has stepped down.
    Off,
    Connecting,
    Connected,
    /// Not connected; `last_error` says why. The supervisor keeps retrying unless the relay said
    /// this hub was replaced.
    Failed,
}

/// Current snapshot of the connector, safe to read or expose to admin APIs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelaySnapshot {
    pub status: RelayStatus,
    pub last_error: Option<String>,
}

/// Tuning knobs for the connector (tests shorten these).
#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// How often the hub pings the relay (the relay closes sockets silent for 90 s).
    pub ping_interval: Duration,
    /// Drop the socket when the relay has sent nothing at all for this long.
    pub idle_timeout: Duration,
    /// How often an idle or running supervisor re-reads settings it wasn't told about (the CLI
    /// changes them straight in the database).
    pub recheck: Duration,
    /// A connection that lasted this long starts the next backoff from the beginning.
    pub stable_after: Duration,
    pub connect_timeout: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(100),
            recheck: Duration::from_secs(30),
            stable_after: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(15),
        }
    }
}

/// Per-hub relay connector state held on [`Hub`].
pub struct RelayState {
    config: Mutex<RelayConfig>,
    snapshot: watch::Sender<RelaySnapshot>,
    reconcile: Notify,
    supervisor_running: AtomicBool,
}

impl Default for RelayState {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayState {
    pub fn new() -> Self {
        let (snapshot, _rx) = watch::channel(RelaySnapshot { status: RelayStatus::Off, last_error: None });
        Self {
            config: Mutex::new(RelayConfig::default()),
            snapshot,
            reconcile: Notify::new(),
            supervisor_running: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> RelayConfig {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_config(&self, config: RelayConfig) {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = config;
        self.notify_changed();
    }

    pub fn snapshot(&self) -> RelaySnapshot {
        self.snapshot.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<RelaySnapshot> {
        self.snapshot.subscribe()
    }

    /// Wake the supervisor to re-read settings, the secret and step-down state.
    pub fn notify_changed(&self) {
        self.reconcile.notify_one();
    }

    fn set(&self, status: RelayStatus, last_error: Option<String>) {
        let next = RelaySnapshot { status, last_error };
        self.snapshot.send_if_modified(|cur| {
            if *cur == next {
                false
            } else {
                *cur = next;
                true
            }
        });
    }
}

// ------------------------------------------------------------------ settings

/// Normalize a relay address: `http(s)://host[:port][/prefix]` (`ws(s)://` is accepted and means the
/// same), no query, no trailing slash.
pub fn normalize_relay_url(raw: &str) -> Result<String> {
    let parsed = url::Url::parse(raw.trim()).context("the relay address must be a URL like https://relay.example.com")?;
    let scheme = match parsed.scheme() {
        "http" | "ws" => "http",
        "https" | "wss" => "https",
        _ => bail!("the relay address must start with https:// (or http:// for a local relay)"),
    };
    let Some(host) = parsed.host_str() else { bail!("the relay address must include a host") };
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("the relay address must not have a query or fragment");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("the relay address must not contain a user name or password");
    }
    // `host_str` already brackets IPv6 addresses.
    let host = host.to_string();
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    Ok(format!("{scheme}://{host}{port}{}", parsed.path().trim_end_matches('/')))
}

/// The hub tunnel socket address for a normalized relay URL.
fn hub_connect_url(base: &str) -> String {
    let ws = if let Some(rest) = base.strip_prefix("https://") { format!("wss://{rest}") } else { base.replacen("http://", "ws://", 1) };
    format!("{ws}/relay/v1/hub/connect")
}

impl Db {
    fn relay_meta(&self, key: &str) -> Option<String> {
        self.c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [key], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
    }

    fn set_relay_meta(&self, key: &str, value: &str) -> Result<()> {
        self.c()
            .execute("INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)", params![key, value])
            .with_context(|| format!("saving {key}"))?;
        Ok(())
    }

    fn del_relay_meta(&self, key: &str) -> Result<()> {
        self.c().execute("DELETE FROM hub_meta WHERE key = ?1", [key]).with_context(|| format!("clearing {key}"))?;
        Ok(())
    }

    /// The relay's address (normalized), if set.
    pub fn relay_url(&self) -> Option<String> {
        self.relay_meta(META_URL)
    }

    /// Set or clear (`None` or empty) the relay address.
    pub fn set_relay_url(&self, url: Option<&str>) -> Result<()> {
        match url.map(str::trim).filter(|s| !s.is_empty()) {
            None => self.del_relay_meta(META_URL),
            Some(raw) => self.set_relay_meta(META_URL, &normalize_relay_url(raw)?),
        }
    }

    /// Whether the hub should keep a relay connection open. Defaults to `false`.
    pub fn relay_enabled(&self) -> bool {
        self.relay_meta(META_ENABLED).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }

    pub fn set_relay_enabled(&self, enabled: bool) -> Result<()> {
        self.set_relay_meta(META_ENABLED, if enabled { "true" } else { "false" })
    }

    /// Whether a sealed relay hub secret is stored (never reveals it).
    pub fn has_relay_secret(&self) -> bool {
        self.relay_meta(META_SECRET).is_some()
    }

    /// Decrypt the hub secret for the relay connection. Internal only.
    pub(crate) fn relay_secret(&self) -> Result<Option<String>> {
        let Some(sealed) = self.relay_meta(META_SECRET) else { return Ok(None) };
        let plain = self
            .unseal(&sealed)
            .context("cannot decrypt the relay secret; is this the master key that sealed it?")?;
        Ok(Some(plain))
    }

    /// The Google account the relay link belongs to, as the relay reported it.
    pub fn relay_account_email(&self) -> Option<String> {
        self.relay_meta(META_EMAIL)
    }

    /// Seal and store the hub secret and the account email from a successful claim.
    fn set_relay_link(&self, secret: &str, email: &str) -> Result<()> {
        let sealed = self.seal(secret).context("sealing the relay secret")?;
        self.set_relay_meta(META_SECRET, &sealed)?;
        self.set_relay_meta(META_EMAIL, email)
    }

    /// Forget the relay link (secret and account) and turn relaying off.
    fn clear_relay_link(&self) -> Result<()> {
        self.del_relay_meta(META_SECRET)?;
        self.del_relay_meta(META_EMAIL)?;
        self.set_relay_enabled(false)
    }
}

// ------------------------------------------------------------------ talking to the relay over HTTP

fn relay_http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(RELAY_CALL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

/// What a relay error response says, short and safe to show.
async fn relay_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["error"].as_str().map(String::from))
        .unwrap_or_else(|| text.trim().to_string());
    let detail: String = detail.chars().take(MAX_ERROR_CHARS).collect();
    if detail.is_empty() { format!("the relay answered {status}") } else { format!("the relay answered {status}: {detail}") }
}

/// Tell the relay to forget this hub (best effort), then clear the link and turn relaying off.
/// Returns whether the relay confirmed. Shared by `DELETE /v1/relay` and `grouper-hub relay unlink`.
pub async fn unlink(db: &Db) -> Result<bool> {
    let mut forgot = false;
    if let (Some(url), Ok(Some(secret))) = (db.relay_url(), db.relay_secret()) {
        let result =
            relay_http().delete(format!("{url}/relay/v1/hubs")).header("authorization", format!("Bearer {secret}")).send().await;
        match result {
            Ok(resp) if resp.status().is_success() => forgot = true,
            Ok(resp) => tracing::warn!("relay: unlink: {}", relay_error(resp).await),
            Err(error) => tracing::warn!("relay: unlink: couldn't reach the relay: {error}"),
        }
    }
    db.clear_relay_link()?;
    Ok(forgot)
}

// ------------------------------------------------------------------ header rules

/// Headers neither side forwards (connection-level, or recomputed).
fn is_hop_by_hop(name: &str) -> bool {
    matches!(name, "connection" | "keep-alive" | "transfer-encoding" | "upgrade" | "te" | "trailer" | "content-length")
        || name.starts_with("proxy-")
        || name.starts_with("sec-websocket-")
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// The headers to send to the hub's loopback listener for a relayed request: the relay's end-to-end
/// headers (`host`, `origin`, `cookie`, `authorization`, …) minus hop-by-hop ones, with the
/// forwarding headers set so the hub treats the caller as remote.
///
/// The client IP is the **last** `x-forwarded-for` entry (the relay sets or appends its own, so
/// anything a client put earlier in the list is ignored); a missing, unparseable or loopback value
/// becomes `0.0.0.0`. `x-forwarded-proto` keeps the relay's `http`/`https`, else `relay_scheme`.
pub(crate) fn forwarded_headers(headers: &[(String, String)], relay_scheme: &str) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(headers.len() + 4);
    let mut client: Option<IpAddr> = None;
    let mut proto: Option<String> = None;
    for (name, value) in headers {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || is_hop_by_hop(&name) {
            continue;
        }
        match name.as_str() {
            "x-forwarded-for" => {
                client = value.rsplit(',').next().and_then(|v| v.trim().parse::<IpAddr>().ok()).filter(|ip| !is_loopback(*ip));
            }
            "x-forwarded-proto" => {
                let v = value.trim().to_ascii_lowercase();
                if v == "http" || v == "https" {
                    proto = Some(v);
                }
            }
            "cf-connecting-ip" | "x-grouper-relayed" => {}
            _ => out.push((name, value.clone())),
        }
    }
    let ip = client.map(|ip| ip.to_string()).unwrap_or_else(|| "0.0.0.0".into());
    out.push(("x-forwarded-for".into(), ip.clone()));
    out.push(("cf-connecting-ip".into(), ip));
    out.push(("x-forwarded-proto".into(), proto.unwrap_or_else(|| relay_scheme.to_string())));
    out.push(("x-grouper-relayed".into(), "1".into()));
    out
}

fn frame(v: Value) -> Message {
    Message::text(v.to_string())
}

fn frame_headers(v: &Value) -> Vec<(String, String)> {
    v["headers"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    Some((pair.first()?.as_str()?.to_string(), pair.get(1)?.as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ calling the hub's loopback listener

/// The hub's own listener, which relayed traffic is replayed against.
struct Local {
    addr: SocketAddr,
    http: reqwest::Client,
}

impl Local {
    fn new(addr: SocketAddr) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self { addr, http }
    }

    fn checked_path(path: &str) -> Result<&str, String> {
        if path.starts_with('/') && !path.starts_with("//") && !path.chars().any(|c| c.is_control() || c == ' ') {
            Ok(path)
        } else {
            Err(format!("bad request path {path:?}"))
        }
    }

    fn http_url(&self, path: &str) -> Result<String, String> {
        Ok(format!("http://{}{}", self.addr, Self::checked_path(path)?))
    }

    fn ws_url(&self, path: &str) -> Result<String, String> {
        Ok(format!("ws://{}{}", self.addr, Self::checked_path(path)?))
    }
}

fn ws_config() -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_FRAME_BYTES);
    config.max_frame_size = Some(MAX_FRAME_BYTES);
    config
}

enum WsCmd {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

struct WsEntry {
    cmds: mpsc::UnboundedSender<WsCmd>,
    abort: AbortHandle,
}

#[derive(Default)]
struct Active {
    reqs: HashMap<u64, AbortHandle>,
    wss: HashMap<u64, WsEntry>,
}

/// Aborts every in-flight request and WebSocket when a relay connection ends.
struct AbortAll(Arc<Mutex<Active>>);

impl Drop for AbortAll {
    fn drop(&mut self) {
        let mut active = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (_, handle) in active.reqs.drain() {
            handle.abort();
        }
        for (_, ws) in active.wss.drain() {
            ws.abort.abort();
        }
    }
}

/// One relay connection's shared handles, cloned into each request task.
#[derive(Clone)]
struct Conn {
    local: Arc<Local>,
    out: mpsc::Sender<Message>,
    active: Arc<Mutex<Active>>,
    scheme: Arc<str>,
}

impl Conn {
    fn active(&self) -> MutexGuard<'_, Active> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue a frame without waiting (for errors raised while reading the relay's frames).
    fn send_later(&self, v: Value) {
        let out = self.out.clone();
        tokio::spawn(async move {
            let _ = out.send(frame(v)).await;
        });
    }

    /// Act on one text frame from the relay. Returns a frame to send straight back (`pong`).
    fn handle(&self, text: &str) -> Option<Message> {
        let v: Value = serde_json::from_str(text).ok()?;
        match v["t"].as_str()? {
            "ping" => return Some(frame(json!({"t": "pong"}))),
            "req" => self.open_request(v["id"].as_u64()?, &v),
            "cancel" => {
                if let Some(handle) = self.active().reqs.remove(&v["id"].as_u64()?) {
                    handle.abort();
                }
            }
            "ws_open" => self.open_websocket(v["id"].as_u64()?, &v),
            "ws_msg" => {
                let cmd = if let Some(text) = v["text"].as_str() {
                    WsCmd::Text(text.to_string())
                } else {
                    WsCmd::Binary(B64.decode(v["b64"].as_str()?).ok()?)
                };
                if let Some(ws) = self.active().wss.get(&v["id"].as_u64()?) {
                    let _ = ws.cmds.send(cmd);
                }
            }
            "ws_close" => {
                let code = v["code"].as_u64().and_then(|c| u16::try_from(c).ok()).unwrap_or(1000);
                let reason = v["reason"].as_str().unwrap_or("").to_string();
                if let Some(ws) = self.active().wss.get(&v["id"].as_u64()?) {
                    let _ = ws.cmds.send(WsCmd::Close(code, reason));
                }
            }
            _ => {}
        }
        None
    }

    fn open_request(&self, id: u64, v: &Value) {
        let mut active = self.active();
        if active.reqs.len() >= MAX_REQUESTS {
            self.send_later(json!({"t": "err", "id": id, "message": "the hub is handling too many relayed requests"}));
            return;
        }
        let body = match v["body"].as_str() {
            None => Vec::new(),
            Some(b64) => match B64.decode(b64) {
                Ok(bytes) => bytes,
                Err(_) => {
                    self.send_later(json!({"t": "err", "id": id, "message": "the request body isn't valid base64"}));
                    return;
                }
            },
        };
        let req = Request {
            method: v["method"].as_str().unwrap_or("GET").to_string(),
            path: v["path"].as_str().unwrap_or("/").to_string(),
            headers: frame_headers(v),
            body,
        };
        let conn = self.clone();
        let task = tokio::spawn(async move {
            if let Err(message) = proxy_request(&conn, id, req).await {
                let _ = conn.out.send(frame(json!({"t": "err", "id": id, "message": message}))).await;
            }
            conn.active().reqs.remove(&id);
        });
        active.reqs.insert(id, task.abort_handle());
    }

    fn open_websocket(&self, id: u64, v: &Value) {
        let mut active = self.active();
        if active.wss.len() >= MAX_WEBSOCKETS {
            self.send_later(json!({"t": "ws_fail", "id": id, "status": 503, "message": "too many relayed WebSockets"}));
            return;
        }
        let path = v["path"].as_str().unwrap_or("/").to_string();
        let headers = frame_headers(v);
        let (cmds, rx) = mpsc::unbounded_channel();
        let conn = self.clone();
        let task = tokio::spawn(async move {
            serve_websocket(&conn, id, &path, &headers, rx).await;
            conn.active().wss.remove(&id);
        });
        active.wss.insert(id, WsEntry { cmds, abort: task.abort_handle() });
    }
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn header_map(list: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in list {
        if let (Ok(name), Ok(value)) =
            (reqwest::header::HeaderName::from_bytes(name.as_bytes()), reqwest::header::HeaderValue::from_str(value))
        {
            map.append(name, value);
        }
    }
    map
}

/// Serve one `req` frame: call the loopback listener and stream the answer back as `res`, `data`
/// chunks and `end`. An `Err` becomes an `err` frame.
async fn proxy_request(conn: &Conn, id: u64, req: Request) -> Result<(), String> {
    let closed = |_| "the relay connection closed".to_string();
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|_| format!("bad method {:?}", req.method))?;
    let url = conn.local.http_url(&req.path)?;
    let mut builder =
        conn.local.http.request(method, url).headers(header_map(&forwarded_headers(&req.headers, &conn.scheme)));
    if !req.body.is_empty() {
        builder = builder.body(req.body);
    }
    let resp = builder.send().await.map_err(|e| format!("the hub didn't answer: {e}"))?;
    let headers: Vec<Value> = resp
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .map(|(name, value)| json!([name.as_str(), String::from_utf8_lossy(value.as_bytes())]))
        .collect();
    let head = json!({"t": "res", "id": id, "status": resp.status().as_u16(), "headers": headers});
    conn.out.send(frame(head)).await.map_err(closed)?;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("the hub's response broke off: {e}"))?;
        for part in chunk.chunks(CHUNK_BYTES) {
            conn.out.send(frame(json!({"t": "data", "id": id, "b64": B64.encode(part)}))).await.map_err(closed)?;
        }
    }
    conn.out.send(frame(json!({"t": "end", "id": id}))).await.map_err(closed)?;
    Ok(())
}

/// Serve one `ws_open`: open a WebSocket to the same path on the loopback listener and pass messages
/// both ways until either side closes.
async fn serve_websocket(
    conn: &Conn,
    id: u64,
    path: &str,
    headers: &[(String, String)],
    mut cmds: mpsc::UnboundedReceiver<WsCmd>,
) {
    let out = &conn.out;
    let fail = |status: u16, message: String| frame(json!({"t": "ws_fail", "id": id, "status": status, "message": message}));
    let request = conn.local.ws_url(path).map_err(|e| e.to_string()).and_then(|url| {
        let mut request = url.into_client_request().map_err(|e| e.to_string())?;
        for (name, value) in forwarded_headers(headers, &conn.scheme) {
            if let (Ok(name), Ok(value)) = (
                tungstenite::http::HeaderName::from_bytes(name.as_bytes()),
                tungstenite::http::HeaderValue::from_str(&value),
            ) {
                // `host` replaces the one for the loopback address, so the hub sees the relay's.
                if name == tungstenite::http::header::HOST {
                    request.headers_mut().insert(name, value);
                } else {
                    request.headers_mut().append(name, value);
                }
            }
        }
        Ok(request)
    });
    let request = match request {
        Ok(r) => r,
        Err(message) => {
            let _ = out.send(fail(400, message)).await;
            return;
        }
    };
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(ws_config()), false);
    let ws = match tokio::time::timeout(Duration::from_secs(15), connect).await {
        Err(_) => {
            let _ = out.send(fail(504, "the hub didn't answer the WebSocket".into())).await;
            return;
        }
        Ok(Err(tungstenite::Error::Http(resp))) => {
            // A non-error answer (the path isn't a WebSocket route) is still a failure to the relay.
            let status = if resp.status().as_u16() >= 400 { resp.status().as_u16() } else { 502 };
            let _ = out.send(fail(status, "the hub refused the WebSocket".into())).await;
            return;
        }
        Ok(Err(e)) => {
            let _ = out.send(fail(502, format!("couldn't open the WebSocket to the hub: {e}"))).await;
            return;
        }
        Ok(Ok((ws, _))) => ws,
    };
    if out.send(frame(json!({"t": "ws_ok", "id": id}))).await.is_err() {
        return;
    }
    let (mut sink, mut stream) = ws.split();
    let close_frame = |code: u16, reason: &str| {
        let code = CloseCode::from(code);
        code.is_allowed().then(|| CloseFrame { code, reason: reason.to_string().into() })
    };
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    return;
                }
                Some(WsCmd::Text(text)) => {
                    if sink.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                Some(WsCmd::Binary(bytes)) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Some(WsCmd::Close(code, reason)) => {
                    let _ = sink.send(Message::Close(close_frame(code, &reason))).await;
                    return;
                }
            },
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if out.send(frame(json!({"t": "ws_msg", "id": id, "text": text.as_str()}))).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if out.send(frame(json!({"t": "ws_msg", "id": id, "b64": B64.encode(&bytes)}))).await.is_err() {
                        return;
                    }
                }
                Some(Ok(Message::Close(close))) => {
                    let (code, reason) = match close {
                        Some(f) if f.code.is_allowed() => (u16::from(f.code), f.reason.to_string()),
                        _ => (1000, String::new()),
                    };
                    let _ = out.send(frame(json!({"t": "ws_close", "id": id, "code": code, "reason": reason}))).await;
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
    let _ = out.send(frame(json!({"t": "ws_close", "id": id, "code": 1011, "reason": "the hub's WebSocket ended"}))).await;
}

// ------------------------------------------------------------------ supervision loop

/// What the settings ask the connector to do right now.
enum Wanted {
    Off,
    Failed(String),
    Run(Plan),
}

/// Everything a connection depends on; a connection ends when this changes.
#[derive(Clone, PartialEq, Eq)]
struct Plan {
    /// Normalized relay base URL (`http(s)://…`).
    url: String,
    secret: String,
    hub_id: String,
}

fn wanted(hub: &Hub) -> Wanted {
    if !hub.db.relay_enabled() || hub.stepdown.is_stepped_down() {
        return Wanted::Off;
    }
    let secret = match hub.db.relay_secret() {
        Ok(Some(secret)) => secret,
        Ok(None) => return Wanted::Off,
        Err(error) => return Wanted::Failed(format!("{error:#}")),
    };
    let Some(url) = hub.db.relay_url() else { return Wanted::Off };
    Wanted::Run(Plan { url, secret, hub_id: hub.db.hub_id() })
}

enum Exit {
    Shutdown,
    /// Settings changed: start over from the current ones.
    Reconcile,
    /// The relay closed the socket with 4001: another hub claimed this account.
    Replaced,
    Dropped { message: String, connected_for: Option<Duration> },
}

fn redact(message: &str, secret: &str) -> String {
    if secret.is_empty() { message.to_string() } else { message.replace(secret, "[redacted]") }
}

/// Wait for a settings change, an optional `delay` or shutdown. Returns `false` when shutting down.
async fn wait(reconcile: &Notify, shutdown: &mut watch::Receiver<bool>, delay: Option<Duration>) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    let sleep = async {
        match delay {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        changed = shutdown.changed() => !(changed.is_err() || *shutdown.borrow()),
        _ = reconcile.notified() => !*shutdown.borrow(),
        _ = sleep => !*shutdown.borrow(),
    }
}

/// Background supervisor started by [`crate::host`] next to the tunnel's. `loopback` is the hub's own
/// listener. Runs until `shutdown` becomes `true`.
pub async fn supervisor_loop(hub: Arc<Hub>, loopback: SocketAddr, mut shutdown: watch::Receiver<bool>) {
    hub.relay.supervisor_running.store(true, Ordering::SeqCst);
    struct RunningGuard(Arc<Hub>);
    impl Drop for RunningGuard {
        fn drop(&mut self) {
            self.0.relay.supervisor_running.store(false, Ordering::SeqCst);
        }
    }
    let _guard = RunningGuard(hub.clone());

    let local = Arc::new(Local::new(loopback));
    let mut backoff = hub.relay.config().backoff_initial;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let cfg = hub.relay.config();
        let plan = match wanted(&hub) {
            Wanted::Off => {
                backoff = cfg.backoff_initial;
                hub.relay.set(RelayStatus::Off, None);
                if !wait(&hub.relay.reconcile, &mut shutdown, Some(cfg.recheck)).await {
                    break;
                }
                continue;
            }
            Wanted::Failed(message) => {
                hub.relay.set(RelayStatus::Failed, Some(message));
                if !wait(&hub.relay.reconcile, &mut shutdown, Some(cfg.recheck)).await {
                    break;
                }
                continue;
            }
            Wanted::Run(plan) => plan,
        };

        hub.relay.set(RelayStatus::Connecting, None);
        match run_connection(&hub, &plan, &local, &cfg, &mut shutdown).await {
            Exit::Shutdown => break,
            Exit::Reconcile => backoff = cfg.backoff_initial,
            Exit::Replaced => {
                tracing::warn!("relay: another hub replaced this one on the relay; not reconnecting");
                hub.relay.set(
                    RelayStatus::Failed,
                    Some(
                        "another hub replaced this one on the relay (it was linked again from elsewhere); \
                         link this hub again to take the account back"
                            .into(),
                    ),
                );
                if !wait(&hub.relay.reconcile, &mut shutdown, None).await {
                    break;
                }
                backoff = cfg.backoff_initial;
            }
            Exit::Dropped { message, connected_for } => {
                if connected_for.is_some_and(|d| d >= cfg.stable_after) {
                    backoff = cfg.backoff_initial;
                }
                let message = redact(&message, &plan.secret);
                tracing::warn!(backoff_ms = backoff.as_millis() as u64, "relay: {message}");
                hub.relay.set(RelayStatus::Failed, Some(message));
                if !wait(&hub.relay.reconcile, &mut shutdown, Some(backoff)).await {
                    break;
                }
                backoff = (backoff * 2).min(cfg.backoff_max);
            }
        }
    }
    hub.relay.set(RelayStatus::Off, None);
}

fn still_wanted(hub: &Hub, plan: &Plan) -> bool {
    matches!(wanted(hub), Wanted::Run(current) if current == *plan)
}

/// One relay connection: connect, say hello, serve frames until it ends.
async fn run_connection(
    hub: &Arc<Hub>,
    plan: &Plan,
    local: &Arc<Local>,
    cfg: &RelayConfig,
    shutdown: &mut watch::Receiver<bool>,
) -> Exit {
    let dropped = |message: String| Exit::Dropped { message, connected_for: None };
    let mut request = match hub_connect_url(&plan.url).into_client_request() {
        Ok(r) => r,
        Err(e) => return dropped(format!("bad relay address: {e}")),
    };
    let (Ok(auth), Ok(hub_id)) = (
        tungstenite::http::HeaderValue::from_str(&format!("Bearer {}", plan.secret)),
        tungstenite::http::HeaderValue::from_str(&plan.hub_id),
    ) else {
        return dropped("the relay secret or hub id isn't a valid header value; link the hub again".into());
    };
    request.headers_mut().insert("authorization", auth);
    request.headers_mut().insert("grouper-hub-id", hub_id);

    let connect = tokio::time::timeout(
        cfg.connect_timeout,
        tokio_tungstenite::connect_async_with_config(request, Some(ws_config()), false),
    );
    tokio::pin!(connect);
    let connected = loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Exit::Shutdown;
                }
            }
            _ = hub.relay.reconcile.notified() => {
                if !still_wanted(hub, plan) {
                    return Exit::Reconcile;
                }
            }
            r = &mut connect => break r,
        }
    };
    let ws = match connected {
        Err(_) => return dropped("timed out connecting to the relay".into()),
        Ok(Err(tungstenite::Error::Http(resp))) if resp.status().as_u16() == 401 => {
            return dropped("the relay refused this hub's secret (401); link the hub again".into());
        }
        Ok(Err(tungstenite::Error::Http(resp))) => {
            return dropped(format!("the relay refused the connection ({})", resp.status()));
        }
        Ok(Err(e)) => return dropped(format!("couldn't reach the relay: {e}")),
        Ok(Ok((ws, _))) => ws,
    };

    let (mut sink, mut stream) = ws.split();
    let hello = frame(json!({"t": "hello", "hub_id": plan.hub_id, "version": env!("CARGO_PKG_VERSION")}));
    if let Err(e) = sink.send(hello).await {
        return dropped(format!("the relay closed the connection: {e}"));
    }
    let connected_at = Instant::now();
    hub.relay.set(RelayStatus::Connected, None);
    tracing::info!("relay: connected");

    let (out, mut out_rx) = mpsc::channel::<Message>(64);
    let active = Arc::new(Mutex::new(Active::default()));
    let _abort_all = AbortAll(active.clone());
    let conn = Conn { local: local.clone(), out, active, scheme: plan.url.split(':').next().unwrap_or("https").into() };

    let mut ping = tokio::time::interval(cfg.ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut last_rx = Instant::now();
    let ended = |message: String| Exit::Dropped { message, connected_for: Some(connected_at.elapsed()) };

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = sink.send(Message::Close(None)).await;
                    return Exit::Shutdown;
                }
            }
            _ = hub.relay.reconcile.notified() => {
                if !still_wanted(hub, plan) {
                    let _ = sink.send(Message::Close(None)).await;
                    return Exit::Reconcile;
                }
            }
            _ = ping.tick() => {
                if !still_wanted(hub, plan) {
                    let _ = sink.send(Message::Close(None)).await;
                    return Exit::Reconcile;
                }
                if last_rx.elapsed() > cfg.idle_timeout {
                    return ended("the relay stopped answering".into());
                }
                if let Err(e) = sink.send(frame(json!({"t": "ping"}))).await {
                    return ended(format!("the relay closed the connection: {e}"));
                }
            }
            Some(message) = out_rx.recv() => {
                if let Err(e) = sink.send(message).await {
                    return ended(format!("the relay closed the connection: {e}"));
                }
            }
            incoming = stream.next() => match incoming {
                None => return ended("the relay closed the connection".into()),
                Some(Err(e)) => return ended(format!("the relay connection failed: {e}")),
                Some(Ok(message)) => {
                    last_rx = Instant::now();
                    match message {
                        Message::Text(text) => {
                            if let Some(reply) = conn.handle(text.as_str())
                                && let Err(e) = sink.send(reply).await
                            {
                                return ended(format!("the relay closed the connection: {e}"));
                            }
                        }
                        Message::Close(close) => {
                            if close.as_ref().is_some_and(|f| u16::from(f.code) == 4001) {
                                return Exit::Replaced;
                            }
                            return ended(match close {
                                Some(f) => format!("the relay closed the connection ({}: {})", u16::from(f.code), f.reason),
                                None => "the relay closed the connection".into(),
                            });
                        }
                        _ => {}
                    }
                }
            },
        }
    }
}

// ------------------------------------------------------------------ admin HTTP routes

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/relay", get(get_relay).put(put_relay).delete(delete_relay))
        .route("/v1/relay/nonce", post(post_nonce))
        .route("/v1/relay/link", post(post_link))
}

/// The status document. Never contains the hub secret.
fn status_payload(hub: &Hub) -> Value {
    let enabled = hub.db.relay_enabled();
    let linked = hub.db.has_relay_secret();
    let url = hub.db.relay_url();
    let snap = hub.relay.snapshot();
    let running = enabled && linked && url.is_some() && !hub.stepdown.is_stepped_down();
    let status = match (running, snap.status) {
        (false, _) => RelayStatus::Off,
        // The supervisor hasn't caught up with a settings change yet.
        (true, RelayStatus::Off) => RelayStatus::Connecting,
        (true, status) => status,
    };
    json!({
        "enabled": enabled,
        "url": url,
        "linked": linked,
        "status": status,
        "last_error": if running { snap.last_error } else { None },
        "account_email": hub.db.relay_account_email(),
    })
}

async fn get_relay(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    Json(status_payload(&hub)).into_response()
}

/// `null` is "clear", a string is "set", absent is "leave alone".
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(d)?))
}

#[derive(Deserialize)]
struct UpdateBody {
    #[serde(default, deserialize_with = "double_option")]
    url: Option<Option<String>>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn put_relay(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<UpdateBody>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    // Check everything before changing anything.
    let new_url = match &body.url {
        None => None,
        Some(None) => Some(None),
        Some(Some(raw)) if raw.trim().is_empty() => Some(None),
        Some(Some(raw)) => match normalize_relay_url(raw) {
            Ok(url) => Some(Some(url)),
            Err(e) => return err(StatusCode::BAD_REQUEST, e),
        },
    };
    if let Some(new) = &new_url
        && *new != hub.db.relay_url()
        && hub.db.has_relay_secret()
    {
        return err(
            StatusCode::CONFLICT,
            "this hub is linked to the current relay; unlink it (DELETE /v1/relay) before changing the address",
        );
    }
    if let Some(new) = new_url
        && let Err(e) = hub.db.set_relay_url(new.as_deref())
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Some(enabled) = body.enabled
        && let Err(e) = hub.db.set_relay_enabled(enabled)
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    hub.relay.notify_changed();
    Json(status_payload(&hub)).into_response()
}

/// A relay call that failed, as the response the admin sees.
fn relay_unavailable(what: &str, e: impl std::fmt::Display) -> Response {
    err(StatusCode::BAD_GATEWAY, format!("couldn't {what} through the relay: {e}"))
}

async fn post_nonce(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some(url) = hub.db.relay_url() else {
        return err(StatusCode::BAD_REQUEST, "set the relay address first (PUT /v1/relay)");
    };
    let resp = match relay_http().post(format!("{url}/relay/v1/nonce")).send().await {
        Ok(r) => r,
        Err(e) => return relay_unavailable("get a nonce", e),
    };
    if !resp.status().is_success() {
        return err(StatusCode::BAD_GATEWAY, relay_error(resp).await);
    }
    match resp.json::<Value>().await {
        Ok(v) if v["nonce"].is_string() => Json(json!({"nonce": v["nonce"], "expires": v["expires"]})).into_response(),
        _ => err(StatusCode::BAD_GATEWAY, "the relay's nonce answer wasn't understood"),
    }
}

#[derive(Deserialize)]
struct LinkBody {
    id_token: String,
}

async fn post_link(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<LinkBody>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some(url) = hub.db.relay_url() else {
        return err(StatusCode::BAD_REQUEST, "set the relay address first (PUT /v1/relay)");
    };
    // The relay checks its own nonce; the hub checks the signature and that this is its owner.
    if let Err((status, message)) = crate::google_auth::verify_owner_token(&hub, &body.id_token).await {
        return err(status, message);
    }
    let claim = json!({"hub_id": hub.db.hub_id(), "id_token": body.id_token});
    let resp = match relay_http().post(format!("{url}/relay/v1/hubs/claim")).json(&claim).send().await {
        Ok(r) => r,
        Err(e) => return relay_unavailable("link this hub", e),
    };
    if !resp.status().is_success() {
        return err(StatusCode::BAD_GATEWAY, relay_error(resp).await);
    }
    let answer = resp.json::<Value>().await.unwrap_or(Value::Null);
    let (Some(secret), email) = (answer["hub_secret"].as_str(), answer["account_email"].as_str().unwrap_or("")) else {
        return err(StatusCode::BAD_GATEWAY, "the relay's link answer wasn't understood");
    };
    if let Err(e) = hub.db.set_relay_link(secret, email).and_then(|()| hub.db.set_relay_enabled(true)) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    tracing::info!(email, "relay: hub linked");
    hub.relay.notify_changed();
    Json(status_payload(&hub)).into_response()
}

async fn delete_relay(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let forgot = match unlink(&hub.db).await {
        Ok(forgot) => forgot,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    hub.relay.notify_changed();
    let mut payload = status_payload(&hub);
    payload["relay_forgot_hub"] = json!(forgot);
    Json(payload).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::convert::Infallible;
    use std::sync::atomic::AtomicUsize;

    use axum::body::{Body, Bytes};
    use axum::extract::Request as AxumRequest;
    use axum::extract::ws::{CloseFrame as AxumClose, Message as AxMsg, WebSocketUpgrade};
    use axum::routing::any;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tokio::task::JoinHandle;

    use crate::db::random_hex;
    use crate::google_auth::Owner;
    use crate::server::router;

    const SECRET: &str = "hs.acct123.SECRETSECRETSECRETSECRET";
    const LATER_SECRET: &str = "hs.acct123.AnotherSecretValueAfterLink";
    const WAIT: Duration = Duration::from_secs(8);

    // ------------------------------------------------------------ pure pieces

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn values(list: &[(String, String)], name: &str) -> Vec<String> {
        list.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone()).collect()
    }

    #[test]
    fn forwarded_headers_follow_the_header_rules() {
        let out = forwarded_headers(
            &h(&[
                ("Host", "relay.example"),
                ("Origin", "https://relay.example"),
                ("Cookie", "a=b"),
                ("Authorization", "Bearer x"),
                ("X-Forwarded-For", "203.0.113.7"),
                ("CF-Connecting-IP", "127.0.0.1"),
                ("X-Grouper-Relayed", "0"),
                ("X-Forwarded-Proto", "https"),
                ("Connection", "keep-alive"),
                ("Keep-Alive", "timeout=5"),
                ("Transfer-Encoding", "chunked"),
                ("Upgrade", "websocket"),
                ("TE", "trailers"),
                ("Trailer", "x"),
                ("Content-Length", "999"),
                ("Proxy-Authorization", "x"),
                ("Sec-WebSocket-Key", "abc"),
                ("X-Custom", "1"),
            ]),
            "http",
        );
        assert_eq!(values(&out, "host"), ["relay.example"]);
        assert_eq!(values(&out, "origin"), ["https://relay.example"]);
        assert_eq!(values(&out, "cookie"), ["a=b"]);
        assert_eq!(values(&out, "authorization"), ["Bearer x"]);
        assert_eq!(values(&out, "x-custom"), ["1"]);
        assert_eq!(values(&out, "x-forwarded-for"), ["203.0.113.7"]);
        assert_eq!(values(&out, "cf-connecting-ip"), ["203.0.113.7"]);
        assert_eq!(values(&out, "x-forwarded-proto"), ["https"]);
        assert_eq!(values(&out, "x-grouper-relayed"), ["1"]);
        for dropped in
            ["connection", "keep-alive", "transfer-encoding", "upgrade", "te", "trailer", "content-length", "proxy-authorization", "sec-websocket-key"]
        {
            assert!(values(&out, dropped).is_empty(), "{dropped} must be dropped");
        }
    }

    #[test]
    fn client_ip_is_never_loopback_and_falls_back_to_unspecified() {
        let ip = |pairs: &[(&str, &str)]| values(&forwarded_headers(&h(pairs), "https"), "x-forwarded-for");
        assert_eq!(ip(&[]), ["0.0.0.0"]);
        assert_eq!(ip(&[("x-forwarded-for", "127.0.0.1")]), ["0.0.0.0"]);
        assert_eq!(ip(&[("x-forwarded-for", "::1")]), ["0.0.0.0"]);
        assert_eq!(ip(&[("x-forwarded-for", "::ffff:127.0.0.1")]), ["0.0.0.0"]);
        assert_eq!(ip(&[("x-forwarded-for", "not an ip")]), ["0.0.0.0"]);
        assert_eq!(ip(&[("x-forwarded-for", "2001:db8::5")]), ["2001:db8::5"]);
        // A client-supplied entry in front of the relay's own is ignored.
        assert_eq!(ip(&[("x-forwarded-for", "127.0.0.1, 198.51.100.4")]), ["198.51.100.4"]);
        assert_eq!(ip(&[("x-forwarded-for", "198.51.100.4, 127.0.0.1")]), ["0.0.0.0"]);
        // The proto falls back to the relay's own scheme and ignores junk.
        let proto = |pairs: &[(&str, &str)]| values(&forwarded_headers(&h(pairs), "https"), "x-forwarded-proto");
        assert_eq!(proto(&[]), ["https"]);
        assert_eq!(proto(&[("x-forwarded-proto", "gopher")]), ["https"]);
        assert_eq!(proto(&[("x-forwarded-proto", "http")]), ["http"]);
    }

    #[test]
    fn relay_addresses_are_normalized() {
        assert_eq!(normalize_relay_url(" https://relay.example.com/ ").unwrap(), "https://relay.example.com");
        assert_eq!(normalize_relay_url("wss://relay.example.com").unwrap(), "https://relay.example.com");
        assert_eq!(normalize_relay_url("http://127.0.0.1:8787").unwrap(), "http://127.0.0.1:8787");
        assert_eq!(normalize_relay_url("ws://[::1]:8787/").unwrap(), "http://[::1]:8787");
        assert_eq!(normalize_relay_url("https://example.com/grouper/").unwrap(), "https://example.com/grouper");
        for bad in ["relay.example.com", "ftp://x.example", "https://", "https://a.example/?x=1", "https://u:p@a.example", ""] {
            assert!(normalize_relay_url(bad).is_err(), "{bad:?} should be refused");
        }
        assert_eq!(hub_connect_url("https://r.example"), "wss://r.example/relay/v1/hub/connect");
        assert_eq!(hub_connect_url("http://127.0.0.1:1/x"), "ws://127.0.0.1:1/x/relay/v1/hub/connect");
    }

    #[test]
    fn the_secret_is_sealed_in_the_database_and_clearing_the_link_turns_relaying_off() {
        let dir = std::env::temp_dir().join(format!("grouper-relay-db-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        assert!(!db.relay_enabled() && !db.has_relay_secret() && db.relay_secret().unwrap().is_none());
        db.set_relay_link(SECRET, "me@example.com").unwrap();
        db.set_relay_enabled(true).unwrap();
        assert_eq!(db.relay_secret().unwrap().as_deref(), Some(SECRET));
        let raw: String =
            db.c().query_row("SELECT value FROM hub_meta WHERE key = 'relay_hub_secret'", [], |r| r.get(0)).unwrap();
        assert!(!raw.contains(SECRET), "stored value must be sealed");
        db.clear_relay_link().unwrap();
        assert!(!db.has_relay_secret() && !db.relay_enabled() && db.relay_account_email().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------ a fake relay

    /// One hub socket as the fake relay sees it.
    struct FakeConn {
        to_hub: UnboundedSender<AxMsg>,
        from_hub: UnboundedReceiver<Value>,
    }

    impl FakeConn {
        fn send(&self, v: Value) {
            self.to_hub.send(AxMsg::Text(v.to_string().into())).unwrap();
        }

        async fn recv(&mut self) -> Value {
            tokio::time::timeout(WAIT, self.from_hub.recv()).await.expect("a frame from the hub").expect("hub socket open")
        }

        /// The next frame, or `None` when nothing comes (or the socket closes) within `wait`.
        async fn recv_within(&mut self, wait: Duration) -> Option<Value> {
            tokio::time::timeout(wait, self.from_hub.recv()).await.ok().flatten()
        }

        async fn closed(&mut self) -> bool {
            loop {
                match tokio::time::timeout(WAIT, self.from_hub.recv()).await {
                    Ok(None) => return true,
                    Ok(Some(_)) => continue,
                    Err(_) => return false,
                }
            }
        }

        fn req(&self, id: u64, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&[u8]>) {
            let mut v = json!({"t": "req", "id": id, "method": method, "path": path,
                "headers": headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()});
            if let Some(body) = body {
                v["body"] = json!(B64.encode(body));
            }
            self.send(v);
        }

        /// Read frames for `id` until its `end` (or an `err`).
        async fn response(&mut self, id: u64) -> Resp {
            let mut resp = Resp::default();
            loop {
                let f = self.recv().await;
                assert_eq!(f["id"], id, "unexpected frame {f}");
                match f["t"].as_str().unwrap() {
                    "res" => {
                        resp.status = f["status"].as_u64().unwrap() as u16;
                        resp.headers = frame_headers(&f);
                    }
                    "data" => resp.chunks.push(B64.decode(f["b64"].as_str().unwrap()).unwrap()),
                    "end" => {
                        resp.ended = true;
                        return resp;
                    }
                    "err" => {
                        resp.err = Some(f["message"].as_str().unwrap().to_string());
                        return resp;
                    }
                    other => panic!("unexpected frame type {other}: {f}"),
                }
            }
        }
    }

    #[derive(Default)]
    struct Resp {
        status: u16,
        headers: Vec<(String, String)>,
        chunks: Vec<Vec<u8>>,
        ended: bool,
        err: Option<String>,
    }

    impl Resp {
        fn body(&self) -> Vec<u8> {
            self.chunks.concat()
        }
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body()).unwrap_or_else(|_| panic!("not json: {:?}", String::from_utf8_lossy(&self.body())))
        }
        fn header(&self, name: &str) -> Vec<String> {
            values(&self.headers, name)
        }
    }

    #[derive(Default)]
    struct RelayShared {
        /// The secret a hub must present.
        expected: Mutex<String>,
        /// Authorization and Grouper-Hub-Id of every accepted-or-refused connect attempt.
        attempts: Mutex<Vec<(String, String)>>,
        accepted: AtomicUsize,
        pings: AtomicUsize,
        claims: Mutex<Vec<Value>>,
        deletes: Mutex<Vec<String>>,
        fail_claim: AtomicBool,
    }

    struct FakeRelay {
        base: String,
        shared: Arc<RelayShared>,
        conns: UnboundedReceiver<FakeConn>,
    }

    impl FakeRelay {
        async fn start(secret: &str) -> FakeRelay {
            let shared = Arc::new(RelayShared::default());
            *shared.expected.lock().unwrap() = secret.to_string();
            let (conns_tx, conns) = unbounded_channel();
            let state = (shared.clone(), conns_tx);
            let app = Router::new()
                .route("/relay/v1/hub/connect", get(fake_connect))
                .route("/relay/v1/nonce", post(|| async { Json(json!({"nonce": "rn.fake.1.2.3", "expires": 4102444800u64})) }))
                .route("/relay/v1/hubs/claim", post(fake_claim))
                .route("/relay/v1/hubs", axum::routing::delete(fake_delete))
                .with_state(state);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await });
            FakeRelay { base, shared, conns }
        }

        /// The next hub socket, with its `hello` already checked.
        async fn accept(&mut self, hub: &Hub) -> FakeConn {
            let mut conn =
                tokio::time::timeout(WAIT, self.conns.recv()).await.expect("the hub to connect").expect("fake relay running");
            let hello = conn.recv().await;
            assert_eq!(hello["t"], "hello");
            assert_eq!(hello["hub_id"], hub.db.hub_id());
            assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
            conn
        }
    }

    type FakeState = (Arc<RelayShared>, UnboundedSender<FakeConn>);

    async fn fake_connect(State((shared, conns)): State<FakeState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let hub_id = headers.get("grouper-hub-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        shared.attempts.lock().unwrap().push((auth.clone(), hub_id.clone()));
        if auth != format!("Bearer {}", shared.expected.lock().unwrap()) || hub_id.is_empty() {
            return err(StatusCode::UNAUTHORIZED, "wrong hub secret");
        }
        shared.accepted.fetch_add(1, Ordering::SeqCst);
        ws.max_message_size(MAX_FRAME_BYTES).on_upgrade(move |mut socket| async move {
            let (to_hub, mut to_hub_rx) = unbounded_channel::<AxMsg>();
            let (from_hub_tx, from_hub) = unbounded_channel::<Value>();
            let _ = conns.send(FakeConn { to_hub, from_hub });
            loop {
                tokio::select! {
                    m = to_hub_rx.recv() => match m {
                        // The test dropped its end: vanish without a close frame.
                        None => return,
                        Some(m) => {
                            let close = matches!(m, AxMsg::Close(_));
                            if socket.send(m).await.is_err() || close {
                                // Let the close frame reach the hub before the socket goes.
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                return;
                            }
                        }
                    },
                    m = socket.recv() => match m {
                        Some(Ok(AxMsg::Text(t))) => {
                            let v: Value = serde_json::from_str(&t).unwrap();
                            match v["t"].as_str() {
                                Some("ping") => {
                                    shared.pings.fetch_add(1, Ordering::SeqCst);
                                    let _ = socket.send(AxMsg::Text(json!({"t": "pong"}).to_string().into())).await;
                                }
                                _ => {
                                    let _ = from_hub_tx.send(v);
                                }
                            }
                        }
                        Some(Ok(AxMsg::Close(_))) | Some(Err(_)) | None => return,
                        Some(Ok(_)) => {}
                    },
                }
            }
        })
    }

    async fn fake_claim(State((shared, _)): State<FakeState>, Json(body): Json<Value>) -> Response {
        shared.claims.lock().unwrap().push(body);
        if shared.fail_claim.load(Ordering::SeqCst) {
            return err(StatusCode::CONFLICT, "this account is already linked elsewhere");
        }
        *shared.expected.lock().unwrap() = LATER_SECRET.to_string();
        Json(json!({"hub_secret": LATER_SECRET, "account_email": "owner@example.com"})).into_response()
    }

    async fn fake_delete(State((shared, _)): State<FakeState>, headers: HeaderMap) -> Response {
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        shared.deletes.lock().unwrap().push(auth);
        Json(json!({"ok": true})).into_response()
    }

    // ------------------------------------------------------------ a fake loopback upstream

    #[derive(Default)]
    struct Upstream {
        /// Headers of every WebSocket handshake `/ws` accepted.
        ws_headers: Mutex<Vec<Vec<(String, String)>>>,
        /// Messages `/ws` received, as text (`bin:` for binary), and whether it was closed.
        ws_seen: Mutex<Vec<String>>,
        slow_dropped: AtomicBool,
    }

    struct DropFlag(Arc<Upstream>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.slow_dropped.store(true, Ordering::SeqCst);
        }
    }

    fn upstream_app(up: Arc<Upstream>) -> Router {
        let echo = |req: AxumRequest| async move {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let headers: Vec<Value> =
                parts.headers.iter().map(|(k, v)| json!([k.as_str(), v.to_str().unwrap_or("<binary>")])).collect();
            let mut resp = Json(json!({
                "method": parts.method.as_str(),
                "uri": parts.uri.to_string(),
                "headers": headers,
                "body": String::from_utf8_lossy(&bytes),
            }))
            .into_response();
            resp.headers_mut().append("set-cookie", "a=1; Path=/".parse().unwrap());
            resp.headers_mut().append("set-cookie", "b=2; Path=/; HttpOnly".parse().unwrap());
            resp.headers_mut().insert("x-custom-out", "yes".parse().unwrap());
            resp
        };
        let big = || async {
            let chunks: Vec<Result<Bytes, Infallible>> =
                (0..5u8).map(|i| Ok(Bytes::from(vec![b'a' + i; 100_000]))).collect();
            Body::from_stream(futures_util::stream::iter(chunks))
        };
        let slow = {
            let up = up.clone();
            move || {
                let guard = DropFlag(up.clone());
                async move {
                    let stream = futures_util::stream::unfold((0u32, guard), |(n, guard)| async move {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        Some((Ok::<_, Infallible>(Bytes::from(format!("tick{n};"))), (n + 1, guard)))
                    });
                    Body::from_stream(stream)
                }
            }
        };
        let redirect = || async { (StatusCode::FOUND, [("location", "/echo")], "moved").into_response() };
        let ws_echo = {
            let up = up.clone();
            move |headers: HeaderMap, ws: WebSocketUpgrade| {
                let up = up.clone();
                async move {
                    up.ws_headers.lock().unwrap().push(
                        headers.iter().map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())).collect(),
                    );
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(m)) = socket.recv().await {
                            match m {
                                AxMsg::Text(t) => {
                                    up.ws_seen.lock().unwrap().push(t.to_string());
                                    let _ = socket.send(AxMsg::Text(t)).await;
                                }
                                AxMsg::Binary(b) => {
                                    up.ws_seen.lock().unwrap().push(format!("bin:{}", hex::encode(&b)));
                                    let _ = socket.send(AxMsg::Binary(b)).await;
                                }
                                AxMsg::Close(f) => {
                                    up.ws_seen.lock().unwrap().push(format!("close:{}", f.map(|f| f.code).unwrap_or(0)));
                                    return;
                                }
                                _ => {}
                            }
                        }
                    })
                }
            }
        };
        let ws_bye = |ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                let _ = socket.send(AxMsg::Text("bye".into())).await;
                let _ = socket.send(AxMsg::Close(Some(AxumClose { code: 1001, reason: "going away".into() }))).await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
        };
        Router::new()
            .route("/echo", any(echo))
            .route("/big", get(big))
            .route("/slow", get(slow))
            .route("/redir", get(redirect))
            .route("/ws", get(ws_echo))
            .route("/ws-bye", get(ws_bye))
    }

    // ------------------------------------------------------------ fixtures

    fn fast_config() -> RelayConfig {
        RelayConfig {
            backoff_initial: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            ping_interval: Duration::from_millis(100),
            idle_timeout: Duration::from_secs(5),
            recheck: Duration::from_millis(100),
            stable_after: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(5),
        }
    }

    struct Fixture {
        hub: Arc<Hub>,
        relay: FakeRelay,
        /// The loopback listener the connector calls (the fake upstream, or the real router).
        addr: SocketAddr,
        shutdown: watch::Sender<bool>,
        task: JoinHandle<()>,
        dir: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
            self.task.abort();
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// A hub (not started through `host`), a listener serving `make(hub)` on loopback, a fake relay,
    /// and the connector supervisor pointed at both. `linked` stores the secret and turns relaying on.
    async fn fixture_with(linked: bool, make: impl FnOnce(&Arc<Hub>) -> Router) -> Fixture {
        let dir = std::env::temp_dir().join(format!("grouper-relay-test-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let hub = Hub::new(db, dir.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = make(&hub);
        tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await });
        let relay = FakeRelay::start(SECRET).await;
        hub.db.set_relay_url(Some(&relay.base)).unwrap();
        if linked {
            hub.db.set_relay_link(SECRET, "you@example.com").unwrap();
            hub.db.set_relay_enabled(true).unwrap();
        }
        hub.relay.set_config(fast_config());
        let (shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(supervisor_loop(hub.clone(), addr, rx));
        Fixture { hub, relay, addr, shutdown, task, dir }
    }

    async fn fixture(make: impl FnOnce(&Arc<Hub>) -> Router) -> Fixture {
        fixture_with(true, make).await
    }

    async fn echo_fixture() -> (Fixture, Arc<Upstream>) {
        let up = Arc::new(Upstream::default());
        let f = fixture(|_| upstream_app(up.clone())).await;
        (f, up)
    }

    async fn wait_status(hub: &Hub, want: RelayStatus) {
        let mut rx = hub.relay.subscribe();
        if tokio::time::timeout(WAIT, rx.wait_for(|s| s.status == want)).await.is_err() {
            panic!("status never became {want:?}; it is {:?}", hub.relay.snapshot());
        }
    }

    // ------------------------------------------------------------ the tunnel protocol

    #[tokio::test]
    async fn the_hub_connects_with_its_secret_and_id_and_pings() {
        let (mut f, _up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;
        {
            let attempts = f.relay.shared.attempts.lock().unwrap();
            assert_eq!(attempts[0].0, format!("Bearer {SECRET}"));
            assert_eq!(attempts[0].1, f.hub.db.hub_id());
        }
        // The hub pings on its own, and answers the relay's pings.
        tokio::time::timeout(WAIT, async {
            while f.relay.shared.pings.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("hub pings");
        conn.send(json!({"t": "ping"}));
        assert_eq!(conn.recv().await, json!({"t": "pong"}));
    }

    #[tokio::test]
    async fn requests_get_status_headers_multiple_cookies_and_body_back() {
        let (mut f, _up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        conn.req(
            7,
            "POST",
            "/echo?x=1&y=two",
            &[
                ("Host", "relay.example"),
                ("Origin", "https://relay.example"),
                ("Cookie", "a=b"),
                ("Authorization", "Bearer x"),
                ("X-Forwarded-For", "203.0.113.7"),
                ("CF-Connecting-IP", "127.0.0.1"),
                ("X-Grouper-Relayed", "0"),
                ("X-Forwarded-Proto", "https"),
                ("Connection", "keep-alive"),
                ("TE", "trailers"),
                ("Content-Length", "999"),
                ("Proxy-Authorization", "x"),
                ("X-Custom", "1"),
                ("Content-Type", "text/plain"),
            ],
            Some(b"hello"),
        );
        let resp = conn.response(7).await;
        assert_eq!((resp.status, resp.ended), (200, true));
        assert_eq!(resp.header("set-cookie"), ["a=1; Path=/", "b=2; Path=/; HttpOnly"]);
        assert_eq!(resp.header("x-custom-out"), ["yes"]);
        assert_eq!(resp.header("content-type"), ["application/json"]);
        for dropped in ["content-length", "transfer-encoding", "connection"] {
            assert!(resp.header(dropped).is_empty(), "{dropped} must not be passed on");
        }
        let seen = resp.json();
        assert_eq!(seen["method"], "POST");
        assert_eq!(seen["uri"], "/echo?x=1&y=two");
        assert_eq!(seen["body"], "hello");
        let seen_headers: Vec<(String, String)> =
            seen["headers"].as_array().unwrap().iter().map(|p| (p[0].as_str().unwrap().into(), p[1].as_str().unwrap().into())).collect();
        assert_eq!(values(&seen_headers, "host"), ["relay.example"], "the relay's host reaches the hub");
        assert_eq!(values(&seen_headers, "origin"), ["https://relay.example"]);
        assert_eq!(values(&seen_headers, "cookie"), ["a=b"]);
        assert_eq!(values(&seen_headers, "authorization"), ["Bearer x"]);
        assert_eq!(values(&seen_headers, "x-custom"), ["1"]);
        assert_eq!(values(&seen_headers, "x-forwarded-for"), ["203.0.113.7"]);
        assert_eq!(values(&seen_headers, "cf-connecting-ip"), ["203.0.113.7"]);
        assert_eq!(values(&seen_headers, "x-forwarded-proto"), ["https"]);
        assert_eq!(values(&seen_headers, "x-grouper-relayed"), ["1"]);
        assert!(values(&seen_headers, "te").is_empty() && values(&seen_headers, "proxy-authorization").is_empty());
        assert_eq!(values(&seen_headers, "content-length"), ["5"], "recomputed from the real body");

        // A forged loopback client address is replaced; a missing one is 0.0.0.0.
        conn.req(8, "GET", "/echo", &[("x-forwarded-for", "127.0.0.1")], None);
        let forged = conn.response(8).await.json();
        assert!(forged["headers"].as_array().unwrap().iter().any(|p| p[0] == "x-forwarded-for" && p[1] == "0.0.0.0"));
        assert!(forged["headers"].as_array().unwrap().iter().any(|p| p[0] == "cf-connecting-ip" && p[1] == "0.0.0.0"));
        // Without an x-forwarded-proto the relay's own scheme is used (the fake relay is http).
        assert!(forged["headers"].as_array().unwrap().iter().any(|p| p[0] == "x-forwarded-proto" && p[1] == "http"));
    }

    #[tokio::test]
    async fn redirects_are_passed_on_not_followed_and_errors_keep_their_status() {
        let (mut f, _up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        conn.req(1, "GET", "/redir", &[], None);
        let resp = conn.response(1).await;
        assert_eq!(resp.status, 302);
        assert_eq!(resp.header("location"), ["/echo"]);
        assert_eq!(resp.body(), b"moved");
        conn.req(2, "GET", "/nowhere", &[], None);
        assert_eq!(conn.response(2).await.status, 404);
        // A path that isn't a path is refused rather than sent anywhere.
        conn.req(3, "GET", "//evil.example/x", &[], None);
        assert!(conn.response(3).await.err.unwrap().contains("bad request path"));
        conn.req(4, "BAD METHOD", "/echo", &[], None);
        assert!(conn.response(4).await.err.unwrap().contains("bad method"));
    }

    #[tokio::test]
    async fn large_bodies_stream_back_in_chunks_of_at_most_48_kib() {
        let (mut f, _up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        conn.req(1, "GET", "/big", &[], None);
        let resp = conn.response(1).await;
        assert_eq!(resp.status, 200);
        assert!(resp.ended);
        assert!(resp.chunks.iter().all(|c| !c.is_empty() && c.len() <= CHUNK_BYTES));
        assert!(resp.chunks.len() >= 500_000usize.div_ceil(CHUNK_BYTES));
        let mut expected = Vec::new();
        for i in 0..5u8 {
            expected.extend(vec![b'a' + i; 100_000]);
        }
        assert_eq!(resp.body(), expected);

        // A big request body arrives whole.
        let body = vec![b'z'; 3 * 1024 * 1024];
        conn.req(2, "POST", "/echo", &[], Some(&body));
        assert_eq!(conn.response(2).await.json()["body"].as_str().unwrap().len(), body.len());
    }

    #[tokio::test]
    async fn requests_run_concurrently_and_cancel_stops_one() {
        let (mut f, up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        conn.req(1, "GET", "/slow", &[], None);
        // The slow body is still open when another request is answered completely.
        loop {
            let f1 = conn.recv().await;
            assert_eq!(f1["id"], 1);
            if f1["t"] == "data" {
                break;
            }
        }
        conn.req(2, "GET", "/echo", &[], None);
        let mut done = false;
        let mut ticks = 0;
        while !done {
            let frame = conn.recv().await;
            match (frame["id"].as_u64().unwrap(), frame["t"].as_str().unwrap()) {
                (2, "end") => done = true,
                (1, "data") => ticks += 1,
                (1, other) => panic!("slow response must not end: {other}"),
                _ => {}
            }
        }
        let _ = ticks;
        assert!(!up.slow_dropped.load(Ordering::SeqCst));

        conn.send(json!({"t": "cancel", "id": 1}));
        tokio::time::timeout(WAIT, async {
            while !up.slow_dropped.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cancel drops the hub request");
        // Frames already queued may still arrive; then nothing more for 1 (and no `end`).
        while conn.recv_within(Duration::from_millis(200)).await.is_some() {}
        assert!(conn.recv_within(Duration::from_millis(400)).await.is_none(), "nothing after cancel");
        // The connection is fine afterwards.
        conn.req(3, "GET", "/echo", &[], None);
        assert_eq!(conn.response(3).await.status, 200);
    }

    #[tokio::test]
    async fn websockets_carry_text_and_binary_with_the_forwarded_headers() {
        let (mut f, up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        conn.send(json!({"t": "ws_open", "id": 9, "path": "/ws?x=1", "headers": [
            ["host", "relay.example"], ["origin", "https://relay.example"], ["cookie", "grouper_session=abc"],
            ["authorization", "Bearer t"], ["x-forwarded-for", "203.0.113.8"],
            ["sec-websocket-key", "bogus"], ["sec-websocket-version", "8"], ["connection", "Upgrade"], ["upgrade", "websocket"]]}));
        assert_eq!(conn.recv().await, json!({"t": "ws_ok", "id": 9}));
        {
            let seen = up.ws_headers.lock().unwrap();
            let headers = &seen[0];
            assert_eq!(values(headers, "host"), ["relay.example"]);
            assert_eq!(values(headers, "origin"), ["https://relay.example"]);
            assert_eq!(values(headers, "cookie"), ["grouper_session=abc"]);
            assert_eq!(values(headers, "authorization"), ["Bearer t"]);
            assert_eq!(values(headers, "x-forwarded-for"), ["203.0.113.8"]);
            assert_eq!(values(headers, "x-grouper-relayed"), ["1"]);
            assert_ne!(values(headers, "sec-websocket-key"), ["bogus"], "the handshake makes its own key");
            assert_eq!(values(headers, "upgrade"), ["websocket"]);
        }
        conn.send(json!({"t": "ws_msg", "id": 9, "text": "héllo"}));
        assert_eq!(conn.recv().await, json!({"t": "ws_msg", "id": 9, "text": "héllo"}));
        conn.send(json!({"t": "ws_msg", "id": 9, "b64": B64.encode([0u8, 1, 2, 255])}));
        assert_eq!(conn.recv().await, json!({"t": "ws_msg", "id": 9, "b64": B64.encode([0u8, 1, 2, 255])}));
        conn.send(json!({"t": "ws_close", "id": 9, "code": 1000, "reason": "done"}));
        tokio::time::timeout(WAIT, async {
            while !up.ws_seen.lock().unwrap().iter().any(|m| m == "close:1000") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub side sees the close");
        assert!(conn.recv_within(Duration::from_millis(300)).await.is_none(), "no echo of a close the relay asked for");

        // The hub side closing is passed on, after its last message.
        conn.send(json!({"t": "ws_open", "id": 10, "path": "/ws-bye", "headers": []}));
        assert_eq!(conn.recv().await, json!({"t": "ws_ok", "id": 10}));
        assert_eq!(conn.recv().await, json!({"t": "ws_msg", "id": 10, "text": "bye"}));
        assert_eq!(conn.recv().await, json!({"t": "ws_close", "id": 10, "code": 1001, "reason": "going away"}));

        // A WebSocket the hub refuses is a ws_fail with its status.
        conn.send(json!({"t": "ws_open", "id": 11, "path": "/echo", "headers": []}));
        let fail = conn.recv().await;
        assert_eq!((fail["t"].as_str(), fail["id"].as_u64()), (Some("ws_fail"), Some(11)));
        assert!(fail["status"].as_u64().unwrap() >= 400);
        conn.send(json!({"t": "ws_open", "id": 12, "path": "bad", "headers": []}));
        let fail = conn.recv().await;
        assert_eq!((fail["t"].as_str(), fail["status"].as_u64()), (Some("ws_fail"), Some(400)));
    }

    // ------------------------------------------------------------ against a real hub

    #[tokio::test]
    async fn relayed_requests_look_remote_to_the_hub() {
        let mut f = fixture(|hub| {
            hub.set_local_mode(true);
            router(hub.clone())
        })
        .await;
        let (_, admin) = f.hub.db.add_device("Laptop", "windows", true).unwrap();
        let mut conn = f.relay.accept(&f.hub).await;

        conn.req(1, "GET", "/healthz", &[], None);
        let resp = conn.response(1).await;
        assert_eq!((resp.status, resp.body()), (200, b"ok".to_vec()));
        conn.req(2, "GET", "/v1/hub", &[("accept", "application/json")], None);
        let info = conn.response(2).await;
        assert_eq!(info.status, 200);
        assert_eq!(info.json()["hub_device_id"], f.hub.db.hub_device_id());

        // Bearer auth works through the relay: the hub checks the token itself.
        conn.req(3, "GET", "/v1/session", &[("authorization", &format!("Bearer {admin}"))], None);
        assert_eq!(conn.response(3).await.json()["signed_in"], true);
        conn.req(4, "GET", "/v1/session", &[("authorization", "Bearer nope")], None);
        assert_eq!(conn.response(4).await.json()["signed_in"], false);

        // Token sign-in is loopback-only in local mode: refused through the relay however the
        // client address is spoofed, fine straight at the loopback listener.
        fn login(spoof: Option<&str>) -> Vec<(&str, &str)> {
            let mut headers = vec![
                ("host", "relay.example"),
                ("origin", "https://relay.example"),
                ("x-grouper", "1"),
                ("content-type", "application/json"),
            ];
            if let Some(ip) = spoof {
                headers.push(("x-forwarded-for", ip));
            }
            headers
        }
        let body = json!({"token": admin}).to_string();
        for (id, spoof) in [(5, Some("203.0.113.9")), (6, None), (7, Some("127.0.0.1")), (8, Some("::1"))] {
            conn.req(id, "POST", "/v1/session", &login(spoof), Some(body.as_bytes()));
            let resp = conn.response(id).await;
            assert_eq!(resp.status, 403, "{spoof:?}: {}", String::from_utf8_lossy(&resp.body()));
            assert!(resp.json()["error"].as_str().unwrap().contains("loopback"));
            assert!(resp.header("set-cookie").is_empty());
        }
        let direct = reqwest::Client::new()
            .post(format!("http://{}/v1/session", f.addr))
            .header("x-grouper", "1")
            .header("origin", format!("http://{}", f.addr))
            .json(&json!({"token": admin}))
            .send()
            .await
            .unwrap();
        assert_eq!(direct.status(), 200);
        assert!(direct.headers().get("set-cookie").is_some());
    }

    #[tokio::test]
    async fn events_websocket_works_through_the_relay_with_a_token_or_a_session() {
        let mut f = fixture(|hub| router(hub.clone())).await;
        let (_, admin) = f.hub.db.add_device("Laptop", "windows", true).unwrap();
        let mut conn = f.relay.accept(&f.hub).await;

        // With a device token.
        conn.send(json!({"t": "ws_open", "id": 1, "path": "/v1/events",
            "headers": [["authorization", format!("Bearer {admin}")], ["host", "relay.example"]]}));
        assert_eq!(conn.recv().await, json!({"t": "ws_ok", "id": 1}));
        f.hub.emit_live("c1", None, core_protocol::EventBody::AssistantDelta { text: "hello over the relay".into() });
        let msg = conn.recv().await;
        assert_eq!((msg["t"].as_str(), msg["id"].as_u64()), (Some("ws_msg"), Some(1)));
        let event: Value = serde_json::from_str(msg["text"].as_str().unwrap()).unwrap();
        assert_eq!(event["conversation_id"], "c1");
        assert!(msg["text"].as_str().unwrap().contains("hello over the relay"));
        conn.send(json!({"t": "ws_close", "id": 1, "code": 1000, "reason": ""}));

        // Without a credential the hub refuses, and the relay hears 401.
        conn.send(json!({"t": "ws_open", "id": 2, "path": "/v1/events", "headers": [["host", "relay.example"]]}));
        let fail = conn.recv().await;
        assert_eq!((fail["t"].as_str(), fail["status"].as_u64()), (Some("ws_fail"), Some(401)));

        // With a session cookie: the hub's own CSRF rule wants an Origin that matches the Host the
        // relay forwarded (the relay's address), which is why `host` and `origin` pass through.
        let login = reqwest::Client::new()
            .post(format!("http://{}/v1/session", f.addr))
            .header("x-grouper", "1")
            .header("origin", format!("http://{}", f.addr))
            .json(&json!({"token": admin}))
            .send()
            .await
            .unwrap();
        let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
        conn.send(json!({"t": "ws_open", "id": 3, "path": "/v1/events", "headers": [
            ["host", "relay.example"], ["origin", "https://relay.example"], ["cookie", cookie]]}));
        assert_eq!(conn.recv().await, json!({"t": "ws_ok", "id": 3}));
        conn.send(json!({"t": "ws_open", "id": 4, "path": "/v1/events", "headers": [
            ["host", "relay.example"], ["origin", "https://evil.example"], ["cookie", cookie]]}));
        let fail = conn.recv().await;
        assert_eq!((fail["t"].as_str(), fail["id"].as_u64(), fail["status"].as_u64()), (Some("ws_fail"), Some(4), Some(403)));
    }

    // ------------------------------------------------------------ the supervisor

    #[tokio::test]
    async fn the_hub_reconnects_after_the_relay_drops_it() {
        let (mut f, _up) = echo_fixture().await;
        let conn = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;
        drop(conn); // the fake relay's socket vanishes without a close frame
        wait_status(&f.hub, RelayStatus::Failed).await;
        assert!(f.hub.relay.snapshot().last_error.is_some());
        let mut again = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;
        assert!(f.hub.relay.snapshot().last_error.is_none());
        again.req(1, "GET", "/echo", &[], None);
        assert_eq!(again.response(1).await.status, 200);
        assert_eq!(f.relay.shared.accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_clean_relay_close_also_reconnects() {
        let (mut f, _up) = echo_fixture().await;
        let conn = f.relay.accept(&f.hub).await;
        conn.to_hub.send(AxMsg::Close(Some(AxumClose { code: 1001, reason: "restarting".into() }))).unwrap();
        let _again = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;
    }

    #[tokio::test]
    async fn code_4001_means_replaced_and_stops_reconnecting() {
        let (mut f, _up) = echo_fixture().await;
        let conn = f.relay.accept(&f.hub).await;
        conn.to_hub.send(AxMsg::Close(Some(AxumClose { code: 4001, reason: "replaced".into() }))).unwrap();
        wait_status(&f.hub, RelayStatus::Failed).await;
        let error = f.hub.relay.snapshot().last_error.unwrap();
        assert!(error.contains("replaced"), "{error}");
        // No hot loop: with a 20 ms backoff a retry would show up almost at once.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(f.relay.shared.attempts.lock().unwrap().len(), 1, "no reconnect after 4001");
        assert_eq!(f.hub.relay.snapshot().status, RelayStatus::Failed);
        // Linking again (any settings change) lets it try once more.
        f.hub.relay.notify_changed();
        let _conn = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;
    }

    #[tokio::test]
    async fn a_refused_secret_is_reported_and_retried_with_backoff() {
        let mut f = fixture(|_| upstream_app(Arc::default())).await;
        *f.relay.shared.expected.lock().unwrap() = "some other secret".into();
        wait_status(&f.hub, RelayStatus::Failed).await;
        let error = f.hub.relay.snapshot().last_error.unwrap();
        assert!(error.contains("401") && !error.contains(SECRET), "{error}");
        tokio::time::timeout(WAIT, async {
            while f.relay.shared.attempts.lock().unwrap().len() < 3 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("keeps retrying");
        // Once the relay accepts the secret again, the hub gets in.
        *f.relay.shared.expected.lock().unwrap() = SECRET.into();
        let _conn = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;
    }

    #[tokio::test]
    async fn turning_relaying_off_or_stepping_down_closes_the_socket_and_back_on_reconnects() {
        let (mut f, _up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        f.hub.db.set_relay_enabled(false).unwrap();
        f.hub.relay.notify_changed();
        assert!(conn.closed().await, "the hub closes its socket");
        wait_status(&f.hub, RelayStatus::Off).await;
        let payload = status_payload(&f.hub);
        assert_eq!((payload["status"].as_str(), payload["enabled"].as_bool()), (Some("off"), Some(false)));

        f.hub.db.set_relay_enabled(true).unwrap();
        f.hub.relay.notify_changed();
        let mut conn = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;

        // A stepped-down hub leaves the relay to the newer one.
        let info = crate::stepdown::StepDownInfo {
            account: "a".into(),
            local_hub_device_id: "x".into(),
            local_epoch: 1,
            peer_hub_device_id: "y".into(),
            peer_epoch: 2,
            address: "https://peer.example".into(),
            detected_at: 0,
        };
        assert!(f.hub.stepdown.mark_stepped_down(info));
        f.hub.relay.notify_changed();
        assert!(conn.closed().await);
        wait_status(&f.hub, RelayStatus::Off).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(f.relay.shared.accepted.load(Ordering::SeqCst), 2, "a stepped-down hub does not reconnect");
    }

    #[tokio::test]
    async fn settings_changed_behind_the_hubs_back_are_noticed() {
        // `grouper-hub relay unlink` edits the database while the hub runs and can't notify it.
        let (mut f, _up) = echo_fixture().await;
        let mut conn = f.relay.accept(&f.hub).await;
        f.hub.db.clear_relay_link().unwrap();
        assert!(conn.closed().await);
        wait_status(&f.hub, RelayStatus::Off).await;
    }

    #[tokio::test]
    async fn an_unlinked_or_disabled_hub_never_connects() {
        let f = fixture_with(false, |_| upstream_app(Arc::default())).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(f.relay.shared.attempts.lock().unwrap().is_empty());
        assert_eq!(f.hub.relay.snapshot().status, RelayStatus::Off);
        let status = status_payload(&f.hub);
        assert_eq!(status["linked"], false);
        assert_eq!(status["status"], "off");
    }

    #[tokio::test]
    async fn host_start_runs_the_connector_against_its_own_loopback_listener() {
        let dir = std::env::temp_dir().join(format!("grouper-relay-host-{}", random_hex(4)));
        let handle = crate::host::start(&dir, &["127.0.0.1:0".parse().unwrap()]).await.unwrap();
        let hub = handle.hub().unwrap();
        let mut relay = FakeRelay::start(SECRET).await;
        hub.relay.set_config(fast_config());
        hub.db.set_relay_url(Some(&relay.base)).unwrap();
        hub.db.set_relay_link(SECRET, "me@example.com").unwrap();
        hub.db.set_relay_enabled(true).unwrap();
        hub.relay.notify_changed();
        let mut conn = relay.accept(&hub).await;
        conn.req(1, "GET", "/healthz", &[], None);
        assert_eq!(conn.response(1).await.body(), b"ok");
        handle.stop().await;
        assert_eq!(hub.relay.snapshot().status, RelayStatus::Off, "stop shuts the connector down");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------ admin endpoints

    struct Admin {
        http: reqwest::Client,
        base: String,
        token: String,
    }

    impl Admin {
        async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (StatusCode, String) {
            let mut rb = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
            if let Some(body) = body {
                rb = rb.json(&body);
            }
            let resp = rb.send().await.unwrap();
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
            (status, resp.text().await.unwrap())
        }
    }

    fn json_of(text: &str) -> Value {
        serde_json::from_str(text).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn admin_endpoints_link_unlink_and_never_return_the_secret() {
        let mut f = fixture_with(false, |hub| router(hub.clone())).await;
        crate::google_auth::tests::configure_hub_for_test(&f.hub).await;
        let owner_sub = "100000000000000000009";
        f.hub
            .db
            .set_owner_if_none(&Owner {
                provider: "google".into(),
                sub: owner_sub.into(),
                email: "owner@example.com".into(),
                name: "Owner".into(),
                claimed_at: 1,
            })
            .unwrap();
        let (_, admin_token) = f.hub.db.add_device("Laptop", "windows", true).unwrap();
        let (_, plain_token) = f.hub.db.add_device("Node", "linux", false).unwrap();
        let base = format!("http://{}", f.addr);
        let admin = Admin { http: reqwest::Client::new(), base: base.clone(), token: admin_token };
        let mut transcript = Vec::new();
        use reqwest::Method as M;

        // Credentials: admin only.
        let anon = reqwest::Client::new().get(format!("{base}/v1/relay")).send().await.unwrap();
        assert_eq!(anon.status(), 401);
        let plain = Admin { token: plain_token, http: reqwest::Client::new(), base: base.clone() };
        for (m, p) in [(M::GET, "/v1/relay"), (M::PUT, "/v1/relay"), (M::DELETE, "/v1/relay"), (M::POST, "/v1/relay/nonce")] {
            assert_eq!(plain.call(m.clone(), p, Some(json!({}))).await.0, 403, "{m} {p}");
        }
        assert_eq!(plain.call(M::POST, "/v1/relay/link", Some(json!({"id_token": "x"}))).await.0, 403);

        // Status before anything is set up.
        let (status, text) = admin.call(M::GET, "/v1/relay", None).await;
        transcript.push(text.clone());
        assert_eq!(status, 200);
        let s = json_of(&text);
        assert_eq!((s["enabled"].as_bool(), s["linked"].as_bool(), s["status"].as_str()), (Some(false), Some(false), Some("off")));
        assert_eq!(s["url"], f.relay.base, "the fixture pointed the hub at the fake relay");
        assert_eq!(s["account_email"], Value::Null);

        // Settings: bad addresses are refused, good ones are normalized, null clears.
        assert_eq!(admin.call(M::PUT, "/v1/relay", Some(json!({"url": "nonsense"}))).await.0, 400);
        let (status, text) =
            admin.call(M::PUT, "/v1/relay", Some(json!({"url": format!("{}/", f.relay.base.replace("http", "ws"))}))).await;
        transcript.push(text.clone());
        assert_eq!(status, 200);
        assert_eq!(json_of(&text)["url"], f.relay.base);
        let (_, text) = admin.call(M::PUT, "/v1/relay", Some(json!({"url": null}))).await;
        assert_eq!(json_of(&text)["url"], Value::Null);
        assert_eq!(admin.call(M::POST, "/v1/relay/nonce", None).await.0, 400, "needs an address");
        assert_eq!(admin.call(M::POST, "/v1/relay/link", Some(json!({"id_token": "x"}))).await.0, 400);
        let (_, text) = admin.call(M::PUT, "/v1/relay", Some(json!({"url": f.relay.base.clone(), "enabled": true}))).await;
        transcript.push(text.clone());
        let s = json_of(&text);
        assert_eq!((s["enabled"].as_bool(), s["linked"].as_bool(), s["status"].as_str()), (Some(true), Some(false), Some("off")));

        // The nonce comes from the relay.
        let (status, text) = admin.call(M::POST, "/v1/relay/nonce", None).await;
        transcript.push(text.clone());
        assert_eq!(status, 200);
        assert_eq!(json_of(&text)["nonce"], "rn.fake.1.2.3");
        assert!(json_of(&text)["expires"].as_u64().unwrap() > 0);

        // Linking needs the owner's token: junk, a stranger and an unsigned token never reach the relay.
        let (status, _) = admin.call(M::POST, "/v1/relay/link", Some(json!({"id_token": "junk"}))).await;
        assert_eq!(status, 401);
        let stranger = crate::google_auth::tests::mint_for_test("n", "100000000000000000010");
        let (status, text) = admin.call(M::POST, "/v1/relay/link", Some(json!({"id_token": stranger}))).await;
        assert_eq!(status, 403, "{text}");
        assert!(f.relay.shared.claims.lock().unwrap().is_empty());
        assert!(!f.hub.db.has_relay_secret());

        // A relay that refuses the claim leaves the hub unlinked and says why.
        let owner_token = crate::google_auth::tests::mint_for_test("n", owner_sub);
        f.relay.shared.fail_claim.store(true, Ordering::SeqCst);
        let (status, text) = admin.call(M::POST, "/v1/relay/link", Some(json!({"id_token": owner_token}))).await;
        assert_eq!(status, 502);
        assert!(json_of(&text)["error"].as_str().unwrap().contains("already linked elsewhere"), "{text}");
        assert!(!f.hub.db.has_relay_secret());
        f.relay.shared.fail_claim.store(false, Ordering::SeqCst);

        // The owner's token links the hub: the relay gets {hub_id, id_token}, the secret is stored
        // sealed, relaying turns on and the connector connects with the new secret.
        let (status, text) = admin.call(M::POST, "/v1/relay/link", Some(json!({"id_token": owner_token}))).await;
        transcript.push(text.clone());
        assert_eq!(status, 200, "{text}");
        let s = json_of(&text);
        assert_eq!((s["linked"].as_bool(), s["enabled"].as_bool(), s["account_email"].as_str()), (Some(true), Some(true), Some("owner@example.com")));
        {
            let claims = f.relay.shared.claims.lock().unwrap();
            let claim = claims.last().unwrap();
            assert_eq!(claim["hub_id"], f.hub.db.hub_id());
            assert_eq!(claim["id_token"], owner_token);
            assert_eq!(claim.as_object().unwrap().len(), 2);
        }
        assert_eq!(f.hub.db.relay_secret().unwrap().as_deref(), Some(LATER_SECRET));
        let mut conn = f.relay.accept(&f.hub).await;
        wait_status(&f.hub, RelayStatus::Connected).await;
        {
            let attempts = f.relay.shared.attempts.lock().unwrap();
            assert_eq!(attempts.last().unwrap().0, format!("Bearer {LATER_SECRET}"));
        }
        let (_, text) = admin.call(M::GET, "/v1/relay", None).await;
        transcript.push(text.clone());
        assert_eq!(json_of(&text)["status"], "connected");

        // The address can't be swapped under a link.
        let (status, _) = admin.call(M::PUT, "/v1/relay", Some(json!({"url": "https://other.example"}))).await;
        assert_eq!(status, 409);
        let (status, text) = admin.call(M::PUT, "/v1/relay", Some(json!({"url": f.relay.base.clone(), "enabled": true}))).await;
        transcript.push(text);
        assert_eq!(status, 200, "the same address is fine");

        // Unlinking tells the relay, forgets the secret and closes the socket.
        let (status, text) = admin.call(M::DELETE, "/v1/relay", None).await;
        transcript.push(text.clone());
        assert_eq!(status, 200);
        let s = json_of(&text);
        assert_eq!((s["linked"].as_bool(), s["enabled"].as_bool(), s["status"].as_str()), (Some(false), Some(false), Some("off")));
        assert_eq!(f.relay.shared.deletes.lock().unwrap().as_slice(), [format!("Bearer {LATER_SECRET}")]);
        assert!(!f.hub.db.has_relay_secret());
        assert!(conn.closed().await);
        wait_status(&f.hub, RelayStatus::Off).await;

        // No endpoint ever put either secret on the wire to an admin.
        for text in &transcript {
            assert!(!text.contains(SECRET) && !text.contains(LATER_SECRET) && !text.contains("hub_secret"), "leaked: {text}");
        }
    }

    #[tokio::test]
    async fn the_relay_unlink_helper_clears_the_link_even_when_the_relay_is_gone() {
        let dir = std::env::temp_dir().join(format!("grouper-relay-unlink-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.set_relay_url(Some("http://127.0.0.1:9")).unwrap();
        db.set_relay_link(SECRET, "me@example.com").unwrap();
        db.set_relay_enabled(true).unwrap();
        assert!(!unlink(&db).await.unwrap(), "relay unreachable: not confirmed");
        assert!(!db.has_relay_secret() && !db.relay_enabled());
        assert_eq!(db.relay_url().as_deref(), Some("http://127.0.0.1:9"), "the address stays for linking again");
        std::fs::remove_dir_all(&dir).ok();
    }
}
