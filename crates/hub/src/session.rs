//! Browser sessions for the web client.
//!
//! Browsers can't put an `Authorization` header on a WebSocket upgrade, so the web
//! client trades a token for a session cookie once:
//!
//! - `POST /v1/session` with `{"token": "<id>.<secret>"}` checks the token and sets an
//!   `HttpOnly`, `SameSite=Strict` cookie holding a random session id (`Secure` when the
//!   request came over HTTPS, as `X-Forwarded-Proto: https` says behind Cloudflare). The
//!   hub keeps only a SHA-256 of the id, in `sessions`, with the token's device, and the
//!   session expires after 30 days. The token itself is never stored by the browser.
//! - `GET /v1/session` says who the caller is; `DELETE /v1/session` logs out.
//! - [`crate::server::Hub::auth`] accepts the cookie wherever a Bearer token is
//!   accepted. A Bearer header, when present, is used alone, as before. The session
//!   inherits the device's current admin flag, and dies with the device's token.
//! - [`guard`] stops cross-site use of the cookie: a cookie-authenticated POST, PUT,
//!   PATCH or DELETE needs `X-Grouper: 1` (a header other sites can't send without a
//!   CORS preflight the hub never grants) and an `Origin`/`Referer` from this host;
//!   a cookie-authenticated WebSocket upgrade needs an `Origin` that is this host.

use std::sync::{Arc, Once};

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use core_protocol::SessionInfo;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::db::{Auth, Db, now, random_hex};
use crate::registry::deny_unless_admin;
use crate::server::{Hub, err};

/// The session cookie's name.
pub const COOKIE: &str = "grouper_session";
/// The session cookie used when the browser reached the hub over HTTPS.
pub const SECURE_COOKIE: &str = "__Host-grouper_session";
/// How long a session lasts.
pub const SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
/// The header the web client sends on every state-changing request.
pub const CSRF_HEADER: &str = "x-grouper";
/// `last_used` is written at most this often per session.
const TOUCH_EVERY_SECS: u64 = 60;
static PLAIN_HTTP_WARNING: Once = Once::new();

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  id_hash TEXT PRIMARY KEY, device_id TEXT NOT NULL, created INTEGER NOT NULL,
  last_used INTEGER NOT NULL, expires INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS sessions_device ON sessions (device_id);
";

pub(crate) fn hash(id: &str) -> String {
    hex::encode(Sha256::digest(id.as_bytes()))
}

