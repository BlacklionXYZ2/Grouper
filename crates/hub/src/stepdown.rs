//! Step-down detection for local-first hubs.
//!
//! Two hubs for the same account must never run the Cloudflare tunnel at the same time (Cloudflare
//! balances traffic across every active connector). Whenever a hub starts, before it launches
//! `cloudflared`, and periodically while it runs, it fetches `<public_address>/v1/hub`.
//!
//! If the peer answers with:
//! - the **same account** (matching opaque account id from `GET /v1/hub`),
//! - a **different `hub_device_id`**, and
//! - a **strictly higher `epoch`**,
//!
//! then this hub has been replaced. It stops `cloudflared`, records the stepped-down state so other
//! code can read or watch it, and logs the event. It never steps down on a lower or equal epoch, a
//! different account, the same `hub_device_id`, or when the address is unreachable.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_protocol::HubInfo;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::server::Hub;

/// How often a running hub polls its public address for a newer hub.
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Timeout for a single `GET /v1/hub` check.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long step-down waits for the tunnel supervisor to stop `cloudflared`.
const STOP_TUNNEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Details recorded when this hub steps down because a newer hub replaced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepDownInfo {
    pub account: String,
    pub local_hub_device_id: String,
    pub local_epoch: u64,
    pub peer_hub_device_id: String,
    pub peer_epoch: u64,
    pub address: String,
    pub detected_at: u64,
}

/// Result of a single step-down check against the public address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// A peer hub with the same account, a different `hub_device_id`, and a higher `epoch` answered.
    SteppedDown(StepDownInfo),
    /// The peer did not supersede this hub (same device, lower/equal epoch, or different account).
    KeepServing,
    /// The public address could not be reached or did not return a valid `/v1/hub` response.
    Unreachable(String),
    /// This hub has no owner yet, so there is no account to compare against.
    NoOwner,
}

/// Shared step-down state held on [`Hub`]; other code can read or subscribe to it.
pub struct StepDownState {
    tx: watch::Sender<Option<StepDownInfo>>,
}

impl Default for StepDownState {
    fn default() -> Self {
        Self::new()
    }
}

impl StepDownState {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(None);
        Self { tx }
    }

    /// Whether this hub has stepped down after seeing a newer hub for the same account.
    pub fn is_stepped_down(&self) -> bool {
        self.tx.borrow().is_some()
    }

    /// Details of the peer hub that caused step-down, if stepped down.
    pub fn info(&self) -> Option<StepDownInfo> {
        self.tx.borrow().clone()
    }

    /// Watch channel notified when this hub transitions into the stepped-down state.
    pub fn subscribe(&self) -> watch::Receiver<Option<StepDownInfo>> {
        self.tx.subscribe()
    }

    /// Record step-down. Returns `true` if this call caused the transition from active to stepped down.
    pub(crate) fn mark_stepped_down(&self, info: StepDownInfo) -> bool {
        let mut transitioned = false;
        self.tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(info);
                transitioned = true;
                true
            }
        });
        transitioned
    }
}

/// Pure step-down predicate: returns `true` only when `remote` is the same account,
/// a different `hub_device_id`, and a strictly higher `epoch`.
pub fn should_step_down(
    local_account: Option<&str>,
    local_hub_device_id: &str,
    local_epoch: u64,
    remote: &HubInfo,
) -> bool {
    let Some(local_acct) = local_account.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let Some(remote_acct) = remote.account.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    local_acct.eq_ignore_ascii_case(remote_acct)
        && !remote.hub_device_id.is_empty()
        && remote.hub_device_id != local_hub_device_id
        && remote.epoch > local_epoch
}

