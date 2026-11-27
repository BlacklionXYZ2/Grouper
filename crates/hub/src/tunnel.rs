//! Cloudflare tunnel manager: sealed tunnel token, `cloudflared` discovery and supervision,
//! remote-access settings, and admin HTTP endpoints.
//!
//! - The tunnel token is kept in `hub_meta` (`tunnel_token`) encrypted with the hub's master key
//!   ([`Db::seal`] / [`Db::unseal`]), the same way provider keys and the VAPID key are sealed.
//!   No endpoint ever returns the token.
//! - `cloudflared` is started only when remote access is turned on in settings (`remote_access_enabled`),
//!   a tunnel token is stored, and this hub has not stepped down. Before spawning `cloudflared`, the
//!   supervisor runs a step-down check against the public hub address so two hubs never run the
//!   same tunnel concurrently.
//! - The token is passed exclusively through the `TUNNEL_TOKEN` environment variable, never on the
//!   command line (`cloudflared tunnel run`).
//!
//! Admin endpoints (require an admin token or admin web session):
//! - `GET /v1/tunnel` — status (`not_found` / `stopped` / `starting` / `connected` / `failed`),
//!   `last_error`, `enabled`, `token_set`, `public_address`, `cloudflared_path`, `stepped_down`.
//! - `PUT /v1/tunnel` — update `enabled` and/or `public_address` (`null` or `""` resets to default).
//! - `GET /v1/tunnel/token` — `{"token_set": true|false}`.
//! - `PUT /v1/tunnel/token` — `{"token": "..."}` seals and stores the token (never returns it).
//! - `DELETE /v1/tunnel/token` — clears the token and stops `cloudflared`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, watch};

use crate::db::Db;
use crate::registry::deny_unless_admin;
use crate::server::{Hub, err};

/// Default public hub address shipped in the apps (`docs/local-first-design.md`).
pub const DEFAULT_PUBLIC_ADDRESS: &str = "https://grouper.clockmillbarn.uk";

const META_TUNNEL_TOKEN: &str = "tunnel_token";
const META_REMOTE_ENABLED: &str = "remote_access_enabled";
const META_PUBLIC_ADDRESS: &str = "public_address";

const MAX_TOKEN_LEN: usize = 4096;
const MAX_ERROR_LINE_LEN: usize = 512;

/// Lifecycle status of the supervised `cloudflared` process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelStatus {
    NotFound,
    Stopped,
    Starting,
    Connected,
    Failed,
}

/// Current snapshot of the tunnel supervisor, safe to read or expose to admin APIs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSnapshot {
    pub status: TunnelStatus,
    pub last_error: Option<String>,
    pub cloudflared_path: Option<String>,
    pub restart_count: u64,
}

impl Default for TunnelSnapshot {
    fn default() -> Self {
        Self {
            status: TunnelStatus::Stopped,
            last_error: None,
            cloudflared_path: None,
            restart_count: 0,
        }
    }
}

/// Tuning knobs for the tunnel supervisor (tests override these for fast backoff and fake binaries).
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    /// Explicit binary path; when `None`, searches `PATH` and standard install directories.
    pub binary_override: Option<PathBuf>,
    /// Initial delay before restarting after an unexpected exit.
    pub backoff_initial: Duration,
    /// Upper bound for exponential restart backoff.
    pub backoff_max: Duration,
    /// If `cloudflared` stays alive this long without logging connection, treat it as connected.
    pub connected_grace: Duration,
    /// How often to re-check for `cloudflared` when it was not found.
    pub not_found_recheck: Duration,
    /// Run a step-down check against the public address before launching `cloudflared`.
    pub check_stepdown_before_start: bool,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            binary_override: None,
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            connected_grace: Duration::from_secs(5),
            not_found_recheck: Duration::from_secs(30),
            check_stepdown_before_start: true,
        }
    }
}

/// Per-hub tunnel supervision state held on [`Hub`].
pub struct TunnelState {
    config: Mutex<TunnelConfig>,
    snapshot: watch::Sender<TunnelSnapshot>,
    reconcile: Notify,
    token_gen: AtomicU64,
    supervisor_running: AtomicBool,
}

