//! Web Push notifications for approvals (RFC 8030 delivery, RFC 8291 message encryption,
//! RFC 8292 VAPID).
//!
//! The web client subscribes its service worker with the hub's VAPID public key and posts the
//! `PushSubscription` here. Each new approval request is pushed to every active subscription;
//! stale notifications are closed by the open page and by the service worker on the next push
//! (no silent `approval_cleared` pushes, which violate `userVisibleOnly` and cause WebKit to
//! revoke subscriptions). Payloads never carry tool input.
//!
//! Delivery fans out to one task per subscription, fed in emit order, so a slow or hanging
//! push endpoint only delays itself. Nothing here can block or fail the approval path.
//!
//! Endpoints (an admin token or the web client's admin session, as for answering approvals):
//! - `GET /v1/push/vapid-key` → `{"public_key"}` (base64url, uncompressed P-256 point)
//! - `POST /v1/push/subscriptions` with `PushSubscription.toJSON()` plus optional `label`
//! - `GET /v1/push/subscriptions`, `DELETE /v1/push/subscriptions/{id}`,
//!   `DELETE /v1/push/subscriptions?endpoint=…`
//! - `POST /v1/push/test` (optional `{"id"}`) → per-subscription results
//!
//! The VAPID private key lives in `hub_meta`, sealed with the master key (see [`Db::vapid_private_key`]).
//! The JWT `sub` is `GROUPER_PUBLIC_URL` (`https://`, `mailto:`, or loopback `http://`) when set,
//! else the origin the subscription was made from (`https://` or loopback `http://`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::agreement::{self, ECDH_P256, UnparsedPublicKey};
use aws_lc_rs::encoding::AsBigEndian;
use aws_lc_rs::hmac;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use core_protocol::{Event, EventBody};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::db::{Db, now, random_hex};
use crate::registry::deny_unless_admin;
use crate::server::{HUB_DEVICE, Hub, err};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS push_subscriptions (
  id TEXT PRIMARY KEY, endpoint TEXT NOT NULL UNIQUE, p256dh TEXT NOT NULL, auth TEXT NOT NULL,
  label TEXT, origin TEXT, expiration_time INTEGER, device_id TEXT, session_id TEXT,
  created INTEGER NOT NULL, last_ok INTEGER, last_error TEXT);
CREATE INDEX IF NOT EXISTS push_subscriptions_device ON push_subscriptions (device_id);
";

/// At most this many subscriptions; subscribing another browser past it is refused.
pub const MAX_SUBSCRIPTIONS: usize = 50;
/// Record size advertised in the aes128gcm header. Payloads are one record, far smaller.
const RECORD_SIZE: u32 = 4096;
/// A request push lives this long at the push service when no approval timeout is set.
const DEFAULT_TTL: u64 = 86_400;
/// Idle per-subscription delivery tasks exit after this long without new pushes.
const SUB_WORKER_IDLE: Duration = Duration::from_secs(60);
/// VAPID JWTs are valid this long (RFC 8292 allows at most 24 hours).
const JWT_LIFETIME: u64 = 12 * 3600;
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_ENDPOINT_LEN: usize = 2048;
const MAX_LABEL_CHARS: usize = 100;
const MAX_TITLE_CHARS: usize = 100;
/// Optional public contact for the hub (`https://…` or `mailto:…`), used as the JWT `sub`.
const ENV_PUBLIC_URL: &str = "GROUPER_PUBLIC_URL";

// ---------------------------------------------------------------- crypto

/// The hub's VAPID signing key.
pub struct Vapid {
    key_pair: EcdsaKeyPair,
}

impl Vapid {
    /// From a 32-byte big-endian P-256 private scalar.
    pub fn from_private_key(private: &[u8]) -> Result<Self> {
        let public = agreement::PrivateKey::from_private_key(&ECDH_P256, private)
            .map_err(|_| anyhow!("invalid VAPID private key"))?
            .compute_public_key()
            .map_err(|_| anyhow!("invalid VAPID private key"))?;
        let key_pair =
            EcdsaKeyPair::from_private_key_and_public_key(&ECDSA_P256_SHA256_FIXED_SIGNING, private, public.as_ref())
                .map_err(|_| anyhow!("invalid VAPID private key"))?;
        Ok(Self { key_pair })
    }

    /// A fresh private scalar.
    pub fn generate_private_key() -> Result<Vec<u8>> {
        let key_pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING)
            .map_err(|_| anyhow!("generating a VAPID key failed"))?;
        let private = key_pair.private_key().as_be_bytes().map_err(|_| anyhow!("exporting the VAPID key failed"))?;
        Ok(private.as_ref().to_vec())
    }

    /// Uncompressed public point (65 bytes, `0x04 || x || y`).
    pub fn public_key(&self) -> &[u8] {
        self.key_pair.public_key().as_ref()
    }

    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public_key())
    }

    /// An ES256 JWT for one push service (`aud` = its scheme and host), valid `JWT_LIFETIME`.
    pub fn jwt(&self, aud: &str, sub: Option<&str>, now: u64) -> Result<String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let mut claims = json!({"aud": aud, "exp": now + JWT_LIFETIME});
        if let Some(sub) = sub {
            claims["sub"] = json!(sub);
        }
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
        let input = format!("{header}.{claims}");
        let signature = self
            .key_pair
            .sign(&aws_lc_rs::rand::SystemRandom::new(), input.as_bytes())
            .map_err(|_| anyhow!("signing the VAPID JWT failed"))?;
        Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref())))
    }

    /// The `Authorization` header value for an endpoint.
    pub fn authorization(&self, endpoint: &str, sub: Option<&str>, now: u64) -> Result<String> {
        let aud = audience(endpoint)?;
        Ok(format!("vapid t={}, k={}", self.jwt(&aud, sub, now)?, self.public_key_b64()))
    }
}

