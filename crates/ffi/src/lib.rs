//! Swift-facing API for the Grouper core.
//!
//! Swift creates a [`GrouperClient`] with two callback objects:
//! - [`ToolHandler`]: Rust calls it when the hub asks this device to run a tool, so
//!   Apple-only tools (EventKit, Photos, …) are written in Swift but driven by the core.
//! - [`ClientListener`]: connection status and live device presence.

use std::sync::{Arc, Mutex};

use core_client::core_protocol;
use tokio::sync::watch;

uniffi::setup_scaffolding!();

#[derive(uniffi::Record, Clone, Debug)]
pub struct Capability {
    /// `^[a-zA-Z0-9_-]{1,64}$`, e.g. `mac_info` (no dots).
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, as a JSON string. `None`: takes no arguments.
    #[uniffi(default)]
    pub input_schema_json: Option<String>,
}

impl Capability {
    /// Check the name and parse the schema.
    fn to_core(&self) -> Result<core_protocol::Capability, ClientError> {
        if !core_protocol::valid_tool_name(&self.name) {
            return Err(ClientError::Invalid {
                message: format!("tool name {:?} must match ^[a-zA-Z0-9_-]{{1,64}}$ (no dots)", self.name),
            });
        }
        let input_schema = match &self.input_schema_json {
            None => core_protocol::default_input_schema(),
            Some(json) => {
                let schema: serde_json::Value = serde_json::from_str(json).map_err(|e| ClientError::Invalid {
                    message: format!("input schema of {:?} is not valid JSON: {e}", self.name),
                })?;
                if !schema.is_object() {
                    return Err(ClientError::Invalid {
                        message: format!("input schema of {:?} must be a JSON object", self.name),
                    });
                }
                schema
            }
        };
        Ok(core_protocol::Capability { name: self.name.clone(), description: self.description.clone(), input_schema })
    }