impl Default for TunnelState {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelState {
    pub fn new() -> Self {
        let (snapshot, _rx) = watch::channel(TunnelSnapshot::default());
        Self {
            config: Mutex::new(TunnelConfig::default()),
            snapshot,
            reconcile: Notify::new(),
            token_gen: AtomicU64::new(0),
            supervisor_running: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> TunnelConfig {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_config(&self, config: TunnelConfig) {
        *self.config.lock().unwrap_or_else(|e| e.into_inner()) = config;
        self.notify_changed();
    }

    pub fn snapshot(&self) -> TunnelSnapshot {
        self.snapshot.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<TunnelSnapshot> {
        self.snapshot.subscribe()
    }

    /// Wake the supervisor to re-read settings, token, and step-down state.
    pub fn notify_changed(&self) {
        self.reconcile.notify_one();
    }

    /// Bump the token generation and wake the supervisor so a running tunnel restarts with the new token.
    pub fn notify_token_changed(&self) {
        self.token_gen.fetch_add(1, Ordering::SeqCst);
        self.reconcile.notify_one();
    }

    fn token_generation(&self) -> u64 {
        self.token_gen.load(Ordering::SeqCst)
    }

    fn set_snapshot(&self, next: TunnelSnapshot) {
        self.snapshot.send_if_modified(|cur| {
            if *cur == next {
                false
            } else {
                *cur = next;
                true
            }
        });
    }

    /// Ask the supervisor to stop `cloudflared` and wait (up to `timeout`) until it reaches `Stopped`.
    pub async fn stop_and_wait(&self, timeout: Duration) {
        self.notify_changed();
        if !self.supervisor_running.load(Ordering::SeqCst) {
            let mut snap = self.snapshot();
            snap.status = TunnelStatus::Stopped;
            snap.last_error = None;
            self.set_snapshot(snap);
            return;
        }
        let mut rx = self.subscribe();
        let _ = tokio::time::timeout(
            timeout,
            rx.wait_for(|s| matches!(s.status, TunnelStatus::Stopped | TunnelStatus::NotFound)),
        )
        .await;
    }
}

// ------------------------------------------------------------------ database settings & sealed token

impl Db {
    /// Seal and save the Cloudflare tunnel token in `hub_meta`.
    pub fn set_tunnel_token(&self, token: &str) -> Result<()> {
        let token = token.trim();
        if token.is_empty() {
            bail!("tunnel token must not be empty");
        }
        if token.len() > MAX_TOKEN_LEN {
            bail!("tunnel token is too long (max {MAX_TOKEN_LEN} bytes)");
        }
        if token.chars().any(char::is_control) {
            bail!("tunnel token must not contain control characters");
        }
        let sealed = self.seal(token).context("sealing tunnel token")?;
        self.c()
            .execute(
                "INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)",
                params![META_TUNNEL_TOKEN, sealed],
            )
            .context("saving sealed tunnel token")?;
        Ok(())
    }

    /// Remove any stored tunnel token.
    pub fn clear_tunnel_token(&self) -> Result<()> {
        self.c()
            .execute("DELETE FROM hub_meta WHERE key = ?1", [META_TUNNEL_TOKEN])
            .context("clearing tunnel token")?;
        Ok(())
    }

    /// Whether a sealed tunnel token is currently stored (never reveals the token).
    pub fn has_tunnel_token(&self) -> bool {
        self.c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [META_TUNNEL_TOKEN], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .is_some_and(|v| !v.is_empty())
    }

    /// Decrypt and return the stored tunnel token for launching `cloudflared`. Internal only.
    pub(crate) fn tunnel_token(&self) -> Result<Option<String>> {
        let sealed: Option<String> = self
            .c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [META_TUNNEL_TOKEN], |r| r.get(0))
            .optional()
            .context("reading sealed tunnel token")?;
        let Some(sealed) = sealed.filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        let plain = self
            .unseal(&sealed)
            .context("cannot decrypt the tunnel token; is this the master key that sealed it?")?;
        Ok(Some(plain))
    }

    /// Whether remote access (running `cloudflared`) is enabled in hub settings. Defaults to `false`.
    pub fn remote_access_enabled(&self) -> bool {
        self.c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [META_REMOTE_ENABLED], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    }

    /// Enable or disable remote access (`cloudflared` starts only when enabled).
    pub fn set_remote_access_enabled(&self, enabled: bool) -> Result<()> {
        let value = if enabled { "true" } else { "false" };
        self.c()
            .execute(
                "INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)",
                params![META_REMOTE_ENABLED, value],
            )
            .context("saving remote_access_enabled")?;
        Ok(())
    }

    /// The custom public hub address if configured, or `None` when using the default.
    pub fn configured_public_address(&self) -> Option<String> {
        self.c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [META_PUBLIC_ADDRESS], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Effective public hub address: configured address if set, else [`DEFAULT_PUBLIC_ADDRESS`].
    pub fn public_address(&self) -> String {
        self.configured_public_address().unwrap_or_else(|| DEFAULT_PUBLIC_ADDRESS.to_string())
    }

    /// Set or clear (`None` / empty) the configured public hub address.
    pub fn set_public_address(&self, address: Option<&str>) -> Result<()> {
        let trimmed = address.map(str::trim).filter(|s| !s.is_empty());
        match trimmed {
            None => {
                self.c()
                    .execute("DELETE FROM hub_meta WHERE key = ?1", [META_PUBLIC_ADDRESS])
                    .context("clearing public_address")?;
            }
            Some(raw) => {
                let parsed = url::Url::parse(raw).context("public_address must be a valid http:// or https:// URL")?;
                if parsed.scheme() != "http" && parsed.scheme() != "https" {
                    bail!("public_address must use http:// or https://");
                }
                if parsed.host_str().is_none() {
                    bail!("public_address must include a host");
                }
                let normalized = raw.trim_end_matches('/');
                self.c()
                    .execute(
                        "INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)",
                        params![META_PUBLIC_ADDRESS, normalized],
                    )
                    .context("saving public_address")?;
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ discovering cloudflared

/// Find `cloudflared` in `PATH` or the usual OS install directories.
pub fn find_cloudflared() -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH");
    find_cloudflared_in(path_env.as_deref(), &standard_install_paths())
}

/// Standard install locations checked after `PATH`.
pub fn standard_install_paths() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut paths = vec![
            PathBuf::from(r"C:\Program Files (x86)\cloudflared\cloudflared.exe"),
            PathBuf::from(r"C:\Program Files\cloudflared\cloudflared.exe"),
        ];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            paths.push(PathBuf::from(local).join("cloudflared").join("cloudflared.exe"));
        }
        paths
    }
    #[cfg(not(windows))]
    {
        vec![
            PathBuf::from("/opt/homebrew/bin/cloudflared"),
            PathBuf::from("/usr/local/bin/cloudflared"),
            PathBuf::from("/usr/bin/cloudflared"),
            PathBuf::from("/snap/bin/cloudflared"),
        ]
    }
}

/// Search `path_env` directories first, then `extra_candidates`, returning the first executable file.
pub fn find_cloudflared_in(path_env: Option<&OsStr>, extra_candidates: &[PathBuf]) -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { &["cloudflared.exe", "cloudflared"] } else { &["cloudflared"] };
    if let Some(paths) = path_env {
        for dir in std::env::split_paths(paths) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for name in names {
                let candidate = dir.join(name);
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    for candidate in extra_candidates {
        if is_executable_file(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn resolve_binary(cfg: &TunnelConfig) -> Option<PathBuf> {
    if let Some(override_path) = &cfg.binary_override {
        return is_executable_file(override_path).then(|| override_path.clone());
    }
    find_cloudflared()
}

// ------------------------------------------------------------------ supervision loop

/// Background supervisor started by [`crate::host`]. Runs until `shutdown` becomes `true`.
pub async fn supervisor_loop(hub: Arc<Hub>, mut shutdown: watch::Receiver<bool>) {
    hub.tunnel.supervisor_running.store(true, Ordering::SeqCst);
    struct RunningGuard(Arc<Hub>);
    impl Drop for RunningGuard {
        fn drop(&mut self) {
            self.0.tunnel.supervisor_running.store(false, Ordering::SeqCst);
        }
    }
    let _guard = RunningGuard(hub.clone());

    let mut backoff = hub.tunnel.config().backoff_initial;
    let mut restart_count = 0u64;

    loop {
        if *shutdown.borrow() {
            let cfg = hub.tunnel.config();
            hub.tunnel.set_snapshot(TunnelSnapshot {
                status: TunnelStatus::Stopped,
                last_error: None,
                cloudflared_path: resolve_binary(&cfg).map(|p| p.display().to_string()),
                restart_count,
            });
            break;
        }

        let cfg = hub.tunnel.config();
        let enabled = hub.db.remote_access_enabled();
        let stepped_down = hub.stepdown.is_stepped_down();

        if !enabled || stepped_down {
            backoff = cfg.backoff_initial;
            hub.tunnel.set_snapshot(TunnelSnapshot {
                status: TunnelStatus::Stopped,
                last_error: None,
                cloudflared_path: resolve_binary(&cfg).map(|p| p.display().to_string()),
                restart_count,
            });
            if !wait_for_change_or_shutdown(&hub.tunnel.reconcile, &mut shutdown, None).await {
                break;
            }
            continue;
        }

        let token = match hub.db.tunnel_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                backoff = cfg.backoff_initial;
                hub.tunnel.set_snapshot(TunnelSnapshot {
                    status: TunnelStatus::Stopped,
                    last_error: None,
                    cloudflared_path: resolve_binary(&cfg).map(|p| p.display().to_string()),
                    restart_count,
                });
                if !wait_for_change_or_shutdown(&hub.tunnel.reconcile, &mut shutdown, None).await {
                    break;
                }
                continue;
            }
            Err(error) => {
                hub.tunnel.set_snapshot(TunnelSnapshot {
                    status: TunnelStatus::Failed,
                    last_error: Some(format!("could not decrypt tunnel token: {error}")),
                    cloudflared_path: resolve_binary(&cfg).map(|p| p.display().to_string()),
                    restart_count,
                });
                if !wait_for_change_or_shutdown(&hub.tunnel.reconcile, &mut shutdown, None).await {
                    break;
                }
                continue;
            }
        };

        // Ask-before-start step-down check: never start cloudflared if a newer hub is already serving.
        if cfg.check_stepdown_before_start {
            let outcome = crate::stepdown::check_hub(&hub).await;
            if matches!(outcome, crate::stepdown::CheckOutcome::SteppedDown(_)) || hub.stepdown.is_stepped_down() {
                continue;
            }
            if *shutdown.borrow() {
                continue;
            }
        }

        let Some(binary) = resolve_binary(&cfg) else {
            hub.tunnel.set_snapshot(TunnelSnapshot {
                status: TunnelStatus::NotFound,
                last_error: Some("cloudflared binary not found in PATH or standard install folders".into()),
                cloudflared_path: None,
                restart_count,
            });
            if !wait_for_change_or_shutdown(&hub.tunnel.reconcile, &mut shutdown, Some(cfg.not_found_recheck)).await {
                break;
            }
            continue;
        };

        let binary_str = binary.display().to_string();
        let run_gen = hub.tunnel.token_generation();
        hub.tunnel.set_snapshot(TunnelSnapshot {
            status: TunnelStatus::Starting,
            last_error: None,
            cloudflared_path: Some(binary_str.clone()),
            restart_count,
        });

        let mut cmd = build_cloudflared_command(&binary, &token);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                restart_count += 1;
                let msg = redact_token(&format!("failed to spawn cloudflared: {error}"), &token);
                tracing::warn!(path = %binary_str, backoff_ms = backoff.as_millis() as u64, "tunnel: {msg}");
                hub.tunnel.set_snapshot(TunnelSnapshot {
                    status: TunnelStatus::Failed,
                    last_error: Some(msg),
                    cloudflared_path: Some(binary_str),
                    restart_count,
                });
                if !wait_for_change_or_shutdown(&hub.tunnel.reconcile, &mut shutdown, Some(backoff)).await {
                    break;
                }
                backoff = (backoff * 2).min(cfg.backoff_max);
                continue;
            }
        };

        tracing::info!(path = %binary_str, "tunnel: started cloudflared");

        let last_line: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let connected_signal = Arc::new(Notify::new());

        let stdout_task = child.stdout.take().map(|out| {
            spawn_log_reader(out, last_line.clone(), connected_signal.clone(), token.clone())
        });
        let stderr_task = child.stderr.take().map(|err_pipe| {
            spawn_log_reader(err_pipe, last_line.clone(), connected_signal.clone(), token.clone())
        });

        let grace_sleep = tokio::time::sleep(cfg.connected_grace);
        tokio::pin!(grace_sleep);
        let mut connected = false;

        enum RunExit {
            Shutdown,
            StopOrRestart,
            Exited(String),
        }

        let run_exit = loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        kill_and_wait(&mut child).await;
                        break RunExit::Shutdown;
                    }
                }
                _ = hub.tunnel.reconcile.notified() => {
                    let must_stop = !hub.db.remote_access_enabled()
                        || hub.stepdown.is_stepped_down()
                        || !hub.db.has_tunnel_token()
                        || hub.tunnel.token_generation() != run_gen
                        || resolve_binary(&hub.tunnel.config()).as_ref() != Some(&binary);
                    if must_stop {
                        kill_and_wait(&mut child).await;
                        break RunExit::StopOrRestart;
                    }
                }
                status = child.wait() => {
                    if let Some(t) = stdout_task {
                        let _ = tokio::time::timeout(Duration::from_millis(200), t).await;
                    }
                    if let Some(t) = stderr_task {
                        let _ = tokio::time::timeout(Duration::from_millis(200), t).await;
                    }
                    let tail = last_line.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let detail = match status {
                        Ok(s) => match tail {
                            Some(line) => format!("cloudflared exited ({s}): {line}"),
                            None => format!("cloudflared exited ({s})"),
                        },
                        Err(e) => format!("waiting on cloudflared failed: {e}"),
                    };
                    break RunExit::Exited(redact_token(&detail, &token));
                }
                _ = connected_signal.notified(), if !connected => {
                    connected = true;
                    backoff = cfg.backoff_initial;
                    hub.tunnel.set_snapshot(TunnelSnapshot {
                        status: TunnelStatus::Connected,
                        last_error: None,
                        cloudflared_path: Some(binary_str.clone()),
                        restart_count,
                    });
                }
                _ = &mut grace_sleep, if !connected => {
                    connected = true;
                    backoff = cfg.backoff_initial;
                    hub.tunnel.set_snapshot(TunnelSnapshot {
                        status: TunnelStatus::Connected,
                        last_error: None,
                        cloudflared_path: Some(binary_str.clone()),
                        restart_count,
                    });
                }
            }
        };

        match run_exit {
            RunExit::Shutdown => {
                hub.tunnel.set_snapshot(TunnelSnapshot {
                    status: TunnelStatus::Stopped,
                    last_error: None,
                    cloudflared_path: Some(binary_str),
                    restart_count,
                });
                break;
            }
            RunExit::StopOrRestart => {
                backoff = cfg.backoff_initial;
                hub.tunnel.set_snapshot(TunnelSnapshot {
                    status: TunnelStatus::Stopped,
                    last_error: None,
                    cloudflared_path: Some(binary_str),
                    restart_count,
                });
            }
            RunExit::Exited(err_msg) => {
                restart_count += 1;
                tracing::warn!(
                    backoff_ms = backoff.as_millis() as u64,
                    restart_count,
                    "tunnel: {err_msg}"
                );
                hub.tunnel.set_snapshot(TunnelSnapshot {
                    status: TunnelStatus::Failed,
                    last_error: Some(err_msg),
                    cloudflared_path: Some(binary_str),
                    restart_count,
                });
                if !wait_for_change_or_shutdown(&hub.tunnel.reconcile, &mut shutdown, Some(backoff)).await {
                    break;
                }
                backoff = (backoff * 2).min(cfg.backoff_max);
            }
        }
    }
}

/// Build the `cloudflared` command. Token goes exclusively via `TUNNEL_TOKEN`, never in argv.
fn build_cloudflared_command(binary: &Path, token: &str) -> Command {
    let mut cmd = Command::new(binary);
    cmd.args(["tunnel", "run"])
        .env("TUNNEL_TOKEN", token)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: avoid flashing a console window when embedded in a desktop app.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

async fn kill_and_wait(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

fn spawn_log_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: R,
    last_line: Arc<Mutex<Option<String>>>,
    connected_signal: Arc<Notify>,
    token: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if is_connected_log_line(trimmed) {
                connected_signal.notify_one();
            }
            let safe = truncate_line(&redact_token(trimmed, &token), MAX_ERROR_LINE_LEN);
            *last_line.lock().unwrap_or_else(|e| e.into_inner()) = Some(safe);
        }
    })
}

