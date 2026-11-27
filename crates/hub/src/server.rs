//! The hub: device connections, presence, the HTTP API and the live event stream.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Json, Router};
use core_agent::thread::ThreadControl;
use core_protocol::{
    ChatRequest, ChatResponse, DeviceInfo, Event, EventBody, HubMessage, IDLE_TIMEOUT_SECS, MAX_WS_MESSAGE_BYTES,
    ModelEntry, NodeMessage, PROTOCOL_VERSION, PendingApproval, Permission, ProviderConfig, ProviderInfo,
    ToolCallRequest, ToolDef, ToolSummary,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};

use crate::db::{Auth, Db};
use crate::device_provider::ModelRoute;
use crate::mcp::McpSession;
use crate::orchestrator::Wake;

const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum outgoing frames buffered per device connection before dropping a slow peer.
pub(crate) const CONN_QUEUE_CAP: usize = 1024;
const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// The hub's own id in the device and tool registries.
pub const HUB_DEVICE: &str = "hub";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseReason {
    Replaced,
    Overflow,
    /// The device's token was revoked (`DELETE /v1/devices/{id}`).
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueResult {
    Sent,
    Full,
    Gone,
}

/// One-shot trigger to close a live device session from outside its read loop.
#[derive(Clone)]
pub(crate) struct CloseHandle(Arc<std::sync::Mutex<Option<oneshot::Sender<CloseReason>>>>);

impl CloseHandle {
    pub(crate) fn new() -> (Self, oneshot::Receiver<CloseReason>) {
        let (tx, rx) = oneshot::channel();
        (Self(Arc::new(std::sync::Mutex::new(Some(tx)))), rx)
    }

    pub(crate) fn trigger(&self, reason: CloseReason) {
        if let Some(tx) = self.0.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(reason);
        }
    }
}

pub(crate) struct Live {
    pub(crate) conn_id: u64,
    pub(crate) tx: mpsc::Sender<HubMessage>,
    /// The device's tools, as its MCP server last listed them.
    pub(crate) tools: Vec<ToolDef>,
    /// The hub's MCP client for this connection, once initialized.
    pub(crate) mcp: Option<rmcp::Peer<rmcp::RoleClient>>,
    /// Whether this device opted in to read-only subscription usage probes.
    pub(crate) usage_probes: bool,
    pub(crate) close: CloseHandle,
    /// Becomes `true` once the session has sent any final error, dropped its writer and aborted its tasks.
    pub(crate) closed: watch::Receiver<bool>,
}

/// Where to deliver `WorkspaceFileResult` frames for an in-flight hand-over.
pub(crate) struct HandoverAckRoute {
    pub(crate) device_id: String,
    pub(crate) conn_id: u64,
    pub(crate) tx: mpsc::Sender<(String, bool, Option<String>)>,
}

/// What the hub tells whoever is waiting on an approval.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApprovalAnswer {
    Allow,
    Deny,
    /// Nobody answered within `secs`.
    TimedOut { secs: u64 },
}

/// Where to send the answer to an approval request.
pub(crate) struct ApprovalRoute {
    pub(crate) device_id: String,
    pub(crate) conversation_id: String,
    pub(crate) thread_id: String,
    pub(crate) tool: String,
    pub(crate) input: Value,
    pub(crate) reason: String,
    pub(crate) requested_at: u64,
    pub(crate) timeout_secs: Option<u64>,
    /// Set for threads running on the hub itself (and for the hub's hand-over copy).
    pub(crate) local: Option<oneshot::Sender<ApprovalAnswer>>,
    /// The approval id the remote device sent, when the hub assigned its own random id.
    pub(crate) device_approval_id: Option<String>,
    /// Distinguishes this route from any later request that happens to use the same id.
    pub(crate) generation: u64,
    /// Cancelled whenever the request is answered or cleared.
    pub(crate) timer: Option<tokio::task::AbortHandle>,
}

pub struct Hub {
    pub db: Db,
    pub data_dir: PathBuf,
    pub(crate) live: Mutex<HashMap<String, Live>>,
    pub(crate) approvals: Mutex<HashMap<String, ApprovalRoute>>,
    pub(crate) handover_acks: Mutex<HashMap<String, HandoverAckRoute>>,
    /// Concurrent model calls currently relayed through hosting devices.
    pub(crate) model_calls: Mutex<HashMap<String, ModelRoute>>,
    /// In-flight standard-chat harness turns waiting on a hosting device, keyed by conversation id.
    pub(crate) harness_turns: Mutex<HashMap<String, crate::harness::PendingHarnessTurn>>,
    /// Requester call ids already used on each live connection; prevents reuse
    /// after a route finishes as well as simultaneous duplicates.
    pub(crate) model_request_ids: Mutex<HashMap<(String, u64), HashSet<String>>>,
    /// Usage already accounted from a relayed ModelResponse, awaiting the device's
    /// matching informational AgentEvent::Usage.
    pub(crate) relayed_usage: std::sync::Mutex<HashMap<String, Vec<providers::Usage>>>,
    /// Model ids with an in-progress built-in probe suite.
    pub(crate) probe_runs: std::sync::Mutex<HashSet<String>>,
    /// Calls (tool + input) the user denied, per conversation. The same call from any
    /// thread of that conversation is refused without asking again until the user next
    /// speaks in it, so a model can't get a denial re-asked by starting a new thread.
    pub(crate) denied_calls: std::sync::Mutex<HashMap<String, HashSet<String>>>,
    handover_ack_timeout: std::sync::Mutex<Duration>,
    pub(crate) hub_threads: Mutex<HashMap<String, ThreadControl>>,
    pub(crate) orchestrators: Mutex<HashMap<String, mpsc::UnboundedSender<Wake>>>,
    events: broadcast::Sender<Event>,
    session_invalidations: broadcast::Sender<String>,
    /// Held while an event is stored and broadcast, so subscribers receive stored
    /// events in `seq` order (they drop a seq lower than one they've already seen).
    emit_order: std::sync::Mutex<()>,
    pub(crate) next_id: AtomicU64,
    pub(crate) limits: crate::limits::LimitState,
    /// Sign in with Google: trusted clients, nonces, `state`s and Google's signing keys.
    pub(crate) google: crate::google_auth::GoogleAuth,
    /// Web Push notifications for approvals ([`crate::push`]).
    pub(crate) push: crate::push::PushState,
    /// Device link rate limits ([`crate::link`]).
    pub(crate) link: crate::link::LinkState,
    /// Cloudflare tunnel supervisor state ([`crate::tunnel`]).
    pub(crate) tunnel: crate::tunnel::TunnelState,
    /// Step-down detection state ([`crate::stepdown`]).
    pub(crate) stepdown: crate::stepdown::StepDownState,
    /// Hub move state machine ([`crate::migrate`]).
    pub(crate) migrate: crate::migrate::MigrateState,
    /// Last-seen host capability metadata per device id ([`crate::devices`]).
    pub(crate) host_info: std::sync::Mutex<HashMap<String, core_protocol::NodeHostInfo>>,
    /// Streamable HTTP session store for `/mcp/orchestrator`. One per hub, created on first use.
    pub(crate) orchestrator_mcp: std::sync::OnceLock<crate::mcp_server::OrchestratorHttp>,
    /// Loopback port serving `/mcp/orchestrator`. Zero until a listener is bound.
    pub(crate) loopback_port: AtomicU16,
}

impl Hub {
    pub fn new(db: Db, data_dir: PathBuf) -> Arc<Self> {
        let initial_push_subs = db.push_subscription_count();
        Arc::new_cyclic(|hub| Self {
            db,
            data_dir,
            live: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            handover_acks: Mutex::new(HashMap::new()),
            model_calls: Mutex::new(HashMap::new()),
            harness_turns: Mutex::new(HashMap::new()),
            model_request_ids: Mutex::new(HashMap::new()),
            relayed_usage: std::sync::Mutex::new(HashMap::new()),
            probe_runs: std::sync::Mutex::new(HashSet::new()),
            denied_calls: std::sync::Mutex::new(HashMap::new()),
            handover_ack_timeout: std::sync::Mutex::new(crate::offline::HANDOVER_ACK_TIMEOUT),
            hub_threads: Mutex::new(HashMap::new()),
            orchestrators: Mutex::new(HashMap::new()),
            // Roomy: streamed text sends many small events.
            events: broadcast::channel(8192).0,
            session_invalidations: broadcast::channel(128).0,
            emit_order: std::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            limits: Default::default(),
            google: crate::google_auth::GoogleAuth::from_env(),
            push: crate::push::PushState::new(hub.clone(), initial_push_subs),
            link: Default::default(),
            tunnel: crate::tunnel::TunnelState::new(),
            stepdown: crate::stepdown::StepDownState::new(),
            migrate: crate::migrate::MigrateState::new(),
            host_info: std::sync::Mutex::new(HashMap::new()),
            orchestrator_mcp: std::sync::OnceLock::new(),
            loopback_port: AtomicU16::new(0),
        })
    }

    /// Record the loopback port that serves `/mcp/orchestrator` for harness orchestrators.
    pub(crate) fn set_loopback_port(&self, port: u16) {
        self.loopback_port.store(port, Ordering::Relaxed);
    }

    pub(crate) fn loopback_port(&self) -> u16 {
        self.loopback_port.load(Ordering::Relaxed)
    }

    /// Delete a chat, revoke its orchestrator tokens, and stop its worker.
    pub async fn delete_conversation(self: &Arc<Self>, id: &str) -> anyhow::Result<bool> {
        let deleted = self.db.delete_conversation(id)?;
        if deleted {
            self.orchestrators.lock().await.remove(id);
        }
        Ok(deleted)
    }

    /// Cloudflare tunnel supervisor state and status.
    pub fn tunnel(&self) -> &crate::tunnel::TunnelState {
        &self.tunnel
    }

    /// Step-down state (whether a newer hub for this account replaced us).
    pub fn stepdown(&self) -> &crate::stepdown::StepDownState {
        &self.stepdown
    }

    /// Whether this hub has stepped down after detecting a higher-epoch peer hub.
    pub fn is_stepped_down(&self) -> bool {
        self.stepdown.is_stepped_down()
    }

    /// Hub move state machine and progress.
    pub fn migrate(&self) -> &crate::migrate::MigrateState {
        &self.migrate
    }

    /// Whether writes are currently paused for an active hub move.
    pub fn is_move_paused(&self) -> bool {
        self.migrate.is_paused()
    }

    /// [`Db::issue_device`], then the live side of a reused device: its old token's browser sessions
    /// and node connection close as on revocation, and clients see the presence change.
    pub(crate) async fn issue_device(
        &self,
        name: &str,
        platform: &str,
        admin: bool,
        install: Option<crate::db::Install<'_>>,
        max_live: Option<usize>,
    ) -> anyhow::Result<crate::db::IssuedDevice> {
        let issued = self.db.issue_device(name, platform, admin, install, max_live)?;
        if issued.reused {
            for session in &issued.ended_sessions {
                self.invalidate_session(session);
            }
            self.sync_push_sub_count();
            if let Some(live) = self.live.lock().await.get(&issued.device.id) {
                live.close.trigger(CloseReason::Revoked);
            }
            tracing::info!(device = %issued.device.id, "same install signed in again: new token, the old one is revoked");
        }
        self.broadcast_presence(&issued.device.id).await;
        Ok(issued)
    }

    /// A globally unique approval id that never collides across hub restarts.
    pub(crate) fn new_approval_id() -> String {
        format!("a_{}", crate::db::random_hex(12))
    }

    /// Override how long a hand-over waits for `WorkspaceFileResult` acknowledgements.
    pub fn set_handover_ack_timeout(&self, timeout: Duration) {
        *self.handover_ack_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
    }

