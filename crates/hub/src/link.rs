//! Device link: a headless device (the CLI) signs in through a browser where the owner is
//! already signed in, like `gh auth login` (`docs/accounts-design.md`, "Contract (hub ↔ clients)").
//!
//! - `POST /v1/auth/link/start` `{name, platform, install_id?}` (no auth; a few per IP per minute and
//!   at most [`MAX_PENDING`] waiting at once) → `{code, poll_secret, verify_url, expires_in, interval}`.
//! - `GET /v1/auth/link/{code}` (admin): the request, for the web client's `#/link?code=…` page.
//! - `POST /v1/auth/link/{code}/approve` or `/deny` (admin; a cookie needs the usual CSRF header and
//!   same-site origin). Approving makes a non-admin device, or reuses the live one of the same
//!   `install_id` (fresh token, old one revoked).
//! - `POST /v1/auth/link/poll` `{poll_secret}` → `pending`, `denied`, `expired`, or exactly once
//!   `approved` with the token and device. Polls faster than `interval` get 429.
//!
//! Codes are 8 characters from the setup-code alphabet (no 0/O, 1/I/L), shown `ABCD-EFGH`, single-use
//! and good for 10 minutes. Codes and poll secrets are stored as SHA-256 hashes. The token made on
//! approval is kept sealed with the master key until the poll that delivers it, then deleted.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Extensions, HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::db::{Install, NODE_INSTALL, now, random_hex};
use crate::google_auth::{CODE_ALPHABET, client_ip};
use crate::registry::deny_unless_admin;
use crate::server::{Hub, MAX_DEVICES, check_device_name, check_install_id, check_platform, err, issue_error};

/// How long a link request lasts.
pub const LINK_TTL_SECS: u64 = 600;
/// How often the device polls, in seconds.
pub const POLL_INTERVAL_SECS: u64 = 3;
/// Link requests waiting for an answer at once, from anyone.
pub const MAX_PENDING: i64 = 20;
/// `start` calls per client IP per minute.
const STARTS_PER_MINUTE: u32 = 5;
const MAX_RATE_ENTRIES: usize = 4096;
const CODE_LEN: usize = 8;
/// Polls closer together than this are refused (the interval, less some network jitter).
const MIN_POLL_GAP: Duration = Duration::from_millis(POLL_INTERVAL_SECS * 1000 - 500);

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS link_requests (
  code_hash TEXT PRIMARY KEY, poll_hash TEXT NOT NULL UNIQUE, name TEXT NOT NULL, platform TEXT NOT NULL,
  install_id TEXT, created INTEGER NOT NULL, expires INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending',
  device_id TEXT, token TEXT);
";

/// Rate limits, kept in memory.
pub(crate) struct LinkState {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Per client IP: start of the current one-minute window and starts in it.
    per_ip: HashMap<IpAddr, (u64, u32)>,
    /// Per poll secret hash: when it last polled.
    polls: HashMap<String, Instant>,
    min_poll_gap: Duration,
}

impl Default for LinkState {
    fn default() -> Self {
        LinkState { inner: Mutex::new(Inner { per_ip: HashMap::new(), polls: HashMap::new(), min_poll_gap: MIN_POLL_GAP }) }
    }
}