fn is_connected_log_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("registered tunnel connection")
}

fn redact_token(msg: &str, token: &str) -> String {
    if token.is_empty() {
        msg.to_string()
    } else {
        msg.replace(token, "[redacted]")
    }
}

fn truncate_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let prefix: String = line.chars().take(max_chars).collect();
        format!("{prefix}...")
    }
}

/// Wait for `reconcile`, an optional `delay`, or `shutdown`. Returns `false` if shutting down.
async fn wait_for_change_or_shutdown(
    reconcile: &Notify,
    shutdown: &mut watch::Receiver<bool>,
    delay: Option<Duration>,
) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    match delay {
        Some(d) => {
            tokio::select! {
                biased;
                changed = shutdown.changed() => !(changed.is_err() || *shutdown.borrow()),
                _ = reconcile.notified() => !*shutdown.borrow(),
                _ = tokio::time::sleep(d) => !*shutdown.borrow(),
            }
        }
        None => {
            tokio::select! {
                biased;
                changed = shutdown.changed() => !(changed.is_err() || *shutdown.borrow()),
                _ = reconcile.notified() => !*shutdown.borrow(),
            }
        }
    }
}

// ------------------------------------------------------------------ admin HTTP routes

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/tunnel", get(get_tunnel_status).put(update_tunnel_settings))
        .route("/v1/tunnel/token", get(get_token_status).put(set_token).delete(delete_token))
}