    pub(crate) fn handover_ack_timeout(&self) -> Duration {
        *self.handover_ack_timeout.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn subscribe_events_for_test(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Store an event and send it to everyone watching.
    pub fn emit(&self, conversation_id: &str, thread_id: Option<&str>, body: EventBody) -> Event {
        let _order = self.emit_order.lock().unwrap_or_else(|e| e.into_inner());
        let e = self.db.add_event(conversation_id, thread_id, body);
        let _ = self.events.send(e.clone());
        self.push_on_event(&e);
        e
    }

    /// Send an event to everyone watching without storing it: streamed text, whose
    /// full message is stored separately once complete. Live-only events have seq 0.
    pub fn emit_live(&self, conversation_id: &str, thread_id: Option<&str>, body: EventBody) {
        let _ = self.events.send(Event {
            seq: 0,
            conversation_id: conversation_id.into(),
            thread_id: thread_id.map(String::from),
            ts: crate::db::now(),
            body,
        });
    }

    pub async fn devices(&self) -> Vec<DeviceInfo> {
        let live = self.live.lock().await;
        let mut out = vec![DeviceInfo {
            id: HUB_DEVICE.into(),
            name: "Hub".into(),
            platform: std::env::consts::OS.into(),
            online: true,
            capabilities: vec![],
            last_seen: Some(crate::db::now()),
            folders: vec![],
        }];
        out.extend(self.db.devices().into_iter().filter(|d| !d.revoked).map(|d| {
            let l = live.get(&d.id);
            let folders = self.db.device_folders(&d.id);
            DeviceInfo {
                id: d.id,
                name: d.name,
                platform: d.platform,
                online: l.is_some(),
                capabilities: l
                    .map(|l| {
                        l.tools
                            .iter()
                            .map(|t| ToolSummary {
                                name: t.name.clone(),
                                description: t.description.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                last_seen: d.last_seen,
                folders,
            }
        }));
        out
    }

    pub async fn is_online(&self, device_id: &str) -> bool {
        device_id == HUB_DEVICE || self.live.lock().await.contains_key(device_id)
    }

    pub(crate) async fn is_live_conn(&self, device_id: &str, conn_id: u64) -> bool {
        self.live.lock().await.get(device_id).is_some_and(|l| l.conn_id == conn_id)
    }

    pub async fn send_to(&self, device_id: &str, msg: HubMessage) -> bool {
        let mut live = self.live.lock().await;
        let Some(l) = live.get(device_id) else { return false };
        match l.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(device = %device_id, conn_id = l.conn_id, "connection queue full; dropping slow peer");
                if let Some(dropped) = live.remove(device_id) {
                    dropped.close.trigger(CloseReason::Overflow);
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub(crate) async fn send_to_conn(&self, device_id: &str, conn_id: u64, msg: HubMessage) -> bool {
        let mut live = self.live.lock().await;
        let Some(l) = live.get(device_id).filter(|l| l.conn_id == conn_id) else { return false };
        match l.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(device = %device_id, conn_id, "connection queue full; dropping slow peer");
                if let Some(dropped) = live.remove(device_id) {
                    dropped.close.trigger(CloseReason::Overflow);
                }
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Queue a model-control frame without treating backpressure as a reason to
    /// disconnect the provider host. Callers turn `Full` into a busy response.
    pub(crate) async fn queue_to_model_host(&self, device_id: &str, conn_id: u64, msg: HubMessage) -> QueueResult {
        let live = self.live.lock().await;
        let Some(connection) = live.get(device_id).filter(|connection| connection.conn_id == conn_id) else {
            return QueueResult::Gone;
        };
        match connection.tx.try_send(msg) {
            Ok(()) => QueueResult::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => QueueResult::Full,
            Err(mpsc::error::TrySendError::Closed(_)) => QueueResult::Gone,
        }
    }

    async fn broadcast(&self, msg: HubMessage) {
        let mut live = self.live.lock().await;
        let mut overflowed = Vec::new();
        for (device_id, l) in live.iter() {
            if let Err(mpsc::error::TrySendError::Full(_)) = l.tx.try_send(msg.clone()) {
                tracing::warn!(
                    device = %device_id,
                    conn_id = l.conn_id,
                    "connection queue full during broadcast; dropping slow peer"
                );
                overflowed.push(device_id.clone());
            }
        }
        for device_id in overflowed {
            if let Some(dropped) = live.remove(&device_id) {
                dropped.close.trigger(CloseReason::Overflow);
            }
        }
    }

    pub(crate) async fn workspace_file_result(
        &self,
        device_id: &str,
        conn_id: u64,
        thread_id: &str,
        path: String,
        ok: bool,
        error: Option<String>,
    ) {
        let tx = self
            .handover_acks
            .lock()
            .await
            .get(thread_id)
            .filter(|r| r.device_id == device_id && r.conn_id == conn_id)
            .map(|r| r.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.try_send((path, ok, error));
        }
    }

    pub(crate) async fn broadcast_presence(&self, changed_device: &str) {
        let devices = self.devices().await;
        let changed = devices.iter().find(|d| d.id == changed_device).cloned();
        let online = changed.as_ref().is_some_and(|d| d.online);
        self.broadcast(HubMessage::Presence { devices }).await;
        self.emit_live(
            "",
            None,
            EventBody::DevicePresence { device_id: changed_device.to_string(), online, device: changed },
        );
    }

    pub async fn broadcast_providers(&self) {
        match self.db.providers() {
            Ok(providers) => self.broadcast(HubMessage::Providers { providers }).await,
            Err(error) => tracing::error!("could not decrypt provider configuration: {error:#}"),
        }
    }

    /// Who is calling: the `Authorization: Bearer` token when there is one (used alone,
    /// as for every API client), otherwise the web client's session cookie
    /// ([`crate::session`]), which carries its device's admin flag.
    pub(crate) fn auth(&self, headers: &HeaderMap) -> Option<Auth> {
        if headers.contains_key(header::AUTHORIZATION) {
            return self.auth_bearer(headers);
        }
        crate::session::cookie_sessions(headers).into_iter().find_map(|id| self.db.session_auth(id))
    }

    pub(crate) fn invalidate_session(&self, id: &str) {
        let _ = self.session_invalidations.send(id.to_string());
    }

    /// Only a Bearer token: for the device connection, which a browser session never opens.
    pub(crate) fn auth_bearer(&self, headers: &HeaderMap) -> Option<Auth> {
        let token = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
        self.db.authenticate(token)
    }

    /// A user's message: create the conversation if needed and wake its agent.
    pub async fn chat(self: &Arc<Self>, req: ChatRequest) -> anyhow::Result<String> {
        anyhow::ensure!(!self.is_move_paused(), "Your hub is moving");
        let text = req.text.trim().to_string();
        anyhow::ensure!(!text.is_empty(), "message is empty");
        let requested = req.model.as_deref().map(str::trim).filter(|model| !model.is_empty()).map(str::to_string);
        let requested_model = match &requested {
            Some(model) => {
                let Some(found) = self.db.find_model(model) else {
                    anyhow::bail!("no model {model:?} in the registry");
                };
                if found.hidden {
                    anyhow::bail!("model {} is hidden and cannot be selected", found.id);
                }
                Some(found)
            }
            None => None,
        };
        let target_mode = match &req.conversation_id {
            Some(id) => {
                let Some(conv) = self.db.conversation(id) else {
                    anyhow::bail!("no such conversation");
                };
                if requested.is_some() && !self.db.history(id).is_empty() {
                    anyhow::bail!(
                        "model can be set on the first message; use POST /v1/conversations/{id}/model to change it"
                    );
                }
                conv.mode
            }
            None => {
                let mode = req.mode.as_deref().unwrap_or("project");
                anyhow::ensure!(mode == "project" || mode == "standard", "mode must be project or standard");
                mode.to_string()
            }
        };
        if target_mode == "project"
            && let Some(found) = &requested_model
        {
            crate::db::check_orchestrator_eligible(found)?;
        }
        let id = match &req.conversation_id {
            Some(id) => id.clone(),
            None => {
                let title: String = text.chars().take(60).collect();
                self.db.create_conversation(&target_mode, &title)?
            }
        };
        if let Some(model) = &requested {
            self.db.set_conversation_model(&id, Some(model))?;
        }
        self.forget_denials(&id);
        self.emit(&id, None, EventBody::UserMessage { text: text.clone() });
        self.wake(&id, Wake::User(text)).await;
        Ok(id)
    }

    /// Pending approvals, oldest first, optionally filtered to one conversation.
    pub async fn pending_approvals(&self, conversation: Option<&str>) -> Vec<PendingApproval> {
        let limits = self.db.limits();
        let approvals = self.approvals.lock().await;
        let mut out: Vec<PendingApproval> = approvals
            .iter()
            .filter(|(_, r)| conversation.is_none_or(|c| r.conversation_id == c))
            .map(|(approval_id, r)| {
                let active_timeout = r.timeout_secs.filter(|_| limits.approval_timeout_secs.is_some());
                PendingApproval {
                    approval_id: approval_id.clone(),
                    device_id: r.device_id.clone(),
                    thread_id: r.thread_id.clone(),
                    conversation_id: r.conversation_id.clone(),
                    tool: r.tool.clone(),
                    input: r.input.clone(),
                    reason: r.reason.clone(),
                    requested_at: r.requested_at,
                    timeout_secs: active_timeout,
                    timeout_at: active_timeout.map(|secs| r.requested_at.saturating_add(secs)),
                    timeout_action: active_timeout.map(|_| limits.approval_timeout_action),
                }
            })
            .collect();
        out.sort_by(|a, b| a.requested_at.cmp(&b.requested_at).then_with(|| a.approval_id.cmp(&b.approval_id)));
        out
    }

    /// Resolve an approval request. Returns false if it doesn't exist (any more).
    ///
    /// A denial also denies any other pending request for the same call in the same
    /// conversation (e.g. a model turn that asked for it twice at once).
    pub async fn resolve_approval(&self, approval_id: &str, approved: bool) -> bool {
        let mut resolved = Vec::new();
        {
            let mut approvals = self.approvals.lock().await;
            let Some(route) = approvals.remove(approval_id) else { return false };
            if !approved {
                let key = crate::threads::denial_key(&route.tool, &route.input);
                let twins: Vec<String> = approvals
                    .iter()
                    .filter(|(_, r)| {
                        r.conversation_id == route.conversation_id && crate::threads::denial_key(&r.tool, &r.input) == key
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                // Remembered while the routes are locked, so a request can't slip in between.
                self.denied_calls.lock().unwrap().entry(route.conversation_id.clone()).or_default().insert(key);
                resolved.extend(twins.into_iter().filter_map(|id| approvals.remove(&id).map(|r| (id, r))));
            }
            resolved.insert(0, (approval_id.to_string(), route));
        }
        for (id, route) in resolved {
            self.answer_route(&id, route, approved).await;
        }
        true
    }

    /// Tell whoever waits on a resolved approval, and the clients.
    async fn answer_route(&self, approval_id: &str, mut route: ApprovalRoute, approved: bool) {
        if let Some(timer) = route.timer.take() {
            timer.abort();
        }
        match route.local {
            Some(tx) => {
                let _ = tx.send(if approved { ApprovalAnswer::Allow } else { ApprovalAnswer::Deny });
            }
            None => {
                let device_id_to_send = route.device_approval_id.as_deref().unwrap_or(approval_id);
                self.send_to(
                    &route.device_id,
                    HubMessage::ApprovalResult { approval_id: device_id_to_send.into(), approved, timeout_secs: None },
                )
                .await;
            }
        }
        self.emit(
            &route.conversation_id,
            Some(&route.thread_id),
            EventBody::ApprovalResolved { approval_id: approval_id.into(), approved, reason: None },
        );
    }
}

#[derive(Debug, PartialEq)]
pub enum CallError {
    Offline,
    UnknownTool,
    Timeout,
    /// Arguments must be a JSON object.
    BadArguments,
}

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/hub", get(hub_info))
        .route(core_protocol::CONNECT_PATH, any(connect))
        .route("/v1/events", any(events_ws))
        .route("/v1/devices", get(list_devices).post(register_device))
        .route("/v1/devices/{id}", delete(revoke_device))
        .route("/v1/devices/{id}/call", post(call_device))
        .route("/v1/devices/{id}/tools/{tool}", put(set_permission))
        .route("/v1/tools", get(list_tools))
        .route("/v1/providers", get(list_providers))
        .route("/v1/providers/{id}", put(put_provider))
        .route("/v1/providers/{id}/sync", post(sync_provider_models))
        .route("/v1/providers/{id}/sync-models", post(sync_provider_models))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{id}", put(put_model).delete(delete_model))
        .route("/v1/usage", get(list_usage))
        .route("/v1/usage/snapshots", get(list_usage_snapshots))
        .route("/v1/usage/refresh", post(refresh_usage))
        .route("/v1/usage/free-requests", get(free_requests_today))
        .route("/v1/chat", post(chat))
        .route("/v1/conversations", get(list_conversations))
        .route("/v1/conversations/{id}", delete(delete_conversation_route))
        .route("/v1/conversations/{id}/model", post(set_conversation_model))
        .route("/v1/conversations/{id}/events", get(conversation_events))
        .route("/v1/threads", get(list_threads))
        .route("/v1/threads/{id}", get(get_thread))
        .route("/v1/threads/{id}/message", post(thread_message))
        .route("/v1/threads/{id}/stop", post(thread_stop))
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approvals/{id}", post(approval))
        .route("/v1/memories", get(list_memories).post(add_memory))
        .route("/v1/memories/{id}", put(update_memory).delete(delete_memory))
        .merge(crate::registry::routes())
        .merge(crate::catalog::routes())
        .merge(crate::probes::routes())
        .merge(crate::skills::routes())
        .merge(crate::limits::routes())
        .merge(crate::files::routes())
        .merge(crate::session::routes())
        .merge(crate::mcp_server::routes())
        .merge(crate::google_auth::routes())
        .merge(crate::link::routes())
        .merge(crate::push::routes())
        .merge(crate::tunnel::routes())
        .merge(crate::devices::routes())
        .merge(crate::migrate::routes())
        .merge(crate::web::routes())
        .with_state(hub.clone())
        .layer(axum::middleware::from_fn_with_state(hub, crate::migrate::pause_guard))
        .layer(axum::middleware::from_fn(crate::session::guard))
        .layer(axum::middleware::from_fn(crate::session::response_security))
}

pub(crate) fn err(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

pub(crate) fn model_rule_err(status: StatusCode, error: anyhow::Error) -> Response {
    if error.downcast_ref::<crate::db::HarnessCannotOrchestrate>().is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": error.to_string(),
                "code": crate::db::HARNESS_CANNOT_ORCHESTRATE_CODE,
            })),
        )
            .into_response();
    }
    err(status, error)
}

/// Any valid token: a device or the user's client. Enough to connect and to read.
macro_rules! authed {
    ($hub:expr, $headers:expr) => {
        match $hub.auth(&$headers) {
            Some(a) => a,
            None => return err(StatusCode::UNAUTHORIZED, "invalid or missing device token"),
        }
    };
}

/// An admin token (`grouper-hub device add --admin`). Required for anything that
/// changes access, answers approvals, touches provider keys, or acts on the user's
/// behalf, so a thread that reads its own device's token can't raise its access.
macro_rules! admin {
    ($hub:expr, $headers:expr) => {{
        let a = authed!($hub, $headers);
        if !a.admin {
            return err(StatusCode::FORBIDDEN, "this needs an admin token (grouper-hub device add --admin)");
        }
        a
    }};
}

async fn list_devices(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    authed!(hub, headers);
    Json(hub.devices().await).into_response()
}

/// Most devices `POST /v1/devices` lets one hub keep registered (revoked devices don't count).
pub const MAX_DEVICES: usize = 100;
const REVOKED_MESSAGE: &str = "this device's token was revoked on the hub";

#[derive(Deserialize)]
struct NewDevice {
    name: String,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    install_id: Option<String>,
}

/// A device name as clients send it: 1 to 64 characters, no control characters.
pub(crate) fn check_device_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err("name must be 1 to 64 characters, without control characters");
    }
    Ok(name)
}

/// A platform as clients send it: one short word (`unknown` when left out).
pub(crate) fn check_platform(platform: Option<&str>) -> Result<&str, &'static str> {
    let platform = platform.map(str::trim).filter(|p| !p.is_empty()).unwrap_or("unknown");
    if platform.len() > 32 || !platform.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("platform must be one short word, such as macos, windows, linux or ios");
    }
    Ok(platform)
}

/// An optional `install_id` as clients send it (absent or null is none; anything else must be valid).
pub(crate) fn check_install_id(install_id: Option<&str>) -> Result<Option<&str>, &'static str> {
    match install_id {
        None => Ok(None),
        Some(id) if crate::db::valid_install_id(id) => Ok(Some(id)),
        Some(_) => Err("install_id must be 16 to 64 characters of letters, digits, - and _"),
    }
}

/// How a failed [`Hub::issue_device`] is answered.
pub(crate) fn issue_error(e: anyhow::Error) -> Response {
    if e.downcast_ref::<crate::db::DeviceCapReached>().is_some() {
        return err(StatusCode::CONFLICT, e);
    }
    err(StatusCode::BAD_REQUEST, e)
}

/// `POST /v1/devices` (admin): register a plain device and return its token, which is shown only
/// here. The Mac app calls it so the owner never pastes a device token. Admin tokens are made only
/// on the hub's CLI. The body is read after the admin check, so a device token always gets 403.
/// With an `install_id` that a live plain device already has, that device gets a fresh token (the
/// old one is revoked) instead of a new device being made.
async fn register_device(State(hub): State<Arc<Hub>>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let caller = admin!(hub, headers).device_id;
    let Ok(req) = serde_json::from_slice::<NewDevice>(&body) else {
        return err(StatusCode::BAD_REQUEST, r#"body must be {"name": "...", "platform": "macos"}"#);
    };
    let (name, platform, install_id) =
        match (check_device_name(&req.name), check_platform(req.platform.as_deref()), check_install_id(req.install_id.as_deref())) {
            (Ok(n), Ok(p), Ok(i)) => (n, p, i),
            (Err(why), _, _) | (_, Err(why), _) | (_, _, Err(why)) => return err(StatusCode::BAD_REQUEST, why),
        };
    let install = install_id.map(|id| crate::db::Install { id, kind: crate::db::NODE_INSTALL });
    let device = match hub.issue_device(name, platform, false, install, Some(MAX_DEVICES)).await {
        Ok(issued) => issued,
        Err(e) => return issue_error(e),
    };
    let (token, reused, device) = (device.token, device.reused, device.device);
    tracing::info!(%caller, device = %device.id, name = %device.name, platform = %device.platform, reused, "device registered");
    let info = hub.devices().await.into_iter().find(|d| d.id == device.id);
    Json(json!({ "device": info, "token": token })).into_response()
}

/// `DELETE /v1/devices/{id}` (admin): revoke a device's token. Its web sessions stop working at
/// once and its live connection, if any, is closed. The caller can't revoke its own token here.
async fn revoke_device(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let caller = admin!(hub, headers).device_id;
    if !hub.db.devices().iter().any(|d| d.id == id && !d.revoked) {
        return err(StatusCode::NOT_FOUND, "no such device");
    }
    if id == caller {
        return err(
            StatusCode::CONFLICT,
            "that is the token making this request; revoke it from another admin client or the hub's CLI",
        );
    }
    if let Err(e) = hub.db.revoke_device(&id) {
        return err(StatusCode::NOT_FOUND, e);
    }
    tracing::info!(%caller, device = %id, "device revoked");
    if let Some(live) = hub.live.lock().await.get(&id) {
        live.close.trigger(CloseReason::Revoked);
    }
    hub.broadcast_presence(&id).await;
    Json(json!({ "revoked": id })).into_response()
}

async fn call_device(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ToolCallRequest>,
) -> Response {
    let caller = admin!(hub, headers).device_id;
    tracing::info!(%caller, device = %id, tool = %req.tool, "tool call");
    match hub.call_tool(&id, &req.tool, req.args).await {
        Ok(r) => Json(r).into_response(),
        Err(CallError::Offline) => err(StatusCode::CONFLICT, "device is offline"),
        Err(CallError::UnknownTool) => err(StatusCode::NOT_FOUND, "device does not offer that tool"),
        Err(CallError::Timeout) => err(StatusCode::GATEWAY_TIMEOUT, "device did not answer in time"),
        Err(CallError::BadArguments) => err(StatusCode::BAD_REQUEST, "args must be a JSON object"),
    }
}

#[derive(Deserialize)]
struct ToolsFilter {
    device: Option<String>,
    #[serde(default)]
    all: bool,
}

async fn list_tools(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<ToolsFilter>) -> Response {
    authed!(hub, headers);
    let tools = if q.all { hub.db.all_tools(q.device.as_deref()) } else { hub.db.tools(q.device.as_deref()) };
    Json(tools).into_response()
}

#[derive(Deserialize)]
struct PermissionBody {
    permission: String,
}

/// Only reachable with an admin token (that is, by the user); no model has a tool for this.
async fn set_permission(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path((device, tool)): Path<(String, String)>,
    Json(body): Json<PermissionBody>,
) -> Response {
    admin!(hub, headers);
    let Some(p) = Permission::parse(&body.permission) else {
        return err(StatusCode::BAD_REQUEST, "permission must be manual, auto or bypass");
    };
    match hub.db.set_permission(&device, &tool, p) {
        Ok(()) => Json(json!({"device_id": device, "tool": tool, "permission": p})).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

async fn list_providers(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    admin!(hub, headers);
    let providers = match hub.db.providers() {
        Ok(providers) => providers,
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    let list: Vec<ProviderInfo> = providers
        .into_iter()
        .map(|p| ProviderInfo {
            via_device: p.via_device().map(String::from),
            id: p.id,
            kind: p.kind,
            base_url: p.base_url,
            has_key: p.api_key.as_deref().is_some_and(|k| !k.is_empty()),
            options: p.options,
        })
        .collect();
    Json(list).into_response()
}

async fn put_provider(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(mut p): Json<ProviderConfig>,
) -> Response {
    admin!(hub, headers);
    p.id = id;
    if let Err(e) = hub.db.upsert_provider(&p) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    hub.broadcast_providers().await;
    if let Ok(Some(saved)) = hub.db.provider(&p.id)
        && crate::sync::is_syncable(&saved)
    {
        crate::sync::spawn_provider_sync(hub.clone(), p.id.clone());
    }
    Json(json!({"id": p.id, "saved": true})).into_response()
}

async fn sync_provider_models(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    admin!(hub, headers);
    match crate::sync::sync_provider(&hub, &id).await {
        Ok(result) => Json(result).into_response(),
        Err(crate::sync::SyncError::NotFound(msg)) => err(StatusCode::NOT_FOUND, msg),
        Err(crate::sync::SyncError::NotSyncable(msg)) => err(StatusCode::BAD_REQUEST, msg),
    }
}

#[derive(Deserialize, Default)]
struct ModelsFilter {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    include_hidden: bool,
}

async fn list_models(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<ModelsFilter>) -> Response {
    authed!(hub, headers);
    let models = if q.all || q.include_hidden { hub.db.models() } else { hub.db.visible_models() };
    Json(models).into_response()
}

async fn put_model(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(mut m): Json<ModelEntry>,
) -> Response {
    admin!(hub, headers);
    m.id = id;
    match hub.db.upsert_model(&m) {
        Ok(()) => Json(hub.db.find_model(&m.id).unwrap_or(m)).into_response(),
        Err(e) => model_rule_err(StatusCode::BAD_REQUEST, e),
    }
}

async fn delete_model(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    admin!(hub, headers);
    match hub.db.delete_model(&id) {
        Ok(()) => Json(json!({"deleted": id})).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize)]
struct UsageQuery {
    since: Option<String>,
    until: Option<String>,
    group: Option<String>,
    conversation: Option<String>,
    /// Only calls through this provider id.
    provider: Option<String>,
}

/// Metered totals. Dates are UTC; the end of the range is exclusive unless a
/// date-only `until` is supplied, in which case that whole day is included.
async fn list_usage(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<UsageQuery>) -> Response {
    admin!(hub, headers);
    let current = crate::db::now();
    let since = match crate::usage::parse_time(q.since.as_deref(), current, false) {
        Ok(value) => value,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    let until = match crate::usage::parse_time(q.until.as_deref(), current, true) {
        Ok(value) => value,
        Err(error) => return err(StatusCode::BAD_REQUEST, error),
    };
    if since >= until {
        return err(StatusCode::BAD_REQUEST, "since must be before until");
    }
    let provider = q.provider.as_deref().map(str::trim).filter(|p| !p.is_empty());
    match hub.db.usage_totals(since, until, q.group.as_deref().unwrap_or("day"), q.conversation.as_deref(), provider) {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => err(StatusCode::BAD_REQUEST, error),
    }
}

async fn list_usage_snapshots(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    admin!(hub, headers);
    match hub.db.latest_usage_snapshots() {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

/// Calls Grouper sent to OpenRouter `:free` models today (UTC), per OpenRouter provider.
/// Grouper's own count, not OpenRouter's allowance.
async fn free_requests_today(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    admin!(hub, headers);
    match crate::usage::free_requests_today(&hub.db, crate::db::now()) {
        Ok(report) => Json(report).into_response(),
        Err(error) => err(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

async fn refresh_usage(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    admin!(hub, headers);
    Json(crate::usage::refresh(&hub).await).into_response()
}

async fn chat(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(req): Json<ChatRequest>) -> Response {
    admin!(hub, headers);
    match hub.chat(req).await {
        Ok(conversation_id) => Json(ChatResponse { conversation_id }).into_response(),
        Err(e) => model_rule_err(StatusCode::BAD_REQUEST, e),
    }
}

async fn list_conversations(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    authed!(hub, headers);
    Json(hub.db.conversations()).into_response()
}

async fn delete_conversation_route(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    admin!(hub, headers);
    match hub.delete_conversation(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such conversation"),
        Err(error) => err(StatusCode::BAD_REQUEST, error),
    }
}

#[derive(Deserialize)]
struct ConversationModelBody {
    /// Registry id, or null to use the role default again.
    model: Option<String>,
}

/// Set or clear the model this conversation's orchestrator or standard chat uses.
async fn set_conversation_model(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ConversationModelBody>,
) -> Response {
    admin!(hub, headers);
    let model = body.model.as_deref().map(str::trim).filter(|model| !model.is_empty());
    match hub.db.set_conversation_model(&id, model) {
        Ok(info) => Json(info).into_response(),
        Err(error) => {
            let status = if error.to_string().contains("no such conversation") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            model_rule_err(status, error)
        }
    }
}

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: i64,
    thread: Option<String>,
}

async fn conversation_events(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<EventQuery>,
) -> Response {
    authed!(hub, headers);
    Json(hub.db.events(&id, q.after, q.thread.as_deref())).into_response()
}

#[derive(Deserialize)]
struct ConversationFilter {
    conversation: Option<String>,
}

async fn list_threads(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Query(q): Query<ConversationFilter>,
) -> Response {
    authed!(hub, headers);
    Json(hub.db.threads(q.conversation.as_deref())).into_response()
}

async fn get_thread(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    authed!(hub, headers);
    match hub.db.thread(&id) {
        Some(t) => Json(t).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such thread"),
    }
}

#[derive(Deserialize)]
struct TextBody {
    text: String,
}

async fn thread_message(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<TextBody>,
) -> Response {
    admin!(hub, headers);
    match hub.user_thread_message(&id, &body.text).await {
        Ok(()) => Json(json!({"sent": true})).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn thread_stop(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    admin!(hub, headers);
    match hub.stop_thread(&id, "user").await {
        Ok(()) => Json(json!({"stopped": true})).into_response(),
        Err(e) => err(StatusCode::CONFLICT, e),
    }
}

async fn list_approvals(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Query(q): Query<ConversationFilter>,
) -> Response {
    admin!(hub, headers);
    Json(hub.pending_approvals(q.conversation.as_deref()).await).into_response()
}

#[derive(Deserialize)]
struct ApprovalBody {
    approve: bool,
}

async fn approval(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ApprovalBody>,
) -> Response {
    admin!(hub, headers);
    if hub.resolve_approval(&id, body.approve).await {
        Json(json!({"approval_id": id, "approved": body.approve})).into_response()
    } else {
        err(StatusCode::NOT_FOUND, "no pending approval with that id")
    }
}

/// Live event stream for clients: `GET /v1/events?conversation=<id>` (optional filter).
async fn events_ws(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Query(q): Query<ConversationFilter>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(credential) = EventCredential::from_headers(&hub, &headers) else {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing device token");
    };
    let mut last_seq = hub.db.last_event_seq();
    let mut rx = hub.events.subscribe();
    let mut invalidations = hub.session_invalidations.subscribe();
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |mut socket| async move {
        let mut revalidate = tokio::time::interval(Duration::from_secs(30));
        revalidate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        revalidate.tick().await;
        loop {
            tokio::select! {
                _ = revalidate.tick() => {
                    if !credential.valid(&hub) {
                        close_policy(&mut socket).await;
                        break;
                    }
                }
                invalidated = invalidations.recv() => {
                    let must_check = match invalidated {
                        Ok(id_hash) => credential.session_hash().is_some_and(|mine| mine == id_hash),
                        Err(broadcast::error::RecvError::Lagged(_)) => credential.session_hash().is_some(),
                        Err(broadcast::error::RecvError::Closed) => false,
                    };
                    if must_check && !credential.valid(&hub) {
                        close_policy(&mut socket).await;
                        break;
                    }
                }
                e = rx.recv() => match e {
                    Ok(e) => {
                        if q.conversation.as_ref().is_some_and(|c| *c != e.conversation_id) {
                            continue;
                        }
                        if e.seq > 0 && e.seq <= last_seq {
                            continue;
                        }
                        if !credential.valid(&hub) {
                            close_policy(&mut socket).await;
                            break;
                        }
                        let text = serde_json::to_string(&e).unwrap();
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                        if e.seq > 0 {
                            last_seq = e.seq;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("event subscriber lagged by {n}");
                        let mut failed = false;
                        for e in hub.db.events_since(last_seq, q.conversation.as_deref()) {
                            if !credential.valid(&hub) {
                                close_policy(&mut socket).await;
                                failed = true;
                                break;
                            }
                            let text = serde_json::to_string(&e).unwrap();
                            if socket.send(Message::Text(text.into())).await.is_err() {
                                failed = true;
                                break;
                            }
                            if e.seq > 0 {
                                last_seq = e.seq;
                            }
                        }
                        if failed {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                incoming = socket.recv() => match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    _ => {}
                },
            }
        }
    })
}

#[derive(Clone)]
enum EventCredential {
    Bearer(String),
    Session { id: String, id_hash: String },
}

impl EventCredential {
    fn from_headers(hub: &Hub, headers: &HeaderMap) -> Option<Self> {
        if headers.contains_key(header::AUTHORIZATION) {
            let token = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
            hub.db.authenticate(token)?;
            return Some(Self::Bearer(token.to_string()));
        }
        crate::session::cookie_sessions(headers)
            .into_iter()
            .find(|id| hub.db.session_auth(id).is_some())
            .map(|id| Self::Session { id: id.to_string(), id_hash: crate::session::hash(id) })
    }

    fn valid(&self, hub: &Hub) -> bool {
        match self {
            Self::Bearer(token) => hub.db.authenticate(token).is_some(),
            Self::Session { id, .. } => hub.db.session_auth(id).is_some(),
        }
    }

    fn session_hash(&self) -> Option<&str> {
        match self {
            Self::Bearer(_) => None,
            Self::Session { id_hash, .. } => Some(id_hash),
        }
    }
}

async fn close_policy(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code::POLICY,
            reason: "authentication expired or was revoked".into(),
        })))
        .await;
}

async fn connect(State(hub): State<Arc<Hub>>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(Auth { device_id, .. }) = hub.auth_bearer(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing device token");
    };
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            if let Err(e) = session(hub, device_id.clone(), socket).await {
                tracing::info!(device = %device_id, "session ended: {e}");
            }
        })
}

async fn session(hub: Arc<Hub>, device_id: String, socket: WebSocket) -> Result<(), String> {
    let (mut sink, mut stream) = socket.split();

    let first = tokio::time::timeout(HELLO_TIMEOUT, stream.next())
        .await
        .map_err(|_| "no hello")?
        .ok_or("closed before hello")?
        .map_err(|e| e.to_string())?;
    let hello_host = match first {
        Message::Text(t) => match serde_json::from_str::<NodeMessage>(&t) {
            Ok(NodeMessage::Hello { protocol_version, client_version, host }) => {
                if protocol_version != PROTOCOL_VERSION {
                    let msg = HubMessage::Error {
                        message: format!("protocol {protocol_version} unsupported, hub speaks {PROTOCOL_VERSION}"),
                    };
                    let _ = sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;
                    return Err("protocol mismatch".into());
                }
                tracing::info!(device = %device_id, %client_version, "connected");
                host
            }
            _ => return Err("first frame was not hello".into()),
        },
        _ => return Err("first frame was not text".into()),
    };
    if let Some(ref info) = hello_host {
        hub.record_host_info(&device_id, info.clone());
    }

    let providers = hub.db.providers().map_err(|error| error.to_string())?;
    // Close any live connection for this device before registering the new one and
    // before collecting orphan threads, so old tasks stop and cannot race the new session.
    let (conn_id, tx, mut rx, close_handle, mut close_rx, closed_tx, orphans) = loop {
        let mut live = hub.live.lock().await;
        if let Some(mut old) = live.remove(&device_id) {
            drop(live);
            old.close.trigger(CloseReason::Replaced);
            let _ = tokio::time::timeout(Duration::from_secs(2), old.closed.wait_for(|done| *done)).await;
            hub.drop_approvals(|r| r.device_id == device_id).await;
            continue;
        }
        let conn_id = hub.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel::<HubMessage>(CONN_QUEUE_CAP);
        let (close_handle, close_rx) = CloseHandle::new();
        let (closed_tx, closed_rx) = watch::channel(false);
        // A device runs one connection: threads still marked running from an earlier one
        // died with it on the device (and any live older connection is now closed).
        let orphans = hub.db.running_threads_on(&device_id);
        live.insert(
            device_id.clone(),
            Live {
                conn_id,
                tx: tx.clone(),
                tools: Vec::new(),
                mcp: None,
                usage_probes: false,
                close: close_handle.clone(),
                closed: closed_rx,
            },
        );
        break (conn_id, tx, rx, close_handle, close_rx, closed_tx, orphans);
    };
    hub.db.touch(&device_id);
    let _ = tx.try_send(HubMessage::Welcome {
        device_id: device_id.clone(),
        device_name: hub
            .db
            .devices()
            .into_iter()
            .find(|device| device.id == device_id)
            .map(|device| device.name)
            .unwrap_or_else(|| device_id.clone()),
        hub_version: HUB_VERSION.into(),
        protocol_version: PROTOCOL_VERSION,
    });
    let _ = tx.try_send(HubMessage::Providers { providers });
    hub.broadcast_presence(&device_id).await;
    // The hub's MCP client: lists the device's tools now and on every change.
    let mcp = McpSession::start(&hub, device_id.clone(), conn_id, tx.clone(), close_handle);
    // Threads waiting for this device continue here once its tools are known.
    tokio::spawn(crate::offline::device_connected(Arc::downgrade(&hub), device_id.clone(), conn_id, orphans));

    let (writer_close_tx, mut writer_close_rx) = oneshot::channel::<Option<HubMessage>>();
    let mut writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                final_msg = &mut writer_close_rx => {
                    if let Ok(Some(msg)) = final_msg {
                        let text = serde_json::to_string(&msg).expect("serialize");
                        let _ = tokio::time::timeout(CLOSE_GRACE, sink.send(Message::Text(text.into()))).await;
                    }
                    break;
                }
                msg = rx.recv() => match msg {
                    Some(msg) => {
                        let text = serde_json::to_string(&msg).expect("serialize");
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        let _ = tokio::time::timeout(CLOSE_GRACE, sink.close()).await;
    });

    let mut close_reason: Option<CloseReason> = None;
    let result = loop {
        let next = tokio::select! {
            biased;
            reason = &mut close_rx => {
                let r = reason.unwrap_or(CloseReason::Replaced);
                close_reason = Some(r);
                break Err(match r {
                    CloseReason::Replaced => "replaced by a newer connection".to_string(),
                    CloseReason::Overflow => "connection queue full".to_string(),
                    CloseReason::Revoked => REVOKED_MESSAGE.to_string(),
                });
            }
            next = tokio::time::timeout(Duration::from_secs(IDLE_TIMEOUT_SECS), stream.next()) => match next {
                Err(_) => break Err("idle timeout".to_string()),
                Ok(None) => break Ok(()),
                Ok(Some(Err(e))) => break Err(e.to_string()),
                Ok(Some(Ok(m))) => m,
            }
        };
        let text = match next {
            Message::Text(t) => t,
            Message::Close(_) => break Ok(()),
            _ => continue,
        };
        match serde_json::from_str::<NodeMessage>(&text) {
            Ok(NodeMessage::Ping) => {
                if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(HubMessage::Pong) {
                    close_reason = Some(CloseReason::Overflow);
                    break Err("connection queue full".to_string());
                }
            }
            Ok(NodeMessage::Mcp { message }) => {
                if hub.is_live_conn(&device_id, conn_id).await
                    && let Err(mpsc::error::TrySendError::Full(_)) = mcp.incoming.try_send(message)
                {
                    close_reason = Some(CloseReason::Overflow);
                    break Err("MCP incoming queue full".to_string());
                }
            }
            Ok(NodeMessage::ModelRequest { call_id, thread_id, provider_id, request }) => {
                if hub.is_live_conn(&device_id, conn_id).await {
                    hub.relay_model_request(&device_id, conn_id, call_id, thread_id, provider_id, request).await;
                }
            }
            Ok(NodeMessage::ModelDelta { call_id, delta }) => {
                hub.model_delta(&device_id, conn_id, &call_id, delta).await;
            }
            Ok(NodeMessage::ModelResponse { call_id, response, error }) => {
                hub.model_response(&device_id, conn_id, &call_id, response, error).await;
            }
            Ok(NodeMessage::ModelCancel { call_id }) => {
                hub.relay_model_cancel(&device_id, conn_id, &call_id).await;
            }
            Ok(
                msg @ (NodeMessage::ThreadEvent { .. }
                | NodeMessage::ThreadFinished { .. }
                | NodeMessage::ApprovalRequest { .. }
                | NodeMessage::AskOrchestrator { .. }
                | NodeMessage::ThreadHarnessSession { .. }),
            ) => {
                handle_thread_frame(&hub, &device_id, conn_id, msg).await;
            }
            Ok(NodeMessage::WorkspaceFileResult { thread_id, path, ok, error }) => {
                if hub.is_live_conn(&device_id, conn_id).await {
                    hub.workspace_file_result(&device_id, conn_id, &thread_id, path, ok, error).await;
                }
            }
            Ok(NodeMessage::HubMoveStatus { move_id, stage, progress_pct, error, target_url }) => {
                if hub.is_live_conn(&device_id, conn_id).await {
                    hub.handle_move_status(&device_id, &move_id, &stage, progress_pct, error, target_url).await;
                }
            }
            Ok(NodeMessage::HarnessReport { harnesses }) => {
                if hub.is_live_conn(&device_id, conn_id).await
                    && let Err(e) = crate::harness::apply_report(&hub.db, &device_id, &harnesses)
                {
                    tracing::warn!(device = %device_id, "applying harness report failed: {e:#}");
                }
            }
            Ok(NodeMessage::HarnessFinished { conversation_id, text, session_id, usage }) => {
                hub.harness_finished(&device_id, conn_id, &conversation_id, text, session_id, usage).await;
            }
            Ok(NodeMessage::HarnessFailed { conversation_id, error }) => {
                hub.harness_failed(&device_id, conn_id, &conversation_id, error).await;
            }
            Ok(NodeMessage::Hello { .. }) => {
                if let Err(mpsc::error::TrySendError::Full(_)) =
                    tx.try_send(HubMessage::Error { message: "already said hello".into() })
                {
                    close_reason = Some(CloseReason::Overflow);
                    break Err("connection queue full".to_string());
                }
            }
            Err(e) => {
                if let Err(mpsc::error::TrySendError::Full(_)) =
                    tx.try_send(HubMessage::Error { message: format!("bad frame: {e}") })
                {
                    close_reason = Some(CloseReason::Overflow);
                    break Err("connection queue full".to_string());
                }
            }
        }
    };

    hub.model_connection_gone(&device_id, conn_id).await;
    hub.harness_connection_gone(&device_id, conn_id).await;

    let call_device_gone = {
        let mut live = hub.live.lock().await;
        if live.get(&device_id).is_some_and(|l| l.conn_id == conn_id) {
            live.remove(&device_id);
            true
        } else {
            matches!(close_reason, Some(CloseReason::Overflow)) && !live.contains_key(&device_id)
        }
    };
    hub.handover_acks.lock().await.retain(|_, r| !(r.device_id == device_id && r.conn_id == conn_id));
    hub.db.touch(&device_id);
    drop(mcp);
    let final_error = match close_reason {
        Some(CloseReason::Replaced) => Some(HubMessage::Error { message: "replaced by a newer connection".into() }),
        Some(CloseReason::Overflow) => Some(HubMessage::Error { message: "connection queue full".into() }),
        Some(CloseReason::Revoked) => Some(HubMessage::Error { message: REVOKED_MESSAGE.into() }),
        None => None,
    };
    let _ = writer_close_tx.send(final_error);
    drop(tx);
    if tokio::time::timeout(CLOSE_GRACE + Duration::from_millis(200), &mut writer).await.is_err() {
        writer.abort();
    }
    let _ = closed_tx.send(true);
    if call_device_gone {
        hub.device_gone(&device_id).await;
        hub.broadcast_presence(&device_id).await;
    }
    tracing::info!(device = %device_id, "disconnected");
    result
}

/// Process a device thread frame only when `conn_id` is still the live connection for `device_id`.
async fn handle_thread_frame(hub: &Arc<Hub>, device_id: &str, conn_id: u64, msg: NodeMessage) {
    if !hub.is_live_conn(device_id, conn_id).await {
        return;
    }
    match msg {
        NodeMessage::ThreadEvent { thread_id, event } if hub.owns_thread(device_id, &thread_id) => {
            hub.thread_event(&thread_id, event);
        }
        NodeMessage::ThreadFinished { thread_id, ok, summary } if hub.owns_thread(device_id, &thread_id) => {
            if !ok {
                crate::usage::note_thread_failure(hub, &thread_id, &summary);
            }
            hub.thread_finished(&thread_id, ok, &summary).await;
            crate::usage::after_device_thread(hub, device_id, &thread_id);
        }
        NodeMessage::ApprovalRequest { approval_id, thread_id, tool, input, reason }
            if hub.owns_thread(device_id, &thread_id) =>
        {
            if !hub.request_device_approval(device_id, &thread_id, &approval_id, &tool, input, &reason).await {
                hub.send_to(
                    device_id,
                    HubMessage::ApprovalResult { approval_id, approved: false, timeout_secs: None },
                )
                .await;
            }
        }
        NodeMessage::AskOrchestrator { thread_id, question } if hub.owns_thread(device_id, &thread_id) => {
            hub.thread_ask(&thread_id, &question).await;
        }
        // Stored in step 2 of harness threads; the frame is accepted so an older build of this
        // match stays exhaustive once the variant exists.
        NodeMessage::ThreadHarnessSession { .. } => {}
        _ => {}
    }
}

// ---- long-term memory (personal, so admin only, reads included)

#[derive(Deserialize)]
struct MemoryQuery {
    q: Option<String>,
    limit: Option<usize>,
}

/// `GET /v1/memories?q=&limit=`: search, or the most recently changed without `q`.
async fn list_memories(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<MemoryQuery>) -> Response {
    admin!(hub, headers);
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let list = match q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => hub.db.search_memories(text, limit),
        None => hub.db.memories(limit),
    };
    Json(list).into_response()
}

async fn add_memory(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<core_protocol::MemoryInput>,
) -> Response {
    admin!(hub, headers);
    match hub.db.add_memory(&body.content, &body.tags.unwrap_or_default(), None) {
        Ok(m) => Json(m).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn update_memory(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(body): Json<core_protocol::MemoryInput>,
) -> Response {
    admin!(hub, headers);
    match hub.db.update_memory(id, &body.content, body.tags.as_deref()) {
        Ok(Some(m)) => Json(m).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, format!("no memory #{id}")),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn delete_memory(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    admin!(hub, headers);
    match hub.db.delete_memory(id) {
        Ok(true) => Json(json!({"deleted": id})).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, format!("no memory #{id}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// `GET /v1/hub`: public identity (no token needed; nothing private).
/// `key_sha256` is omitted while no hub identity key exists (arrives with LAN TLS).
/// The account as `GET /v1/hub` shows it: an opaque id, the first 16 bytes of
/// SHA-256("grouper-account:" + provider + ":" + sub) in hex. `/v1/hub` needs no sign-in, so it
/// never carries the owner's email; two hubs of one account still get the same id (step-down).
pub(crate) fn account_id(provider: &str, sub: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("grouper-account:{provider}:{sub}").as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

async fn hub_info(State(hub): State<Arc<Hub>>) -> Response {
    let (hub_device_id, epoch) = match hub.db.ensure_hub_device() {
        Ok(state) => state,
        Err(error) => return err(StatusCode::INTERNAL_SERVER_ERROR, error),
    };
    Json(core_protocol::HubInfo {
        hub_id: hub.db.hub_id(),
        hub_version: HUB_VERSION.into(),
        protocol_version: PROTOCOL_VERSION,
        account: hub.db.owner().map(|owner| account_id(&owner.provider, &owner.sub)),
        hub_device_id,
        epoch,
        key_sha256: hub.db.identity_key_sha256(),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_protocol::AgentEvent;
    use serde_json::Value;

    /// Streamed text reaches subscribers live, for the orchestrator and for a hub
    /// thread, but only the full messages are stored.
    #[tokio::test]
    async fn deltas_are_live_only() {
        let dir = std::env::temp_dir().join(format!("grouper-stream-test-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "s".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {"rules": [
                {"system": "orchestrator of Grouper", "last": "go now", "reply": [
                    {"type": "text", "text": "Starting a thread for that."},
                    {"type": "tool_use", "name": "start_thread", "input": {"title": "T", "device": "hub", "goal": "write"}}
                ]},
                {"system": "You are a subagent", "reply": [{"type": "text", "text": "thread result here"}]},
                {"system": "orchestrator of Grouper", "last": "done]", "reply": [{"type": "text", "text": "All finished."}]}
            ], "default": []}}),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "fake".into(),
            provider_id: "s".into(),
            model: "fake-1".into(),
            input_price: 0.0,
            output_price: 0.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: true,
            roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
            local: false,
            modalities: vec![],
            ..Default::default()
        })
        .unwrap();
        let hub = Hub::new(db, dir.clone());
        let mut rx = hub.events.subscribe();
        let conv = hub
            .chat(ChatRequest { conversation_id: None, mode: Some("project".into()), text: "go now".into(), model: None })
            .await
            .unwrap();

        let mut live = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let e = rx.recv().await.unwrap();
                let done = matches!(&e.body, EventBody::AssistantMessage { text } if text == "All finished.");
                live.push(e);
                if done {
                    break;
                }
            }
        })
        .await
        .expect("conversation finished");

        let orch_deltas: String = live
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::AssistantDelta { text } => {
                    assert_eq!((e.seq, e.thread_id.as_deref()), (0, None));
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(orch_deltas, "Starting a thread for that.All finished.");
        let thread_deltas: String = live
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ThreadActivity { event: AgentEvent::TextDelta { text } } => {
                    assert_eq!(e.seq, 0);
                    assert!(e.thread_id.is_some());
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(thread_deltas, "thread result here");

        let stored = hub.db.events(&conv, 0, None);
        assert!(stored.iter().all(|e| e.seq > 0));
        assert!(!stored.iter().any(|e| matches!(
            e.body,
            EventBody::AssistantDelta { .. } | EventBody::ThreadActivity { event: AgentEvent::TextDelta { .. } }
        )));
        let texts: Vec<Value> = stored
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::AssistantMessage { text } => Some(json!(text)),
                EventBody::ThreadActivity { event: AgentEvent::Text { text } } => Some(json!(["thread", text])),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec![json!("Starting a thread for that."), json!(["thread", "thread result here"]), json!("All finished.")]
        );
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    /// A conversation can pin its orchestrator or standard model. Threads in it still
    /// go through routing unless the orchestrator names one.
    #[tokio::test]
    async fn conversation_model_overrides_the_orchestrator_not_thread_routing() {
        let dir = std::env::temp_dir().join(format!("grouper-model-override-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "s".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {"rules": [
                {"system": "orchestrator of Grouper", "last": "go now", "reply": [
                    {"type": "text", "text": "Starting a thread for that."},
                    {"type": "tool_use", "name": "start_thread", "input": {"title": "T", "device": "hub", "goal": "write"}}
                ]},
                {"system": "You are a subagent", "reply": [{"type": "text", "text": "thread result here"}]},
                {"system": "orchestrator of Grouper", "last": "done]", "reply": [{"type": "text", "text": "All finished."}]},
                {"system": "orchestrator of Grouper", "last": "use the default", "reply": [{"type": "text", "text": "Default model speaking."}]},
                {"system": "You are Grouper", "last": "standard hello", "reply": [{"type": "text", "text": "Standard reply."}]}
            ], "default": [{"type": "text", "text": "OK"}]}}),
        })
        .unwrap();
        let entry = |id: &str, name: &str, roles: &[&str]| ModelEntry {
            id: id.into(),
            provider_id: "s".into(),
            model: name.into(),
            input_price: if id == "big" { 3.0 } else { 1.0 },
            output_price: 1.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: roles.iter().map(|role| (*role).to_string()).collect(),
            local: false,
            modalities: vec![],
            ..Default::default()
        };
        db.upsert_model(&entry("big", "big-model", &["orchestrator", "standard", "thread"])).unwrap();
        db.upsert_model(&entry("picked", "picked-model", &[])).unwrap();
        let hub = Hub::new(db, dir.clone());
        let mut rx = hub.events.subscribe();

        async fn until(rx: &mut tokio::sync::broadcast::Receiver<core_protocol::Event>, pred: impl Fn(&core_protocol::Event) -> bool) {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let event = rx.recv().await.expect("event stream closed");
                    if pred(&event) {
                        break;
                    }
                }
            })
            .await
            .expect("timed out waiting for an event");
        }

        let usage = |hub: &Hub, conversation: &str| -> Vec<(Option<String>, String)> {
            let connection = hub.db.c();
            let mut statement = connection
                .prepare("SELECT thread_id, model FROM usage WHERE conversation_id = ?1 ORDER BY id")
                .unwrap();
            statement
                .query_map([conversation], |row| Ok((row.get::<_, Option<String>>(0)?, row.get(1)?)))
                .unwrap()
                .filter_map(Result::ok)
                .collect()
        };

        let conv = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("project".into()),
                text: "go now".into(),
                model: Some("picked".into()),
            })
            .await
            .unwrap();
        until(&mut rx, |event| {
            matches!(&event.body, EventBody::AssistantMessage { text } if text == "All finished.")
        })
        .await;
        assert_eq!(hub.db.conversation(&conv).unwrap().model.as_deref(), Some("picked"));
        let threads = hub.db.threads(Some(&conv));
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].model, "big", "threads keep using routing, not the conversation override");
        let rows = usage(&hub, &conv);
        assert!(rows.iter().any(|(thread, model)| thread.is_none() && model == "picked-model"), "{rows:?}");
        assert!(rows.iter().any(|(thread, model)| thread.is_some() && model == "big-model"), "{rows:?}");
        assert!(
            rows.iter().all(|(thread, model)| {
                if thread.is_none() { model == "picked-model" } else { model == "big-model" }
            }),
            "{rows:?}"
        );

        let error = hub
            .chat(ChatRequest {
                conversation_id: Some(conv.clone()),
                mode: None,
                text: "go now again".into(),
                model: Some("big".into()),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("first message"), "{error}");

        assert!(hub.db.set_conversation_model(&conv, None).unwrap().model.is_none());
        hub.chat(ChatRequest {
            conversation_id: Some(conv.clone()),
            mode: None,
            text: "use the default".into(),
            model: None,
        })
        .await
        .unwrap();
        until(&mut rx, |event| {
            matches!(&event.body, EventBody::AssistantMessage { text } if text == "Default model speaking.")
        })
        .await;
        assert_eq!(usage(&hub, &conv).last().map(|row| row.1.as_str()), Some("big-model"));

        let standard = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("standard".into()),
                text: "standard hello".into(),
                model: None,
            })
            .await
            .unwrap();
        until(&mut rx, |event| {
            event.conversation_id == standard
                && matches!(&event.body, EventBody::AssistantMessage { text } if text == "Standard reply.")
        })
        .await;
        assert!(hub.db.conversation(&standard).unwrap().model.is_none());
        assert_eq!(usage(&hub, &standard)[0].1, "big-model");