/// Build the HTTP client used for `/v1/hub` step-down checks.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(FETCH_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Fetch `<base_url>/v1/hub` and parse it as [`HubInfo`].
pub async fn fetch_hub_info(client: &reqwest::Client, base_url: &str) -> Result<HubInfo> {
    let url = format!("{}/v1/hub", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await.with_context(|| format!("fetching {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("{url} answered with HTTP {status}");
    }
    resp.json::<HubInfo>().await.with_context(|| format!("decoding HubInfo from {url}"))
}

/// Run one step-down check against this hub's public address (configured or default).
pub async fn check_hub(hub: &Arc<Hub>) -> CheckOutcome {
    let client = http_client();
    check_hub_with_client(hub, &client).await
}

/// Run one step-down check using an existing [`reqwest::Client`].
pub async fn check_hub_with_client(hub: &Arc<Hub>, client: &reqwest::Client) -> CheckOutcome {
    if let Some(info) = hub.stepdown.info() {
        return CheckOutcome::SteppedDown(info);
    }
    let Some(owner) = hub.db.owner() else {
        return CheckOutcome::NoOwner;
    };
    let (local_hub_device_id, local_epoch) = match hub.db.ensure_hub_device() {
        Ok(pair) => pair,
        Err(error) => {
            tracing::warn!("stepdown: could not read local hub device state: {error:#}");
            return CheckOutcome::KeepServing;
        }
    };
    let address = hub.db.public_address();
    let remote = match fetch_hub_info(client, &address).await {
        Ok(info) => info,
        Err(error) => {
            tracing::debug!(address = %address, "stepdown: public address unreachable: {error:#}");
            return CheckOutcome::Unreachable(error.to_string());
        }
    };

    let local_account = crate::server::account_id(&owner.provider, &owner.sub);
    if should_step_down(Some(&local_account), &local_hub_device_id, local_epoch, &remote) {
        let info = StepDownInfo {
            account: local_account,
            local_hub_device_id,
            local_epoch,
            peer_hub_device_id: remote.hub_device_id,
            peer_epoch: remote.epoch,
            address,
            detected_at: crate::db::now(),
        };
        if hub.stepdown.mark_stepped_down(info.clone()) {
            tracing::warn!(
                account = %info.account,
                local_hub_device_id = %info.local_hub_device_id,
                local_epoch = info.local_epoch,
                peer_hub_device_id = %info.peer_hub_device_id,
                peer_epoch = info.peer_epoch,
                address = %info.address,
                "hub replaced by a newer hub; stepping down and stopping cloudflared"
            );
        }
        hub.tunnel.stop_and_wait(STOP_TUNNEL_TIMEOUT).await;
        return CheckOutcome::SteppedDown(info);
    }

    CheckOutcome::KeepServing
}

/// Background loop started by [`crate::host`]: checks on startup and every [`DEFAULT_CHECK_INTERVAL`].
pub async fn stepdown_loop(hub: Arc<Hub>, shutdown: watch::Receiver<bool>) {
    run_loop(hub, DEFAULT_CHECK_INTERVAL, shutdown).await;
}

/// Configurable step-down loop (used directly in tests with a short interval).
pub async fn run_loop(hub: Arc<Hub>, interval: Duration, mut shutdown: watch::Receiver<bool>) {
    let client = http_client();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                if *shutdown.borrow() {
                    break;
                }
                if let CheckOutcome::SteppedDown(_) = check_hub_with_client(&hub, &client).await {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::routing::get;
    use axum::{Json, Router};
    use tokio::net::TcpListener;

    use crate::db::{Db, random_hex};
    use crate::google_auth::Owner;

    fn make_hub(account: Option<&str>) -> (Arc<Hub>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("grouper-stepdown-test-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let _ = db.ensure_hub_device().unwrap();
        if let Some(email) = account {
            db.set_owner_if_none(&Owner {
                provider: "google".into(),
                sub: "sub-123".into(),
                email: email.into(),
                name: "Test Owner".into(),
                claimed_at: crate::db::now(),
            })
            .unwrap();
        }
        (Hub::new(db, dir.clone()), dir)
    }

    async fn start_fake_peer(initial: HubInfo) -> (String, Arc<Mutex<HubInfo>>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(Mutex::new(initial));
        let peer_state = state.clone();
        let app = Router::new().route(
            "/v1/hub",
            get(move || {
                let s = peer_state.clone();
                async move { Json(s.lock().unwrap().clone()) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), state, task)
    }

    fn peer_info(account: Option<&str>, hub_device_id: &str, epoch: u64) -> HubInfo {
        HubInfo {
            hub_id: "peer-hub-id".into(),
            hub_version: "0.1.0".into(),
            protocol_version: core_protocol::PROTOCOL_VERSION,
            account: account.map(String::from),
            hub_device_id: hub_device_id.into(),
            epoch,
            key_sha256: None,
        }
    }

    #[test]
    fn predicate_checks_account_device_and_strictly_higher_epoch() {
        let same_acct_higher = peer_info(Some("oscar@example.com"), "dev-b", 3);
        assert!(should_step_down(Some("oscar@example.com"), "dev-a", 2, &same_acct_higher));
        // Case-insensitive email matching.
        assert!(should_step_down(Some("Oscar@Example.COM"), "dev-a", 2, &same_acct_higher));

        // Equal or lower epoch: never step down.
        assert!(!should_step_down(Some("oscar@example.com"), "dev-a", 3, &same_acct_higher));
        assert!(!should_step_down(Some("oscar@example.com"), "dev-a", 4, &same_acct_higher));

        // Same hub_device_id: never step down even with higher epoch.
        let same_dev = peer_info(Some("oscar@example.com"), "dev-a", 10);
        assert!(!should_step_down(Some("oscar@example.com"), "dev-a", 1, &same_dev));

        // Different account: never step down.
        let other_acct = peer_info(Some("stranger@example.com"), "dev-b", 10);
        assert!(!should_step_down(Some("oscar@example.com"), "dev-a", 1, &other_acct));

        // Missing account or empty remote hub_device_id: never step down.
        assert!(!should_step_down(None, "dev-a", 1, &same_acct_higher));
        assert!(!should_step_down(Some("oscar@example.com"), "dev-a", 1, &peer_info(None, "dev-b", 10)));
        assert!(!should_step_down(Some("oscar@example.com"), "dev-a", 1, &peer_info(Some("oscar@example.com"), "", 10)));
    }

    #[tokio::test]
    async fn fake_peer_higher_epoch_steps_down_while_equal_lower_and_unreachable_do_not() {
        let (hub, dir) = make_hub(Some("oscar@example.com"));
        // Bump local epoch to 2 by assigning another device as hub.
        let (other_dev, _) = hub.db.add_device("mac", "macos", false).unwrap();
        let local_epoch = hub.db.set_hub_device_id(&other_dev.id).unwrap();
        assert_eq!(local_epoch, 2);
        let local_hub_dev = hub.db.hub_device_id();

        // 1. Unreachable address must not step down.
        let dead_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead_listener.local_addr().unwrap();
        drop(dead_listener);
        hub.db.set_public_address(Some(&format!("http://{dead_addr}"))).unwrap();
        let outcome = check_hub(&hub).await;
        assert!(matches!(outcome, CheckOutcome::Unreachable(_)), "expected Unreachable, got {outcome:?}");
        assert!(!hub.is_stepped_down());

        // 2. Start fake peer hub with lower epoch (1 < 2): must not step down.
        let (peer_url, peer_state, peer_task) =
            start_fake_peer(peer_info(Some(&crate::server::account_id("google", "sub-123")), "remote-dev", 1)).await;
        hub.db.set_public_address(Some(&peer_url)).unwrap();
        assert_eq!(check_hub(&hub).await, CheckOutcome::KeepServing);
        assert!(!hub.is_stepped_down());

        // 3. Equal epoch (2 == 2): must not step down.
        peer_state.lock().unwrap().epoch = 2;
        assert_eq!(check_hub(&hub).await, CheckOutcome::KeepServing);
        assert!(!hub.is_stepped_down());

        // 4. Different account with higher epoch (5 > 2): must not step down.
        {
            let mut s = peer_state.lock().unwrap();
            s.account = Some("someone-else@example.com".into());
            s.epoch = 5;
        }
        assert_eq!(check_hub(&hub).await, CheckOutcome::KeepServing);
        assert!(!hub.is_stepped_down());

        // 5. Same account, same hub_device_id, higher epoch (5 > 2): must not step down.
        {
            let mut s = peer_state.lock().unwrap();
            s.account = Some(crate::server::account_id("google", "sub-123"));
            s.hub_device_id = local_hub_dev.clone();
            s.epoch = 5;
        }
        assert_eq!(check_hub(&hub).await, CheckOutcome::KeepServing);
        assert!(!hub.is_stepped_down());

        // 6. Same account, different hub_device_id, higher epoch (3 > 2): steps down!
        {
            let mut s = peer_state.lock().unwrap();
            s.hub_device_id = "new-hub-device".into();
            s.epoch = 3;
        }
        let mut sub = hub.stepdown.subscribe();
        let outcome = check_hub(&hub).await;
        match outcome {
            CheckOutcome::SteppedDown(info) => {
                assert_eq!(info.peer_hub_device_id, "new-hub-device");
                assert_eq!(info.peer_epoch, 3);
                assert_eq!(info.local_hub_device_id, local_hub_dev);
                assert_eq!(info.local_epoch, 2);
                assert_eq!(info.account, crate::server::account_id("google", "sub-123"));
            }
            other => panic!("expected SteppedDown, got {other:?}"),
        }
        assert!(hub.is_stepped_down());
        assert_eq!(sub.borrow_and_update().as_ref().unwrap().peer_epoch, 3);

        peer_task.abort();
        drop(hub);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn periodic_loop_detects_replacement_and_exits() {
        let (hub, dir) = make_hub(Some("oscar@example.com"));
        let (peer_url, peer_state, peer_task) =
            start_fake_peer(peer_info(Some(&crate::server::account_id("google", "sub-123")), "new-pc", 1)).await;
        hub.db.set_public_address(Some(&peer_url)).unwrap();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut sub = hub.stepdown.subscribe();
        let loop_task = tokio::spawn(run_loop(hub.clone(), Duration::from_millis(25), shutdown_rx));

        // While peer epoch is equal (1 == 1), hub stays active.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!hub.is_stepped_down());

        // Peer advances to epoch 2 -> loop detects it, marks stepped down, and exits cleanly.
        peer_state.lock().unwrap().epoch = 2;
        tokio::time::timeout(Duration::from_secs(3), sub.wait_for(|v| v.is_some()))
            .await
            .expect("step-down triggered")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), loop_task)
            .await
            .expect("loop exited after step-down")
            .unwrap();
        assert!(hub.is_stepped_down());

        let _ = shutdown_tx.send(true);
        peer_task.abort();
        drop(hub);
        std::fs::remove_dir_all(&dir).ok();
    }
}
