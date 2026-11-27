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
use core_agent::folders::{FolderSpec, Folders};
use core_agent::thread::{ApprovalDecision, ThreadControl, ThreadHost, spawn_thread};
use core_agent::tools::{Workspace, file_tools, shell_tool};
use core_agent::{Tool, ToolOutput};
use core_protocol::{
    AgentEvent, Capability, DeviceInfo, HubMessage, MAX_WS_MESSAGE_BYTES, NodeHostInfo, NodeMessage,
    PING_INTERVAL_SECS, PROTOCOL_VERSION, ProviderConfig, ThreadSpec, ToolDef,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

pub mod folders;
mod device_provider;
mod handover;
pub mod harness;
pub mod mcp_host;
mod mcp_server;
pub mod usage_probes;
mod workspace_files;

use mcp_host::{HostTool, McpHost};
use mcp_server::{DeviceServer, ServerSession};

pub use core_protocol;

pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Maximum outgoing frames buffered for the hub connection before dropping it.
const CONN_QUEUE_CAP: usize = 1024;

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

/// Hub move offer delivered to a target node over its WebSocket connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubMoveOfferInfo {
    pub move_id: String,
    pub ticket: String,
    pub target_device_id: String,
    pub current_epoch: u64,
    pub snapshot_bytes: u64,
    pub snapshot_sha256: String,
}

/// Hub move commit signal delivered to a target node when the old hub is ready to switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubMoveCommitInfo {
    pub move_id: String,
    pub ticket: String,
    pub target_device_id: String,
    pub new_epoch: u64,
}

pub trait Events: Send + Sync + 'static {
    fn status(&self, status: Status);
    fn presence(&self, devices: Vec<DeviceInfo>);
    /// A thread started (`running`) or ended (`done`/`failed`/`stopped`) on this device.
    fn thread(&self, _thread_id: &str, _title: &str, _state: &str) {}
    /// The current hub offered this node a move ticket (`HubMessage::HubMoveOffer`).
    fn hub_move_offer(&self, _offer: HubMoveOfferInfo) {}
    /// The current hub told this node to commit a staged hub move (`HubMessage::HubMoveCommit`).
    fn hub_move_commit(&self, _commit: HubMoveCommitInfo) {}
    /// The current hub aborted an in-progress move (`HubMessage::HubMoveAbort`).
    fn hub_move_abort(&self, _move_id: &str, _reason: &str) {}
}

/// Detect whether this device can host Grouper, its client version, and whether `cloudflared` is installed.
pub fn detect_host_info() -> NodeHostInfo {
    NodeHostInfo {
        can_host: cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
        app_version: CLIENT_VERSION.to_string(),
        free_bytes: None,
        cloudflared: has_cloudflared(),
    }
}

fn has_cloudflared() -> bool {
    let names: &[&str] = if cfg!(windows) { &["cloudflared.exe", "cloudflared"] } else { &["cloudflared"] };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            for name in names {
                if is_executable_file(&dir.join(name)) {
                    return true;
                }
            }
        }
    }
    #[cfg(windows)]
    let extra = {
        let mut paths = vec![
            PathBuf::from(r"C:\Program Files (x86)\cloudflared\cloudflared.exe"),
            PathBuf::from(r"C:\Program Files\cloudflared\cloudflared.exe"),
        ];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            paths.push(PathBuf::from(local).join("cloudflared").join("cloudflared.exe"));
        }
        paths
    };
    #[cfg(not(windows))]
    let extra = [
        PathBuf::from("/opt/homebrew/bin/cloudflared"),
        PathBuf::from("/usr/local/bin/cloudflared"),
        PathBuf::from("/usr/bin/cloudflared"),
        PathBuf::from("/snap/bin/cloudflared"),
    ];
    extra.iter().any(|p| is_executable_file(p))
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
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
    /// Scoped folders threads here may reach as `@<name>/<path>` (see
    /// [`core_agent::folders`]). The caller loads them ([`folders::load`]).
    pub folders: Vec<FolderSpec>,
    /// Opt in to read-only subscription usage probes (Claude Code, Codex).
    /// Off by default because these call unofficial provider endpoints.
    pub usage_probes: bool,
}

enum Outcome {
    Retry(String),
    Stop,
}

/// Run until `shutdown` becomes true or the hub rejects the token.
pub async fn run(config: Config, handler: Arc<dyn ToolHandler>, events: Arc<dyn Events>, shutdown: watch::Receiver<bool>) {
    run_with_host(config, Some(detect_host_info()), handler, events, shutdown).await;
}

