//! The device side of the Grouper protocol, shared by every client (Swift apps via
//! the `ffi` crate, the `grouper` CLI, and later Windows/Linux nodes).
//!
//! [`run`] keeps one connection to the hub alive: it says hello, serves the device's
//! tools to the hub over MCP (platform tools through a [`ToolHandler`], and the tools
//! of local MCP servers from the [`mcp_host`]), runs subagent threads the
//! orchestrator starts here, reports presence and status through [`Events`], pings,
//! and reconnects with backoff.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// Read-only subscription usage probes (Claude Code, Codex, Grok, Antigravity). The CLI and
    /// desktop app turn this on by default (usage belongs to the whole account); `false` is the
    /// device owner's local off switch. Never settable from the hub.
    pub usage_probes: bool,
    /// Which providers to probe when `usage_probes` is on. `None`: every supported provider
    /// (Antigravity only if its CLI is installed here).
    pub usage_probe_providers: Option<Vec<String>>,
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
    /// Harness threads (one CLI session each), keyed by thread id.
    harness_runs: Mutex<HashMap<String, HarnessThreadRun>>,
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
            harness_runs: Mutex::new(HashMap::new()),
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
        for (_, run) in self.harness_runs.lock().unwrap().drain() {
            run.abort();
        }
        self.model_waiters.lock().unwrap().clear();
    }

    fn push_input(&self, thread_id: &str, from: &str, text: &str) {
        if let Some(c) = self.controls.lock().unwrap().get(thread_id) {
            c.send(from, text);
            return;
        }
        if let Some(run) = self.harness_runs.lock().unwrap().get(thread_id) {
            // Same shape the API loop queues, so a resumed CLI turn can see who spoke.
            let _ = run.input.send(format!("[Message from the {from}]\n{text}"));
        }
    }

    fn stop_one(&self, thread_id: &str) {
        if let Some(c) = self.controls.lock().unwrap().remove(thread_id) {
            c.stop();
        }
        if let Some(run) = self.harness_runs.lock().unwrap().remove(thread_id) {
            run.abort();
        }
        let prefix = format!("{thread_id}-");
        self.approvals.lock().unwrap().retain(|id, _| !id.starts_with(&prefix));
    }
}

/// One harness thread: aborting it drops the CLI child (`kill_on_drop`).
struct HarnessThreadRun {
    abort: tokio::task::AbortHandle,
    input: mpsc::UnboundedSender<String>,
}

/// What the harness thread should do after a CLI turn.
enum HarnessInbox {
    /// `message_thread` or a step-in arrived; run another turn on the same session.
    Message(String),
    /// Nothing queued. The run has been removed so a late message cannot land in the gap.
    Finished,
    /// `stop_thread` already dropped the sender (and aborted the task).
    Stopped,
}

/// Next queued message, or the decision to end. The empty check and the removal share
/// `harness_runs`'s lock, which is also what [`Threads::push_input`] holds while it sends,
/// so a message cannot arrive after we decide the inbox is empty.
fn poll_harness_inbox(
    rx: &mut mpsc::UnboundedReceiver<String>,
    threads: &std::sync::Weak<Threads>,
    thread_id: &str,
) -> HarnessInbox {
    match rx.try_recv() {
        Ok(message) => return HarnessInbox::Message(message),
        Err(mpsc::error::TryRecvError::Disconnected) => return HarnessInbox::Stopped,
        Err(mpsc::error::TryRecvError::Empty) => {}
    }
    let Some(t) = threads.upgrade() else {
        return HarnessInbox::Stopped;
    };
    let mut runs = t.harness_runs.lock().unwrap();
    match rx.try_recv() {
        Ok(message) => HarnessInbox::Message(message),
        Err(mpsc::error::TryRecvError::Disconnected) => HarnessInbox::Stopped,
        Err(mpsc::error::TryRecvError::Empty) => {
            runs.remove(thread_id);
            HarnessInbox::Finished
        }
    }
}