impl LinkState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `ip` may start another link now (and count it).
    fn allow_start(&self, ip: IpAddr) -> bool {
        let now = now();
        let mut s = self.lock();
        if s.per_ip.len() >= MAX_RATE_ENTRIES {
            s.per_ip.retain(|_, (start, _)| now < *start + 60);
            if s.per_ip.len() >= MAX_RATE_ENTRIES && !s.per_ip.contains_key(&ip) {
                return false;
            }
        }
        let window = s.per_ip.entry(ip).or_insert((now, 0));
        if now >= window.0 + 60 {
            *window = (now, 0);
        }
        if window.1 >= STARTS_PER_MINUTE {
            return false;
        }
        window.1 += 1;
        true
    }

    /// Whether this poll secret may poll now (and note it).
    fn allow_poll(&self, poll_hash: &str) -> bool {
        let at = Instant::now();
        let mut s = self.lock();
        let gap = s.min_poll_gap;
        if s.polls.len() >= MAX_RATE_ENTRIES {
            let ttl = Duration::from_secs(LINK_TTL_SECS);
            s.polls.retain(|_, last| at.duration_since(*last) < ttl);
        }
        if let Some(last) = s.polls.get(poll_hash)
            && at.duration_since(*last) < gap
        {
            return false;
        }
        s.polls.insert(poll_hash.to_string(), at);
        true
    }

    #[cfg(test)]
    pub(crate) fn set_min_poll_gap(&self, gap: Duration) {
        self.lock().min_poll_gap = gap;
    }
}

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// A fresh code, e.g. `K7PM-3XQR`.
fn random_code() -> String {
    let mut chars = Vec::with_capacity(CODE_LEN);
    while chars.len() < CODE_LEN {
        let mut buf = [0u8; 16];
        getrandom::fill(&mut buf).expect("system randomness");
        // 248 = 8 * 31: rejecting the rest keeps every character equally likely.
        chars.extend(buf.iter().filter(|&&b| b < 248).map(|&b| CODE_ALPHABET[b as usize % CODE_ALPHABET.len()] as char));
    }
    chars.truncate(CODE_LEN);
    let s: String = chars.into_iter().collect();
    format!("{}-{}", &s[..4], &s[4..])
}

/// A code as typed (any case, dashes or spaces) in canonical form, if it could be one.
fn normalize_code(code: &str) -> Option<String> {
    let s: String = code.chars().filter(|c| !matches!(c, '-' | ' ')).map(|c| c.to_ascii_uppercase()).collect();
    (s.len() == CODE_LEN && s.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(s)
}

fn display_code(normalized: &str) -> String {
    format!("{}-{}", &normalized[..4], &normalized[4..])
}

/// The hub's address for the approval link: `GROUPER_PUBLIC_URL`, else the address the device used.
fn base_url(hub: &Hub, headers: &HeaderMap) -> String {
    if let Some(url) = hub.google.config().public_url {
        return url;
    }
    match headers.get(header::HOST).and_then(|v| v.to_str().ok()).map(str::trim).filter(|h| !h.is_empty()) {
        Some(host) => format!("{}://{host}", if crate::session::is_https(headers) { "https" } else { "http" }),
        None => String::new(),
    }
}

fn internal(e: impl std::fmt::Display) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, e)
}

/// One link request as the database has it.
struct Request {
    code_hash: String,
    name: String,
    platform: String,
    install_id: Option<String>,
    created: u64,
    expires: u64,
    status: String,
    device_id: Option<String>,
}

fn request_row(r: &rusqlite::Row) -> rusqlite::Result<Request> {
    Ok(Request {
        code_hash: r.get(0)?,
        name: r.get(1)?,
        platform: r.get(2)?,
        install_id: r.get(3)?,
        created: r.get::<_, i64>(4)? as u64,
        expires: r.get::<_, i64>(5)? as u64,
        status: r.get(6)?,
        device_id: r.get(7)?,
    })
}
const REQUEST_COLS: &str = "code_hash, name, platform, install_id, created, expires, status, device_id";

/// The unexpired request for a code typed in a URL.
fn find_by_code(hub: &Hub, code: &str) -> Option<(String, Request)> {
    let code = normalize_code(code)?;
    let req = hub
        .db
        .c()
        .query_row(
            &format!("SELECT {REQUEST_COLS} FROM link_requests WHERE code_hash = ?1 AND expires > ?2"),
            params![sha(&code), now() as i64],
            request_row,
        )
        .optional()
        .ok()
        .flatten()?;
    Some((code, req))
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "no device is waiting with that code; it may have expired (codes last 10 minutes)")
}

fn already(status: &str) -> Response {
    let what = if status == "denied" { "denied" } else { "approved" };
    err(StatusCode::CONFLICT, format!("this request was already {what}; each code works once"))
}

#[derive(Deserialize)]
struct StartBody {
    name: String,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    install_id: Option<String>,
}

