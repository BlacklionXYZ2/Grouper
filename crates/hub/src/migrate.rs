//! Hub move state machine (`docs/local-first-design.md`, section 4a).
//!
//! The hub is a device id in the account (`hub_device_id`, with monotonic `epoch`). Moving the hub
//! transfers `grouper.db` + `workspaces/` (via the existing [`crate::backup`] archive + restore check)
//! and encrypted secrets (master key, optional identity key, optional tunnel token) to another
//! eligible, online desktop install of the same account, reassigns `hub_device_id` to that target
//! device with `epoch + 1`, and steps the old hub down so it becomes an ordinary device.
//!
//! State transitions: `idle → preparing → copying → switching → done | rolled_back`.
//!
//! Admin & move endpoints:
//! - `GET /v1/hub/move` (authed) — current [`MoveStatus`].
//! - `POST /v1/hub/move` (admin + fresh Google sign-in) — preflight and start a move.
//!   Fresh sign-in is either an `id_token` or a cookie session from `POST /v1/auth/google/return`
//!   created within the last 5 minutes for the owner's `sub`.
//! - `DELETE /v1/hub/move` (admin) — cancel an active move before the Switch commit point.
//! - `GET /v1/hub/move/{ticket}/snapshot` (target device token) — one-time backup archive download.
//! - `POST /v1/hub/move/{ticket}/secrets` (target device token) — one-time X25519-encrypted secrets hand-over.
//! - `POST /v1/hub/move/{ticket}/status` (target device token) — target progress/ready/switched/failed reports.

use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::agreement::{self, UnparsedPublicKey, X25519};
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use core_protocol::{EventBody, HubMessage};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::db::{Db, now, random_hex};
use crate::devices::compare_versions;
use crate::host::{self, HubHandle};
use crate::registry::deny_unless_admin;
use crate::server::{HUB_DEVICE, Hub, err};
use crate::stepdown::{StepDownInfo, fetch_hub_info, http_client};

const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
const SECRETS_KDF_DOMAIN: &[u8] = b"grouper-hub-move-secrets-v1\0";
const SECRETS_ALGORITHM: &str = "x25519-xchacha20poly1305";
const IDENTITY_KEY_NAME: &str = "identity.key";
const IDENTITY_KEY_META: &str = "hub_identity_key";
const HEADER_SNAPSHOT_SHA256: &str = "x-snapshot-sha256";
const STOP_TUNNEL_TIMEOUT: Duration = Duration::from_secs(5);
const FRESH_SIGNIN_MAX_AGE: Duration = Duration::from_secs(300);

/// State of the hub move state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovePhase {
    Idle,
    Preparing,
    Copying,
    Switching,
    Done,
    RolledBack,
}

impl MovePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Preparing => "preparing",
            Self::Copying => "copying",
            Self::Switching => "switching",
            Self::Done => "done",
            Self::RolledBack => "rolled_back",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Preparing | Self::Copying | Self::Switching)
    }
}

/// How to handle running hub threads during Step 2 (Pause).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadsMode {
    #[default]
    Wait,
    Stop,
}

/// Public snapshot of the hub move state machine (`GET /v1/hub/move`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_id: Option<String>,
    pub state: MovePhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads_mode: Option<ThreadsMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_move_backup: Option<String>,
}

impl Default for MoveStatus {
    fn default() -> Self {
        Self {
            move_id: None,
            state: MovePhase::Idle,
            target_device_id: None,
            target_name: None,
            threads_mode: None,
            progress_pct: None,
            snapshot_bytes: None,
            message: None,
            error: None,
            started_at: None,
            updated_at: None,
            new_epoch: None,
            pre_move_backup: None,
        }
    }
}

/// Fault-injection hook for testing rollback at every stage of the move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectedFailure {
    /// Fail during Step 2 (Preparing / Pause) before building the snapshot.
    BeforeSnapshot,
    /// Corrupt the snapshot archive bytes after computing the manifest SHA-256 (Step 3: Copy).
    CorruptSnapshot,
    /// Hand over a wrong master key so the target fails its restore decryption check (Step 4: Secrets).
    CorruptSecrets,
    /// Do not confirm the switch before `confirm_timeout` elapses (Step 5: Switch).
    SkipSwitchConfirm,
}

/// Tuning knobs for the move state machine (tests override timeouts and inject failures).
#[derive(Clone, Debug)]
pub struct MigrateConfig {
    /// How long Step 5 waits for the new hub to confirm before rolling back with `epoch + 2`.
    pub confirm_timeout: Duration,
    /// How long Steps 3-4 wait for the target to download, verify, and report ready.
    pub copy_timeout: Duration,
    /// Polling cadence for hub-thread drain and switch confirmation.
    pub poll_interval: Duration,
    /// Optional fault injection point for rollback tests.
    pub inject_failure: Option<InjectedFailure>,
}

impl Default for MigrateConfig {
    fn default() -> Self {
        Self {
            confirm_timeout: Duration::from_secs(120),
            copy_timeout: Duration::from_secs(300),
            poll_interval: Duration::from_secs(1),
            inject_failure: None,
        }
    }
}

/// Request body for `POST /v1/hub/move`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StartMoveRequest {
    /// Target device id, install id, or device name.
    pub target_device: String,
    /// Fresh Google ID token signed with a hub-issued nonce (<= 5 minutes old).
    /// When omitted, a cookie session opened by `POST /v1/auth/google/return` within the
    /// last 5 minutes for the owner's `sub` is accepted instead.
    #[serde(default)]
    pub id_token: Option<String>,
    /// What to do with running hub threads (`wait` or `stop`).
    #[serde(default)]
    pub threads: ThreadsMode,
}

#[derive(Clone, Debug)]
struct TargetUpdate {
    stage: String,
    progress_pct: Option<u8>,
    error: Option<String>,
    target_url: Option<String>,
}

struct ActiveTicket {
    move_id: String,
    ticket: String,
    target_device_id: String,
    snapshot_path: PathBuf,
    snapshot_sha256: String,
    snapshot_consumed: bool,
    secrets_consumed: bool,
    expires_at: u64,
    status_tx: mpsc::UnboundedSender<TargetUpdate>,
}

/// Per-hub move state held on [`Hub`].
pub struct MigrateState {
    paused: AtomicBool,
    config: Mutex<MigrateConfig>,
    status: watch::Sender<MoveStatus>,
    ticket: Mutex<Option<ActiveTicket>>,
    cancel: Notify,
    cancelled: AtomicBool,
}

impl Default for MigrateState {
    fn default() -> Self {
        Self::new()
    }
}

impl MigrateState {
    pub fn new() -> Self {
        let (status, _rx) = watch::channel(MoveStatus::default());
        Self {
            paused: AtomicBool::new(false),
            config: Mutex::new(MigrateConfig::default()),
            status,
            ticket: Mutex::new(None),
            cancel: Notify::new(),
            cancelled: AtomicBool::new(false),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(AtomicOrdering::SeqCst)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, AtomicOrdering::SeqCst);
    }

    pub fn config(&self) -> MigrateConfig {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_config(&self, config: MigrateConfig) {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = config;
    }

    pub fn status(&self) -> MoveStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<MoveStatus> {
        self.status.subscribe()
    }

    fn set_status(&self, hub: &Hub, next: MoveStatus) {
        let event = EventBody::HubMoveProgress {
            move_id: next.move_id.clone().unwrap_or_default(),
            state: next.state.as_str().to_string(),
            target_device_id: next.target_device_id.clone(),
            target_name: next.target_name.clone(),
            progress_pct: next.progress_pct,
            message: next.message.clone(),
            error: next.error.clone(),
        };
        let _ = self.status.send(next);
        hub.emit_live("", None, event);
    }

    fn clear_ticket(&self) -> Option<PathBuf> {
        self.ticket.lock().unwrap_or_else(|e| e.into_inner()).take().map(|t| t.snapshot_path)
    }
}

// ------------------------------------------------------------------ secrets encryption (X25519 + XChaCha20Poly1305)

/// Secrets handed over from the old hub to the target during Step 4 (never stored in backup archives).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveSecrets {
    pub master_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_token: Option<String>,
}

/// Encrypted secrets payload returned by `POST /v1/hub/move/{ticket}/secrets`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedSecretsEnvelope {
    pub algorithm: String,
    pub sender_public_key: String,
    pub nonce: String,
    pub ciphertext: String,
}

/// Ephemeral X25519 keypair generated by the target node for one secrets hand-over.
pub struct TargetKeyPair {
    private: agreement::PrivateKey,
    public: Vec<u8>,
}

impl TargetKeyPair {
    pub fn generate() -> Result<Self> {
        let private = agreement::PrivateKey::generate(&X25519)
            .map_err(|_| anyhow!("generating ephemeral X25519 key failed"))?;
        let public = private
            .compute_public_key()
            .map_err(|_| anyhow!("computing X25519 public key failed"))?
            .as_ref()
            .to_vec();
        Ok(Self { private, public })
    }

    pub fn public_key_b64(&self) -> String {
        STANDARD.encode(&self.public)
    }

    /// Decrypt an [`EncryptedSecretsEnvelope`] sealed to this keypair's public key.
    pub fn decrypt_secrets(&self, env: &EncryptedSecretsEnvelope) -> Result<MoveSecrets> {
        if env.algorithm != SECRETS_ALGORITHM {
            bail!("unsupported secrets envelope algorithm {:?}", env.algorithm);
        }
        let sender_pub = STANDARD
            .decode(env.sender_public_key.trim())
            .context("invalid base64 sender_public_key")?;
        if sender_pub.len() != 32 {
            bail!("sender_public_key must be 32 bytes");
        }
        let nonce_bytes = STANDARD.decode(env.nonce.trim()).context("invalid base64 nonce")?;
        if nonce_bytes.len() != 24 {
            bail!("nonce must be 24 bytes");
        }
        let ciphertext = STANDARD.decode(env.ciphertext.trim()).context("invalid base64 ciphertext")?;
        let shared = agreement::agree(
            &self.private,
            UnparsedPublicKey::new(&X25519, &sender_pub),
            (),
            |secret| Ok(secret.to_vec()),
        )
        .map_err(|_| anyhow!("ECDH key agreement failed"))?;
        let key = derive_secrets_key(&shared, &sender_pub, &self.public);
        let cipher = XChaCha20Poly1305::new((&key).into());
        let plaintext = cipher
            .decrypt(XNonce::from_slice(&nonce_bytes), ciphertext.as_slice())
            .map_err(|_| anyhow!("decrypting hub secrets failed (wrong key or tampered payload)"))?;
        serde_json::from_slice(&plaintext).context("decoding decrypted hub secrets")
    }
}

/// Encrypt `secrets` to `target_public_key_b64` using ephemeral X25519 + XChaCha20Poly1305.
pub fn encrypt_secrets(target_public_key_b64: &str, secrets: &MoveSecrets) -> Result<EncryptedSecretsEnvelope> {
    let target_pub = STANDARD
        .decode(target_public_key_b64.trim())
        .context("invalid base64 target public key")?;
    if target_pub.len() != 32 {
        bail!("target public key must be 32 bytes");
    }
    let sender_priv = agreement::PrivateKey::generate(&X25519)
        .map_err(|_| anyhow!("generating ephemeral sender X25519 key failed"))?;
    let sender_pub = sender_priv
        .compute_public_key()
        .map_err(|_| anyhow!("computing sender X25519 public key failed"))?
        .as_ref()
        .to_vec();
    let shared = agreement::agree(
        &sender_priv,
        UnparsedPublicKey::new(&X25519, &target_pub),
        (),
        |secret| Ok(secret.to_vec()),
    )
    .map_err(|_| anyhow!("ECDH key agreement failed"))?;
    let key = derive_secrets_key(&shared, &sender_pub, &target_pub);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).context("generating XChaCha20 nonce")?;
    let plaintext = serde_json::to_vec(secrets).context("encoding hub secrets")?;
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_slice())
        .map_err(|_| anyhow!("encrypting hub secrets failed"))?;
    Ok(EncryptedSecretsEnvelope {
        algorithm: SECRETS_ALGORITHM.to_string(),
        sender_public_key: STANDARD.encode(&sender_pub),
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    })
}

fn derive_secrets_key(shared: &[u8], sender_pub: &[u8], target_pub: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SECRETS_KDF_DOMAIN);
    hasher.update(shared);
    hasher.update(sender_pub);
    hasher.update(target_pub);
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

fn read_master_key_b64(data_dir: &Path) -> Result<String> {
    crate::secrets::read_master_key_b64(data_dir)
}

