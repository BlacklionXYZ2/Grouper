//! `grouper hub …`: run and manage a local Grouper hub on Linux or any headless computer.
//!
//! - `grouper hub init`: prepare the local hub data directory (`<local data dir>/grouper/hub` by
//!   default, overridable with `--data`).
//! - `grouper hub run`: start the embedded hub in local mode (`hub::host::start_with_install`)
//!   alongside an in-process non-admin `core-client` node so this machine can both host the hub
//!   and run threads.
//! - `grouper hub status`: query `GET /v1/hub` and (when an admin token is available) `GET /v1/tunnel`.
//! - `grouper hub move <device>` / `grouper hub take`: direct the user to confirm the move in the
//!   web UI (`Devices → Change hub…`), because `POST /v1/hub/move` requires a fresh Google sign-in
//!   and the CLI does not have a Google OAuth client yet.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use core_client::core_protocol::{Capability, DeviceInfo, HubInfo, default_input_schema};
use core_client::{Events, Status, ToolHandler};
use hub::db::{Db, Install, NODE_INSTALL};
use hub::host::HubHandle;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::credentials;

/// Default loopback address bound by `grouper hub run` (matches the Cloudflare tunnel target).
pub const DEFAULT_BIND: &str = "127.0.0.1:47600";
/// Default local hub base URL queried by `grouper hub status` when `--hub` / `GROUPER_HUB` is unset.
pub const DEFAULT_LOCAL_HUB_URL: &str = "http://127.0.0.1:47600";

const NODE_STOP_GRACE: Duration = Duration::from_secs(2);
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Subcommand)]
pub enum HubCmd {
    /// Set up a local hub data directory (default: <local data dir>/grouper/hub, e.g. ~/.local/share/grouper/hub).
    Init {
        /// Hub data directory (default: platform local data dir + grouper/hub).
        #[arg(long, env = "GROUPER_DATA")]
        data: Option<PathBuf>,
    },
    /// Run the embedded hub in local mode plus an in-process device node on this machine.
    Run {
        /// Hub data directory (default: platform local data dir + grouper/hub).
        #[arg(long, env = "GROUPER_DATA")]
        data: Option<PathBuf>,
        /// Socket address to bind (default: 127.0.0.1:47600).
        #[arg(long, default_value = DEFAULT_BIND)]
        bind: SocketAddr,
        /// Where thread workspaces go for this machine's in-process node.
        #[arg(long, env = "GROUPER_WORKSPACE")]
        workspace: Option<PathBuf>,
        /// Don't offer run_shell to threads running on this machine's node.
        #[arg(long)]
        no_shell: bool,
        /// MCP servers to run here (Claude-style mcp.json). Default: <config dir>/grouper/mcp.json.
        #[arg(long, env = "GROUPER_MCP_CONFIG")]
        mcp_config: Option<PathBuf>,
        /// A scoped folder threads may reach as `@name/…`: name=path[:ro|rw] (read-only unless :rw).
        #[arg(long = "folder", value_name = "NAME=PATH[:ro|rw]")]
        folders: Vec<String>,
        /// Scoped folders file. Default: <config dir>/grouper/folders.json.
        #[arg(long, env = "GROUPER_FOLDERS_CONFIG")]
        folders_config: Option<PathBuf>,
        /// Enable read-only subscription usage probes (Claude Code, Codex).
        #[arg(long, env = "GROUPER_USAGE_PROBES")]
        usage_probes: bool,
    },
    /// Show whether this machine's hub is running, its identity (account, hub_device_id, epoch) and tunnel status.
    Status,
    /// Move the hub to another device of your account (opens/prints the web UI confirmation link).
    Move {
        /// Target device name or id.
        device: String,
    },
    /// Move the hub onto this machine (prints the web UI confirmation link).
    Take,
}

/// Default hub data directory: `<local data dir>/grouper/hub` (e.g. `~/.local/share/grouper/hub` on Linux).
pub fn default_data_dir() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|d| d.join("grouper").join("hub"))
        .context("the operating system did not provide a local data directory; pass --data <dir>")
}

fn resolve_data_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    match arg {
        Some(dir) => Ok(dir),
        None => default_data_dir(),
    }
}

fn machine_device_name() -> String {
    let raw = crate::hostname();
    let cleaned: String = raw.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    if cleaned.is_empty() || cleaned.eq_ignore_ascii_case("hub") {
        "Grouper Host".into()
    } else {
        cleaned
    }
}