        let overridden = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("standard".into()),
                text: "standard hello".into(),
                model: Some("picked".into()),
            })
            .await
            .unwrap();
        until(&mut rx, |event| {
            event.conversation_id == overridden
                && matches!(&event.body, EventBody::AssistantMessage { text } if text == "Standard reply.")
        })
        .await;
        assert_eq!(hub.db.conversation(&overridden).unwrap().model.as_deref(), Some("picked"));
        assert_eq!(usage(&hub, &overridden)[0].1, "picked-model");
        let error = hub
            .chat(ChatRequest {
                conversation_id: Some(overridden),
                mode: None,
                text: "standard hello again".into(),
                model: Some("big".into()),
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("first message"), "{error}");

        hub.db.set_conversation_model(&conv, Some("picked")).unwrap();
        hub.db.delete_model("picked").unwrap();
        hub.chat(ChatRequest {
            conversation_id: Some(conv.clone()),
            mode: None,
            text: "after the model was removed".into(),
            model: None,
        })
        .await
        .unwrap();
        until(&mut rx, |event| {
            matches!(&event.body, EventBody::Error { message } if message.contains("not in the registry"))
        })
        .await;

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn replaced_connection_closes_old_and_ignores_stale_thread_frames() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let dir = std::env::temp_dir().join(format!("grouper-replace-test-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (device, token) = db.add_device("Laptop", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let ws_connect = |token: String| async move {
            let mut req = format!("ws://{addr}{}", core_protocol::CONNECT_PATH).into_client_request().unwrap();
            req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            let hello = NodeMessage::Hello { protocol_version: PROTOCOL_VERSION, client_version: "test".into(), host: None };
            ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap().into())).await.unwrap();
            ws
        };

        let mut ws1 = ws_connect(token.clone()).await;
        // Wait for Welcome on ws1.
        let mut welcomed = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(WsMsg::Text(t))) = ws1.next().await {
                if matches!(serde_json::from_str::<HubMessage>(&t), Ok(HubMessage::Welcome { .. })) {
                    welcomed = true;
                    break;
                }
            }
        })
        .await
        .expect("ws1 welcomed");
        assert!(welcomed);
        let conn1 = hub.live.lock().await.get(&device.id).unwrap().conn_id;

        // Create a running thread on this device before ws2 connects.
        let conv = hub.db.create_conversation("project", "test").unwrap();
        let thread = hub.db.create_thread(&conv, "T", &device.id, "m", None, "brief").unwrap();

        // Connect ws2 on the same token.
        let mut ws2 = ws_connect(token.clone()).await;

        // ws1 must receive the replaced Error frame and then close.
        let mut saw_replaced_error = false;
        let mut saw_close = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match ws1.next().await {
                    Some(Ok(WsMsg::Text(t))) => {
                        if let Ok(HubMessage::Error { message }) = serde_json::from_str::<HubMessage>(&t)
                            && message.contains("replaced by a newer connection")
                        {
                            saw_replaced_error = true;
                        }
                    }
                    Some(Ok(WsMsg::Close(_))) | None => {
                        saw_close = true;
                        break;
                    }
                    Some(Err(_)) => {
                        saw_close = true;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("ws1 closed after replacement");
        assert!(saw_replaced_error, "ws1 must receive replaced error frame");
        assert!(saw_close, "ws1 must close");

        // ws2 receives Welcome and owns the live entry with a newer conn_id.
        let mut ws2_welcomed = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(WsMsg::Text(t))) = ws2.next().await {
                if matches!(serde_json::from_str::<HubMessage>(&t), Ok(HubMessage::Welcome { .. })) {
                    ws2_welcomed = true;
                    break;
                }
            }
        })
        .await
        .expect("ws2 welcomed");
        assert!(ws2_welcomed);
        let conn2 = hub.live.lock().await.get(&device.id).unwrap().conn_id;
        assert_ne!(conn1, conn2);

        // Orphan thread from conn1 becomes waiting_device.
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.db.thread(&thread.id).unwrap().status != crate::offline::WAITING_DEVICE {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("orphan marked waiting_device");

        // Thread frames from the stale conn_id (conn1) are ignored, while conn2's work.
        let fresh_thread = hub.db.create_thread(&conv, "T2", &device.id, "m", None, "brief").unwrap();
        let before_events = hub.db.events(&conv, 0, Some(&fresh_thread.id)).len();
        handle_thread_frame(
            &hub,
            &device.id,
            conn1,
            NodeMessage::ThreadEvent { thread_id: fresh_thread.id.clone(), event: AgentEvent::Text { text: "stale".into() } },
        )
        .await;
        handle_thread_frame(
            &hub,
            &device.id,
            conn1,
            NodeMessage::ApprovalRequest {
                approval_id: "stale-a1".into(),
                thread_id: fresh_thread.id.clone(),
                tool: "run_shell".into(),
                input: json!({}),
                reason: "stale".into(),
            },
        )
        .await;
        handle_thread_frame(
            &hub,
            &device.id,
            conn1,
            NodeMessage::AskOrchestrator { thread_id: fresh_thread.id.clone(), question: "stale?".into() },
        )
        .await;
        handle_thread_frame(
            &hub,
            &device.id,
            conn1,
            NodeMessage::ThreadFinished { thread_id: fresh_thread.id.clone(), ok: false, summary: "stale".into() },
        )
        .await;
        assert_eq!(hub.db.events(&conv, 0, Some(&fresh_thread.id)).len(), before_events);
        assert_eq!(hub.db.thread(&fresh_thread.id).unwrap().status, "running");
        assert!(hub.approvals.lock().await.is_empty());

        // Same ThreadEvent from the live conn2 is accepted.
        handle_thread_frame(
            &hub,
            &device.id,
            conn2,
            NodeMessage::ThreadEvent { thread_id: fresh_thread.id.clone(), event: AgentEvent::Text { text: "live".into() } },
        )
        .await;
        assert_eq!(hub.db.events(&conv, 0, Some(&fresh_thread.id)).len(), before_events + 1);

        let _ = ws2.close(None).await;
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn slow_peer_queue_overflow_drops_connection_without_blocking() {
        let dir = std::env::temp_dir().join(format!("grouper-overflow-test-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (device, _) = db.add_device("Slow", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());

        let (tx, _rx) = mpsc::channel(2);
        let (close, mut close_rx) = CloseHandle::new();
        let (_closed_tx, closed_rx) = watch::channel(false);
        hub.live.lock().await.insert(
            device.id.clone(),
            Live { conn_id: 1, tx, tools: Vec::new(), mcp: None, usage_probes: false, close, closed: closed_rx },
        );

        assert!(hub.send_to(&device.id, HubMessage::Pong).await);
        assert!(hub.send_to(&device.id, HubMessage::Pong).await);
        // Third message overflows the bounded queue: returns false immediately, evicts peer, signals Overflow.
        assert!(!hub.send_to(&device.id, HubMessage::Pong).await);
        assert!(!hub.is_online(&device.id).await);
        assert_eq!(close_rx.try_recv().ok(), Some(CloseReason::Overflow));

        // Same for broadcast: slow peer dropped, healthy peer keeps receiving.
        let (other, _) = hub.db.add_device("Fast", "linux", false).unwrap();
        let (slow_tx, _slow_rx) = mpsc::channel(1);
        let (slow_close, mut slow_close_rx) = CloseHandle::new();
        let (_s_tx, s_rx) = watch::channel(false);
        let (fast_tx, mut fast_rx) = mpsc::channel(4);
        let (fast_close, _fast_close_rx) = CloseHandle::new();
        let (_f_tx, f_rx) = watch::channel(false);
        {
            let mut live = hub.live.lock().await;
            live.insert(
                device.id.clone(),
                Live { conn_id: 2, tx: slow_tx, tools: Vec::new(), mcp: None, usage_probes: false, close: slow_close, closed: s_rx },
            );
            live.insert(
                other.id.clone(),
                Live { conn_id: 3, tx: fast_tx, tools: Vec::new(), mcp: None, usage_probes: false, close: fast_close, closed: f_rx },
            );
        }
        hub.broadcast(HubMessage::Pong).await;
        hub.broadcast(HubMessage::Pong).await;
        assert!(!hub.is_online(&device.id).await);
        assert!(hub.is_online(&other.id).await);
        assert_eq!(slow_close_rx.try_recv().ok(), Some(CloseReason::Overflow));
        assert!(fast_rx.try_recv().is_ok());
        assert!(fast_rx.try_recv().is_ok());

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_denied_call_is_refused_in_other_threads_of_the_chat_until_the_user_speaks() {
        let dir =
            std::env::temp_dir().join(format!("grouper-denied-chat-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (worker, _) = db.add_device("Laptop", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let c1 = hub.db.create_conversation("project", "chat").unwrap();
        let c2 = hub.db.create_conversation("project", "other").unwrap();
        let t1 = hub
            .db
            .create_thread(&c1, "T1", &worker.id, "m", None, "b")
            .unwrap();
        let t2 = hub
            .db
            .create_thread(&c1, "T2", &worker.id, "m", None, "b")
            .unwrap();
        let t3 = hub
            .db
            .create_thread(&c2, "T3", &worker.id, "m", None, "b")
            .unwrap();
        let rm = json!({"command": "rm important.txt"});

        // The user denies the call in T1.
        let (tx, _rx) = oneshot::channel();
        let a1 = format!("{}-1", t1.id);
        assert!(
            hub.request_approval(
                &worker.id,
                &t1.id,
                &a1,
                "run_shell",
                rm.clone(),
                "shell",
                Some(tx)
            )
            .await
        );
        assert!(hub.resolve_approval(&a1, false).await);

        // A new thread in the same chat asks for the same call: refused, the user isn't asked.
        let (tx, rx) = oneshot::channel();
        let a2 = format!("{}-1", t2.id);
        assert!(
            hub.request_approval(
                &worker.id,
                &t2.id,
                &a2,
                "run_shell",
                rm.clone(),
                "shell",
                Some(tx)
            )
            .await
        );
        assert!(matches!(rx.await, Ok(ApprovalAnswer::Deny)));
        assert!(hub.pending_approvals(None).await.is_empty());
        // From a device (no local channel) it's refused too; the caller replies "denied".
        assert!(
            !hub.request_approval(
                &worker.id,
                &t2.id,
                &format!("{}-2", t2.id),
                "run_shell",
                rm.clone(),
                "shell",
                None
            )
            .await
        );

        // A different call, or the same call in another chat, still asks.
        let (tx, _rx) = oneshot::channel();
        let other = json!({"command": "ls"});
        assert!(
            hub.request_approval(
                &worker.id,
                &t2.id,
                &format!("{}-3", t2.id),
                "run_shell",
                other,
                "shell",
                Some(tx)
            )
            .await
        );
        let (tx, _rx) = oneshot::channel();
        assert!(
            hub.request_approval(
                &worker.id,
                &t3.id,
                &format!("{}-1", t3.id),
                "run_shell",
                rm.clone(),
                "shell",
                Some(tx)
            )
            .await
        );
        assert_eq!(hub.pending_approvals(None).await.len(), 2);

        // Once the user speaks in the chat, the call is asked again.
        hub.forget_denials(&c1);
        let (tx, _rx) = oneshot::channel();
        assert!(
            hub.request_approval(
                &worker.id,
                &t2.id,
                &format!("{}-4", t2.id),
                "run_shell",
                rm,
                "shell",
                Some(tx)
            )
            .await
        );
        assert_eq!(hub.pending_approvals(Some(&c1)).await.len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn denying_a_call_also_denies_identical_pending_requests_in_the_chat() {
        let dir = std::env::temp_dir().join(format!("grouper-denied-twins-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (worker, _) = db.add_device("Laptop", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let (tx, mut device_rx) = mpsc::channel(8);
        let (close, _close_rx) = CloseHandle::new();
        let (_closed_tx, closed_rx) = watch::channel(false);
        hub.live.lock().await.insert(
            worker.id.clone(),
            Live { conn_id: 1, tx, tools: Vec::new(), mcp: None, usage_probes: false, close, closed: closed_rx },
        );
        let c1 = hub.db.create_conversation("project", "chat").unwrap();
        let c2 = hub.db.create_conversation("project", "other").unwrap();
        let t1 = hub.db.create_thread(&c1, "T1", &worker.id, "m", None, "b").unwrap();
        let t2 = hub.db.create_thread(&c1, "T2", &worker.id, "m", None, "b").unwrap();
        let t3 = hub.db.create_thread(&c2, "T3", &worker.id, "m", None, "b").unwrap();
        let rm = json!({"command": "rm important.txt"});

        // One model turn asked for the same call twice at once (a hub-local wait and a
        // device request), another thread asked too; plus a different call, and the
        // same call in another chat.
        let ask = |thread: &str, n: u32, input: &Value, local: bool| {
            let hub = hub.clone();
            let (worker, thread, input) = (worker.id.clone(), thread.to_string(), input.clone());
            async move {
                let (tx, rx) = oneshot::channel();
                let id = format!("{thread}-{n}");
                let tx = local.then_some(tx);
                assert!(hub.request_approval(&worker, &thread, &id, "run_shell", input, "shell", tx).await);
                (id, rx)
            }
        };
        let (a1, mut r1) = ask(&t1.id, 1, &rm, true).await;
        let (a2, mut r2) = ask(&t1.id, 2, &rm, true).await;
        let (a3, _) = ask(&t1.id, 3, &rm, false).await;
        let (a4, mut r4) = ask(&t2.id, 1, &rm, true).await;
        let (a5, mut r5) = ask(&t1.id, 4, &json!({"command": "ls"}), true).await;
        let (a6, mut r6) = ask(&t3.id, 1, &rm, true).await;
        assert_eq!(hub.pending_approvals(None).await.len(), 6);

        assert!(hub.resolve_approval(&a1, false).await);
        assert_eq!(r1.try_recv().ok(), Some(ApprovalAnswer::Deny));
        assert_eq!(r2.try_recv().ok(), Some(ApprovalAnswer::Deny));
        assert_eq!(r4.try_recv().ok(), Some(ApprovalAnswer::Deny));
        match device_rx.try_recv() {
            Ok(HubMessage::ApprovalResult { approval_id, approved: false, timeout_secs: None }) => {
                assert_eq!(approval_id, a3)
            }
            other => panic!("the device wasn't told: {other:?}"),
        }
        assert!(device_rx.try_recv().is_err());
        // The different call and the other chat's request still wait.
        assert!(r5.try_recv().is_err() && r6.try_recv().is_err());
        let left: Vec<String> = hub.pending_approvals(None).await.into_iter().map(|p| p.approval_id).collect();
        assert_eq!(left.len(), 2);
        assert!(left.contains(&a5) && left.contains(&a6), "{left:?}");
        let mut resolved: Vec<String> = hub
            .db
            .events(&c1, 0, None)
            .into_iter()
            .filter_map(|e| match e.body {
                EventBody::ApprovalResolved { approval_id, approved: false, .. } => Some(approval_id),
                _ => None,
            })
            .collect();
        resolved.sort();
        let mut want = vec![a1, a2, a3, a4];
        want.sort();
        assert_eq!(resolved, want);
        // Approving doesn't touch the others.
        assert!(hub.resolve_approval(&a5, true).await);
        assert!(r6.try_recv().is_err());
        assert_eq!(hub.pending_approvals(None).await.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn pending_approvals_endpoint_lists_filters_and_requires_admin() {
        let dir = std::env::temp_dir().join(format!("grouper-approvals-test-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Admin", "ios", true).unwrap();
        let (worker, worker_token) = db.add_device("Laptop", "windows", false).unwrap();
        db.set_limits(&core_protocol::Limits {
            approval_timeout_secs: Some(90),
            approval_timeout_action: core_protocol::ApprovalTimeoutAction::Stop,
            ..Default::default()
        })
        .unwrap();
        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let base = format!("http://{addr}");
        let http = reqwest::Client::new();

        // Auth checks: 401 unauth, 403 non-admin.
        assert_eq!(http.get(format!("{base}/v1/approvals")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            http.get(format!("{base}/v1/approvals")).bearer_auth(&worker_token).send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let c1 = hub.db.create_conversation("project", "first").unwrap();
        let c2 = hub.db.create_conversation("project", "second").unwrap();
        let t1 = hub.db.create_thread(&c1, "T1", &worker.id, "m", None, "b").unwrap();
        let t2 = hub.db.create_thread(&c2, "T2", &worker.id, "m", None, "b").unwrap();
        let a1 = format!("{}-1", t1.id);
        let a2 = format!("{}-1", t2.id);
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        assert!(
            hub.request_approval(&worker.id, &t1.id, &a1, "run_shell", json!({"command": "ls"}), "shell rule", Some(tx1))
                .await
        );
        assert!(
            hub.request_approval(&worker.id, &t2.id, &a2, "write_file", json!({"path": "a.txt"}), "manual", Some(tx2))
                .await
        );

        let all: Vec<PendingApproval> = http
            .get(format!("{base}/v1/approvals"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        let first = all.iter().find(|a| a.approval_id == a1).unwrap();
        assert_eq!(first.device_id, worker.id);
        assert_eq!(first.thread_id, t1.id);
        assert_eq!(first.conversation_id, c1);
        assert_eq!(first.tool, "run_shell");
        assert_eq!(first.input, json!({"command": "ls"}));
        assert_eq!(first.reason, "shell rule");
        assert!(first.requested_at > 0);
        assert_eq!(first.timeout_secs, Some(90));
        assert_eq!(first.timeout_at, Some(first.requested_at + 90));
        assert_eq!(first.timeout_action, Some(core_protocol::ApprovalTimeoutAction::Stop));

        let filtered: Vec<PendingApproval> = http
            .get(format!("{base}/v1/approvals?conversation={c2}"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].approval_id, a2);

        // Resolving removes it from the list.
        assert!(hub.resolve_approval(&a1, true).await);
        let remaining: Vec<PendingApproval> = http
            .get(format!("{base}/v1/approvals"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].approval_id, a2);

        // Turning off timeout clears timeout fields on pending approvals.
        hub.db.set_limits(&core_protocol::Limits { approval_timeout_secs: None, ..Default::default() }).unwrap();
        let after_off: Vec<PendingApproval> = http
            .get(format!("{base}/v1/approvals"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(after_off[0].timeout_secs, None);
        assert_eq!(after_off[0].timeout_at, None);

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn device_presence_events_are_live_only_and_not_stored() {
        use tokio_tungstenite::tungstenite::Message as WsMsg;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let dir = std::env::temp_dir().join(format!("grouper-presence-event-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Admin", "ios", true).unwrap();
        let (worker, worker_token) = db.add_device("Workstation", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });

        // Subscribe to /v1/events with Bearer token.
        let mut ev_req = format!("ws://{addr}/v1/events").into_client_request().unwrap();
        ev_req.headers_mut().insert("authorization", format!("Bearer {admin_token}").parse().unwrap());
        let (mut ev_ws, _) = tokio_tungstenite::connect_async(ev_req).await.unwrap();

        // Connect worker device.
        let mut dev_req = format!("ws://{addr}{}", core_protocol::CONNECT_PATH).into_client_request().unwrap();
        dev_req.headers_mut().insert("authorization", format!("Bearer {worker_token}").parse().unwrap());
        let (mut dev_ws, _) = tokio_tungstenite::connect_async(dev_req).await.unwrap();
        let hello = NodeMessage::Hello { protocol_version: PROTOCOL_VERSION, client_version: "test".into(), host: None };
        dev_ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap().into())).await.unwrap();

        let online_ev: Event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Ok(WsMsg::Text(t))) = ev_ws.next().await
                    && let Ok(e) = serde_json::from_str::<Event>(&t)
                    && matches!(&e.body, EventBody::DevicePresence { device_id, .. } if *device_id == worker.id)
                {
                    break e;
                }
            }
        })
        .await
        .expect("online presence event");
        assert_eq!(online_ev.seq, 0);
        match online_ev.body {
            EventBody::DevicePresence { device_id, online, device } => {
                assert_eq!(device_id, worker.id);
                assert!(online);
                assert_eq!(device.unwrap().name, "Workstation");
            }
            other => panic!("unexpected event {other:?}"),
        }

        // Simulate tool list change broadcast.
        let conn_id = hub.live.lock().await.get(&worker.id).unwrap().conn_id;
        assert!(
            crate::mcp::apply_tool_list(
                &hub,
                &worker.id,
                conn_id,
                vec![ToolDef { name: "echo".into(), description: "Echo".into(), input_schema: json!({}) }],
            )
            .await
        );
        hub.broadcast_presence(&worker.id).await;
        let tools_ev: Event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Ok(WsMsg::Text(t))) = ev_ws.next().await
                    && let Ok(e) = serde_json::from_str::<Event>(&t)
                    && matches!(&e.body, EventBody::DevicePresence { device_id, .. } if *device_id == worker.id)
                {
                    break e;
                }
            }
        })
        .await
        .expect("tool update presence event");
        assert_eq!(tools_ev.seq, 0);
        match tools_ev.body {
            EventBody::DevicePresence { online, device, .. } => {
                assert!(online);
                assert_eq!(device.unwrap().capabilities.len(), 1);
            }
            other => panic!("unexpected event {other:?}"),
        }

        // Disconnect worker device -> offline presence event.
        let _ = dev_ws.close(None).await;
        drop(dev_ws);
        let offline_ev: Event = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Ok(WsMsg::Text(t))) = ev_ws.next().await
                    && let Ok(e) = serde_json::from_str::<Event>(&t)
                    && matches!(&e.body, EventBody::DevicePresence { device_id, online: false, .. } if *device_id == worker.id)
                {
                    break e;
                }
            }
        })
        .await
        .expect("offline presence event");
        assert_eq!(offline_ev.seq, 0);

        // Presence events are live-only and never stored in SQLite.
        let stored_count: i64 = hub.db.c().query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0)).unwrap();
        assert_eq!(stored_count, 0, "device_presence must never be stored in events");

        let _ = ev_ws.close(None).await;
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn thread_endpoints_include_current_user_rating_and_providers_include_options() {
        let dir = std::env::temp_dir().join(format!("grouper-rating-provider-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Admin", "ios", true).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p1".into(),
            kind: "openai".into(),
            base_url: Some("https://example.com/v1".into()),
            api_key: Some("super-secret-key".into()),
            options: json!({"max_tokens_field": "max_completion_tokens"}),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "m1".into(),
            provider_id: "p1".into(),
            model: "gpt-test".into(),
            input_price: 1.0,
            output_price: 2.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 128000,
            use_cases: String::new(),
            free: false,
            roles: vec!["thread".into()],
            local: false,
            modalities: vec![],
            ..Default::default()
        })
        .unwrap();
        let conv = db.create_conversation("project", "rating test").unwrap();
        let t = db.create_thread(&conv, "Rate me", HUB_DEVICE, "m1", None, "brief").unwrap();
        assert!(db.finish_thread(&t.id, "done", "all good"));
        assert!(db.record_outcome(&t.id));

        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let base = format!("http://{addr}");
        let http = reqwest::Client::new();

        // Provider listing includes non-secret options, never the key.
        let raw_providers = http
            .get(format!("{base}/v1/providers"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!raw_providers.contains("super-secret-key"));
        let providers: Vec<ProviderInfo> = serde_json::from_str(&raw_providers).unwrap();
        assert_eq!(providers.len(), 1);
        assert!(providers[0].has_key);
        assert_eq!(providers[0].options, json!({"max_tokens_field": "max_completion_tokens"}));

        // Thread starts unrated.
        let got: core_protocol::ThreadInfo = http
            .get(format!("{base}/v1/threads/{}", t.id))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got.rating, None);

        // Rate up -> GET /v1/threads/{id} and GET /v1/threads both return rating = Some(1).
        let r = http
            .post(format!("{base}/v1/threads/{}/rating", t.id))
            .bearer_auth(&admin_token)
            .json(&json!({"rating": "up"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let got: core_protocol::ThreadInfo = http
            .get(format!("{base}/v1/threads/{}", t.id))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got.rating, Some(1));
        let list: Vec<core_protocol::ThreadInfo> = http
            .get(format!("{base}/v1/threads?conversation={conv}"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list[0].rating, Some(1));

        // Rate down -> -1; clear -> None.
        http.post(format!("{base}/v1/threads/{}/rating", t.id))
            .bearer_auth(&admin_token)
            .json(&json!({"rating": "down"}))
            .send()
            .await
            .unwrap();
        let got: core_protocol::ThreadInfo = http
            .get(format!("{base}/v1/threads/{}", t.id))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got.rating, Some(-1));

        http.post(format!("{base}/v1/threads/{}/rating", t.id))
            .bearer_auth(&admin_token)
            .json(&json!({"rating": "clear"}))
            .send()
            .await
            .unwrap();
        let got: core_protocol::ThreadInfo = http
            .get(format!("{base}/v1/threads/{}", t.id))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got.rating, None);

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn device_approval_gets_random_hub_id_and_replies_with_device_id() {
        let dir = std::env::temp_dir().join(format!("grouper-dev-approval-id-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (worker, _) = db.add_device("Laptop", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let (tx, mut dev_rx) = mpsc::channel(8);
        let (close, _close_rx) = CloseHandle::new();
        let (_closed_tx, closed_rx) = watch::channel(false);
        hub.live.lock().await.insert(
            worker.id.clone(),
            Live { conn_id: 7, tx, tools: Vec::new(), mcp: None, usage_probes: false, close, closed: closed_rx },
        );
        let conv = hub.db.create_conversation("project", "dev-approval").unwrap();
        let thread = hub.db.create_thread(&conv, "T", &worker.id, "m", None, "b").unwrap();
        let device_id = format!("{}-a1", thread.id);

        handle_thread_frame(
            &hub,
            &worker.id,
            7,
            NodeMessage::ApprovalRequest {
                approval_id: device_id.clone(),
                thread_id: thread.id.clone(),
                tool: "run_shell".into(),
                input: json!({"command": "echo hi"}),
                reason: "manual".into(),
            },
        )
        .await;

        let pending = hub.pending_approvals(Some(&conv)).await;
        assert_eq!(pending.len(), 1);
        let hub_id = pending[0].approval_id.clone();
        assert_ne!(hub_id, device_id);
        assert!(hub_id.starts_with("a_") && hub_id.len() == 26, "{hub_id}");

        // Duplicate device_approval_id while pending is rejected with an immediate denial.
        handle_thread_frame(
            &hub,
            &worker.id,
            7,
            NodeMessage::ApprovalRequest {
                approval_id: device_id.clone(),
                thread_id: thread.id.clone(),
                tool: "run_shell".into(),
                input: json!({"command": "echo hi"}),
                reason: "manual".into(),
            },
        )
        .await;
        match dev_rx.try_recv() {
            Ok(HubMessage::ApprovalResult { approval_id, approved: false, timeout_secs: None }) => {
                assert_eq!(approval_id, device_id);
            }
            other => panic!("expected duplicate rejection, got {other:?}"),
        }
        assert_eq!(hub.pending_approvals(Some(&conv)).await.len(), 1);

        // Resolving by hub_id sends the device's original approval_id back to the device.
        assert!(hub.resolve_approval(&hub_id, true).await);
        match dev_rx.try_recv() {
            Ok(HubMessage::ApprovalResult { approval_id, approved: true, timeout_secs: None }) => {
                assert_eq!(approval_id, device_id);
            }
            other => panic!("expected approval reply with device id, got {other:?}"),
        }

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn device_disconnect_emits_live_only_dropped_approval_event() {
        let dir = std::env::temp_dir().join(format!("grouper-drop-event-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (worker, _) = db.add_device("Laptop", "windows", false).unwrap();
        let hub = Hub::new(db, dir.clone());
        let conv = hub.db.create_conversation("project", "drop").unwrap();
        let thread = hub.db.create_thread(&conv, "T", &worker.id, "m", None, "b").unwrap();
        let (tx, _rx) = oneshot::channel();
        let approval_id = Hub::new_approval_id();
        assert!(
            hub.request_approval(&worker.id, &thread.id, &approval_id, "run_shell", json!({}), "manual", Some(tx)).await
        );

        let mut ev_rx = hub.events.subscribe();
        hub.device_gone(&worker.id).await;

        let ev = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let e = ev_rx.recv().await.unwrap();
                if matches!(
                    &e.body,
                    EventBody::ApprovalResolved { approval_id: id, approved: false, reason }
                        if id == &approval_id && reason.as_deref() == Some("dropped")
                ) {
                    break e;
                }
            }
        })
        .await
        .expect("live dropped event on device disconnect");
        assert_eq!(ev.seq, 0);
        assert_eq!(ev.conversation_id, conv);
        assert_eq!(ev.thread_id.as_deref(), Some(thread.id.as_str()));
        // Live-only: not stored in SQLite events.
        assert!(
            hub.db.events(&conv, 0, None).iter().all(|e| !matches!(e.body, EventBody::ApprovalResolved { .. })),
            "dropped approval event must be live-only"
        );

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn models_endpoint_round_trips_kind_device_id_and_hidden_filtering() {
        let dir = std::env::temp_dir().join(format!("grouper-models-kind-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Admin", "macos", true).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();

        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let base = format!("http://{addr}");
        let http = reqwest::Client::new();

        // Register an API provider and a harness provider via HTTP.
        let r = http
            .put(format!("{base}/v1/providers/openai"))
            .bearer_auth(&admin_token)
            .json(&json!({"id": "openai", "kind": "openai", "base_url": "https://api.openai.com/v1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let harness_provider_id = format!("claude-code@{}", pc.id);
        let r = http
            .put(format!("{base}/v1/providers/{harness_provider_id}"))
            .bearer_auth(&admin_token)
            .json(&json!({
                "id": harness_provider_id,
                "kind": "harness",
                "options": {"harness": "claude-code", "via_device": "Gaming PC", "signed_in": true}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // PUT an API model omitting new fields: defaults to kind="api", no device_id, existing fields preserved.
        let api_saved: Value = http
            .put(format!("{base}/v1/models/gpt-mini"))
            .bearer_auth(&admin_token)
            .json(&json!({
                "id": "gpt-mini",
                "provider_id": "openai",
                "model": "gpt-4o-mini",
                "input_price": 0.15,
                "output_price": 0.60,
                "context_window": 128000,
                "use_cases": "quick answers",
                "free": false,
                "roles": ["standard"],
                "local": false,
                "modalities": ["text"]
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(api_saved["kind"], "api");
        assert!(api_saved.get("device_id").is_none());
        assert_eq!(api_saved["source"], "manual");
        assert_eq!(api_saved["price_source"], "manual");
        assert_eq!(api_saved["tools"], "unknown");
        assert_eq!(api_saved["hidden"], false);

        // PUT a harness model under the harness provider: kind="harness" and device_id resolved to pc.id.
        let harness_saved: ModelEntry = http
            .put(format!("{base}/v1/models/cc-opus"))
            .bearer_auth(&admin_token)
            .json(&json!({
                "id": "cc-opus",
                "provider_id": harness_provider_id,
                "model": "opus",
                "source": "auto",
                "display_name": "Claude Code · opus",
                "price_source": "none",
                "tools": "yes"
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(harness_saved.kind, "harness");
        assert_eq!(harness_saved.device_id.as_deref(), Some(pc.id.as_str()));
        assert_eq!(harness_saved.source, "auto");
        assert_eq!(harness_saved.display_name.as_deref(), Some("Claude Code · opus"));
        assert_eq!(harness_saved.price_source, "none");
        assert_eq!(harness_saved.tools, "yes");

        // GET /v1/models returns both with kind, device_id, and every legacy JSON field intact.
        let raw_list: Vec<Value> = http
            .get(format!("{base}/v1/models"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(raw_list.len(), 2);
        let api_row = raw_list.iter().find(|v| v["id"] == "gpt-mini").unwrap();
        for key in [
            "id",
            "provider_id",
            "model",
            "input_price",
            "output_price",
            "cache_read_price",
            "cache_write_price",
            "context_window",
            "use_cases",
            "free",
            "roles",
            "local",
            "modalities",
            "kind",
            "source",
            "hidden",
            "price_source",
            "tools",
        ] {
            assert!(api_row.get(key).is_some(), "missing expected key {key:?} on {api_row}");
        }
        assert_eq!(api_row["kind"], "api");

        let list: Vec<ModelEntry> = serde_json::from_value(Value::Array(raw_list)).unwrap();
        let harness_row = list.iter().find(|m| m.id == "cc-opus").unwrap();
        assert_eq!(harness_row.kind, "harness");
        assert_eq!(harness_row.device_id.as_deref(), Some(pc.id.as_str()));

        // Hiding a model omits it from GET /v1/models, while ?include_hidden=true returns it.
        let hidden_resp = http
            .put(format!("{base}/v1/models/cc-opus"))
            .bearer_auth(&admin_token)
            .json(&ModelEntry {
                hidden: true,
                hidden_reason: Some("user".into()),
                ..harness_row.clone()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(hidden_resp.status(), StatusCode::OK);

        let visible: Vec<ModelEntry> = http
            .get(format!("{base}/v1/models"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(visible.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["gpt-mini"]);

        let all: Vec<ModelEntry> = http
            .get(format!("{base}/v1/models?include_hidden=true"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        let hidden_row = all.iter().find(|m| m.id == "cc-opus").unwrap();
        assert!(hidden_row.hidden);
        assert_eq!(hidden_row.hidden_reason.as_deref(), Some("user"));
        assert_eq!(hidden_row.kind, "harness");
        assert_eq!(hidden_row.device_id.as_deref(), Some(pc.id.as_str()));

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn direct_api_calls_enforce_harness_orchestration_rules() {
        let dir = std::env::temp_dir().join(format!("grouper-h2-api-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Admin", "macos", true).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();

        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let base = format!("http://{addr}");
        let http = reqwest::Client::new();

        // Register an API provider and two harness providers (one non-orchestrating, one orchestrating).
        assert_eq!(
            http.put(format!("{base}/v1/providers/openai"))
                .bearer_auth(&admin_token)
                .json(&json!({"id": "openai", "kind": "openai", "base_url": "https://api.openai.com/v1"}))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let codex_pid = format!("codex@{}", pc.id);
        assert_eq!(
            http.put(format!("{base}/v1/providers/{codex_pid}"))
                .bearer_auth(&admin_token)
                .json(&json!({
                    "id": codex_pid,
                    "kind": "harness",
                    "options": {"harness": "codex", "via_device": pc.id, "signed_in": true}
                }))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let claude_pid = format!("claude-code@{}", pc.id);
        assert_eq!(
            http.put(format!("{base}/v1/providers/{claude_pid}"))
                .bearer_auth(&admin_token)
                .json(&json!({
                    "id": claude_pid,
                    "kind": "harness",
                    "options": {"harness": "claude-code", "via_device": pc.id, "signed_in": true}
                }))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        // Register an API orchestrator model.
        assert_eq!(
            http.put(format!("{base}/v1/models/api-orch"))
                .bearer_auth(&admin_token)
                .json(&json!({
                    "id": "api-orch",
                    "provider_id": "openai",
                    "model": "gpt-4o",
                    "roles": ["orchestrator"]
                }))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        // Direct API bypass: PUT /v1/models/{id} with roles: ["orchestrator"] on a harness without can_orchestrate -> 400.
        let r = http
            .put(format!("{base}/v1/models/codex-o3"))
            .bearer_auth(&admin_token)
            .json(&json!({
                "id": "codex-o3",
                "provider_id": codex_pid,
                "model": "o3",
                "can_orchestrate": false,
                "roles": ["orchestrator"]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["code"], "harness_cannot_orchestrate");
        assert_eq!(
            body["error"],
            "codex-o3 is a harness and can't orchestrate a project chat; pick it in a standard chat or for threads"
        );

        // Same harness MAY be saved with standard + thread roles.
        assert_eq!(
            http.put(format!("{base}/v1/models/codex-o3"))
                .bearer_auth(&admin_token)
                .json(&json!({
                    "id": "codex-o3",
                    "provider_id": codex_pid,
                    "model": "o3",
                    "can_orchestrate": false,
                    "roles": ["standard", "thread"]
                }))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        // Harness WITH can_orchestrate = true MAY hold the orchestrator role.
        assert_eq!(
            http.put(format!("{base}/v1/models/cc-opus"))
                .bearer_auth(&admin_token)
                .json(&json!({
                    "id": "cc-opus",
                    "provider_id": claude_pid,
                    "model": "opus",
                    "can_orchestrate": true,
                    "roles": ["orchestrator", "standard", "thread"]
                }))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );

        // Create a project chat and a standard chat.
        let project_conv = hub.db.create_conversation("project", "Project").unwrap();
        let standard_conv = hub.db.create_conversation("standard", "Standard").unwrap();

        // Direct API bypass: POST /v1/conversations/{id}/model on a project chat with non-orchestrating harness -> 400.
        let r = http
            .post(format!("{base}/v1/conversations/{project_conv}/model"))
            .bearer_auth(&admin_token)
            .json(&json!({"model": "codex-o3"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["code"], "harness_cannot_orchestrate");
        assert_eq!(
            body["error"],
            "codex-o3 is a harness and can't orchestrate a project chat; pick it in a standard chat or for threads"
        );
        assert!(hub.db.conversation(&project_conv).unwrap().model.is_none());

        // Project chat accepts can_orchestrate = true harness via HTTP.
        let r = http
            .post(format!("{base}/v1/conversations/{project_conv}/model"))
            .bearer_auth(&admin_token)
            .json(&json!({"model": "cc-opus"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(hub.db.conversation(&project_conv).unwrap().model.as_deref(), Some("cc-opus"));

        // Standard chat accepts non-orchestrating harness via HTTP.
        let r = http
            .post(format!("{base}/v1/conversations/{standard_conv}/model"))
            .bearer_auth(&admin_token)
            .json(&json!({"model": "codex-o3"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(hub.db.conversation(&standard_conv).unwrap().model.as_deref(), Some("codex-o3"));

        // Direct API bypass: POST /v1/chat starting a new project chat with non-orchestrating harness -> 400
        // and does NOT create an orphan conversation row.
        let before_convs = hub.db.conversations().len();
        let r = http
            .post(format!("{base}/v1/chat"))
            .bearer_auth(&admin_token)
            .json(&json!({"text": "plan a task", "mode": "project", "model": "codex-o3"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["code"], "harness_cannot_orchestrate");
        assert_eq!(
            body["error"],
            "codex-o3 is a harness and can't orchestrate a project chat; pick it in a standard chat or for threads"
        );
        assert_eq!(hub.db.conversations().len(), before_convs, "rejected /v1/chat must not leave an orphan conversation");

        // Starting a new standard chat with a non-orchestrating harness via POST /v1/chat succeeds.
        let r = http
            .post(format!("{base}/v1/chat"))
            .bearer_auth(&admin_token)
            .json(&json!({"text": "quick question", "mode": "standard", "model": "codex-o3"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let chat_resp: ChatResponse = r.json().await.unwrap();
        assert_eq!(hub.db.conversation(&chat_resp.conversation_id).unwrap().model.as_deref(), Some("codex-o3"));

        // Hub::free_alternates delegates and never returns a harness row.
        hub.db
            .upsert_model(&ModelEntry {
                id: "free-a".into(),
                provider_id: "openai".into(),
                model: "free-a".into(),
                free: true,
                ..Default::default()
            })
            .unwrap();
        hub.db
            .upsert_model(&ModelEntry {
                id: "free-harness".into(),
                provider_id: "openai".into(),
                model: "free-harness".into(),
                kind: "harness".into(),
                free: true,
                ..Default::default()
            })
            .unwrap();
        assert!(hub.free_alternates("openai", "free-a").is_empty());

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
