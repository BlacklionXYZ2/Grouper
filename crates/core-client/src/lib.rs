//! The device side of the Grouper protocol, shared by every client (Swift apps via
//! the `ffi` crate, the `grouper` CLI, and later Windows/Linux nodes).
//!
//! [`run`] keeps one connection to the hub alive: it says hello, serves the device's
//! tools to the hub over MCP (platform tools through a [`ToolHandler`], and the tools
//! of local MCP servers from the [`mcp_host`]), runs subagent threads the
//! orchestrator starts here, reports presence and status through [`Events`], pings,
//! and reconnects with backoff.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use core_agent::thread::{ThreadControl, ThreadHost, spawn_thread};
use core_agent::tools::{Workspace, file_tools, shell_tool};
use core_agent::{Tool, ToolOutput};
use core_protocol::{
    AgentEvent, Capability, DeviceInfo, HubMessage, NodeMessage, PING_INTERVAL_SECS, PROTOCOL_VERSION, ProviderConfig,
    ThreadSpec, ToolDef,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

pub mod mcp_host;
mod mcp_server;

use mcp_host::{HostTool, McpHost};
use mcp_server::{DeviceServer, ServerSession};

pub use core_protocol;

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Implemented by the platform layer (Swift, CLI, …). Called off the async runtime,
/// so blocking is fine.
pub trait ToolHandler: Send + Sync + 'static {
    /// `args` is JSON. Return the tool's output (usually JSON) or an error message.
    fn call(&self, tool: &str, args: &str) -> Result<String, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Connected { device_id: String },
    Disconnected { reason: String },
    /// The hub rejected the token. The client stops retrying.
    Unauthorized,
}

pub trait Events: Send + Sync + 'static {
    fn status(&self, status: Status);
    fn presence(&self, devices: Vec<DeviceInfo>);
    /// A thread started (`running`) or ended (`done`/`failed`/`stopped`) on this device.
    fn thread(&self, _thread_id: &str, _title: &str, _state: &str) {}
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Hub base URL, e.g. `https://hub.example.com` or `http://127.0.0.1:8080`.
    pub hub_url: String,
    /// Device token from `grouper-hub device add`.
    pub token: String,
    pub capabilities: Vec<Capability>,
    /// Where threads get their workspace folders. `None`: this device runs no threads.
    pub workspace_root: Option<PathBuf>,
    /// Offer `run_shell` to threads (still gated by the device's permission level).
    pub allow_shell: bool,
    /// The MCP host's config file. `None`: `GROUPER_MCP_CONFIG`, else
    /// `<OS config dir>/grouper/mcp.json` (see [`mcp_host`]).
    pub mcp_config: Option<PathBuf>,
}

enum Outcome {
    Retry(String),
    Stop,
}

/// Run until `shutdown` becomes true or the hub rejects the token.
pub async fn run(config: Config, handler: Arc<dyn ToolHandler>, events: Arc<dyn Events>, mut shutdown: watch::Receiver<bool>) {
    let url = match core_protocol::connect_url(&config.hub_url) {
        Ok(u) => u,
        Err(e) => {
            events.status(Status::Disconnected { reason: e });
            return;
        }
    };
    // Local MCP servers start now, in the background, and live across reconnects
    // (agents keep their chat sessions in memory).
    let mcp_config = config.mcp_config.clone();
    let specs = tokio::task::spawn_blocking(move || mcp_host::load(mcp_config.as_deref())).await.unwrap_or_default();
    let host = McpHost::start(specs);
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            break;
        }
        events.status(Status::Connecting);
        let started = tokio::time::Instant::now();
        let outcome = tokio::select! {
            o = connect_once(&url, &config, &handler, &events, &host) => o,
            _ = shutdown.changed() => Outcome::Stop,
        };
        match outcome {
            Outcome::Stop => break,
            Outcome::Retry(reason) => {
                tracing::info!("disconnected: {reason}");
                events.status(Status::Disconnected { reason });
            }
        }
        if started.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => break,
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
    host.shutdown().await;
    events.status(Status::Disconnected { reason: "stopped".into() });
}