/// Scheme, host and (non-default) port of a push endpoint.
pub fn audience(endpoint: &str) -> Result<String> {
    let url = reqwest::Url::parse(endpoint).context("push endpoint is not a URL")?;
    match url.origin() {
        origin @ url::Origin::Tuple(..) => Ok(origin.ascii_serialization()),
        url::Origin::Opaque(_) => bail!("push endpoint has no origin"),
    }
}

fn hmac_sha256(key: &[u8], data: &[&[u8]]) -> [u8; 32] {
    let mut ctx = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    for part in data {
        ctx.update(part);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.sign().as_ref());
    out
}

/// Content-encryption key and nonce for one aes128gcm message (RFC 8291 section 3.4, RFC 8188 section 2.2).
fn derive_keys(
    ecdh_secret: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> ([u8; 16], [u8; 12]) {
    // IKM = HKDF(auth_secret, ecdh_secret, "WebPush: info" || 0x00 || ua_public || as_public, 32)
    let prk_key = hmac_sha256(auth_secret, &[ecdh_secret]);
    let ikm = hmac_sha256(&prk_key, &[b"WebPush: info\0", ua_public, as_public, &[1]]);
    let prk = hmac_sha256(salt, &[&ikm]);
    let cek_full = hmac_sha256(&prk, &[b"Content-Encoding: aes128gcm\0", &[1]]);
    let nonce_full = hmac_sha256(&prk, &[b"Content-Encoding: nonce\0", &[1]]);
    let mut cek = [0u8; 16];
    cek.copy_from_slice(&cek_full[..16]);
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&nonce_full[..12]);
    (cek, nonce)
}

/// Encrypt one push message for a subscriber with a given application-server key and salt.
/// Deterministic, for tests; [`encrypt`] picks both at random.
pub fn encrypt_with(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    as_private: &agreement::PrivateKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>> {
    if auth_secret.len() != 16 {
        bail!("auth secret must be 16 bytes");
    }
    let as_public = as_private.compute_public_key().map_err(|_| anyhow!("bad ephemeral key"))?;
    let as_public = as_public.as_ref();
    let ecdh_secret = agreement::agree(as_private, UnparsedPublicKey::new(&ECDH_P256, ua_public), (), |secret| {
        Ok(secret.to_vec())
    })
    .map_err(|_| anyhow!("subscriber key p256dh is not a valid P-256 point"))?;
    let (cek, nonce) = derive_keys(&ecdh_secret, auth_secret, ua_public, as_public, salt);

    // One record: the plaintext, then the last-record delimiter 0x02 (no further padding).
    let mut record = Vec::with_capacity(plaintext.len() + 1 + 16);
    record.extend_from_slice(plaintext);
    record.push(2);
    // Push services take at most 4096 bytes of body: the 86-byte header, the record and its tag.
    if 86 + record.len() + 16 > RECORD_SIZE as usize {
        bail!("push payload too large");
    }
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).map_err(|_| anyhow!("bad content key"))?);
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut record)
        .map_err(|_| anyhow!("encrypting the push payload failed"))?;

    // Header: salt (16) || rs (4, big endian) || idlen (1) || keyid (the server's public key).
    let mut out = Vec::with_capacity(16 + 4 + 1 + as_public.len() + record.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&record);
    Ok(out)
}

/// Encrypt one push message with a fresh ephemeral key and salt.
pub fn encrypt(plaintext: &[u8], ua_public: &[u8], auth_secret: &[u8]) -> Result<Vec<u8>> {
    let as_private = agreement::PrivateKey::generate(&ECDH_P256).map_err(|_| anyhow!("generating a key failed"))?;
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).context("generating a push salt")?;
    encrypt_with(plaintext, ua_public, auth_secret, &as_private, &salt)
}

/// Base64url, with or without padding (browsers send it without).
fn b64(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    URL_SAFE_NO_PAD.decode(value.trim_end_matches('=')).ok().or_else(|| URL_SAFE.decode(value).ok())
}

/// `Topic` for one approval: base64url, 32 characters, the same for its request and its clear.
pub fn topic(conversation_id: &str, approval_id: &str) -> String {
    let digest = Sha256::digest(format!("{conversation_id}\n{approval_id}").as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..24])
}

// ---------------------------------------------------------------- storage