impl HarnessThreadRun {
    fn abort(self) {
        self.abort.abort();
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
    if spec.harness.is_some() {
        let Some(root) = config.workspace_root.clone() else {
            let id = spec.thread_id.clone();
            events.thread(&id, &spec.title, "failed");
            threads.send(NodeMessage::ThreadFinished {
                thread_id: id,
                ok: false,
                summary: "This device doesn't run threads.".into(),
            });
            return;
        };
        start_harness_thread(spec, &root, events, threads, folders, harness::ScanOptions::host());
        return;
    }
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

/// Run a harness thread with [`harness::turn`] at [`HarnessLevel::Workspace`](harness::HarnessLevel::Workspace).
///
/// `opts` is [`harness::ScanOptions::host`] in production, which is what `harness::turn` uses.
/// Tests pass an isolated scan so the turn runs a fake CLI. The brief is turn 1 and the
/// working directory is the thread workspace. `message_thread` and a step-in queued during
/// a turn are further turns that resume the same session. An empty inbox after a turn ends
/// the thread, and that reply is the summary. The run is registered before the first turn,
/// and aborting it drops the child (`kill_on_drop`), which is how `stop_thread` kills the process.
fn start_harness_thread(
    spec: ThreadSpec,
    workspace_root: &Path,
    events: &Arc<dyn Events>,
    threads: &Arc<Threads>,
    folders: &Arc<Folders>,
    opts: harness::ScanOptions,
) {
    let id = spec.thread_id.clone();
    let title = spec.title.clone();
    let fail = |msg: String| {
        events.thread(&id, &title, "failed");
        threads.send(NodeMessage::ThreadFinished { thread_id: id.clone(), ok: false, summary: msg });
    };
    let Some(harness_spec) = spec.harness.clone() else {
        return fail("not a harness thread".into());
    };
    let ws = match Workspace::create_scoped(workspace_root.join(&id), folders.clone(), &spec.device_name) {
        Ok(w) => w,
        Err(e) => return fail(format!("workspace: {e}")),
    };
    let workdir = ws.root().to_path_buf();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    let weak = Arc::downgrade(threads);
    let task_events = events.clone();
    let thread_id = id.clone();
    let brief = spec.brief.clone();
    let task_title = title.clone();
    let join = tokio::spawn(async move {
        // Registered (or shut down) before any CLI process exists.
        if ready_rx.await.is_err() {
            return;
        }
        let mut session = harness_spec.session.filter(|s| !s.trim().is_empty());
        let mut last_reply = String::new();
        let mut upcoming = Some(brief);
        loop {
            let message = if let Some(m) = upcoming.take() {
                m
            } else {
                match poll_harness_inbox(&mut input_rx, &weak, &thread_id) {
                    HarnessInbox::Message(m) => m,
                    HarnessInbox::Finished => break,
                    HarnessInbox::Stopped => return,
                }
            };
            // Same call as `harness::turn`, plus the scan options so a test can supply a fake CLI.
            let result = harness::turn_with(
                &opts,
                &harness_spec.agent,
                &harness_spec.model,
                session.as_deref(),
                &message,
                &workdir,
                harness::HarnessLevel::Workspace,
                None,
            )
            .await;
            let Some(t) = weak.upgrade() else { return };
            match result {
                Ok(out) => {
                    if !out.session_id.trim().is_empty() {
                        session = Some(out.session_id.clone());
                        t.send(NodeMessage::ThreadHarnessSession {
                            thread_id: thread_id.clone(),
                            session_id: out.session_id,
                        });
                    }
                    t.send(NodeMessage::ThreadEvent {
                        thread_id: thread_id.clone(),
                        event: AgentEvent::Text { text: out.text.clone() },
                    });
                    last_reply = out.text;
                }
                Err(error) => {
                    task_events.thread(&thread_id, &task_title, "failed");
                    t.harness_runs.lock().unwrap().remove(&thread_id);
                    t.send(NodeMessage::ThreadFinished { thread_id, ok: false, summary: error });
                    return;
                }
            }
        }
        task_events.thread(&thread_id, &task_title, "done");
        if let Some(t) = weak.upgrade() {
            t.send(NodeMessage::ThreadFinished { thread_id, ok: true, summary: last_reply });
        }
    });
    let abort = join.abort_handle();
    let inserted = {
        let mut runs = threads.harness_runs.lock().unwrap();
        if threads.closed.load(Ordering::SeqCst) {
            abort.abort();
            false
        } else {
            runs.insert(id, HarnessThreadRun { abort, input: input_tx });
            true
        }
    };
    if inserted {
        events.thread(&spec.thread_id, &spec.title, "running");
        let _ = ready_tx.send(());
    }
}

const MAX_HARNESS_TASKS: usize = 16;

#[allow(clippy::too_many_arguments)]
fn start_harness_turn(
    conversation_id: String,
    agent: String,
    model: String,
    session_id: Option<String>,
    message: String,
    mcp: Option<core_protocol::HarnessMcp>,
    allowed_tools: Vec<String>,
    system_prompt: Option<String>,
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
        let attach = mcp.as_ref().map(|mcp| harness::ClaudeOrchestrator {
            mcp,
            allowed_tools: &allowed_tools,
            system_prompt: system_prompt.as_deref(),
        });
        let result = harness::turn(
            &agent,
            &model,
            session_id.as_deref(),
            &message,
            &workdir,
            harness::HarnessLevel::Confined,
            attach.as_ref(),
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
                        threads.push_input(&thread_id, &from, &text);
                    }
                    Ok(HubMessage::StopThread { thread_id }) => {
                        threads.stop_one(&thread_id);
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
                    Ok(HubMessage::HarnessTurn {
                        conversation_id,
                        agent,
                        model,
                        session_id,
                        message,
                        mcp,
                        allowed_tools,
                        system_prompt,
                    }) => {
                        start_harness_turn(
                            conversation_id,
                            agent,
                            model,
                            session_id,
                            message,
                            mcp,
                            allowed_tools,
                            system_prompt,
                            config,
                            &threads,
                        );
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
            harness: None,
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

    /// Fake `claude` that records each turn under `$HOME` and, while `hold` exists, waits so a
    /// test can queue another message or kill the process. Never a real CLI.
    const FAKE_THREAD_CLAUDE: &str = r#"
use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

fn after(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .expect("home");
    let pid = std::process::id();
    fs::write(home.join("pid"), pid.to_string()).unwrap();
    let mut waited = false;
    while home.join("hold").exists() {
        waited = true;
        thread::sleep(Duration::from_millis(30));
    }
    let resume = after(&args, "--resume");
    let msg = after(&args, "-p").unwrap_or_default();
    let mode = after(&args, "--permission-mode").unwrap_or_default();
    let model = after(&args, "--model").unwrap_or_default();
    let cwd = env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
    let sid = resume.clone().unwrap_or_else(|| "thread-sess-1".into());
    let line = format!(
        "pid={pid}\nwaited={waited}\nresume={}\nmode={mode}\nmodel={model}\ncwd={cwd}\nmsg={msg}\n---\n",
        resume.unwrap_or_else(|| "NONE".into())
    );
    let log = home.join("invocations.log");
    let prev = fs::read_to_string(&log).unwrap_or_default();
    fs::write(&log, format!("{prev}{line}")).unwrap();
    let reply = msg.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "");
    println!(
        "{{\"type\":\"result\",\"is_error\":false,\"session_id\":\"{sid}\",\"result\":\"{reply}\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}"
    );
}
"#;

    fn fake_thread_claude() -> PathBuf {
        static BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BIN.get_or_init(|| {
            let dir = std::env::temp_dir().join("grouper-h3-fake-claude");
            std::fs::create_dir_all(&dir).unwrap();
            let src = dir.join("fake_thread_claude.rs");
            std::fs::write(&src, FAKE_THREAD_CLAUDE).unwrap();
            let exe = dir.join(if cfg!(windows) { "claude.exe" } else { "claude" });
            let status = std::process::Command::new("rustc")
                .arg(&src)
                .arg("-O")
                .arg("-o")
                .arg(&exe)
                .status()
                .expect("rustc");
            assert!(status.success(), "compiling fake claude failed");
            exe
        })
        .clone()
    }

    fn pid_alive(pid: u32) -> bool {
        #[cfg(windows)]
        {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .expect("tasklist");
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        }
        #[cfg(not(windows))]
        {
            std::path::Path::new(&format!("/proc/{pid}")).exists()
        }
    }

    struct Quiet;
    impl Events for Quiet {
        fn status(&self, _status: Status) {}
        fn presence(&self, _devices: Vec<DeviceInfo>) {}
    }

    fn harness_thread_spec(id: &str, brief: &str) -> ThreadSpec {
        ThreadSpec {
            thread_id: id.into(),
            conversation_id: "c-harness".into(),
            title: "Harness thread".into(),
            provider_id: "claude-code@pc".into(),
            model: "opus".into(),
            system: String::new(),
            brief: brief.into(),
            permissions: HashMap::new(),
            max_turns: 4,
            device_name: "Gaming PC".into(),
            ask_timeout_secs: Some(core_protocol::DEFAULT_ASK_TIMEOUT_SECS),
            fallback_models: Vec::new(),
            harness: Some(core_protocol::HarnessThread {
                agent: "claude-code".into(),
                model: "opus".into(),
                session: None,
            }),
        }
    }

    struct HarnessRun {
        dir: PathBuf,
        threads: Arc<Threads>,
        rx: mpsc::Receiver<NodeMessage>,
    }

    impl HarnessRun {
        fn start(id: &str, brief: &str, hold: bool) -> Self {
            let dir = std::env::temp_dir().join(format!("grouper-h3-thread-{id}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let home = dir.join("home");
            std::fs::create_dir_all(&home).unwrap();
            if hold {
                std::fs::write(home.join("hold"), "1").unwrap();
            }
            let bin = fake_thread_claude();
            let opts = harness::ScanOptions::isolated(&home, vec![bin.parent().unwrap().to_path_buf()], 1_700_000_000);
            let (tx, rx) = mpsc::channel(CONN_QUEUE_CAP);
            let threads = Arc::new(Threads::new(tx, Arc::new(|| {})));
            let events: Arc<dyn Events> = Arc::new(Quiet);
            start_harness_thread(
                harness_thread_spec(id, brief),
                &dir.join("workspaces"),
                &events,
                &threads,
                &Arc::new(Folders::default()),
                opts,
            );
            Self { dir, threads, rx }
        }

        fn home(&self) -> PathBuf {
            self.dir.join("home")
        }

        async fn recv(&mut self) -> NodeMessage {
            tokio::time::timeout(Duration::from_secs(20), self.rx.recv())
                .await
                .expect("harness thread timed out")
                .expect("output closed")
        }
    }

    impl Drop for HarnessRun {
        fn drop(&mut self) {
            let ids: Vec<String> = self.threads.harness_runs.lock().unwrap().keys().cloned().collect();
            for id in ids {
                self.threads.stop_one(&id);
            }
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    async fn wait_pid(home: &Path) -> u32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(text) = std::fs::read_to_string(home.join("pid"))
                && let Ok(pid) = text.trim().parse::<u32>()
                && pid_alive(pid)
            {
                return pid;
            }
            if std::time::Instant::now() > deadline {
                panic!("fake claude never started");
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    #[tokio::test]
    async fn harness_thread_start_runs_a_turn_and_a_message_resumes_the_session() {
        let mut run = HarnessRun::start("t-resume", "Write the note.", true);
        let pid = wait_pid(&run.home()).await;
        assert!(pid_alive(pid), "first turn should be inside the fake CLI");
        run.threads.push_input("t-resume", "user", "ship it");
        std::fs::remove_file(run.home().join("hold")).unwrap();

        let mut session = None;
        let mut summary = None;
        let mut saw_text = false;
        while summary.is_none() {
            match run.recv().await {
                NodeMessage::ThreadHarnessSession { thread_id, session_id } => {
                    assert_eq!(thread_id, "t-resume");
                    session = Some(session_id);
                }
                NodeMessage::ThreadEvent { event: AgentEvent::Text { text }, .. } => {
                    saw_text = true;
                    assert!(text.contains("Write the note.") || text.contains("ship it"), "{text}");
                }
                NodeMessage::ThreadFinished { thread_id, ok, summary: done } => {
                    assert_eq!(thread_id, "t-resume");
                    assert!(ok, "{done}");
                    summary = Some(done);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(session.as_deref(), Some("thread-sess-1"));
        assert!(saw_text);
        let summary = summary.unwrap();
        assert!(summary.contains("ship it"), "{summary}");
        assert!(summary.contains("[Message from the user]"), "{summary}");

        let log = std::fs::read_to_string(run.home().join("invocations.log")).unwrap();
        let turns: Vec<&str> = log.split("---\n").filter(|s| !s.trim().is_empty()).collect();
        assert_eq!(turns.len(), 2, "{log}");
        assert!(turns[0].contains("resume=NONE"), "{log}");
        assert!(turns[0].contains("mode=acceptEdits"), "{log}");
        assert!(turns[0].contains("model=opus"), "{log}");
        assert!(turns[0].contains("msg=Write the note."), "{log}");
        assert!(turns[0].contains("cwd="), "{log}");
        assert!(
            turns[0].contains("t-resume"),
            "workdir should be the thread workspace: {log}"
        );
        assert!(turns[1].contains("resume=thread-sess-1"), "{log}");
        assert!(turns[1].contains("mode=acceptEdits"), "{log}");
        assert!(turns[1].contains("ship it"), "{log}");
        assert!(run.threads.harness_runs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn harness_thread_stop_kills_the_cli_process() {
        let mut run = HarnessRun::start("stop", "Stay running.", true);
        let pid = wait_pid(&run.home()).await;
        assert!(pid_alive(pid));
        run.threads.stop_one("stop");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pid_alive(pid) {
            if std::time::Instant::now() > deadline {
                panic!("stop_thread left fake claude {pid} running");
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(run.threads.harness_runs.lock().unwrap().is_empty());
        assert!(!run.home().join("invocations.log").exists(), "killed before the CLI finished the turn");
        let late = tokio::time::timeout(Duration::from_millis(400), run.rx.recv()).await;
        assert!(late.is_err() || late.unwrap().is_none(), "stop does not finish the thread with the CLI reply");
    }
}