/// Per-connection state for threads running on this device.
///
/// Running threads hold an `Arc` to this (as their [`ThreadHost`]), so it can't rely
/// on `Drop` to stop them: [`StopOnDisconnect`] calls [`Threads::shutdown`] when the
/// connection ends.
struct Threads {
    out: mpsc::UnboundedSender<NodeMessage>,
    controls: Mutex<HashMap<String, ThreadControl>>,
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    providers: Mutex<Vec<ProviderConfig>>,
    next: AtomicU64,
    /// Set once the connection is gone: no new threads or approvals.
    closed: AtomicBool,
}

impl Threads {
    fn new(out: mpsc::UnboundedSender<NodeMessage>) -> Self {
        Self {
            out,
            controls: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            providers: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    /// The connection is gone and the hub has already marked these threads failed:
    /// stop every thread and deny every pending approval.
    fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let controls: Vec<ThreadControl> = self.controls.lock().unwrap().drain().map(|(_, c)| c).collect();
        for c in controls {
            c.stop();
        }
        // Dropping the senders resolves each waiting `approve` as a denial.
        self.approvals.lock().unwrap().clear();
    }
}

/// Stops this connection's threads when dropped, whether `connect_once` returned or
/// was cancelled by shutdown.
struct StopOnDisconnect(Arc<Threads>);

impl Drop for StopOnDisconnect {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

#[async_trait]
impl ThreadHost for Threads {
    fn emit(&self, thread_id: &str, event: AgentEvent) {
        let _ = self.out.send(NodeMessage::ThreadEvent { thread_id: thread_id.into(), event });
    }
    async fn approve(&self, thread_id: &str, tool: &str, input: &Value, reason: &str) -> bool {
        let approval_id = format!("{thread_id}-a{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            let mut approvals = self.approvals.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            approvals.insert(approval_id.clone(), tx);
        }
        let _ = self.out.send(NodeMessage::ApprovalRequest {
            approval_id,
            thread_id: thread_id.into(),
            tool: tool.into(),
            input: input.clone(),
            reason: reason.into(),
        });
        rx.await.unwrap_or(false)
    }
    fn ask_orchestrator(&self, thread_id: &str, question: &str) {
        let _ = self.out.send(NodeMessage::AskOrchestrator { thread_id: thread_id.into(), question: question.into() });
    }
}

/// A device capability (implemented by the platform, e.g. Swift) offered to threads.
struct CapabilityTool {
    cap: Capability,
    handler: Arc<dyn ToolHandler>,
}

#[async_trait]
impl Tool for CapabilityTool {
    fn def(&self) -> ToolDef {
        capability_def(&self.cap)
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let (handler, name) = (self.handler.clone(), self.cap.name.clone());
        match tokio::task::spawn_blocking(move || handler.call(&name, &input.to_string())).await {
            Ok(Ok(out)) => ToolOutput::ok(out),
            Ok(Err(e)) => ToolOutput::err(e),
            Err(e) => ToolOutput::err(e.to_string()),
        }
    }
}

fn capability_def(cap: &Capability) -> ToolDef {
    ToolDef {
        name: cap.name.clone(),
        description: cap.description.clone(),
        input_schema: cap.input_schema.clone(),
    }
}

fn start_thread(
    spec: ThreadSpec,
    config: &Config,
    handler: &Arc<dyn ToolHandler>,
    events: &Arc<dyn Events>,
    threads: &Arc<Threads>,
    host: &Arc<McpHost>,
) {
    let id = spec.thread_id.clone();
    let fail = |msg: String| {
        let _ = threads.out.send(NodeMessage::ThreadFinished { thread_id: id.clone(), ok: false, summary: msg });
    };
    let Some(root) = &config.workspace_root else {
        return fail("This device doesn't run threads.".into());
    };
    let Some(pcfg) = threads.providers.lock().unwrap().iter().find(|p| p.id == spec.provider_id).cloned() else {
        return fail(format!("This device has no provider {:?}.", spec.provider_id));
    };
    let provider = match providers::build(&pcfg) {
        Ok(p) => p,
        Err(e) => return fail(e.to_string()),
    };
    let ws = match Workspace::create(root.join(&id)) {
        Ok(w) => w,
        Err(e) => return fail(format!("workspace: {e}")),
    };
    let mut tools = file_tools(&ws);
    if config.allow_shell {
        tools.push(shell_tool(&ws));
    }
    for cap in &config.capabilities {
        tools.push(Arc::new(CapabilityTool { cap: cap.clone(), handler: handler.clone() }));
    }
    // `<server>__<tool>` from this device's MCP servers. Stopping the thread drops an
    // in-flight call, which cancels it on the server.
    tools.extend(HostTool::all(host));

    let title = spec.title.clone();
    events.thread(&id, &title, "running");
    let host: Arc<dyn ThreadHost> = threads.clone();
    let (control, handle) = spawn_thread(spec, provider, tools, host, ws);
    {
        let mut controls = threads.controls.lock().unwrap();
        if threads.closed.load(Ordering::SeqCst) {
            control.stop();
        } else {
            controls.insert(id.clone(), control);
        }
    }
    let (threads, events) = (Arc::downgrade(threads), events.clone());
    tokio::spawn(async move {
        let result = handle.await.unwrap_or_else(|e| core_agent::thread::ThreadResult {
            ok: false,
            summary: format!("thread crashed: {e}"),
        });
        events.thread(&id, &title, if result.ok { "done" } else { "failed" });
        if let Some(t) = threads.upgrade() {
            t.controls.lock().unwrap().remove(&id);
            let _ = t.out.send(NodeMessage::ThreadFinished { thread_id: id, ok: result.ok, summary: result.summary });
        }
    });
}

async fn connect_once(
    url: &str,
    config: &Config,
    handler: &Arc<dyn ToolHandler>,
    events: &Arc<dyn Events>,
    host: &Arc<McpHost>,
) -> Outcome {
    let mut request = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return Outcome::Retry(e.to_string()),
    };
    match format!("Bearer {}", config.token.trim()).parse() {
        Ok(v) => {
            request.headers_mut().insert("authorization", v);
        }
        Err(_) => {
            events.status(Status::Unauthorized);
            return Outcome::Stop;
        }
    }

