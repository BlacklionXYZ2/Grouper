//! Embeddable hub host: starts and stops the Grouper hub in-process on one or more sockets.
//!
//! Used by the desktop apps (Windows Tauri, macOS via `ffi`), the CLI (`grouper hub`), and the
//! standalone `grouper-hub serve` binary. [`start`] runs in local mode (in-process [`HubHandle::claim_local`],
//! HTTP setup-code claiming off, token sign-in loopback-only); [`start_with_options`] also supports
//! server mode for `grouper-hub serve`.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::backup::{DataLock, lock_data_dir};
use crate::db::{Db, IssuedDevice};
use crate::google_auth::{Owner, announce_setup_code};
use crate::server::{CloseReason, Hub, router};

const STOP_GRACE: Duration = Duration::from_secs(2);

/// How [`start_with_options`] runs the hub.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Embedded local mode (`true` by default): HTTP setup-code claim is disabled, token sign-in
    /// is accepted only from loopback, and [`HubHandle::claim_local`] is enabled.
    pub local: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { local: true }
    }
}

struct Running {
    hub: Arc<Hub>,
    shutdown: watch::Sender<bool>,
    collector: JoinHandle<()>,
    tunnel: JoinHandle<()>,
    stepdown: JoinHandle<()>,
    servers: Vec<JoinHandle<io::Result<()>>>,
    _lock: DataLock,
}

/// Handle to a running in-process hub started by [`start`] or [`start_with_options`].
pub struct HubHandle {
    data_dir: PathBuf,
    binds: Vec<SocketAddr>,
    running: Mutex<Option<Running>>,
}

/// Start the hub in local mode on `binds` (`127.0.0.1:47600`, `127.0.0.1:0` in tests, etc.).
pub async fn start(data_dir: impl AsRef<Path>, binds: &[SocketAddr]) -> Result<HubHandle> {
    start_inner(data_dir, binds, Options::default(), false).await
}

/// Start the hub in local mode initially paused for a hub-move switch (refuses writes with 503
/// until [`crate::migrate::MigrateState::set_paused`] is cleared).
pub async fn start_paused(data_dir: impl AsRef<Path>, binds: &[SocketAddr]) -> Result<HubHandle> {
    start_inner(data_dir, binds, Options::default(), true).await
}

/// Start the hub with explicit [`Options`] (used by `grouper-hub serve` for server mode).
pub async fn start_with_options(
    data_dir: impl AsRef<Path>,
    binds: &[SocketAddr],
    options: Options,
) -> Result<HubHandle> {
    start_inner(data_dir, binds, options, false).await
}

async fn start_inner(
    data_dir: impl AsRef<Path>,
    binds: &[SocketAddr],
    options: Options,
    start_paused: bool,
) -> Result<HubHandle> {
    ensure!(!binds.is_empty(), "at least one bind address is required");
    let data_dir = data_dir.as_ref().to_path_buf();
    let lock = lock_data_dir(&data_dir)?;
    let db = Db::open(&data_dir)?;
    let _ = db.ensure_hub_device()?;

    let mut listeners = Vec::with_capacity(binds.len());
    let mut local_addrs = Vec::with_capacity(binds.len());
    for &addr in binds {
        let listener = TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
        let bound = listener.local_addr().with_context(|| format!("reading local address for {addr}"))?;
        listeners.push((listener, bound));
        local_addrs.push(bound);
    }

    let hub = Hub::new(db, data_dir.clone());
    hub.set_local_mode(options.local);
    if start_paused {
        hub.migrate().set_paused(true);
    }
    // Threads waiting for a device keep waiting; a hub part that was running stops at once.
    hub.recover_waiting_threads();
    // Threads that were running when the hub stopped can't be resumed.
    for thread in hub.db.threads(None).into_iter().filter(|t| t.status == "running") {
        hub.db.finish_thread(&thread.id, "failed", "The hub restarted while this thread was running.");
    }

    let collector = tokio::spawn(crate::usage::collector_loop(hub.clone()));
    if !options.local {
        announce_setup_code(&hub);
    }

    let (shutdown, shutdown_rx) = watch::channel(false);
    let tunnel = tokio::spawn(crate::tunnel::supervisor_loop(hub.clone(), shutdown_rx.clone()));
    let stepdown = tokio::spawn(crate::stepdown::stepdown_loop(hub.clone(), shutdown_rx.clone()));
    let mut servers = Vec::with_capacity(listeners.len());
    for (listener, bound) in listeners {
        tracing::info!("grouper-hub listening on {bound}, data in {}", data_dir.display());
        // Peer addresses let sign-in rate limits and the loopback guard tell clients apart.
        let app = router(hub.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let mut rx = shutdown_rx.clone();
        servers.push(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.wait_for(|stop| *stop).await;
                })
                .await
        }));
    }

    Ok(HubHandle {
        data_dir,
        binds: local_addrs,
        running: Mutex::new(Some(Running { hub, shutdown, collector, tunnel, stepdown, servers, _lock: lock })),
    })
}

