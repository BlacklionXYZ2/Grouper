//! In-process `core-client` device node (`grouper connect` equivalent) so this PC runs threads.
//!
//! Security boundary (`docs/local-first-design.md` §3): the desktop install uses two separate
//! device rows — an admin client for the UI and a non-admin node row (`install_kind = "node"`)
//! here — so a thread shell never holds an admin credential.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_client::core_protocol::{Capability, DeviceInfo, default_input_schema};
use core_client::usage_probes::UsageProbeSetting;
use core_client::{Events, HubMoveCommitInfo, HubMoveOfferInfo, Status, ToolHandler};
use hub::db::{Install, NODE_INSTALL};
use hub::host::HubHandle;
use hub::migrate::{MoveCommit, MoveOffer, TargetMoveOptions, execute_target_move};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::settings::{DesktopSettings, machine_device_name};

const NODE_TOKEN_ENV: &str = "GROUPER_DESKTOP_NODE_TOKEN";
const WORKSPACE_ENV: &str = "GROUPER_WORKSPACE";
const NO_SHELL_ENV: &str = "GROUPER_DESKTOP_NO_SHELL";
const USAGE_PROBES_ENV: &str = "GROUPER_USAGE_PROBES";
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

pub type OnPromotedCallback = Arc<dyn Fn(HubHandle) + Send + Sync + 'static>;

/// Target-side parameters used when this desktop install is in client mode (`AnotherComputer`)
/// and its node receives a hub move offer + commit from the current hub.
#[derive(Clone)]
pub struct MoveTargetContext {
    pub remote_hub_url: String,
    pub device_token: String,
    pub hub_data_dir: PathBuf,
    pub local_hub_bind: SocketAddr,
    pub confirm_address: String,
    pub confirm_timeout: Duration,
    pub poll_interval: Duration,
    pub on_promoted: OnPromotedCallback,
}

impl MoveTargetContext {
    pub fn new(
        remote_hub_url: String,
        device_token: String,
        hub_data_dir: PathBuf,
        local_hub_bind: SocketAddr,
        confirm_address: String,
        on_promoted: OnPromotedCallback,
    ) -> Self {
        Self {
            remote_hub_url,
            device_token,
            hub_data_dir,
            local_hub_bind,
            confirm_address,
            confirm_timeout: Duration::from_secs(120),
            poll_interval: Duration::from_secs(1),
            on_promoted,
        }
    }
}

/// Local usage-reading choice (from desktop.json); GROUPER_USAGE_PROBES overrides it when set.
static USAGE_SETTING: RwLock<Option<UsageProbeSetting>> = RwLock::new(None);

pub fn set_usage_setting(setting: UsageProbeSetting) {
    *USAGE_SETTING.write().unwrap_or_else(|e| e.into_inner()) = Some(setting);
}

fn effective_usage_setting() -> UsageProbeSetting {
    if let Ok(raw) = std::env::var(USAGE_PROBES_ENV)
        && let Ok(parsed) = UsageProbeSetting::parse(&raw)
    {
        return parsed;
    }
    USAGE_SETTING.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

/// Everything needed to start the same node again (used when the usage setting changes).
#[derive(Clone)]
struct NodeRecipe {
    hub_url: String,
    token: String,
    app_data_dir: PathBuf,
    mcp_config: Option<PathBuf>,
    folders_config: Option<PathBuf>,
    move_target: Option<MoveTargetContext>,
}

pub struct NodeHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    recipe: NodeRecipe,
}

impl NodeHandle {
    pub fn start(hub_url: &str, token: String, app_data_dir: &Path) -> Self {
        Self::start_with_options(hub_url, token, app_data_dir, None, None, None)
    }

    pub fn start_client(
        hub_url: &str,
        token: String,
        app_data_dir: &Path,
        move_target: MoveTargetContext,
    ) -> Self {
        Self::start_with_options(hub_url, token, app_data_dir, None, None, Some(move_target))
    }