/// `POST /v1/auth/link/start`: a device asks to be linked.
async fn start(State(hub): State<Arc<Hub>>, headers: HeaderMap, ext: Extensions, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<StartBody>(&body) else {
        return err(StatusCode::BAD_REQUEST, r#"body must be {"name": "...", "platform": "windows", "install_id": "..."}"#);
    };
    let (name, platform, install_id) =
        match (check_device_name(&req.name), check_platform(req.platform.as_deref()), check_install_id(req.install_id.as_deref())) {
            (Ok(n), Ok(p), Ok(i)) => (n, p.to_lowercase(), i),
            (Err(why), _, _) | (_, Err(why), _) | (_, _, Err(why)) => return err(StatusCode::BAD_REQUEST, why),
        };
    if !hub.link.allow_start(client_ip(&headers, &ext)) {
        return err(StatusCode::TOO_MANY_REQUESTS, "too many sign-in requests from here; wait a minute");
    }
    let created = now();
    let expires = created + LINK_TTL_SECS;
    let poll_secret = random_hex(32);
    let code = {
        let c = hub.db.c();
        if let Err(e) = c.execute("DELETE FROM link_requests WHERE expires <= ?1", [created as i64]) {
            return internal(e);
        }
        let pending: i64 = match c.query_row(
            "SELECT COUNT(*) FROM link_requests WHERE status IN ('pending', 'approving')",
            [],
            |r| r.get(0),
        ) {
            Ok(n) => n,
            Err(e) => return internal(e),
        };
        if pending >= MAX_PENDING {
            return err(StatusCode::TOO_MANY_REQUESTS, "too many devices are waiting to be linked; try again in a few minutes");
        }
        let mut made = None;
        for _ in 0..5 {
            let code = random_code();
            let inserted = c.execute(
                "INSERT OR IGNORE INTO link_requests (code_hash, poll_hash, name, platform, install_id, created, expires)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    sha(&normalize_code(&code).expect("well formed")),
                    sha(&poll_secret),
                    name,
                    platform,
                    install_id,
                    created as i64,
                    expires as i64
                ],
            );
            match inserted {
                Ok(1) => {
                    made = Some(code);
                    break;
                }
                Ok(_) => continue,
                Err(e) => return internal(e),
            }
        }
        match made {
            Some(code) => code,
            None => return internal("couldn't make a unique code; try again"),
        }
    };
    tracing::info!(name = %name, platform = %platform, "device link requested");
    Json(json!({
        "code": code,
        "poll_secret": poll_secret,
        "verify_url": format!("{}/#/link?code={code}", base_url(&hub, &headers)),
        "expires_in": LINK_TTL_SECS,
        "interval": POLL_INTERVAL_SECS,
    }))
    .into_response()
}

/// `GET /v1/auth/link/{code}` (admin): what is asking to be linked.
async fn show(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(code): Path<String>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some((code, req)) = find_by_code(&hub, &code) else { return not_found() };
    let status = match req.status.as_str() {
        "pending" | "approving" => "pending",
        "denied" => "denied",
        _ => "approved",
    };
    let replaces = match (&req.install_id, status) {
        (Some(id), "pending") => hub.db.install_device_name(Install { id, kind: NODE_INSTALL }, false),
        _ => None,
    };
    Json(json!({
        "code": display_code(&code),
        "name": req.name,
        "platform": req.platform,
        "requested_at": req.created,
        "expires": req.expires,
        "status": status,
        "replaces": replaces,
    }))
    .into_response()
}