#[derive(Clone, Debug, Serialize)]
pub struct SubscriptionInfo {
    pub id: String,
    pub endpoint: String,
    pub label: Option<String>,
    /// Milliseconds since the epoch, as the browser reported it.
    pub expiration_time: Option<i64>,
    pub created: u64,
    pub last_ok: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
struct Subscription {
    info: SubscriptionInfo,
    p256dh: Vec<u8>,
    auth: Vec<u8>,
    origin: Option<String>,
}

const SUB_COLS: &str = "id, endpoint, label, expiration_time, created, last_ok, last_error, p256dh, auth, origin";

fn sub_row(r: &rusqlite::Row) -> rusqlite::Result<Subscription> {
    let p256dh: String = r.get(7)?;
    let auth: String = r.get(8)?;
    Ok(Subscription {
        info: SubscriptionInfo {
            id: r.get(0)?,
            endpoint: r.get(1)?,
            label: r.get(2)?,
            expiration_time: r.get(3)?,
            created: r.get::<_, i64>(4)? as u64,
            last_ok: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            last_error: r.get(6)?,
        },
        p256dh: b64(&p256dh).unwrap_or_default(),
        auth: b64(&auth).unwrap_or_default(),
        origin: r.get(9)?,
    })
}

/// What a browser posts: `PushSubscription.toJSON()` plus an optional label.
#[derive(Deserialize)]
pub struct NewSubscription {
    pub endpoint: String,
    #[serde(default, rename = "expirationTime")]
    pub expiration_time: Option<f64>,
    pub keys: SubscriptionKeys,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Deserialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

fn prune_invalid_push_subscriptions(c: &rusqlite::Connection) {
    let current = now() as i64;
    let _ = c.execute(
        "DELETE FROM push_subscriptions
         WHERE (session_id IS NOT NULL AND session_id NOT IN (SELECT id_hash FROM sessions WHERE expires > ?1))
            OR (device_id IS NOT NULL AND device_id NOT IN (SELECT id FROM devices WHERE revoked = 0 AND admin != 0))",
        [current],
    );
}

impl Db {
    fn push_subscriptions(&self) -> Vec<Subscription> {
        let c = self.c();
        prune_invalid_push_subscriptions(&c);
        let Ok(mut st) = c.prepare(&format!("SELECT {SUB_COLS} FROM push_subscriptions ORDER BY created, id")) else {
            return vec![];
        };
        st.query_map([], sub_row).map(|rows| rows.filter_map(Result::ok).collect()).unwrap_or_default()
    }

    pub fn push_subscription_list(&self) -> Vec<SubscriptionInfo> {
        self.push_subscriptions().into_iter().map(|s| s.info).collect()
    }