    fn start_with_options(
        hub_url: &str,
        token: String,
        app_data_dir: &Path,
        mcp_config: Option<PathBuf>,
        folders_config: Option<&Path>,
        move_target: Option<MoveTargetContext>,
    ) -> Self {
        let workspace = std::env::var_os(WORKSPACE_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| app_data_dir.join("node-workspaces"));

        let allow_shell = !env_flag(NO_SHELL_ENV);
        // On by default (usage belongs to the whole account). The local off switch is the setting
        // in desktop.json; GROUPER_USAGE_PROBES overrides it when set (for testing).
        let usage = effective_usage_setting();
        let folders = match core_client::folders::load(folders_config, &[]) {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!("skipping scoped folders config: {err}");
                Vec::new()
            }
        };

        let config = core_client::Config {
            hub_url: hub_url.trim_end_matches('/').to_string(),
            token,
            capabilities: vec![
                Capability {
                    name: "echo".into(),
                    description: "Returns its arguments".into(),
                    input_schema: json!({"type": "object", "additionalProperties": true}),
                },
                Capability {
                    name: "system_info".into(),
                    description: "OS, architecture and hostname".into(),
                    input_schema: default_input_schema(),
                },
            ],
            workspace_root: Some(workspace),
            allow_shell,
            mcp_config,
            folders,
            usage_probes: usage.enabled,
            usage_probe_providers: usage.providers,
        };
        let recipe = NodeRecipe {
            hub_url: hub_url.to_string(),
            token: config.token.clone(),
            app_data_dir: app_data_dir.to_path_buf(),
            mcp_config: config.mcp_config.clone(),
            folders_config: folders_config.map(Path::to_path_buf),
            move_target: move_target.clone(),
        };

        let events = Arc::new(DesktopNodeEvents::new(move_target));
        let (shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            core_client::run(config, Arc::new(DesktopTools), events, rx).await;
        });
        Self { shutdown, task, recipe }
    }

    /// Stop this node and start an identical one, so the hub sees the current usage advertisement.
    pub async fn restart(self) -> Self {
        let r = self.recipe.clone();
        self.stop().await;
        Self::start_with_options(&r.hub_url, r.token, &r.app_data_dir, r.mcp_config, r.folders_config.as_deref(), r.move_target)
    }

    pub async fn stop(mut self) {
        let _ = self.shutdown.send(true);
        if tokio::time::timeout(STOP_TIMEOUT, &mut self.task).await.is_err() {
            self.task.abort();
        }
    }
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

struct DesktopTools;

impl ToolHandler for DesktopTools {
    fn call(&self, tool: &str, args: &str) -> std::result::Result<String, String> {
        match tool {
            "echo" => Ok(args.to_string()),
            "system_info" => Ok(json!({
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "hostname": machine_device_name(),
                "client": "grouper-desktop",
                "version": core_client::CLIENT_VERSION,
            })
            .to_string()),
            _ => Err(format!("unknown tool {tool}")),
        }
    }
}

struct ActiveTargetMove {
    move_id: String,
    commit_tx: Option<oneshot::Sender<std::result::Result<MoveCommit, String>>>,
}

struct DesktopNodeEvents {
    move_target: Option<MoveTargetContext>,
    active_move: Arc<Mutex<Option<ActiveTargetMove>>>,
}

impl DesktopNodeEvents {
    fn new(move_target: Option<MoveTargetContext>) -> Self {
        Self {
            move_target,
            active_move: Arc::new(Mutex::new(None)),
        }
    }

    fn cancel_uncommitted_move(&self, reason: &str) {
        let tx = {
            let mut guard = self.active_move.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_mut().and_then(|m| m.commit_tx.take())
        };
        if let Some(tx) = tx {
            let _ = tx.send(Err(reason.to_string()));
        }
    }
}

impl Events for DesktopNodeEvents {
    fn status(&self, status: Status) {
        match status {
            Status::Connecting => tracing::debug!("desktop node connecting to hub"),
            Status::Connected { device_id } => tracing::info!(device = %device_id, "desktop node online"),
            Status::Disconnected { reason } => {
                tracing::info!("desktop node disconnected: {reason}");
                self.cancel_uncommitted_move(&format!("disconnected from hub before switch: {reason}"));
            }
            Status::Unauthorized => {
                tracing::warn!("desktop node token rejected by hub");
                self.cancel_uncommitted_move("node token rejected by hub");
            }
        }
    }

    fn presence(&self, _devices: Vec<DeviceInfo>) {}