/// `POST /v1/auth/link/{code}/approve` (admin): make (or reuse) the device; its token waits for the poll.
async fn approve(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(code): Path<String>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some((_, req)) = find_by_code(&hub, &code) else { return not_found() };
    // Claim the request first, so two approvals can't both make a device.
    let claimed = hub.db.c().execute(
        "UPDATE link_requests SET status = 'approving' WHERE code_hash = ?1 AND status = 'pending' AND expires > ?2",
        params![req.code_hash, now() as i64],
    );
    match claimed {
        Ok(1) => {}
        Ok(_) => return already(&req.status),
        Err(e) => return internal(e),
    }
    let release = |hub: &Hub| {
        let _ = hub.db.c().execute(
            "UPDATE link_requests SET status = 'pending' WHERE code_hash = ?1 AND status = 'approving'",
            [&req.code_hash],
        );
    };
    let install = req.install_id.as_deref().map(|id| Install { id, kind: NODE_INSTALL });
    let issued = match hub.issue_device(&req.name, &req.platform, false, install, Some(MAX_DEVICES)).await {
        Ok(issued) => issued,
        Err(e) => {
            release(&hub);
            return issue_error(e);
        }
    };
    let sealed = match hub.db.seal(&issued.token) {
        Ok(s) => s,
        Err(e) => {
            release(&hub);
            return internal(e);
        }
    };
    if let Err(e) = hub.db.c().execute(
        "UPDATE link_requests SET status = 'approved', device_id = ?1, token = ?2 WHERE code_hash = ?3",
        params![issued.device.id, sealed, req.code_hash],
    ) {
        return internal(e);
    }
    let d = issued.device;
    tracing::info!(device = %d.id, name = %d.name, reused = issued.reused, "device link approved");
    Json(json!({
        "status": "approved",
        "device": {"id": d.id, "name": d.name, "platform": d.platform, "admin": d.admin},
    }))
    .into_response()
}