    pub fn push_subscription_count(&self) -> usize {
        let c = self.c();
        prune_invalid_push_subscriptions(&c);
        c.query_row("SELECT COUNT(*) FROM push_subscriptions", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    fn push_subscription(&self, id: &str) -> Option<Subscription> {
        let c = self.c();
        prune_invalid_push_subscriptions(&c);
        c.query_row(&format!("SELECT {SUB_COLS} FROM push_subscriptions WHERE id = ?1"), [id], sub_row)
            .optional()
            .ok()
            .flatten()
    }

    /// Insert or update by endpoint. Errors are for the caller to show. An origin from the
    /// request's `Origin` header replaces the stored one; one guessed from `Host` only fills a gap.
    fn upsert_push_subscription(
        &self,
        new: &NewSubscription,
        label: Option<&str>,
        origin: &RequestOrigin,
        device_id: &str,
        session_id: Option<&str>,
    ) -> Result<SubscriptionInfo, String> {
        let (origin, from_header) = (origin.origin.as_deref(), origin.from_header);
        let c = self.c();
        prune_invalid_push_subscriptions(&c);
        let existing: Option<String> = c
            .query_row("SELECT id FROM push_subscriptions WHERE endpoint = ?1", [&new.endpoint], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        let expiration = new.expiration_time.filter(|t| t.is_finite() && *t > 0.0).map(|t| t as i64);
        let id = match existing {
            Some(id) => {
                // The browser subscribed again (perhaps with new keys or a new session): keep the id, take the rest.
                c.execute(
                    "UPDATE push_subscriptions SET p256dh = ?2, auth = ?3, label = COALESCE(?4, label), \
                     origin = CASE WHEN ?8 THEN COALESCE(?5, origin) ELSE COALESCE(origin, ?5) END, \
                     expiration_time = ?6, device_id = ?7, session_id = ?9 WHERE id = ?1",
                    params![
                        id,
                        new.keys.p256dh.trim(),
                        new.keys.auth.trim(),
                        label,
                        origin,
                        expiration,
                        device_id,
                        from_header,
                        session_id
                    ],
                )
                .map_err(|e| e.to_string())?;
                id
            }
            None => {
                let count: i64 =
                    c.query_row("SELECT COUNT(*) FROM push_subscriptions", [], |r| r.get(0)).map_err(|e| e.to_string())?;
                if count as usize >= MAX_SUBSCRIPTIONS {
                    return Err(format!("there are already {MAX_SUBSCRIPTIONS} push subscriptions; remove one first"));
                }
                let id = random_hex(8);
                c.execute(
                    "INSERT INTO push_subscriptions (id, endpoint, p256dh, auth, label, origin, expiration_time, \
                     device_id, session_id, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        id,
                        new.endpoint,
                        new.keys.p256dh.trim(),
                        new.keys.auth.trim(),
                        label,
                        origin,
                        expiration,
                        device_id,
                        session_id,
                        now() as i64
                    ],
                )
                .map_err(|e| e.to_string())?;
                id
            }
        };
        drop(c);
        self.push_subscription(&id).map(|s| s.info).ok_or_else(|| "subscription vanished".to_string())
    }

    pub fn delete_push_subscription(&self, id: &str) -> bool {
        self.c().execute("DELETE FROM push_subscriptions WHERE id = ?1", [id]).unwrap_or(0) > 0
    }

    fn delete_push_subscription_by_endpoint(&self, endpoint: &str) -> Option<String> {
        let c = self.c();
        let id: Option<String> = c
            .query_row("SELECT id FROM push_subscriptions WHERE endpoint = ?1", [endpoint], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        if let Some(id) = &id {
            let _ = c.execute("DELETE FROM push_subscriptions WHERE id = ?1", [id]);
        }
        id
    }

    fn record_push_result(&self, id: &str, error: Option<&str>) {
        let c = self.c();
        let _ = match error {
            None => c.execute(
                "UPDATE push_subscriptions SET last_ok = ?2, last_error = NULL WHERE id = ?1",
                params![id, now() as i64],
            ),
            Some(error) => {
                c.execute("UPDATE push_subscriptions SET last_error = ?2 WHERE id = ?1", params![id, error])
            }
        };
    }
}

// ---------------------------------------------------------------- sending

struct SubJob {
    vapid: Arc<Vapid>,
    sub: Subscription,
    message: Arc<Message>,
}

/// Per-hub push state: the ordered dispatcher queue and its settings.
pub struct PushState {
    hub: Weak<Hub>,
    queue: OnceLock<mpsc::UnboundedSender<Event>>,
    vapid: Mutex<Option<Arc<Vapid>>>,
    client: reqwest::Client,
    allow_local_http: AtomicBool,
    sub_count: AtomicUsize,
    warned_no_sub: AtomicBool,
}

impl PushState {
    pub(crate) fn new(hub: Weak<Hub>, initial_subs: usize) -> Self {
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            hub,
            queue: OnceLock::new(),
            vapid: Mutex::new(None),
            client,
            allow_local_http: AtomicBool::new(false),
            sub_count: AtomicUsize::new(initial_subs),
            warned_no_sub: AtomicBool::new(false),
        }
    }

    fn enqueue(&self, event: Event) {
        let tx = self.queue.get_or_init(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(dispatcher(self.hub.clone(), rx));
                }
                Err(_) => tracing::warn!("web push needs a Tokio runtime; notifications are off"),
            }
            tx
        });
        let _ = tx.send(event);
    }
}

impl Hub {
    /// Tests only: accept `http://` push endpoints on loopback hosts. Production requires https.
    pub fn set_push_allow_local_http(&self, allow: bool) {
        self.push.allow_local_http.store(allow, Ordering::Relaxed);
    }

    pub(crate) fn sync_push_sub_count(&self) {
        self.push.sub_count.store(self.db.push_subscription_count(), Ordering::Release);
    }

    /// The VAPID key, loaded (or made) once.
    pub fn vapid(&self) -> Result<Arc<Vapid>> {
        let mut cached = self.push.vapid.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = cached.as_ref() {
            return Ok(Arc::clone(v));
        }
        let private = self.db.vapid_private_key(Vapid::generate_private_key)?;
        let vapid = Arc::new(Vapid::from_private_key(&private)?);
        *cached = Some(Arc::clone(&vapid));
        Ok(vapid)
    }

