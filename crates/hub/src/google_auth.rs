//! Sign in with Google: one owner per hub (`docs/accounts-design.md`).
//!
//! - Every client signs in with Google itself and hands the hub a Google **ID token**. The hub
//!   checks it on its own: RS256 signature against Google's published keys (cached per
//!   `Cache-Control`, fetched again for an unknown `kid` at most once a minute), `iss` is
//!   Google, `aud` is a trusted client ID, `exp`/`iat` in range (5 minutes of clock skew),
//!   `email_verified`, and a `nonce` this hub issued in the last 10 minutes and nobody used.
//!   The nonce is what stops a token minted for hub A from working at hub B.
//! - A hub has one owner (Google `sub`, email, name). An unclaimed hub needs a one-time setup
//!   code (logged at start, or `grouper-hub setup-code`) with the first sign-in; afterwards only
//!   the owner's `sub` may sign in. An admin can link or unlink the owner of an existing hub.
//! - Apps get an admin device token; browsers get an admin session cookie
//!   ([`crate::session`]). Browsers sign in through Google's `form_post` to one shared return
//!   page (`/oauth/google/return`, served by every hub, registered once on Grouper's web
//!   client), which forwards the result to the hub named in `state`. That hub only accepts a
//!   `state` it made (one-time, 10 minutes) whose nonce matches the ID token's.
//!
//! Routes: `GET /v1/auth/status`, `POST /v1/auth/google/nonce`, `POST /v1/auth/google`,
//! `POST /v1/auth/google/start`, `GET|POST /oauth/google/return`, `POST /v1/auth/google/return`,
//! `GET|POST|DELETE /v1/auth/google/link` (admin).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::db::{Auth, Db, now, random_hex};
use crate::registry::deny_unless_admin;
use crate::server::{Hub, err};

/// Grouper's own Google OAuth client IDs (iOS app, Mac app), trusted when `GROUPER_GOOGLE_CLIENT_IDS` is unset.
// Grouper's official OAuth clients (project grouper-515912): iPhone app, Mac app, web. Public identifiers.
const DEFAULT_CLIENT_IDS: &[&str] = &[
    "998648431961-aeomun58v9i625ofsprfc5oroalv233t.apps.googleusercontent.com",
    "998648431961-cnc9c9930iacdrs9u7b25cgtp7g26f9q.apps.googleusercontent.com",
    "998648431961-dh13qf3a3c596iadav1vmfnljr3j0dhr.apps.googleusercontent.com",
];
/// Grouper's Google "Web application" client, for browsers. Empty turns browser Google sign-in off.
// Grouper's official web client; its only redirect URI is the shared return page on grouper.clockmillbarn.uk.
const DEFAULT_WEB_CLIENT_ID: &str = "998648431961-dh13qf3a3c596iadav1vmfnljr3j0dhr.apps.googleusercontent.com";
/// The return page registered on the web client.
const DEFAULT_RETURN_URL: &str = "https://grouper.clockmillbarn.uk/oauth/google/return";
const ENV_CLIENT_IDS: &str = "GROUPER_GOOGLE_CLIENT_IDS";
const ENV_WEB_CLIENT_ID: &str = "GROUPER_GOOGLE_WEB_CLIENT_ID";
const ENV_RETURN_URL: &str = "GROUPER_GOOGLE_RETURN_URL";
const ENV_PUBLIC_URL: &str = "GROUPER_PUBLIC_URL";
const GOOGLE_JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_ISSUERS: &[&str] = &["https://accounts.google.com", "accounts.google.com"];
const PROVIDER: &str = "google";

/// Paths that take a cross-site form POST by design; [`crate::session`]'s CSRF guard skips them.
pub(crate) const CROSS_SITE_PATHS: &[&str] = &[RETURN_PAGE_PATH, RETURN_PATH];
const RETURN_PAGE_PATH: &str = "/oauth/google/return";
const RETURN_PATH: &str = "/v1/auth/google/return";

/// Nonces and browser `state`s are good for this long, once.
pub const NONCE_TTL_SECS: u64 = 600;
/// At most this many unused nonces (and as many `state`s) at once, whoever asked.
const MAX_PENDING: usize = 1000;
/// Nonces per client IP per minute.
const NONCES_PER_MINUTE: u32 = 20;
const MAX_RATE_ENTRIES: usize = 4096;
/// Allowed clock difference for `exp` and `iat`.
const SKEW_SECS: u64 = 300;
pub const SETUP_CODE_TTL_SECS: u64 = 3600;
const MAX_SETUP_CODES: i64 = 20;
/// Setup code characters: no 0/O, 1/I/L.
pub(crate) const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 12;
const JWKS_DEFAULT_MAX_AGE: u64 = 3600;
const JWKS_MIN_MAX_AGE: u64 = 60;
const JWKS_MAX_MAX_AGE: u64 = 24 * 3600;
/// An unknown `kid` refetches Google's keys at most this often.
const JWKS_UNKNOWN_KID_REFETCH: Duration = Duration::from_secs(60);
/// After a failed fetch, stale keys are fetched again at most this often.
const JWKS_RETRY: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ID_TOKEN_LEN: usize = 16 * 1024;
const DEFAULT_DEVICE_NAME: &str = "Grouper app";
const MAX_DEVICE_NAME_CHARS: usize = 64;
/// `hub_meta` key of the admin device that Google browser sessions belong to.
const WEB_DEVICE_KEY: &str = "google_web_device";
const WEB_DEVICE_NAME: &str = "Web (Google sign-in)";
const MAX_FORM_BYTES: usize = 64 * 1024;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hub_owner (
  id INTEGER PRIMARY KEY CHECK (id = 1), provider TEXT NOT NULL, sub TEXT NOT NULL, email TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '', claimed_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS setup_codes (
  code_hash TEXT PRIMARY KEY, created INTEGER NOT NULL, expires INTEGER NOT NULL);
";

// ---------------------------------------------------------------- owner and setup codes

/// The account that owns this hub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Owner {
    pub provider: String,
    pub sub: String,
    pub email: String,
    pub name: String,
    pub claimed_at: u64,
}

