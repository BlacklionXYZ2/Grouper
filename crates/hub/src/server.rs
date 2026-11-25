//! The hub: device connections, presence, the HTTP API and the live event stream.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Json, Router};
use core_agent::thread::ThreadControl;
use core_protocol::{
    ChatRequest, ChatResponse, DeviceInfo, Event, EventBody, HubMessage, IDLE_TIMEOUT_SECS, MAX_WS_MESSAGE_BYTES,
    ModelEntry, NodeMessage, PROTOCOL_VERSION, Permission, ProviderConfig, ProviderInfo, ToolCallRequest, ToolDef,
    ToolSummary,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
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
    /// Set for threads running on the hub itself (and for the hub's hand-over copy).
    pub(crate) local: Option<oneshot::Sender<ApprovalAnswer>>,
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
}

impl Hub {
    pub fn new(db: Db, data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            db,
            data_dir,
            live: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            handover_acks: Mutex::new(HashMap::new()),
            model_calls: Mutex::new(HashMap::new()),
            handover_ack_timeout: std::sync::Mutex::new(crate::offline::HANDOVER_ACK_TIMEOUT),
            hub_threads: Mutex::new(HashMap::new()),
            orchestrators: Mutex::new(HashMap::new()),
            // Roomy: streamed text sends many small events.
            events: broadcast::channel(8192).0,
            session_invalidations: broadcast::channel(128).0,
            emit_order: std::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            limits: Default::default(),
        })
    }

    /// Override how long a hand-over waits for `WorkspaceFileResult` acknowledgements.
    pub fn set_handover_ack_timeout(&self, timeout: Duration) {
        *self.handover_ack_timeout.lock().unwrap_or_else(|e| e.into_inner()) = timeout;
    }

    pub(crate) fn handover_ack_timeout(&self) -> Duration {
        *self.handover_ack_timeout.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Store an event and send it to everyone watching.
    pub fn emit(&self, conversation_id: &str, thread_id: Option<&str>, body: EventBody) -> Event {
        let _order = self.emit_order.lock().unwrap_or_else(|e| e.into_inner());
        let e = self.db.add_event(conversation_id, thread_id, body);
        let _ = self.events.send(e.clone());
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

    pub(crate) async fn broadcast_presence(&self) {
        let devices = self.devices().await;
        self.broadcast(HubMessage::Presence { devices }).await;
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
        let text = req.text.trim().to_string();
        anyhow::ensure!(!text.is_empty(), "message is empty");
        let requested = req.model.as_deref().map(str::trim).filter(|model| !model.is_empty()).map(str::to_string);
        if let Some(model) = &requested {
            anyhow::ensure!(self.db.find_model(model).is_some(), "no model {model:?} in the registry");
        }
        let id = match &req.conversation_id {
            Some(id) => {
                anyhow::ensure!(self.db.conversation(id).is_some(), "no such conversation");
                if requested.is_some() && !self.db.history(id).is_empty() {
                    anyhow::bail!(
                        "model can be set on the first message; use POST /v1/conversations/{id}/model to change it"
                    );
                }
                id.clone()
            }
            None => {
                let mode = req.mode.as_deref().unwrap_or("project");
                anyhow::ensure!(mode == "project" || mode == "standard", "mode must be project or standard");
                let title: String = text.chars().take(60).collect();
                self.db.create_conversation(mode, &title)?
            }
        };
        if let Some(model) = &requested {
            self.db.set_conversation_model(&id, Some(model))?;
        }
        self.emit(&id, None, EventBody::UserMessage { text: text.clone() });
        self.wake(&id, Wake::User(text)).await;
        Ok(id)
    }

    /// Resolve an approval request. Returns false if it doesn't exist (any more).
    pub async fn resolve_approval(&self, approval_id: &str, approved: bool) -> bool {
        let Some(mut route) = self.approvals.lock().await.remove(approval_id) else { return false };
        if let Some(timer) = route.timer.take() {
            timer.abort();
        }
        match route.local {
            Some(tx) => {
                let _ = tx.send(if approved { ApprovalAnswer::Allow } else { ApprovalAnswer::Deny });
            }
            None => {
                self.send_to(
                    &route.device_id,
                    HubMessage::ApprovalResult { approval_id: approval_id.into(), approved, timeout_secs: None },
                )
                .await;
            }
        }
        self.emit(
            &route.conversation_id,
            Some(&route.thread_id),
            EventBody::ApprovalResolved { approval_id: approval_id.into(), approved },
        );
        true
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
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{id}/call", post(call_device))
        .route("/v1/devices/{id}/tools/{tool}", put(set_permission))
        .route("/v1/tools", get(list_tools))
        .route("/v1/providers", get(list_providers))
        .route("/v1/providers/{id}", put(put_provider))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{id}", put(put_model).delete(delete_model))
        .route("/v1/usage", get(list_usage))
        .route("/v1/usage/snapshots", get(list_usage_snapshots))
        .route("/v1/usage/refresh", post(refresh_usage))
        .route("/v1/chat", post(chat))
        .route("/v1/conversations", get(list_conversations))
        .route("/v1/conversations/{id}/model", post(set_conversation_model))
        .route("/v1/conversations/{id}/events", get(conversation_events))
        .route("/v1/threads", get(list_threads))
        .route("/v1/threads/{id}", get(get_thread))
        .route("/v1/threads/{id}/message", post(thread_message))
        .route("/v1/threads/{id}/stop", post(thread_stop))
        .route("/v1/approvals/{id}", post(approval))
        .route("/v1/memories", get(list_memories).post(add_memory))
        .route("/v1/memories/{id}", put(update_memory).delete(delete_memory))
        .merge(crate::registry::routes())
        .merge(crate::catalog::routes())
        .merge(crate::skills::routes())
        .merge(crate::limits::routes())
        .merge(crate::files::routes())
        .merge(crate::session::routes())
        .merge(crate::web::routes())
        .with_state(hub)
        .layer(axum::middleware::from_fn(crate::session::guard))
        .layer(axum::middleware::from_fn(crate::session::response_security))
}

pub(crate) fn err(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
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
    Json(json!({"id": p.id, "saved": true})).into_response()
}

async fn list_models(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    authed!(hub, headers);
    Json(hub.db.models()).into_response()
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
        Ok(()) => Json(m).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
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
    match hub.db.usage_totals(since, until, q.group.as_deref().unwrap_or("day"), q.conversation.as_deref()) {
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

async fn refresh_usage(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    admin!(hub, headers);
    Json(crate::usage::refresh(&hub).await).into_response()
}

async fn chat(State(hub): State<Arc<Hub>>, headers: HeaderMap, Json(req): Json<ChatRequest>) -> Response {
    admin!(hub, headers);
    match hub.chat(req).await {
        Ok(conversation_id) => Json(ChatResponse { conversation_id }).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn list_conversations(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    authed!(hub, headers);
    Json(hub.db.conversations()).into_response()
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
            err(status, error)
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
                        Ok(id) => credential.session_id().is_some_and(|mine| mine == id),
                        Err(broadcast::error::RecvError::Lagged(_)) => credential.session_id().is_some(),
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
    Session(String),
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
            .map(|id| Self::Session(id.to_string()))
    }

    fn valid(&self, hub: &Hub) -> bool {
        match self {
            Self::Bearer(token) => hub.db.authenticate(token).is_some(),
            Self::Session(id) => hub.db.session_auth(id).is_some(),
        }
    }

    fn session_id(&self) -> Option<&str> {
        match self {
            Self::Bearer(_) => None,
            Self::Session(id) => Some(id),
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
    match first {
        Message::Text(t) => match serde_json::from_str::<NodeMessage>(&t) {
            Ok(NodeMessage::Hello { protocol_version, client_version }) => {
                if protocol_version != PROTOCOL_VERSION {
                    let msg = HubMessage::Error {
                        message: format!("protocol {protocol_version} unsupported, hub speaks {PROTOCOL_VERSION}"),
                    };
                    let _ = sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;
                    return Err("protocol mismatch".into());
                }
                tracing::info!(device = %device_id, %client_version, "connected");
            }
            _ => return Err("first frame was not hello".into()),
        },
        _ => return Err("first frame was not text".into()),
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
            hub.approvals.lock().await.retain(|_, r| {
                if r.device_id == device_id {
                    if let Some(timer) = r.timer.take() {
                        timer.abort();
                    }
                    false
                } else {
                    true
                }
            });
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
    hub.broadcast_presence().await;
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
            Ok(NodeMessage::ModelRequest { call_id, provider_id, request }) => {
                if hub.is_live_conn(&device_id, conn_id).await {
                    hub.relay_model_request(&device_id, conn_id, call_id, provider_id, request).await;
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
                | NodeMessage::AskOrchestrator { .. }),
            ) => {
                handle_thread_frame(&hub, &device_id, conn_id, msg).await;
            }
            Ok(NodeMessage::WorkspaceFileResult { thread_id, path, ok, error }) => {
                if hub.is_live_conn(&device_id, conn_id).await {
                    hub.workspace_file_result(&device_id, conn_id, &thread_id, path, ok, error).await;
                }
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
        hub.broadcast_presence().await;
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
            hub.thread_finished(&thread_id, ok, &summary).await;
            crate::usage::after_device_thread(hub, device_id, &thread_id);
        }
        NodeMessage::ApprovalRequest { approval_id, thread_id, tool, input, reason }
            if hub.owns_thread(device_id, &thread_id) =>
        {
            if !hub.request_approval(device_id, &thread_id, &approval_id, &tool, input, &reason, None).await {
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
async fn hub_info(State(hub): State<Arc<Hub>>) -> Response {
    Json(core_protocol::HubInfo {
        hub_id: hub.db.hub_id(),
        hub_version: HUB_VERSION.into(),
        protocol_version: PROTOCOL_VERSION,
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
            let hello = NodeMessage::Hello { protocol_version: PROTOCOL_VERSION, client_version: "test".into() };
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
}
