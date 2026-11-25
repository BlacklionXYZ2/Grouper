//! The device as an MCP server to the hub, over its WebSocket ([`core_protocol::tunnel`]).
//!
//! It lists the thread tools (when this device runs threads), the platform's tools
//! (Swift via UniFFI, the CLI's) and every `<server>__<tool>` of the [`McpHost`], and
//! tells the hub when that list changes. Workspace tools only run inside a thread, so
//! direct calls to them are refused. Its `initialize` result carries the device's
//! scoped folders (names and modes) in `_meta["grouper/folders"]`, and it answers the
//! hub's unlisted workspace tools ([`crate::workspace_files`]).

use std::path::PathBuf;
use std::sync::Arc;

use core_agent::folders::Folders;
use core_agent::tools::builtin_defs;
use core_protocol::tunnel::{Tunnel, result_from_text, tool_to_mcp};
use core_protocol::{Capability, FOLDERS_META_KEY, NodeMessage, ToolDef, USAGE_PROBES_META_KEY};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult, MetaObject, PaginatedRequestParams,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::mcp_host::McpHost;
use crate::{CLIENT_VERSION, Config, ToolHandler, capability_def};

pub(crate) struct DeviceServer {
    /// Workspace tools: listed (so the hub can set their permissions) but thread-only.
    workspace: Vec<ToolDef>,
    capabilities: Vec<Capability>,
    handler: Arc<dyn ToolHandler>,
    host: Arc<McpHost>,
    folders: Arc<Folders>,
    /// Where thread workspaces live, for the hub's workspace tools.
    workspace_root: Option<PathBuf>,
    /// Whether this device opted in to read-only subscription usage probes.
    usage_probes: bool,
}

impl DeviceServer {
    pub(crate) fn new(
        config: &Config,
        handler: Arc<dyn ToolHandler>,
        host: Arc<McpHost>,
        folders: Arc<Folders>,
    ) -> Self {
        let workspace = if config.workspace_root.is_some() { builtin_defs(config.allow_shell) } else { Vec::new() };
        Self {
            workspace,
            capabilities: config.capabilities.clone(),
            handler,
            host,
            folders,
            workspace_root: config.workspace_root.clone(),
            usage_probes: config.usage_probes,
        }
    }

    pub(crate) fn tool_defs(&self) -> Vec<ToolDef> {
        let mut defs = self.workspace.clone();
        defs.extend(self.capabilities.iter().map(capability_def));
        defs.extend(self.host.tools());
        defs
    }

    async fn run(&self, name: &str, args: Value) -> (bool, String) {
        // Internal hub-only tools: checked first, so no other tool can shadow them.
        if crate::workspace_files::is_internal(name) {
            let (root, name) = (self.workspace_root.clone(), name.to_string());
            return tokio::task::spawn_blocking(move || crate::workspace_files::call(root.as_deref(), &name, &args))
                .await
                .unwrap_or_else(|e| (false, e.to_string()));
        }
        if crate::usage_probes::is_internal(name) {
            if !self.usage_probes {
                return (false, serde_json::json!({"error": "usage probes are disabled on this device"}).to_string());
            }
            return crate::usage_probes::call(&args).await;
        }
        if self.workspace.iter().any(|d| d.name == name) {
            return (false, format!("{name} only runs inside a thread on this device"));
        }
        if self.capabilities.iter().any(|c| c.name == name) {
            let (handler, name) = (self.handler.clone(), name.to_string());
            return match tokio::task::spawn_blocking(move || handler.call(&name, &args.to_string())).await {
                Ok(Ok(out)) => (true, out),
                Ok(Err(e)) => (false, e),
                Err(e) => (false, e.to_string()),
            };
        }
        if self.host.has_tool(name) {
            return self.host.call(name, args).await;
        }
        (false, format!("this device has no tool {name}"))
    }
}

impl ServerHandler for DeviceServer {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
            .with_server_info(Implementation::new("grouper-device", CLIENT_VERSION));
        let mut meta = serde_json::Map::new();
        if self.workspace_root.is_some() && !self.folders.is_empty() {
            meta.insert(FOLDERS_META_KEY.into(), serde_json::to_value(self.folders.infos()).unwrap_or_default());
        }
        if self.usage_probes {
            meta.insert(USAGE_PROBES_META_KEY.into(), serde_json::json!(crate::usage_probes::SUPPORTED_PROVIDERS));
        }
        if !meta.is_empty() {
            info.meta = Some(MetaObject(meta));
        }
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tool_defs().iter().map(tool_to_mcp).collect()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = Value::Object(request.arguments.unwrap_or_default());
        // The hub cancelling (`notifications/cancelled`) drops the work, which in turn
        // cancels a call into a local MCP server.
        let (ok, output) = tokio::select! {
            r = self.run(&request.name, args) => r,
            _ = context.ct.cancelled() => (false, "cancelled".to_string()),
        };
        Ok(result_from_text(ok, output).into())
    }
}

/// The device's MCP server for one connection. Dropping it ends the session.
pub(crate) struct ServerSession {
    /// Feed the `message` of every [`HubMessage::Mcp`](core_protocol::HubMessage::Mcp) here.
    pub(crate) incoming: mpsc::Sender<Value>,
    task: JoinHandle<()>,
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ServerSession {
    pub(crate) fn start(
        server: DeviceServer,
        out: mpsc::Sender<NodeMessage>,
        on_overflow: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (tunnel, incoming) = Tunnel::new(move |message| match out.try_send(NodeMessage::Mcp { message }) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                on_overflow();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        // Subscribe before serving so no change is missed while the hub lists tools.
        let mut changes = server.host.subscribe();
        changes.borrow_and_update();
        let task = tokio::spawn(async move {
            let running = match server.serve(tunnel).await {
                Ok(r) => r,
                Err(e) => return tracing::warn!("MCP session with the hub failed: {e}"),
            };
            while changes.changed().await.is_ok() {
                if running.peer().notify_tool_list_changed().await.is_err() {
                    break;
                }
            }
            let _ = running.waiting().await;
        });
        Self { incoming, task }
    }
}
