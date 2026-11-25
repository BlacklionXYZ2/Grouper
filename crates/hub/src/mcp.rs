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

use crate::server::{CallError, CloseHandle, CloseReason, Hub};

/// How long a platform tool (Swift, CLI) may take.
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the device has to answer `initialize` and `tools/list`.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounded capacity for coalesced `tools/list_changed` notifications per device.
const REFRESH_QUEUE_CAP: usize = 16;

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
    refresh: mpsc::Sender<()>,
}

impl ClientHandler for DeviceClient {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        // Coalesced: if a refresh is already queued, dropping duplicates is intentional.
        let _ = self.refresh.try_send(());
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::default(), Implementation::new("grouper-hub", env!("CARGO_PKG_VERSION")))
    }
}

/// The MCP session of one device connection. Dropping it ends the session.
pub(crate) struct McpSession {
    /// Feed the `message` of every [`NodeMessage::Mcp`](core_protocol::NodeMessage::Mcp) here.
    pub(crate) incoming: mpsc::Sender<Value>,
    task: JoinHandle<()>,
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl McpSession {
    /// Start the session for connection `conn_id` of `device_id`; `out` is that
    /// connection's bounded outgoing frame queue.
    pub(crate) fn start(
        hub: &Arc<Hub>,
        device_id: String,
        conn_id: u64,
        out: mpsc::Sender<HubMessage>,
        close: CloseHandle,
    ) -> Self {
        let (tunnel, incoming) = Tunnel::new(move |message| match out.try_send(HubMessage::Mcp { message }) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                close.trigger(CloseReason::Overflow);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        let hub = Arc::downgrade(hub);
        let task = tokio::spawn(async move {
            let (refresh, mut refreshes) = mpsc::channel(REFRESH_QUEUE_CAP);
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

/// Apply a listed tool set only if `conn_id` is still the live connection for `device_id`.
/// Checked before writing to the database so a replaced connection can't clobber a newer one.
pub(crate) async fn apply_tool_list(hub: &Hub, device_id: &str, conn_id: u64, defs: Vec<ToolDef>) -> bool {
    let mut live = hub.live.lock().await;
    let Some(l) = live.get_mut(device_id).filter(|l| l.conn_id == conn_id) else {
        return false;
    };
    hub.db.upsert_tools(device_id, &defs);
    l.tools = defs;
    true
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
    if !apply_tool_list(&hub, device_id, conn_id, defs).await {
        return;
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

    #[tokio::test]
    async fn stale_connection_does_not_write_tools_to_db() {
        let dir = std::env::temp_dir().join(format!("grouper-mcp-stale-{}", crate::db::random_hex(4)));
        let db = crate::db::Db::open(&dir).unwrap();
        let (dev, _) = db.add_device("Laptop", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let (tx, _rx) = mpsc::channel(4);
        let (close, _close_rx) = CloseHandle::new();
        let (_closed_tx, closed_rx) = tokio::sync::watch::channel(false);
        hub.live.lock().await.insert(
            dev.id.clone(),
            crate::server::Live { conn_id: 2, tx, tools: Vec::new(), mcp: None, close, closed: closed_rx },
        );
        let tool = |name: &str| ToolDef { name: name.into(), description: "d".into(), input_schema: json!({}) };
        // Current connection (conn_id 2) writes its tool list.
        assert!(apply_tool_list(&hub, &dev.id, 2, vec![tool("fresh_tool")]).await);
        // Stale connection (conn_id 1) is rejected BEFORE touching the DB.
        assert!(!apply_tool_list(&hub, &dev.id, 1, vec![tool("stale_tool")]).await);
        let names: Vec<String> = hub.db.tools(Some(&dev.id)).into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["fresh_tool".to_string()]);
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