/// A session id as the hub makes them: 64 hex characters.
fn well_formed(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Db {
    /// Start a session for `device_id`. Returns the raw id (for the cookie only) and
    /// when it expires. Expired sessions are cleared on the way.
    pub fn create_session(&self, device_id: &str) -> anyhow::Result<(String, u64)> {
        let id = random_hex(32);
        let created = now();
        let expires = created + SESSION_TTL_SECS;
        let c = self.c();
        c.execute("DELETE FROM sessions WHERE expires <= ?1", [created as i64])?;
        c.execute(
            "INSERT INTO sessions (id_hash, device_id, created, last_used, expires) VALUES (?1, ?2, ?3, ?3, ?4)",
            params![hash(&id), device_id, created as i64, expires as i64],
        )?;
        Ok((id, expires))
    }

    /// Who a session belongs to, if it exists, hasn't expired and its device's token
    /// is still valid. The admin flag is the device's current one.
    pub fn session_auth(&self, id: &str) -> Option<Auth> {
        if !well_formed(id) {
            return None;
        }
        let key = hash(id);
        let c = self.c();
        let (device_id, admin, expires, last_used): (String, i64, i64, i64) = c
            .query_row(
                "SELECT s.device_id, d.admin, s.expires, s.last_used FROM sessions s
                 JOIN devices d ON d.id = s.device_id WHERE s.id_hash = ?1 AND d.revoked = 0",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .ok()
            .flatten()?;
        let current = now() as i64;
        if expires <= current {
            let _ = c.execute("DELETE FROM sessions WHERE id_hash = ?1", [&key]);
            return None;
        }
        if current - last_used >= TOUCH_EVERY_SECS as i64 {
            let _ = c.execute("UPDATE sessions SET last_used = ?1 WHERE id_hash = ?2", params![current, key]);
        }
        Some(Auth { device_id, admin: admin != 0 })
    }

    /// End a session. Returns whether it existed. Also removes any push subscriptions owned by it.
    pub fn delete_session(&self, id: &str) -> bool {
        if !well_formed(id) {
            return false;
        }
        let key = hash(id);
        let c = self.c();
        let _ = c.execute("DELETE FROM push_subscriptions WHERE session_id = ?1", [&key]);
        c.execute("DELETE FROM sessions WHERE id_hash = ?1", [&key]).unwrap_or(0) > 0
    }

    /// Active, non-expired browser sessions for non-revoked devices, most recently used first.
    pub fn list_sessions(&self, current_hash: Option<&str>) -> Vec<SessionInfo> {
        let current = now() as i64;
        let c = self.c();
        let _ = c.execute("DELETE FROM sessions WHERE expires <= ?1", [current]);
        let mut st = c
            .prepare(
                "SELECT s.id_hash, s.device_id, d.name, d.admin, s.created, s.last_used, s.expires
                 FROM sessions s
                 JOIN devices d ON d.id = s.device_id
                 WHERE d.revoked = 0 AND s.expires > ?1
                 ORDER BY s.last_used DESC, s.created DESC",
            )
            .unwrap();
        st.query_map([current], |r| {
            let id_hash: String = r.get(0)?;
            Ok(SessionInfo {
                current: current_hash.is_some_and(|h| h == id_hash),
                id: id_hash,
                device_id: r.get(1)?,
                device_name: r.get(2)?,
                admin: r.get::<_, i64>(3)? != 0,
                created: r.get::<_, i64>(4)? as u64,
                last_used: r.get::<_, i64>(5)? as u64,
                expires: r.get::<_, i64>(6)? as u64,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect()
    }

    /// Revoke a session by its stored hash (`SessionInfo::id`). Returns whether it existed.
    /// Also removes any push subscriptions owned by it.
    pub fn delete_session_by_hash(&self, id_hash: &str) -> bool {
        if !well_formed(id_hash) {
            return false;
        }
        let key = id_hash.to_ascii_lowercase();
        let c = self.c();
        let _ = c.execute("DELETE FROM push_subscriptions WHERE session_id = ?1", [&key]);
        c.execute("DELETE FROM sessions WHERE id_hash = ?1", [&key]).unwrap_or(0) > 0
    }
}

/// Every possible session id in the request's `Cookie` headers. Browsers can send
/// duplicate cookies (for example after a name migration), so callers must try all
/// of them rather than trusting the first one.
pub(crate) fn cookie_sessions(headers: &HeaderMap) -> Vec<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == COOKIE || *name == SECURE_COOKIE)
        .map(|(_, value)| value.trim().trim_matches('"'))
        .collect()
}

/// Whether the request reached the hub over HTTPS (directly or via a proxy that says so).
fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
}

fn set_cookie(value: &str, max_age: u64, secure: bool) -> HeaderValue {
    let (name, secure) = if secure { (SECURE_COOKIE, "; Secure") } else { (COOKIE, "") };
    HeaderValue::from_str(&format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"))
        .expect("cookie header is ASCII")
}

fn clear_legacy_cookie() -> HeaderValue {
    HeaderValue::from_static("grouper_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0; Secure")
}

/// The `host[:port]` of an `Origin` or `Referer` value.
fn authority(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&rest[..end]).filter(|a| !a.is_empty())
}

/// Whether `url` (an `Origin` or `Referer`) names this hub's `Host`. In particular,
/// do not trust `X-Forwarded-Host`: Cloudflare passes client-supplied values through.
fn same_host(headers: &HeaderMap, url: &str) -> bool {
    let Some(origin) = authority(url) else { return false };
    headers.get(header::HOST).and_then(|h| h.to_str().ok()).is_some_and(|host| host.trim().eq_ignore_ascii_case(origin))
}

fn header_has_token(headers: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case(token))
}

/// `None` when the request may go on; otherwise why not.
fn csrf_refusal(method: &Method, path: &str, headers: &HeaderMap) -> Option<&'static str> {
    let bearer = headers.contains_key(header::AUTHORIZATION);
    let cookie = !cookie_sessions(headers).is_empty();
    let session_route = path == "/v1/session";
    if bearer || !(cookie || session_route) {
        return None;
    }
    if headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) == Some("cross-site") {
        return Some("cross-site request refused");
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let upgrade = header_has_token(headers, header::UPGRADE, "websocket");
    if upgrade {
        return match origin {
            Some(o) if same_host(headers, o) => None,
            _ => Some("WebSocket with a session cookie needs an Origin from this hub"),
        };
    }
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    if headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) != Some("1") {
        return Some("cookie-authenticated changes need the X-Grouper: 1 header");
    }
    match (origin, headers.get(header::REFERER).and_then(|v| v.to_str().ok())) {
        (Some(o), _) if same_host(headers, o) => {}
        (None, Some(r)) if same_host(headers, r) => {}
        (None, None) => return Some("cookie-authenticated changes need an Origin or Referer from this hub"),
        _ => return Some("request came from another site"),
    }
    None
}