    fn from_core(c: core_protocol::ToolSummary) -> Self {
        Self { name: c.name, description: c.description, input_schema_json: None }
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub online: bool,
    pub capabilities: Vec<Capability>,
    pub last_seen: Option<u64>,
}

#[derive(uniffi::Enum, Clone, Debug)]
pub enum ConnectionStatus {
    Connecting,
    Connected { device_id: String },
    Disconnected { reason: String },
    Unauthorized,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ToolError {
    #[error("{message}")]
    Failed { message: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for ToolError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        ToolError::Failed { message: e.reason }
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ClientError {
    #[error("{message}")]
    Invalid { message: String },
}

/// Implemented in Swift.
#[uniffi::export(with_foreign)]
pub trait ToolHandler: Send + Sync {
    fn call_tool(&self, tool: String, args_json: String) -> Result<String, ToolError>;
}

/// Implemented in Swift. Called from a background thread.
#[uniffi::export(with_foreign)]
pub trait ClientListener: Send + Sync {
    fn on_status(&self, status: ConnectionStatus);
    fn on_presence(&self, devices: Vec<Device>);
    /// A subagent thread started (`running`) or ended (`done`/`failed`) on this device.
    fn on_thread(&self, thread_id: String, title: String, state: String);
}

struct ToolBridge(Arc<dyn ToolHandler>);
impl core_client::ToolHandler for ToolBridge {
    fn call(&self, tool: &str, args: &str) -> Result<String, String> {
        self.0.call_tool(tool.to_string(), args.to_string()).map_err(|e| e.to_string())
    }
}

struct EventBridge(Arc<dyn ClientListener>);
impl core_client::Events for EventBridge {
    fn status(&self, s: core_client::Status) {
        self.0.on_status(match s {
            core_client::Status::Connecting => ConnectionStatus::Connecting,
            core_client::Status::Connected { device_id } => ConnectionStatus::Connected { device_id },
            core_client::Status::Disconnected { reason } => ConnectionStatus::Disconnected { reason },
            core_client::Status::Unauthorized => ConnectionStatus::Unauthorized,
        });
    }
    fn presence(&self, devices: Vec<core_protocol::DeviceInfo>) {
        self.0.on_presence(
            devices
                .into_iter()
                .map(|d| Device {
                    id: d.id,
                    name: d.name,
                    platform: d.platform,
                    online: d.online,
                    capabilities: d.capabilities.into_iter().map(Capability::from_core).collect(),
                    last_seen: d.last_seen,
                })
                .collect(),
        );
    }
    fn thread(&self, thread_id: &str, title: &str, state: &str) {
        self.0.on_thread(thread_id.into(), title.into(), state.into());
    }
}

/// The device owner's usage-reading choice, as the node wants it. Unknown provider names are
/// ignored; an explicit list with none left means nothing is read.
fn usage_probe_setting(enabled: bool, providers: Option<Vec<String>>) -> core_client::usage_probes::UsageProbeSetting {
    use core_client::usage_probes::{SUPPORTED_PROVIDERS, UsageProbeSetting};
    if !enabled {
        return UsageProbeSetting::off();
    }
    let Some(list) = providers else {
        return UsageProbeSetting::default();
    };
    let mut kept: Vec<String> = Vec::new();
    for name in list {
        let name = name.trim().to_ascii_lowercase();
        if SUPPORTED_PROVIDERS.contains(&name.as_str()) && !kept.contains(&name) {
            kept.push(name);
        }
    }
    if kept.is_empty() {
        UsageProbeSetting::off()
    } else {
        UsageProbeSetting { enabled: true, providers: Some(kept) }
    }
}

#[derive(uniffi::Object)]
pub struct GrouperClient {
    runtime: tokio::runtime::Runtime,
    config: core_client::Config,
    handler: Arc<dyn ToolHandler>,
    listener: Arc<dyn ClientListener>,
    shutdown: Mutex<Option<watch::Sender<bool>>>,
}

#[uniffi::export]
impl GrouperClient {
    /// `workspace_root`: folder for thread workspaces, or `None` to run no threads here.
    /// `allow_shell`: offer `run_shell` to threads (still gated by permissions).
    /// `usage_probes`: read this device's subscription usage (percentages and reset times only).
    /// On by default in the apps; this is the device owner's local off switch.
    /// `usage_probe_providers` narrows it to some of claude, codex, grok and antigravity
    /// (`None`: all of them).
    /// `relay_token`: the relay sign-in token when `hub_url` is a relay; sent as `Grouper-Relay`.
    #[uniffi::constructor]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hub_url: String,
        token: String,
        capabilities: Vec<Capability>,
        workspace_root: Option<String>,
        allow_shell: bool,
        usage_probes: bool,
        usage_probe_providers: Option<Vec<String>>,
        relay_token: Option<String>,
        handler: Arc<dyn ToolHandler>,
        listener: Arc<dyn ClientListener>,
    ) -> Result<Arc<Self>, ClientError> {
        core_protocol::connect_url(&hub_url).map_err(|message| ClientError::Invalid { message })?;
        if core_protocol::split_token(&token).is_none() {
            return Err(ClientError::Invalid { message: "token should look like <device-id>.<secret>".into() });
        }
        let usage = usage_probe_setting(usage_probes, usage_probe_providers);
        let capabilities = capabilities.iter().map(Capability::to_core).collect::<Result<Vec<_>, _>>()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("grouper-core")
            .build()
            .map_err(|e| ClientError::Invalid { message: e.to_string() })?;
        Ok(Arc::new(Self {
            runtime,
            config: core_client::Config {
                hub_url,
                token,
                capabilities,
                workspace_root: workspace_root.map(std::path::PathBuf::from),
                allow_shell,
                // GROUPER_MCP_CONFIG, else <config dir>/grouper/mcp.json.
                mcp_config: None,
                // GROUPER_FOLDERS_CONFIG, else <config dir>/grouper/folders.json.
                folders: core_client::folders::load(None, &[]).unwrap_or_else(|e| {
                    eprintln!("grouper: scoped folders: {e}");
                    Vec::new()
                }),
                usage_probes: usage.enabled,
                usage_probe_providers: usage.providers,
                relay_token: relay_token.filter(|t| !t.trim().is_empty()),
            },
            handler,
            listener,
            shutdown: Mutex::new(None),
        }))
    }

    /// Start connecting (and keep reconnecting) in the background. No-op if running.
    pub fn start(&self) {
        let mut guard = self.shutdown.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let (tx, rx) = watch::channel(false);
        *guard = Some(tx);
        self.runtime.spawn(core_client::run(
            self.config.clone(),
            Arc::new(ToolBridge(self.handler.clone())),
            Arc::new(EventBridge(self.listener.clone())),
            rx,
        ));
    }

    /// Disconnect and stop reconnecting.
    pub fn stop(&self) {
        if let Some(tx) = self.shutdown.lock().unwrap().take() {
            let _ = tx.send(true);
        }
    }
}

// ---- offline cache (core-sync)

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SyncStoreError {
    /// The hub didn't answer. Reads from the cache still work.
    #[error("{message}")]
    Unreachable { message: String },
    /// The hub rejected the token: open Settings.
    #[error("the hub rejected the token")]
    Unauthorized,
    #[error("{message}")]
    Failed { message: String },
}

impl From<core_sync::SyncError> for SyncStoreError {
    fn from(e: core_sync::SyncError) -> Self {
        match e {
            core_sync::SyncError::Unreachable(message) => SyncStoreError::Unreachable { message },
            core_sync::SyncError::Unauthorized => SyncStoreError::Unauthorized,
            other => SyncStoreError::Failed { message: other.to_string() },
        }
    }
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct SyncSummary {
    pub conversations: u64,
    pub threads: u64,
    pub new_events: u64,
    /// `None` when the token isn't admin (memories need one).
    pub memories: Option<u64>,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct CachedConversation {
    pub id: String,
    /// `project` or `standard` (empty until the hub's list names it).
    pub mode: String,
    pub title: String,
    pub created: u64,
    pub model: Option<String>,
    pub last_seq: i64,
    pub last_activity: u64,
    /// `user` or `assistant`, with the latest message's text (clipped to 200 characters).
    pub preview_role: Option<String>,
    pub preview_text: Option<String>,
    pub unread: u32,
    pub pending_approvals: u32,
    pub active_threads: u32,
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct CachedThreadCard {
    pub thread_id: String,
    pub seq: i64,
    pub ts: u64,
    pub title: String,
    pub device_id: String,
    pub model: String,
    pub reason: Option<String>,
    /// `running`, `waiting_device`, `done`, `failed` or `stopped`.
    pub status: String,
    pub latest_text: Option<String>,
    pub open_tool: Option<String>,
    pub checkpoint: Option<String>,
    pub summary: Option<String>,
    pub pending_approvals: u32,
}

/// One transcript row, in `seq` order.
// A by-value FFI type (UniFFI enums can't hold a Box); the size doesn't matter here.
#[allow(clippy::large_enum_variant)]
#[derive(uniffi::Enum, Clone, Debug)]
pub enum CachedMessage {
    User { seq: i64, ts: u64, text: String },
    Assistant { seq: i64, ts: u64, text: String },
    Thread { card: CachedThreadCard },
    Error { seq: i64, ts: u64, message: String },
    BudgetWarning { seq: i64, ts: u64, scope: String, spent: f64, limit: f64 },
}

#[derive(uniffi::Record, Clone, Debug)]
pub struct CachedApproval {
    pub approval_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub device_id: String,
    pub tool: String,
    /// The tool's arguments as JSON text, for pretty-printing.
    pub input_json: String,
    pub reason: String,
    pub seq: i64,
    pub ts: u64,
}

/// The app's local copy of the hub's chats: read it any time (offline included),
/// refresh it with `sync_once` when the hub is reachable. Read-only: sending goes
/// through the hub's HTTP API. The token is used, never stored in the cache.
/// Calls block: make them off the main thread.
#[derive(uniffi::Object)]
pub struct SyncStore {
    runtime: tokio::runtime::Runtime,
    store: Arc<core_sync::Store>,
    client: Option<core_sync::SyncClient>,
}

#[uniffi::export]
impl SyncStore {
    /// Open (or create) the cache at `cache_path`. Without `hub_url` and `token` it
    /// only reads (`sync_once` then fails).
    #[uniffi::constructor]
    pub fn open(
        cache_path: String,
        hub_url: Option<String>,
        token: Option<String>,
        relay_token: Option<String>,
    ) -> Result<Arc<Self>, SyncStoreError> {
        let store = Arc::new(core_sync::Store::open(&cache_path)?);
        let client = match (hub_url, token) {
            (Some(hub), Some(token)) => Some(core_sync::SyncClient::new(&hub, &token, store.clone())?.with_relay_token(relay_token)),
            _ => None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| SyncStoreError::Failed { message: e.to_string() })?;
        Ok(Arc::new(Self { runtime, store, client }))
    }

    /// Catch up with the hub once. Safe to repeat; nothing is stored twice.
    pub fn sync_once(&self) -> Result<SyncSummary, SyncStoreError> {
        let client = self
            .client
            .as_ref()
            .ok_or(SyncStoreError::Failed { message: "opened without a hub URL and token".into() })?;
        let r = self.runtime.block_on(client.sync_once())?;
        Ok(SyncSummary {
            conversations: r.conversations as u64,
            threads: r.threads as u64,
            new_events: r.new_events as u64,
            memories: r.memories.map(|n| n as u64),
        })
    }

    /// Unix seconds the cache last heard from the hub, for "cached at …".
    pub fn last_update(&self) -> Result<Option<u64>, SyncStoreError> {
        Ok(self.store.last_update()?)
    }

    /// Conversations, most recently active first.
    pub fn conversations(&self) -> Result<Vec<CachedConversation>, SyncStoreError> {
        Ok(self
            .store
            .conversation_summaries()?
            .into_iter()
            .map(|c| CachedConversation {
                id: c.info.id,
                mode: c.info.mode,
                title: c.info.title,
                created: c.info.created,
                model: c.info.model,
                last_seq: c.last_seq,
                last_activity: c.last_activity,
                preview_role: c.preview.as_ref().map(|p| p.role.clone()),
                preview_text: c.preview.map(|p| p.text),
                unread: c.unread,
                pending_approvals: c.pending_approvals,
                active_threads: c.active_threads,
            })
            .collect())
    }

    /// A conversation's messages and thread cards.
    pub fn messages(&self, conversation_id: String) -> Result<Vec<CachedMessage>, SyncStoreError> {
        use core_sync::TranscriptItem as T;
        Ok(self
            .store
            .transcript(&conversation_id)?
            .into_iter()
            .map(|item| match item {
                T::User { seq, ts, text } => CachedMessage::User { seq, ts, text },
                T::Assistant { seq, ts, text } => CachedMessage::Assistant { seq, ts, text },
                T::Error { seq, ts, message } => CachedMessage::Error { seq, ts, message },
                T::BudgetWarning { seq, ts, scope, spent, limit } => {
                    CachedMessage::BudgetWarning { seq, ts, scope, spent, limit }
                }
                T::Thread(c) => {
                    let c = *c;
                    CachedMessage::Thread {
                        card: CachedThreadCard {
                            thread_id: c.thread_id,
                            seq: c.seq,
                            ts: c.ts,
                            title: c.title,
                            device_id: c.device_id,
                            model: c.model,
                            reason: c.reason,
                            status: c.status,
                            latest_text: c.latest_text,
                            open_tool: c.open_tool,
                            checkpoint: c.checkpoint,
                            summary: c.summary,
                            pending_approvals: c.pending_approvals,
                        },
                    }
                }
            })
            .collect())
    }

    /// Approvals waiting for an answer (one conversation's, or all).
    pub fn pending_approvals(&self, conversation_id: Option<String>) -> Result<Vec<CachedApproval>, SyncStoreError> {
        Ok(self
            .store
            .pending_approvals(conversation_id.as_deref())?
            .into_iter()
            .map(|a| CachedApproval {
                approval_id: a.approval_id,
                conversation_id: a.conversation_id,
                thread_id: a.thread_id,
                device_id: a.device_id,
                tool: a.tool,
                input_json: a.input.to_string(),
                reason: a.reason,
                seq: a.seq,
                ts: a.ts,
            })
            .collect())
    }

    /// Mark a conversation read up to `upto_seq` (its latest event when `None`).
    pub fn mark_read(&self, conversation_id: String, upto_seq: Option<i64>) -> Result<(), SyncStoreError> {
        Ok(self.store.mark_read(&conversation_id, upto_seq)?)
    }
}

/// Version of the Rust core, handy for checking the bindings are wired up.
#[uniffi::export]
pub fn core_version() -> String {
    core_client::CLIENT_VERSION.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(name: &str, schema: Option<&str>) -> Capability {
        Capability { name: name.into(), description: "d".into(), input_schema_json: schema.map(String::from) }
    }

    #[test]
    fn sync_store_reads_an_empty_cache_and_needs_a_hub_to_sync() {
        let dir = std::env::temp_dir().join(format!("grouper-ffi-sync-{}", std::process::id()));
        let path = dir.join("cache.db").to_string_lossy().to_string();
        let store = SyncStore::open(path.clone(), None, None, None).unwrap();
        assert!(store.conversations().unwrap().is_empty());
        assert!(store.messages("nope".into()).unwrap().is_empty());
        assert!(store.pending_approvals(None).unwrap().is_empty());
        assert!(matches!(store.sync_once(), Err(SyncStoreError::Failed { .. })));
        drop(store);
        // Nothing listens on port 9: unreachable, not a crash.
        let store = SyncStore::open(path, Some("http://127.0.0.1:9".into()), Some("d.s".into()), None).unwrap();
        assert!(matches!(store.sync_once(), Err(SyncStoreError::Unreachable { .. })));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_reading_follows_the_device_setting() {
        use core_client::usage_probes::UsageProbeSetting;
        assert_eq!(usage_probe_setting(true, None), UsageProbeSetting::default());
        assert_eq!(usage_probe_setting(false, Some(vec!["claude".into()])), UsageProbeSetting::off());
        let some = usage_probe_setting(true, Some(vec![" Grok ".into(), "bogus".into(), "grok".into(), "claude".into()]));
        assert_eq!(some.providers, Some(vec!["grok".to_string(), "claude".to_string()]));
        assert_eq!(usage_probe_setting(true, Some(vec![])), UsageProbeSetting::off());
        assert_eq!(usage_probe_setting(true, Some(vec!["bogus".into()])), UsageProbeSetting::off());
    }

    #[test]
    fn capabilities_are_validated() {
        let c = cap("mac_info", None).to_core().unwrap();
        assert_eq!(c.input_schema, core_protocol::default_input_schema());
        let c = cap("echo", Some(r#"{"type":"object","properties":{"x":{"type":"string"}}}"#)).to_core().unwrap();
        assert_eq!(c.input_schema["properties"]["x"]["type"], "string");
        assert!(cap("mac.info", None).to_core().is_err());
        assert!(cap("echo", Some("{not json")).to_core().is_err());
        assert!(cap("echo", Some("[1]")).to_core().is_err());
    }
}
