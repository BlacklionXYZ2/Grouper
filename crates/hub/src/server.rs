//! The hub: device connections, presence, the HTTP API and the live event stream.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Json, Router};
use core_agent::thread::ThreadControl;
use core_protocol::{
    Capability, ChatRequest, ChatResponse, DeviceInfo, Event, EventBody, HubMessage, IDLE_TIMEOUT_SECS, ModelEntry,
    NodeMessage, PROTOCOL_VERSION, Permission, ProviderConfig, ProviderInfo, ToolCallRequest, ToolCallResponse,
    ToolDef,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use crate::db::{Auth, Db};
use crate::orchestrator::Wake;

const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// The hub's own id in the device and tool registries.
pub const HUB_DEVICE: &str = "hub";

pub(crate) struct Live {
    conn_id: u64,
    pub(crate) tx: mpsc::UnboundedSender<HubMessage>,
    capabilities: Vec<Capability>,
}

/// Where to send the answer to an approval request.
pub(crate) struct ApprovalRoute {
    pub(crate) device_id: String,
    pub(crate) conversation_id: String,
    pub(crate) thread_id: String,
    /// Set for threads running on the hub itself.
    pub(crate) local: Option<oneshot::Sender<bool>>,
}

pub struct Hub {
    pub db: Db,
    pub data_dir: PathBuf,
    pub(crate) live: Mutex<HashMap<String, Live>>,
    pending_calls: Mutex<HashMap<String, oneshot::Sender<ToolCallResponse>>>,
    pub(crate) approvals: Mutex<HashMap<String, ApprovalRoute>>,
    pub(crate) hub_threads: Mutex<HashMap<String, ThreadControl>>,
    pub(crate) orchestrators: Mutex<HashMap<String, mpsc::UnboundedSender<Wake>>>,
    events: broadcast::Sender<Event>,
    pub(crate) next_id: AtomicU64,
}

impl Hub {
    pub fn new(db: Db, data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            db,
            data_dir,
            live: Mutex::new(HashMap::new()),
            pending_calls: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            hub_threads: Mutex::new(HashMap::new()),
            orchestrators: Mutex::new(HashMap::new()),
            // Roomy: streamed text sends many small events.
            events: broadcast::channel(8192).0,
            next_id: AtomicU64::new(1),
        })
    }

    /// Store an event and send it to everyone watching.
    pub fn emit(&self, conversation_id: &str, thread_id: Option<&str>, body: EventBody) -> Event {
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
        }];
        out.extend(self.db.devices().into_iter().filter(|d| !d.revoked).map(|d| {
            let l = live.get(&d.id);
            DeviceInfo {
                id: d.id,
                name: d.name,
                platform: d.platform,
                online: l.is_some(),
                capabilities: l.map(|l| l.capabilities.clone()).unwrap_or_default(),
                last_seen: d.last_seen,
            }
        }));
        out
    }

    pub async fn is_online(&self, device_id: &str) -> bool {
        device_id == HUB_DEVICE || self.live.lock().await.contains_key(device_id)
    }

    pub async fn send_to(&self, device_id: &str, msg: HubMessage) -> bool {
        self.live.lock().await.get(device_id).is_some_and(|l| l.tx.send(msg).is_ok())
    }

    async fn broadcast(&self, msg: HubMessage) {
        for l in self.live.lock().await.values() {
            let _ = l.tx.send(msg.clone());
        }
    }

    async fn broadcast_presence(&self) {
        let devices = self.devices().await;
        self.broadcast(HubMessage::Presence { devices }).await;
    }

    pub async fn broadcast_providers(&self) {
        match self.db.providers() {
            Ok(providers) => self.broadcast(HubMessage::Providers { providers }).await,
            Err(error) => tracing::error!("could not decrypt provider configuration: {error:#}"),
        }
    }

    pub(crate) fn auth(&self, headers: &HeaderMap) -> Option<Auth> {
        let token = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
        self.db.authenticate(token)
    }

    /// Send a direct tool call to an online device and wait for its answer.
    pub async fn call_tool(&self, device_id: &str, tool: &str, args: String) -> Result<ToolCallResponse, CallError> {
        let call_id = format!("c{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            let live = self.live.lock().await;
            let Some(l) = live.get(device_id) else { return Err(CallError::Offline) };
            if !l.capabilities.iter().any(|c| c.name == tool) {
                return Err(CallError::UnknownTool);
            }
            self.pending_calls.lock().await.insert(call_id.clone(), tx);
            if l.tx.send(HubMessage::ToolCall { call_id: call_id.clone(), tool: tool.into(), args }).is_err() {
                self.pending_calls.lock().await.remove(&call_id);
                return Err(CallError::Offline);
            }
        }
        match tokio::time::timeout(TOOL_TIMEOUT, rx).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(_)) => Err(CallError::Offline),
            Err(_) => {
                self.pending_calls.lock().await.remove(&call_id);
                Err(CallError::Timeout)
            }
        }
    }

    /// A user's message: create the conversation if needed and wake its agent.
    pub async fn chat(self: &Arc<Self>, req: ChatRequest) -> anyhow::Result<String> {
        let text = req.text.trim().to_string();
        anyhow::ensure!(!text.is_empty(), "message is empty");
        let id = match req.conversation_id {
            Some(id) => {
                anyhow::ensure!(self.db.conversation(&id).is_some(), "no such conversation");
                id
            }
            None => {
                let mode = req.mode.as_deref().unwrap_or("project");
                anyhow::ensure!(mode == "project" || mode == "standard", "mode must be project or standard");
                let title: String = text.chars().take(60).collect();
                self.db.create_conversation(mode, &title)?
            }
        };
        self.emit(&id, None, EventBody::UserMessage { text: text.clone() });
        self.wake(&id, Wake::User(text)).await;
        Ok(id)
    }

    /// Resolve an approval request. Returns false if it doesn't exist (any more).
    pub async fn resolve_approval(&self, approval_id: &str, approved: bool) -> bool {
        let Some(route) = self.approvals.lock().await.remove(approval_id) else { return false };
        match route.local {
            Some(tx) => {
                let _ = tx.send(approved);
            }
            None => {
                self.send_to(&route.device_id, HubMessage::ApprovalResult { approval_id: approval_id.into(), approved })
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
}

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
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
        .route("/v1/conversations/{id}/events", get(conversation_events))
        .route("/v1/threads", get(list_threads))
        .route("/v1/threads/{id}", get(get_thread))
        .route("/v1/threads/{id}/message", post(thread_message))
        .route("/v1/threads/{id}/stop", post(thread_stop))
        .route("/v1/approvals/{id}", post(approval))
        .route("/v1/memories", get(list_memories).post(add_memory))
        .route("/v1/memories/{id}", put(update_memory).delete(delete_memory))
        .merge(crate::registry::routes())
        .merge(crate::skills::routes())
        .with_state(hub)
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
    match hub.call_tool(&id, &req.tool, req.args.to_string()).await {
        Ok(r) => Json(r).into_response(),
        Err(CallError::Offline) => err(StatusCode::CONFLICT, "device is offline"),
        Err(CallError::UnknownTool) => err(StatusCode::NOT_FOUND, "device does not offer that tool"),
        Err(CallError::Timeout) => err(StatusCode::GATEWAY_TIMEOUT, "device did not answer in time"),
    }
}

#[derive(Deserialize)]
struct DeviceFilter {
    device: Option<String>,
}

async fn list_tools(State(hub): State<Arc<Hub>>, headers: HeaderMap, Query(q): Query<DeviceFilter>) -> Response {
    authed!(hub, headers);
    Json(hub.db.tools(q.device.as_deref())).into_response()
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
    authed!(hub, headers);
    let mut last_seq = hub.db.last_event_seq();
    let mut rx = hub.events.subscribe();
    ws.on_upgrade(move |mut socket| async move {
        loop {
            tokio::select! {
                e = rx.recv() => match e {
                    Ok(e) => {
                        if q.conversation.as_ref().is_some_and(|c| *c != e.conversation_id) {
                            continue;
                        }
                        if e.seq > 0 && e.seq <= last_seq {
                            continue;
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

async fn connect(State(hub): State<Arc<Hub>>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let device_id = authed!(hub, headers).device_id;
    ws.on_upgrade(move |socket| async move {
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
    let (capabilities, thread_tools): (Vec<Capability>, Vec<ToolDef>) = match first {
        Message::Text(t) => match serde_json::from_str::<NodeMessage>(&t) {
            Ok(NodeMessage::Hello { protocol_version, capabilities, client_version, thread_tools }) => {
                if protocol_version != PROTOCOL_VERSION {
                    let msg = HubMessage::Error {
                        message: format!("protocol {protocol_version} unsupported, hub speaks {PROTOCOL_VERSION}"),
                    };
                    let _ = sink.send(Message::Text(serde_json::to_string(&msg).unwrap().into())).await;
                    return Err("protocol mismatch".into());
                }
                tracing::info!(device = %device_id, %client_version, tools = thread_tools.len(), "connected");
                (capabilities, thread_tools)
            }
            _ => return Err("first frame was not hello".into()),
        },
        _ => return Err("first frame was not text".into()),
    };

    let providers = hub.db.providers().map_err(|error| error.to_string())?;
    hub.db.upsert_tools(&device_id, &thread_tools);
    let conn_id = hub.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<HubMessage>();
    hub.live.lock().await.insert(device_id.clone(), Live { conn_id, tx: tx.clone(), capabilities });
    hub.db.touch(&device_id);
    let _ = tx.send(HubMessage::Welcome {
        device_id: device_id.clone(),
        hub_version: HUB_VERSION.into(),
        protocol_version: PROTOCOL_VERSION,
    });
    let _ = tx.send(HubMessage::Providers { providers });
    hub.broadcast_presence().await;

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let text = serde_json::to_string(&msg).expect("serialize");
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let result = loop {
        let next = match tokio::time::timeout(Duration::from_secs(IDLE_TIMEOUT_SECS), stream.next()).await {
            Err(_) => break Err("idle timeout".to_string()),
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => break Err(e.to_string()),
            Ok(Some(Ok(m))) => m,
        };
        let text = match next {
            Message::Text(t) => t,
            Message::Close(_) => break Ok(()),
            _ => continue,
        };
        match serde_json::from_str::<NodeMessage>(&text) {
            Ok(NodeMessage::Ping) => {
                let _ = tx.send(HubMessage::Pong);
            }
            Ok(NodeMessage::ToolResult { call_id, ok, output }) => {
                if let Some(waiter) = hub.pending_calls.lock().await.remove(&call_id) {
                    let _ = waiter.send(ToolCallResponse { ok, output });
                }
            }
            Ok(NodeMessage::ThreadEvent { thread_id, event }) => {
                if hub.owns_thread(&device_id, &thread_id) {
                    hub.thread_event(&thread_id, event);
                }
            }
            Ok(NodeMessage::ThreadFinished { thread_id, ok, summary }) => {
                if hub.owns_thread(&device_id, &thread_id) {
                    hub.thread_finished(&thread_id, ok, &summary).await;
                }
            }
            Ok(NodeMessage::ApprovalRequest { approval_id, thread_id, tool, input, reason }) => {
                if hub.owns_thread(&device_id, &thread_id) {
                    hub.request_approval(&device_id, &thread_id, &approval_id, &tool, input, &reason, None).await;
                }
            }
            Ok(NodeMessage::AskOrchestrator { thread_id, question }) => {
                if hub.owns_thread(&device_id, &thread_id) {
                    hub.thread_ask(&thread_id, &question).await;
                }
            }
            Ok(NodeMessage::Hello { .. }) => {
                let _ = tx.send(HubMessage::Error { message: "already said hello".into() });
            }
            Err(e) => {
                let _ = tx.send(HubMessage::Error { message: format!("bad frame: {e}") });
            }
        }
    };

    let replaced = {
        let mut live = hub.live.lock().await;
        if live.get(&device_id).is_some_and(|l| l.conn_id == conn_id) {
            live.remove(&device_id);
            false
        } else {
            true
        }
    };
    hub.db.touch(&device_id);
    drop(tx);
    writer.abort();
    if !replaced {
        hub.device_gone(&device_id).await;
    }
    hub.broadcast_presence().await;
    tracing::info!(device = %device_id, "disconnected");
    result
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
        })
        .unwrap();
        let hub = Hub::new(db, dir.clone());
        let mut rx = hub.events.subscribe();
        let conv = hub
            .chat(ChatRequest { conversation_id: None, mode: Some("project".into()), text: "go now".into() })
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
}
