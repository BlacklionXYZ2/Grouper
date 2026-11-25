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
    /// `^[a-zA-Z0-9_-]{1,64}$`, e.g. `mac_info` (no dots).
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, as a JSON string. `None`: takes no arguments.
    #[uniffi(default)]
    pub input_schema_json: Option<String>,
}

impl Capability {
    /// Check the name and parse the schema.
    fn to_core(&self) -> Result<core_protocol::Capability, ClientError> {
        if !core_protocol::valid_tool_name(&self.name) {
            return Err(ClientError::Invalid {
                message: format!("tool name {:?} must match ^[a-zA-Z0-9_-]{{1,64}}$ (no dots)", self.name),
            });
        }
        let input_schema = match &self.input_schema_json {
            None => core_protocol::default_input_schema(),
            Some(json) => {
                let schema: serde_json::Value = serde_json::from_str(json).map_err(|e| ClientError::Invalid {
                    message: format!("input schema of {:?} is not valid JSON: {e}", self.name),
                })?;
                if !schema.is_object() {
                    return Err(ClientError::Invalid {
                        message: format!("input schema of {:?} must be a JSON object", self.name),
                    });
                }
                schema
            }
        };
        Ok(core_protocol::Capability { name: self.name.clone(), description: self.description.clone(), input_schema })
    }

    fn from_core(c: core_protocol::ToolSummary) -> Self {
        Self { name: c.name, description: c.description, input_schema_json: None }
    }
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
                    capabilities: d.capabilities.into_iter().map(Capability::from_core).collect(),
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
        let capabilities = capabilities.iter().map(Capability::to_core).collect::<Result<Vec<_>, _>>()?;
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
                capabilities,
                workspace_root: workspace_root.map(std::path::PathBuf::from),
                allow_shell,
                // GROUPER_MCP_CONFIG, else <config dir>/grouper/mcp.json.
                mcp_config: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(name: &str, schema: Option<&str>) -> Capability {
        Capability { name: name.into(), description: "d".into(), input_schema_json: schema.map(String::from) }
    }

    #[test]
    fn capabilities_are_validated() {
        let c = cap("mac_info", None).to_core().unwrap();
        assert_eq!(c.input_schema, core_protocol::default_input_schema());
        let c = cap("echo", Some(r#"{"type":"object","properties":{"x":{"type":"string"}}}"#)).to_core().unwrap();
        assert_eq!(c.input_schema["properties"]["x"]["type"], "string");
        assert!(cap("mac.info", None).to_core().is_err());
        assert!(cap("echo", Some("{not json")).to_core().is_err());
        assert!(cap("echo", Some("[1]")).to_core().is_err());
    }
}