fn read_identity_key(data_dir: &Path, db: &Db) -> Option<String> {
    if let Ok(raw) = fs::read_to_string(data_dir.join(IDENTITY_KEY_NAME)) {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    db.c()
        .query_row("SELECT value FROM hub_meta WHERE key = ?1", [IDENTITY_KEY_META], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
        .and_then(|v| {
            if crate::secrets::Secrets::is_sealed(&v) {
                db.unseal(&v).ok()
            } else {
                Some(v)
            }
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("key"),
        random_hex(4)
    ));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let res = (|| -> Result<()> {
        let mut f = opts.open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        writeln!(f, "{}", contents.trim())?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, path).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("replacing {}", path.display())
    })?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, hex::encode(hasher.finalize())))
}

// ------------------------------------------------------------------ pause middleware & fresh sign-in

/// Axum middleware that refuses mutating requests with HTTP 503 while a hub move is paused.
pub(crate) async fn pause_guard(State(hub): State<Arc<Hub>>, req: Request, next: Next) -> Response {
    if !hub.is_move_paused() {
        return next.run(req).await;
    }
    let method = req.method();
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let path = req.uri().path();
    let exempt = path == "/healthz"
        || path == "/v1/session"
        || path.starts_with("/v1/auth/")
        || path == "/v1/hub/move"
        || path.starts_with("/v1/hub/move/");
    if exempt {
        return next.run(req).await;
    }
    err(StatusCode::SERVICE_UNAVAILABLE, "Your hub is moving")
}

/// Require a fresh Google sign-in per section 4a.
///
/// Either a one-time `id_token` (`iat` within 5 minutes, nonce consumed, `sub` equal to the
/// hub owner's) or, when no token is sent, the caller's cookie session from
/// `POST /v1/auth/google/return` created within that same window for the owner's `sub`.
/// A plain token session, an older Google session, or another account's session does not qualify.
/// Cookie CSRF rules are unchanged: they run in the session guard before this handler.
async fn verify_fresh_google_signin(
    hub: &Hub,
    headers: &HeaderMap,
    id_token: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    if let Some(raw) = id_token.map(str::trim).filter(|s| !s.is_empty()) {
        return crate::google_auth::verify_fresh(hub, raw, FRESH_SIGNIN_MAX_AGE).await;
    }
    verify_fresh_google_session(hub, headers)
}

fn verify_fresh_google_session(hub: &Hub, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let Some(session) = crate::session::first_live_cookie_session(hub, headers) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "a fresh Google sign-in is required to move the hub".into(),
        ));
    };
    let Some(sub) = session.google_sub else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "a fresh Google sign-in is required to move the hub".into(),
        ));
    };
    if session.created.saturating_add(FRESH_SIGNIN_MAX_AGE.as_secs()) <= now() {
        return Err((StatusCode::UNAUTHORIZED, "this Google sign-in is too old; sign in again".into()));
    }
    match hub.db.owner() {
        Some(owner) if owner.provider == "google" && owner.sub == sub => Ok(()),
        Some(_) => Err((StatusCode::FORBIDDEN, "this hub belongs to a different Google account".into())),
        None => Err((StatusCode::FORBIDDEN, "this hub has no owner yet".into())),
    }
}

// ------------------------------------------------------------------ old hub move driver

impl Hub {
    /// Handle a `NodeMessage::HubMoveStatus` update arriving over a device WebSocket connection.
    pub(crate) async fn handle_move_status(
        &self,
        device_id: &str,
        move_id: &str,
        stage: &str,
        progress_pct: Option<u8>,
        error: Option<String>,
        target_url: Option<String>,
    ) {
        let tx = {
            let guard = self.migrate.ticket.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .filter(|t| t.move_id == move_id && t.target_device_id == device_id)
                .map(|t| t.status_tx.clone())
        };
        if let Some(tx) = tx {
            let _ = tx.send(TargetUpdate {
                stage: stage.to_string(),
                progress_pct,
                error,
                target_url,
            });
        }
    }