impl Owner {
    /// What clients see: no `sub`.
    fn public(&self) -> Value {
        json!({"provider": self.provider, "email": self.email, "name": self.name, "claimed_at": self.claimed_at})
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    Claimed,
    BadCode,
    AlreadyClaimed,
}

/// A fresh setup code: three groups of four from [`CODE_ALPHABET`], e.g. `K7PM-3XQR-T9WD`.
fn random_code() -> String {
    let mut chars = Vec::with_capacity(CODE_LEN);
    while chars.len() < CODE_LEN {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect("system randomness");
        // 248 = 8 * 31: rejecting the rest keeps every character equally likely.
        chars.extend(buf.iter().filter(|&&b| b < 248).map(|&b| CODE_ALPHABET[b as usize % CODE_ALPHABET.len()] as char));
    }
    chars.truncate(CODE_LEN);
    chars.chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("-")
}

/// A setup code as typed (any case, dashes or spaces) in canonical form, if it could be one.
fn normalize_code(code: &str) -> Option<String> {
    let s: String = code.chars().filter(|c| !matches!(c, '-' | ' ')).map(|c| c.to_ascii_uppercase()).collect();
    (s.len() == CODE_LEN && s.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(s)
}

fn code_hash(normalized: &str) -> String {
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

/// Where a setup code is used, relative to the hub's address.
pub fn claim_path(code: &str) -> String {
    format!("/#/claim?code={code}")
}

impl Db {
    /// The hub's owner, if it has been claimed or linked.
    pub fn owner(&self) -> Option<Owner> {
        self.c()
            .query_row("SELECT provider, sub, email, name, claimed_at FROM hub_owner WHERE id = 1", [], |r| {
                Ok(Owner {
                    provider: r.get(0)?,
                    sub: r.get(1)?,
                    email: r.get(2)?,
                    name: r.get(3)?,
                    claimed_at: r.get::<_, i64>(4)? as u64,
                })
            })
            .optional()
            .ok()
            .flatten()
    }

    /// Set the owner when there is none (drop setup codes and link unattributed devices). Returns whether it was set.
    pub(crate) fn set_owner_if_none(&self, o: &Owner) -> Result<bool> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let set = tx.execute(
            "INSERT OR IGNORE INTO hub_owner (id, provider, sub, email, name, claimed_at) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
            params![o.provider, o.sub, o.email, o.name, o.claimed_at as i64],
        )? == 1;
        if set {
            tx.execute("DELETE FROM setup_codes", [])?;
            tx.execute("UPDATE devices SET account_sub = ?1 WHERE account_sub IS NULL", [&o.sub])?;
        }
        tx.commit()?;
        Ok(set)
    }

    /// Claim an unclaimed hub: use up `code` and set the owner, both or neither.
    pub(crate) fn claim_with_setup_code(&self, code: &str, o: &Owner) -> Result<Claim> {
        let Some(code) = normalize_code(code) else { return Ok(Claim::BadCode) };
        let mut c = self.c();
        let tx = c.transaction()?;
        if tx.query_row("SELECT 1 FROM hub_owner WHERE id = 1", [], |_| Ok(())).optional()?.is_some() {
            return Ok(Claim::AlreadyClaimed);
        }
        let used = tx.execute(
            "DELETE FROM setup_codes WHERE code_hash = ?1 AND expires > ?2",
            params![code_hash(&code), now() as i64],
        )?;
        if used == 0 {
            return Ok(Claim::BadCode);
        }
        tx.execute(
            "INSERT INTO hub_owner (id, provider, sub, email, name, claimed_at) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
            params![o.provider, o.sub, o.email, o.name, o.claimed_at as i64],
        )?;
        tx.execute("DELETE FROM setup_codes", [])?;
        tx.execute("UPDATE devices SET account_sub = ?1 WHERE account_sub IS NULL", [&o.sub])?;
        tx.commit()?;
        Ok(Claim::Claimed)
    }

    /// Keep the owner's email and name current (they can change at Google; `sub` can't).
    fn refresh_owner(&self, email: &str, name: &str) {
        let _ = self.c().execute("UPDATE hub_owner SET email = ?1, name = ?2 WHERE id = 1", params![email, name]);
    }

    /// Unlink the owner. Device tokens and sessions keep working. Returns who it was.
    pub fn reset_owner(&self) -> Result<Option<Owner>> {
        let owner = self.owner();
        let mut c = self.c();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM hub_owner", [])?;
        tx.execute("UPDATE devices SET account_sub = NULL", [])?;
        tx.commit()?;
        Ok(owner)
    }

    /// A new one-time setup code, valid [`SETUP_CODE_TTL_SECS`]. Only its hash is stored, so a
    /// running hub accepts a code that `grouper-hub setup-code` made. Returns it and its expiry.
    pub fn new_setup_code(&self) -> Result<(String, u64)> {
        let code = random_code();
        let created = now();
        let expires = created + SETUP_CODE_TTL_SECS;
        let c = self.c();
        c.execute("DELETE FROM setup_codes WHERE expires <= ?1", [created as i64])?;
        c.execute(
            "DELETE FROM setup_codes WHERE code_hash NOT IN
             (SELECT code_hash FROM setup_codes ORDER BY created DESC, rowid DESC LIMIT ?1)",
            [MAX_SETUP_CODES - 1],
        )?;
        c.execute(
            "INSERT INTO setup_codes (code_hash, created, expires) VALUES (?1, ?2, ?3)",
            params![code_hash(&normalize_code(&code).expect("well formed")), created as i64, expires as i64],
        )?;
        Ok((code, expires))
    }

    /// The admin device that browser sessions from Google sign-in belong to, made on first use (its
    /// token is never shown). Revoking it in Devices ends every such session; the next sign-in makes another.
    fn google_web_device(&self) -> Result<String> {
        let existing: Option<String> = self
            .c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [WEB_DEVICE_KEY], |r| r.get(0))
            .optional()?;
        if let Some(id) = existing
            && self.devices().iter().any(|d| d.id == id && !d.revoked && d.admin)
        {
            return Ok(id);
        }
        let (device, _token) = self.add_device(WEB_DEVICE_NAME, "web", true)?;
        self.c().execute(
            "INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)",
            params![WEB_DEVICE_KEY, device.id],
        )?;
        Ok(device.id)
    }
}

/// At startup: if Google sign-in is on, not in local mode, and nobody owns the hub yet, log a setup code.
pub fn announce_setup_code(hub: &Hub) {
    if hub.is_local_mode() || !hub.google.config().enabled() || hub.db.owner().is_some() {
        return;
    }
    match hub.db.new_setup_code() {
        Ok((code, _)) => {
            let base = public_url().unwrap_or_else(|| "<this hub's address>".into());
            tracing::info!(
                "This hub has no owner yet. Claim it with Google within an hour: open {base}{} (setup code {code}); \
                 `grouper-hub setup-code` makes a new code",
                claim_path(&code)
            );
        }
        Err(e) => tracing::warn!("couldn't make a setup code: {e:#}"),
    }
}

fn public_url() -> Option<String> {
    std::env::var(ENV_PUBLIC_URL).ok().map(|v| v.trim().trim_end_matches('/').to_string()).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------- configuration and state

#[derive(Clone, Debug)]
pub(crate) struct Config {
    /// App client IDs (iOS, macOS, CLI) whose ID tokens are accepted.
    pub(crate) client_ids: Vec<String>,
    /// The web client used for browser sign-in; its ID tokens are accepted too. Empty: no browser sign-in.
    pub(crate) web_client_id: String,
    pub(crate) return_url: String,
    pub(crate) public_url: Option<String>,
    pub(crate) jwks_url: String,
    pub(crate) issuers: Vec<String>,
}

impl Config {
    fn from_env() -> Self {
        let client_ids = match std::env::var(ENV_CLIENT_IDS) {
            Ok(v) => v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            Err(_) => DEFAULT_CLIENT_IDS.iter().map(|s| s.to_string()).collect(),
        };
        let web_client_id = std::env::var(ENV_WEB_CLIENT_ID).map(|v| v.trim().to_string()).unwrap_or(DEFAULT_WEB_CLIENT_ID.into());
        let return_url = std::env::var(ENV_RETURN_URL)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_RETURN_URL.into());
        Config {
            client_ids,
            web_client_id,
            return_url,
            public_url: public_url(),
            jwks_url: GOOGLE_JWKS_URL.into(),
            issuers: GOOGLE_ISSUERS.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Google sign-in is on when any client is trusted.
    pub(crate) fn enabled(&self) -> bool {
        !self.client_ids.is_empty() || !self.web_client_id.is_empty()
    }

    fn trusts(&self, aud: &str) -> bool {
        self.client_ids.iter().any(|c| c == aud) || (!self.web_client_id.is_empty() && self.web_client_id == aud)
    }
}

/// A browser sign-in in progress, keyed by the random id in its `state`.
struct Pending {
    expires: u64,
    origin: String,
    nonce: String,
    setup_code: Option<String>,
}

#[derive(Default)]
struct Issued {
    nonces: HashMap<String, u64>,
    states: HashMap<String, Pending>,
    /// Per client IP: start of the current one-minute window and nonces issued in it.
    per_ip: HashMap<IpAddr, (u64, u32)>,
}

#[derive(Clone)]
struct RsaKey {
    n: Vec<u8>,
    e: Vec<u8>,
}

#[derive(Default)]
struct Jwks {
    url: String,
    keys: HashMap<String, RsaKey>,
    fresh_until: Option<Instant>,
    last_fetch: Option<Instant>,
}

pub(crate) struct GoogleAuth {
    config: RwLock<Config>,
    issued: Mutex<Issued>,
    jwks: tokio::sync::Mutex<Jwks>,
    http: reqwest::Client,
    local_mode: AtomicBool,
}

/// Why a sign-in was refused. The message is plain and never contains the token.
#[derive(Debug)]
struct Refusal {
    status: StatusCode,
    message: String,
}

impl Refusal {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Refusal { status, message: message.into() }
    }
    fn token(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        err(self.status, self.message)
    }
}

fn disabled() -> Refusal {
    Refusal::new(StatusCode::NOT_FOUND, "Google sign-in isn't turned on for this hub (GROUPER_GOOGLE_CLIENT_IDS)")
}

/// What a verified ID token says.
#[derive(Debug)]
struct Identity {
    sub: String,
    email: String,
    name: String,
    nonce: String,
}

impl Identity {
    fn owner(&self) -> Owner {
        Owner {
            provider: PROVIDER.into(),
            sub: self.sub.clone(),
            email: self.email.clone(),
            name: self.name.clone(),
            claimed_at: now(),
        }
    }
}

/// A short, stable tag for a Google `sub` in logs.
fn sub_tag(sub: &str) -> String {
    hex::encode(&Sha256::digest(sub.as_bytes())[..6])
}

impl GoogleAuth {
    pub(crate) fn from_env() -> Self {
        Self::with_config(Config::from_env())
    }

    fn with_config(config: Config) -> Self {
        GoogleAuth {
            config: RwLock::new(config),
            issued: Mutex::default(),
            jwks: tokio::sync::Mutex::default(),
            http: reqwest::Client::builder().timeout(FETCH_TIMEOUT).build().unwrap_or_default(),
            local_mode: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_local(&self) -> bool {
        self.local_mode.load(AtomicOrdering::Relaxed)
    }

    pub(crate) fn set_local(&self, local: bool) {
        self.local_mode.store(local, AtomicOrdering::Relaxed);
    }

    pub(crate) fn config(&self) -> Config {
        self.config.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[cfg(test)]
    pub(crate) fn configure(&self, f: impl FnOnce(&mut Config)) {
        f(&mut self.config.write().unwrap());
    }

    fn issued(&self) -> std::sync::MutexGuard<'_, Issued> {
        self.issued.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A one-time nonce for `ip`, rate-limited per IP and bounded overall.
    fn issue_nonce(&self, ip: IpAddr) -> Result<(String, u64), Refusal> {
        let now = now();
        let mut s = self.issued();
        if s.per_ip.len() >= MAX_RATE_ENTRIES {
            s.per_ip.retain(|_, (start, _)| now < *start + 60);
        }
        if s.per_ip.len() >= MAX_RATE_ENTRIES && !s.per_ip.contains_key(&ip) {
            return Err(Refusal::new(StatusCode::TOO_MANY_REQUESTS, "too many sign-ins at once; try again in a minute"));
        }
        let window = s.per_ip.entry(ip).or_insert((now, 0));
        if now >= window.0 + 60 {
            *window = (now, 0);
        }
        if window.1 >= NONCES_PER_MINUTE {
            return Err(Refusal::new(StatusCode::TOO_MANY_REQUESTS, "too many sign-in attempts from here; wait a minute"));
        }
        window.1 += 1;
        s.nonces.retain(|_, expires| *expires > now);
        if s.nonces.len() >= MAX_PENDING {
            return Err(Refusal::new(StatusCode::TOO_MANY_REQUESTS, "too many sign-ins in progress; try again in a few minutes"));
        }
        let nonce = random_hex(16);
        let expires = now + NONCE_TTL_SECS;
        s.nonces.insert(nonce.clone(), expires);
        Ok((nonce, expires))
    }

    /// Use up a nonce this hub issued. False when unknown, used or expired.
    fn consume_nonce(&self, nonce: &str) -> bool {
        self.issued().nonces.remove(nonce).is_some_and(|expires| expires > now())
    }

    /// A browser `state`: base64url JSON `{"o": origin, "id": random}`. The origin is in clear so
    /// the shared return page (maybe on another hub) knows where to forward; the id is kept here.
    fn new_state(&self, origin: &str, nonce: &str, setup_code: Option<String>) -> Result<String, Refusal> {
        let now = now();
        let mut s = self.issued();
        s.states.retain(|_, p| p.expires > now);
        if s.states.len() >= MAX_PENDING {
            return Err(Refusal::new(StatusCode::TOO_MANY_REQUESTS, "too many sign-ins in progress; try again in a few minutes"));
        }
        let id = random_hex(16);
        let pending = Pending { expires: now + NONCE_TTL_SECS, origin: origin.into(), nonce: nonce.into(), setup_code };
        s.states.insert(id.clone(), pending);
        Ok(URL_SAFE_NO_PAD.encode(json!({"o": origin, "id": id}).to_string()))
    }

    /// Use up a `state` this hub made. Refused when unknown, used, expired, or naming another origin.
    fn take_state(&self, state: &str) -> Result<Pending, &'static str> {
        let (origin, id) = decode_state(state).ok_or("the sign-in came back with a broken state; start again")?;
        let pending = self.issued().states.remove(&id).ok_or("this sign-in wasn't started here, or was already used; start again")?;
        if pending.expires <= now() {
            return Err("the sign-in took too long; start again");
        }
        if pending.origin != origin {
            return Err("the sign-in came back for a different hub address; start again");
        }
        Ok(pending)
    }

    #[cfg(test)]
    fn age_pending(&self, secs: u64) {
        let mut s = self.issued();
        s.nonces.values_mut().for_each(|e| *e -= secs);
        s.states.values_mut().for_each(|p| p.expires -= secs);
    }

    #[cfg(test)]
    async fn age_jwks(&self, by: Duration) {
        let mut jwks = self.jwks.lock().await;
        jwks.last_fetch = jwks.last_fetch.and_then(|t| t.checked_sub(by));
    }

    /// Google's key `kid`, from the cache or a fetch (see the module docs for when it refetches).
    async fn key(&self, kid: &str, url: &str) -> Result<RsaKey, Refusal> {
        let mut jwks = self.jwks.lock().await;
        if jwks.url != url {
            *jwks = Jwks { url: url.into(), ..Default::default() };
        }
        let now = Instant::now();
        let fresh = jwks.fresh_until.is_some_and(|t| now < t);
        let since = |every: Duration| jwks.last_fetch.is_none_or(|t| now.duration_since(t) >= every);
        let fetch = if fresh { !jwks.keys.contains_key(kid) && since(JWKS_UNKNOWN_KID_REFETCH) } else { since(JWKS_RETRY) };
        if fetch {
            jwks.last_fetch = Some(now);
            match self.fetch_jwks(url).await {
                Ok((keys, max_age)) => {
                    jwks.keys = keys;
                    jwks.fresh_until = Some(now + max_age);
                }
                Err(e) => tracing::warn!("couldn't fetch Google's signing keys: {e:#}"),
            }
        }
        match jwks.keys.get(kid) {
            Some(k) => Ok(k.clone()),
            None if jwks.keys.is_empty() => {
                Err(Refusal::new(StatusCode::SERVICE_UNAVAILABLE, "the hub couldn't reach Google to check the sign-in; try again"))
            }
            None => Err(Refusal::token("the ID token is signed with a key Google doesn't list")),
        }
    }

    async fn fetch_jwks(&self, url: &str) -> Result<(HashMap<String, RsaKey>, Duration)> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        let max_age = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .and_then(max_age)
            .unwrap_or(JWKS_DEFAULT_MAX_AGE)
            .clamp(JWKS_MIN_MAX_AGE, JWKS_MAX_MAX_AGE);
        let body: Value = response.json().await?;
        let mut keys = HashMap::new();
        for k in body["keys"].as_array().into_iter().flatten() {
            if k["kty"] != "RSA" || k.get("use").is_some_and(|u| u != "sig") {
                continue;
            }
            let b64 = |v: &Value| v.as_str().and_then(|s| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok());
            if let (Some(kid), Some(n), Some(e)) = (k["kid"].as_str(), b64(&k["n"]), b64(&k["e"])) {
                keys.insert(kid.to_string(), RsaKey { n: strip_zeros(n), e: strip_zeros(e) });
            }
        }
        anyhow::ensure!(!keys.is_empty(), "no RSA signing keys in {url}");
        Ok((keys, Duration::from_secs(max_age)))
    }

    /// Check an ID token's signature and claims (not its nonce: the caller uses that up).
    async fn verify(&self, id_token: &str) -> Result<Identity, Refusal> {
        self.verify_inner(id_token, true).await
    }

    /// Check an ID token's signature and claims without requiring a hub nonce (for in-process [`Hub::claim_local`]).
    async fn verify_without_nonce(&self, id_token: &str) -> Result<Identity, Refusal> {
        self.verify_inner(id_token, false).await
    }

    async fn verify_inner(&self, id_token: &str, require_nonce: bool) -> Result<Identity, Refusal> {
        let cfg = self.config();
        if !cfg.enabled() {
            return Err(disabled());
        }
        let id_token = id_token.trim();
        let not_a_token = || Refusal::token("that isn't a Google ID token");
        if id_token.len() > MAX_ID_TOKEN_LEN {
            return Err(not_a_token());
        }
        let mut parts = id_token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(not_a_token());
        };
        let header = decode_json(h).ok_or_else(not_a_token)?;
        if header["alg"] != "RS256" {
            return Err(Refusal::token("the ID token must be signed with RS256"));
        }
        let kid = header["kid"].as_str().ok_or_else(not_a_token)?;
        let signature = URL_SAFE_NO_PAD.decode(s).map_err(|_| not_a_token())?;
        let key = self.key(kid, &cfg.jwks_url).await?;
        RsaPublicKeyComponents { n: &key.n[..], e: &key.e[..] }
            .verify(&RSA_PKCS1_2048_8192_SHA256, format!("{h}.{p}").as_bytes(), &signature)
            .map_err(|_| Refusal::token("the ID token's signature doesn't check out"))?;
        let claims = decode_json(p).ok_or_else(not_a_token)?;
        check_claims(&claims, &cfg, now(), require_nonce)
    }
}

fn strip_zeros(mut v: Vec<u8>) -> Vec<u8> {
    let zeros = v.iter().take_while(|&&b| b == 0).count().min(v.len().saturating_sub(1));
    v.drain(..zeros);
    v
}

fn max_age(cache_control: &str) -> Option<u64> {
    cache_control.split(',').find_map(|d| d.trim().strip_prefix("max-age=")?.trim_matches('"').parse().ok())
}

fn decode_json(part: &str) -> Option<Value> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok().filter(Value::is_object)
}

/// Google's rules for ID token claims (signature already checked).
fn check_claims(c: &Value, cfg: &Config, now: u64, require_nonce: bool) -> Result<Identity, Refusal> {
    let text = |k: &str| c[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let iss = c["iss"].as_str().unwrap_or_default();
    if !cfg.issuers.iter().any(|i| i == iss) {
        return Err(Refusal::token("the ID token isn't from Google"));
    }
    let aud: Vec<&str> = match &c["aud"] {
        Value::String(a) => vec![a.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !aud.iter().any(|a| cfg.trusts(a)) {
        return Err(Refusal::token("the ID token is for another app"));
    }
    // With several audiences, Google says the authorized party must be one of ours.
    if aud.len() > 1 && !c["azp"].as_str().is_some_and(|azp| cfg.trusts(azp)) {
        return Err(Refusal::token("the ID token is for another app"));
    }
    let exp = c["exp"].as_u64().ok_or_else(|| Refusal::token("the ID token has no expiry"))?;
    if exp + SKEW_SECS <= now {
        return Err(Refusal::token("the ID token has expired; sign in again"));
    }
    let iat = c["iat"].as_u64().ok_or_else(|| Refusal::token("the ID token has no issue time"))?;
    if iat > now + SKEW_SECS {
        return Err(Refusal::token("the ID token was issued in the future; check the hub's clock"));
    }
    let verified = matches!(&c["email_verified"], Value::Bool(true)) || c["email_verified"] == "true";
    let (Some(sub), Some(email)) = (text("sub"), text("email")) else {
        return Err(Refusal::token("the ID token has no account or email"));
    };
    if !verified {
        return Err(Refusal::token("that Google account's email isn't verified"));
    }
    let nonce = if require_nonce {
        text("nonce").ok_or_else(|| Refusal::token("the ID token has no nonce; start the sign-in from this hub"))?
    } else {
        text("nonce").unwrap_or_default()
    };
    Ok(Identity { sub, email, name: text("name").unwrap_or_default(), nonce })
}

/// The origin and id in a `state`, if it is well formed and the origin is one sign-in may return to.
fn decode_state(state: &str) -> Option<(String, String)> {
    let v = decode_json(state.trim())?;
    let (origin, id) = (v["o"].as_str()?, v["id"].as_str()?);
    (web_origin(origin).as_deref() == Some(origin) && !id.is_empty()).then(|| (origin.to_string(), id.to_string()))
}

/// `url` as an origin (`scheme://host[:port]`) that browser sign-in may return to: HTTPS, or plain
/// HTTP on a loopback address.
fn web_origin(url: &str) -> Option<String> {
    let u = url::Url::parse(url.trim()).ok()?;
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    let ok = match u.scheme() {
        "https" => u.host().is_some(),
        "http" => match u.host()? {
            url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
        },
        _ => false,
    };
    ok.then(|| u.origin().ascii_serialization())
}

/// This hub's origin for a browser sign-in: `GROUPER_PUBLIC_URL`, else the page's `Origin` when it is this
/// host, else `Host` (HTTPS when a proxy says so).
fn hub_origin(cfg: &Config, headers: &HeaderMap) -> Option<String> {
    if let Some(url) = &cfg.public_url {
        return web_origin(url);
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && crate::session::same_host(headers, origin)
    {
        return web_origin(origin);
    }
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let scheme = if crate::session::is_https(headers) { "https" } else { "http" };
    web_origin(&format!("{scheme}://{host}"))
}

/// The caller's IP for rate limiting. Behind a local proxy (Cloudflare Tunnel, Docker), the peer is
/// loopback or private and `CF-Connecting-IP` names the real client; otherwise the peer is used.
pub(crate) fn client_ip(headers: &HeaderMap, ext: &Extensions) -> IpAddr {
    let peer = ext.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    let local = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    };
    let forwarded = || {
        headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<IpAddr>().ok())
    };
    match peer {
        Some(ip) if !local(&ip) => ip,
        _ => forwarded().or(peer).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
    }
}

// ---------------------------------------------------------------- sign-in

impl Hub {
    /// Enable or disable embedded local mode (HTTP setup-code claim off, token sign-in loopback-only,
    /// in-process [`Hub::claim_local`] enabled).
    pub fn set_local_mode(&self, local: bool) {
        self.google.set_local(local);
    }

    pub fn is_local_mode(&self) -> bool {
        self.google.is_local()
    }

    /// Claim an embedded local hub in-process with a Google ID token (no setup code and no hub nonce).
    /// Checks signature, issuer, audience, expiry/issue times and `email_verified`. If already claimed
    /// by the same Google `sub`, refreshes name/email and returns the owner; another `sub` is refused.
    pub async fn claim_local(&self, id_token: &str) -> Result<Owner> {
        anyhow::ensure!(self.is_local_mode(), "claim_local is only available when the hub runs in local mode");
        let id = self.google.verify_without_nonce(id_token).await.map_err(|r| anyhow::anyhow!("{}", r.message))?;
        let _ = self.db.ensure_hub_device()?;
        let candidate = id.owner();
        if self.db.set_owner_if_none(&candidate)? {
            tracing::info!(email = %id.email, "local hub claimed with Google");
            return self.db.owner().ok_or_else(|| anyhow::anyhow!("owner missing after claim"));
        }
        match self.db.owner() {
            Some(owner) if owner.provider == PROVIDER && owner.sub == id.sub => {
                self.db.refresh_owner(&id.email, &id.name);
                let _ = self
                    .db
                    .c()
                    .execute("UPDATE devices SET account_sub = ?1 WHERE account_sub IS NULL", [&id.sub]);
                self.db.owner().ok_or_else(|| anyhow::anyhow!("owner missing after refresh"))
            }
            Some(_) => {
                tracing::info!(sub = %sub_tag(&id.sub), "local claim refused: not this hub's owner");
                anyhow::bail!("this hub belongs to a different Google account")
            }
            None => anyhow::bail!("failed to store hub owner"),
        }
    }

    /// Claim the embedded local hub with `id_token` and issue (or reuse by `install_id`) an admin token
    /// for the embedding app (`macos`, `windows`, `linux`, `ios` or `cli`).
    pub async fn claim_local_client(
        &self,
        id_token: &str,
        client: &str,
        device_name: Option<&str>,
        install_id: Option<&str>,
    ) -> Result<(Owner, crate::db::IssuedDevice)> {
        let platform = match client {
            p @ ("ios" | "macos" | "windows" | "linux" | "cli") => p,
            _ => anyhow::bail!("client must be ios, macos, windows, linux or cli"),
        };
        let install_id = crate::server::check_install_id(install_id).map_err(anyhow::Error::msg)?;
        let owner = self.claim_local(id_token).await?;
        let name: String = device_name.unwrap_or("").trim().chars().take(MAX_DEVICE_NAME_CHARS).collect();
        let name = if name.is_empty() { DEFAULT_DEVICE_NAME.to_string() } else { name };
        let install = install_id.map(|id| crate::db::Install { id, kind: platform });
        let issued = self.issue_device(&name, platform, true, install, None).await?;
        Ok((owner, issued))
    }
}

/// After a verified ID token and nonce: is this person allowed in? Claims an unclaimed hub with a setup code
/// in server mode; in local mode HTTP claiming and setup codes are refused.
fn authorize(hub: &Hub, id: &Identity, setup_code: Option<&str>) -> Result<(), Refusal> {
    let code = setup_code.map(str::trim).filter(|c| !c.is_empty());
    if hub.is_local_mode() && code.is_some() {
        return Err(Refusal::new(StatusCode::FORBIDDEN, "setup-code claiming over HTTP is disabled in local mode"));
    }
    match hub.db.owner() {
        Some(owner) if owner.provider == PROVIDER && owner.sub == id.sub => {
            hub.db.refresh_owner(&id.email, &id.name);
            Ok(())
        }
        Some(_) => {
            tracing::info!(sub = %sub_tag(&id.sub), "Google sign-in refused: not this hub's owner");
            Err(Refusal::new(StatusCode::FORBIDDEN, "this hub belongs to a different Google account"))
        }
        None if hub.is_local_mode() => Err(Refusal::new(
            StatusCode::FORBIDDEN,
            "this local hub has no owner yet; claim it in Grouper on this computer",
        )),
        None => {
            let code = code.ok_or_else(|| {
                Refusal::new(
                    StatusCode::FORBIDDEN,
                    "this hub has no owner yet: claim it with the setup code from the hub's log \
                     (or run `grouper-hub setup-code` on the hub)",
                )
            })?;
            match hub.db.claim_with_setup_code(code, &id.owner()) {
                Ok(Claim::Claimed) => {
                    tracing::info!(email = %id.email, "hub claimed with Google");
                    Ok(())
                }
                Ok(Claim::BadCode) => Err(Refusal::new(
                    StatusCode::FORBIDDEN,
                    "that setup code isn't valid or has expired; run `grouper-hub setup-code` on the hub for a new one",
                )),
                Ok(Claim::AlreadyClaimed) => {
                    Err(Refusal::new(StatusCode::CONFLICT, "this hub was claimed a moment ago; sign in again"))
                }
                Err(e) => Err(Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
            }
        }
    }
}

fn web_auth(hub: &Hub) -> Result<Auth, Refusal> {
    let device_id = hub.db.google_web_device().map_err(|e| Refusal::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(Auth { device_id, admin: true })
}

/// `GET /v1/auth/status`: whether the hub has an owner, which Google sign-ins are on, and whether it runs in local mode.
async fn status(State(hub): State<Arc<Hub>>) -> Response {
    let cfg = hub.google.config();
    Json(json!({
        "claimed": hub.db.owner().is_some(),
        "google": cfg.enabled(),
        "google_web": !cfg.web_client_id.is_empty(),
        "local": hub.is_local_mode(),
    }))
    .into_response()
}

/// `POST /v1/auth/google/nonce`: a one-time nonce for an app's Google sign-in.
async fn nonce(State(hub): State<Arc<Hub>>, headers: HeaderMap, ext: Extensions) -> Response {
    if !hub.google.config().enabled() {
        return disabled().into_response();
    }
    match hub.google.issue_nonce(client_ip(&headers, &ext)) {
        Ok((nonce, expires)) => Json(json!({"nonce": nonce, "expires": expires})).into_response(),
        Err(r) => r.into_response(),
    }
}

#[derive(Deserialize)]
struct SignInBody {
    id_token: String,
    #[serde(default)]
    setup_code: Option<String>,
    client: String,
    #[serde(default)]
    device_name: Option<String>,
    /// Apps: this install's random id. A live device of the same install and client gets a fresh
    /// token (the old one is revoked) instead of a new device being made.
    #[serde(default)]
    install_id: Option<String>,
}

/// `POST /v1/auth/google`: sign in (or claim the hub) with a Google ID token. Web: a session cookie,
/// answered like `POST /v1/session`. Apps: an admin device token, shown once: a new device, or the
/// same device again for the same `install_id` and client.
async fn sign_in(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<SignInBody>) -> Response {
    let platform = match body.client.as_str() {
        "web" => None,
        p @ ("ios" | "macos" | "cli") => Some(p),
        _ => return err(StatusCode::BAD_REQUEST, "client must be web, ios, macos or cli"),
    };
    let install_id = match crate::server::check_install_id(body.install_id.as_deref()) {
        Ok(id) => id,
        Err(why) => return err(StatusCode::BAD_REQUEST, why),
    };
    if platform.is_none()
        && let Some(why) = crate::session::login_csrf_refusal(&headers)
    {
        return err(StatusCode::FORBIDDEN, why);
    }
    let id = match hub.google.verify(&body.id_token).await {
        Ok(id) => id,
        Err(r) => return r.into_response(),
    };
    if !hub.google.consume_nonce(&id.nonce) {
        return Refusal::token("this sign-in wasn't started on this hub, was already used, or took too long; try again")
            .into_response();
    }
    if let Err(r) = authorize(&hub, &id, body.setup_code.as_deref()) {
        return r.into_response();
    }
    tracing::info!(email = %id.email, client = %body.client, "Google sign-in");
    let Some(platform) = platform else {
        return match web_auth(&hub) {
            Ok(auth) => crate::session::session_response(&hub, &headers, &auth),
            Err(r) => r.into_response(),
        };
    };
    let name: String = body.device_name.as_deref().unwrap_or("").trim().chars().take(MAX_DEVICE_NAME_CHARS).collect();
    let name = if name.is_empty() { DEFAULT_DEVICE_NAME.to_string() } else { name };
    let install = install_id.map(|id| crate::db::Install { id, kind: platform });
    match hub.issue_device(&name, platform, true, install, None).await {
        Ok(issued) => {
            let d = issued.device;
            Json(json!({
                "token": issued.token,
                "device": {"id": d.id, "name": d.name, "platform": d.platform, "admin": d.admin},
            }))
            .into_response()
        }
        Err(e) => crate::server::issue_error(e),
    }
}

#[derive(Deserialize, Default)]
struct StartBody {
    #[serde(default)]
    setup_code: Option<String>,
}

/// `POST /v1/auth/google/start`: everything a browser needs to go to Google: a nonce, a `state`, and the
/// authorization URL. Google posts the result to the shared return page, which forwards it here.
async fn start(State(hub): State<Arc<Hub>>, headers: HeaderMap, ext: Extensions, body: Bytes) -> Response {
    let cfg = hub.google.config();
    if cfg.web_client_id.is_empty() {
        return err(StatusCode::NOT_FOUND, "browser Google sign-in isn't turned on for this hub (GROUPER_GOOGLE_WEB_CLIENT_ID)");
    }
    let start: StartBody = if body.iter().all(u8::is_ascii_whitespace) {
        StartBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(s) => s,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("bad request body: {e}")),
        }
    };
    let Some(origin) = hub_origin(&cfg, &headers) else {
        return err(
            StatusCode::BAD_REQUEST,
            "browser Google sign-in needs the hub on HTTPS (or localhost); set GROUPER_PUBLIC_URL to its https:// address",
        );
    };
    let (nonce, expires) = match hub.google.issue_nonce(client_ip(&headers, &ext)) {
        Ok(n) => n,
        Err(r) => return r.into_response(),
    };
    let setup_code = start.setup_code.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
    if hub.is_local_mode() && (setup_code.is_some() || hub.db.owner().is_none()) {
        return err(
            StatusCode::FORBIDDEN,
            "claiming over HTTP is disabled in local mode; sign in through Grouper on this computer",
        );
    }
    let state = match hub.google.new_state(&origin, &nonce, setup_code) {
        Ok(s) => s,
        Err(r) => return r.into_response(),
    };
    let auth_url = url::Url::parse_with_params(
        GOOGLE_AUTH_URL,
        [
            ("client_id", cfg.web_client_id.as_str()),
            ("redirect_uri", cfg.return_url.as_str()),
            ("response_type", "id_token"),
            ("scope", "openid email profile"),
            ("response_mode", "form_post"),
            ("prompt", "select_account"),
            ("nonce", nonce.as_str()),
            ("state", state.as_str()),
        ],
    )
    .expect("constant URL");
    Json(json!({
        "nonce": nonce,
        "state": state,
        "client_id": cfg.web_client_id,
        "redirect_uri": cfg.return_url,
        "auth_url": auth_url.as_str(),
        "expires": expires,
    }))
    .into_response()
}

fn form(bytes: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(bytes).into_owned().collect()
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn html_page(status: StatusCode, csp: &str, body: &str) -> Response {
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\">\
         <meta name=\"viewport\" content=\"width=device-width\"><title>Grouper sign-in</title></head><body>{body}</body></html>"
    );
    let mut response = (status, page).into_response();
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    response
}

const SUBMIT_SCRIPT: &str = "document.forms[0].submit()";

/// `GET|POST /oauth/google/return`: the shared return page. Google `form_post`s the ID token (or an error)
/// and `state` here; the page reads the hub's origin from `state` and posts them on to that hub. It stores
/// nothing and trusts nothing: the receiving hub checks `state` and the token's nonce.
async fn return_page(method: Method, uri: Uri, body: Bytes) -> Response {
    let mut params = form(uri.query().unwrap_or("").as_bytes());
    if method == Method::POST && body.len() <= MAX_FORM_BYTES {
        params.extend(form(&body));
    }
    let Some((origin, _)) = params.get("state").and_then(|s| decode_state(s)) else {
        return html_page(
            StatusCode::BAD_REQUEST,
            "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            "<p>This sign-in can't continue: it doesn't say which hub to return to (or that hub isn't on HTTPS). \
             Go back to your hub and start again.</p>",
        );
    };
    let fields: String = ["id_token", "state", "error"]
        .iter()
        .filter_map(|k| params.get(*k).map(|v| (k, v)))
        .map(|(k, v)| format!("<input type=\"hidden\" name=\"{k}\" value=\"{}\">", html_escape(v)))
        .collect();
    let script_hash = STANDARD.encode(Sha256::digest(SUBMIT_SCRIPT.as_bytes()));
    let csp = format!(
        "default-src 'none'; script-src 'sha256-{script_hash}'; form-action {origin}; base-uri 'none'; frame-ancestors 'none'"
    );
    let body = format!(
        "<form method=\"post\" action=\"{}\">{fields}<p>Returning to your hub…</p>\
         <noscript><button type=\"submit\">Continue</button></noscript></form><script>{SUBMIT_SCRIPT}</script>",
        html_escape(&format!("{origin}{RETURN_PATH}"))
    );
    html_page(StatusCode::OK, &csp, &body)
}

fn see_other(location: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, v);
    }
    response
}

fn return_failed(message: &str) -> Response {
    tracing::info!("browser Google sign-in refused: {message}");
    let message: String = url::form_urlencoded::byte_serialize(message.as_bytes()).collect();
    see_other(&format!("/#/claim?error={message}"))
}

/// `POST /v1/auth/google/return`: the browser arrives here from the return page with Google's answer. It's a
/// cross-site form POST by design, so the session CSRF guard skips it; the one-time `state` (made here, bound
/// to the nonce) is the protection. Success: an admin session cookie and a redirect to the app.
async fn finish(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    let params = if body.len() <= MAX_FORM_BYTES { form(&body) } else { HashMap::new() };
    let Some(state) = params.get("state") else {
        return return_failed("the sign-in came back without its state; start again");
    };
    let pending = match hub.google.take_state(state) {
        Ok(p) => p,
        Err(why) => return return_failed(why),
    };
    // The state is used up; its nonce goes with it whatever happens next.
    let nonce_ok = hub.google.consume_nonce(&pending.nonce);
    if let Some(e) = params.get("error") {
        let e: String = e.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect();
        return return_failed(&format!("Google didn't sign you in ({e})"));
    }
    let Some(id_token) = params.get("id_token") else {
        return return_failed("Google didn't send a sign-in; start again");
    };
    let id = match hub.google.verify(id_token).await {
        Ok(id) => id,
        Err(r) => return return_failed(&r.message),
    };
    if !nonce_ok || id.nonce != pending.nonce {
        return return_failed("this sign-in wasn't started here, or took too long; start again");
    }
    if let Err(r) = authorize(&hub, &id, pending.setup_code.as_deref()) {
        return return_failed(&r.message);
    }
    tracing::info!(email = %id.email, client = "web", "Google sign-in");
    let auth = match web_auth(&hub) {
        Ok(a) => a,
        Err(r) => return return_failed(&r.message),
    };
    match crate::session::start_session(&hub, &headers, &auth) {
        Ok((cookies, _)) => {
            let mut response = see_other("/#/");
            for cookie in cookies {
                response.headers_mut().append(header::SET_COOKIE, cookie);
            }
            response
        }
        Err(e) => return_failed(&format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct LinkBody {
    id_token: String,
}

/// `GET /v1/auth/google/link` (admin): the linked Google account, if any.
async fn linked(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    Json(json!({"owner": hub.db.owner().map(|o| o.public())})).into_response()
}

/// `POST /v1/auth/google/link` (admin): link a Google account to an existing hub as its owner.
async fn link(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<LinkBody>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let id = match hub.google.verify(&body.id_token).await {
        Ok(id) => id,
        Err(r) => return r.into_response(),
    };
    if !hub.google.consume_nonce(&id.nonce) {
        return Refusal::token("this sign-in wasn't started on this hub, was already used, or took too long; try again")
            .into_response();
    }
    match hub.db.set_owner_if_none(&id.owner()) {
        Ok(true) => tracing::info!(email = %id.email, "Google account linked as the hub's owner"),
        Ok(false) => match hub.db.owner() {
            Some(o) if o.provider == PROVIDER && o.sub == id.sub => hub.db.refresh_owner(&id.email, &id.name),
            _ => return err(StatusCode::CONFLICT, "another Google account is already linked to this hub; unlink it first"),
        },
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
    Json(json!({"owner": hub.db.owner().map(|o| o.public())})).into_response()
}

/// `DELETE /v1/auth/google/link` (admin): unlink the owner. Tokens and sessions keep working.
async fn unlink(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.db.reset_owner() {
        Ok(owner) => {
            if let Some(o) = &owner {
                tracing::info!(email = %o.email, "Google account unlinked from the hub");
            }
            Json(json!({"unlinked": owner.is_some()})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/auth/status", get(status))
        .route("/v1/auth/google", post(sign_in))
        .route("/v1/auth/google/nonce", post(nonce))
        .route("/v1/auth/google/start", post(start))
        .route(RETURN_PATH, post(finish))
        .route(RETURN_PAGE_PATH, get(return_page).post(return_page))
        .route("/v1/auth/google/link", get(linked).post(link).delete(unlink))
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{KeyPair, RSA_PKCS1_SHA256, RsaKeyPair};
    use reqwest::StatusCode as S;

    use super::*;
    use crate::server::router;

    const CLIENT: &str = "test-ios.apps.googleusercontent.com";
    const WEB_CLIENT: &str = "test-web.apps.googleusercontent.com";
    const ISS: &str = "https://accounts.google.com";
    const KID: &str = "test-key-1";
    const OWNER: &str = "100000000000000000001";
    const STRANGER: &str = "100000000000000000002";
    const RETURN_URL: &str = "https://return.example/oauth/google/return";

    /// The test's Google key, and another one Google doesn't have.
    fn keys() -> &'static (RsaKeyPair, RsaKeyPair) {
        static KEYS: OnceLock<(RsaKeyPair, RsaKeyPair)> = OnceLock::new();
        KEYS.get_or_init(|| {
            (RsaKeyPair::generate(KeySize::Rsa2048).unwrap(), RsaKeyPair::generate(KeySize::Rsa2048).unwrap())
        })
    }

    /// `n` and `e` from a DER `RSAPublicKey` (SEQUENCE of two INTEGERs).
    fn rsa_components(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
        fn read(d: &[u8], tag: u8) -> (&[u8], &[u8]) {
            assert_eq!(d[0], tag);
            let (len, start) = if d[1] < 0x80 {
                (d[1] as usize, 2)
            } else {
                let n = (d[1] & 0x7f) as usize;
                (d[2..2 + n].iter().fold(0usize, |a, &b| (a << 8) | b as usize), 2 + n)
            };
            (&d[start..start + len], &d[start + len..])
        }
        let (seq, _) = read(der, 0x30);
        let (n, rest) = read(seq, 0x02);
        let (e, _) = read(rest, 0x02);
        (strip_zeros(n.to_vec()), strip_zeros(e.to_vec()))
    }

    fn mint_with(key: &RsaKeyPair, header: &Value, claims: &Value) -> String {
        let input = format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut sig = vec![0; key.public_modulus_len()];
        key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig))
    }

    fn mint(claims: &Value) -> String {
        mint_with(&keys().0, &json!({"alg": "RS256", "kid": KID, "typ": "JWT"}), claims)
    }

    fn claims(nonce: &str, sub: &str) -> Value {
        let t = now();
        json!({
            "iss": ISS, "aud": CLIENT, "azp": CLIENT, "sub": sub, "email": format!("{sub}@example.com"),
            "email_verified": true, "name": "Test Person", "iat": t, "exp": t + 3600, "nonce": nonce,
        })
    }

    fn web_claims(nonce: &str, sub: &str) -> Value {
        let mut c = claims(nonce, sub);
        c["aud"] = json!(WEB_CLIENT);
        c["azp"] = json!(WEB_CLIENT);
        c
    }

    struct Fixture {
        hub: Arc<Hub>,
        base: String,
        admin_token: String,
        device_token: String,
        dir: std::path::PathBuf,
        http: reqwest::Client,
        jwks_hits: Arc<AtomicUsize>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await });
        format!("http://{addr}")
    }

    /// A fake Google key set that counts fetches.
    async fn jwks_server(hits: Arc<AtomicUsize>) -> String {
        let (n, e) = rsa_components(keys().0.public_key().as_ref());
        let body = json!({"keys": [
            {"kty": "RSA", "alg": "RS256", "use": "sig", "kid": KID, "n": URL_SAFE_NO_PAD.encode(n), "e": URL_SAFE_NO_PAD.encode(e)},
            {"kty": "EC", "kid": "ignored", "crv": "P-256"},
        ]});
        let app = Router::new().route(
            "/certs",
            get(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                async move { ([(header::CACHE_CONTROL, "public, max-age=3600, must-revalidate")], Json(body)) }
            }),
        );
        format!("{}/certs", serve(app).await)
    }

    async fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!("grouper-google-test-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Laptop", "windows", true).unwrap();
        let (_, device_token) = db.add_device("Node", "linux", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let jwks_hits = Arc::new(AtomicUsize::new(0));
        let jwks = jwks_server(jwks_hits.clone()).await;
        hub.google.configure(|c| {
            c.client_ids = vec![CLIENT.into()];
            c.web_client_id = WEB_CLIENT.into();
            c.return_url = RETURN_URL.into();
            c.public_url = None;
            c.jwks_url = jwks;
            c.issuers = vec![ISS.into()];
        });
        let base = serve(router(hub.clone())).await;
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Fixture { hub, base, admin_token, device_token, dir, http, jwks_hits }
    }

    async fn parts(r: reqwest::Response) -> (S, HeaderMap, Value) {
        let status = r.status();
        let headers = r.headers().clone();
        (status, headers, r.json().await.unwrap_or(json!(null)))
    }

    fn form_body(pairs: &[(&str, &str)]) -> String {
        url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
    }

    fn cookie_pair(headers: &HeaderMap) -> String {
        let set = headers.get(header::SET_COOKIE).expect("set-cookie").to_str().unwrap();
        set.split(';').next().unwrap().to_string()
    }

    impl Fixture {
        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        async fn nonce(&self) -> String {
            let (status, _, body) = parts(self.http.post(self.url("/v1/auth/google/nonce")).send().await.unwrap()).await;
            assert_eq!(status, S::OK, "{body}");
            assert!(body["expires"].as_u64().unwrap() > now());
            body["nonce"].as_str().unwrap().to_string()
        }

        async fn sign_in(&self, token: &str, client: &str, code: Option<&str>) -> (S, HeaderMap, Value) {
            let mut body = json!({"id_token": token, "client": client});
            if let Some(code) = code {
                body["setup_code"] = json!(code);
            }
            let mut rb = self.http.post(self.url("/v1/auth/google")).json(&body);
            if client == "web" {
                rb = rb.header("x-grouper", "1").header("origin", &self.base);
            }
            parts(rb.send().await.unwrap()).await
        }

        /// Claim the hub as `OWNER` from the iPhone app; returns the app's admin token.
        async fn claim(&self) -> String {
            let (code, _) = self.hub.db.new_setup_code().unwrap();
            let token = mint(&claims(&self.nonce().await, OWNER));
            let (status, _, body) = self.sign_in(&token, "ios", Some(&code)).await;
            assert_eq!(status, S::OK, "{body}");
            body["token"].as_str().unwrap().to_string()
        }

        async fn status(&self) -> Value {
            self.http.get(self.url("/v1/auth/status")).send().await.unwrap().json().await.unwrap()
        }

        async fn start(&self, setup_code: Option<&str>) -> Value {
            let body = setup_code.map(|c| json!({"setup_code": c}).to_string()).unwrap_or_default();
            let r = self.http.post(self.url("/v1/auth/google/start")).header("origin", &self.base).body(body);
            let (status, _, body) = parts(r.send().await.unwrap()).await;
            assert_eq!(status, S::OK, "{body}");
            body
        }

        /// The browser's form POST from the return page to this hub: cross-site, as in real life.
        async fn finish(&self, pairs: &[(&str, &str)]) -> (S, HeaderMap) {
            let r = self
                .http
                .post(self.url(RETURN_PATH))
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("origin", "https://return.example")
                .header("sec-fetch-site", "cross-site")
                .body(form_body(pairs))
                .send()
                .await
                .unwrap();
            (r.status(), r.headers().clone())
        }
    }

    #[test]
    fn setup_codes_origins_and_cache_control() {
        let code = random_code();
        assert_eq!(code.len(), 14);
        assert_eq!(code.matches('-').count(), 2);
        assert!(code.replace('-', "").bytes().all(|b| CODE_ALPHABET.contains(&b)));
        assert_ne!(random_code(), code);
        assert_eq!(normalize_code(" k7pm 3xqr-t9wd ").as_deref(), Some("K7PM3XQRT9WD"));
        assert_eq!(normalize_code("K7PM-3XQR-T9W0"), None, "0 isn't in the alphabet");
        assert_eq!(normalize_code("K7PM-3XQR"), None);
        assert_eq!(max_age("public, max-age=19940, must-revalidate"), Some(19940));
        assert_eq!(max_age("no-cache"), None);
        assert_eq!(web_origin("https://hub.example.com/x?y"), Some("https://hub.example.com".into()));
        assert_eq!(web_origin("http://localhost:8080"), Some("http://localhost:8080".into()));
        assert_eq!(web_origin("http://[::1]:8080"), Some("http://[::1]:8080".into()));
        assert_eq!(web_origin("http://hub.lan:8080"), None);
        assert_eq!(web_origin("https://user:pw@hub.example.com"), None);
        assert_eq!(web_origin("javascript:alert(1)"), None);
    }

    #[tokio::test]
    async fn status_and_claiming_with_a_setup_code() {
        let f = fixture().await;
        assert_eq!(f.status().await, json!({"claimed": false, "google": true, "google_web": true, "local": false}));

        // No code, a wrong code, an expired code: refused. Each try needs a fresh nonce.
        let (status, _, body) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", None).await;
        assert_eq!(status, S::FORBIDDEN, "{body}");
        let (status, _, _) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", Some("AAAA-BBBB-CCCC")).await;
        assert_eq!(status, S::FORBIDDEN);
        let (expired, _) = f.hub.db.new_setup_code().unwrap();
        f.hub.db.c().execute("UPDATE setup_codes SET expires = ?1", [now() as i64 - 1]).unwrap();
        let (status, _, _) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", Some(&expired)).await;
        assert_eq!(status, S::FORBIDDEN);
        assert_eq!(f.status().await["claimed"], false);

        // A valid code (typed loosely) claims the hub and returns an admin token for the app.
        let (code, _) = f.hub.db.new_setup_code().unwrap();
        let typed = code.to_lowercase().replace('-', " ");
        let (status, _, body) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", Some(&typed)).await;
        assert_eq!(status, S::OK, "{body}");
        assert_eq!(body["device"]["name"], "Grouper app");
        assert_eq!(body["device"]["platform"], "ios");
        assert_eq!(body["device"]["admin"], true);
        let app_token = body["token"].as_str().unwrap();
        let r = f.http.get(f.url("/v1/limits")).bearer_auth(app_token).send().await.unwrap();
        assert_eq!(r.status(), S::OK, "the app's token is an admin token");
        assert_eq!(f.status().await["claimed"], true);
        let owner = f.hub.db.owner().unwrap();
        assert_eq!((owner.sub.as_str(), owner.email.as_str()), (OWNER, "100000000000000000001@example.com"));
        let codes: i64 = f.hub.db.c().query_row("SELECT COUNT(*) FROM setup_codes", [], |r| r.get(0)).unwrap();
        assert_eq!(codes, 0, "claiming uses up every setup code");

        // Once claimed, a code doesn't let anyone else in.
        let (code, _) = f.hub.db.new_setup_code().unwrap();
        let (status, _, _) = f.sign_in(&mint(&claims(&f.nonce().await, STRANGER)), "ios", Some(&code)).await;
        assert_eq!(status, S::FORBIDDEN);
        assert_eq!(f.hub.db.owner().unwrap().sub, OWNER);
    }

    #[tokio::test]
    async fn the_owner_signs_in_again_and_others_are_refused() {
        let f = fixture().await;
        f.claim().await;
        let token = mint(&claims(&f.nonce().await, OWNER));
        let r = f
            .http
            .post(f.url("/v1/auth/google"))
            .json(&json!({"id_token": token, "client": "macos", "device_name": "  Grouper on Mac  "}))
            .send()
            .await
            .unwrap();
        let (status, _, body) = parts(r).await;
        assert_eq!(status, S::OK, "{body}");
        assert_eq!(body["device"]["name"], "Grouper on Mac");
        assert_eq!(body["device"]["platform"], "macos");

        let token = mint(&claims(&f.nonce().await, STRANGER));
        let (status, _, body) = f.sign_in(&token, "cli", None).await;
        assert_eq!(status, S::FORBIDDEN);
        assert!(body["error"].as_str().unwrap().contains("different Google account"));
        assert!(!body.to_string().contains(&token), "errors never echo the token");

        let (status, _, _) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "tv", None).await;
        assert_eq!(status, S::BAD_REQUEST);
    }

    #[tokio::test]
    async fn signing_in_again_from_the_same_install_reuses_its_device() {
        let f = fixture().await;
        f.claim().await;
        let sign_in = |install: Option<&'static str>, client: &'static str| {
            let f = &f;
            async move {
                let mut body = json!({"id_token": mint(&claims(&f.nonce().await, OWNER)), "client": client});
                if let Some(id) = install {
                    body["install_id"] = json!(id);
                }
                parts(f.http.post(f.url("/v1/auth/google")).json(&body).send().await.unwrap()).await
            }
        };
        const INSTALL: &str = "ios-install-0123456789abcdef";
        let (status, _, first) = sign_in(Some(INSTALL), "ios").await;
        assert_eq!(status, S::OK, "{first}");
        let (_, _, second) = sign_in(Some(INSTALL), "ios").await;
        assert_eq!(second["device"]["id"], first["device"]["id"], "same device");
        assert_eq!(second["device"]["admin"], true);
        let old = first["token"].as_str().unwrap();
        assert_eq!(f.http.get(f.url("/v1/limits")).bearer_auth(old).send().await.unwrap().status(), S::UNAUTHORIZED);
        let new = second["token"].as_str().unwrap();
        assert_eq!(f.http.get(f.url("/v1/limits")).bearer_auth(new).send().await.unwrap().status(), S::OK);
        // Another client kind, or no install id: a new device.
        let (_, _, mac) = sign_in(Some(INSTALL), "macos").await;
        assert_ne!(mac["device"]["id"], first["device"]["id"]);
        let (_, _, plain) = sign_in(None, "ios").await;
        assert_ne!(plain["device"]["id"], first["device"]["id"]);
        let (status, _, _) = sign_in(Some("nope"), "ios").await;
        assert_eq!(status, S::BAD_REQUEST);
    }

    #[tokio::test]
    async fn web_sign_in_gives_a_working_admin_session() {
        let f = fixture().await;
        f.claim().await;
        // Without the web client's CSRF header and Origin: refused, like POST /v1/session.
        let token = mint(&claims(&f.nonce().await, OWNER));
        let r = f.http.post(f.url("/v1/auth/google")).json(&json!({"id_token": token, "client": "web"})).send().await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);

        let (status, headers, body) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "web", None).await;
        assert_eq!(status, S::OK, "{body}");
        assert_eq!(body["signed_in"], true);
        assert_eq!(body["admin"], true);
        assert!(body["expires"].as_u64().unwrap() > now());
        let set = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
        let cookie = cookie_pair(&headers);
        let r = f.http.get(f.url("/v1/limits")).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK);
        let r = f
            .http
            .put(f.url("/v1/limits"))
            .header("cookie", &cookie)
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .json(&json!({"thread_budget_usd": 0.25}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        assert_eq!(f.hub.db.limits().thread_budget_usd, Some(0.25));

        // Browser sessions share one admin device, whose token is never shown.
        let (status, _, second) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "web", None).await;
        assert_eq!(status, S::OK);
        assert_eq!(second["device_id"], body["device_id"]);
        assert_eq!(second["name"], WEB_DEVICE_NAME);
        // Revoking that device ends the sessions; the next sign-in makes a new one.
        f.hub.db.revoke_device(body["device_id"].as_str().unwrap()).unwrap();
        let r = f.http.get(f.url("/v1/limits")).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::UNAUTHORIZED);
        let (status, _, third) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "web", None).await;
        assert_eq!(status, S::OK);
        assert_ne!(third["device_id"], body["device_id"]);
    }

    #[tokio::test]
    async fn nonces_are_one_time_ours_and_rate_limited() {
        let f = fixture().await;
        f.claim().await;
        let token = mint(&claims(&f.nonce().await, OWNER));
        assert_eq!(f.sign_in(&token, "ios", None).await.0, S::OK);
        let (status, _, body) = f.sign_in(&token, "ios", None).await;
        assert_eq!(status, S::UNAUTHORIZED, "a replayed token (and nonce) is refused: {body}");
        let (status, _, _) = f.sign_in(&mint(&claims(&random_hex(16), OWNER)), "ios", None).await;
        assert_eq!(status, S::UNAUTHORIZED, "a nonce this hub never issued");
        let (status, _, _) = f.sign_in(&mint(&claims("", OWNER)), "ios", None).await;
        assert_eq!(status, S::UNAUTHORIZED, "no nonce");
        let old = f.nonce().await;
        f.hub.google.age_pending(NONCE_TTL_SECS);
        let (status, _, _) = f.sign_in(&mint(&claims(&old, OWNER)), "ios", None).await;
        assert_eq!(status, S::UNAUTHORIZED, "an expired nonce");

        // 20 nonces a minute per client IP (the real client's, behind a local proxy).
        let ask = |ip: &'static str| f.http.post(f.url("/v1/auth/google/nonce")).header("cf-connecting-ip", ip).send();
        for _ in 0..NONCES_PER_MINUTE {
            assert_eq!(ask("203.0.113.7").await.unwrap().status(), S::OK);
        }
        assert_eq!(ask("203.0.113.7").await.unwrap().status(), S::TOO_MANY_REQUESTS);
        assert_eq!(ask("203.0.113.8").await.unwrap().status(), S::OK);
    }

    #[tokio::test]
    async fn bad_tokens_are_refused_without_using_up_the_nonce() {
        let f = fixture().await;
        f.claim().await;
        assert_eq!(f.jwks_hits.load(Ordering::SeqCst), 1, "keys are fetched once and cached");
        let nonce = f.nonce().await;
        let good = claims(&nonce, OWNER);
        let with = |k: &str, v: Value| {
            let mut c = good.clone();
            c[k] = v;
            c
        };
        let header = json!({"alg": "RS256", "kid": KID});
        let tampered = {
            let token = mint(&good);
            let p: Vec<&str> = token.split('.').collect();
            format!("{}.{}.{}", p[0], URL_SAFE_NO_PAD.encode(with("sub", json!(STRANGER)).to_string()), p[2])
        };
        let mut multi_untrusted = with("aud", json!([CLIENT, "other"]));
        multi_untrusted["azp"] = json!("other");
        let cases: Vec<(&str, String)> = vec![
            ("wrong aud", mint(&with("aud", json!("someone-else.apps.googleusercontent.com")))),
            ("aud list without our azp", mint(&multi_untrusted)),
            ("wrong iss", mint(&with("iss", json!("https://evil.example")))),
            ("expired", mint(&with("exp", json!(now() - SKEW_SECS - 60)))),
            ("issued in the future", mint(&with("iat", json!(now() + SKEW_SECS + 60)))),
            ("email not verified", mint(&with("email_verified", json!(false)))),
            ("signed by another key", mint_with(&keys().1, &header, &good)),
            ("tampered payload", tampered),
            ("HS256", mint_with(&keys().0, &json!({"alg": "HS256", "kid": KID}), &good)),
            ("unknown kid", mint_with(&keys().0, &json!({"alg": "RS256", "kid": "nope"}), &good)),
            ("not a JWT", "abc.def".into()),
        ];
        for (what, token) in &cases {
            let (status, _, body) = f.sign_in(token, "ios", None).await;
            assert_eq!(status, S::UNAUTHORIZED, "{what}: {body}");
            assert!(!body.to_string().contains(token.as_str()), "{what}: errors never echo the token");
        }
        // The keys were fetched under a minute ago, so the unknown kid didn't refetch them. A minute later,
        // one unknown kid does, once; another straight after doesn't.
        assert_eq!(f.jwks_hits.load(Ordering::SeqCst), 1);
        f.hub.google.age_jwks(JWKS_UNKNOWN_KID_REFETCH).await;
        let unknown = mint_with(&keys().0, &json!({"alg": "RS256", "kid": "nope-2"}), &good);
        assert_eq!(f.sign_in(&unknown, "ios", None).await.0, S::UNAUTHORIZED);
        assert_eq!(f.jwks_hits.load(Ordering::SeqCst), 2);
        assert_eq!(f.sign_in(&unknown, "ios", None).await.0, S::UNAUTHORIZED);
        assert_eq!(f.jwks_hits.load(Ordering::SeqCst), 2);

        // None of that used up the nonce: the genuine token still works.
        assert_eq!(f.sign_in(&mint(&good), "ios", None).await.0, S::OK);
        // Several audiences are fine when the authorized party is ours.
        let mut multi = claims(&f.nonce().await, OWNER);
        multi["aud"] = json!(["other", CLIENT]);
        assert_eq!(f.sign_in(&mint(&multi), "ios", None).await.0, S::OK);
    }

    #[tokio::test]
    async fn link_and_unlink_with_an_admin_session() {
        let f = fixture().await;
        let r = f
            .http
            .post(f.url("/v1/session"))
            .header("x-grouper", "1")
            .header("origin", &f.base)
            .json(&json!({"token": f.admin_token}))
            .send()
            .await
            .unwrap();
        let cookie = cookie_pair(r.headers());
        let link = |method: Method, token: Option<String>| {
            let mut rb = f
                .http
                .request(method, f.url("/v1/auth/google/link"))
                .header("cookie", &cookie)
                .header("x-grouper", "1")
                .header("origin", &f.base);
            if let Some(t) = token {
                rb = rb.json(&json!({"id_token": t}));
            }
            rb.send()
        };
        let (_, _, body) = parts(link(Method::GET, None).await.unwrap()).await;
        assert_eq!(body["owner"], Value::Null);

        let (status, _, body) = parts(link(Method::POST, Some(mint(&claims(&f.nonce().await, OWNER)))).await.unwrap()).await;
        assert_eq!(status, S::OK, "{body}");
        assert_eq!(body["owner"]["email"], "100000000000000000001@example.com");
        assert!(body["owner"].get("sub").is_none());
        assert_eq!(f.status().await["claimed"], true);
        // The same account again is fine; another is a conflict.
        let (status, _, _) = parts(link(Method::POST, Some(mint(&claims(&f.nonce().await, OWNER)))).await.unwrap()).await;
        assert_eq!(status, S::OK);
        let (status, _, _) = parts(link(Method::POST, Some(mint(&claims(&f.nonce().await, STRANGER)))).await.unwrap()).await;
        assert_eq!(status, S::CONFLICT);
        assert_eq!(f.hub.db.owner().unwrap().sub, OWNER);
        // Linking checks the nonce too.
        let (status, _, _) = parts(link(Method::POST, Some(mint(&claims(&random_hex(16), OWNER)))).await.unwrap()).await;
        assert_eq!(status, S::UNAUTHORIZED);

        // Admins only.
        let r = f.http.delete(f.url("/v1/auth/google/link")).bearer_auth(&f.device_token).send().await.unwrap();
        assert_eq!(r.status(), S::FORBIDDEN);
        let r = f.http.delete(f.url("/v1/auth/google/link")).send().await.unwrap();
        assert_eq!(r.status(), S::UNAUTHORIZED);
        // The linked owner can now sign in from an app without a setup code.
        assert_eq!(f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", None).await.0, S::OK);

        let (status, _, body) = parts(link(Method::DELETE, None).await.unwrap()).await;
        assert_eq!(status, S::OK);
        assert_eq!(body["unlinked"], true);
        assert_eq!(f.status().await["claimed"], false);
        let r = f.http.get(f.url("/v1/limits")).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK, "sessions and tokens keep working after unlinking");
    }

    #[tokio::test]
    async fn a_hub_without_client_ids_has_google_sign_in_off() {
        let f = fixture().await;
        f.hub.google.configure(|c| {
            c.client_ids.clear();
            c.web_client_id.clear();
        });
        assert_eq!(f.status().await, json!({"claimed": false, "google": false, "google_web": false, "local": false}));
        assert_eq!(f.http.post(f.url("/v1/auth/google/nonce")).send().await.unwrap().status(), S::NOT_FOUND);
        assert_eq!(f.http.post(f.url("/v1/auth/google/start")).send().await.unwrap().status(), S::NOT_FOUND);
        let (status, _, _) = f.sign_in(&mint(&claims("x", OWNER)), "ios", None).await;
        assert_eq!(status, S::NOT_FOUND);
        // App clients alone: on, but not in browsers.
        f.hub.google.configure(|c| c.client_ids = vec![CLIENT.into()]);
        assert_eq!(f.status().await, json!({"claimed": false, "google": true, "google_web": false, "local": false}));
    }

    #[tokio::test]
    async fn browser_sign_in_through_the_return_page_claims_and_starts_a_session() {
        let f = fixture().await;
        let (code, _) = f.hub.db.new_setup_code().unwrap();
        let start = f.start(Some(&code)).await;
        assert_eq!(start["client_id"], WEB_CLIENT);
        assert_eq!(start["redirect_uri"], RETURN_URL);
        let nonce = start["nonce"].as_str().unwrap();
        let state = start["state"].as_str().unwrap();
        assert_eq!(decode_state(state).unwrap().0, f.base, "the state names this hub in clear");
        let decoded = String::from_utf8(URL_SAFE_NO_PAD.decode(state).unwrap()).unwrap();
        assert!(!decoded.contains(&code), "the setup code stays on the hub: {decoded}");
        let auth_url = url::Url::parse(start["auth_url"].as_str().unwrap()).unwrap();
        assert_eq!(auth_url.origin().ascii_serialization(), "https://accounts.google.com");
        let q: HashMap<String, String> = auth_url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], WEB_CLIENT);
        assert_eq!(q["redirect_uri"], RETURN_URL);
        assert_eq!(q["response_type"], "id_token");
        assert_eq!(q["response_mode"], "form_post");
        assert_eq!(q["scope"], "openid email profile");
        assert_eq!(q["prompt"], "select_account");
        assert_eq!((q["nonce"].as_str(), q["state"].as_str()), (nonce, state));
        let token = mint(&web_claims(nonce, OWNER));

        // Google form_posts to the return page (here on this same hub, with a stale cookie and cross-site
        // headers to show the CSRF guard lets it through); it forwards to the hub named in `state`.
        let r = f
            .http
            .post(f.url(RETURN_PAGE_PATH))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("cookie", format!("grouper_session={}", "0".repeat(64)))
            .header("origin", "https://accounts.google.com")
            .header("sec-fetch-site", "cross-site")
            .body(form_body(&[("id_token", &token), ("state", state)]))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        let csp = r.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
        assert!(csp.contains(&format!("form-action {}", f.base)) && csp.contains("script-src 'sha256-"), "{csp}");
        assert!(csp.contains("default-src 'none'"), "{csp}");
        let html = r.text().await.unwrap();
        assert!(html.contains(&format!("action=\"{}/v1/auth/google/return\"", f.base)), "{html}");
        assert!(html.contains(&format!("name=\"id_token\" value=\"{token}\"")));
        assert!(html.contains(&format!("<script>{SUBMIT_SCRIPT}</script>")));

        // The browser posts it on, cross-site: the hub checks state and nonce, claims, sets the cookie.
        let (status, headers) = f.finish(&[("id_token", &token), ("state", state)]).await;
        assert_eq!(status, S::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/#/");
        let cookie = cookie_pair(&headers);
        let r = f.http.get(f.url("/v1/limits")).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(r.status(), S::OK, "an admin session");
        assert_eq!(f.hub.db.owner().unwrap().sub, OWNER, "the setup code given to /start claimed the hub");

        // The state is one-time.
        let (status, headers) = f.finish(&[("id_token", &token), ("state", state)]).await;
        assert_eq!(status, S::SEE_OTHER);
        assert!(headers[header::LOCATION].to_str().unwrap().starts_with("/#/claim?error="));
        assert!(headers.get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn browser_state_must_be_ours_fresh_and_for_this_hub() {
        let f = fixture().await;
        f.claim().await;
        let web_token = |nonce: &str| mint(&web_claims(nonce, OWNER));
        let refused = |(status, headers): (S, HeaderMap)| {
            assert_eq!(status, S::SEE_OTHER);
            assert!(headers.get(header::SET_COOKIE).is_none());
            let location = headers[header::LOCATION].to_str().unwrap().to_string();
            assert!(location.starts_with("/#/claim?error="), "{location}");
            location
        };

        // Expired.
        let s = f.start(None).await;
        f.hub.google.age_pending(NONCE_TTL_SECS);
        let (state, nonce) = (s["state"].as_str().unwrap(), s["nonce"].as_str().unwrap());
        let location = refused(f.finish(&[("id_token", &web_token(nonce)), ("state", state)]).await);
        assert!(location.contains("too+long"), "{location}");

        // Another hub's origin with this hub's id.
        let s = f.start(None).await;
        let (state, nonce) = (s["state"].as_str().unwrap(), s["nonce"].as_str().unwrap());
        let id = decode_state(state).unwrap().1;
        let forged = URL_SAFE_NO_PAD.encode(json!({"o": "https://evil.example", "id": id}).to_string());
        refused(f.finish(&[("id_token", &web_token(nonce)), ("state", &forged)]).await);
        // That used the state up.
        refused(f.finish(&[("id_token", &web_token(nonce)), ("state", state)]).await);

        // An unknown id, and no state at all.
        let unknown = URL_SAFE_NO_PAD.encode(json!({"o": f.base, "id": random_hex(16)}).to_string());
        refused(f.finish(&[("id_token", &web_token(nonce)), ("state", &unknown)]).await);
        refused(f.finish(&[("id_token", &web_token(nonce))]).await);

        // A token whose nonce belongs to another sign-in.
        let (s1, s2) = (f.start(None).await, f.start(None).await);
        let token = web_token(s2["nonce"].as_str().unwrap());
        refused(f.finish(&[("id_token", &token), ("state", s1["state"].as_str().unwrap())]).await);

        // Google said no.
        let s = f.start(None).await;
        let location = refused(f.finish(&[("error", "access_denied"), ("state", s["state"].as_str().unwrap())]).await);
        assert!(location.contains("access_denied"), "{location}");

        // A stranger's Google account, with a good state and nonce.
        let s = f.start(None).await;
        let stranger = mint(&web_claims(s["nonce"].as_str().unwrap(), STRANGER));
        let location = refused(f.finish(&[("id_token", &stranger), ("state", s["state"].as_str().unwrap())]).await);
        assert!(location.contains("different+Google+account"), "{location}");

        // The happy path still works after all that.
        let s = f.start(None).await;
        let token = web_token(s["nonce"].as_str().unwrap());
        let (status, headers) = f.finish(&[("id_token", &token), ("state", s["state"].as_str().unwrap())]).await;
        assert_eq!((status, headers[header::LOCATION].to_str().unwrap()), (S::SEE_OTHER, "/#/"));
    }

    #[tokio::test]
    async fn the_return_page_only_forwards_to_https_or_loopback_hubs() {
        let f = fixture().await;
        let page = |query: String| f.http.get(f.url(&format!("{RETURN_PAGE_PATH}?{query}"))).send();
        let state = |o: &str| URL_SAFE_NO_PAD.encode(json!({"o": o, "id": "abc"}).to_string());
        for bad in ["http://hub.lan:8080", "https://hub.example.com/path", "javascript:alert(1)", "not a url"] {
            let r = page(form_body(&[("state", &state(bad))])).await.unwrap();
            assert_eq!(r.status(), S::BAD_REQUEST, "{bad}");
            assert!(r.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("form-action 'none'"));
        }
        assert_eq!(page(String::new()).await.unwrap().status(), S::BAD_REQUEST);
        // Errors come back by GET too; values are escaped.
        let r = page(form_body(&[("state", &state("https://hub.example.com")), ("error", "\"><script>x</script>")]))
            .await
            .unwrap();
        assert_eq!(r.status(), S::OK);
        let html = r.text().await.unwrap();
        assert!(html.contains("action=\"https://hub.example.com/v1/auth/google/return\""));
        assert!(html.contains("value=\"&quot;&gt;&lt;script&gt;x&lt;/script&gt;\""), "{html}");
        assert!(!html.contains("<script>x"));
    }

    #[tokio::test]
    async fn hub_info_reports_account_hub_device_id_and_epoch_and_omits_missing_key() {
        let f = fixture().await;
        let before: Value = f.http.get(f.url("/v1/hub")).send().await.unwrap().json().await.unwrap();
        assert_eq!(before["account"], Value::Null);
        let hub_dev = before["hub_device_id"].as_str().expect("hub_device_id").to_string();
        assert!(!hub_dev.is_empty());
        assert_eq!(before["epoch"], 1);
        assert!(before.get("key_sha256").is_none(), "no identity key yet, so key_sha256 is omitted: {before}");
        assert!(f.hub.db.devices().iter().any(|d| d.id == hub_dev && d.account_sub.is_none()));

        f.claim().await;
        let after: Value = f.http.get(f.url("/v1/hub")).send().await.unwrap().json().await.unwrap();
        assert_eq!(after["account"], format!("{OWNER}@example.com"));
        assert_eq!(after["hub_device_id"], hub_dev);
        assert_eq!(after["epoch"], 1);
        assert!(after.get("key_sha256").is_none());
        assert!(f.hub.db.devices().iter().all(|d| d.account_sub.as_deref() == Some(OWNER)));
    }

    #[tokio::test]
    async fn claim_local_claims_without_setup_code_or_nonce_and_links_devices() {
        let f = fixture().await;
        let nonce_less = |sub: &str| {
            let t = now();
            mint(&json!({
                "iss": ISS, "aud": CLIENT, "azp": CLIENT, "sub": sub, "email": format!("{sub}@example.com"),
                "email_verified": true, "name": "Local Owner", "iat": t, "exp": t + 3600,
            }))
        };

        // Server mode refuses `claim_local`.
        assert!(f.hub.claim_local(&nonce_less(OWNER)).await.is_err());
        assert!(f.hub.db.owner().is_none());

        f.hub.set_local_mode(true);
        assert_eq!(f.status().await, json!({"claimed": false, "google": true, "google_web": true, "local": true}));

        // Invalid tokens are still rejected.
        let bad_sig = mint_with(
            &keys().1,
            &json!({"alg": "RS256", "kid": KID}),
            &json!({"iss": ISS, "aud": CLIENT, "sub": OWNER, "email": "x@example.com", "email_verified": true, "iat": now(), "exp": now() + 600}),
        );
        assert!(f.hub.claim_local(&bad_sig).await.is_err());
        assert!(f.hub.db.owner().is_none());

        // First Google sign-in claims with no setup code and no hub nonce, and links devices to the account.
        let (owner, issued) = f
            .hub
            .claim_local_client(&nonce_less(OWNER), "windows", Some("Oscar's PC"), Some("win-install-0123456789abcdef"))
            .await
            .unwrap();
        assert_eq!(owner.sub, OWNER);
        assert_eq!(owner.email, format!("{OWNER}@example.com"));
        assert!(issued.device.admin);
        assert_eq!(issued.device.platform, "windows");
        assert_eq!(issued.device.name, "Oscar's PC");
        assert!(f.hub.db.authenticate(&issued.token).unwrap().admin);
        assert!(f.hub.db.devices().iter().all(|d| d.account_sub.as_deref() == Some(OWNER)));

        // Same owner again succeeds idempotently; another Google account is refused.
        let again = f.hub.claim_local(&nonce_less(OWNER)).await.unwrap();
        assert_eq!(again.sub, OWNER);
        let err = f.hub.claim_local(&nonce_less(STRANGER)).await.unwrap_err().to_string();
        assert!(err.contains("different Google account"), "{err}");
        assert_eq!(f.hub.db.owner().unwrap().sub, OWNER);
    }

    #[tokio::test]
    async fn local_mode_refuses_setup_code_claim_over_http() {
        let f = fixture().await;
        f.hub.set_local_mode(true);

        // Local mode never announces a setup code at startup.
        announce_setup_code(&f.hub);
        let announced: i64 = f.hub.db.c().query_row("SELECT COUNT(*) FROM setup_codes", [], |r| r.get(0)).unwrap();
        assert_eq!(announced, 0);

        // Even if a setup code row exists in SQLite, HTTP claiming (with or without it) is refused.
        let (code, _) = f.hub.db.new_setup_code().unwrap();
        let (status, _, body) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", Some(&code)).await;
        assert_eq!(status, S::FORBIDDEN, "{body}");
        let (status, _, body) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", None).await;
        assert_eq!(status, S::FORBIDDEN, "{body}");
        assert!(f.hub.db.owner().is_none());

        // Browser /start is also refused while unclaimed or when given a setup code.
        for payload in [String::new(), json!({"setup_code": code}).to_string()] {
            let r = f
                .http
                .post(f.url("/v1/auth/google/start"))
                .header("origin", &f.base)
                .body(payload)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), S::FORBIDDEN);
        }
        assert!(f.hub.db.owner().is_none());

        // Once claimed in-process via claim_local, the owner can sign in over HTTP without a setup code,
        // while setup_code over HTTP and strangers remain refused.
        f.hub.claim_local(&mint(&claims("unused", OWNER))).await.unwrap();
        let (status, _, _) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", Some(&code)).await;
        assert_eq!(status, S::FORBIDDEN);
        let (status, _, body) = f.sign_in(&mint(&claims(&f.nonce().await, OWNER)), "ios", None).await;
        assert_eq!(status, S::OK, "{body}");
        let (status, _, _) = f.sign_in(&mint(&claims(&f.nonce().await, STRANGER)), "ios", None).await;
        assert_eq!(status, S::FORBIDDEN);
    }
}