/// Result of `grouper hub init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    pub data_dir: PathBuf,
    pub hub_device_id: String,
    pub epoch: u64,
    pub already_existed: bool,
}

/// Create and initialize a local hub data directory (`grouper.db`, master key, and initial host device row).
pub fn init_data_dir(data_dir: &Path, install_id: Option<&str>) -> Result<InitResult> {
    let already_existed = data_dir.join("grouper.db").is_file();
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating hub data directory {}", data_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", data_dir.display()))?;
    }
    let _lock = hub::backup::lock_data_dir(data_dir)
        .with_context(|| format!("locking hub data directory {}", data_dir.display()))?;
    let db = Db::open(data_dir).with_context(|| format!("initializing hub database in {}", data_dir.display()))?;
    let (hub_device_id, epoch) = db
        .ensure_hub_device_for_install(install_id)
        .context("initializing hub device identity")?;
    Ok(InitResult {
        data_dir: data_dir.to_path_buf(),
        hub_device_id,
        epoch,
        already_existed,
    })
}

/// Options for running the embedded hub + in-process node.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    pub install_id: String,
    pub workspace: Option<PathBuf>,
    pub allow_shell: bool,
    pub mcp_config: Option<PathBuf>,
    pub folders: Vec<String>,
    pub folders_config: Option<PathBuf>,
    pub usage_probes: bool,
}

/// Live handles for `grouper hub run` (embedded hub + in-process non-admin node).
pub struct RunningLocalHub {
    pub hub_handle: HubHandle,
    pub hub_url: String,
    pub node_device_id: String,
    node_shutdown: watch::Sender<bool>,
    node_task: JoinHandle<()>,
}

impl RunningLocalHub {
    /// Start the embedded local hub and connect an in-process non-admin `core-client` node to it.
    pub async fn start(opts: RunOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.data_dir)
            .with_context(|| format!("creating hub data directory {}", opts.data_dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&opts.data_dir, std::fs::Permissions::from_mode(0o700));
        }

        let hub_handle = hub::host::start_with_install(&opts.data_dir, &[opts.bind], Some(&opts.install_id))
            .await
            .with_context(|| format!("starting embedded hub on {}", opts.bind))?;
        let bound = hub_handle.binds()[0];
        let hub_url = format!("http://{bound}");

        let hub = hub_handle.hub().context("embedded hub is not running")?;
        let name = machine_device_name();
        let issued = hub
            .db
            .issue_device(
                &name,
                std::env::consts::OS,
                false,
                Some(Install { id: &opts.install_id, kind: NODE_INSTALL }),
                Some(hub::server::MAX_DEVICES),
            )
            .context("issuing local node device token")?;
        let node_device_id = issued.device.id.clone();

        let workspace = opts.workspace.unwrap_or_else(|| opts.data_dir.join("node-workspaces"));
        let folders = core_client::folders::load(opts.folders_config.as_deref(), &opts.folders)
            .map_err(anyhow::Error::msg)
            .context("loading scoped folders")?;

        let config = core_client::Config {
            hub_url: hub_url.clone(),
            token: issued.token,
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
            allow_shell: opts.allow_shell,
            mcp_config: opts.mcp_config,
            folders,
            usage_probes: opts.usage_probes,
        };

        let (node_shutdown, node_rx) = watch::channel(false);
        let node_task = tokio::spawn(async move {
            core_client::run(config, Arc::new(HubNodeTools), Arc::new(HubNodeEvents), node_rx).await;
        });

        Ok(Self {
            hub_handle,
            hub_url,
            node_device_id,
            node_shutdown,
            node_task,
        })
    }

    /// Stop the in-process node and then shut down the embedded hub cleanly.
    pub async fn stop(mut self) {
        let _ = self.node_shutdown.send(true);
        if tokio::time::timeout(NODE_STOP_GRACE, &mut self.node_task).await.is_err() {
            self.node_task.abort();
        }
        self.hub_handle.stop().await;
    }
}

struct HubNodeTools;

impl ToolHandler for HubNodeTools {
    fn call(&self, tool: &str, args: &str) -> std::result::Result<String, String> {
        match tool {
            "echo" => Ok(args.to_string()),
            "system_info" => Ok(json!({
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "hostname": machine_device_name(),
                "client": "grouper-cli",
                "version": core_client::CLIENT_VERSION,
            })
            .to_string()),
            _ => Err(format!("unknown tool {tool}")),
        }
    }
}

struct HubNodeEvents;

