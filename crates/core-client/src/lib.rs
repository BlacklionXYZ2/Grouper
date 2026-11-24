//! The device side of the Grouper protocol, shared by every client (Swift apps via
//! the `ffi` crate, the `grouper` CLI, and later Windows/Linux nodes).
//!
//! [`run`] keeps one connection to the hub alive: it says hello with the device's
//! capabilities and thread tools, answers direct tool calls through a
//! [`ToolHandler`], runs subagent threads the orchestrator starts here, reports
//! presence and status through [`Events`], pings, and reconnects with backoff.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use core_agent::thread::{ThreadControl, ThreadHost, spawn_thread};
use core_agent::tools::{Workspace, builtin_defs, file_tools, shell_tool};
use core_agent::{Tool, ToolOutput};
use core_protocol::{
    AgentEvent, Capability, DeviceInfo, HubMessage, NodeMessage, PING_INTERVAL_SECS, PROTOCOL_VERSION, ProviderConfig,
    ThreadSpec, ToolDef,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

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
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            break;
        }
        events.status(Status::Connecting);
        let started = tokio::time::Instant::now();
        let outcome = tokio::select! {
            o = connect_once(&url, &config, &handler, &events) => o,
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
    events.status(Status::Disconnected { reason: "stopped".into() });
}

/// Per-connection state for threads running on this device.
struct Threads {
    out: mpsc::UnboundedSender<NodeMessage>,
    controls: Mutex<HashMap<String, ThreadControl>>,
    approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    providers: Mutex<Vec<ProviderConfig>>,
    next: AtomicU64,
}

impl Drop for Threads {
    fn drop(&mut self) {
        // Connection gone: the hub has already marked these threads failed.
        for c in self.controls.lock().unwrap().values() {
            c.stop();
        }
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
        self.approvals.lock().unwrap().insert(approval_id.clone(), tx);
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
        name: cap.name.replace('.', "_"),
        description: cap.description.clone(),
        input_schema: json!({"type": "object", "additionalProperties": true}),
    }
}

fn thread_tool_defs(config: &Config) -> Vec<ToolDef> {
    if config.workspace_root.is_none() {
        return vec![];
    }
    let mut defs = builtin_defs(config.allow_shell);
    defs.extend(config.capabilities.iter().map(capability_def));
    defs
}

fn start_thread(
    spec: ThreadSpec,
    config: &Config,
    handler: &Arc<dyn ToolHandler>,
    events: &Arc<dyn Events>,
    threads: &Arc<Threads>,
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

    let title = spec.title.clone();
    events.thread(&id, &title, "running");
    let host: Arc<dyn ThreadHost> = threads.clone();
    let (control, handle) = spawn_thread(spec, provider, tools, host);
    threads.controls.lock().unwrap().insert(id.clone(), control);
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

async fn connect_once(url: &str, config: &Config, handler: &Arc<dyn ToolHandler>, events: &Arc<dyn Events>) -> Outcome {
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

    let hello = NodeMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_version: CLIENT_VERSION.into(),
        capabilities: config.capabilities.clone(),
        thread_tools: thread_tool_defs(config),
    };
    if let Err(e) = sink.send(Message::Text(serde_json::to_string(&hello).unwrap().into())).await {
        return Outcome::Retry(e.to_string());
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<NodeMessage>();
    let threads = Arc::new(Threads {
        out: out_tx.clone(),
        controls: Mutex::new(HashMap::new()),
        approvals: Mutex::new(HashMap::new()),
        providers: Mutex::new(Vec::new()),
        next: AtomicU64::new(1),
    });
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
                    Ok(HubMessage::ToolCall { call_id, tool, args }) => {
                        let handler = handler.clone();
                        let out_tx = out_tx.clone();
                        tokio::task::spawn_blocking(move || {
                            let (ok, output) = match handler.call(&tool, &args) {
                                Ok(o) => (true, o),
                                Err(e) => (false, e),
                            };
                            let _ = out_tx.send(NodeMessage::ToolResult { call_id, ok, output });
                        });
                    }
                    Ok(HubMessage::Providers { providers }) => {
                        *threads.providers.lock().unwrap() = providers;
                    }
                    Ok(HubMessage::StartThread { spec }) => start_thread(spec, config, handler, events, &threads),
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