    /// Called inside `emit_order` for every stored event; queues pushes for approval requests
    /// without DB I/O under the lock.
    pub(crate) fn push_on_event(&self, event: &Event) {
        if !matches!(event.body, EventBody::ApprovalRequested { .. }) {
            return;
        }
        if self.push.sub_count.load(Ordering::Acquire) > 0 {
            self.push.enqueue(event.clone());
        }
    }
}

/// One message to deliver to every subscription.
struct Message {
    kind: &'static str,
    payload: Vec<u8>,
    topic: Option<String>,
    ttl: u64,
    urgency: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Delivery {
    pub id: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub error: Option<String>,
    /// The push service said the subscription is gone (404/410), so the hub deleted it.
    pub removed: bool,
}

/// Fans out each event to per-subscription worker tasks, preserving emit order per
/// subscription without letting a slow endpoint block healthy ones.
async fn dispatcher(hub: Weak<Hub>, mut rx: mpsc::UnboundedReceiver<Event>) {
    let mut workers: HashMap<String, mpsc::UnboundedSender<SubJob>> = HashMap::new();
    while let Some(event) = rx.recv().await {
        let Some(hub) = hub.upgrade() else { break };
        let Some(message) = build_message(&hub, event).map(Arc::new) else { continue };
        let subs = hub.db.push_subscriptions();
        hub.push.sub_count.store(subs.len(), Ordering::Release);
        if subs.is_empty() {
            continue;
        }
        let vapid = match hub.vapid() {
            Ok(v) => v,
            Err(error) => {
                tracing::error!("web push: no VAPID key: {error:#}");
                continue;
            }
        };
        workers.retain(|_, tx| !tx.is_closed());
        for sub in subs {
            let id = sub.info.id.clone();
            let job = SubJob { vapid: Arc::clone(&vapid), sub, message: Arc::clone(&message) };
            let job = match workers.get(&id) {
                Some(tx) => match tx.send(job) {
                    Ok(()) => continue,
                    Err(mpsc::error::SendError(job)) => {
                        workers.remove(&id);
                        job
                    }
                },
                None => job,
            };
            let (tx, mut sub_rx) = mpsc::unbounded_channel::<SubJob>();
            let _ = tx.send(job);
            workers.insert(id, tx);
            let sub_hub = Arc::downgrade(&hub);
            tokio::spawn(async move {
                while let Ok(Some(job)) = tokio::time::timeout(SUB_WORKER_IDLE, sub_rx.recv()).await {
                    let Some(hub) = sub_hub.upgrade() else { break };
                    if deliver(&hub, &job.vapid, &job.sub, &job.message).await.removed {
                        break;
                    }
                }
            });
        }
    }
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_TITLE_CHARS).collect()
}

fn build_message(hub: &Hub, event: Event) -> Option<Message> {
    let EventBody::ApprovalRequested { approval_id, device_id, tool, .. } = &event.body else { return None };
    let device = if device_id == HUB_DEVICE {
        "Hub".to_string()
    } else {
        let name = hub.db.devices().into_iter().find(|d| &d.id == device_id).map(|d| d.name);
        name.unwrap_or_else(|| device_id.clone())
    };
    let thread_title = event.thread_id.as_deref().and_then(|t| hub.db.thread(t)).map(|t| clip(&t.title));
    let chat_title = hub.db.conversation(&event.conversation_id).map(|c| clip(&c.title));
    // Deliberately no tool input or reason: the push service and the OS see this.
    let payload = json!({
        "type": "approval_requested",
        "approval_id": approval_id,
        "conversation_id": event.conversation_id,
        "thread_id": event.thread_id,
        "tool": clip(tool),
        "device": clip(&device),
        "thread_title": thread_title,
        "chat_title": chat_title,
        "ts": event.ts,
    });
    Some(Message {
        kind: "approval_requested",
        payload: serde_json::to_vec(&payload).ok()?,
        topic: Some(topic(&event.conversation_id, approval_id)),
        ttl: hub.db.limits().approval_timeout_secs.unwrap_or(DEFAULT_TTL),
        urgency: "high",
    })
}

async fn deliver_all(hub: &Hub, message: &Message, only: Option<&str>) -> Vec<Delivery> {
    let subs: Vec<Subscription> =
        hub.db.push_subscriptions().into_iter().filter(|s| only.is_none_or(|id| s.info.id == id)).collect();
    if subs.is_empty() {
        return vec![];
    }
    let vapid = match hub.vapid() {
        Ok(v) => v,
        Err(error) => {
            tracing::error!("web push: no VAPID key: {error:#}");
            return subs
                .into_iter()
                .map(|s| Delivery {
                    id: s.info.id,
                    ok: false,
                    status: None,
                    error: Some(error.to_string()),
                    removed: false,
                })
                .collect();
        }
    };
    futures_util::future::join_all(subs.iter().map(|s| deliver(hub, &vapid, s, message))).await
}

enum Attempt {
    Status(u16),
    Failed(String),
}

async fn deliver(hub: &Hub, vapid: &Vapid, sub: &Subscription, message: &Message) -> Delivery {
    let id = sub.info.id.clone();
    let mut attempt = send_once(hub, vapid, sub, message).await;
    // One retry, for failures that may pass: rate limits, server errors, the network.
    let retry = match &attempt {
        Ok(Attempt::Status(status)) => *status == 429 || *status >= 500,
        Ok(Attempt::Failed(_)) => true,
        Err(_) => false,
    };
    if retry {
        tokio::time::sleep(RETRY_DELAY).await;
        attempt = send_once(hub, vapid, sub, message).await;
    }
    let (status, error, removed) = match attempt {
        Ok(Attempt::Status(status)) if (200..300).contains(&status) => (Some(status), None, false),
        Ok(Attempt::Status(status @ (404 | 410))) => {
            hub.db.delete_push_subscription(&id);
            hub.sync_push_sub_count();
            (Some(status), Some(format!("the push service says the subscription is gone ({status})")), true)
        }
        Ok(Attempt::Status(status)) => (Some(status), Some(format!("push service answered {status}")), false),
        Ok(Attempt::Failed(error)) => (None, Some(error), false),
        Err(error) => (None, Some(format!("{error:#}")), false),
    };
    let host =
        reqwest::Url::parse(&sub.info.endpoint).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
    match &error {
        None => tracing::info!(subscription = %id, %host, kind = message.kind, status, "web push sent"),
        Some(e) => {
            tracing::info!(subscription = %id, %host, kind = message.kind, status, removed, error = %e, "web push failed")
        }
    }
    if !removed {
        hub.db.record_push_result(&id, error.as_deref());
    }
    Delivery { id, ok: error.is_none(), status, error, removed }
}

/// `Err` for problems on our side (bad stored keys), which retrying won't fix.
async fn send_once(hub: &Hub, vapid: &Vapid, sub: &Subscription, message: &Message) -> Result<Attempt> {
    let body = encrypt(&message.payload, &sub.p256dh, &sub.auth)?;
    let public_url = std::env::var(ENV_PUBLIC_URL).ok().and_then(|u| contact_from_env(&u));
    let sub_claim = public_url.or_else(|| sub.origin.as_deref().and_then(contact_origin));
    if sub_claim.is_none() && !hub.push.warned_no_sub.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "web push: no VAPID contact (`sub`) available; set {ENV_PUBLIC_URL} to an https:// URL or mailto: \
             address (Apple and Mozilla push services reject JWTs without sub)"
        );
    }
    let authorization = vapid.authorization(&sub.info.endpoint, sub_claim.as_deref(), now())?;
    let mut request = hub
        .push
        .client
        .post(&sub.info.endpoint)
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_ENCODING, "aes128gcm")
        .header("TTL", message.ttl.to_string())
        .header("Urgency", message.urgency);
    if let Some(topic) = &message.topic {
        request = request.header("Topic", topic);
    }
    match request.body(body).send().await {
        Ok(response) => Ok(Attempt::Status(response.status().as_u16())),
        Err(error) => Ok(Attempt::Failed(format!("sending failed: {}", without_url(&error)))),
    }
}