/// Like [`run`], but reports an explicit [`NodeHostInfo`] in the `Hello` frame (or `None` to omit it).
pub async fn run_with_host(
    config: Config,
    host_info: Option<NodeHostInfo>,
    handler: Arc<dyn ToolHandler>,
    events: Arc<dyn Events>,
    mut shutdown: watch::Receiver<bool>,
) {
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
    let (folders, warnings) = Folders::open(&config.folders);
    for w in warnings {
        tracing::warn!("scoped folders: {w}");
    }
    let folders = Arc::new(folders);
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            break;
        }
        events.status(Status::Connecting);
        let started = tokio::time::Instant::now();
        let outcome = tokio::select! {
            o = connect_once(&url, &config, host_info.as_ref(), &handler, &events, &host, &folders) => o,
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
    out: mpsc::Sender<NodeMessage>,
    on_overflow: Arc<dyn Fn() + Send + Sync>,
    controls: Mutex<HashMap<String, ThreadControl>>,
    approvals: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    providers: Mutex<Vec<ProviderConfig>>,
    /// This connection's authenticated device id and name, set by Welcome.
    identity: Mutex<Option<(String, String)>>,
    /// Calls this device is executing for the hub, keyed by hub call id.
    model_tasks: Mutex<HashMap<String, watch::Sender<bool>>>,
    /// Standard-chat harness turns executing on this device, keyed by conversation id.
    harness_tasks: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// Calls a local thread asked the hub to relay, keyed by device call id.
    model_waiters: Mutex<HashMap<String, device_provider::ModelWaiter>>,
    /// Random for each connection, so a reconnect cannot reuse an old approval id.
    approval_nonce: u64,
    next: AtomicU64,
    /// Set once the connection is gone: no new threads or approvals.
    closed: AtomicBool,
}

impl Threads {
    fn new(out: mpsc::Sender<NodeMessage>, on_overflow: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).expect("system randomness for approval ids");
        Self {
            out,
            on_overflow,
            controls: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            providers: Mutex::new(Vec::new()),
            identity: Mutex::new(None),
            model_tasks: Mutex::new(HashMap::new()),
            harness_tasks: Mutex::new(HashMap::new()),
            model_waiters: Mutex::new(HashMap::new()),
            approval_nonce: u64::from_le_bytes(nonce),
            next: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    fn send(&self, msg: NodeMessage) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.out.try_send(msg) {
            (self.on_overflow)();
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
        for (_, cancel) in self.model_tasks.lock().unwrap().drain() {
            let _ = cancel.send(true);
        }
        for (_, handle) in self.harness_tasks.lock().unwrap().drain() {
            handle.abort();
        }
        self.model_waiters.lock().unwrap().clear();
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
        self.send(NodeMessage::ThreadEvent { thread_id: thread_id.into(), event });
    }
    async fn approve(&self, thread_id: &str, tool: &str, input: &Value, reason: &str) -> ApprovalDecision {
        let approval_id = format!(
            "{thread_id}-{:016x}-a{}",
            self.approval_nonce,
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        {
            let mut approvals = self.approvals.lock().unwrap();
            if self.closed.load(Ordering::SeqCst) {
                return ApprovalDecision::Denied;
            }
            approvals.insert(approval_id.clone(), tx);
        }
        self.send(NodeMessage::ApprovalRequest {
            approval_id,
            thread_id: thread_id.into(),
            tool: tool.into(),
            input: input.clone(),
            reason: reason.into(),
        });
        rx.await.unwrap_or(ApprovalDecision::Denied)
    }
    fn ask_orchestrator(&self, thread_id: &str, question: &str) {
        self.send(NodeMessage::AskOrchestrator { thread_id: thread_id.into(), question: question.into() });
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
    folders: &Arc<Folders>,
) {
    let id = spec.thread_id.clone();
    let fail = |msg: String| {
        threads.send(NodeMessage::ThreadFinished { thread_id: id.clone(), ok: false, summary: msg });
    };
    let Some(root) = &config.workspace_root else {
        return fail("This device doesn't run threads.".into());
    };
    let Some(pcfg) = threads.providers.lock().unwrap().iter().find(|p| p.id == spec.provider_id).cloned() else {
        return fail(format!("This device has no provider {:?}.", spec.provider_id));
    };
    let provider = match device_provider::build(&pcfg, &spec.fallback_models, threads, &id) {
        Ok(p) => p,
        Err(e) => return fail(e.to_string()),
    };
    let ws = match Workspace::create_scoped(root.join(&id), folders.clone(), &spec.device_name) {
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
            t.send(NodeMessage::ThreadFinished { thread_id: id, ok: result.ok, summary: result.summary });
        }
    });
}

const MAX_HARNESS_TASKS: usize = 16;

fn start_harness_turn(
    conversation_id: String,
    agent: String,
    model: String,
    session_id: Option<String>,
    message: String,
    config: &Config,
    threads: &Arc<Threads>,
) {
    let workdir = match harness::chat_workdir(config.workspace_root.as_deref(), &conversation_id) {
        Ok(dir) => dir,
        Err(error) => {
            threads.send(NodeMessage::HarnessFailed { conversation_id, error });
            return;
        }
    };
    {
        let tasks = threads.harness_tasks.lock().unwrap();
        if !tasks.contains_key(&conversation_id) && tasks.len() >= MAX_HARNESS_TASKS {
            drop(tasks);
            threads.send(NodeMessage::HarnessFailed {
                conversation_id,
                error: "this device is busy with other harness turns; try again shortly".into(),
            });
            return;
        }
    }
    let weak = Arc::downgrade(threads);
    let conv_key = conversation_id.clone();
    let join = tokio::spawn(async move {
        let result = harness::turn(
            &agent,
            &model,
            session_id.as_deref(),
            &message,
            &workdir,
            harness::HarnessLevel::Confined,
        )
        .await;
        if let Some(t) = weak.upgrade() {
            t.harness_tasks.lock().unwrap().remove(&conv_key);
            match result {
                Ok(out) => t.send(NodeMessage::HarnessFinished {
                    conversation_id: conv_key,
                    text: out.text,
                    session_id: out.session_id,
                    usage: out.usage,
                }),
                Err(error) => t.send(NodeMessage::HarnessFailed { conversation_id: conv_key, error }),
            }
        }
    });
    let abort = join.abort_handle();
    let mut tasks = threads.harness_tasks.lock().unwrap();
    if threads.closed.load(Ordering::SeqCst) {
        abort.abort();
    } else if let Some(prev) = tasks.insert(conversation_id, abort) {
        prev.abort();
    }
}

async fn connect_once(
    url: &str,
    config: &Config,
    host_info: Option<&NodeHostInfo>,
    handler: &Arc<dyn ToolHandler>,
    events: &Arc<dyn Events>,
    host: &Arc<McpHost>,
    folders: &Arc<Folders>,
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

    let mut ws_config = tungstenite::protocol::WebSocketConfig::default();
    ws_config.max_message_size = Some(MAX_WS_MESSAGE_BYTES);
    ws_config.max_frame_size = Some(MAX_WS_MESSAGE_BYTES);
    let ws = match tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false).await {
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
        host: host_info.cloned(),
    };
    if let Err(e) = sink.send(Message::Text(serde_json::to_string(&hello).unwrap().into())).await {
        return Outcome::Retry(e.to_string());
    }