/// Middleware: refuse cross-site use of the session cookie (see the module docs).
/// Requests with a Bearer token, or with no cookie, pass through untouched.
pub(crate) async fn guard(req: Request, next: Next) -> Response {
    if let Some(why) = csrf_refusal(req.method(), req.uri().path(), req.headers()) {
        return err(StatusCode::FORBIDDEN, why);
    }
    next.run(req).await
}

/// Add response headers that are part of the HTTP security boundary. This layer is
/// outside [`guard`], so it also decorates authentication, routing and CSRF errors.
pub(crate) async fn response_security(req: Request, next: Next) -> Response {
    let v1 = req.uri().path().starts_with("/v1/");
    let https = is_https(req.headers());
    let mut response = next.run(req).await;
    if v1 {
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store, private"));
    }
    if https {
        response
            .headers_mut()
            .insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    }
    response
}

#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

fn who(hub: &Hub, auth: &Auth) -> serde_json::Value {
    let name = hub.db.devices().into_iter().find(|d| d.id == auth.device_id).map(|d| d.name);
    json!({"signed_in": true, "device_id": auth.device_id, "name": name, "admin": auth.admin})
}

/// `POST /v1/session`: trade a token for a session cookie.
async fn login(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<LoginBody>) -> Response {
    let Some(auth) = hub.db.authenticate(&body.token) else {
        return err(StatusCode::UNAUTHORIZED, "that token isn't valid on this hub");
    };
    let (id, expires) = match hub.db.create_session(&auth.device_id) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    tracing::info!(device = %auth.device_id, admin = auth.admin, "web session started");
    let mut body = who(&hub, &auth);
    body["expires"] = json!(expires);
    let mut response = Json(body).into_response();
    let secure = is_https(&headers);
    if !secure {
        PLAIN_HTTP_WARNING.call_once(|| {
            tracing::warn!("web sessions over plain HTTP can be sniffed; use HTTPS outside a trusted LAN")
        });
    }
    response.headers_mut().insert(header::SET_COOKIE, set_cookie(&id, SESSION_TTL_SECS, secure));
    if secure {
        // Remove the old LAN-cookie name as the browser migrates to the __Host- cookie.
        response.headers_mut().append(header::SET_COOKIE, clear_legacy_cookie());
    }
    response
}

/// `GET /v1/session`: who this cookie (or Bearer token) belongs to. Answers 200 with
/// `signed_in: false` rather than 401, so the web app's first look isn't an error.
async fn whoami(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let body = match hub.auth(&headers) {
        Some(auth) => who(&hub, &auth),
        None => json!({"signed_in": false}),
    };
    Json(body).into_response()
}

/// The hash of the session cookie authenticating this request, if any (Bearer requests have none).
pub(crate) fn current_session_hash(hub: &Hub, headers: &HeaderMap) -> Option<String> {
    if headers.contains_key(header::AUTHORIZATION) {
        return None;
    }
    cookie_sessions(headers)
        .into_iter()
        .find(|id| hub.db.session_auth(id).is_some())
        .map(hash)
}