/// reqwest errors include the URL; a push endpoint is a capability, so keep it out of logs.
fn without_url(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request error"
    };
    kind.to_string()
}

/// Contact from `GROUPER_PUBLIC_URL`: accepts `mailto:`, `https://`, or loopback `http://`.
fn contact_from_env(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let url = reqwest::Url::parse(trimmed).ok()?;
    if url.scheme() == "mailto" {
        let addr = url.path();
        let valid = url.query().is_none()
            && url.fragment().is_none()
            && !addr.chars().any(char::is_whitespace)
            && addr
                .split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'));
        return valid.then(|| format!("mailto:{addr}"));
    }
    contact_origin(trimmed)
}

/// Subscribe origin suitable for the JWT `sub` claim: `https://` or loopback `http://`.
fn contact_origin(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url.trim()).ok()?;
    let host = url.host()?;
    let ok = match url.scheme() {
        "https" => true,
        "http" => is_loopback(&host),
        _ => false,
    };
    ok.then(|| url.origin().ascii_serialization())
}

// ---------------------------------------------------------------- validation

fn is_loopback(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
    }
}

fn validate(new: &NewSubscription, allow_local_http: bool) -> Result<(), String> {
    if new.endpoint.len() > MAX_ENDPOINT_LEN {
        return Err("endpoint is too long".into());
    }
    let url = reqwest::Url::parse(&new.endpoint).map_err(|_| "endpoint is not a URL".to_string())?;
    let Some(host) = url.host() else { return Err("endpoint has no host".into()) };
    let ok = match url.scheme() {
        "https" => true,
        "http" => allow_local_http && is_loopback(&host),
        _ => false,
    };
    if !ok || !url.username().is_empty() || url.password().is_some() {
        return Err("endpoint must be an https:// URL".into());
    }
    let p256dh = b64(&new.keys.p256dh).ok_or("keys.p256dh is not base64url")?;
    if p256dh.len() != 65 || p256dh[0] != 4 {
        return Err("keys.p256dh must be an uncompressed P-256 point (65 bytes)".into());
    }
    let parsed: Result<agreement::ParsedPublicKey, _> = UnparsedPublicKey::new(&ECDH_P256, &p256dh[..]).try_into();
    if parsed.is_err() {
        return Err("keys.p256dh is not a valid P-256 point".into());
    }
    if b64(&new.keys.auth).is_none_or(|a| a.len() != 16) {
        return Err("keys.auth must be 16 bytes of base64url".into());
    }
    Ok(())
}

struct RequestOrigin {
    origin: Option<String>,
    from_header: bool,
}

/// The origin the web client subscribed from: its `Origin` header, else this request's `Host`.
fn request_origin(headers: &HeaderMap) -> RequestOrigin {
    let header_origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|origin| reqwest::Url::parse(origin).ok())
        .filter(|url| matches!(url.scheme(), "https" | "http") && url.host_str().is_some());
    if let Some(url) = header_origin {
        return RequestOrigin { origin: Some(url.origin().ascii_serialization()), from_header: true };
    }
    let origin = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|host| reqwest::Url::parse(&format!("https://{}", host.trim())).ok())
        .filter(|url| url.host_str().is_some())
        .map(|url| url.origin().ascii_serialization());
    RequestOrigin { origin, from_header: false }
}

// ---------------------------------------------------------------- HTTP

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/push/vapid-key", get(vapid_key))
        .route("/v1/push/subscriptions", get(list).post(subscribe).delete(unsubscribe_endpoint))
        .route("/v1/push/subscriptions/{id}", axum::routing::delete(unsubscribe))
        .route("/v1/push/test", post(test_push))
}