    let ws = match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _)) => ws,
        Err(tungstenite::Error::Http(resp)) if resp.status() == 401 => {
            events.status(Status::Unauthorized);
            return Outcome::Stop;
        }
        Err(e) => return Outcome::Retry(e.to_string()),
    };
    let (mut sink, mut stream) = ws.split();

    let hello = NodeMessage::Hello { protocol_version: PROTOCOL_VERSION, client_version: CLIENT_VERSION.into() };
    if let Err(e) = sink.send(Message::Text(serde_json::to_string(&hello).unwrap().into())).await {
        return Outcome::Retry(e.to_string());
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<NodeMessage>();
    let threads = Arc::new(Threads::new(out_tx.clone()));
    let _stop_threads = StopOnDisconnect(threads.clone());
    // This device's MCP server; the hub is its client.
    let mcp = ServerSession::start(DeviceServer::new(config, handler.clone(), host.clone()), out_tx.clone());
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if let Err(e) = sink.send(Message::Text(serde_json::to_string(&NodeMessage::Ping).unwrap().into())).await {
                    return Outcome::Retry(e.to_string());
                }
            }
            Some(msg) = out_rx.recv() => {
                if let Err(e) = sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await {
                    return Outcome::Retry(e.to_string());
                }
            }
            incoming = stream.next() => {
                let text = match incoming {
                    None => return Outcome::Retry("hub closed the connection".into()),
                    Some(Err(e)) => return Outcome::Retry(e.to_string()),
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) => return Outcome::Retry("hub closed the connection".into()),
                    Some(Ok(_)) => continue,
                };
                match serde_json::from_str::<HubMessage>(&text) {
                    Ok(HubMessage::Welcome { device_id, .. }) => events.status(Status::Connected { device_id }),
                    Ok(HubMessage::Presence { devices }) => events.presence(devices),
                    Ok(HubMessage::Mcp { message }) => {
                        let _ = mcp.incoming.send(message);
                    }
                    Ok(HubMessage::Providers { providers }) => {
                        *threads.providers.lock().unwrap() = providers;
                    }
                    Ok(HubMessage::StartThread { spec }) => start_thread(spec, config, handler, events, &threads, host),
                    Ok(HubMessage::ThreadInput { thread_id, from, text }) => {
                        if let Some(c) = threads.controls.lock().unwrap().get(&thread_id) {
                            c.send(&from, &text);
                        }
                    }
                    Ok(HubMessage::StopThread { thread_id }) => {
                        if let Some(c) = threads.controls.lock().unwrap().remove(&thread_id) {
                            c.stop();
                        }
                        let prefix = format!("{thread_id}-a");
                        threads.approvals.lock().unwrap().retain(|id, _| !id.starts_with(&prefix));
                    }
                    Ok(HubMessage::ApprovalResult { approval_id, approved }) => {
                        if let Some(tx) = threads.approvals.lock().unwrap().remove(&approval_id) {
                            let _ = tx.send(approved);
                        }
                    }
                    Ok(HubMessage::Pong) => {}
                    Ok(HubMessage::Error { message }) => {
                        tracing::warn!("hub error: {message}");
                        if message.contains("protocol") {
                            events.status(Status::Disconnected { reason: message });
                            return Outcome::Stop;
                        }
                    }
                    Err(e) => tracing::warn!("unreadable frame from hub: {e}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_protocol::Permission;
    use serde_json::json;

    /// A thread waiting for approval must end, and release its host, when the
    /// connection drops (it used to wait forever and keep `Threads` alive).
    #[tokio::test]
    async fn disconnect_stops_threads_and_denies_approvals() {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let threads = Arc::new(Threads::new(out_tx));
        let guard = StopOnDisconnect(threads.clone());

        let provider = providers::build(&ProviderConfig {
            id: "fake".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {"default": [{"type": "tool_use", "name": "list_files", "input": {}}]}}),
        })
        .unwrap();
        let dir = std::env::temp_dir().join(format!("grouper-client-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let spec = ThreadSpec {
            thread_id: "t1".into(),
            conversation_id: "c1".into(),
            title: "test".into(),
            provider_id: "fake".into(),
            model: "fake".into(),
            system: String::new(),
            brief: "go".into(),
            permissions: HashMap::from([("list_files".to_string(), Permission::Manual)]),
            max_turns: 5,
        };
        let host: Arc<dyn ThreadHost> = threads.clone();
        let (control, handle) = spawn_thread(spec, provider, file_tools(&ws), host, ws.clone());
        threads.controls.lock().unwrap().insert("t1".into(), control);

        // Wait until the thread is blocked on an approval.
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), out_rx.recv()).await.unwrap().unwrap();
            if matches!(msg, NodeMessage::ApprovalRequest { .. }) {
                break;
            }
        }
        assert_eq!(threads.approvals.lock().unwrap().len(), 1);

        drop(guard); // the connection ends
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await.expect("thread kept running").unwrap();
        assert!(!result.ok);
        assert!(threads.controls.lock().unwrap().is_empty());
        assert!(threads.approvals.lock().unwrap().is_empty());
        // Nothing else holds the connection state once the thread is gone.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(Arc::strong_count(&threads), 1);
        // A late approval request is denied at once.
        assert!(!threads.approve("t1", "list_files", &json!({}), "late").await);
        std::fs::remove_dir_all(dir).ok();
    }
}