    fn thread(&self, thread_id: &str, title: &str, state: &str) {
        tracing::info!(thread = %thread_id, title = %title, state = %state, "desktop node thread");
    }

    fn hub_move_offer(&self, offer: HubMoveOfferInfo) {
        let Some(ctx) = self.move_target.clone() else {
            tracing::debug!(move_id = %offer.move_id, "ignoring hub move offer because this computer is already the hub");
            return;
        };

        let (commit_tx, commit_rx) = oneshot::channel();
        {
            let mut guard = self.active_move.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_some() {
                tracing::warn!(move_id = %offer.move_id, "ignoring hub move offer while another target move is active");
                return;
            }
            *guard = Some(ActiveTargetMove {
                move_id: offer.move_id.clone(),
                commit_tx: Some(commit_tx),
            });
        }

        let active_slot = Arc::clone(&self.active_move);
        let move_id = offer.move_id.clone();
        tokio::spawn(async move {
            struct ClearSlot {
                slot: Arc<Mutex<Option<ActiveTargetMove>>>,
                move_id: String,
            }
            impl Drop for ClearSlot {
                fn drop(&mut self) {
                    let mut guard = self.slot.lock().unwrap_or_else(|e| e.into_inner());
                    if guard.as_ref().is_some_and(|m| m.move_id == self.move_id) {
                        *guard = None;
                    }
                }
            }
            let _clear = ClearSlot {
                slot: active_slot,
                move_id: move_id.clone(),
            };

            let mut opts = TargetMoveOptions::new(ctx.hub_data_dir.clone(), vec![ctx.local_hub_bind]);
            opts.confirm_address = Some(ctx.confirm_address.clone());
            opts.confirm_timeout = ctx.confirm_timeout;
            opts.poll_interval = ctx.poll_interval;

            let move_offer = MoveOffer {
                move_id: offer.move_id,
                ticket: offer.ticket,
                target_device_id: offer.target_device_id,
                current_epoch: offer.current_epoch,
                snapshot_bytes: offer.snapshot_bytes,
                snapshot_sha256: offer.snapshot_sha256,
            };

            match execute_target_move(
                &ctx.remote_hub_url,
                &ctx.device_token,
                move_offer,
                opts,
                commit_rx,
            )
            .await
            {
                Ok(new_hub_handle) => {
                    tracing::info!(
                        move_id = %move_id,
                        data_dir = %ctx.hub_data_dir.display(),
                        "hub move confirmed on this computer; switching desktop app to hub mode"
                    );
                    (ctx.on_promoted)(new_hub_handle);
                }
                Err(err) => {
                    tracing::warn!(move_id = %move_id, "target hub move did not complete: {err:#}");
                }
            }
        });
    }

    fn hub_move_commit(&self, commit: HubMoveCommitInfo) {
        let tx = {
            let mut guard = self.active_move.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_mut()
                .filter(|m| m.move_id == commit.move_id)
                .and_then(|m| m.commit_tx.take())
        };
        if let Some(tx) = tx {
            let _ = tx.send(Ok(MoveCommit {
                move_id: commit.move_id,
                ticket: commit.ticket,
                target_device_id: commit.target_device_id,
                new_epoch: commit.new_epoch,
            }));
        }
    }