async fn vapid_key(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.vapid() {
        Ok(v) => Json(json!({"public_key": v.public_key_b64()})).into_response(),
        Err(error) => {
            tracing::error!("web push: no VAPID key: {error:#}");
            err(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
        }
    }
}

async fn list(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    Json(hub.db.push_subscription_list()).into_response()
}

async fn subscribe(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(body): Json<NewSubscription>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some(auth) = hub.auth(&headers) else { return err(StatusCode::UNAUTHORIZED, "invalid or missing device token") };
    if let Err(message) = validate(&body, hub.push.allow_local_http.load(Ordering::Relaxed)) {
        return err(StatusCode::BAD_REQUEST, message);
    }
    let label: Option<String> = body
        .label
        .as_deref()
        .map(|l| l.trim().chars().take(MAX_LABEL_CHARS).collect::<String>())
        .filter(|l| !l.is_empty());
    let origin = request_origin(&headers);
    let session_id = crate::session::current_session_hash(&hub, &headers);
    // Make sure the key exists before the browser relies on it.
    if let Err(error) = hub.vapid() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"));
    }
    match hub.db.upsert_push_subscription(&body, label.as_deref(), &origin, &auth.device_id, session_id.as_deref()) {
        Ok(info) => {
            hub.sync_push_sub_count();
            tracing::info!(subscription = %info.id, device = %auth.device_id, "web push subscription saved");
            Json(info).into_response()
        }
        Err(message) if message.contains("already") => err(StatusCode::CONFLICT, message),
        Err(message) => err(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}

async fn unsubscribe(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    if hub.db.delete_push_subscription(&id) {
        hub.sync_push_sub_count();
        Json(json!({"deleted": id})).into_response()
    } else {
        err(StatusCode::NOT_FOUND, "no push subscription with that id")
    }
}

#[derive(Deserialize)]
struct EndpointQuery {
    endpoint: Option<String>,
}

async fn unsubscribe_endpoint(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Query(q): Query<EndpointQuery>,
) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let Some(endpoint) = q.endpoint.filter(|e| !e.is_empty()) else {
        return err(
            StatusCode::BAD_REQUEST,
            "give ?endpoint=<the subscription's endpoint> or use /v1/push/subscriptions/{id}",
        );
    };
    match hub.db.delete_push_subscription_by_endpoint(&endpoint) {
        Some(id) => {
            hub.sync_push_sub_count();
            Json(json!({"deleted": id})).into_response()
        }
        None => err(StatusCode::NOT_FOUND, "no push subscription with that endpoint"),
    }
}

async fn test_push(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    let only = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v.get("id").and_then(Value::as_str).map(String::from),
            Err(_) => return err(StatusCode::BAD_REQUEST, "body must be JSON, e.g. {} or {\"id\": \"…\"}"),
        }
    };
    if only.as_deref().is_some_and(|id| hub.db.push_subscription(id).is_none()) {
        return err(StatusCode::NOT_FOUND, "no push subscription with that id");
    }
    let message = Message {
        kind: "test",
        payload: serde_json::to_vec(&json!({"type": "test", "ts": now()})).unwrap_or_default(),
        topic: None,
        ttl: 300,
        urgency: "normal",
    };
    let results = deliver_all(&hub, &message, only.as_deref()).await;
    Json(json!({ "results": results })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey as SigKey};

    fn d(s: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(s).unwrap()
    }

    /// RFC 8291 Appendix A.
    #[test]
    fn encryption_matches_the_rfc_8291_test_vector() {
        let plaintext = d("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        let as_private =
            agreement::PrivateKey::from_private_key(&ECDH_P256, &d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"))
                .unwrap();
        let as_public = d("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        assert_eq!(as_private.compute_public_key().unwrap().as_ref(), &as_public[..]);
        let ua_public = d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let auth = d("BTBZMqHH6r4Tts7J_aSIgg");

        let ecdh = d("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");
        let (cek, nonce) = derive_keys(&ecdh, &auth, &ua_public, &as_public, &salt);
        assert_eq!(cek.to_vec(), d("oIhVW04MRdy2XN9CiKLxTg"));
        assert_eq!(nonce.to_vec(), d("4h_95klXJ5E_qnoN"));

        let out = encrypt_with(&plaintext, &ua_public, &auth, &as_private, &salt).unwrap();
        let mut expected = d("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        assert_eq!(expected.len(), 86);
        expected.extend(d("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"));
        assert_eq!(out, expected);

        // The subscriber's side agrees on the shared secret.
        let ua_private =
            agreement::PrivateKey::from_private_key(&ECDH_P256, &d("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"))
                .unwrap();
        let shared =
            agreement::agree(&ua_private, UnparsedPublicKey::new(&ECDH_P256, &as_public[..]), (), |s| Ok(s.to_vec()))
                .unwrap();
        assert_eq!(shared, ecdh);
    }

    #[test]
    fn random_encryptions_differ() {
        let ua = agreement::PrivateKey::generate(&ECDH_P256).unwrap();
        let ua_public = ua.compute_public_key().unwrap();
        let a = encrypt(b"hi", ua_public.as_ref(), &[7; 16]).unwrap();
        let b = encrypt(b"hi", ua_public.as_ref(), &[7; 16]).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 86 + 3 + 16);
        assert!(encrypt(b"hi", &[4; 65], &[7; 16]).is_err(), "not a point on the curve");
    }

    #[test]
    fn vapid_jwt_has_the_right_header_and_claims_and_verifies() {
        let private = Vapid::generate_private_key().unwrap();
        assert_eq!(private.len(), 32);
        let vapid = Vapid::from_private_key(&private).unwrap();
        assert_eq!(vapid.public_key().len(), 65);
        assert_eq!(vapid.public_key()[0], 4);
        // The same scalar gives the same key.
        assert_eq!(Vapid::from_private_key(&private).unwrap().public_key(), vapid.public_key());

        let now = 1_800_000_000;
        let auth = vapid
            .authorization("https://fcm.googleapis.com/fcm/send/abc:def", Some("https://grouper.example.org"), now)
            .unwrap();
        let rest = auth.strip_prefix("vapid t=").unwrap();
        let (jwt, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, vapid.public_key_b64());
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&d(parts[0])).unwrap();
        assert_eq!(header, json!({"typ": "JWT", "alg": "ES256"}));
        let claims: Value = serde_json::from_slice(&d(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "https://grouper.example.org");
        let exp = claims["exp"].as_u64().unwrap();
        assert!(exp > now && exp <= now + 86_400, "{exp}");
        let signature = d(parts[2]);
        assert_eq!(signature.len(), 64);
        let input = format!("{}.{}", parts[0], parts[1]);
        SigKey::new(&ECDSA_P256_SHA256_FIXED, vapid.public_key()).verify(input.as_bytes(), &signature).unwrap();
        let other = Vapid::from_private_key(&Vapid::generate_private_key().unwrap()).unwrap();
        assert!(SigKey::new(&ECDSA_P256_SHA256_FIXED, other.public_key()).verify(input.as_bytes(), &signature).is_err());

        // No sub when none is known; a port stays in aud.
        let jwt = vapid.jwt(&audience("https://push.example:8443/x?y").unwrap(), None, now).unwrap();
        let claims: Value = serde_json::from_slice(&d(jwt.split('.').nth(1).unwrap())).unwrap();
        assert_eq!(claims, json!({"aud": "https://push.example:8443", "exp": now + JWT_LIFETIME}));
    }

    #[test]
    fn topics_are_short_url_safe_and_stable() {
        let t = topic("c1", "a7");
        assert_eq!(t.len(), 32);
        assert!(t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(t, topic("c1", "a7"));
        assert_ne!(t, topic("c2", "a7"));
    }

    #[test]
    fn contact_subject_accepts_mailto_https_and_loopback_http() {
        assert_eq!(contact_from_env("mailto:ops@example.com").as_deref(), Some("mailto:ops@example.com"));
        assert_eq!(contact_from_env("  https://grouper.example.org/app  ").as_deref(), Some("https://grouper.example.org"));
        assert_eq!(contact_from_env("http://localhost:8080/x").as_deref(), Some("http://localhost:8080"));
        assert_eq!(contact_from_env("http://127.0.0.1:8080").as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(contact_from_env("http://[::1]:8080").as_deref(), Some("http://[::1]:8080"));
        assert_eq!(contact_from_env("http://example.com"), None);
        assert_eq!(contact_from_env("mailto:not-an-email"), None);
        assert_eq!(contact_from_env("mailto:user@localhost"), None);
        assert_eq!(contact_from_env("mailto:ops@example.com?subject=hi"), None);

        assert_eq!(contact_origin("https://grouper.example.org/x").as_deref(), Some("https://grouper.example.org"));
        assert_eq!(contact_origin("http://localhost:8080").as_deref(), Some("http://localhost:8080"));
        assert_eq!(contact_origin("http://127.0.0.1:9000").as_deref(), Some("http://127.0.0.1:9000"));
        assert_eq!(contact_origin("http://[::1]:8080").as_deref(), Some("http://[::1]:8080"));
        assert_eq!(contact_origin("http://example.com"), None);
        assert_eq!(contact_origin("mailto:ops@example.com"), None);
    }

    #[test]
    fn endpoints_must_be_https_unless_loopback_http_is_allowed() {
        let ua = agreement::PrivateKey::generate(&ECDH_P256).unwrap();
        let p256dh = URL_SAFE_NO_PAD.encode(ua.compute_public_key().unwrap().as_ref());
        let sub = |endpoint: &str| NewSubscription {
            endpoint: endpoint.into(),
            expiration_time: None,
            keys: SubscriptionKeys { p256dh: p256dh.clone(), auth: URL_SAFE_NO_PAD.encode([1u8; 16]) },
            label: None,
        };
        assert!(validate(&sub("https://push.example/abc"), false).is_ok());
        assert!(validate(&sub("http://push.example/abc"), false).is_err());
        assert!(validate(&sub("http://127.0.0.1:9/abc"), false).is_err());
        assert!(validate(&sub("http://127.0.0.1:9/abc"), true).is_ok());
        assert!(validate(&sub("http://push.example/abc"), true).is_err());
        assert!(validate(&sub("ftp://push.example/abc"), true).is_err());
        assert!(validate(&sub("https://user:pw@push.example/abc"), false).is_err());
        let mut bad = sub("https://push.example/abc");
        bad.keys.auth = "short".into();
        assert!(validate(&bad, false).is_err());
        let mut bad = sub("https://push.example/abc");
        bad.keys.p256dh = URL_SAFE_NO_PAD.encode([4u8; 65]);
        assert!(validate(&bad, false).is_err());
    }
}