impl Events for HubNodeEvents {
    fn status(&self, status: Status) {
        match status {
            Status::Connecting => tracing::debug!("in-process node connecting to hub"),
            Status::Connected { device_id } => tracing::info!(device = %device_id, "in-process node online"),
            Status::Disconnected { reason } => tracing::info!("in-process node disconnected: {reason}"),
            Status::Unauthorized => tracing::warn!("in-process node token rejected by hub"),
        }
    }

    fn presence(&self, _devices: Vec<DeviceInfo>) {}

    fn thread(&self, thread_id: &str, title: &str, state: &str) {
        tracing::info!(thread = %thread_id, title = %title, state = %state, "in-process node thread");
    }
}

/// Subset of the `GET /v1/tunnel` admin response shown by `grouper hub status`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TunnelStatusPayload {
    pub status: String,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub cloudflared_path: Option<String>,
    #[serde(default)]
    pub restart_count: u64,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token_set: bool,
    #[serde(default)]
    pub public_address: String,
    #[serde(default)]
    pub stepped_down: bool,
}

/// Outcome of querying `GET /v1/tunnel` during `grouper hub status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelReport {
    Ok(TunnelStatusPayload),
    NoToken,
    AdminRequired,
    Error(String),
}

/// Result of `grouper hub status`.
#[derive(Debug, Clone, PartialEq)]
pub struct HubStatusReport {
    pub hub_url: String,
    pub running: bool,
    pub info: Option<HubInfo>,
    pub tunnel: Option<TunnelReport>,
    pub unreachable_error: Option<String>,
}

/// Query `GET /v1/hub` and (when `token` is provided) `GET /v1/tunnel`.
pub async fn fetch_status(hub_url: &str, token: Option<&str>) -> Result<HubStatusReport> {
    let base = hub_url.trim().trim_end_matches('/').to_string();
    let http = reqwest::Client::builder().timeout(STATUS_TIMEOUT).build()?;

    let resp = match http.get(format!("{base}/v1/hub")).send().await {
        Ok(r) => r,
        Err(err) => {
            return Ok(HubStatusReport {
                hub_url: base,
                running: false,
                info: None,
                tunnel: None,
                unreachable_error: Some(err.to_string()),
            });
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("GET {base}/v1/hub failed ({status}): {body}");
    }
    let info: HubInfo = resp.json().await.context("parsing /v1/hub response")?;

    let tunnel = match token.map(str::trim).filter(|t| !t.is_empty()) {
        None => TunnelReport::NoToken,
        Some(tok) => match http.get(format!("{base}/v1/tunnel")).bearer_auth(tok).send().await {
            Ok(r) if r.status().is_success() => match r.json::<TunnelStatusPayload>().await {
                Ok(payload) => TunnelReport::Ok(payload),
                Err(err) => TunnelReport::Error(format!("invalid /v1/tunnel JSON: {err}")),
            },
            Ok(r) if matches!(r.status(), reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
                TunnelReport::AdminRequired
            }
            Ok(r) => TunnelReport::Error(format!("HTTP {}", r.status())),
            Err(err) => TunnelReport::Error(err.to_string()),
        },
    };

    Ok(HubStatusReport {
        hub_url: base,
        running: true,
        info: Some(info),
        tunnel: Some(tunnel),
        unreachable_error: None,
    })
}

/// Format a [`HubStatusReport`] for human display.
pub fn format_status(report: &HubStatusReport) -> String {
    let mut out = String::new();
    if !report.running {
        out.push_str(&format!("hub:           not running ({})\n", report.hub_url));
        if let Some(err) = &report.unreachable_error {
            out.push_str(&format!("detail:        {err}\n"));
        }
        return out;
    }

    let info = report.info.as_ref().expect("running status report includes HubInfo");
    out.push_str(&format!("hub:           running ({})\n", report.hub_url));
    out.push_str(&format!(
        "account:       {}\n",
        info.account.as_deref().unwrap_or("(unclaimed)")
    ));
    out.push_str(&format!("hub_device_id: {}\n", info.hub_device_id));
    out.push_str(&format!("epoch:         {}\n", info.epoch));
    out.push_str(&format!("version:       {}\n", info.hub_version));

    match &report.tunnel {
        Some(TunnelReport::Ok(t)) => {
            let mut notes = Vec::new();
            notes.push(if t.enabled { "enabled" } else { "disabled" });
            notes.push(if t.token_set { "token set" } else { "no token" });
            if t.stepped_down {
                notes.push("stepped down");
            }
            out.push_str(&format!("tunnel:        {} ({})\n", t.status, notes.join(", ")));
            if !t.public_address.is_empty() {
                out.push_str(&format!("public_url:    {}\n", t.public_address));
            }
            if let Some(err) = &t.last_error {
                out.push_str(&format!("tunnel_error:  {err}\n"));
            }
        }
        Some(TunnelReport::NoToken) => {
            out.push_str("tunnel:        unavailable (pass an admin --token or GROUPER_TOKEN to check /v1/tunnel)\n");
        }
        Some(TunnelReport::AdminRequired) => {
            out.push_str("tunnel:        unavailable (admin token required for /v1/tunnel)\n");
        }
        Some(TunnelReport::Error(msg)) => {
            out.push_str(&format!("tunnel:        error ({msg})\n"));
        }
        None => {}
    }

    out
}

/// Web UI URL for **Devices → Change hub…** on `hub_url`.
pub fn change_hub_url(hub_url: &str) -> String {
    format!("{}/#/devices", hub_url.trim().trim_end_matches('/'))
}

/// Message printed by `grouper hub move <device>` (never sends a request without a fresh Google ID token).
pub fn format_move_guidance(hub_url: &str, target_device: &str) -> String {
    let url = change_hub_url(hub_url);
    format!(
        "Moving the hub to \"{target_device}\" (POST /v1/hub/move) requires a fresh Google sign-in.\n\
         The Grouper CLI does not have a Google sign-in client yet, so no request was sent.\n\
         \n\
         Confirm the move in the web UI (Devices → Change hub…):\n\
         \n    {url}\n"
    )
}

/// Message printed by `grouper hub take` (never sends a request without a fresh Google ID token).
pub fn format_take_guidance(hub_url: &str, this_device: &str) -> String {
    let url = change_hub_url(hub_url);
    format!(
        "Moving the hub to this machine (\"{this_device}\", POST /v1/hub/move) requires a fresh Google sign-in.\n\
         The Grouper CLI does not have a Google sign-in client yet, so no request was sent.\n\
         \n\
         Make sure this machine's node is online, then confirm the move in the web UI (Devices → Change hub…):\n\
         \n    {url}\n"
    )
}

fn resolve_status_target(cli_hub: Option<&str>) -> String {
    cli_hub
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("GROUPER_HUB")
                .ok()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_LOCAL_HUB_URL.to_string())
}