    let (out_tx, mut out_rx) = mpsc::channel::<NodeMessage>(CONN_QUEUE_CAP);
    let (overflow_tx, mut overflow_rx) = oneshot::channel::<&'static str>();
    let overflow_slot = Arc::new(Mutex::new(Some(overflow_tx)));
    let on_overflow: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        if let Some(tx) = overflow_slot.lock().unwrap().take() {
            let _ = tx.send("outgoing queue full");
        }
    });
    let threads = Arc::new(Threads::new(out_tx.clone(), on_overflow.clone()));
    let _stop_threads = StopOnDisconnect(threads.clone());
    // This device's MCP server; the hub is its client.
    let mcp = ServerSession::start(
        DeviceServer::new(config, handler.clone(), host.clone(), folders.clone()),
        out_tx.clone(),
        on_overflow.clone(),
    );
    let _harnesses = harness::HarnessSession::start(host.subscribe(), out_tx.clone(), on_overflow);
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.tick().await;

    loop {
        tokio::select! {
            reason = &mut overflow_rx => {
                return Outcome::Retry(reason.unwrap_or("outgoing queue full").into());
            }
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
                    Ok(HubMessage::Welcome { device_id, device_name, .. }) => {
                        *threads.identity.lock().unwrap() = Some((device_id.clone(), device_name));
                        events.status(Status::Connected { device_id });
                    }
                    Ok(HubMessage::Presence { devices }) => events.presence(devices),
                    Ok(HubMessage::Mcp { message }) => {
                        if let Err(mpsc::error::TrySendError::Full(_)) = mcp.incoming.try_send(message) {
                            return Outcome::Retry("MCP incoming queue full".into());
                        }
                    }
                    Ok(HubMessage::ModelRequest { call_id, provider_id, request }) => {
                        device_provider::start_host_call(&threads, call_id, provider_id, request);
                    }
                    Ok(HubMessage::ModelDelta { call_id, delta }) => {
                        device_provider::relay_delta(&threads, &call_id, delta);
                    }
                    Ok(HubMessage::ModelResponse { call_id, response, error }) => {
                        device_provider::relay_response(&threads, &call_id, response, error);
                    }
                    Ok(HubMessage::ModelCancel { call_id }) => {
                        device_provider::cancel_host_call(&threads, &call_id);
                    }
                    Ok(HubMessage::Providers { providers }) => {
                        *threads.providers.lock().unwrap() = providers;
                    }
                    Ok(HubMessage::StartThread { spec }) => {
                        start_thread(*spec, config, handler, events, &threads, host, folders)
                    }
                    Ok(HubMessage::ThreadInput { thread_id, from, text }) => {
                        if let Some(c) = threads.controls.lock().unwrap().get(&thread_id) {
                            c.send(&from, &text);
                        }
                    }
                    Ok(HubMessage::StopThread { thread_id }) => {
                        if let Some(c) = threads.controls.lock().unwrap().remove(&thread_id) {
                            c.stop();
                        }
                        let prefix = format!("{thread_id}-");
                        threads.approvals.lock().unwrap().retain(|id, _| !id.starts_with(&prefix));
                    }
                    Ok(HubMessage::ApprovalResult { approval_id, approved, timeout_secs }) => {
                        if let Some(tx) = threads.approvals.lock().unwrap().remove(&approval_id) {
                            let decision = if approved {
                                ApprovalDecision::Approved
                            } else if let Some(secs) = timeout_secs {
                                ApprovalDecision::TimedOut { secs }
                            } else {
                                ApprovalDecision::Denied
                            };
                            let _ = tx.send(decision);
                        }
                    }
                    // Written before the thread's StartThread, which follows on this connection.
                    Ok(HubMessage::WorkspaceFile { thread_id, path, content_base64 }) => {
                        let root = config.workspace_root.clone();
                        let ack = tokio::task::spawn_blocking(move || {
                            handover::apply_file(root.as_deref(), thread_id, path, &content_base64)
                        })
                        .await
                        .unwrap_or_else(|e| NodeMessage::WorkspaceFileResult {
                            thread_id: String::new(),
                            path: String::new(),
                            ok: false,
                            error: Some(e.to_string()),
                        });
                        if let Err(mpsc::error::TrySendError::Full(_)) = out_tx.try_send(ack) {
                            return Outcome::Retry("outgoing queue full".into());
                        }
                    }
                    Ok(HubMessage::HubMoveOffer {
                        move_id,
                        ticket,
                        target_device_id,
                        current_epoch,
                        snapshot_bytes,
                        snapshot_sha256,
                    }) => {
                        events.hub_move_offer(HubMoveOfferInfo {
                            move_id,
                            ticket,
                            target_device_id,
                            current_epoch,
                            snapshot_bytes,
                            snapshot_sha256,
                        });
                    }
                    Ok(HubMessage::HubMoveCommit { move_id, ticket, target_device_id, new_epoch }) => {
                        events.hub_move_commit(HubMoveCommitInfo {
                            move_id,
                            ticket,
                            target_device_id,
                            new_epoch,
                        });
                    }
                    Ok(HubMessage::HubMoveAbort { move_id, reason }) => {
                        events.hub_move_abort(&move_id, &reason);
                    }
                    Ok(HubMessage::HarnessTurn { conversation_id, agent, model, session_id, message }) => {
                        start_harness_turn(conversation_id, agent, model, session_id, message, config, &threads);
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

    #[test]
    fn detect_host_info_reports_desktop_capability_and_version() {
        let info = detect_host_info();
        assert_eq!(info.app_version, CLIENT_VERSION);
        let expected_can_host = cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux"));
        assert_eq!(info.can_host, expected_can_host);
        let hello = NodeMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: CLIENT_VERSION.into(),
            host: Some(info.clone()),
        };
        let encoded = serde_json::to_value(&hello).unwrap();
        assert_eq!(encoded["type"], "hello");
        assert_eq!(encoded["host"]["can_host"], expected_can_host);
        assert_eq!(encoded["host"]["app_version"], CLIENT_VERSION);
    }

    #[test]
    fn approval_nonce_is_fresh_for_each_connection() {
        let (tx1, _rx1) = mpsc::channel(CONN_QUEUE_CAP);
        let (tx2, _rx2) = mpsc::channel(CONN_QUEUE_CAP);
        let first = Threads::new(tx1, Arc::new(|| {}));
        let second = Threads::new(tx2, Arc::new(|| {}));
        assert_ne!(first.approval_nonce, second.approval_nonce);
    }

    /// A thread waiting for approval must end, and release its host, when the
    /// connection drops (it used to wait forever and keep `Threads` alive).
    #[tokio::test]
    async fn disconnect_stops_threads_and_denies_approvals() {
        let (out_tx, mut out_rx) = mpsc::channel(CONN_QUEUE_CAP);
        let threads = Arc::new(Threads::new(out_tx, Arc::new(|| {})));
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
            device_name: "Laptop".into(),
            ask_timeout_secs: Some(core_protocol::DEFAULT_ASK_TIMEOUT_SECS),
            fallback_models: Vec::new(),
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
        assert_eq!(threads.approve("t1", "list_files", &json!({}), "late").await, ApprovalDecision::Denied);
        std::fs::remove_dir_all(dir).ok();
    }
}