impl HubHandle {
    /// Data directory holding `grouper.db` and `workspaces`.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Actual socket addresses bound by the hub (including OS-chosen ports when `:0` was passed).
    pub fn binds(&self) -> &[SocketAddr] {
        &self.binds
    }

    /// Live [`Hub`] reference while running (`None` once [`Self::stop`] has been called).
    pub fn hub(&self) -> Option<Arc<Hub>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.hub.clone())
    }

    /// Claim this embedded local hub in-process with a Google ID token (no setup code).
    pub async fn claim_local(&self, id_token: &str) -> Result<Owner> {
        let hub = self.hub().context("hub is already stopped")?;
        hub.claim_local(id_token).await
    }

    /// Claim this embedded local hub in-process and issue (or reuse) an admin token for `client`.
    pub async fn claim_local_client(
        &self,
        id_token: &str,
        client: &str,
        device_name: Option<&str>,
        install_id: Option<&str>,
    ) -> Result<(Owner, IssuedDevice)> {
        let hub = self.hub().context("hub is already stopped")?;
        hub.claim_local_client(id_token, client, device_name, install_id).await
    }

    /// Whether this hub has stepped down after detecting a newer hub for the same account.
    pub fn is_stepped_down(&self) -> bool {
        self.hub().is_some_and(|h| h.is_stepped_down())
    }

    /// Stop all listeners, background loops and live connections, releasing the data-directory lock.
    /// Calling `stop` more than once is a safe no-op.
    pub async fn stop(&self) {
        let Some(Running { hub, shutdown, collector, mut tunnel, mut stepdown, servers, _lock }) =
            self.running.lock().unwrap_or_else(|e| e.into_inner()).take()
        else {
            return;
        };
        let _ = shutdown.send(true);
        collector.abort();
        if tokio::time::timeout(STOP_GRACE, &mut tunnel).await.is_err() {
            tunnel.abort();
        }
        if tokio::time::timeout(STOP_GRACE, &mut stepdown).await.is_err() {
            stepdown.abort();
        }
        for (_, live) in hub.live.lock().await.drain() {
            live.close.trigger(CloseReason::Replaced);
        }
        for mut server in servers {
            if tokio::time::timeout(STOP_GRACE, &mut server).await.is_err() {
                server.abort();
            }
        }
    }
}

impl Drop for HubHandle {
    fn drop(&mut self) {
        if let Some(running) = self.running.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = running.shutdown.send(true);
            running.collector.abort();
            running.tunnel.abort();
            running.stepdown.abort();
            for server in running.servers {
                server.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[tokio::test]
    async fn start_serves_binds_in_local_mode_and_stop_releases_lock_for_restart() {
        let root = std::env::temp_dir().join(format!("grouper-host-test-{}", crate::db::random_hex(4)));
        let data = root.join("data");
        let snap = root.join("snap.tar.gz");
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();

        assert!(start(&data, &[]).await.is_err(), "empty binds must fail");
        assert!(lock_data_dir(&data).is_ok(), "failed start must not leave the directory locked");

        let handle = start(&data, &[loopback, loopback]).await.unwrap();
        assert_eq!(handle.binds().len(), 2);
        assert_ne!(handle.binds()[0], handle.binds()[1]);
        assert!(lock_data_dir(&data).is_err(), "running hub holds grouper.lock");

        let http = reqwest::Client::new();
        let mut hub_dev_id = String::new();
        for addr in handle.binds() {
            let health = http.get(format!("http://{addr}/healthz")).send().await.unwrap().text().await.unwrap();
            assert_eq!(health, "ok");
            let info: Value = http.get(format!("http://{addr}/v1/hub")).send().await.unwrap().json().await.unwrap();
            assert_eq!(info["account"], Value::Null);
            assert_eq!(info["epoch"], 1);
            assert!(info.get("key_sha256").is_none());
            hub_dev_id = info["hub_device_id"].as_str().unwrap().to_string();
            assert!(!hub_dev_id.is_empty());
            let status: Value =
                http.get(format!("http://{addr}/v1/auth/status")).send().await.unwrap().json().await.unwrap();
            assert_eq!(status["local"], true);
        }

        // Library backup works while the embedded hub is serving.
        crate::backup::backup(&data, &snap, true).unwrap();
        assert!(snap.is_file());

        let old_addr = handle.binds()[0];
        handle.stop().await;
        handle.stop().await; // idempotent
        assert!(handle.hub().is_none());
        assert!(http.get(format!("http://{old_addr}/healthz")).send().await.is_err());

        // Lock and SQLite handle were released, so restarting on the same data_dir works immediately.
        let handle2 = start(&data, &[loopback]).await.unwrap();
        let info2: Value =
            http.get(format!("http://{}/v1/hub", handle2.binds()[0])).send().await.unwrap().json().await.unwrap();
        assert_eq!(info2["hub_device_id"], hub_dev_id);
        assert_eq!(info2["epoch"], 1);
        handle2.stop().await;

        std::fs::remove_dir_all(&root).ok();
    }
}
