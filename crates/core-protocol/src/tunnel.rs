//! MCP over the device connection: an rmcp [`Transport`] whose messages travel as
//! [`NodeMessage::Mcp`](crate::NodeMessage::Mcp) / [`HubMessage::Mcp`](crate::HubMessage::Mcp)
//! frames, plus conversions between Grouper's tool types and rmcp's.
//!
//! The connection owns the WebSocket. Each side makes a [`Tunnel`] with a function
//! that wraps an outgoing JSON-RPC message in its frame and queues it, and feeds the
//! `message` of every incoming `Mcp` frame into the sender [`Tunnel::new`] returns.
//! The device serves its tools with rmcp's server over the tunnel; the hub is the client.

use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use rmcp::service::{RxJsonRpcMessage, ServiceRole, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::ToolDef;

/// The connection behind a tunnel has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelClosed;

impl std::fmt::Display for TunnelClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the connection carrying MCP has closed")
    }
}

impl std::error::Error for TunnelClosed {}

/// Maximum incoming MCP JSON-RPC frames buffered per tunnel.
pub const TUNNEL_QUEUE_CAP: usize = 1024;

type SendFn = Arc<dyn Fn(Value) -> bool + Send + Sync>;

/// One end of an MCP session carried in connection frames.
pub struct Tunnel {
    send: SendFn,
    incoming: mpsc::Receiver<Value>,
}

impl Tunnel {
    /// `send` queues one outgoing JSON-RPC message (as a frame) and returns false once
    /// the connection is gone or its queue overflows. Feed incoming messages into the
    /// returned bounded sender (using `try_send`); dropping it ends the session.
    pub fn new(send: impl Fn(Value) -> bool + Send + Sync + 'static) -> (Self, mpsc::Sender<Value>) {
        Self::with_capacity(TUNNEL_QUEUE_CAP, send)
    }

    /// Like [`Tunnel::new`], with an explicit incoming buffer capacity.
    pub fn with_capacity(
        capacity: usize,
        send: impl Fn(Value) -> bool + Send + Sync + 'static,
    ) -> (Self, mpsc::Sender<Value>) {
        let (tx, incoming) = mpsc::channel(capacity.max(1));
        (Self { send: Arc::new(send), incoming }, tx)
    }
}

impl<R: ServiceRole> Transport<R> for Tunnel {
    type Error = TunnelClosed;

    fn send(&mut self, item: TxJsonRpcMessage<R>) -> impl Future<Output = Result<(), TunnelClosed>> + Send + 'static {
        let sent = serde_json::to_value(&item).is_ok_and(|v| (self.send)(v));
        std::future::ready(if sent { Ok(()) } else { Err(TunnelClosed) })
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<R>> {
        loop {
            let value = self.incoming.recv().await?;
            // Not a JSON-RPC message for this role: drop it, as a stream transport
            // would drop an unparseable line.
            if let Ok(message) = serde_json::from_value(value) {
                return Some(message);
            }
        }
    }

    async fn close(&mut self) -> Result<(), TunnelClosed> {
        self.incoming.close();
        Ok(())
    }
}

/// A Grouper tool definition as an MCP tool. A schema that isn't an object becomes
/// the empty object schema.
pub fn tool_to_mcp(def: &ToolDef) -> Tool {
    let schema: JsonObject = match &def.input_schema {
        Value::Object(o) => o.clone(),
        _ => match crate::default_input_schema() {
            Value::Object(o) => o,
            _ => unreachable!(),
        },
    };
    Tool::new(def.name.clone(), def.description.clone(), Arc::new(schema))
}

/// An MCP tool as a Grouper tool definition.
pub fn tool_from_mcp(tool: &Tool) -> ToolDef {
    ToolDef {
        name: tool.name.to_string(),
        description: tool.description.as_deref().map(str::to_string).or_else(|| tool.title.clone()).unwrap_or_default(),
        input_schema: Value::Object((*tool.input_schema).clone()),
    }
}

/// A tool's output as one string: its text blocks joined by newlines, otherwise its
/// structured content as JSON. Returns `(ok, output)`.
pub fn result_text(result: &CallToolResult) -> (bool, String) {
    let ok = !result.is_error.unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    for block in &result.content {
        parts.push(match block {
            ContentBlock::Text(t) => t.text.clone(),
            ContentBlock::Image(_) => "[image]".to_string(),
            ContentBlock::Audio(_) => "[audio]".to_string(),
            ContentBlock::Resource(r) => match &r.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
                _ => "[binary resource]".to_string(),
            },
            ContentBlock::ResourceLink(_) => "[resource link]".to_string(),
            #[allow(unreachable_patterns)]
            _ => "[unsupported content]".to_string(),
        });
    }
    if parts.is_empty()
        && let Some(s) = &result.structured_content
    {
        parts.push(s.to_string());
    }
    (ok, parts.join("\n"))
}

/// A tool's output as an MCP result.
pub fn result_from_text(ok: bool, output: String) -> CallToolResult {
    let content = vec![ContentBlock::text(output)];
    if ok { CallToolResult::success(content) } else { CallToolResult::error(content) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_and_results_convert() {
        let def = ToolDef {
            name: "mac_info".into(),
            description: "Mac details".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        };
        let tool = tool_to_mcp(&def);
        assert_eq!(tool_from_mcp(&tool), def);
        let wire = serde_json::to_value(&tool).unwrap();
        assert_eq!(wire["inputSchema"]["properties"]["x"]["type"], "string");

        let bad = ToolDef { input_schema: Value::Null, ..def };
        assert_eq!(tool_from_mcp(&tool_to_mcp(&bad)).input_schema, crate::default_input_schema());

        assert_eq!(result_text(&result_from_text(true, "hi".into())), (true, "hi".into()));
        assert_eq!(result_text(&result_from_text(false, "no".into())), (false, "no".into()));
        let structured = CallToolResult::structured(serde_json::json!({"a": 1}));
        assert!(result_text(&structured).1.contains("\"a\""));
    }

    #[test]
    fn tunnel_incoming_queue_is_bounded() {
        let (_tunnel, tx) = Tunnel::with_capacity(2, |_| true);
        assert!(tx.try_send(serde_json::json!(1)).is_ok());
        assert!(tx.try_send(serde_json::json!(2)).is_ok());
        assert!(matches!(tx.try_send(serde_json::json!(3)), Err(mpsc::error::TrySendError::Full(_))));
    }
}
