//! Swift-facing API for the Grouper core.
//!
//! Swift creates a [`GrouperClient`] with two callback objects:
//! - [`ToolHandler`]: Rust calls it when the hub asks this device to run a tool, so
//!   Apple-only tools (EventKit, Photos, …) are written in Swift but driven by the core.
//! - [`ClientListener`]: connection status and live device presence.

use std::sync::{Arc, Mutex};

use core_client::core_protocol;
use tokio::sync::watch;

uniffi::setup_scaffolding!();

#[derive(uniffi::Record, Clone, Debug)]
pub struct Capability {
    pub name: String,
    pub description: String,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub online: bool,
    pub capabilities: Vec<Capability>,
    pub last_seen: Option<u64>,
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum ConnectionStatus {
    Connecting,
    Connected { device_id: String },
    Disconnected { reason: String },
    Unauthorized,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ToolError {
    #[error("{message}")]
    Failed { message: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for ToolError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        ToolError::Failed { message: e.reason }
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ClientError {
    #[error("{message}")]
    Invalid { message: String },
}

/// Implemented in Swift.
#[uniffi::export(with_foreign)]
pub trait ToolHandler: Send + Sync {
    fn call_tool(&self, tool: String, args_json: String) -> Result<String, ToolError>;
}

/// Implemented in Swift. Called from a background thread.
#[uniffi::export(with_foreign)]
pub trait ClientListener: Send + Sync {
    fn on_status(&self, status: ConnectionStatus);
    fn on_presence(&self, devices: Vec<Device>);
    /// A subagent thread started (`running`) or ended (`done`/`failed`) on this device.
    fn on_thread(&self, thread_id: String, title: String, state: String);
}

struct ToolBridge(Arc<dyn ToolHandler>);
impl core_client::ToolHandler for ToolBridge {
    fn call(&self, tool: &str, args: &str) -> Result<String, String> {
        self.0.call_tool(tool.to_string(), args.to_string()).map_err(|e| e.to_string())
    }
}

struct EventBridge(Arc<dyn ClientListener>);
impl core_client::Events for EventBridge {
    fn status(&self, s: core_client::Status) {
        self.0.on_status(match s {
            core_client::Status::Connecting => ConnectionStatus::Connecting,
            core_client::Status::Connected { device_id } => ConnectionStatus::Connected { device_id },
            core_client::Status::Disconnected { reason } => ConnectionStatus::Disconnected { reason },
            core_client::Status::Unauthorized => ConnectionStatus::Unauthorized,
        });
    }
    fn presence(&self, devices: Vec<core_protocol::DeviceInfo>) {
        self.0.on_presence(
            devices
                .into_iter()
                .map(|d| Device {
                    id: d.id,
                    name: d.name,
                    platform: d.platform,
                    online: d.online,
                    capabilities: d
                        .capabilities
                        .into_iter()
                        .map(|c| Capability { name: c.name, description: c.description })
                        .collect(),
                    last_seen: d.last_seen,
                })
                .collect(),
        );
    }
    fn thread(&self, thread_id: &str, title: &str, state: &str) {
        self.0.on_thread(thread_id.into(), title.into(), state.into());
    }
}

#[derive(uniffi::Object)]
pub struct GrouperClient {
    runtime: tokio::runtime::Runtime,
    config: core_client::Config,
    handler: Arc<dyn ToolHandler>,
    listener: Arc<dyn ClientListener>,
    shutdown: Mutex<Option<watch::Sender<bool>>>,
}

#[uniffi::export]
impl GrouperClient {
    /// `workspace_root`: folder for thread workspaces, or `None` to run no threads here.
    /// `allow_shell`: offer `run_shell` to threads (still gated by permissions).
    #[uniffi::constructor]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hub_url: String,
        token: String,
        capabilities: Vec<Capability>,
        workspace_root: Option<String>,
        allow_shell: bool,
        handler: Arc<dyn ToolHandler>,
        listener: Arc<dyn ClientListener>,
    ) -> Result<Arc<Self>, ClientError> {
        core_protocol::connect_url(&hub_url).map_err(|message| ClientError::Invalid { message })?;
        if core_protocol::split_token(&token).is_none() {
            return Err(ClientError::Invalid { message: "token should look like <device-id>.<secret>".into() });
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("grouper-core")
            .build()
            .map_err(|e| ClientError::Invalid { message: e.to_string() })?;
        Ok(Arc::new(Self {
            runtime,
            config: core_client::Config {
                hub_url,
                token,
                capabilities: capabilities
                    .into_iter()
                    .map(|c| core_protocol::Capability { name: c.name, description: c.description })
                    .collect(),
                workspace_root: workspace_root.map(std::path::PathBuf::from),
                allow_shell,
            },
            handler,
            listener,
            shutdown: Mutex::new(None),
        }))
    }

    /// Start connecting (and keep reconnecting) in the background. No-op if running.
    pub fn start(&self) {
        let mut guard = self.shutdown.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let (tx, rx) = watch::channel(false);
        *guard = Some(tx);
        self.runtime.spawn(core_client::run(
            self.config.clone(),
            Arc::new(ToolBridge(self.handler.clone())),
            Arc::new(EventBridge(self.listener.clone())),
            rx,
        ));
    }

    /// Disconnect and stop reconnecting.
    pub fn stop(&self) {
        if let Some(tx) = self.shutdown.lock().unwrap().take() {
            let _ = tx.send(true);
        }
    }
}

/// Version of the Rust core, handy for checking the bindings are wired up.
#[uniffi::export]
pub fn core_version() -> String {
    core_client::CLIENT_VERSION.to_string()
}