fn status_payload(hub: &Hub) -> serde_json::Value {
    let snap = hub.tunnel.snapshot();
    let cfg = hub.tunnel.config();
    let cloudflared_path = snap.cloudflared_path.or_else(|| resolve_binary(&cfg).map(|p| p.display().to_string()));
    let stepdown = hub.stepdown.info();
    json!({
        "status": snap.status,
        "last_error": snap.last_error,
        "cloudflared_path": cloudflared_path,
        "restart_count": snap.restart_count,
        "enabled": hub.db.remote_access_enabled(),
        "token_set": hub.db.has_tunnel_token(),
        "public_address": hub.db.public_address(),
        "custom_address": hub.db.configured_public_address(),
        "stepped_down": stepdown.is_some(),
        "stepdown": stepdown,
    })
}

async fn get_tunnel_status(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    Json(status_payload(&hub)).into_response()
}

#[derive(Deserialize)]
struct UpdateTunnelBody {
    #[serde(default)]
    enabled: Option<bool>,
    /// Use `null` or `""` to revert to the default public address.
    #[serde(default)]
    public_address: Option<Option<String>>,
}

async fn update_tunnel_settings(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<UpdateTunnelBody>,
) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    if let Some(addr_opt) = &body.public_address
        && let Err(e) = hub.db.set_public_address(addr_opt.as_deref())
    {
        return err(StatusCode::BAD_REQUEST, e);
    }
    if let Some(enabled) = body.enabled
        && let Err(e) = hub.db.set_remote_access_enabled(enabled)
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    hub.tunnel.notify_changed();
    Json(status_payload(&hub)).into_response()
}