fn resolve_optional_token(hub_url: &str, cli_token: Option<&str>) -> Option<String> {
    if let Some(tok) = cli_token.map(str::trim).filter(|t| !t.is_empty()) {
        return Some(tok.to_string());
    }
    let Ok(Some(saved)) = credentials::load(false) else {
        return None;
    };
    if credentials::normalize_hub(&saved.hub) != credentials::normalize_hub(hub_url) {
        return None;
    }
    credentials::load(true).ok().flatten().and_then(|c| c.token)
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Dispatch `grouper hub …`.
pub async fn run(cli_hub: Option<&str>, cli_token: Option<&str>, cmd: HubCmd) -> Result<()> {
    match cmd {
        HubCmd::Init { data } => {
            let data_dir = resolve_data_dir(data)?;
            let install_id = credentials::install_id()?;
            let res = init_data_dir(&data_dir, Some(&install_id))?;
            if res.already_existed {
                println!("Hub data directory already initialized: {}", res.data_dir.display());
            } else {
                println!("Initialized hub data directory: {}", res.data_dir.display());
            }
            println!("hub_device_id: {}", res.hub_device_id);
            println!("epoch:         {}", res.epoch);
            println!(
                "Start the hub with `grouper hub run --data {}` (or install deploy/grouper-hub.service).",
                res.data_dir.display()
            );
            Ok(())
        }
        HubCmd::Run {
            data,
            bind,
            workspace,
            no_shell,
            mcp_config,
            folders,
            folders_config,
            usage_probes,
        } => {
            let data_dir = resolve_data_dir(data)?;
            let install_id = credentials::install_id()?;
            let running = RunningLocalHub::start(RunOptions {
                data_dir: data_dir.clone(),
                bind,
                install_id,
                workspace,
                allow_shell: !no_shell,
                mcp_config,
                folders,
                folders_config,
                usage_probes,
            })
            .await?;

            println!("grouper hub listening on {} (data: {})", running.hub_url, data_dir.display());
            println!("in-process node device: {}", running.node_device_id);
            println!("Press Ctrl+C to stop.");

            wait_for_shutdown_signal().await;
            println!("Shutting down hub and in-process node…");
            running.stop().await;
            Ok(())
        }
        HubCmd::Status => {
            let hub_url = resolve_status_target(cli_hub);
            let token = resolve_optional_token(&hub_url, cli_token);
            let report = fetch_status(&hub_url, token.as_deref()).await?;
            print!("{}", format_status(&report));
            Ok(())
        }
        HubCmd::Move { device } => {
            let hub_url = credentials::resolve_hub(cli_hub)?;
            print!("{}", format_move_guidance(&hub_url, &device));
            Ok(())
        }
        HubCmd::Take => {
            let hub_url = credentials::resolve_hub(cli_hub)?;
            let this_device = machine_device_name();
            print!("{}", format_take_guidance(&hub_url, &this_device));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn temp_root(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", hub::db::random_hex(6)))
    }

    #[test]
    fn init_creates_data_dir_and_is_idempotent() {
        let root = temp_root("grouper-cli-hub-init");
        let data = root.join("nested").join("hub");
        assert!(!data.exists());

        let install_id = "cli_hub_init_test_0123456789abcd";
        let first = init_data_dir(&data, Some(install_id)).unwrap();
        assert!(data.is_dir(), "init must create the hub data directory");
        assert!(data.join("grouper.db").is_file(), "init must create grouper.db");
        assert!(!first.already_existed);
        assert!(!first.hub_device_id.is_empty());
        assert_eq!(first.epoch, 1);

        // Running init again keeps the same hub_device_id and epoch.
        let second = init_data_dir(&data, Some(install_id)).unwrap();
        assert!(second.already_existed);
        assert_eq!(second.hub_device_id, first.hub_device_id);
        assert_eq!(second.epoch, first.epoch);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn init_command_creates_dir_using_temp_credentials_file() {
        let _lock = credentials::test_env::lock();
        let root = temp_root("grouper-cli-hub-init-cmd");
        let creds = root.join("credentials.json");
        let data = root.join("hub-data");
        let _creds_guard = credentials::test_env::VarGuard::set("GROUPER_CREDENTIALS_FILE", &creds);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run(None, None, HubCmd::Init { data: Some(data.clone()) }))
            .unwrap();
        assert!(data.is_dir(), "grouper hub init must create --data directory");
        assert!(data.join("grouper.db").is_file(), "grouper hub init must create grouper.db");
        assert!(creds.is_file(), "install_id should be saved only in the temp credentials file");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn status_reports_running_hub_identity_and_tunnel_status() {
        let root = temp_root("grouper-cli-hub-status");
        let data = root.join("hub");
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let install_id = "cli_hub_status_test_0123456789ab";

        let handle = hub::host::start_with_install(&data, &[loopback], Some(install_id))
            .await
            .unwrap();
        let hub_url = format!("http://{}", handle.binds()[0]);
        let hub_arc = handle.hub().unwrap();
        let expected_hub_device_id = hub_arc.db.hub_device_id();

        // Issue an admin token and a plain device token for testing /v1/tunnel authorization.
        let (_admin_dev, admin_token) = hub_arc.db.add_device("CLI Admin", "linux", true).unwrap();
        let (_plain_dev, plain_token) = hub_arc.db.add_device("CLI Plain", "linux", false).unwrap();

        // 1. Without a token: /v1/hub succeeds, tunnel notes that an admin token is needed.
        let no_tok = fetch_status(&hub_url, None).await.unwrap();
        assert!(no_tok.running);
        let info = no_tok.info.as_ref().unwrap();
        assert_eq!(info.account, None);
        assert_eq!(info.hub_device_id, expected_hub_device_id);
        assert_eq!(info.epoch, 1);
        assert_eq!(no_tok.tunnel, Some(TunnelReport::NoToken));
        let rendered_no_tok = format_status(&no_tok);
        assert!(rendered_no_tok.contains("running"), "{rendered_no_tok}");
        assert!(rendered_no_tok.contains("(unclaimed)"), "{rendered_no_tok}");
        assert!(rendered_no_tok.contains(&expected_hub_device_id), "{rendered_no_tok}");
        assert!(rendered_no_tok.contains("epoch:         1"), "{rendered_no_tok}");

        // 2. With a non-admin token: /v1/tunnel returns 403 -> AdminRequired.
        let plain = fetch_status(&hub_url, Some(&plain_token)).await.unwrap();
        assert!(plain.running);
        assert_eq!(plain.tunnel, Some(TunnelReport::AdminRequired));
        let rendered_plain = format_status(&plain);
        assert!(rendered_plain.contains("admin token required"), "{rendered_plain}");

        // 3. With an admin token: /v1/tunnel returns full tunnel status.
        let admin = fetch_status(&hub_url, Some(&admin_token)).await.unwrap();
        assert!(admin.running);
        let Some(TunnelReport::Ok(ref tunnel)) = admin.tunnel else {
            panic!("expected TunnelReport::Ok, got {:?}", admin.tunnel);
        };
        assert_eq!(tunnel.status, "stopped");
        assert!(!tunnel.enabled);
        assert!(!tunnel.token_set);
        assert_eq!(tunnel.public_address, hub::tunnel::DEFAULT_PUBLIC_ADDRESS);
        let rendered_admin = format_status(&admin);
        assert!(rendered_admin.contains("tunnel:        stopped (disabled, no token)"), "{rendered_admin}");
        assert!(rendered_admin.contains(hub::tunnel::DEFAULT_PUBLIC_ADDRESS), "{rendered_admin}");

        // 4. After stopping the hub, status reports not running.
        handle.stop().await;
        let stopped = fetch_status(&hub_url, Some(&admin_token)).await.unwrap();
        assert!(!stopped.running);
        assert!(stopped.info.is_none());
        let rendered_stopped = format_status(&stopped);
        assert!(rendered_stopped.contains("not running"), "{rendered_stopped}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn running_local_hub_connects_in_process_node_and_stops_cleanly() {
        let root = temp_root("grouper-cli-hub-run");
        let data = root.join("hub");
        let install_id = "cli_hub_run_test_0123456789abcde";
        std::fs::create_dir_all(&root).unwrap();
        let mcp_path = root.join("mcp.json");
        let folders_path = root.join("folders.json");
        std::fs::write(&mcp_path, r#"{"mcpServers":{}}"#).unwrap();
        std::fs::write(&folders_path, r#"{"folders":{}}"#).unwrap();

        let running = RunningLocalHub::start(RunOptions {
            data_dir: data.clone(),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            install_id: install_id.into(),
            workspace: Some(root.join("workspaces")),
            allow_shell: false,
            mcp_config: Some(mcp_path),
            folders: vec![],
            folders_config: Some(folders_path),
            usage_probes: false,
        })
        .await
        .unwrap();

        let hub_arc = running.hub_handle.hub().unwrap();
        assert_eq!(running.node_device_id, hub_arc.db.hub_device_id());

        // Wait for the in-process node to come online and advertise system_info.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let devices = hub_arc.devices().await;
                if let Some(dev) = devices.iter().find(|d| d.id == running.node_device_id && d.online)
                    && dev.capabilities.iter().any(|c| c.name == "system_info")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("in-process node connected to embedded hub");

        let hub_url = running.hub_url.clone();
        running.stop().await;

        let after_stop = fetch_status(&hub_url, None).await.unwrap();
        assert!(!after_stop.running, "hub must stop serving after RunningLocalHub::stop");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn move_and_take_print_confirmation_url_and_never_send_http_request() {
        // Bind a stub TCP listener that counts any incoming connection so we prove neither
        // `move` nor `take` ever sends an HTTP request without a fresh Google sign-in token.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stub_url = format!("http://{addr}");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_bg = Arc::clone(&hits);
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                hits_bg.fetch_add(1, Ordering::SeqCst);
                let _ = stream
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        // Verify the rendered guidance directly.
        let move_msg = format_move_guidance(&stub_url, "linux-box");
        assert!(move_msg.contains(&format!("{stub_url}/#/devices")), "{move_msg}");
        assert!(move_msg.contains("Devices → Change hub"), "{move_msg}");
        assert!(move_msg.contains("linux-box"), "{move_msg}");
        assert!(move_msg.contains("Google sign-in"), "{move_msg}");

        let take_msg = format_take_guidance(&stub_url, "my-linux-host");
        assert!(take_msg.contains(&format!("{stub_url}/#/devices")), "{take_msg}");
        assert!(take_msg.contains("Devices → Change hub"), "{take_msg}");
        assert!(take_msg.contains("my-linux-host"), "{take_msg}");
        assert!(take_msg.contains("Google sign-in"), "{take_msg}");

        // Also run the actual command dispatcher with an explicit --hub pointing at the stub
        // (and no token, never touching real credentials).
        run(Some(&stub_url), None, HubCmd::Move { device: "linux-box".into() })
            .await
            .unwrap();
        run(Some(&stub_url), None, HubCmd::Take).await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "move and take must never send a request to the hub without a fresh Google token"
        );
        server.abort();
    }
}