    /// Run preflight checks and start moving the hub to `req.target_device`.
    pub async fn start_move(
        self: &Arc<Self>,
        headers: &HeaderMap,
        req: StartMoveRequest,
    ) -> Result<MoveStatus, (StatusCode, String)> {
        verify_fresh_google_signin(self, headers, req.id_token.as_deref()).await?;

        if self.is_stepped_down() {
            return Err((
                StatusCode::CONFLICT,
                "this hub has already stepped down and cannot start a move".into(),
            ));
        }
        if self.migrate.status().state.is_active() {
            return Err((StatusCode::CONFLICT, "a hub move is already in progress".into()));
        }

        let installs = self.install_devices().await;
        let query = req.target_device.trim();
        if query.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "target_device is required".into()));
        }

        let target = installs
            .installs
            .iter()
            .find(|i| {
                i.device_id == query
                    || i.node_device_id.as_deref() == Some(query)
                    || i.install_id.as_deref() == Some(query)
                    || i.device_ids.iter().any(|id| id == query)
                    || i.name.eq_ignore_ascii_case(query)
            })
            .cloned()
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no device {query:?} found")))?;

        if target.is_hub {
            return Err((StatusCode::CONFLICT, format!("{} is already the hub", target.name)));
        }
        if !target.eligible {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{} cannot host Grouper (must be an eligible Mac, Windows, or Linux install)", target.name),
            ));
        }
        if !target.online {
            return Err((
                StatusCode::CONFLICT,
                format!("{} is offline; turn it on to move the hub there", target.name),
            ));
        }
        let target_node_id = target.node_device_id.clone().ok_or_else(|| {
            (StatusCode::CONFLICT, format!("{} has no connected node row to receive the move", target.name))
        })?;
        if !self.is_online(&target_node_id).await {
            return Err((
                StatusCode::CONFLICT,
                format!("{} node connection is offline", target.name),
            ));
        }
        if let Some(host) = &target.host {
            if compare_versions(&host.app_version, HUB_VERSION) == Ordering::Less {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "Update Grouper on {} first (running {}, hub is {})",
                        target.name, host.app_version, HUB_VERSION
                    ),
                ));
            }
            let est = installs.estimated_snapshot_bytes;
            let needed = est.saturating_mul(2);
            if let Some(free) = host.free_bytes
                && free < needed
            {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "{} does not have enough free disk space (have {free} bytes, need at least {needed} bytes)",
                        target.name
                    ),
                ));
            }
        }

        // Verify target belongs to the same account when the hub has an owner.
        if let Some(owner) = self.db.owner() {
            let target_account: Option<String> = self
                .db
                .c()
                .query_row("SELECT account_sub FROM devices WHERE id = ?1", [&target_node_id], |r| r.get(0))
                .optional()
                .ok()
                .flatten()
                .flatten();
            if let Some(sub) = target_account
                && sub != owner.sub
            {
                return Err((
                    StatusCode::FORBIDDEN,
                    format!("{} belongs to a different account", target.name),
                ));
            }
        }

        let (current_hub_device_id, current_epoch) = self
            .db
            .ensure_hub_device()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        let move_id = format!("m_{}", random_hex(8));
        let started_at = now();
        let initial = MoveStatus {
            move_id: Some(move_id.clone()),
            state: MovePhase::Preparing,
            target_device_id: Some(target_node_id.clone()),
            target_name: Some(target.name.clone()),
            threads_mode: Some(req.threads),
            progress_pct: Some(0),
            snapshot_bytes: Some(installs.estimated_snapshot_bytes),
            message: Some(format!("Preparing move to {}…", target.name)),
            error: None,
            started_at: Some(started_at),
            updated_at: Some(started_at),
            new_epoch: None,
            pre_move_backup: None,
        };

        self.migrate.cancelled.store(false, AtomicOrdering::SeqCst);
        self.migrate.set_paused(true);
        self.migrate.set_status(self, initial.clone());

        let hub = Arc::clone(self);
        let target_name = target.name.clone();
        let target_free_bytes = target.host.as_ref().and_then(|h| h.free_bytes);
        let threads_mode = req.threads;
        tokio::spawn(async move {
            drive_move(
                hub,
                move_id,
                current_hub_device_id,
                current_epoch,
                target_node_id,
                target_name,
                target_free_bytes,
                threads_mode,
                started_at,
            )
            .await;
        });

        Ok(initial)
    }

    /// Cancel an active hub move if it has not yet reached the Switch commit point.
    pub async fn cancel_move(&self) -> Result<MoveStatus, (StatusCode, String)> {
        let current = self.migrate.status();
        match current.state {
            MovePhase::Preparing | MovePhase::Copying => {
                self.migrate.cancelled.store(true, AtomicOrdering::SeqCst);
                self.migrate.cancel.notify_waiters();
                Ok(current)
            }
            MovePhase::Switching => Err((
                StatusCode::CONFLICT,
                "cannot cancel a move once switching has started".into(),
            )),
            _ => Err((StatusCode::CONFLICT, "no hub move is currently in progress".into())),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_move(
    hub: Arc<Hub>,
    move_id: String,
    current_hub_device_id: String,
    current_epoch: u64,
    target_device_id: String,
    target_name: String,
    target_free_bytes: Option<u64>,
    threads_mode: ThreadsMode,
    started_at: u64,
) {
    let cfg = hub.migrate.config();

    let rollback_pre_switch = |reason: String| {
        let hub = hub.clone();
        let move_id = move_id.clone();
        let target_device_id = target_device_id.clone();
        let target_name = target_name.clone();
        async move {
            let _ = hub
                .send_to(
                    &target_device_id,
                    HubMessage::HubMoveAbort { move_id: move_id.clone(), reason: reason.clone() },
                )
                .await;
            if let Some(path) = hub.migrate.clear_ticket() {
                let _ = fs::remove_file(path);
            }
            hub.migrate.set_paused(false);
            hub.migrate.set_status(
                &hub,
                MoveStatus {
                    move_id: Some(move_id),
                    state: MovePhase::RolledBack,
                    target_device_id: Some(target_device_id),
                    target_name: Some(target_name),
                    threads_mode: Some(threads_mode),
                    progress_pct: None,
                    snapshot_bytes: None,
                    message: Some("Move rolled back; this hub resumed serving".into()),
                    error: Some(reason),
                    started_at: Some(started_at),
                    updated_at: Some(now()),
                    new_epoch: None,
                    pre_move_backup: None,
                },
            );
        }
    };

    // ---- Step 2: Pause (drain or stop hub threads; device threads are untouched)
    let running_hub_threads: Vec<String> = hub
        .db
        .threads(None)
        .into_iter()
        .filter(|t| t.device_id == HUB_DEVICE && t.status == "running")
        .map(|t| t.id)
        .collect();

    match threads_mode {
        ThreadsMode::Stop => {
            for tid in running_hub_threads {
                let _ = hub.stop_thread_as(&tid, "move", "Stopped: the hub moved").await;
            }
        }
        ThreadsMode::Wait => {
            while hub
                .db
                .threads(None)
                .iter()
                .any(|t| t.device_id == HUB_DEVICE && t.status == "running")
            {
                if hub.migrate.cancelled.load(AtomicOrdering::SeqCst) {
                    rollback_pre_switch("Move cancelled while waiting for hub threads".into()).await;
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(cfg.poll_interval) => {}
                    _ = hub.migrate.cancel.notified() => {
                        rollback_pre_switch("Move cancelled while waiting for hub threads".into()).await;
                        return;
                    }
                }
            }
        }
    }

    if hub.migrate.cancelled.load(AtomicOrdering::SeqCst) {
        rollback_pre_switch("Move cancelled before snapshot".into()).await;
        return;
    }
    if cfg.inject_failure == Some(InjectedFailure::BeforeSnapshot) {
        rollback_pre_switch("Injected failure during preparing stage".into()).await;
        return;
    }

    // ---- Step 3: Ticketed snapshot (reuse crate::backup)
    // Materialize hub_id before taking the snapshot so old and new hubs share the same hub_id.
    let _ = hub.db.hub_id();
    let snapshot_path = hub.data_dir.join(format!(".move-{move_id}.tar.gz"));
    let data_dir = hub.data_dir.clone();
    let snap_for_build = snapshot_path.clone();
    let build_res = tokio::task::spawn_blocking(move || -> Result<(u64, String)> {
        crate::backup::backup(&data_dir, &snap_for_build, true)?;
        sha256_file(&snap_for_build)
    })
    .await;

    let (snapshot_bytes, snapshot_sha256) = match build_res {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            let _ = fs::remove_file(&snapshot_path);
            rollback_pre_switch(format!("creating hub snapshot failed: {e:#}")).await;
            return;
        }
        Err(e) => {
            let _ = fs::remove_file(&snapshot_path);
            rollback_pre_switch(format!("snapshot task failed: {e}")).await;
            return;
        }
    };

    if let Some(free) = target_free_bytes
        && free < snapshot_bytes.saturating_mul(2)
    {
        let _ = fs::remove_file(&snapshot_path);
        rollback_pre_switch(format!(
            "{target_name} does not have enough free disk space for the {snapshot_bytes}-byte snapshot"
        ))
        .await;
        return;
    }

    // If testing snapshot corruption, flip a byte in the archive AFTER computing snapshot_sha256.
    if cfg.inject_failure == Some(InjectedFailure::CorruptSnapshot)
        && let Ok(mut bytes) = fs::read(&snapshot_path)
        && let Some(last) = bytes.last_mut()
    {
        *last ^= 0xFF;
        let _ = fs::write(&snapshot_path, &bytes);
    }

    let ticket = random_hex(16);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<TargetUpdate>();
    {
        let mut guard = hub.migrate.ticket.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(ActiveTicket {
            move_id: move_id.clone(),
            ticket: ticket.clone(),
            target_device_id: target_device_id.clone(),
            snapshot_path: snapshot_path.clone(),
            snapshot_sha256: snapshot_sha256.clone(),
            snapshot_consumed: false,
            secrets_consumed: false,
            expires_at: now() + 600,
            status_tx,
        });
    }

    hub.migrate.set_status(
        &hub,
        MoveStatus {
            move_id: Some(move_id.clone()),
            state: MovePhase::Copying,
            target_device_id: Some(target_device_id.clone()),
            target_name: Some(target_name.clone()),
            threads_mode: Some(threads_mode),
            progress_pct: Some(10),
            snapshot_bytes: Some(snapshot_bytes),
            message: Some(format!("Moving your hub to {target_name}… copying (10%)")),
            error: None,
            started_at: Some(started_at),
            updated_at: Some(now()),
            new_epoch: None,
            pre_move_backup: None,
        },
    );

    let sent = hub
        .send_to(
            &target_device_id,
            HubMessage::HubMoveOffer {
                move_id: move_id.clone(),
                ticket: ticket.clone(),
                target_device_id: target_device_id.clone(),
                current_epoch,
                snapshot_bytes,
                snapshot_sha256,
            },
        )
        .await;
    if !sent {
        rollback_pre_switch(format!("could not deliver move ticket to {target_name}")).await;
        return;
    }

    // ---- Step 3 & 4 wait: wait until target finishes copy + secrets + restore check and reports `ready`
    let copy_deadline = tokio::time::Instant::now() + cfg.copy_timeout;
    let mut reported_target_url: Option<String> = None;
    loop {
        if hub.migrate.cancelled.load(AtomicOrdering::SeqCst) {
            rollback_pre_switch("Move cancelled during copy".into()).await;
            return;
        }
        let remain = copy_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            rollback_pre_switch(format!("{target_name} did not become ready in time")).await;
            return;
        }
        tokio::select! {
            _ = hub.migrate.cancel.notified() => {
                rollback_pre_switch("Move cancelled during copy".into()).await;
                return;
            }
            _ = tokio::time::sleep(remain) => {
                rollback_pre_switch(format!("{target_name} did not become ready in time")).await;
                return;
            }
            update = status_rx.recv() => {
                let Some(u) = update else {
                    rollback_pre_switch(format!("lost status channel with {target_name}")).await;
                    return;
                };
                if let Some(url) = u.target_url {
                    reported_target_url = Some(url);
                }
                match u.stage.as_str() {
                    "copying" => {
                        let pct = u.progress_pct.unwrap_or(50).min(89);
                        hub.migrate.set_status(
                            &hub,
                            MoveStatus {
                                move_id: Some(move_id.clone()),
                                state: MovePhase::Copying,
                                target_device_id: Some(target_device_id.clone()),
                                target_name: Some(target_name.clone()),
                                threads_mode: Some(threads_mode),
                                progress_pct: Some(pct),
                                snapshot_bytes: Some(snapshot_bytes),
                                message: Some(format!("Moving your hub to {target_name}… copying ({pct}%)")),
                                error: None,
                                started_at: Some(started_at),
                                updated_at: Some(now()),
                                new_epoch: None,
                                pre_move_backup: None,
                            },
                        );
                    }
                    "ready" => break,
                    "failed" => {
                        let why = u.error.unwrap_or_else(|| format!("{target_name} failed to stage the hub copy"));
                        rollback_pre_switch(why).await;
                        return;
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- Step 5: Switch (the commit point)
    let new_epoch = current_epoch + 1;
    hub.migrate.set_status(
        &hub,
        MoveStatus {
            move_id: Some(move_id.clone()),
            state: MovePhase::Switching,
            target_device_id: Some(target_device_id.clone()),
            target_name: Some(target_name.clone()),
            threads_mode: Some(threads_mode),
            progress_pct: Some(90),
            snapshot_bytes: Some(snapshot_bytes),
            message: Some(format!("Switching hub to {target_name}…")),
            error: None,
            started_at: Some(started_at),
            updated_at: Some(now()),
            new_epoch: Some(new_epoch),
            pre_move_backup: None,
        },
    );

    // Stop cloudflared before the target brings up its connector so two hubs never serve the tunnel at once.
    hub.tunnel.stop_and_wait(STOP_TUNNEL_TIMEOUT).await;

    let sent_commit = hub
        .send_to(
            &target_device_id,
            HubMessage::HubMoveCommit {
                move_id: move_id.clone(),
                ticket: ticket.clone(),
                target_device_id: target_device_id.clone(),
                new_epoch,
            },
        )
        .await;
    if !sent_commit {
        rollback_step5(
            &hub,
            &move_id,
            &current_hub_device_id,
            current_epoch,
            &target_device_id,
            &target_name,
            threads_mode,
            started_at,
            format!("lost connection to {target_name} at switch"),
        )
        .await;
        return;
    }

    // Wait up to `confirm_timeout` for the new hub to answer `/v1/hub` with `target_device_id` and `epoch >= new_epoch`.
    let client = http_client();
    // /v1/hub shows the opaque account id (server::account_id), never the email.
    let owner_account = hub.db.owner().map(|o| crate::server::account_id(&o.provider, &o.sub));
    let confirm_deadline = tokio::time::Instant::now() + cfg.confirm_timeout;

    let confirmed_url: Option<String> = loop {
        // Drain any non-blocking target updates (e.g. direct URL or failure).
        while let Ok(u) = status_rx.try_recv() {
            if let Some(url) = u.target_url {
                reported_target_url = Some(url);
            }
            if u.stage == "failed" {
                let why = u.error.unwrap_or_else(|| format!("{target_name} failed to start during switch"));
                rollback_step5(
                    &hub,
                    &move_id,
                    &current_hub_device_id,
                    current_epoch,
                    &target_device_id,
                    &target_name,
                    threads_mode,
                    started_at,
                    why,
                )
                .await;
                return;
            }
        }

        if cfg.inject_failure != Some(InjectedFailure::SkipSwitchConfirm) {
            // Check direct target URL (LAN/local mode) and public address (configured or default).
            let candidates = confirm_candidates(
                reported_target_url.as_deref(),
                &hub.db.public_address(),
            );
            let mut found = None;
            for addr in &candidates {
                if let Ok(info) = fetch_hub_info(&client, addr).await {
                    let account_ok = match (&owner_account, &info.account) {
                        (Some(local), Some(remote)) => local.eq_ignore_ascii_case(remote),
                        (None, None) => true,
                        _ => false,
                    };
                    if account_ok && info.hub_device_id == target_device_id && info.epoch >= new_epoch {
                        found = Some((addr.clone(), info.epoch));
                        break;
                    }
                }
            }
            if let Some((addr, confirmed_epoch)) = found {
                // Step 6: Old hub steps down and becomes an ordinary device.
                let stepdown_info = StepDownInfo {
                    account: owner_account.clone().unwrap_or_default(),
                    local_hub_device_id: current_hub_device_id.clone(),
                    local_epoch: current_epoch,
                    peer_hub_device_id: target_device_id.clone(),
                    peer_epoch: confirmed_epoch,
                    address: addr.clone(),
                    detected_at: now(),
                };
                hub.stepdown.mark_stepped_down(stepdown_info);
                hub.tunnel.stop_and_wait(STOP_TUNNEL_TIMEOUT).await;
                break Some(addr);
            }
        }

        let remain = confirm_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            break None;
        }
        let sleep_for = remain.min(cfg.poll_interval);
        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            update = status_rx.recv() => {
                if let Some(u) = update {
                    if let Some(url) = u.target_url {
                        reported_target_url = Some(url);
                    }
                    if u.stage == "failed" {
                        let why = u.error.unwrap_or_else(|| format!("{target_name} failed during switch"));
                        rollback_step5(
                            &hub,
                            &move_id,
                            &current_hub_device_id,
                            current_epoch,
                            &target_device_id,
                            &target_name,
                            threads_mode,
                            started_at,
                            why,
                        )
                        .await;
                        return;
                    }
                }
            }
        }
    };

    let Some(_addr) = confirmed_url else {
        rollback_step5(
            &hub,
            &move_id,
            &current_hub_device_id,
            current_epoch,
            &target_device_id,
            &target_name,
            threads_mode,
            started_at,
            format!("{target_name} did not confirm serving within {:?}", cfg.confirm_timeout),
        )
        .await;
        return;
    };

    // Clean up temporary transfer archive; record pre-move retention marker (old data stays untouched for 7 days).
    if let Some(path) = hub.migrate.clear_ticket() {
        let _ = fs::remove_file(path);
    }
    let pre_move_marker = format!("pre-move-{}", now());
    let _ = fs::write(
        hub.data_dir.join(format!("{pre_move_marker}.marker")),
        format!("moved_to={target_device_id}\nnew_epoch={new_epoch}\nretain_days=7\n"),
    );

    hub.migrate.set_status(
        &hub,
        MoveStatus {
            move_id: Some(move_id),
            state: MovePhase::Done,
            target_device_id: Some(target_device_id),
            target_name: Some(target_name.clone()),
            threads_mode: Some(threads_mode),
            progress_pct: Some(100),
            snapshot_bytes: Some(snapshot_bytes),
            message: Some(format!("Hub moved to {target_name}; this machine is now an ordinary device")),
            error: None,
            started_at: Some(started_at),
            updated_at: Some(now()),
            new_epoch: Some(new_epoch),
            pre_move_backup: Some(pre_move_marker),
        },
    );
}

fn confirm_candidates(reported_target_url: Option<&str>, public_address: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(url) = reported_target_url.map(str::trim).filter(|s| !s.is_empty()) {
        candidates.push(url.trim_end_matches('/').to_string());
    }
    let pub_addr = public_address.trim().trim_end_matches('/');
    if !pub_addr.is_empty() && !candidates.iter().any(|c| c == pub_addr) {
        candidates.push(pub_addr.to_string());
    }
    candidates
}

#[allow(clippy::too_many_arguments)]
async fn rollback_step5(
    hub: &Arc<Hub>,
    move_id: &str,
    current_hub_device_id: &str,
    current_epoch: u64,
    target_device_id: &str,
    target_name: &str,
    threads_mode: ThreadsMode,
    started_at: u64,
    reason: String,
) {
    // Per section 4a: if target fails to confirm at step 5, old hub resumes from its untouched data
    // with `epoch + 2` so the target sees a higher epoch than its own (`epoch + 1`) and discards its copy.
    let recovery_epoch = current_epoch + 2;
    let _ = hub.db.c().execute(
        "UPDATE hub_state SET hub_device_id = ?1, epoch = ?2 WHERE id = 1",
        params![current_hub_device_id, recovery_epoch as i64],
    );
    let _ = hub
        .send_to(
            target_device_id,
            HubMessage::HubMoveAbort { move_id: move_id.to_string(), reason: reason.clone() },
        )
        .await;
    if let Some(path) = hub.migrate.clear_ticket() {
        let _ = fs::remove_file(path);
    }
    hub.migrate.set_paused(false);
    hub.tunnel.notify_changed();
    hub.migrate.set_status(
        hub,
        MoveStatus {
            move_id: Some(move_id.to_string()),
            state: MovePhase::RolledBack,
            target_device_id: Some(target_device_id.to_string()),
            target_name: Some(target_name.to_string()),
            threads_mode: Some(threads_mode),
            progress_pct: None,
            snapshot_bytes: None,
            message: Some(format!(
                "Switch timed out or failed; old hub resumed at epoch {recovery_epoch}"
            )),
            error: Some(reason),
            started_at: Some(started_at),
            updated_at: Some(now()),
            new_epoch: Some(recovery_epoch),
            pre_move_backup: None,
        },
    );
}

// ------------------------------------------------------------------ HTTP routes

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/hub/move", get(get_move_status).post(post_start_move).delete(delete_cancel_move))
        .route("/v1/hub/move/{ticket}/snapshot", get(get_move_snapshot))
        .route("/v1/hub/move/{ticket}/secrets", post(post_move_secrets))
        .route("/v1/hub/move/{ticket}/status", post(post_move_status))
}

async fn get_move_status(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if hub.auth(&headers).is_none() {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing device token");
    }
    Json(hub.migrate.status()).into_response()
}

async fn post_start_move(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(req): Json<StartMoveRequest>,
) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.start_move(&headers, req).await {
        Ok(status) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Err((code, msg)) => err(code, msg),
    }
}

async fn delete_cancel_move(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.cancel_move().await {
        Ok(status) => Json(status).into_response(),
        Err((code, msg)) => err(code, msg),
    }
}

async fn get_move_snapshot(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    AxumPath(ticket): AxumPath<String>,
) -> Response {
    let Some(auth) = hub.auth_bearer(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing device token");
    };
    let (path, sha256) = {
        let mut guard = hub.migrate.ticket.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = guard.as_mut() else {
            return err(StatusCode::NOT_FOUND, "no active move ticket");
        };
        if active.ticket != ticket {
            return err(StatusCode::NOT_FOUND, "unknown move ticket");
        }
        if active.target_device_id != auth.device_id {
            return err(StatusCode::FORBIDDEN, "ticket belongs to another device");
        }
        if now() > active.expires_at {
            return err(StatusCode::GONE, "move ticket expired");
        }
        if active.snapshot_consumed {
            return err(StatusCode::GONE, "snapshot ticket was already used");
        }
        active.snapshot_consumed = true;
        (active.snapshot_path.clone(), active.snapshot_sha256.clone())
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("reading snapshot failed: {e}")),
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/gzip"));
    if let Ok(val) = HeaderValue::from_str(&sha256) {
        resp.headers_mut().insert(HEADER_SNAPSHOT_SHA256, val);
    }
    resp
}

#[derive(Deserialize)]
struct SecretsExchangeRequest {
    public_key: String,
}

async fn post_move_secrets(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    AxumPath(ticket): AxumPath<String>,
    Json(body): Json<SecretsExchangeRequest>,
) -> Response {
    let Some(auth) = hub.auth_bearer(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing device token");
    };
    {
        let mut guard = hub.migrate.ticket.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = guard.as_mut() else {
            return err(StatusCode::NOT_FOUND, "no active move ticket");
        };
        if active.ticket != ticket {
            return err(StatusCode::NOT_FOUND, "unknown move ticket");
        }
        if active.target_device_id != auth.device_id {
            return err(StatusCode::FORBIDDEN, "ticket belongs to another device");
        }
        if now() > active.expires_at {
            return err(StatusCode::GONE, "move ticket expired");
        }
        if active.secrets_consumed {
            return err(StatusCode::GONE, "secrets ticket was already used");
        }
        active.secrets_consumed = true;
    }

    let inject = hub.migrate.config().inject_failure;
    let master_key = if inject == Some(InjectedFailure::CorruptSecrets) {
        STANDARD.encode([0xAAu8; 32])
    } else {
        match read_master_key_b64(&hub.data_dir) {
            Ok(k) => k,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("reading master key: {e:#}")),
        }
    };
    let identity_key = read_identity_key(&hub.data_dir, &hub.db);
    let tunnel_token = match hub.db.tunnel_token() {
        Ok(tok) => tok,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("reading tunnel token: {e:#}")),
    };
    let secrets = MoveSecrets { master_key, identity_key, tunnel_token };
    match encrypt_secrets(&body.public_key, &secrets) {
        Ok(env) => Json(env).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("encrypting secrets failed: {e:#}")),
    }
}