    fn hub_move_abort(&self, move_id: &str, reason: &str) {
        let tx = {
            let mut guard = self.active_move.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_mut()
                .filter(|m| m.move_id == move_id)
                .and_then(|m| m.commit_tx.take())
        };
        if let Some(tx) = tx {
            let _ = tx.send(Err(reason.to_string()));
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Ensure this PC has a valid, non-admin node token on the embedded local hub, reusing any saved
/// token that still authenticates as a plain device of this install or issuing/reusing the
/// install's node row (which is also the hub row when started with `settings.install_id`).
pub fn ensure_local_node_token(
    hub_handle: &HubHandle,
    hub_url: &str,
    settings: &mut DesktopSettings,
) -> Result<String> {
    let hub = hub_handle.hub().context("embedded hub is not running")?;
    if let Ok(Some(saved)) = settings.node_token_for(hub_url)
        && let Some(auth) = hub.db.authenticate(&saved)
        && !auth.admin
        && hub.db.device_install_id(&auth.device_id).as_deref() == Some(settings.install_id.as_str())
    {
        return Ok(saved);
    }

    let name = machine_device_name();
    let issued = hub
        .db
        .issue_device(
            &name,
            std::env::consts::OS,
            false,
            Some(Install { id: &settings.install_id, kind: NODE_INSTALL }),
            Some(hub::server::MAX_DEVICES),
        )
        .context("issuing local node device token")?;
    settings.set_node_token(hub_url, &issued.token)?;
    Ok(issued.token)
}

/// Resolve or register a non-admin node token for a remote hub (`AnotherComputer` mode).
/// Returns `Ok(None)` when the user has not signed in with an admin token yet and no node token is configured.
pub async fn resolve_remote_node_token(
    remote_hub_url: &str,
    settings: &mut DesktopSettings,
) -> Result<Option<String>> {
    let base = remote_hub_url.trim_end_matches('/');
    let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;

    if let Some(env_tok) = std::env::var(NODE_TOKEN_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        ensure_plain_device_token(&http, base, &env_tok).await?;
        return Ok(Some(env_tok));
    }

    if let Ok(Some(saved)) = settings.node_token_for(base) {
        match check_remote_token_kind(&http, base, &saved).await {
            RemoteTokenCheck::PlainUsable | RemoteTokenCheck::Unreachable => return Ok(Some(saved)),
            RemoteTokenCheck::Admin | RemoteTokenCheck::Rejected => {}
        }
    }

    let Some(admin_tok) = settings.admin_token_for(base)? else {
        return Ok(None);
    };

    let resp = http
        .post(format!("{base}/v1/devices"))
        .bearer_auth(&admin_tok)
        .json(&json!({
            "name": machine_device_name(),
            "platform": std::env::consts::OS,
            "install_id": settings.install_id,
        }))
        .send()
        .await
        .with_context(|| format!("registering desktop node at {base}"))?;
    if !resp.status().is_success() {
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let msg = body["error"].as_str().unwrap_or("hub refused node registration");
        bail!("{msg}");
    }
    let body: Value = resp.json().await.context("reading device registration response")?;
    let token = body["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .context("hub returned no device token")?
        .to_string();
    settings.set_node_token(base, &token)?;
    Ok(Some(token))
}

#[derive(PartialEq, Eq)]
enum RemoteTokenCheck {
    PlainUsable,
    Admin,
    Rejected,
    Unreachable,
}

async fn check_remote_token_kind(http: &reqwest::Client, base: &str, token: &str) -> RemoteTokenCheck {
    // `/v1/providers` is admin-only: 403 proves the token is valid AND non-admin; 200 means admin; 401 means revoked.
    let Ok(resp) = http.get(format!("{base}/v1/providers")).bearer_auth(token).send().await else {
        return RemoteTokenCheck::Unreachable;
    };
    match resp.status() {
        reqwest::StatusCode::FORBIDDEN => RemoteTokenCheck::PlainUsable,
        reqwest::StatusCode::OK => RemoteTokenCheck::Admin,
        reqwest::StatusCode::UNAUTHORIZED => RemoteTokenCheck::Rejected,
        _ => RemoteTokenCheck::Unreachable,
    }
}

async fn ensure_plain_device_token(http: &reqwest::Client, base: &str, token: &str) -> Result<()> {
    match check_remote_token_kind(http, base, token).await {
        RemoteTokenCheck::Admin => bail!("{NODE_TOKEN_ENV} is an admin token; the node requires a plain device token"),
        RemoteTokenCheck::Rejected => bail!("the hub at {base} rejected {NODE_TOKEN_ENV}"),
        RemoteTokenCheck::PlainUsable | RemoteTokenCheck::Unreachable => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn in_process_node_connects_to_embedded_hub_and_serves_tools() {
        let root = std::env::temp_dir().join(format!("grouper-desktop-node-{}", hub::db::random_hex(4)));
        let hub_dir = root.join("hub");
        let app_dir = root.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();

        let mut settings = DesktopSettings::load(&app_dir).unwrap();
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let hub_handle = hub::host::start_with_install(&hub_dir, &[loopback], Some(&settings.install_id))
            .await
            .unwrap();
        let hub_url = format!("http://{}", hub_handle.binds()[0]);

        let token = ensure_local_node_token(&hub_handle, &hub_url, &mut settings).unwrap();
        settings.save(&app_dir).unwrap();

        // Re-running reuses the exact same plain token rather than minting duplicate devices.
        let token_again = ensure_local_node_token(&hub_handle, &hub_url, &mut settings).unwrap();
        assert_eq!(token, token_again);

        let hub_arc = hub_handle.hub().unwrap();
        let auth = hub_arc.db.authenticate(&token).expect("node token authenticates");
        assert!(!auth.admin, "node must never hold an admin credential");
        assert_eq!(auth.device_id, hub_arc.db.hub_device_id(), "node row and hub row are the same device");
        assert_eq!(
            hub_arc.db.device_install_id(&auth.device_id).as_deref(),
            Some(settings.install_id.as_str())
        );

        let installs = hub_arc.install_devices().await;
        assert_eq!(installs.installs.len(), 1);
        assert!(installs.installs[0].is_hub);
        assert!(!installs.installs[0].selectable);
        assert!(installs.move_targets().is_empty());

        let node = NodeHandle::start_with_options(
            &hub_url,
            token,
            &app_dir,
            Some(app_dir.join("mcp.json")),
            Some(&app_dir.join("folders.json")),
            None,
        );

        // Wait until the hub sees this PC's node online with its MCP tools listed.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let devices = hub_arc.devices().await;
                if let Some(dev) = devices.iter().find(|d| d.id == auth.device_id && d.online)
                    && dev.capabilities.iter().any(|c| c.name == "system_info")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("in-process node connected and registered capabilities");

        let call = hub_arc.call_tool(&auth.device_id, "system_info", json!({})).await.unwrap();
        assert!(call.ok, "system_info tool call succeeded: {:?}", call.output);
        let info: Value = serde_json::from_str(&call.output).unwrap();
        assert_eq!(info["client"], "grouper-desktop");

        node.stop().await;
        hub_handle.stop().await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn hub_mode_node_ignores_offers_and_client_node_routes_abort_and_disconnect() {
        let root = std::env::temp_dir().join(format!("grouper-desktop-move-ev-{}", hub::db::random_hex(4)));
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));

        // 1. Hub-mode node (`move_target: None`) ignores move offers completely.
        let hub_events = DesktopNodeEvents::new(None);
        hub_events.hub_move_offer(HubMoveOfferInfo {
            move_id: "m_ignored".into(),
            ticket: "tok".into(),
            target_device_id: "dev1".into(),
            current_epoch: 1,
            snapshot_bytes: 100,
            snapshot_sha256: "00".repeat(32),
        });
        assert!(hub_events.active_move.lock().unwrap().is_none());

        // 2. Client-mode node tracks active move and clears slot when move fails / aborts.
        let hub_handle = hub::host::start(&root.join("src-hub"), &[loopback]).await.unwrap();
        let live_url = format!("http://{}", hub_handle.binds()[0]);
        let (_dev, dev_token) = hub_handle.hub().unwrap().db.add_device("PC", "windows", false).unwrap();
        let promoted = Arc::new(Mutex::new(false));
        let promoted_flag = promoted.clone();
        let mut ctx = MoveTargetContext::new(
            live_url,
            dev_token,
            root.join("hub"),
            loopback,
            "https://grouper.clockmillbarn.uk".into(),
            Arc::new(move |_| {
                *promoted_flag.lock().unwrap() = true;
            }),
        );
        ctx.confirm_timeout = Duration::from_millis(100);
        ctx.poll_interval = Duration::from_millis(20);

        let client_events = DesktopNodeEvents::new(Some(ctx));
        client_events.hub_move_offer(HubMoveOfferInfo {
            move_id: "m_fail".into(),
            ticket: "t_unknown_ticket".into(),
            target_device_id: "dev1".into(),
            current_epoch: 1,
            snapshot_bytes: 100,
            snapshot_sha256: "00".repeat(32),
        });
        client_events.hub_move_abort("m_fail", "cancelled");

        tokio::time::timeout(Duration::from_secs(5), async {
            while client_events.active_move.lock().unwrap().is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("failed/aborted target move clears active_move slot");
        assert!(!*promoted.lock().unwrap());
        assert!(!root.join("hub").join("grouper.db").exists());

        hub_handle.stop().await;
        let _ = std::fs::remove_dir_all(&root);
    }
}
