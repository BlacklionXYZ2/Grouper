//! In-process `core-client` device node (`grouper connect` equivalent) so this PC runs threads.
//!
//! Security boundary (`docs/local-first-design.md` §3): the desktop install uses two separate
//! device rows — an admin client for the UI and a non-admin node row (`install_kind = "node"`)
//! here — so a thread shell never holds an admin credential.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_client::core_protocol::{Capability, DeviceInfo, default_input_schema};
use core_client::{Events, Status, ToolHandler};
use hub::db::{Install, NODE_INSTALL};
use hub::host::HubHandle;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::settings::{DesktopSettings, machine_device_name};

const NODE_TOKEN_ENV: &str = "GROUPER_DESKTOP_NODE_TOKEN";
const WORKSPACE_ENV: &str = "GROUPER_WORKSPACE";
const NO_SHELL_ENV: &str = "GROUPER_DESKTOP_NO_SHELL";
const USAGE_PROBES_ENV: &str = "GROUPER_USAGE_PROBES";
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

pub struct NodeHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl NodeHandle {
    pub fn start(hub_url: &str, token: String, app_data_dir: &Path) -> Self {
        Self::start_with_options(hub_url, token, app_data_dir, None, None)
    }

    fn start_with_options(
        hub_url: &str,
        token: String,
        app_data_dir: &Path,
        mcp_config: Option<PathBuf>,
        folders_config: Option<&Path>,
    ) -> Self {
        let workspace = std::env::var_os(WORKSPACE_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| app_data_dir.join("node-workspaces"));

        let allow_shell = !env_flag(NO_SHELL_ENV);
        let usage_probes = env_flag(USAGE_PROBES_ENV);
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
            usage_probes,
        };

        let (shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            core_client::run(config, Arc::new(DesktopTools), Arc::new(DesktopNodeEvents), rx).await;
        });
        Self { shutdown, task }
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

struct DesktopNodeEvents;

impl Events for DesktopNodeEvents {
    fn status(&self, status: Status) {
        match status {
            Status::Connecting => tracing::debug!("desktop node connecting to hub"),
            Status::Connected { device_id } => tracing::info!(device = %device_id, "desktop node online"),
            Status::Disconnected { reason } => tracing::info!("desktop node disconnected: {reason}"),
            Status::Unauthorized => tracing::warn!("desktop node token rejected by hub"),
        }
    }

    fn presence(&self, _devices: Vec<DeviceInfo>) {}

    fn thread(&self, thread_id: &str, title: &str, state: &str) {
        tracing::info!(thread = %thread_id, title = %title, state = %state, "desktop node thread");
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
}