#[derive(Deserialize)]
struct MoveStatusReport {
    stage: String,
    #[serde(default)]
    progress_pct: Option<u8>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

async fn post_move_status(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    AxumPath(ticket): AxumPath<String>,
    Json(body): Json<MoveStatusReport>,
) -> Response {
    let Some(auth) = hub.auth_bearer(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing device token");
    };
    let tx = {
        let guard = hub.migrate.ticket.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = guard.as_ref() else {
            return err(StatusCode::NOT_FOUND, "no active move ticket");
        };
        if active.ticket != ticket {
            return err(StatusCode::NOT_FOUND, "unknown move ticket");
        }
        if active.target_device_id != auth.device_id {
            return err(StatusCode::FORBIDDEN, "ticket belongs to another device");
        }
        active.status_tx.clone()
    };
    let _ = tx.send(TargetUpdate {
        stage: body.stage,
        progress_pct: body.progress_pct,
        error: body.error,
        target_url: body.target_url,
    });
    Json(json!({"ok": true})).into_response()
}

// ------------------------------------------------------------------ target-side move executor

/// Configuration for the target device executing an incoming hub move.
#[derive(Clone, Debug)]
pub struct TargetMoveOptions {
    pub data_dir: PathBuf,
    pub binds: Vec<SocketAddr>,
    /// Public address to poll (in addition to local loopback) when confirming the switch; when `None`, confirms against our own bound address.
    pub confirm_address: Option<String>,
    pub confirm_timeout: Duration,
    pub poll_interval: Duration,
    /// Optional fault injection for rollback testing.
    pub inject_failure: Option<InjectedFailure>,
}

impl TargetMoveOptions {
    pub fn new(data_dir: PathBuf, binds: Vec<SocketAddr>) -> Self {
        Self {
            data_dir,
            binds,
            confirm_address: None,
            confirm_timeout: Duration::from_secs(120),
            poll_interval: Duration::from_secs(1),
            inject_failure: None,
        }
    }
}

/// Incoming move offer details (mirrors `core_protocol::HubMessage::HubMoveOffer`).
#[derive(Clone, Debug)]
pub struct MoveOffer {
    pub move_id: String,
    pub ticket: String,
    pub target_device_id: String,
    pub current_epoch: u64,
    pub snapshot_bytes: u64,
    pub snapshot_sha256: String,
}

/// Commit instruction received from the old hub (`core_protocol::HubMessage::HubMoveCommit`).
#[derive(Clone, Debug)]
pub struct MoveCommit {
    pub move_id: String,
    pub ticket: String,
    pub target_device_id: String,
    pub new_epoch: u64,
}

/// Execute the target side of a hub move:
/// 1. Download snapshot archive with one-time ticket and verify SHA-256 into `<data>/incoming-<ts>`.
/// 2. Generate ephemeral X25519 keypair, fetch encrypted secrets, store in staging, and run
///    [`crate::backup::restore`] into staging (verifying every manifest hash + provider key decryption).
/// 3. Report `ready` and wait for `commit_rx`.
/// 4. On commit: install staged data into `opts.data_dir`, reassign `hub_device_id` to
///    `target_device_id` (`epoch + 1`), start the new hub paused, confirm `/v1/hub`, unpause, and
///    report `switched`.
///
/// Any failure before commit cleans up all staging files and leaves `opts.data_dir` untouched.
pub async fn execute_target_move(
    old_hub_url: &str,
    device_token: &str,
    offer: MoveOffer,
    opts: TargetMoveOptions,
    commit_rx: oneshot::Receiver<Result<MoveCommit, String>>,
) -> Result<HubHandle> {
    let base = old_hub_url.trim_end_matches('/');
    let client = http_client();

    let report_status = |stage: &str, progress_pct: Option<u8>, error: Option<String>, target_url: Option<String>| {
        let client = client.clone();
        let url = format!("{base}/v1/hub/move/{}/status", offer.ticket);
        let token = device_token.to_string();
        let body = json!({
            "stage": stage,
            "progress_pct": progress_pct,
            "error": error,
            "target_url": target_url,
        });
        async move {
            let _ = client.post(url).bearer_auth(&token).json(&body).send().await;
        }
    };

    fs::create_dir_all(&opts.data_dir).with_context(|| format!("creating {}", opts.data_dir.display()))?;
    let stamp = format!("{}-{}", now(), random_hex(4));
    let incoming_dir = opts.data_dir.join(format!("incoming-{stamp}"));
    let archive_parent = opts.data_dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(&opts.data_dir);
    let archive_path = archive_parent.join(format!(".incoming-{stamp}.tar.gz"));

    struct StagingCleanup {
        incoming_dir: PathBuf,
        archive_path: PathBuf,
    }
    impl Drop for StagingCleanup {
        fn drop(&mut self) {
            let _ = crate::secrets::remove_master_key(&self.incoming_dir);
            let _ = fs::remove_dir_all(&self.incoming_dir);
            let _ = fs::remove_file(&self.archive_path);
        }
    }
    let _cleanup = StagingCleanup {
        incoming_dir: incoming_dir.clone(),
        archive_path: archive_path.clone(),
    };

    // ---- Step 3: Download snapshot archive and verify SHA-256
    report_status("copying", Some(25), None, None).await;
    let snap_resp = match client
        .get(format!("{base}/v1/hub/move/{}/snapshot", offer.ticket))
        .bearer_auth(device_token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("downloading snapshot failed: {e}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    };
    if !snap_resp.status().is_success() {
        let msg = format!("snapshot download returned HTTP {}", snap_resp.status());
        report_status("failed", None, Some(msg.clone()), None).await;
        bail!("{msg}");
    }
    let snap_bytes = match snap_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("reading snapshot body failed: {e}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    };
    let actual_sha = hex::encode(Sha256::digest(&snap_bytes));
    if actual_sha != offer.snapshot_sha256 {
        let msg = format!(
            "snapshot SHA-256 mismatch: expected {}, got {actual_sha}",
            offer.snapshot_sha256
        );
        report_status("failed", None, Some(msg.clone()), None).await;
        bail!("{msg}");
    }
    fs::write(&archive_path, &snap_bytes)
        .with_context(|| format!("writing staged archive {}", archive_path.display()))?;
    report_status("copying", Some(60), None, None).await;

    // ---- Step 4: Secrets hand-over (ephemeral X25519) + staged restore verification
    let keypair = TargetKeyPair::generate()?;
    let sec_resp = match client
        .post(format!("{base}/v1/hub/move/{}/secrets", offer.ticket))
        .bearer_auth(device_token)
        .json(&json!({ "public_key": keypair.public_key_b64() }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("secrets hand-over request failed: {e}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    };
    if !sec_resp.status().is_success() {
        let msg = format!("secrets hand-over returned HTTP {}", sec_resp.status());
        report_status("failed", None, Some(msg.clone()), None).await;
        bail!("{msg}");
    }
    let envelope: EncryptedSecretsEnvelope = match sec_resp.json().await {
        Ok(e) => e,
        Err(e) => {
            let msg = format!("decoding secrets envelope failed: {e}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    };
    let secrets = match keypair.decrypt_secrets(&envelope) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("decrypting secrets failed: {e:#}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    };

    // Stage master key & run the full existing restore verification inside `incoming_dir`
    // (touches nothing in `opts.data_dir` yet!).
    fs::create_dir_all(&incoming_dir)?;
    crate::secrets::write_master_key_b64(&incoming_dir, &secrets.master_key)?;
    if let Some(id_key) = &secrets.identity_key {
        write_private_file(&incoming_dir.join(IDENTITY_KEY_NAME), id_key)?;
    }

    let stage_dir = incoming_dir.clone();
    let stage_archive = archive_path.clone();
    let verify_res = tokio::task::spawn_blocking(move || {
        crate::backup::restore(&stage_dir, &stage_archive, false)
    })
    .await;
    match verify_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let msg = format!("staged restore verification failed: {e:#}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
        Err(e) => {
            let msg = format!("restore verification task panicked: {e}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    }

    // Report ready and wait for commit from old hub.
    report_status("ready", Some(85), None, None).await;
    let commit = match commit_rx.await {
        Ok(Ok(c)) => c,
        Ok(Err(reason)) => bail!("move aborted by old hub: {reason}"),
        Err(_) => bail!("move cancelled before commit"),
    };

    // ---- Step 5: Install staged data, bump hub_device_id + epoch, start paused, confirm, unpause
    if opts.inject_failure == Some(InjectedFailure::SkipSwitchConfirm) {
        // Simulate a target that never confirms; wait briefly for old hub's epoch + 2 rollback,
        // verify it rolled back, and discard our staged copy without taking any writes.
        let deadline = tokio::time::Instant::now() + opts.confirm_timeout + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if let Ok(info) = fetch_hub_info(&client, base).await
                && info.epoch > commit.new_epoch
            {
                bail!(
                    "target skipped confirmation; old hub rolled back to epoch {}",
                    info.epoch
                );
            }
            tokio::time::sleep(opts.poll_interval).await;
        }
        bail!("target skipped confirmation");
    }

    let target_dir = opts.data_dir.clone();
    let install_archive = archive_path.clone();
    let remove_incoming = incoming_dir.clone();
    let target_dev = commit.target_device_id.clone();
    let expected_epoch = commit.new_epoch;
    let install_secrets = secrets.clone();

    let install_res = tokio::task::spawn_blocking(move || -> Result<()> {
        // Remove staging directory before restoring into `target_dir` so restore doesn't move it aside.
        let _ = crate::secrets::remove_master_key(&remove_incoming);
        let _ = fs::remove_dir_all(&remove_incoming);
        crate::secrets::write_master_key_b64(&target_dir, &install_secrets.master_key)?;
        if let Some(id_key) = &install_secrets.identity_key {
            write_private_file(&target_dir.join(IDENTITY_KEY_NAME), id_key)?;
        }
        crate::backup::restore(&target_dir, &install_archive, true)?;
        let db = Db::open(&target_dir)?;
        let epoch = db.set_hub_device_id(&target_dev)?;
        if epoch != expected_epoch {
            db.c().execute(
                "UPDATE hub_state SET hub_device_id = ?1, epoch = ?2 WHERE id = 1",
                params![target_dev, expected_epoch as i64],
            )?;
        }
        let _ = db.c().execute(
            "INSERT OR REPLACE INTO hub_meta (key, value) VALUES ('hub_since', ?1)",
            params![now().to_string()],
        );
        if let Some(tok) = &install_secrets.tunnel_token {
            db.set_tunnel_token(tok)?;
        }
        drop(db);
        Ok(())
    })
    .await;

    match install_res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let msg = format!("installing staged hub data failed: {e:#}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
        Err(e) => {
            let msg = format!("install task panicked: {e}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    }

    let handle = match host::start_paused(&opts.data_dir, &opts.binds).await {
        Ok(h) => h,
        Err(e) => {
            let msg = format!("starting new hub failed: {e:#}");
            report_status("failed", None, Some(msg.clone()), None).await;
            bail!("{msg}");
        }
    };

    let self_url = format!("http://{}", handle.binds()[0]);
    let confirm_url = opts
        .confirm_address
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string());

    // Poll local loopback (and `confirm_address` when set) until we see our own
    // `target_device_id` at `epoch >= new_epoch`, or abort if a higher epoch (`epoch + 2` rollback) appears.
    let deadline = tokio::time::Instant::now() + opts.confirm_timeout;
    let mut loopback_confirmed = false;
    let mut confirmed = false;
    while tokio::time::Instant::now() < deadline {
        if !loopback_confirmed
            && let Ok(local_info) = fetch_hub_info(&client, &self_url).await
            && local_info.hub_device_id == commit.target_device_id
            && local_info.epoch >= commit.new_epoch
        {
            loopback_confirmed = true;
        }
        if loopback_confirmed {
            match &confirm_url {
                None => {
                    confirmed = true;
                    break;
                }
                Some(url) if url == &self_url => {
                    confirmed = true;
                    break;
                }
                Some(url) => {
                    if let Ok(info) = fetch_hub_info(&client, url).await {
                        if info.hub_device_id == commit.target_device_id && info.epoch >= commit.new_epoch {
                            confirmed = true;
                            break;
                        }
                        if info.hub_device_id != commit.target_device_id && info.epoch > commit.new_epoch {
                            handle.stop().await;
                            bail!(
                                "old hub rolled back with higher epoch {} (ours was {}); discarding target copy",
                                info.epoch,
                                commit.new_epoch
                            );
                        }
                    }
                }
            }
        }
        tokio::time::sleep(opts.poll_interval).await;
    }

    if !confirmed {
        handle.stop().await;
        let check_url = confirm_url.as_deref().unwrap_or(&self_url);
        let msg = format!("new hub did not confirm at {check_url} within {:?}", opts.confirm_timeout);
        report_status("failed", None, Some(msg.clone()), None).await;
        bail!("{msg}");
    }

    // Confirmed: unpause the new hub and notify old hub of our URL for immediate step-down.
    if let Some(new_hub) = handle.hub() {
        new_hub.record_hub_since(now());
        new_hub.migrate().set_paused(false);
    }
    let switched_url = confirm_url.unwrap_or(self_url);
    report_status("switched", Some(100), None, Some(switched_url)).await;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_protocol::{NodeHostInfo, ProviderConfig};
    use crate::google_auth::Owner;

    async fn setup_test_owner(hub: &Hub, sub: &str) {
        crate::google_auth::tests::configure_hub_for_test(hub).await;
        hub.db
            .set_owner_if_none(&Owner {
                provider: "google".into(),
                sub: sub.into(),
                email: format!("{sub}@example.com"),
                name: "Test Owner".into(),
                claimed_at: now(),
            })
            .unwrap();
    }

    async fn fetch_nonce(http: &reqwest::Client, base: &str) -> String {
        let resp = http.post(format!("{base}/v1/auth/google/nonce")).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        body["nonce"].as_str().unwrap().to_string()
    }

    async fn fresh_owner_token(http: &reqwest::Client, base: &str, sub: &str) -> String {
        let nonce = fetch_nonce(http, base).await;
        crate::google_auth::tests::mint_for_test(&nonce, sub)
    }

    #[test]
    fn secrets_x25519_encryption_round_trips_and_rejects_tampering_or_wrong_key() {
        let target = TargetKeyPair::generate().unwrap();
        let other = TargetKeyPair::generate().unwrap();
        let plain = MoveSecrets {
            master_key: STANDARD.encode([42u8; 32]),
            identity_key: Some("ed25519-identity-key-test".into()),
            tunnel_token: Some("cf-tunnel-secret-token".into()),
        };

        let env = encrypt_secrets(&target.public_key_b64(), &plain).unwrap();
        assert_eq!(env.algorithm, SECRETS_ALGORITHM);
        assert!(!env.ciphertext.contains("cf-tunnel-secret-token"));
        assert_eq!(target.decrypt_secrets(&env).unwrap(), plain);

        // Wrong recipient key fails.
        assert!(other.decrypt_secrets(&env).is_err());

        // Tampered ciphertext fails authentication.
        let mut tampered = env.clone();
        let mut raw = STANDARD.decode(&tampered.ciphertext).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        tampered.ciphertext = STANDARD.encode(raw);
        assert!(target.decrypt_secrets(&tampered).is_err());
    }

    #[tokio::test]
    async fn pause_guard_refuses_writes_with_503_and_allows_reads_and_move_routes() {
        let dir = std::env::temp_dir().join(format!("grouper-migrate-pause-{}", random_hex(4)));
        let handle = host::start(&dir, &["127.0.0.1:0".parse().unwrap()]).await.unwrap();
        let hub = handle.hub().unwrap();
        let (_admin, admin_token) = hub.db.add_device("Admin", "windows", true).unwrap();
        let base = format!("http://{}", handle.binds()[0]);
        let http = reqwest::Client::new();

        // While unpaused, writes work normally.
        let ok_mem = http
            .post(format!("{base}/v1/memories"))
            .bearer_auth(&admin_token)
            .json(&json!({"content": "before pause"}))
            .send()
            .await
            .unwrap();
        assert_eq!(ok_mem.status(), reqwest::StatusCode::OK);

        // Pause for move: mutating requests return 503 "Your hub is moving", reads still work.
        hub.migrate().set_paused(true);
        let paused_mem = http
            .post(format!("{base}/v1/memories"))
            .bearer_auth(&admin_token)
            .json(&json!({"content": "during pause"}))
            .send()
            .await
            .unwrap();
        assert_eq!(paused_mem.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = paused_mem.json().await.unwrap();
        assert_eq!(body["error"], "Your hub is moving");

        // GET /v1/memories and GET /v1/hub/move still work while paused.
        let read_mem = http
            .get(format!("{base}/v1/memories"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap();
        assert_eq!(read_mem.status(), reqwest::StatusCode::OK);
        let move_status: MoveStatus = http
            .get(format!("{base}/v1/hub/move"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(move_status.state, MovePhase::Idle);

        // Unpause: writes resume immediately.
        hub.migrate().set_paused(false);
        let resumed_mem = http
            .post(format!("{base}/v1/memories"))
            .bearer_auth(&admin_token)
            .json(&json!({"content": "after pause"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resumed_mem.status(), reqwest::StatusCode::OK);

        handle.stop().await;
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn preflight_rejects_offline_ineligible_older_version_and_low_disk_targets() {
        let dir = std::env::temp_dir().join(format!("grouper-migrate-preflight-{}", random_hex(4)));
        let handle = host::start(&dir, &["127.0.0.1:0".parse().unwrap()]).await.unwrap();
        let hub = handle.hub().unwrap();
        setup_test_owner(&hub, "sub-preflight").await;
        let (hub_dev_id, _) = hub.db.ensure_hub_device().unwrap();
        let (_admin, admin_token) = hub.db.add_device("Admin", "windows", true).unwrap();
        let (phone, _) = hub.db.add_device("iPhone", "ios", false).unwrap();
        let (offline_mac, _) = hub.db.add_device("Offline Mac", "macos", false).unwrap();
        hub.record_host_info(
            &offline_mac.id,
            NodeHostInfo { can_host: true, app_version: HUB_VERSION.into(), free_bytes: Some(1_000_000_000), cloudflared: true },
        );
        let (old_mac, old_mac_token) = hub.db.add_device("Old Mac", "macos", false).unwrap();
        let (tight_pc, tight_pc_token) = hub.db.add_device("Tight PC", "windows", false).unwrap();

        let base = format!("http://{}", handle.binds()[0]);
        let ws_addr = handle.binds()[0];

        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let connect = |token: String, host: NodeHostInfo| async move {
            let mut req = format!("ws://{ws_addr}{}", core_protocol::CONNECT_PATH).into_client_request().unwrap();
            req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            let hello = core_protocol::NodeMessage::Hello {
                protocol_version: core_protocol::PROTOCOL_VERSION,
                client_version: host.app_version.clone(),
                host: Some(host),
            };
            ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap().into())).await.unwrap();
            while let Some(Ok(WsMsg::Text(t))) = ws.next().await {
                if matches!(serde_json::from_str::<HubMessage>(&t), Ok(HubMessage::Welcome { .. })) {
                    break;
                }
            }
            ws
        };

        let _old_ws = connect(
            old_mac_token,
            NodeHostInfo { can_host: true, app_version: "0.0.1".into(), free_bytes: Some(1_000_000_000), cloudflared: true },
        )
        .await;
        let _tight_ws = connect(
            tight_pc_token,
            NodeHostInfo { can_host: true, app_version: HUB_VERSION.into(), free_bytes: Some(1), cloudflared: true },
        )
        .await;

        let http = reqwest::Client::new();
        for (target, expected_substr) in [
            (hub_dev_id.as_str(), "already the hub"),
            (phone.id.as_str(), "cannot host"),
            (offline_mac.id.as_str(), "offline"),
            (old_mac.id.as_str(), "Update Grouper"),
            (tight_pc.id.as_str(), "free disk space"),
        ] {
            let id_token = fresh_owner_token(&http, &base, "sub-preflight").await;
            let resp = http
                .post(format!("{base}/v1/hub/move"))
                .bearer_auth(&admin_token)
                .json(&json!({"target_device": target, "id_token": id_token}))
                .send()
                .await
                .unwrap();
            assert!(!resp.status().is_success(), "target {target} should fail preflight");
            let body: serde_json::Value = resp.json().await.unwrap();
            let err_str = body["error"].as_str().unwrap_or("");
            assert!(err_str.contains(expected_substr), "target {target}: expected {expected_substr:?} in {err_str:?}");
            // Preflight failures never pause or change state.
            assert!(!hub.is_move_paused());
            assert_eq!(hub.migrate().status().state, MovePhase::Idle);
        }

        handle.stop().await;
        fs::remove_dir_all(&dir).ok();
    }

    /// Helper: start a real hub + connect a target node via `core_client::run_with_host` that drives
    /// `execute_target_move`, and verify both full move and forced-failure rollbacks at every step.
    struct TestTargetEvents {
        offer_tx: Mutex<Option<oneshot::Sender<MoveOffer>>>,
        commit_tx: Mutex<Option<oneshot::Sender<Result<MoveCommit, String>>>>,
    }

    impl core_client::Events for TestTargetEvents {
        fn status(&self, _status: core_client::Status) {}
        fn presence(&self, _devices: Vec<core_protocol::DeviceInfo>) {}
        fn hub_move_offer(&self, offer: core_client::HubMoveOfferInfo) {
            if let Some(tx) = self.offer_tx.lock().unwrap().take() {
                let _ = tx.send(MoveOffer {
                    move_id: offer.move_id,
                    ticket: offer.ticket,
                    target_device_id: offer.target_device_id,
                    current_epoch: offer.current_epoch,
                    snapshot_bytes: offer.snapshot_bytes,
                    snapshot_sha256: offer.snapshot_sha256,
                });
            }
        }
        fn hub_move_commit(&self, commit: core_client::HubMoveCommitInfo) {
            if let Some(tx) = self.commit_tx.lock().unwrap().take() {
                let _ = tx.send(Ok(MoveCommit {
                    move_id: commit.move_id,
                    ticket: commit.ticket,
                    target_device_id: commit.target_device_id,
                    new_epoch: commit.new_epoch,
                }));
            }
        }
        fn hub_move_abort(&self, _move_id: &str, reason: &str) {
            if let Some(tx) = self.commit_tx.lock().unwrap().take() {
                let _ = tx.send(Err(reason.to_string()));
            }
        }
    }

    struct NoopTools;
    impl core_client::ToolHandler for NoopTools {
        fn call(&self, _tool: &str, _args: &str) -> Result<String, String> {
            Ok("{}".into())
        }
    }

    /// Full two-hub-in-one-process integration test:
    /// 1. Old hub has an owner, sealed provider key, sealed tunnel token, memory, and an existing
    ///    admin device token plus a connected eligible target node.
    /// 2. Admin starts `POST /v1/hub/move` -> snapshot + X25519 secrets hand-over + switch.
    /// 3. New hub comes up in the same process with `hub_device_id == target_id` and `epoch == 2`.
    /// 4. Old hub steps down (`old_handle.is_stepped_down() == true`) and reports `state == done`.
    /// 5. Existing device token, sealed provider key, sealed tunnel token, and memories all work
    ///    against the new hub without signing in again.
    #[tokio::test]
    async fn two_hubs_move_succeeds_old_hub_steps_down_and_existing_token_still_works() {
        let root = std::env::temp_dir().join(format!("grouper-two-hub-move-{}", random_hex(4)));
        let old_dir = root.join("old-hub");
        let new_dir = root.join("new-hub");
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let old_handle = host::start(&old_dir, &[loopback]).await.unwrap();
        let old_hub = old_handle.hub().unwrap();
        crate::google_auth::tests::configure_hub_for_test(&old_hub).await;
        old_hub.migrate().set_config(MigrateConfig {
            confirm_timeout: Duration::from_secs(10),
            copy_timeout: Duration::from_secs(15),
            poll_interval: Duration::from_millis(30),
            inject_failure: None,
        });

        // Set owner, sealed provider key, sealed tunnel token, and a memory on old hub.
        old_hub
            .db
            .set_owner_if_none(&Owner {
                provider: "google".into(),
                sub: "sub-oscar".into(),
                email: "oscar@example.com".into(),
                name: "Oscar".into(),
                claimed_at: now(),
            })
            .unwrap();
        old_hub
            .db
            .upsert_provider(&ProviderConfig {
                id: "openai".into(),
                kind: "openai".into(),
                base_url: None,
                api_key: Some("sk-super-secret-provider-key".into()),
                options: json!({}),
            })
            .unwrap();
        old_hub.db.set_tunnel_token("cf-secret-tunnel-token-123").unwrap();
        old_hub.db.add_memory("remember this across hub move", &["move".into()], None).unwrap();

        let (old_hub_dev_id, old_epoch) = old_hub.db.ensure_hub_device().unwrap();
        assert_eq!(old_epoch, 1);

        // Existing client device token (e.g. Oscar's iPhone) that must keep working after the move.
        let (_phone, phone_admin_token) = old_hub.db.add_device("Oscar's iPhone", "ios", true).unwrap();

        // Target Mac install (node row) that will become the new hub.
        let mac_install = "target_mac_install_12345";
        let mac_node = old_hub
            .db
            .issue_device(
                "Oscar's Mac",
                "macos",
                false,
                Some(crate::db::Install { id: mac_install, kind: crate::db::NODE_INSTALL }),
                None,
            )
            .unwrap();
        let target_id = mac_node.device.id.clone();

        let old_url = format!("http://{}", old_handle.binds()[0]);

        // Connect target node using real core-client with NodeHostInfo.
        let (offer_tx, offer_rx) = oneshot::channel();
        let (commit_tx, commit_rx) = oneshot::channel();
        let events = Arc::new(TestTargetEvents {
            offer_tx: Mutex::new(Some(offer_tx)),
            commit_tx: Mutex::new(Some(commit_tx)),
        });
        let (node_stop_tx, node_stop_rx) = watch::channel(false);
        let client_cfg = core_client::Config {
            hub_url: old_url.clone(),
            token: mac_node.token.clone(),
            capabilities: vec![],
            workspace_root: None,
            allow_shell: false,
            mcp_config: None,
            folders: vec![],
            usage_probes: false,
        };
        let target_host_info = NodeHostInfo {
            can_host: true,
            app_version: HUB_VERSION.into(),
            free_bytes: Some(10_000_000_000),
            cloudflared: true,
        };
        let node_task = tokio::spawn(core_client::run_with_host(
            client_cfg,
            Some(target_host_info),
            Arc::new(NoopTools),
            events,
            node_stop_rx,
        ));

        // Wait until old hub sees the target node online & selectable.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if old_hub.is_online(&target_id).await
                    && old_hub.host_info_for(&target_id).is_some_and(|h| h.can_host)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("target node connected");

        // Spawn target move runner waiting for the WS offer.
        let old_url_for_target = old_url.clone();
        let target_token = mac_node.token.clone();
        let new_dir_clone = new_dir.clone();
        let target_runner = tokio::spawn(async move {
            let offer = tokio::time::timeout(Duration::from_secs(10), offer_rx)
                .await
                .expect("received move offer")
                .unwrap();
            let mut opts = TargetMoveOptions::new(new_dir_clone, vec![loopback]);
            opts.confirm_timeout = Duration::from_secs(10);
            opts.poll_interval = Duration::from_millis(30);
            execute_target_move(&old_url_for_target, &target_token, offer, opts, commit_rx).await
        });

        // Start the move via admin HTTP API.
        let http = reqwest::Client::new();
        let id_token = fresh_owner_token(&http, &old_url, "sub-oscar").await;
        let start_resp = http
            .post(format!("{old_url}/v1/hub/move"))
            .bearer_auth(&phone_admin_token)
            .json(&json!({
                "target_device": target_id,
                "id_token": id_token,
                "threads": "stop"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(start_resp.status(), reqwest::StatusCode::ACCEPTED);

        // Wait for the new hub handle to come up and old hub to finish stepping down.
        let new_handle = tokio::time::timeout(Duration::from_secs(15), target_runner)
            .await
            .expect("target move finished")
            .unwrap()
            .expect("target move succeeded");

        let mut old_move_rx = old_hub.migrate().subscribe();
        tokio::time::timeout(Duration::from_secs(10), old_move_rx.wait_for(|s| s.state == MovePhase::Done))
            .await
            .expect("old hub reached Done")
            .unwrap();

        // Verify old hub stepped down and became an ordinary device.
        assert!(old_handle.is_stepped_down(), "old hub must step down after switch");
        let stepdown_info = old_hub.stepdown().info().unwrap();
        assert_eq!(stepdown_info.peer_hub_device_id, target_id);
        assert_eq!(stepdown_info.peer_epoch, 2);
        assert_eq!(stepdown_info.local_hub_device_id, old_hub_dev_id);

        // Verify new hub identity: same hub_id, hub_device_id == target_id, epoch == 2.
        let new_url = format!("http://{}", new_handle.binds()[0]);
        let new_info: core_protocol::HubInfo = http
            .get(format!("{new_url}/v1/hub"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(new_info.hub_id, old_hub.db.hub_id());
        assert_eq!(new_info.account.as_deref(), Some(crate::server::account_id("google", "sub-oscar").as_str()));
        assert_eq!(new_info.hub_device_id, target_id);
        assert_eq!(new_info.epoch, 2);

        // Verify existing phone admin token still works against the NEW hub (both reads and writes!).
        let memories: Vec<core_protocol::MemoryEntry> = http
            .get(format!("{new_url}/v1/memories"))
            .bearer_auth(&phone_admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(memories.iter().any(|m| m.content == "remember this across hub move"));

        let write_on_new = http
            .post(format!("{new_url}/v1/memories"))
            .bearer_auth(&phone_admin_token)
            .json(&json!({"content": "written on new hub"}))
            .send()
            .await
            .unwrap();
        assert_eq!(write_on_new.status(), reqwest::StatusCode::OK);

        // Verify sealed provider key and sealed tunnel token decrypt cleanly on the new hub.
        let new_hub = new_handle.hub().unwrap();
        let providers = new_hub.db.providers().unwrap();
        let openai = providers.iter().find(|p| p.id == "openai").unwrap();
        assert_eq!(openai.api_key.as_deref(), Some("sk-super-secret-provider-key"));
        assert_eq!(new_hub.db.tunnel_token().unwrap().as_deref(), Some("cf-secret-tunnel-token-123"));

        // Verify staging files were cleaned up on new hub.
        let leftovers: Vec<_> = fs::read_dir(&new_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("incoming-"))
            .collect();
        assert!(leftovers.is_empty(), "staging files should be cleaned up: {leftovers:?}");

        let _ = node_stop_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), node_task).await;
        new_handle.stop().await;
        old_handle.stop().await;
        fs::remove_dir_all(&root).ok();
    }

    /// Forced failure at every stage of the move rolls back cleanly and resumes the old hub:
    /// 1. Preparing (`BeforeSnapshot`): old hub unpauses, epoch stays 1, writes resume.
    /// 2. Copying (`CorruptSnapshot`): target catches SHA-256 mismatch, deletes staging, old hub
    ///    unpauses, epoch stays 1, writes resume.
    /// 3. Secrets (`CorruptSecrets`): target fails provider-key decryption restore check, deletes
    ///    staging, target_data_dir untouched, old hub unpauses, epoch stays 1, writes resume.
    /// 4. Switching (`SkipSwitchConfirm`): target doesn't confirm in time, old hub rolls back to
    ///    `epoch + 2` (epoch 3), unpauses, resumes serving, and target sees higher epoch and discards.
    #[tokio::test]
    async fn forced_failure_at_each_step_rolls_back_and_old_hub_resumes() {
        let root = std::env::temp_dir().join(format!("grouper-move-rollback-{}", random_hex(4)));
        let old_dir = root.join("old-hub");
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let old_handle = host::start(&old_dir, &[loopback]).await.unwrap();
        let old_hub = old_handle.hub().unwrap();
        crate::google_auth::tests::configure_hub_for_test(&old_hub).await;
        old_hub
            .db
            .set_owner_if_none(&Owner {
                provider: "google".into(),
                sub: "sub-rollback".into(),
                email: "oscar@example.com".into(),
                name: "Oscar".into(),
                claimed_at: now(),
            })
            .unwrap();
        old_hub
            .db
            .upsert_provider(&ProviderConfig {
                id: "openai".into(),
                kind: "openai".into(),
                base_url: None,
                api_key: Some("sk-must-decrypt-with-real-key".into()),
                options: json!({}),
            })
            .unwrap();

        let (orig_hub_dev_id, orig_epoch) = old_hub.db.ensure_hub_device().unwrap();
        assert_eq!(orig_epoch, 1);
        let (_admin, admin_token) = old_hub.db.add_device("Admin", "windows", true).unwrap();
        let (target_dev, target_token) = old_hub.db.add_device("Target Mac", "macos", false).unwrap();
        let old_url = format!("http://{}", old_handle.binds()[0]);
        let http = reqwest::Client::new();

        for (step_name, failure, expected_epoch_after) in [
            ("preparing", InjectedFailure::BeforeSnapshot, 1u64),
            ("copying", InjectedFailure::CorruptSnapshot, 1u64),
            ("secrets", InjectedFailure::CorruptSecrets, 1u64),
            ("switching", InjectedFailure::SkipSwitchConfirm, 3u64),
        ] {
            let target_dir = root.join(format!("target-{step_name}"));
            old_hub.migrate().set_config(MigrateConfig {
                confirm_timeout: Duration::from_secs(2),
                copy_timeout: Duration::from_secs(10),
                poll_interval: Duration::from_millis(25),
                inject_failure: Some(failure),
            });

            let (offer_tx, offer_rx) = oneshot::channel();
            let (commit_tx, commit_rx) = oneshot::channel();
            let events = Arc::new(TestTargetEvents {
                offer_tx: Mutex::new(Some(offer_tx)),
                commit_tx: Mutex::new(Some(commit_tx)),
            });
            let (node_stop_tx, node_stop_rx) = watch::channel(false);
            let client_cfg = core_client::Config {
                hub_url: old_url.clone(),
                token: target_token.clone(),
                capabilities: vec![],
                workspace_root: None,
                allow_shell: false,
                mcp_config: None,
                folders: vec![],
                usage_probes: false,
            };
            let host_info = NodeHostInfo {
                can_host: true,
                app_version: HUB_VERSION.into(),
                free_bytes: Some(10_000_000_000),
                cloudflared: true,
            };
            let node_task = tokio::spawn(core_client::run_with_host(
                client_cfg,
                Some(host_info),
                Arc::new(NoopTools),
                events,
                node_stop_rx,
            ));

            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if old_hub.is_online(&target_dev.id).await
                        && old_hub.host_info_for(&target_dev.id).is_some_and(|h| h.can_host)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("target connected");

            let old_url_t = old_url.clone();
            let target_tok_t = target_token.clone();
            let target_dir_t = target_dir.clone();
            let target_task = tokio::spawn(async move {
                let Ok(Ok(offer)) = tokio::time::timeout(Duration::from_secs(10), offer_rx).await else {
                    return Err(anyhow!("no offer (failed before copy)"));
                };
                let mut opts = TargetMoveOptions::new(target_dir_t, vec![loopback]);
                opts.confirm_timeout = Duration::from_secs(2);
                opts.poll_interval = Duration::from_millis(25);
                if failure == InjectedFailure::SkipSwitchConfirm {
                    opts.inject_failure = Some(InjectedFailure::SkipSwitchConfirm);
                }
                execute_target_move(&old_url_t, &target_tok_t, offer, opts, commit_rx).await
            });

            let mut status_rx = old_hub.migrate().subscribe();
            let id_token = fresh_owner_token(&http, &old_url, "sub-rollback").await;
            let start = http
                .post(format!("{old_url}/v1/hub/move"))
                .bearer_auth(&admin_token)
                .json(&json!({"target_device": target_dev.id, "id_token": id_token, "threads": "stop"}))
                .send()
                .await
                .unwrap();
            assert_eq!(start.status(), reqwest::StatusCode::ACCEPTED, "step {step_name}");
            let started_status: MoveStatus = start.json().await.unwrap();
            let expected_move_id = started_status.move_id.clone();

            tokio::time::timeout(
                Duration::from_secs(10),
                status_rx.wait_for(|s| s.move_id == expected_move_id && s.state == MovePhase::RolledBack),
            )
            .await
            .unwrap_or_else(|_| panic!("step {step_name} did not roll back in time"))
            .unwrap();

            let target_res = tokio::time::timeout(Duration::from_secs(15), target_task)
                .await
                .expect("target task exited")
                .unwrap();
            assert!(target_res.is_err(), "target must fail for step {step_name}");

            // Old hub must NOT have stepped down, must be unpaused, and must keep original hub_device_id.
            assert!(!old_handle.is_stepped_down(), "step {step_name} must not step down");
            assert!(!old_hub.is_move_paused(), "step {step_name} must unpause old hub");
            assert_eq!(old_hub.db.hub_device_id(), orig_hub_dev_id, "step {step_name}");
            assert_eq!(old_hub.db.epoch(), expected_epoch_after, "step {step_name}");

            // Writes against old hub work again immediately after rollback.
            let write_after = http
                .post(format!("{old_url}/v1/memories"))
                .bearer_auth(&admin_token)
                .json(&json!({"content": format!("resumed after {step_name}")}))
                .send()
                .await
                .unwrap();
            assert_eq!(write_after.status(), reqwest::StatusCode::OK, "step {step_name} resumed writes");

            // Target directory must not have a live grouper.db or leftover incoming-* staging files.
            assert!(!target_dir.join("grouper.db").exists(), "step {step_name} must not install grouper.db");
            if target_dir.exists() {
                let leftovers: Vec<_> = fs::read_dir(&target_dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().starts_with("incoming-"))
                    .collect();
                assert!(leftovers.is_empty(), "step {step_name} left staging files: {leftovers:?}");
            }

            let _ = node_stop_tx.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(2), node_task).await;
            // Wait until old hub clears the disconnected node before starting the next scenario.
            tokio::time::timeout(Duration::from_secs(15), async {
                while old_hub.is_online(&target_dev.id).await {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("target disconnected cleanly");
        }

        old_handle.stop().await;
        fs::remove_dir_all(&root).ok();
    }

    /// Cancelling an active move before Switch rolls back cleanly and resumes the old hub;
    /// snapshot/secrets tickets reject wrong devices and reuse.
    #[tokio::test]
    async fn cancel_during_copy_rolls_back_and_tickets_are_single_use() {
        let root = std::env::temp_dir().join(format!("grouper-move-cancel-{}", random_hex(4)));
        let old_dir = root.join("old-hub");
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let old_handle = host::start(&old_dir, &[loopback]).await.unwrap();
        let old_hub = old_handle.hub().unwrap();
        setup_test_owner(&old_hub, "sub-cancel").await;
        old_hub.migrate().set_config(MigrateConfig {
            confirm_timeout: Duration::from_secs(5),
            copy_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(25),
            inject_failure: None,
        });

        let (_admin, admin_token) = old_hub.db.add_device("Admin", "windows", true).unwrap();
        let (target_dev, target_token) = old_hub.db.add_device("Target Mac", "macos", false).unwrap();
        let (_other, other_token) = old_hub.db.add_device("Other Mac", "macos", false).unwrap();
        let old_url = format!("http://{}", old_handle.binds()[0]);

        let (offer_tx, offer_rx) = oneshot::channel();
        let (commit_tx, _commit_rx) = oneshot::channel();
        let events = Arc::new(TestTargetEvents {
            offer_tx: Mutex::new(Some(offer_tx)),
            commit_tx: Mutex::new(Some(commit_tx)),
        });
        let (node_stop_tx, node_stop_rx) = watch::channel(false);
        let client_cfg = core_client::Config {
            hub_url: old_url.clone(),
            token: target_token.clone(),
            capabilities: vec![],
            workspace_root: None,
            allow_shell: false,
            mcp_config: None,
            folders: vec![],
            usage_probes: false,
        };
        let host_info = NodeHostInfo {
            can_host: true,
            app_version: HUB_VERSION.into(),
            free_bytes: Some(10_000_000_000),
            cloudflared: true,
        };
        let node_task = tokio::spawn(core_client::run_with_host(
            client_cfg,
            Some(host_info),
            Arc::new(NoopTools),
            events,
            node_stop_rx,
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if old_hub.is_online(&target_dev.id).await
                    && old_hub.host_info_for(&target_dev.id).is_some_and(|h| h.can_host)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("target connected");

        let http = reqwest::Client::new();
        let mut status_rx = old_hub.migrate().subscribe();
        let id_token = fresh_owner_token(&http, &old_url, "sub-cancel").await;
        let start = http
            .post(format!("{old_url}/v1/hub/move"))
            .bearer_auth(&admin_token)
            .json(&json!({"target_device": target_dev.id, "id_token": id_token, "threads": "stop"}))
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), reqwest::StatusCode::ACCEPTED);
        let offer = tokio::time::timeout(Duration::from_secs(5), offer_rx).await.unwrap().unwrap();

        // Another device cannot use this ticket (403 Forbidden).
        let wrong_dev = http
            .get(format!("{old_url}/v1/hub/move/{}/snapshot", offer.ticket))
            .bearer_auth(&other_token)
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_dev.status(), reqwest::StatusCode::FORBIDDEN);

        // First download succeeds, second download with same ticket is rejected (410 Gone).
        let first_snap = http
            .get(format!("{old_url}/v1/hub/move/{}/snapshot", offer.ticket))
            .bearer_auth(&target_token)
            .send()
            .await
            .unwrap();
        assert_eq!(first_snap.status(), reqwest::StatusCode::OK);
        let second_snap = http
            .get(format!("{old_url}/v1/hub/move/{}/snapshot", offer.ticket))
            .bearer_auth(&target_token)
            .send()
            .await
            .unwrap();
        assert_eq!(second_snap.status(), reqwest::StatusCode::GONE);

        // Cancel while in Copying -> rolls back and unpauses.
        let cancel = http
            .delete(format!("{old_url}/v1/hub/move"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), reqwest::StatusCode::OK);

        tokio::time::timeout(Duration::from_secs(5), status_rx.wait_for(|s| s.state == MovePhase::RolledBack))
            .await
            .expect("cancelled move rolled back")
            .unwrap();
        assert!(!old_hub.is_move_paused());
        assert_eq!(old_hub.db.epoch(), 1);

        let _ = node_stop_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), node_task).await;
        old_handle.stop().await;
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn post_hub_move_enforces_fresh_owner_google_signin() {
        let root = std::env::temp_dir().join(format!("grouper-move-fresh-auth-{}", random_hex(4)));
        let old_dir = root.join("hub");
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let handle = host::start(&old_dir, &[loopback]).await.unwrap();
        let hub = handle.hub().unwrap();
        setup_test_owner(&hub, "sub-oscar").await;
        hub.migrate().set_config(MigrateConfig {
            confirm_timeout: Duration::from_secs(5),
            copy_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(25),
            inject_failure: None,
        });

        let (_admin, admin_token) = hub.db.add_device("Admin", "windows", true).unwrap();
        let (target_dev, target_token) = hub.db.add_device("Target Mac", "macos", false).unwrap();
        let base = format!("http://{}", handle.binds()[0]);

        // Connect an eligible online target so only `id_token` decides whether `/v1/hub/move` accepts.
        let (offer_tx, _offer_rx) = oneshot::channel();
        let (commit_tx, _commit_rx) = oneshot::channel();
        let events = Arc::new(TestTargetEvents {
            offer_tx: Mutex::new(Some(offer_tx)),
            commit_tx: Mutex::new(Some(commit_tx)),
        });
        let (node_stop_tx, node_stop_rx) = watch::channel(false);
        let client_cfg = core_client::Config {
            hub_url: base.clone(),
            token: target_token,
            capabilities: vec![],
            workspace_root: None,
            allow_shell: false,
            mcp_config: None,
            folders: vec![],
            usage_probes: false,
        };
        let host_info = NodeHostInfo {
            can_host: true,
            app_version: HUB_VERSION.into(),
            free_bytes: Some(10_000_000_000),
            cloudflared: true,
        };
        let node_task = tokio::spawn(core_client::run_with_host(
            client_cfg,
            Some(host_info),
            Arc::new(NoopTools),
            events,
            node_stop_rx,
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if hub.is_online(&target_dev.id).await
                    && hub.host_info_for(&target_dev.id).is_some_and(|h| h.can_host)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("target connected");

        let http = reqwest::Client::new();
        let post_move = |payload: serde_json::Value| {
            http.post(format!("{base}/v1/hub/move"))
                .bearer_auth(&admin_token)
                .json(&payload)
                .send()
        };

        // 1. Missing or blank token -> 401 Unauthorized.
        for payload in [
            json!({"target_device": target_dev.id}),
            json!({"target_device": target_dev.id, "id_token": null}),
            json!({"target_device": target_dev.id, "id_token": "   "}),
        ] {
            let resp = post_move(payload).await.unwrap();
            assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
            assert!(!hub.is_move_paused());
            assert_eq!(hub.migrate().status().state, MovePhase::Idle);
        }

        // 2. Stale `iat` (>= 5 minutes old, even with valid signature, fresh hub nonce, and unexpired `exp`) -> 401.
        for age_secs in [300u64, 360u64] {
            let nonce = fetch_nonce(&http, &base).await;
            let stale_tok = crate::google_auth::tests::mint_for_test_at(&nonce, "sub-oscar", now() - age_secs);
            let resp = post_move(json!({"target_device": target_dev.id, "id_token": stale_tok})).await.unwrap();
            assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED, "age_secs={age_secs}");
            assert!(!hub.is_move_paused());
            assert_eq!(hub.migrate().status().state, MovePhase::Idle);
        }

        // 3. Reused nonce from `POST /v1/auth/google` -> 401 Unauthorized.
        let login_tok = fresh_owner_token(&http, &base, "sub-oscar").await;
        let signin_resp = http
            .post(format!("{base}/v1/auth/google"))
            .json(&json!({"id_token": login_tok, "client": "ios"}))
            .send()
            .await
            .unwrap();
        assert_eq!(signin_resp.status(), reqwest::StatusCode::OK);
        let replay_login = post_move(json!({"target_device": target_dev.id, "id_token": login_tok})).await.unwrap();
        assert_eq!(replay_login.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(!hub.is_move_paused());

        // 4. Wrong `sub` (valid signature + fresh hub nonce + fresh `iat`, another Google account) -> 403 Forbidden.
        let wrong_sub_tok = fresh_owner_token(&http, &base, "sub-stranger").await;
        let wrong_sub_resp = post_move(json!({"target_device": target_dev.id, "id_token": wrong_sub_tok})).await.unwrap();
        assert_eq!(wrong_sub_resp.status(), reqwest::StatusCode::FORBIDDEN);
        assert!(!hub.is_move_paused());
        assert_eq!(hub.migrate().status().state, MovePhase::Idle);

        // 5. Valid fresh owner token -> 202 Accepted, and replaying that same move token after cancel -> 401.
        let mut status_rx = hub.migrate().subscribe();
        let valid_tok = fresh_owner_token(&http, &base, "sub-oscar").await;
        let ok_resp = post_move(json!({"target_device": target_dev.id, "id_token": valid_tok, "threads": "stop"}))
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), reqwest::StatusCode::ACCEPTED);

        tokio::time::timeout(Duration::from_secs(5), status_rx.wait_for(|s| s.state == MovePhase::Copying))
            .await
            .expect("entered copying")
            .unwrap();
        let cancel = http
            .delete(format!("{base}/v1/hub/move"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), reqwest::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(5), status_rx.wait_for(|s| s.state == MovePhase::RolledBack))
            .await
            .expect("cancelled cleanly")
            .unwrap();

        let replay_move = post_move(json!({"target_device": target_dev.id, "id_token": valid_tok, "threads": "stop"}))
            .await
            .unwrap();
        assert_eq!(replay_move.status(), reqwest::StatusCode::UNAUTHORIZED);

        // A cookie session qualifies only when POST /v1/auth/google/return just created it
        // for this owner. Token sessions, sessions older than 5 minutes, and other accounts do not.
        // The id_token cases above already accepted a fresh token.
        let quiet = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let login = quiet
            .post(format!("{base}/v1/session"))
            .header("x-grouper", "1")
            .header("origin", &base)
            .json(&json!({"token": &admin_token}))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), reqwest::StatusCode::OK);
        let token_cookie = login.headers().get(reqwest::header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
        let token_session = quiet
            .post(format!("{base}/v1/hub/move"))
            .header("cookie", &token_cookie)
            .header("x-grouper", "1")
            .header("origin", &base)
            .json(&json!({"target_device": target_dev.id, "threads": "stop"}))
            .send()
            .await
            .unwrap();
        assert_eq!(token_session.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(!hub.migrate().status().state.is_active());

        let start = quiet
            .post(format!("{base}/v1/auth/google/start"))
            .header("origin", &base)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), reqwest::StatusCode::OK);
        let start_body: serde_json::Value = start.json().await.unwrap();
        let nonce = start_body["nonce"].as_str().unwrap();
        let state = start_body["state"].as_str().unwrap();
        let web_token = crate::google_auth::tests::mint_for_test(nonce, "sub-oscar");
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("id_token", &web_token)
            .append_pair("state", state)
            .finish();
        let returned = quiet
            .post(format!("{base}/v1/auth/google/return"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .unwrap();
        assert_eq!(returned.status(), reqwest::StatusCode::SEE_OTHER);
        let google_cookie = returned
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let (recorded_sub, recorded_at): (String, i64) = hub
            .db
            .c()
            .query_row(
                "SELECT google_sub, created FROM sessions WHERE google_sub IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(recorded_sub, "sub-oscar");
        assert!(now().saturating_sub(recorded_at as u64) < 30);

        let post_cookie = |cookie: &str, csrf: bool| {
            let mut req = quiet
                .post(format!("{base}/v1/hub/move"))
                .header("cookie", cookie)
                .header("origin", &base)
                .json(&json!({"target_device": target_dev.id, "threads": "stop"}));
            if csrf {
                req = req.header("x-grouper", "1");
            }
            req.send()
        };

        let no_csrf = post_cookie(&google_cookie, false).await.unwrap();
        assert_eq!(no_csrf.status(), reqwest::StatusCode::FORBIDDEN, "cookie moves still need the CSRF header");
        assert!(!hub.is_move_paused());
        assert!(!hub.migrate().status().state.is_active());

        hub.db
            .c()
            .execute("UPDATE sessions SET created = ?1 WHERE google_sub = 'sub-oscar'", [now() as i64 - 360])
            .unwrap();
        let stale = post_cookie(&google_cookie, true).await.unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::UNAUTHORIZED);
        let stale_body: serde_json::Value = stale.json().await.unwrap();
        assert!(stale_body["error"].as_str().unwrap().contains("too old"), "{stale_body}");
        assert!(!hub.migrate().status().state.is_active());

        hub.db
            .c()
            .execute(
                "UPDATE sessions SET created = ?1, google_sub = ?2 WHERE google_sub = 'sub-oscar'",
                params![now() as i64, "sub-other"],
            )
            .unwrap();
        let other = post_cookie(&google_cookie, true).await.unwrap();
        assert_eq!(other.status(), reqwest::StatusCode::FORBIDDEN);
        let other_body: serde_json::Value = other.json().await.unwrap();
        assert!(other_body["error"].as_str().unwrap().contains("different Google account"), "{other_body}");
        assert!(!hub.migrate().status().state.is_active());

        hub.db
            .c()
            .execute("UPDATE sessions SET google_sub = 'sub-oscar' WHERE google_sub = 'sub-other'", [])
            .unwrap();
        let mut status_rx = hub.migrate().subscribe();
        let accepted = post_cookie(&google_cookie, true).await.unwrap();
        assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(5), status_rx.wait_for(|s| s.state == MovePhase::Copying))
            .await
            .expect("google session move entered copying")
            .unwrap();
        let cancel = quiet
            .delete(format!("{base}/v1/hub/move"))
            .header("cookie", &google_cookie)
            .header("x-grouper", "1")
            .header("origin", &base)
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), reqwest::StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(5), status_rx.wait_for(|s| s.state == MovePhase::RolledBack))
            .await
            .expect("google session move cancelled")
            .unwrap();

        let _ = node_stop_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), node_task).await;
        handle.stop().await;
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn confirm_candidates_includes_default_and_configured_public_address() {
        let dir = std::env::temp_dir().join(format!("grouper-confirm-cands-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();

        // With no custom public_address set, `db.public_address()` is DEFAULT_PUBLIC_ADDRESS.
        assert_eq!(db.configured_public_address(), None);
        assert_eq!(db.public_address(), crate::tunnel::DEFAULT_PUBLIC_ADDRESS);
        assert_eq!(
            confirm_candidates(None, &db.public_address()),
            vec![crate::tunnel::DEFAULT_PUBLIC_ADDRESS.to_string()]
        );
        assert_eq!(
            confirm_candidates(Some("http://127.0.0.1:47600/"), &db.public_address()),
            vec![
                "http://127.0.0.1:47600".to_string(),
                crate::tunnel::DEFAULT_PUBLIC_ADDRESS.to_string(),
            ]
        );

        // With a custom public_address in Settings, uses that and deduplicates matching reported URLs.
        db.set_public_address(Some("https://hub.example.org/")).unwrap();
        assert_eq!(
            confirm_candidates(None, &db.public_address()),
            vec!["https://hub.example.org".to_string()]
        );
        assert_eq!(
            confirm_candidates(Some("https://hub.example.org"), &db.public_address()),
            vec!["https://hub.example.org".to_string()]
        );

        drop(db);
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn desktop_style_target_confirms_via_public_address_and_loopback() {
        let root = std::env::temp_dir().join(format!("grouper-desktop-confirm-move-{}", random_hex(4)));
        let old_dir = root.join("old-hub");
        let new_dir = root.join("new-hub");
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let old_handle = host::start(&old_dir, &[loopback]).await.unwrap();
        let old_hub = old_handle.hub().unwrap();
        setup_test_owner(&old_hub, "sub-desktop-move").await;
        old_hub.migrate().set_config(MigrateConfig {
            confirm_timeout: Duration::from_secs(10),
            copy_timeout: Duration::from_secs(15),
            poll_interval: Duration::from_millis(30),
            inject_failure: None,
        });

        let old_url = format!("http://{}", old_handle.binds()[0]);
        let reserved = tokio::net::TcpListener::bind(loopback).await.unwrap();
        let new_bind = reserved.local_addr().unwrap();
        drop(reserved);
        let new_expected_url = format!("http://{new_bind}");

        // Simulated public tunnel endpoint: serves `/v1/hub` from `new_expected_url` once the target
        // hub comes up on `new_bind`, falling back to `old_url` before the cutover.
        let old_for_pub = old_url.clone();
        let new_for_pub = new_expected_url.clone();
        let pub_app = Router::new().route(
            "/v1/hub",
            get(move || {
                let old_u = old_for_pub.clone();
                let new_u = new_for_pub.clone();
                async move {
                    let c = reqwest::Client::builder()
                        .timeout(Duration::from_secs(2))
                        .build()
                        .unwrap();
                    if let Ok(info) = fetch_hub_info(&c, &new_u).await {
                        return Json(info).into_response();
                    }
                    match fetch_hub_info(&c, &old_u).await {
                        Ok(info) => Json(info).into_response(),
                        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
                    }
                }
            }),
        );
        let pub_listener = tokio::net::TcpListener::bind(loopback).await.unwrap();
        let pub_url = format!("http://{}", pub_listener.local_addr().unwrap());
        let pub_task = tokio::spawn(async move {
            let _ = axum::serve(pub_listener, pub_app).await;
        });

        old_hub.db.set_public_address(Some(&pub_url)).unwrap();
        let (_admin, admin_token) = old_hub.db.add_device("Admin", "windows", true).unwrap();
        let pc_install = "target_win_pc_install_1234";
        let pc_node = old_hub
            .db
            .issue_device(
                "Oscar's Windows PC",
                "windows",
                false,
                Some(crate::db::Install { id: pc_install, kind: crate::db::NODE_INSTALL }),
                None,
            )
            .unwrap();
        let target_id = pc_node.device.id.clone();

        let (offer_tx, offer_rx) = oneshot::channel();
        let (commit_tx, commit_rx) = oneshot::channel();
        let events = Arc::new(TestTargetEvents {
            offer_tx: Mutex::new(Some(offer_tx)),
            commit_tx: Mutex::new(Some(commit_tx)),
        });
        let (node_stop_tx, node_stop_rx) = watch::channel(false);
        let client_cfg = core_client::Config {
            hub_url: old_url.clone(),
            token: pc_node.token.clone(),
            capabilities: vec![],
            workspace_root: None,
            allow_shell: false,
            mcp_config: None,
            folders: vec![],
            usage_probes: false,
        };
        let host_info = NodeHostInfo {
            can_host: true,
            app_version: HUB_VERSION.into(),
            free_bytes: Some(10_000_000_000),
            cloudflared: true,
        };
        let node_task = tokio::spawn(core_client::run_with_host(
            client_cfg,
            Some(host_info),
            Arc::new(NoopTools),
            events,
            node_stop_rx,
        ));

        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if old_hub.is_online(&target_id).await
                    && old_hub.host_info_for(&target_id).is_some_and(|h| h.can_host)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("desktop target node connected");

        let old_url_t = old_url.clone();
        let target_tok_t = pc_node.token.clone();
        let pub_url_t = pub_url.clone();
        let new_dir_t = new_dir.clone();
        let target_runner = tokio::spawn(async move {
            let offer = tokio::time::timeout(Duration::from_secs(10), offer_rx).await.unwrap().unwrap();
            let mut opts = TargetMoveOptions::new(new_dir_t, vec![new_bind]);
            opts.confirm_address = Some(pub_url_t);
            opts.confirm_timeout = Duration::from_secs(10);
            opts.poll_interval = Duration::from_millis(30);
            execute_target_move(&old_url_t, &target_tok_t, offer, opts, commit_rx).await
        });

        let http = reqwest::Client::new();
        let id_token = fresh_owner_token(&http, &old_url, "sub-desktop-move").await;
        let start = http
            .post(format!("{old_url}/v1/hub/move"))
            .bearer_auth(&admin_token)
            .json(&json!({"target_device": target_id, "id_token": id_token, "threads": "stop"}))
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), reqwest::StatusCode::ACCEPTED);

        let new_handle = tokio::time::timeout(Duration::from_secs(15), target_runner)
            .await
            .expect("desktop target move finished")
            .unwrap()
            .expect("desktop target move succeeded");

        let mut move_rx = old_hub.migrate().subscribe();
        tokio::time::timeout(Duration::from_secs(10), move_rx.wait_for(|s| s.state == MovePhase::Done))
            .await
            .expect("old hub confirmed via public address and finished")
            .unwrap();

        assert!(old_handle.is_stepped_down());
        let step_info = old_hub.stepdown().info().unwrap();
        assert_eq!(step_info.address, pub_url);
        assert_eq!(step_info.peer_hub_device_id, target_id);
        assert_eq!(step_info.peer_epoch, 2);

        let _ = node_stop_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(2), node_task).await;
        pub_task.abort();
        new_handle.stop().await;
        old_handle.stop().await;
        fs::remove_dir_all(&root).ok();
    }
}
