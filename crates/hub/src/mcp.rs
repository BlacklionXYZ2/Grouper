//! The hub as the MCP client of every connected device.
//!
//! Each device is an MCP server over its own WebSocket ([`core_protocol::tunnel`]).
//! When a device connects the hub initializes a session, lists its tools into the
//! tool registry and the device's live entry, and lists them again whenever the
//! device sends `notifications/tools/list_changed`. Direct tool calls
//! ([`Hub::call_tool`], `POST /v1/devices/{id}/call`) are MCP `tools/call` requests.

use std::sync::{Arc, Weak};
use std::time::Duration;

use core_protocol::tunnel::{Tunnel, result_text, tool_from_mcp};
use core_protocol::{HubMessage, MCP_GRACE_SECS, ToolCallResponse, ToolDef, mcp_call_timeout, mcp_server_of};
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, Implementation, JsonObject};
use rmcp::service::{NotificationContext, RoleClient};
use rmcp::{ClientHandler, Peer, ServiceError, ServiceExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::server::{CallError, Hub};

/// How long a platform tool (Swift, CLI) may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the device has to answer `initialize` and `tools/list`.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the hub waits for a direct call. Tools of a device's MCP servers
/// (`<server>__<tool>`) run as long as their `timeout_seconds` argument allows; the
/// hub waits a grace period beyond the device's own limit so the device answers first.
pub(crate) fn call_timeout(tool: &str, args: &Value) -> Duration {
    if mcp_server_of(tool).is_some() {
        mcp_call_timeout(args) + Duration::from_secs(MCP_GRACE_SECS)
    } else {
        TOOL_TIMEOUT
    }
}

/// The hub's side of one device's MCP session.
struct DeviceClient {
    refresh: mpsc::UnboundedSender<()>,
}

impl ClientHandler for DeviceClient {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.refresh.send(());
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::default(), Implementation::new("grouper-hub", env!("CARGO_PKG_VERSION")))
    }
}

/// The MCP session of one device connection. Dropping it ends the session.
pub(crate) struct McpSession {
    /// Feed the `message` of every [`NodeMessage::Mcp`](core_protocol::NodeMessage::Mcp) here.
    pub(crate) incoming: mpsc::UnboundedSender<Value>,
    task: JoinHandle<()>,
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl McpSession {
    /// Start the session for connection `conn_id` of `device_id`; `out` is that
    /// connection's outgoing frame queue.
    pub(crate) fn start(hub: &Arc<Hub>, device_id: String, conn_id: u64, out: mpsc::UnboundedSender<HubMessage>) -> Self {
        let (tunnel, incoming) = Tunnel::new(move |message| out.send(HubMessage::Mcp { message }).is_ok());
        let hub = Arc::downgrade(hub);
        let task = tokio::spawn(async move {
            let (refresh, mut refreshes) = mpsc::unbounded_channel();
            let client = match tokio::time::timeout(LIST_TIMEOUT, DeviceClient { refresh }.serve(tunnel)).await {
                Ok(Ok(client)) => client,
                Ok(Err(e)) => return tracing::warn!(device = %device_id, "MCP initialize failed: {e}"),
                Err(_) => return tracing::warn!(device = %device_id, "MCP initialize timed out"),
            };
            let peer = client.peer().clone();
            {
                let Some(h) = hub.upgrade() else { return };
                let mut live = h.live.lock().await;
                match live.get_mut(&device_id).filter(|l| l.conn_id == conn_id) {
                    Some(l) => l.mcp = Some(peer.clone()),
                    None => return,
                }
            }
            loop {
                refresh_tools(&hub, &device_id, conn_id, &peer).await;
                if refreshes.recv().await.is_none() {
                    break;
                }
                // Several notifications in a row need one listing.
                while refreshes.try_recv().is_ok() {}
            }
            drop(client);
        });
        Self { incoming, task }
    }
}

/// List the device's tools into the registry and its live entry, then tell everyone.
async fn refresh_tools(hub: &Weak<Hub>, device_id: &str, conn_id: u64, peer: &Peer<RoleClient>) {
    let tools = match tokio::time::timeout(LIST_TIMEOUT, peer.list_all_tools()).await {
        Ok(Ok(tools)) => tools,
        Ok(Err(e)) => return tracing::warn!(device = %device_id, "tools/list failed: {e}"),
        Err(_) => return tracing::warn!(device = %device_id, "tools/list timed out"),
    };
    let defs: Vec<ToolDef> = tools
        .iter()
        .map(tool_from_mcp)
        .filter(|d| {
            let ok = core_protocol::valid_tool_name(&d.name);
            if !ok {
                tracing::warn!(device = %device_id, tool = %d.name, "ignoring a tool with an invalid name");
            }
            ok
        })
        .collect();
    let Some(hub) = hub.upgrade() else { return };
    hub.db.upsert_tools(device_id, &defs);
    {
        let mut live = hub.live.lock().await;
        match live.get_mut(device_id).filter(|l| l.conn_id == conn_id) {
            Some(l) => l.tools = defs,
            None => return,
        }
    }
    tracing::info!(device = %device_id, "tool list updated");
    hub.broadcast_presence().await;
}

impl Hub {
    /// Call one of an online device's tools (MCP `tools/call`) and wait for the answer.
    /// `args` must be a JSON object (or null for none).
    pub async fn call_tool(&self, device_id: &str, tool: &str, args: Value) -> Result<ToolCallResponse, CallError> {
        let arguments: JsonObject = match args {
            Value::Null => JsonObject::new(),
            Value::Object(o) => o,
            _ => return Err(CallError::BadArguments),
        };
        let peer = {
            let live = self.live.lock().await;
            let Some(l) = live.get(device_id) else { return Err(CallError::Offline) };
            if !l.tools.iter().any(|t| t.name == tool) {
                return Err(CallError::UnknownTool);
            }
            l.mcp.clone().ok_or(CallError::Offline)?
        };
        let timeout = call_timeout(tool, &Value::Object(arguments.clone()));
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        // On timeout the call is dropped, which sends `notifications/cancelled`.
        match tokio::time::timeout(timeout, peer.call_tool(params)).await {
            Err(_) => Err(CallError::Timeout),
            Ok(Err(ServiceError::TransportClosed | ServiceError::TransportSend(_))) => Err(CallError::Offline),
            Ok(Err(ServiceError::McpError(e))) => Ok(ToolCallResponse { ok: false, output: e.message.to_string() }),
            Ok(Err(e)) => Ok(ToolCallResponse { ok: false, output: e.to_string() }),
            Ok(Ok(result)) => {
                let (ok, output) = result_text(&result);
                Ok(ToolCallResponse { ok, output })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn call_timeouts() {
        assert_eq!(call_timeout("system_info", &json!({})), TOOL_TIMEOUT);
        let agent = call_timeout("codex__codex_chat", &json!({"timeout_seconds": 100}));
        assert_eq!(agent, Duration::from_secs(100 + 2 * MCP_GRACE_SECS));
        let default = call_timeout("codex__codex_chat", &json!({}));
        assert_eq!(default, Duration::from_secs(600 + 2 * MCP_GRACE_SECS));
    }
}