/// `POST /v1/auth/link/{code}/deny` (admin).
async fn deny(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(code): Path<String>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some((_, req)) = find_by_code(&hub, &code) else { return not_found() };
    match hub.db.c().execute(
        "UPDATE link_requests SET status = 'denied' WHERE code_hash = ?1 AND status = 'pending' AND expires > ?2",
        params![req.code_hash, now() as i64],
    ) {
        Ok(1) => {
            tracing::info!(name = %req.name, "device link denied");
            Json(json!({"status": "denied"})).into_response()
        }
        Ok(_) => already(&req.status),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct PollBody {
    poll_secret: String,
}

/// `POST /v1/auth/link/poll`: the device asks how its request went. The token comes back once.
async fn poll(State(hub): State<Arc<Hub>>, body: Bytes) -> Response {
    let Ok(req) = serde_json::from_slice::<PollBody>(&body) else {
        return err(StatusCode::BAD_REQUEST, r#"body must be {"poll_secret": "..."}"#);
    };
    let secret = req.poll_secret.trim();
    if secret.is_empty() || secret.len() > 128 {
        return err(StatusCode::BAD_REQUEST, "poll_secret is missing or too long");
    }
    let poll_hash = sha(secret);
    if !hub.link.allow_poll(&poll_hash) {
        return err(StatusCode::TOO_MANY_REQUESTS, format!("slow down: poll every {POLL_INTERVAL_SECS} seconds"));
    }
    let expired = || Json(json!({"status": "expired"})).into_response();
    // Read and use up an approved token in one transaction.
    let taken: Result<Option<(Request, Option<String>)>, rusqlite::Error> = (|| {
        let mut c = hub.db.c();
        let tx = c.transaction()?;
        let row = tx
            .query_row(
                &format!("SELECT {REQUEST_COLS}, token FROM link_requests WHERE poll_hash = ?1"),
                [&poll_hash],
                |r| Ok((request_row(r)?, r.get::<_, Option<String>>(8)?)),
            )
            .optional()?;
        let Some((req, token)) = row else { return Ok(None) };
        let live = req.expires > now();
        if req.status == "approved" || !live {
            // Delivered now, or never: the sealed token doesn't stay behind either way.
            let status = if req.status == "approved" && live { "delivered" } else { req.status.as_str() };
            tx.execute("UPDATE link_requests SET status = ?1, token = NULL WHERE poll_hash = ?2", params![status, poll_hash])?;
        }
        tx.commit()?;
        Ok(Some((req, token)))
    })();
    let (req, token) = match taken {
        Ok(Some(found)) => found,
        Ok(None) => return expired(),
        Err(e) => return internal(e),
    };
    if req.expires <= now() {
        return expired();
    }
    match req.status.as_str() {
        "pending" | "approving" => Json(json!({"status": "pending"})).into_response(),
        "denied" => Json(json!({"status": "denied"})).into_response(),
        "approved" => {
            let Some(sealed) = token else { return expired() };
            let token = match hub.db.unseal(&sealed) {
                Ok(t) => t,
                Err(e) => return internal(e),
            };
            let device: Value = hub
                .db
                .devices()
                .into_iter()
                .find(|d| Some(&d.id) == req.device_id.as_ref())
                .map(|d| json!({"id": d.id, "name": d.name, "platform": d.platform, "admin": d.admin}))
                .unwrap_or(Value::Null);
            Json(json!({"status": "approved", "token": token, "device": device})).into_response()
        }
        // Delivered already: this request is finished.
        _ => expired(),
    }
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/auth/link/start", post(start))
        .route("/v1/auth/link/poll", post(poll))
        .route("/v1/auth/link/{code}", get(show))
        .route("/v1/auth/link/{code}/approve", post(approve))
        .route("/v1/auth/link/{code}/deny", post(deny))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use reqwest::StatusCode as S;

    use super::*;
    use crate::db::Db;
    use crate::server::router;

    struct Fixture {
        hub: Arc<Hub>,
        base: String,
        admin_token: String,
        device_token: String,
        dir: std::path::PathBuf,
        http: reqwest::Client,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    const INSTALL: &str = "cli-install-0123456789abcdef";

    async fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!("grouper-link-test-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Laptop", "windows", true).unwrap();
        let (_, device_token) = db.add_device("Node", "linux", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        hub.google.configure(|c| c.public_url = None);
        hub.link.set_min_poll_gap(Duration::ZERO);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await });
        Fixture { hub, base: format!("http://{addr}"), admin_token, device_token, dir, http: reqwest::Client::new() }
    }

    impl Fixture {
        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        async fn send(&self, rb: reqwest::RequestBuilder) -> (S, Value) {
            let r = rb.send().await.unwrap();
            let status = r.status();
            (status, r.json().await.unwrap_or(Value::Null))
        }

        async fn start_from(&self, ip: &str, install_id: Option<&str>) -> (S, Value) {
            let mut body = json!({"name": "Build box", "platform": "Windows"});
            if let Some(id) = install_id {
                body["install_id"] = json!(id);
            }
            self.send(self.http.post(self.url("/v1/auth/link/start")).header("cf-connecting-ip", ip).json(&body)).await
        }

        async fn start(&self, install_id: Option<&str>) -> Value {
            let (status, body) = self.start_from("203.0.113.1", install_id).await;
            assert_eq!(status, S::OK, "{body}");
            body
        }

        async fn poll(&self, secret: &str) -> (S, Value) {
            self.send(self.http.post(self.url("/v1/auth/link/poll")).json(&json!({"poll_secret": secret}))).await
        }

        async fn act(&self, code: &str, action: &str, token: Option<&str>) -> (S, Value) {
            let mut rb = self.http.post(self.url(&format!("/v1/auth/link/{code}/{action}")));
            if let Some(t) = token {
                rb = rb.bearer_auth(t);
            }
            self.send(rb).await
        }

        async fn show(&self, code: &str, token: &str) -> (S, Value) {
            self.send(self.http.get(self.url(&format!("/v1/auth/link/{code}"))).bearer_auth(token)).await
        }

        /// Start, approve and poll: the token and device the CLI would save.
        async fn link(&self, install_id: Option<&str>) -> (String, Value) {
            let started = self.start(install_id).await;
            let code = started["code"].as_str().unwrap();
            let (status, _) = self.act(code, "approve", Some(&self.admin_token)).await;
            assert_eq!(status, S::OK);
            let (status, body) = self.poll(started["poll_secret"].as_str().unwrap()).await;
            assert_eq!(status, S::OK, "{body}");
            (body["token"].as_str().unwrap().to_string(), body["device"].clone())
        }
    }

    #[test]
    fn codes_look_right() {
        let code = random_code();
        assert_eq!(code.len(), 9);
        assert_eq!(&code[4..5], "-");
        assert!(code.replace('-', "").bytes().all(|b| CODE_ALPHABET.contains(&b)));
        assert_eq!(normalize_code(" k7pm 3xqr ").as_deref(), Some("K7PM3XQR"));
        assert_eq!(normalize_code("K7PM-3XQ0"), None, "0 isn't in the alphabet");
        assert_eq!(normalize_code("K7PM"), None);
    }

    #[tokio::test]
    async fn happy_path_gives_a_non_admin_token_exactly_once() {
        let f = fixture().await;
        let started = f.start(Some(INSTALL)).await;
        let code = started["code"].as_str().unwrap().to_string();
        let secret = started["poll_secret"].as_str().unwrap().to_string();
        assert_eq!(started["expires_in"], 600);
        assert_eq!(started["interval"], 3);
        assert_eq!(started["verify_url"], format!("{}/#/link?code={code}", f.base));
        assert!(secret.len() >= 64);
        // Only hashes at rest.
        let stored: (String, String) = f
            .hub
            .db
            .c()
            .query_row("SELECT code_hash, poll_hash FROM link_requests", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert!(!stored.0.contains(&code.replace('-', "")) && stored.1 != secret);

        assert_eq!(f.poll(&secret).await.1, json!({"status": "pending"}));
        let (status, shown) = f.show(&code.to_lowercase(), &f.admin_token).await;
        assert_eq!(status, S::OK, "{shown}");
        assert_eq!((shown["name"].as_str(), shown["platform"].as_str()), (Some("Build box"), Some("windows")));
        assert_eq!((shown["code"].as_str(), shown["status"].as_str()), (Some(code.as_str()), Some("pending")));
        assert_eq!(shown["replaces"], Value::Null);

        let (status, approved) = f.act(&code, "approve", Some(&f.admin_token)).await;
        assert_eq!(status, S::OK, "{approved}");
        assert_eq!(approved["device"]["admin"], false);
        assert!(approved.get("token").is_none(), "the browser never sees the token");
        let sealed: Option<String> =
            f.hub.db.c().query_row("SELECT token FROM link_requests", [], |r| r.get(0)).unwrap();
        assert!(sealed.as_deref().is_some_and(|t| t.starts_with("enc:")), "the waiting token is sealed");

        let (status, body) = f.poll(&secret).await;
        assert_eq!(status, S::OK);
        assert_eq!(body["status"], "approved");
        assert_eq!(body["device"]["id"], approved["device"]["id"]);
        assert_eq!(body["device"]["admin"], false);
        let token = body["token"].as_str().unwrap();
        let auth = f.hub.db.authenticate(token).unwrap();
        assert!(!auth.admin, "a linked device isn't an admin");
        let r = f.http.get(f.url("/v1/limits")).bearer_auth(token).send().await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = f.http.get(f.url("/v1/devices")).bearer_auth(token).send().await.unwrap();
        assert_eq!(r.status(), S::OK, "it can connect and read");

        // Single use: the token isn't handed out again, and the code can't be approved twice.
        assert_eq!(f.poll(&secret).await.1, json!({"status": "expired"}));
        let gone: Option<String> = f.hub.db.c().query_row("SELECT token FROM link_requests", [], |r| r.get(0)).unwrap();
        assert_eq!(gone, None);
        assert_eq!(f.act(&code, "approve", Some(&f.admin_token)).await.0, S::CONFLICT);
        assert_eq!(f.act(&code, "deny", Some(&f.admin_token)).await.0, S::CONFLICT);
        assert_eq!(f.show(&code, &f.admin_token).await.1["status"], "approved");
    }

    #[tokio::test]
    async fn deny_and_expiry() {
        let f = fixture().await;
        let started = f.start(None).await;
        let (code, secret) = (started["code"].as_str().unwrap(), started["poll_secret"].as_str().unwrap());
        let (status, body) = f.act(code, "deny", Some(&f.admin_token)).await;
        assert_eq!((status, body), (S::OK, json!({"status": "denied"})));
        assert_eq!(f.poll(secret).await.1, json!({"status": "denied"}));
        assert_eq!(f.act(code, "approve", Some(&f.admin_token)).await.0, S::CONFLICT);
        assert_eq!(f.hub.db.devices().len(), 2, "no device was made");

        let started = f.start(None).await;
        let (code, secret) = (started["code"].as_str().unwrap(), started["poll_secret"].as_str().unwrap());
        f.hub.db.c().execute("UPDATE link_requests SET expires = ?1", [now() as i64 - 1]).unwrap();
        assert_eq!(f.poll(secret).await.1, json!({"status": "expired"}));
        assert_eq!(f.show(code, &f.admin_token).await.0, S::NOT_FOUND);
        assert_eq!(f.act(code, "approve", Some(&f.admin_token)).await.0, S::NOT_FOUND);
        assert_eq!(f.poll("not-a-secret-anyone-made").await.1, json!({"status": "expired"}));
        assert_eq!(f.show("ZZZZ-ZZZZ", &f.admin_token).await.0, S::NOT_FOUND);
    }

    #[tokio::test]
    async fn approve_and_show_need_an_admin() {
        let f = fixture().await;
        let started = f.start(None).await;
        let code = started["code"].as_str().unwrap();
        assert_eq!(f.act(code, "approve", None).await.0, S::UNAUTHORIZED);
        assert_eq!(f.act(code, "deny", None).await.0, S::UNAUTHORIZED);
        assert_eq!(f.act(code, "approve", Some(&f.device_token)).await.0, S::FORBIDDEN);
        assert_eq!(f.show(code, &f.device_token).await.0, S::FORBIDDEN);
        let r = f.http.get(f.url(&format!("/v1/auth/link/{code}"))).send().await.unwrap();
        assert_eq!(r.status(), S::UNAUTHORIZED);
        // A cookie session needs the CSRF header and a same-site origin, as for every change.
        let r = f.http.post(f.url("/v1/session")).header("x-grouper", "1").header("origin", &f.base)
            .json(&json!({"token": f.admin_token})).send().await.unwrap();
        let cookie = r.headers()[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string();
        let r = f.http.post(f.url(&format!("/v1/auth/link/{code}/approve"))).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        assert_eq!(f.poll(started["poll_secret"].as_str().unwrap()).await.1["status"], "pending");
        let r = f
            .http
            .post(f.url(&format!("/v1/auth/link/{code}/approve")))
            .header("cookie", &cookie)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
    }

    #[tokio::test]
    async fn the_same_install_reuses_its_device() {
        let f = fixture().await;
        let (first_token, first) = f.link(Some(INSTALL)).await;
        let started = f.start(Some(INSTALL)).await;
        let shown = f.show(started["code"].as_str().unwrap(), &f.admin_token).await.1;
        assert_eq!(shown["replaces"], "Build box", "the page can say which device it replaces");
        let (status, _) = f.act(started["code"].as_str().unwrap(), "approve", Some(&f.admin_token)).await;
        assert_eq!(status, S::OK);
        let second = f.poll(started["poll_secret"].as_str().unwrap()).await.1;
        assert_eq!(second["device"]["id"], first["id"], "same device id");
        assert!(f.hub.db.authenticate(&first_token).is_none(), "the old token is revoked");
        assert!(f.hub.db.authenticate(second["token"].as_str().unwrap()).is_some());
        let live = f.hub.db.devices().into_iter().filter(|d| !d.revoked).count();
        assert_eq!(live, 3, "no extra device");

        // Without an install id (or with another one) a new device is made.
        let (_, other) = f.link(None).await;
        assert_ne!(other["id"], first["id"]);
        let (_, other) = f.link(Some("another-install-0123456789")).await;
        assert_ne!(other["id"], first["id"]);

        // Bad install ids are refused.
        for bad in ["short", "has spaces in it, too many", &"x".repeat(65)] {
            let (status, _) = f.start_from("203.0.113.9", Some(bad)).await;
            assert_eq!(status, S::BAD_REQUEST, "{bad}");
        }
    }

    #[tokio::test]
    async fn starts_and_polls_are_rate_limited() {
        let f = fixture().await;
        for _ in 0..STARTS_PER_MINUTE {
            assert_eq!(f.start_from("198.51.100.1", None).await.0, S::OK);
        }
        assert_eq!(f.start_from("198.51.100.1", None).await.0, S::TOO_MANY_REQUESTS);
        // A cap on waiting requests, whoever asks.
        for i in 0..(MAX_PENDING as u32 - STARTS_PER_MINUTE) {
            let ip = format!("198.51.100.{}", 10 + i / STARTS_PER_MINUTE);
            assert_eq!(f.start_from(&ip, None).await.0, S::OK);
        }
        assert_eq!(f.start_from("198.51.100.200", None).await.0, S::TOO_MANY_REQUESTS);

        // Polling faster than the interval is refused.
        f.hub.db.c().execute("DELETE FROM link_requests", []).unwrap();
        f.hub.link.set_min_poll_gap(MIN_POLL_GAP);
        let started = f.start_from("198.51.100.201", None).await.1;
        let secret = started["poll_secret"].as_str().unwrap();
        assert_eq!(f.poll(secret).await.0, S::OK);
        assert_eq!(f.poll(secret).await.0, S::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn devices_and_google_reuse_by_install_id() {
        let f = fixture().await;
        let register = |install: Option<&str>| {
            let mut body = json!({"name": "Mac node", "platform": "macos"});
            if let Some(id) = install {
                body["install_id"] = json!(id);
            }
            f.http.post(f.url("/v1/devices")).bearer_auth(&f.admin_token).json(&body)
        };
        let (status, first) = f.send(register(Some(INSTALL))).await;
        assert_eq!(status, S::OK, "{first}");
        let (_, second) = f.send(register(Some(INSTALL))).await;
        assert_eq!(second["device"]["id"], first["device"]["id"]);
        assert!(f.hub.db.authenticate(first["token"].as_str().unwrap()).is_none());
        assert!(f.hub.db.authenticate(second["token"].as_str().unwrap()).is_some());
        let (status, _) = f.send(register(Some("bad id"))).await;
        assert_eq!(status, S::BAD_REQUEST);

        // An install's node device and its admin app device are separate.
        let (app, _) = f.hub.db.add_device("App", "macos", true).unwrap();
        let issued = f
            .hub
            .issue_device("Grouper on Mac", "macos", true, Some(Install { id: INSTALL, kind: "macos" }), None)
            .await
            .unwrap();
        assert!(!issued.reused);
        assert_ne!(issued.device.id, app.id);
        assert_ne!(issued.device.id, first["device"]["id"].as_str().unwrap());
        let again = f
            .hub
            .issue_device("Grouper on Mac", "macos", true, Some(Install { id: INSTALL, kind: "macos" }), None)
            .await
            .unwrap();
        assert!(again.reused);
        assert_eq!(again.device.id, issued.device.id);
        assert!(again.device.admin);
        // The Mac app's scheme: install X for its admin app device, X-node for its node device.
        // Registering (and re-registering) the node never touches the admin device's token.
        let node_install = format!("{INSTALL}-node");
        let (_, node) = f.send(register(Some(&node_install))).await;
        let (_, node_again) = f.send(register(Some(&node_install))).await;
        assert_eq!(node["device"]["id"], node_again["device"]["id"]);
        assert_ne!(node["device"]["id"].as_str(), Some(again.device.id.as_str()));
        assert_eq!(node_again["device"]["platform"], "macos");
        let app_auth = f.hub.db.authenticate(&again.token).expect("the admin app token still works");
        assert!(app_auth.admin);
        assert!(!f.hub.db.authenticate(node_again["token"].as_str().unwrap()).unwrap().admin);

        // The cap applies to new devices only.
        let live = f.hub.db.devices().iter().filter(|d| !d.revoked).count();
        let (_, third) = f.send(register(Some(INSTALL))).await;
        assert_eq!(third["device"]["id"], first["device"]["id"]);
        let full = f.hub.db.issue_device("Extra", "linux", false, None, Some(live));
        assert!(full.unwrap_err().downcast_ref::<crate::db::DeviceCapReached>().is_some());
        let reused = f.hub.db.issue_device("Mac node", "macos", false, Some(Install { id: INSTALL, kind: NODE_INSTALL }), Some(live));
        assert!(reused.unwrap().reused);
    }
}