/// `DELETE /v1/session`: forget this browser's session and clear its cookie.
async fn logout(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    let mut ended = false;
    for id in cookie_sessions(&headers) {
        let deleted = hub.db.delete_session(id);
        if deleted {
            hub.invalidate_session(&hash(id));
            ended = true;
        }
    }
    if ended {
        hub.sync_push_sub_count();
    }
    let mut response = Json(json!({"signed_out": ended})).into_response();
    let secure = is_https(&headers);
    response.headers_mut().insert(header::SET_COOKIE, set_cookie("", 0, secure));
    if secure {
        response.headers_mut().append(header::SET_COOKIE, clear_legacy_cookie());
    }
    response
}

/// `GET /v1/sessions` (admin): active browser sessions on this hub.
async fn list_sessions(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let current = current_session_hash(&hub, &headers);
    Json(hub.db.list_sessions(current.as_deref())).into_response()
}

/// `DELETE /v1/sessions/{id}` (admin): revoke a browser session and close its live sockets.
async fn revoke_session(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let target = id.to_ascii_lowercase();
    let is_current = current_session_hash(&hub, &headers).as_deref() == Some(target.as_str());
    if !hub.db.delete_session_by_hash(&target) {
        return err(StatusCode::NOT_FOUND, "no such session");
    }
    hub.sync_push_sub_count();
    hub.invalidate_session(&target);
    let mut response = Json(json!({"revoked": target, "current": is_current})).into_response();
    if is_current {
        let secure = is_https(&headers);
        response.headers_mut().insert(header::SET_COOKIE, set_cookie("", 0, secure));
        if secure {
            response.headers_mut().append(header::SET_COOKIE, clear_legacy_cookie());
        }
    }
    response
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/session", post(login).get(whoami).delete(logout))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/{id}", delete(revoke_session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::router;
    use futures_util::StreamExt;
    use reqwest::StatusCode as S;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    struct Fixture {
        hub: Arc<Hub>,
        base: String,
        addr: std::net::SocketAddr,
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

    async fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!("grouper-session-test-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Phone", "ios", true).unwrap();
        let (_, device_token) = db.add_device("Laptop", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Fixture { hub, base: format!("http://{addr}"), addr, admin_token, device_token, dir, http }
    }

    impl Fixture {
        /// Log in like the web app does and return the cookie pair (`grouper_session=…`).
        async fn login(&self, token: &str) -> (S, Option<String>, serde_json::Value) {
            let r = self
                .http
                .post(format!("{}/v1/session", self.base))
                .header("x-grouper", "1")
                .header("origin", &self.base)
                .json(&json!({"token": token}))
                .send()
                .await
                .unwrap();
            let status = r.status();
            let cookie = r.headers().get("set-cookie").map(|v| v.to_str().unwrap().to_string());
            let body = r.json().await.unwrap_or(json!(null));
            (status, cookie, body)
        }
    }

    fn pair(set_cookie: &str) -> String {
        set_cookie.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn session_create_use_and_logout() {
        let f = fixture().await;

        // A bad token gets no cookie.
        let (status, cookie, _) = f.login("nope.nope").await;
        assert_eq!(status, S::UNAUTHORIZED);
        assert!(cookie.is_none());

        let (status, cookie, body) = f.login(&f.admin_token).await;
        assert_eq!(status, S::OK, "{body}");
        assert_eq!(body["admin"], true);
        assert_eq!(body["name"], "Phone");
        let set = cookie.expect("set-cookie");
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/"), "{set}");
        assert!(!set.contains("Secure"), "plain HTTP: no Secure flag: {set}");
        let cookie = pair(&set);
        // The cookie holds a random id, never the token or its secret.
        let raw = cookie.strip_prefix("grouper_session=").unwrap();
        assert!(well_formed(raw));
        assert!(!f.admin_token.contains(raw) && !raw.contains(f.admin_token.split('.').nth(1).unwrap()));
        // Only its hash is stored.
        let stored: String = f.hub.db.c().query_row("SELECT id_hash FROM sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(stored, hash(raw));
        assert_ne!(stored, raw);

        // The cookie works on reads and, with the CSRF header and a same-site Origin, on admin writes.
        let r = f.http.get(format!("{}/v1/session", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.json::<serde_json::Value>().await.unwrap()["signed_in"], true);
        let r = f.http.get(format!("{}/v1/session", f.base)).send().await.unwrap();
        assert_eq!(r.status(), S::OK);
        assert_eq!(r.json::<serde_json::Value>().await.unwrap()["signed_in"], false);
        let r = f.http.get(format!("{}/v1/devices", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK);
        let prefixed = format!("{COOKIE}={}; {cookie}", "0".repeat(64));
        let r = f.http.get(format!("{}/v1/devices", f.base)).header("cookie", prefixed).send().await.unwrap();
        assert_eq!(r.status(), S::OK, "a bad first session cookie must not hide a later valid one");
        let r = f.http.get(format!("{}/v1/limits", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK, "cookie inherits the admin flag on an admin route");
        let r = f
            .http
            .put(format!("{}/v1/limits", f.base))
            .header("cookie", &cookie)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .json(&json!({"thread_budget_usd": 0.5}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        assert_eq!(f.hub.db.limits().thread_budget_usd, Some(0.5));

        // Logout ends every valid session cookie presented, not only the first one.
        let (extra, _) = f.hub.db.create_session(body["device_id"].as_str().unwrap()).unwrap();
        let logout_cookies = format!("{cookie}; {COOKIE}={extra}");
        let r = f
            .http
            .delete(format!("{}/v1/session", f.base))
            .header("cookie", logout_cookies)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        assert!(r.headers()["set-cookie"].to_str().unwrap().contains("Max-Age=0"));
        let count: i64 = f.hub.db.c().query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
        let r = f.http.get(format!("{}/v1/devices", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn device_sessions_are_not_admin_and_die_with_the_token() {
        let f = fixture().await;
        let (status, cookie, body) = f.login(&f.device_token).await;
        assert_eq!(status, S::OK);
        assert_eq!(body["admin"], false);
        let cookie = pair(&cookie.unwrap());
        let r = f.http.get(format!("{}/v1/limits", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = f.http.get(format!("{}/v1/devices", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK);
        // Granting admin to the device upgrades its live session; revoking the token ends it.
        let device_id = body["device_id"].as_str().unwrap().to_string();
        f.hub.db.set_admin(&device_id, true).unwrap();
        let r = f.http.get(format!("{}/v1/limits", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK);
        f.hub.db.revoke_device(&device_id).unwrap();
        let r = f.http.get(format!("{}/v1/devices", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn expired_sessions_are_refused_and_removed() {
        let f = fixture().await;
        let (_, cookie, _) = f.login(&f.admin_token).await;
        let cookie = pair(&cookie.unwrap());
        let raw = cookie.strip_prefix("grouper_session=").unwrap().to_string();
        assert!(f.hub.db.session_auth(&raw).is_some());
        f.hub.db.c().execute("UPDATE sessions SET expires = ?1", [now() as i64 - 1]).unwrap();
        let r = f.http.get(format!("{}/v1/devices", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::UNAUTHORIZED);
        let count: i64 = f.hub.db.c().query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "an expired session is deleted when it's seen");
        assert!(f.hub.db.session_auth(&"0".repeat(64)).is_none());
        assert!(f.hub.db.session_auth("short").is_none());
    }

    #[tokio::test]
    async fn https_logins_get_a_secure_cookie() {
        let f = fixture().await;
        let r = f
            .http
            .post(format!("{}/v1/session", f.base))
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .header("x-forwarded-proto", "https")
            .json(&json!({"token": f.admin_token}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        let sets: Vec<_> = r.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap()).collect();
        let set = sets.iter().find(|v| v.starts_with("__Host-grouper_session=")).unwrap();
        assert!(set.starts_with("__Host-grouper_session="), "{set}");
        assert!(set.contains("; Secure") && set.contains("Path=/") && !set.contains("Domain="), "{set}");
        assert!(sets.iter().any(|v| v.starts_with("grouper_session=") && v.contains("Max-Age=0")));
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store, private");
        assert_eq!(r.headers()[header::STRICT_TRANSPORT_SECURITY], "max-age=31536000");
    }

    #[tokio::test]
    async fn cookie_writes_need_the_csrf_header_and_a_same_site_origin() {
        let f = fixture().await;
        let (_, cookie, _) = f.login(&f.admin_token).await;
        let cookie = pair(&cookie.unwrap());
        let put = |headers: Vec<(&'static str, String)>| {
            let mut rb = f
                .http
                .put(format!("{}/v1/limits", f.base))
                .header("cookie", &cookie)
                .json(&json!({"daily_budget_usd": 9.0}));
            for (k, v) in headers {
                rb = rb.header(k, v);
            }
            rb.send()
        };
        // No X-Grouper header: refused, even from this origin.
        let r = put(vec![("origin", f.base.clone())]).await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        // Another site's Origin or Referer: refused, even with the header.
        let r = put(vec![("x-grouper", "1".into()), ("origin", "https://evil.example".into())]).await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = put(vec![("x-grouper", "1".into()), ("referer", "https://evil.example/x".into())]).await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = put(vec![("x-grouper", "1".into()), ("origin", "null".into())]).await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = put(vec![("x-grouper", "1".into()), ("sec-fetch-site", "cross-site".into())]).await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = put(vec![("x-grouper", "1".into())]).await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN, "a custom header alone is not enough");
        assert_eq!(f.hub.db.limits().daily_budget_usd, None, "nothing changed");
        // A client-controlled forwarded host must not count as this hub.
        let r = put(vec![
            ("x-grouper", "1".into()),
            ("origin", "https://grouper.example.org".into()),
            ("x-forwarded-host", "grouper.example.org".into()),
        ])
        .await
        .unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = put(vec![("x-grouper", "1".into()), ("referer", format!("{}/settings", f.base))]).await.unwrap();
        assert_eq!(r.status(), S::OK, "a same-host Referer is accepted when Origin is absent");
        // Reads need neither.
        let r = f.http.get(format!("{}/v1/limits", f.base)).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK);
        // Bearer clients are untouched: no header, no Origin, still fine.
        let r = f
            .http
            .put(format!("{}/v1/limits", f.base))
            .bearer_auth(&f.admin_token)
            .json(&json!({"daily_budget_usd": null}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        // Logging in from another site is refused too.
        let r = f
            .http
            .post(format!("{}/v1/session", f.base))
            .header("x-grouper", "1")
            .header("origin", "https://evil.example")
            .json(&json!({"token": f.admin_token}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
    }

    #[tokio::test]
    async fn websocket_with_a_cookie_checks_origin() {
        let f = fixture().await;
        let (_, cookie, _) = f.login(&f.admin_token).await;
        let cookie = pair(&cookie.unwrap());
        let open = |origin: Option<String>, path: &'static str| {
            let mut req = format!("ws://{}{path}", f.addr).into_client_request().unwrap();
            req.headers_mut().insert("cookie", cookie.parse().unwrap());
            if let Some(o) = origin {
                req.headers_mut().insert("origin", o.parse().unwrap());
            }
            tokio_tungstenite::connect_async(req)
        };
        let refused = |r: Result<_, tokio_tungstenite::tungstenite::Error>| match r {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => 101,
        };
        assert_eq!(refused(open(Some("https://evil.example".into()), "/v1/events").await), 403);
        assert_eq!(refused(open(None, "/v1/events").await), 403);
        let (mut ws, _) = open(Some(f.base.clone()), "/v1/events").await.expect("same-origin upgrade");
        // It streams events.
        let conv = f.hub.db.create_conversation("project", "t").unwrap();
        f.hub.emit(&conv, None, core_protocol::EventBody::Error { message: "hello".into() });
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
        assert!(msg.to_text().unwrap().contains("hello"));
        // Logging out invalidates this exact session and promptly closes its stream.
        let r = f
            .http
            .delete(format!("{}/v1/session", f.base))
            .header("cookie", &cookie)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
        match msg {
            tokio_tungstenite::tungstenite::Message::Close(Some(frame)) => {
                assert_eq!(frame.code, tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy)
            }
            other => panic!("expected policy close, got {other:?}"),
        }
        // The device connection never takes a cookie, even from this origin.
        assert_eq!(refused(open(Some(f.base.clone()), core_protocol::CONNECT_PATH).await), 401);
    }

    #[tokio::test]
    async fn event_stream_revalidates_cookie_and_bearer_before_sending() {
        let f = fixture().await;
        let (_, cookie, _) = f.login(&f.admin_token).await;
        let cookie = pair(&cookie.unwrap());
        let mut cookie_req = format!("ws://{}/v1/events", f.addr).into_client_request().unwrap();
        cookie_req.headers_mut().insert("cookie", cookie.parse().unwrap());
        cookie_req.headers_mut().insert("origin", f.base.parse().unwrap());
        let (mut cookie_ws, _) = tokio_tungstenite::connect_async(cookie_req).await.unwrap();
        f.hub.db.c().execute("UPDATE sessions SET expires = ?1", [now() as i64 - 1]).unwrap();
        let conv = f.hub.db.create_conversation("project", "expired").unwrap();
        f.hub.emit(&conv, None, core_protocol::EventBody::Error { message: "must not leak".into() });
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), cookie_ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            msg,
            tokio_tungstenite::tungstenite::Message::Close(Some(frame))
                if frame.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy
        ));

        let auth = f.hub.db.authenticate(&f.device_token).unwrap();
        let mut bearer_req = format!("ws://{}/v1/events", f.addr).into_client_request().unwrap();
        bearer_req
            .headers_mut()
            .insert(header::AUTHORIZATION, format!("Bearer {}", f.device_token).parse().unwrap());
        let (mut bearer_ws, _) = tokio_tungstenite::connect_async(bearer_req).await.unwrap();
        f.hub.db.revoke_device(&auth.device_id).unwrap();
        f.hub.emit(&conv, None, core_protocol::EventBody::Error { message: "must not leak either".into() });
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), bearer_ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            msg,
            tokio_tungstenite::tungstenite::Message::Close(Some(frame))
                if frame.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy
        ));
    }

    #[tokio::test]
    async fn v1_responses_are_private_and_https_responses_get_hsts() {
        let f = fixture().await;
        for path in ["/v1/devices", "/v1/not-a-route"] {
            let r = f
                .http
                .get(format!("{}{path}", f.base))
                .header("x-forwarded-proto", "https")
                .send()
                .await
                .unwrap();
            assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store, private", "{path}");
            assert_eq!(r.headers()[header::STRICT_TRANSPORT_SECURITY], "max-age=31536000", "{path}");
        }
        let r = f
            .http
            .put(format!("{}/v1/limits", f.base))
            .header("cookie", format!("{COOKIE}={}", "0".repeat(64)))
            .header("x-forwarded-proto", "https")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store, private");
        assert_eq!(r.headers()[header::STRICT_TRANSPORT_SECURITY], "max-age=31536000");

        let r = f
            .http
            .get(format!("{}/healthz", f.base))
            .header("x-forwarded-proto", "https")
            .send()
            .await
            .unwrap();
        assert!(r.headers().get(header::CACHE_CONTROL).is_none());
        assert_eq!(r.headers()[header::STRICT_TRANSPORT_SECURITY], "max-age=31536000");
    }

    #[test]
    fn cookie_and_origin_parsing() {
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            "a=1; grouper_session=abc ; __Host-grouper_session=def; b=2".parse().unwrap(),
        );
        assert_eq!(cookie_sessions(&h), vec!["abc", "def"]);
        h.insert("host", "hub.local:8080".parse().unwrap());
        assert!(same_host(&h, "http://hub.local:8080"));
        assert!(same_host(&h, "http://HUB.local:8080/app/#x"));
        assert!(!same_host(&h, "http://hub.local:8081"));
        assert!(!same_host(&h, "http://hub.local:8080.evil.example"));
        assert!(!same_host(&h, "null"));
        assert_eq!(authority("https://a.b/c?d"), Some("a.b"));

        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        h.insert(header::UPGRADE, "h2c, WebSocket, x".parse().unwrap());
        assert!(csrf_refusal(&Method::GET, "/v1/events", &h).is_some());
        h.insert(header::ORIGIN, "http://hub.local:8080".parse().unwrap());
        assert_eq!(csrf_refusal(&Method::GET, "/v1/events", &h), None);
    }

    #[tokio::test]
    async fn list_and_revoke_sessions_marks_current_and_closes_live_sockets() {
        let f = fixture().await;
        let (_, c1, _) = f.login(&f.admin_token).await;
        let (_, c2, _) = f.login(&f.device_token).await;
        let cookie1 = pair(&c1.unwrap());
        let cookie2 = pair(&c2.unwrap());
        let raw1 = cookie1.strip_prefix("grouper_session=").unwrap().to_string();
        let raw2 = cookie2.strip_prefix("grouper_session=").unwrap().to_string();

        // Auth checks: unauth -> 401, non-admin -> 403.
        assert_eq!(f.http.get(format!("{}/v1/sessions", f.base)).send().await.unwrap().status(), S::UNAUTHORIZED);
        assert_eq!(
            f.http.get(format!("{}/v1/sessions", f.base)).header("cookie", &cookie2).send().await.unwrap().status(),
            S::FORBIDDEN
        );

        // Open a live /v1/events socket for session 2 (the non-admin browser).
        let mut ws_req = format!("ws://{}/v1/events", f.addr).into_client_request().unwrap();
        ws_req.headers_mut().insert("cookie", cookie2.parse().unwrap());
        ws_req.headers_mut().insert("origin", f.base.parse().unwrap());
        let (mut ws2, _) = tokio_tungstenite::connect_async(ws_req).await.unwrap();

        // Admin lists sessions via cookie1: sees both, only session 1 is marked current, raw cookies are never leaked.
        let r = f.http.get(format!("{}/v1/sessions", f.base)).header("cookie", &cookie1).send().await.unwrap();
        assert_eq!(r.status(), S::OK);
        let text = r.text().await.unwrap();
        assert!(!text.contains(&raw1) && !text.contains(&raw2), "raw session ids must not be exposed");
        let list: Vec<SessionInfo> = serde_json::from_str(&text).unwrap();
        assert_eq!(list.len(), 2);
        let s1 = list.iter().find(|s| s.id == hash(&raw1)).unwrap();
        let s2 = list.iter().find(|s| s.id == hash(&raw2)).unwrap();
        assert!(s1.current);
        assert_eq!(s1.device_name, "Phone");
        assert!(s1.admin);
        assert!(!s2.current);
        assert_eq!(s2.device_name, "Laptop");
        assert!(!s2.admin);
        assert!(s1.expires > s1.created && s1.last_used >= s1.created);

        // Bearer admin sees none marked current.
        let bearer_list: Vec<SessionInfo> = f
            .http
            .get(format!("{}/v1/sessions", f.base))
            .bearer_auth(&f.admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(bearer_list.iter().all(|s| !s.current));

        // Non-admin cannot revoke.
        let r = f
            .http
            .delete(format!("{}/v1/sessions/{}", f.base, s1.id))
            .header("cookie", &cookie2)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);

        // Revoking session 2 deletes it and immediately closes ws2 with a Policy close frame.
        let r = f
            .http
            .delete(format!("{}/v1/sessions/{}", f.base, s2.id))
            .header("cookie", &cookie1)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws2.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(
            msg,
            tokio_tungstenite::tungstenite::Message::Close(Some(frame))
                if frame.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy
        ));
        // Session 2 cookie no longer authenticates.
        assert_eq!(
            f.http.get(format!("{}/v1/devices", f.base)).header("cookie", &cookie2).send().await.unwrap().status(),
            S::UNAUTHORIZED
        );

        // Revoking again or nonexistent returns 404.
        let r = f
            .http
            .delete(format!("{}/v1/sessions/{}", f.base, s2.id))
            .header("cookie", &cookie1)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::NOT_FOUND);

        // Revoking current session also clears the cookie.
        let r = f
            .http
            .delete(format!("{}/v1/sessions/{}", f.base, s1.id))
            .header("cookie", &cookie1)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        assert!(r.headers()["set-cookie"].to_str().unwrap().contains("Max-Age=0"));
        assert_eq!(
            f.http.get(format!("{}/v1/devices", f.base)).header("cookie", &cookie1).send().await.unwrap().status(),
            S::UNAUTHORIZED
        );
    }
}