async fn get_token_status(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    Json(json!({ "token_set": hub.db.has_tunnel_token() })).into_response()
}

#[derive(Deserialize)]
struct SetTokenBody {
    token: String,
}

async fn set_token(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<SetTokenBody>,
) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    if let Err(e) = hub.db.set_tunnel_token(&body.token) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    hub.tunnel.notify_token_changed();
    Json(json!({ "token_set": true })).into_response()
}

async fn delete_token(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    if let Err(e) = hub.db.clear_tunnel_token() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    hub.tunnel.notify_token_changed();
    Json(json!({ "token_set": false })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    use serde_json::Value;
    use tokio::net::TcpListener;

    use crate::db::random_hex;
    use crate::secrets::Secrets;
    use crate::server::router;

    /// Tiny native fake `cloudflared` compiled once per test process with `rustc`.
    /// Reads `<exe_dir>/behavior.txt` and appends a JSON line per invocation to `<exe_dir>/launches.jsonl`.
    const FAKE_CLOUDFLARED_SRC: &str = r#"
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn main() {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().expect("exe parent");
    let count_path = dir.join("count.txt");
    let launch_num: u64 = fs::read_to_string(&count_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    let _ = fs::write(&count_path, launch_num.to_string());

    let args: Vec<String> = std::env::args().skip(1).collect();
    let token = std::env::var("TUNNEL_TOKEN").ok();
    let pid = std::process::id();

    let args_json = args
        .iter()
        .map(|a| format!("\"{}\"", json_escape(a)))
        .collect::<Vec<_>>()
        .join(",");
    let token_json = match &token {
        Some(t) => format!("\"{}\"", json_escape(t)),
        None => "null".to_string(),
    };
    let record = format!(
        "{{\"launch\":{launch_num},\"pid\":{pid},\"args\":[{args_json}],\"token\":{token_json}}}\n"
    );

    let log_path: PathBuf = dir.join("launches.jsonl");
    let mut file = OpenOptions::new().create(true).append(true).open(&log_path).expect("open log");
    file.write_all(record.as_bytes()).expect("write log");
    file.flush().expect("flush log");

    // Behavior file: "fail_first:<N>" or "stay" (default).
    let behavior = fs::read_to_string(dir.join("behavior.txt")).unwrap_or_else(|_| "stay".into());
    let behavior = behavior.trim();
    if let Some(n_str) = behavior.strip_prefix("fail_first:") {
        let fail_count: u64 = n_str.trim().parse().unwrap_or(1);
        if launch_num <= fail_count {
            eprintln!("simulated failure on launch {launch_num}");
            std::process::exit(1);
        }
    }

    eprintln!("INF Registered tunnel connection connIndex=0");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
"#;

    fn compiled_fake_cloudflared() -> PathBuf {
        static BIN: OnceLock<PathBuf> = OnceLock::new();
        BIN.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!(
                "grouper-fake-cloudflared-{}-{}",
                std::process::id(),
                random_hex(4)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let src = dir.join("fake_cloudflared.rs");
            let exe_name = if cfg!(windows) { "fake_cloudflared.exe" } else { "fake_cloudflared" };
            let out = dir.join(exe_name);
            std::fs::write(&src, FAKE_CLOUDFLARED_SRC).unwrap();
            let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
            let status = std::process::Command::new(&rustc)
                .args(["--edition", "2021"])
                .arg(&src)
                .arg("-o")
                .arg(&out)
                .status()
                .expect("running rustc to compile fake cloudflared");
            assert!(status.success(), "rustc failed compiling fake cloudflared");
            out
        })
        .clone()
    }

    fn install_fake_in(dir: &Path, behavior: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let exe_name = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };
        let dst = dir.join(exe_name);
        std::fs::copy(compiled_fake_cloudflared(), &dst).unwrap();
        std::fs::write(dir.join("behavior.txt"), behavior).unwrap();
        dst
    }

    fn read_launches(dir: &Path) -> Vec<Value> {
        let path = dir.join("launches.jsonl");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return vec![];
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect("valid json line"))
            .collect()
    }

    fn fast_config(binary: Option<PathBuf>) -> TunnelConfig {
        TunnelConfig {
            binary_override: binary,
            backoff_initial: Duration::from_millis(30),
            backoff_max: Duration::from_millis(120),
            connected_grace: Duration::from_millis(150),
            not_found_recheck: Duration::from_millis(100),
            check_stepdown_before_start: false,
        }
    }

    #[test]
    fn sealed_token_round_trip_and_master_key_encryption() {
        let dir = std::env::temp_dir().join(format!("grouper-tunnel-db-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();

        assert!(!db.has_tunnel_token());
        assert_eq!(db.tunnel_token().unwrap(), None);
        assert!(!db.remote_access_enabled());
        assert_eq!(db.public_address(), DEFAULT_PUBLIC_ADDRESS);

        let raw_token = "eyJhIjoiZmFrZS10dW5uZWwtdG9rZW4ifQ==";
        db.set_tunnel_token(raw_token).unwrap();
        assert!(db.has_tunnel_token());
        assert_eq!(db.tunnel_token().unwrap().as_deref(), Some(raw_token));

        // Stored value in SQLite must be sealed (`enc:v1:...`), never plaintext.
        let stored: String = db
            .c()
            .query_row("SELECT value FROM hub_meta WHERE key = 'tunnel_token'", [], |r| r.get(0))
            .unwrap();
        assert!(Secrets::is_sealed(&stored), "expected sealed token, got {stored}");
        assert!(!stored.contains(raw_token));

        // Empty or control-char tokens are rejected.
        assert!(db.set_tunnel_token("   ").is_err());
        assert!(db.set_tunnel_token("abc\ndef").is_err());

        db.clear_tunnel_token().unwrap();
        assert!(!db.has_tunnel_token());
        assert_eq!(db.tunnel_token().unwrap(), None);

        // Settings validation.
        assert!(db.set_public_address(Some("ftp://bad.example")).is_err());
        db.set_public_address(Some("https://custom.example.com/")).unwrap();
        assert_eq!(db.public_address(), "https://custom.example.com");
        db.set_public_address(None).unwrap();
        assert_eq!(db.public_address(), DEFAULT_PUBLIC_ADDRESS);

        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_cloudflared_checks_path_and_extra_dirs() {
        let root = std::env::temp_dir().join(format!("grouper-find-cf-{}", random_hex(4)));
        let path_dir = root.join("in_path");
        let extra_dir = root.join("extra");
        std::fs::create_dir_all(&path_dir).unwrap();
        std::fs::create_dir_all(&extra_dir).unwrap();

        let exe_name = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };
        let extra_bin = extra_dir.join(exe_name);
        std::fs::copy(compiled_fake_cloudflared(), &extra_bin).unwrap();

        // Missing from PATH -> falls back to extra install locations.
        let found = find_cloudflared_in(Some(path_dir.as_os_str()), std::slice::from_ref(&extra_bin));
        assert_eq!(found.as_deref(), Some(extra_bin.as_path()));

        // Present in PATH -> PATH wins over extra install locations.
        let path_bin = path_dir.join(exe_name);
        std::fs::copy(compiled_fake_cloudflared(), &path_bin).unwrap();
        let found2 = find_cloudflared_in(Some(path_dir.as_os_str()), std::slice::from_ref(&extra_bin));
        assert_eq!(found2.as_deref(), Some(path_bin.as_path()));

        // Missing everywhere -> None.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(find_cloudflared_in(Some(empty.as_os_str()), &[empty.join(exe_name)]), None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn admin_api_sets_clears_and_never_returns_token() {
        let dir = std::env::temp_dir().join(format!("grouper-tunnel-api-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_tok) = db.add_device("admin", "windows", true).unwrap();
        let (_, plain_tok) = db.add_device("node", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        hub.tunnel.set_config(fast_config(Some(dir.join("missing-cloudflared"))));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let http = reqwest::Client::new();
        let base = format!("http://{addr}");
        let secret = "super-secret-cf-token-xyz-98765";

        // Unauthenticated and non-admin callers are refused.
        assert_eq!(http.get(format!("{base}/v1/tunnel")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            http.get(format!("{base}/v1/tunnel"))
                .bearer_auth(&plain_tok)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            http.put(format!("{base}/v1/tunnel/token"))
                .bearer_auth(&plain_tok)
                .json(&json!({"token": secret}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );

        // Admin checks initial status.
        let r: Value = http
            .get(format!("{base}/v1/tunnel"))
            .bearer_auth(&admin_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r["enabled"], false);
        assert_eq!(r["token_set"], false);
        assert_eq!(r["status"], "stopped");
        assert_eq!(r["public_address"], DEFAULT_PUBLIC_ADDRESS);

        // Set token: response confirms token_set=true and NEVER includes the token string.
        let put_text = http
            .put(format!("{base}/v1/tunnel/token"))
            .bearer_auth(&admin_tok)
            .json(&json!({"token": secret}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!put_text.contains(secret), "PUT response leaked token: {put_text}");
        let put_json: Value = serde_json::from_str(&put_text).unwrap();
        assert_eq!(put_json["token_set"], true);

        // GET /v1/tunnel/token and GET /v1/tunnel show token_set=true and never include the token.
        for path in ["/v1/tunnel/token", "/v1/tunnel"] {
            let body = http
                .get(format!("{base}{path}"))
                .bearer_auth(&admin_tok)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(!body.contains(secret), "{path} leaked token: {body}");
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["token_set"], true);
        }

        // Toggle enabled and custom public address.
        let upd: Value = http
            .put(format!("{base}/v1/tunnel"))
            .bearer_auth(&admin_tok)
            .json(&json!({"enabled": true, "public_address": "https://hub.example.test"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(upd["enabled"], true);
        assert_eq!(upd["public_address"], "https://hub.example.test");

        // Clear token.
        let del: Value = http
            .delete(format!("{base}/v1/tunnel/token"))
            .bearer_auth(&admin_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(del["token_set"], false);
        assert!(!hub.db.has_tunnel_token());

        server.abort();
        drop(hub);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn supervisor_passes_token_via_env_only_restarts_with_backoff_and_stops_cleanly() {
        let root = std::env::temp_dir().join(format!("grouper-tunnel-sup-{}", random_hex(4)));
        let data_dir = root.join("data");
        let bin_dir = root.join("bin");
        let fake_bin = install_fake_in(&bin_dir, "fail_first:1");

        let db = Db::open(&data_dir).unwrap();
        let secret = "test-tunnel-token-env-only-12345";
        db.set_tunnel_token(secret).unwrap();
        let hub = Hub::new(db, data_dir.clone());
        hub.tunnel.set_config(fast_config(Some(fake_bin)));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut snap_rx = hub.tunnel.subscribe();
        let sup_task = tokio::spawn(supervisor_loop(hub.clone(), shutdown_rx));

        // Remote access is OFF by default -> must NOT start cloudflared ("ask before start").
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(hub.tunnel.snapshot().status, TunnelStatus::Stopped);
        assert!(read_launches(&bin_dir).is_empty(), "cloudflared must not start while remote access is off");

        // Enable remote access -> first launch fails, supervisor records Failed & restarts with backoff,
        // second launch succeeds and transitions to Connected.
        hub.db.set_remote_access_enabled(true).unwrap();
        hub.tunnel.notify_changed();

        tokio::time::timeout(
            Duration::from_secs(5),
            snap_rx.wait_for(|s| s.status == TunnelStatus::Connected),
        )
        .await
        .expect("tunnel reached Connected after retry")
        .unwrap();

        let snap = hub.tunnel.snapshot();
        assert_eq!(snap.status, TunnelStatus::Connected);
        assert!(snap.restart_count >= 1, "expected at least one restart after first failure");
        assert!(snap.last_error.is_none());

        let launches = read_launches(&bin_dir);
        assert_eq!(launches.len(), 2, "expected 1 failed + 1 connected launch, got {launches:?}");
        for launch in &launches {
            let args: Vec<&str> = launch["args"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
            assert_eq!(args, vec!["tunnel", "run"], "token must never appear on command line");
            assert!(!args.iter().any(|a| a.contains(secret) || *a == "--token"));
            assert_eq!(launch["token"], secret, "token must be passed via TUNNEL_TOKEN env var");
        }

        // Turning remote access off stops cloudflared and transitions to Stopped.
        hub.db.set_remote_access_enabled(false).unwrap();
        hub.tunnel.notify_changed();
        tokio::time::timeout(
            Duration::from_secs(5),
            snap_rx.wait_for(|s| s.status == TunnelStatus::Stopped),
        )
        .await
        .expect("tunnel stopped when remote access disabled")
        .unwrap();

        // Re-enable, wait for Connected, then stop via hub shutdown channel.
        hub.db.set_remote_access_enabled(true).unwrap();
        hub.tunnel.notify_changed();
        tokio::time::timeout(
            Duration::from_secs(5),
            snap_rx.wait_for(|s| s.status == TunnelStatus::Connected),
        )
        .await
        .expect("tunnel reconnected")
        .unwrap();
        assert_eq!(read_launches(&bin_dir).len(), 3);

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(5), sup_task)
            .await
            .expect("supervisor exited on shutdown")
            .unwrap();
        assert_eq!(hub.tunnel.snapshot().status, TunnelStatus::Stopped);

        drop(hub);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn supervisor_reports_not_found_when_binary_missing() {
        let root = std::env::temp_dir().join(format!("grouper-tunnel-nf-{}", random_hex(4)));
        let db = Db::open(&root).unwrap();
        db.set_tunnel_token("token-for-missing-binary").unwrap();
        db.set_remote_access_enabled(true).unwrap();
        let hub = Hub::new(db, root.clone());
        hub.tunnel.set_config(fast_config(Some(root.join("does-not-exist"))));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut snap_rx = hub.tunnel.subscribe();
        let sup_task = tokio::spawn(supervisor_loop(hub.clone(), shutdown_rx));

        tokio::time::timeout(
            Duration::from_secs(3),
            snap_rx.wait_for(|s| s.status == TunnelStatus::NotFound),
        )
        .await
        .expect("status became NotFound")
        .unwrap();
        assert!(hub.tunnel.snapshot().last_error.is_some());

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), sup_task).await;
        drop(hub);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn stepdown_stops_running_cloudflared_and_prevents_restart() {
        use axum::routing::get as axum_get;
        use core_protocol::HubInfo;

        let root = std::env::temp_dir().join(format!("grouper-tunnel-sd-{}", random_hex(4)));
        let data_dir = root.join("data");
        let bin_dir = root.join("bin");
        let fake_bin = install_fake_in(&bin_dir, "stay");

        let db = Db::open(&data_dir).unwrap();
        let _ = db.ensure_hub_device().unwrap();
        db.set_owner_if_none(&crate::google_auth::Owner {
            provider: "google".into(),
            sub: "sub-999".into(),
            email: "oscar@example.com".into(),
            name: "Oscar".into(),
            claimed_at: crate::db::now(),
        })
        .unwrap();
        db.set_tunnel_token("token-stepdown-test").unwrap();
        db.set_remote_access_enabled(true).unwrap();

        // Start a fake peer hub initially at equal epoch (1), so cloudflared starts normally.
        let peer_state = Arc::new(Mutex::new(HubInfo {
            hub_id: "peer".into(),
            hub_version: "0.1.0".into(),
            protocol_version: core_protocol::PROTOCOL_VERSION,
            account: Some(crate::server::account_id("google", "sub-999")),
            hub_device_id: "replacement-device".into(),
            epoch: 1,
            key_sha256: None,
        }));
        let peer_clone = peer_state.clone();
        let peer_app = Router::new().route(
            "/v1/hub",
            axum_get(move || {
                let s = peer_clone.clone();
                async move { Json(s.lock().unwrap().clone()) }
            }),
        );
        let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer_listener.local_addr().unwrap();
        let peer_task = tokio::spawn(async move {
            let _ = axum::serve(peer_listener, peer_app).await;
        });
        db.set_public_address(Some(&format!("http://{peer_addr}"))).unwrap();

        let hub = Hub::new(db, data_dir.clone());
        let mut cfg = fast_config(Some(fake_bin));
        cfg.check_stepdown_before_start = true;
        hub.tunnel.set_config(cfg);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut snap_rx = hub.tunnel.subscribe();
        let sup_task = tokio::spawn(supervisor_loop(hub.clone(), shutdown_rx));

        tokio::time::timeout(
            Duration::from_secs(5),
            snap_rx.wait_for(|s| s.status == TunnelStatus::Connected),
        )
        .await
        .expect("tunnel started and connected")
        .unwrap();
        assert!(!hub.is_stepped_down());

        // Replacement hub bumps epoch to 2 -> step-down check stops cloudflared and marks stepped down.
        peer_state.lock().unwrap().epoch = 2;
        let outcome = crate::stepdown::check_hub(&hub).await;
        assert!(matches!(outcome, crate::stepdown::CheckOutcome::SteppedDown(_)));
        assert!(hub.is_stepped_down());
        assert_eq!(hub.tunnel.snapshot().status, TunnelStatus::Stopped);

        // Even nudging reconcile must NOT restart cloudflared once stepped down.
        hub.tunnel.notify_changed();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hub.tunnel.snapshot().status, TunnelStatus::Stopped);
        assert_eq!(read_launches(&bin_dir).len(), 1);

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), sup_task).await;
        peer_task.abort();
        drop(hub);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn pre_start_stepdown_prevents_cloudflared_from_ever_starting() {
        use axum::routing::get as axum_get;
        use core_protocol::HubInfo;

        let root = std::env::temp_dir().join(format!("grouper-tunnel-precheck-{}", random_hex(4)));
        let data_dir = root.join("data");
        let bin_dir = root.join("bin");
        let fake_bin = install_fake_in(&bin_dir, "stay");

        let db = Db::open(&data_dir).unwrap();
        let _ = db.ensure_hub_device().unwrap();
        db.set_owner_if_none(&crate::google_auth::Owner {
            provider: "google".into(),
            sub: "sub-pre".into(),
            email: "oscar@example.com".into(),
            name: "Oscar".into(),
            claimed_at: crate::db::now(),
        })
        .unwrap();
        db.set_tunnel_token("token-never-launched").unwrap();
        db.set_remote_access_enabled(true).unwrap();

        let peer_info = HubInfo {
            hub_id: "peer".into(),
            hub_version: "0.1.0".into(),
            protocol_version: core_protocol::PROTOCOL_VERSION,
            account: Some(crate::server::account_id("google", "sub-pre")),
            hub_device_id: "already-newer-hub".into(),
            epoch: 5,
            key_sha256: None,
        };
        let peer_app = Router::new().route("/v1/hub", axum_get(move || async move { Json(peer_info.clone()) }));
        let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer_listener.local_addr().unwrap();
        let peer_task = tokio::spawn(async move {
            let _ = axum::serve(peer_listener, peer_app).await;
        });
        db.set_public_address(Some(&format!("http://{peer_addr}"))).unwrap();

        let hub = Hub::new(db, data_dir.clone());
        let mut cfg = fast_config(Some(fake_bin));
        cfg.check_stepdown_before_start = true;
        hub.tunnel.set_config(cfg);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut sd_rx = hub.stepdown.subscribe();
        let sup_task = tokio::spawn(supervisor_loop(hub.clone(), shutdown_rx));

        tokio::time::timeout(Duration::from_secs(3), sd_rx.wait_for(|v| v.is_some()))
            .await
            .expect("pre-start step-down detected")
            .unwrap();
        assert!(hub.is_stepped_down());
        assert_eq!(hub.tunnel.snapshot().status, TunnelStatus::Stopped);
        assert!(read_launches(&bin_dir).is_empty(), "cloudflared must never launch when a newer hub is serving");

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), sup_task).await;
        peer_task.abort();
        drop(hub);
        std::fs::remove_dir_all(&root).ok();
    }
}
