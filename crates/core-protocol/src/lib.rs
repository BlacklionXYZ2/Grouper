//! Grouper wire protocol.
//!
//! Every device connects to the hub with an outbound WebSocket at [`CONNECT_PATH`],
//! authenticating with `Authorization: Bearer <device-token>` on the upgrade request.
//! Frames are JSON text messages: devices send [`NodeMessage`], the hub sends [`HubMessage`].
//!
//! Tool calls travel hub → device over the device's own connection, so devices never
//! need to accept inbound connections. Each device is an MCP server and the hub its
//! MCP client: JSON-RPC rides inside [`NodeMessage::Mcp`] and [`HubMessage::Mcp`]
//! (see the `tunnel` feature for the rmcp transport).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "tunnel")]
pub mod tunnel;

/// Bumped on any breaking change to the frames below.
pub const PROTOCOL_VERSION: u32 = 5;

/// WebSocket endpoint for devices, relative to the hub's base URL.
pub const CONNECT_PATH: &str = "/v1/connect";

/// WebSocket endpoint streaming conversation [`Event`]s to clients.
pub const EVENTS_PATH: &str = "/v1/events";

/// How often a device sends [`NodeMessage::Ping`].
pub const PING_INTERVAL_SECS: u64 = 20;

/// The hub drops a connection that has been silent for this long.
pub const IDLE_TIMEOUT_SECS: u64 = 60;

/// Maximum incoming WebSocket message size accepted by the hub and clients (16 MB).
/// Fits an 8 MB base64-encoded hand-over file plus framing overhead.
pub const MAX_WS_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Something a device can do for the orchestrator.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Capability {
    /// Tool name, valid for every provider and for MCP (see [`valid_tool_name`]),
    /// e.g. `mac_info`, `codex__codex_chat`. No dots.
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments (an object schema).
    #[serde(default = "default_input_schema")]
    pub input_schema: Value,
}

/// The input schema of a tool that takes no arguments: `{"type":"object","properties":{}}`.
pub fn default_input_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// Tool names every provider and MCP accept: `^[a-zA-Z0-9_-]{1,64}$`.
pub fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A tool's name and description as carried in [`DeviceInfo`] (presence and
/// `GET /v1/devices`). Input schemas are omitted to keep presence small; fetch
/// `GET /v1/tools` ([`ToolInfo`]) when schemas are needed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
}

/// A registered device as the hub sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// `macos`, `ios`, `windows`, `linux`, …
    pub platform: String,
    pub online: bool,
    /// Capabilities reported on the device's latest connection (names and descriptions only).
    pub capabilities: Vec<ToolSummary>,
    /// Unix seconds when the device was last heard from, if ever.
    pub last_seen: Option<u64>,
    /// Scoped folders the device last reported (names and modes, never paths).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<FolderInfo>,
}

/// What threads may do in a device's scoped folder.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FolderMode {
    Read,
    ReadWrite,
}

impl FolderMode {
    /// `read`/`ro`/`r` or `read_write`/`rw`/`read-write`, any case.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" | "ro" | "r" | "read_only" | "read-only" => Some(Self::Read),
            "read_write" | "rw" | "read-write" | "readwrite" => Some(Self::ReadWrite),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }
}

/// A scoped folder as a device reports it to the hub: a named root the user allowed
/// on that device, reached by threads as `@<name>/<path>`. The real path stays on the device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FolderInfo {
    pub name: String,
    pub mode: FolderMode,
}

/// Folder names: `^[A-Za-z0-9_-]{1,32}$`, matched case-insensitively.
pub fn valid_folder_name(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Key in the device MCP server's `initialize` result `_meta` carrying its
/// [`FolderInfo`] list.
pub const FOLDERS_META_KEY: &str = "grouper/folders";

/// Key in the device MCP server's `initialize` result `_meta` advertising opted-in
/// read-only subscription usage probes (e.g. `["claude", "codex"]`).
pub const USAGE_PROBES_META_KEY: &str = "grouper/usage_probes";

/// Device MCP tools only the hub calls (never listed, never offered to threads):
/// list a thread's workspace (`{thread_id}`), read one file from it (`{thread_id, path}`),
/// and probe local subscription usage (`{providers?: ["claude", "codex"]}`).
pub const WORKSPACE_LIST_TOOL: &str = "grouper_workspace_list";
pub const WORKSPACE_READ_TOOL: &str = "grouper_workspace_read";
pub const USAGE_PROBE_TOOL: &str = "grouper_usage_probe";
/// Largest workspace file the hub fetches from a device.
pub const MAX_WORKSPACE_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// One file in a thread's workspace (`path` uses `/`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceFile {
    pub path: String,
    pub size: u64,
}

/// `GET /v1/threads/{id}/files`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceListing {
    pub thread_id: String,
    /// Where the files are: the thread's device, or `hub`.
    pub device_id: String,
    pub files: Vec<WorkspaceFile>,
    /// More files exist than were listed.
    #[serde(default)]
    pub truncated: bool,
}

/// Device → hub.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeMessage {
    /// Must be the first frame on a new connection. The device's tools are listed
    /// over MCP (`tools/list`) once the hub has welcomed it.
    Hello { protocol_version: u32, client_version: String },
    /// One MCP JSON-RPC message from the device's MCP server to the hub's client.
    Mcp { message: Value },
    /// Ask the hub to relay a model call to the device named by the provider's
    /// `options.via_device`. Used by a thread running on a different device.
    ModelRequest { call_id: String, provider_id: String, request: Value },
    /// A streamed model delta from this device when it is hosting the provider.
    ModelDelta { call_id: String, delta: Value },
    /// The final model response, or an error, from this provider host.
    ModelResponse {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Cancel a relayed model call previously requested by this device.
    ModelCancel { call_id: String },
    Ping,
    /// Something happened in a thread running on this device.
    ThreadEvent { thread_id: String, event: AgentEvent },
    ThreadFinished { thread_id: String, ok: bool, summary: String },
    /// A thread wants to run a tool that needs the user's approval.
    ApprovalRequest { approval_id: String, thread_id: String, tool: String, input: Value, reason: String },
    /// A thread needs something from the orchestrator (often another device's data).
    AskOrchestrator { thread_id: String, question: String },
    /// Offline hand-over: acknowledgement for one [`HubMessage::WorkspaceFile`].
    WorkspaceFileResult {
        thread_id: String,
        path: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Hub → device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMessage {
    Welcome {
        device_id: String,
        device_name: String,
        hub_version: String,
        protocol_version: u32,
    },
    /// Full device list, sent on connect and whenever any device comes or goes.
    Presence { devices: Vec<DeviceInfo> },
    /// One MCP JSON-RPC message from the hub's MCP client to the device's server
    /// (`initialize`, `tools/list`, `tools/call`, `notifications/cancelled`, …).
    Mcp { message: Value },
    /// Run a model call on this provider-hosting device. `request` is a serialized
    /// `providers::Request`; the device streams `ModelDelta` then `ModelResponse`.
    ModelRequest { call_id: String, provider_id: String, request: Value },
    /// A streamed delta relayed back to a non-host device thread.
    ModelDelta { call_id: String, delta: Value },
    /// The final response (serialized `providers::Response`) or error relayed back
    /// to a non-host device thread.
    ModelResponse {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Stop work on a model call hosted by this device.
    ModelCancel { call_id: String },
    Pong,
    Error { message: String },
    /// The account's providers and keys, sent on connect and whenever they change.
    Providers { providers: Vec<ProviderConfig> },
    /// Run a subagent thread on this device.
    StartThread { spec: ThreadSpec },
    /// A message for a running thread, from the orchestrator or the user.
    ThreadInput { thread_id: String, from: String, text: String },
    StopThread { thread_id: String },
    ApprovalResult {
        approval_id: String,
        approved: bool,
        /// Set when the hub denied the call because nobody answered in time.
        /// Older devices ignore the field and treat it as an ordinary denial.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// Offline hand-over: a file the hub prepared for a thread while this device was
    /// away. Write it into that thread's workspace (`path` is workspace-relative, with
    /// `/`) before the thread's `StartThread`, which follows on the same connection.
    WorkspaceFile { thread_id: String, path: String, content_base64: String },
}

/// A tool definition as models see it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One of the account's model providers. `api_key` is only sent to devices.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProviderConfig {
    /// Account-chosen id, e.g. `anthropic`, `openrouter`.
    pub id: String,
    /// `anthropic`, `openai` (any OpenAI-compatible endpoint) or `scripted` (tests).
    pub kind: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Kind-specific options, e.g. `{"max_tokens_field": "max_completion_tokens"}`,
    /// `{"script": "/path/to/script.json"}`, or `{"via_device": "Gaming PC"}`.
    /// With `via_device`, `base_url` is interpreted on that device and all other
    /// nodes reach it through the device's outbound hub connection.
    #[serde(default)]
    pub options: Value,
}

impl ProviderConfig {
    /// Device id or name that hosts this provider, if configured.
    pub fn via_device(&self) -> Option<&str> {
        self.options.get("via_device").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    }

    /// A copy suitable for constructing the provider on its hosting device.
    pub fn without_via_device(&self) -> Self {
        let mut config = self.clone();
        if let Some(options) = config.options.as_object_mut() {
            options.remove("via_device");
        }
        config
    }
}

/// How a tool call on a device is gated.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Every call waits for the user's approval.
    Manual,
    /// Runs unless the call hits a risk rule, then asks.
    Auto,
    /// Always runs.
    Bypass,
}

impl Permission {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "auto" => Some(Self::Auto),
            "bypass" => Some(Self::Bypass),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }
    /// Starting level for a newly seen tool: Auto for workspace file tools and for
    /// the tools of the CLI-agent MCP servers ([`AGENT_MCP_SERVERS`]), Manual for
    /// everything else (other MCP tools included).
    pub fn default_for(tool: &str) -> Self {
        let agent = mcp_server_of(tool).is_some_and(|s| AGENT_MCP_SERVERS.contains(&s));
        if FILE_TOOLS.contains(&tool) || agent { Self::Auto } else { Self::Manual }
    }
}

/// Tools that only touch the thread's own workspace folder.
pub const FILE_TOOLS: &[&str] = &["read_file", "write_file", "list_files"];

/// Separator between an MCP server's name and its tool's: `codex__codex_chat`.
pub const MCP_SEPARATOR: &str = "__";

/// The subagent-chat servers that run CLI agents (and its catalog). Their tools start on Auto.
pub const AGENT_MCP_SERVERS: &[&str] =
    &["hermes", "antigravity", "claude-code", "codex", "pi", "opencode", "cursor", "grok", "catalog"];

/// The server part of an MCP host tool name (`codex` for `codex__codex_chat`).
pub fn mcp_server_of(tool: &str) -> Option<&str> {
    tool.split_once(MCP_SEPARATOR).map(|(server, _)| server).filter(|s| !s.is_empty())
}

/// Default time an MCP host tool may run, unless its `timeout_seconds` argument says otherwise.
pub const MCP_DEFAULT_TIMEOUT_SECS: u64 = 600;
/// Longest time an MCP host tool may run.
pub const MCP_MAX_TIMEOUT_SECS: u64 = 1800;
/// Added on top of a tool's own timeout before the device gives up on it.
pub const MCP_GRACE_SECS: u64 = 30;

/// How long a device waits for one of its MCP servers' tools: the call's
/// `timeout_seconds` argument (default 600 s, at most 1,800 s) plus a grace period.
/// The hub waits [`MCP_GRACE_SECS`] longer still, so the device's answer arrives first.
pub fn mcp_call_timeout(args: &Value) -> std::time::Duration {
    let asked = args.get("timeout_seconds").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)));
    let secs = asked.unwrap_or(MCP_DEFAULT_TIMEOUT_SECS).clamp(1, MCP_MAX_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs + MCP_GRACE_SECS)
}

/// Everything a device needs to run a subagent thread.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ThreadSpec {
    pub thread_id: String,
    pub conversation_id: String,
    pub title: String,
    pub provider_id: String,
    pub model: String,
    pub system: String,
    /// The brief, rendered as the thread's first user message.
    pub brief: String,
    pub permissions: HashMap<String, Permission>,
    pub max_turns: u32,
    /// The device's name as the hub knows it, for approval reasons (`writes to @docs on Laptop`).
    #[serde(default)]
    pub device_name: String,
    /// How long `ask_orchestrator` waits, in seconds. Missing on a spec from an older
    /// hub means [`DEFAULT_ASK_TIMEOUT_SECS`], so those devices keep today's 30 minutes.
    /// `null` waits forever.
    #[serde(default = "default_ask_timeout_secs")]
    pub ask_timeout_secs: Option<u64>,
}

/// How long `ask_orchestrator` waits when the account hasn't changed it.
pub const DEFAULT_ASK_TIMEOUT_SECS: u64 = 30 * 60;

fn default_ask_timeout_secs() -> Option<u64> {
    Some(DEFAULT_ASK_TIMEOUT_SECS)
}

/// What an agent loop did, reported as it happens.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// The model's full text for one turn, once the turn is complete.
    Text { text: String },
    /// A piece of text as the model generates it. The same text arrives again in
    /// the turn's [`AgentEvent::Text`], so a client that missed deltas loses nothing.
    TextDelta { text: String },
    ToolCall { call_id: String, name: String, input: Value },
    ToolResult { call_id: String, name: String, ok: bool, output: String },
    Usage {
        provider_id: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    },
    Notice { text: String },
}

/// One entry in a conversation's event log.
///
/// Streamed text ([`EventBody::AssistantDelta`], and [`EventBody::ThreadActivity`]
/// carrying [`AgentEvent::TextDelta`]) is live-only: it reaches clients on the event
/// stream but is never stored, so it has `seq` 0 and doesn't appear in history.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    /// Position in the conversation's log; 0 for live-only events.
    pub seq: i64,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub ts: u64,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBody {
    UserMessage { text: String },
    AssistantMessage { text: String },
    /// Part of an assistant message as it streams (live only, never stored). The
    /// complete text follows as an [`EventBody::AssistantMessage`].
    AssistantDelta { text: String },
    ThreadStarted { title: String, device_id: String, model: String, reason: Option<String>, brief: String },
    ThreadActivity { event: AgentEvent },
    ThreadInput { from: String, text: String },
    ThreadFinished { status: String, summary: String },
    /// The thread's device is offline (it was when the thread started, or it
    /// disconnected mid-thread). The thread waits for it; meanwhile the hub works
    /// towards a checkpoint in its own workspace.
    ThreadWaiting { device_id: String, reason: String },
    /// The hub's part of a waiting thread reached its checkpoint: `summary` is its
    /// checkpoint report. The thread keeps waiting for `device_id`.
    ThreadCheckpoint { device_id: String, summary: String },
    /// The device is back: the hub copied `files` from its workspace (and left out
    /// `skipped`, with reasons) and the thread continues there with `brief`.
    ThreadHandedOver { device_id: String, files: Vec<String>, skipped: Vec<String>, brief: String },
    ApprovalRequested { approval_id: String, device_id: String, tool: String, input: Value, reason: String },
    ApprovalResolved { approval_id: String, approved: bool },
    /// The orchestrator finished a turn; it wakes again on the next message or thread result.
    TurnFinished { running_threads: u32 },
    Error { message: String },
    /// Estimated spend crossed the warning fraction of a budget (see [`Limits`]): once per
    /// `scope` (`thread`, `conversation` or `daily`) and period. Amounts are USD.
    BudgetWarning { scope: String, spent: f64, limit: f64 },
}

/// `POST /v1/chat`
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatRequest {
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// `project` (orchestrator + threads, the default) or `standard` (one model).
    #[serde(default)]
    pub mode: Option<String>,
    pub text: String,
    /// Registry id for this conversation's orchestrator or standard model.
    /// Applied on the conversation's first message. Change it later with
    /// `POST /v1/conversations/{id}/model`.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChatResponse {
    pub conversation_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ConversationInfo {
    pub id: String,
    pub mode: String,
    pub title: String,
    pub created: u64,
    /// Registry id used instead of the role default, when this conversation has one.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ThreadInfo {
    pub id: String,
    pub conversation_id: String,
    pub title: String,
    pub device_id: String,
    pub model: String,
    /// `running`, `waiting_device` (its device is offline; see
    /// [`EventBody::ThreadWaiting`]), `done`, `failed` or `stopped`.
    pub status: String,
    /// Why it runs where it does, when that isn't its natural device.
    pub reason: Option<String>,
    pub summary: Option<String>,
    pub created: u64,
    pub finished: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// A row of the tool registry.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolInfo {
    pub device_id: String,
    pub name: String,
    pub description: String,
    pub permission: Permission,
    /// Whether the device offered this tool in its latest `tools/list`.
    #[serde(default = "default_true")]
    pub available: bool,
    /// JSON Schema for the tool's arguments (an object schema).
    #[serde(default = "default_input_schema")]
    pub input_schema: Value,
}

/// A row of the model registry.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModelEntry {
    /// Account-chosen id, e.g. `opus`, `free-llama`.
    pub id: String,
    pub provider_id: String,
    /// The provider's model name, e.g. `claude-opus-5-5`.
    pub model: String,
    /// USD per million tokens.
    #[serde(default)]
    pub input_price: f64,
    #[serde(default)]
    pub output_price: f64,
    /// USD per million tokens read from the provider's prompt cache.
    #[serde(default)]
    pub cache_read_price: Option<f64>,
    /// USD per million tokens written to the provider's prompt cache.
    #[serde(default)]
    pub cache_write_price: Option<f64>,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub use_cases: String,
    #[serde(default)]
    pub free: bool,
    /// Roles this model is the default for: `orchestrator`, `standard`, `thread`.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Runs on your own hardware, for privacy routing (`require: local` rules). A model
    /// whose provider's base_url is localhost, 127.0.0.1, ::1 or `*.local` counts as
    /// local even without this flag.
    #[serde(default)]
    pub local: bool,
    /// Input modalities from the provider's model card, e.g. `text`, `image`.
    /// Empty until a catalog import sets them.
    #[serde(default)]
    pub modalities: Vec<String>,
}

/// `POST /v1/models/import`. Pull one model from the provider's catalog.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModelImportRequest {
    pub provider: String,
    /// The provider's model name, e.g. `anthropic/claude-sonnet-4`.
    pub model: String,
    /// Registry id. Derived from `model` when omitted.
    #[serde(default)]
    pub id: Option<String>,
    /// Roles this model becomes the default for. Omitted keeps the current roles
    /// (none, on a new row).
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    /// When omitted, a new row is not local and an existing row keeps its flag.
    #[serde(default)]
    pub local: Option<bool>,
    /// Run the built-in probe suite after importing. Off by default (probes spend tokens).
    #[serde(default)]
    pub probe: bool,
}

/// Result of one deterministic probe task (`POST /v1/models/{id}/probe`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProbeResult {
    /// Short probe id, e.g. `code`, `writing`, `research`, `data`, `tool_use`.
    pub id: String,
    /// Registry task type recorded on the outcome row.
    pub task_type: String,
    pub passed: bool,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    /// Deterministic grader summary or provider error.
    pub detail: String,
}

/// Summary of running the built-in probe suite against one model (`POST /v1/models/{id}/probe`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProbeReport {
    pub model_id: String,
    pub passed: usize,
    pub total: usize,
    pub total_latency_ms: u64,
    pub total_tokens: u64,
    pub total_cost: f64,
    pub results: Vec<ProbeResult>,
    /// Pending use-case proposal created or updated from these results, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_text: Option<String>,
    /// Set when the suite stopped early (for example, after hitting the daily budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_reason: Option<String>,
}

/// Saved registry row plus the use-case proposal waiting for confirmation, if one was drafted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModelImportResult {
    pub model: ModelEntry,
    #[serde(default)]
    pub proposal_id: Option<i64>,
    /// Probe results when `probe: true` was requested on import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReport>,
    /// Why probing did not complete after the model was imported, if `probe: true` failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

/// Aggregated metered usage returned by `GET /v1/usage`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsageTotal {
    /// The day, provider, model, conversation or thread represented by this row.
    pub key: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost: f64,
}

impl UsageTotal {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// A provider-reported usage, spend or balance observation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsageSnapshot {
    pub account: String,
    pub provider: String,
    pub kind: String,
    pub window: String,
    #[serde(default)]
    pub used_pct: Option<f64>,
    #[serde(default)]
    pub used_amount: Option<f64>,
    #[serde(default)]
    pub limit_amount: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub resets_at: Option<u64>,
    pub source: String,
    #[serde(default)]
    pub device_id: Option<String>,
    pub fetched_at: u64,
}

/// One provider's result from the internal device [`USAGE_PROBE_TOOL`] call.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsageProbeEntry {
    /// Canonical provider name (`claude`, `codex`).
    pub provider: String,
    /// `ok`, `not_signed_in`, `expired`, or `unavailable`.
    pub status: String,
    /// Human-readable status detail when `status != "ok"` (never contains tokens or secrets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Normalized snapshots when `status == "ok"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<UsageSnapshot>,
}

/// JSON payload returned by the internal device [`USAGE_PROBE_TOOL`] call.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UsageProbeResponse {
    pub results: Vec<UsageProbeEntry>,
}

/// What the hub does when [`Limits::approval_timeout_secs`] elapses.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTimeoutAction {
    /// Resolve the approval as denied. The thread keeps going.
    #[default]
    Deny,
    /// Resolve the approval as denied and stop the thread.
    Stop,
}

impl ApprovalTimeoutAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Stop => "stop",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "deny" => Some(Self::Deny),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// Account limits (`GET`/`PUT /v1/limits`). `PUT` takes a JSON object with only the
/// keys to change; `null` turns one off.
///
/// Spending caps, running-thread caps and `approval_timeout_secs` are off (`None`)
/// until set. `ask_timeout_secs` defaults to [`DEFAULT_ASK_TIMEOUT_SECS`]; `None`
/// waits forever. `approval_timeout_action` defaults to deny.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Threads running at once across the account (hub parts of threads waiting for
    /// their device count too).
    #[serde(default)]
    pub max_running_threads: Option<u32>,
    /// Threads running at once on any one device (the hub included).
    #[serde(default)]
    pub max_running_threads_per_device: Option<u32>,
    /// Default cap for one thread. `start_thread` may ask for less, never more.
    #[serde(default)]
    pub thread_budget_usd: Option<f64>,
    /// Cap for one conversation: its orchestrator (or standard chat) and its threads.
    #[serde(default)]
    pub conversation_budget_usd: Option<f64>,
    /// Cap for all usage in one UTC day.
    #[serde(default)]
    pub daily_budget_usd: Option<f64>,
    /// Warn when spend reaches this fraction of a budget. `None` means [`DEFAULT_WARN_AT_FRACTION`].
    #[serde(default)]
    pub warn_at_fraction: Option<f64>,
    /// How long a pending approval waits, in seconds. `None` (the default) waits until
    /// the user answers or the thread stops.
    #[serde(default)]
    pub approval_timeout_secs: Option<u64>,
    /// What happens when [`Self::approval_timeout_secs`] elapses. `deny` (the default)
    /// leaves the thread running; `stop` stops it.
    #[serde(default)]
    pub approval_timeout_action: ApprovalTimeoutAction,
    /// How long `ask_orchestrator` waits, in seconds. `None` waits forever. Absent
    /// means [`DEFAULT_ASK_TIMEOUT_SECS`].
    #[serde(default = "default_ask_timeout_secs")]
    pub ask_timeout_secs: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_running_threads: None,
            max_running_threads_per_device: None,
            thread_budget_usd: None,
            conversation_budget_usd: None,
            daily_budget_usd: None,
            warn_at_fraction: None,
            approval_timeout_secs: None,
            approval_timeout_action: ApprovalTimeoutAction::Deny,
            ask_timeout_secs: default_ask_timeout_secs(),
        }
    }
}

pub const DEFAULT_WARN_AT_FRACTION: f64 = 0.8;

/// The keys of [`Limits`], as `PUT /v1/limits` and `grouper limits set` take them.
pub const LIMIT_KEYS: &[&str] = &[
    "max_running_threads",
    "max_running_threads_per_device",
    "thread_budget_usd",
    "conversation_budget_usd",
    "daily_budget_usd",
    "warn_at_fraction",
    "approval_timeout_secs",
    "approval_timeout_action",
    "ask_timeout_secs",
];

/// Response of `GET`/`PUT /v1/limits`: the settings and where things stand.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LimitsReport {
    pub limits: Limits,
    /// The warning fraction in force.
    pub warn_at_fraction: f64,
    /// Estimated spend so far in the current UTC day.
    pub spent_today_usd: f64,
    /// Unix seconds when the daily budget resets (next 00:00 UTC).
    pub day_resets_at: u64,
    /// Threads counted against `max_running_threads` now.
    pub running_threads: u32,
}

/// `GET /v1/hub`: who this hub is. Public; nothing private in it. `hub_id` is random,
/// made once per hub database, so a client cache can tell two hubs apart.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HubInfo {
    pub hub_id: String,
    pub hub_version: String,
    pub protocol_version: u32,
}

/// A provider as listed to clients (keys never leave the hub this way).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProviderInfo {
    pub id: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub has_key: bool,
    /// Device id or name that hosts the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_device: Option<String>,
}

/// Body of `POST /v1/devices/{id}/call`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCallRequest {
    pub tool: String,
    /// Any JSON value; sent to the device JSON-encoded.
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Response of `POST /v1/devices/{id}/call`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCallResponse {
    pub ok: bool,
    pub output: String,
}

/// Turn a hub base URL (`https://hub.example.com`) into its WebSocket URL.
pub fn connect_url(base: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let rest = if let Some(r) = base.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = base.strip_prefix("http://") {
        format!("ws://{r}")
    } else if base.starts_with("wss://") || base.starts_with("ws://") {
        base.to_string()
    } else {
        return Err(format!("hub URL must start with http(s):// or ws(s)://, got {base:?}"));
    };
    Ok(format!("{rest}{CONNECT_PATH}"))
}

/// A device token is `<device-id>.<secret>`.
pub fn split_token(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.trim().split_once('.')?;
    (!id.is_empty() && !secret.is_empty()).then_some((id, secret))
}

/// A long-term memory about the user or their projects, shared by every conversation
/// (`GET /v1/memories`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MemoryEntry {
    pub id: i64,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The conversation it was remembered in; `None` when added by the user directly.
    #[serde(default)]
    pub conversation_id: Option<String>,
    pub created: u64,
    pub updated: u64,
    /// When it was last put in front of a model.
    #[serde(default)]
    pub last_used: Option<u64>,
}

/// Body of `POST /v1/memories` and `PUT /v1/memories/{id}`. On update, `tags: None`
/// keeps the current tags.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct MemoryInput {
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

// ---- model registry learning: outcomes, stats, proposals, priority rules

fn default_outcome_source() -> String {
    "thread".into()
}

/// How threads or probes did, for one model or device and one task type (`GET /v1/registry/stats`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct OutcomeStats {
    /// Model id or device id.
    pub subject: String,
    /// `code`, `research`, …; empty for every task type together.
    pub task_type: String,
    /// Where these outcomes came from: `thread` or `probe`.
    #[serde(default = "default_outcome_source")]
    pub source: String,
    pub done: u64,
    pub failed: u64,
    pub stopped: u64,
    /// Done, not graded 1–2 by the orchestrator and not rated down by you.
    pub successes: u64,
    /// successes / (done + failed); stopped threads don't count. `None` before any.
    pub success_rate: Option<f64>,
    pub median_duration_s: Option<f64>,
    /// USD per thread (or per probe).
    pub mean_cost: Option<f64>,
    /// Mean tokens per outcome, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_tokens: Option<f64>,
    /// The orchestrator's 1–5 grades.
    pub mean_grade: Option<f64>,
    /// Your thumbs up minus thumbs down.
    pub net_rating: i64,
}

impl Default for OutcomeStats {
    fn default() -> Self {
        Self {
            subject: String::new(),
            task_type: String::new(),
            source: default_outcome_source(),
            done: 0,
            failed: 0,
            stopped: 0,
            successes: 0,
            success_rate: None,
            median_duration_s: None,
            mean_cost: None,
            mean_tokens: None,
            mean_grade: None,
            net_rating: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct RegistryStats {
    pub models: Vec<OutcomeStats>,
    pub devices: Vec<OutcomeStats>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<OutcomeStats>,
}

/// A change to a model's registry row that waits for your approval, so the table
/// doesn't drift (`GET /v1/registry/proposals`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RegistryProposal {
    pub id: i64,
    pub model_id: String,
    /// Only `use_cases` for now.
    pub field: String,
    /// The proposed new value.
    pub text: String,
    pub reason: String,
    /// `pending`, `accepted` or `rejected`.
    pub status: String,
    /// `stats` (the hub's automatic check), `orchestrator`, `catalog` (drafted from a model card), or `probe`.
    pub source: String,
    pub created: u64,
    pub decided: Option<u64>,
}

/// What a priority rule does. `when_*` say when it applies (all given must match;
/// none means always). `require` / `forbid` filter models by `local`, `remote`,
/// `free` or `paid`; `prefer` (`cheapest`, `local` or `free`) orders what's left.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_task_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer: Option<String>,
}

/// A rule you set for choosing models, e.g. "Sensitive data: local only"
/// (`GET /v1/registry/rules`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PriorityRule {
    pub id: i64,
    pub text: String,
    pub rule: RuleSpec,
    pub enabled: bool,
    pub created: u64,
}

/// Body of `POST /v1/registry/rules`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RuleInput {
    pub text: String,
    pub rule: RuleSpec,
}

// ---- skills

/// A procedure that worked, saved so similar tasks reuse it instead of re-planning
/// (`GET /v1/skills`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Skill {
    pub id: i64,
    pub name: String,
    pub description: String,
    /// Keywords that should bring it up.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Markdown: the steps, with which device and model handled each.
    pub steps: String,
    #[serde(default)]
    pub tools: Vec<String>,
    /// Models and devices it has worked with.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub devices: Vec<String>,
    pub success_count: u64,
    #[serde(default)]
    pub last_used: Option<u64>,
    pub created: u64,
    pub updated: u64,
}

/// Body of `PUT /v1/skills/{id}`: fields left out stay as they are; lists replace.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct SkillInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let m = NodeMessage::Hello { protocol_version: PROTOCOL_VERSION, client_version: "0.1.0".into() };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains(r#""type":"hello""#));
        assert_eq!(serde_json::from_str::<NodeMessage>(&s).unwrap(), m);

        let rpc = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let h = HubMessage::Mcp { message: rpc.clone() };
        let s = serde_json::to_string(&h).unwrap();
        assert_eq!(s, format!(r#"{{"type":"mcp","message":{rpc}}}"#));
        assert_eq!(serde_json::from_str::<HubMessage>(&s).unwrap(), h);
        let n = NodeMessage::Mcp { message: rpc };
        assert_eq!(serde_json::from_str::<NodeMessage>(&serde_json::to_string(&n).unwrap()).unwrap(), n);

        let ack = NodeMessage::WorkspaceFileResult {
            thread_id: "t1".into(),
            path: "a/b.txt".into(),
            ok: false,
            error: Some("denied".into()),
        };
        let s = serde_json::to_string(&ack).unwrap();
        assert!(s.contains(r#""type":"workspace_file_result""#), "{s}");
        assert_eq!(serde_json::from_str::<NodeMessage>(&s).unwrap(), ack);
    }

    #[test]
    fn mcp_permissions_and_timeouts() {
        assert_eq!(Permission::default_for("codex__codex_chat"), Permission::Auto);
        assert_eq!(Permission::default_for("claude-code__claude_code_chat"), Permission::Auto);
        assert_eq!(Permission::default_for("hermes__hermes_chat"), Permission::Auto);
        assert_eq!(Permission::default_for("grok__grok_chat"), Permission::Auto);
        assert_eq!(Permission::default_for("opencode__opencode_chat"), Permission::Auto);
        assert_eq!(Permission::default_for("pi__pi_chat"), Permission::Auto);
        assert_eq!(Permission::default_for("hermes__list_hermes_chats"), Permission::Auto);
        assert_eq!(Permission::default_for("catalog__list_available_agents"), Permission::Auto);
        assert_eq!(Permission::default_for("git__git_commit"), Permission::Manual);
        assert_eq!(Permission::default_for("read_file"), Permission::Auto);
        assert_eq!(Permission::default_for("run_shell"), Permission::Manual);
        assert_eq!(Permission::default_for("codex_chat"), Permission::Manual);
        assert_eq!(mcp_server_of("codex__codex_chat"), Some("codex"));
        assert_eq!(mcp_server_of("__x"), None);
        let secs = |v: Value| mcp_call_timeout(&v).as_secs();
        assert_eq!(secs(serde_json::json!({})), 600 + MCP_GRACE_SECS);
        assert_eq!(secs(serde_json::json!({"timeout_seconds": 60})), 60 + MCP_GRACE_SECS);
        assert_eq!(secs(serde_json::json!({"timeout_seconds": 99999})), 1800 + MCP_GRACE_SECS);
        assert_eq!(secs(serde_json::json!({"timeout_seconds": 12.5})), 12 + MCP_GRACE_SECS);
    }

    #[test]
    fn capability_schemas_and_names() {
        let c: Capability = serde_json::from_str(r#"{"name":"mac_info","description":"d"}"#).unwrap();
        assert_eq!(c.input_schema, default_input_schema());
        assert!(valid_tool_name("mac_info"));
        assert!(valid_tool_name("codex__codex_chat"));
        assert!(valid_tool_name("a-b_C9"));
        assert!(!valid_tool_name("mac.info"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name(&"a".repeat(65)));
        assert!(!valid_tool_name("has space"));
    }

    #[test]
    fn folders_on_the_wire() {
        assert_eq!(FolderMode::parse("RW"), Some(FolderMode::ReadWrite));
        assert_eq!(FolderMode::parse("ro"), Some(FolderMode::Read));
        assert_eq!(FolderMode::parse("all"), None);
        let f = FolderInfo { name: "projects".into(), mode: FolderMode::ReadWrite };
        assert_eq!(serde_json::to_value(&f).unwrap(), serde_json::json!({"name": "projects", "mode": "read_write"}));
        assert!(valid_folder_name("my-docs_2") && !valid_folder_name("a/b") && !valid_folder_name(""));
        // Older hubs and devices: no folders, no device name.
        let d: DeviceInfo = serde_json::from_str(
            r#"{"id":"d","name":"n","platform":"p","online":true,"capabilities":[],"last_seen":null}"#,
        )
        .unwrap();
        assert!(d.folders.is_empty());
        assert!(!serde_json::to_string(&d).unwrap().contains("folders"));
    }

    #[test]
    fn urls() {
        assert_eq!(connect_url("https://hub.example.com/").unwrap(), "wss://hub.example.com/v1/connect");
        assert_eq!(connect_url("http://127.0.0.1:8080").unwrap(), "ws://127.0.0.1:8080/v1/connect");
        assert!(connect_url("hub.example.com").is_err());
    }

    #[test]
    fn events_flatten() {
        let e = Event {
            seq: 1,
            conversation_id: "c".into(),
            thread_id: None,
            ts: 0,
            body: EventBody::AssistantMessage { text: "hi".into() },
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""kind":"assistant_message""#));
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);

        let d = Event {
            seq: 0,
            conversation_id: "c".into(),
            thread_id: Some("t1".into()),
            ts: 0,
            body: EventBody::ThreadActivity { event: AgentEvent::TextDelta { text: "par".into() } },
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(s.contains(r#""kind":"thread_activity","event":{"type":"text_delta","text":"par"}"#), "{s}");
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), d);
    }

    #[test]
    fn tokens() {
        assert_eq!(split_token("abc.def"), Some(("abc", "def")));
        assert_eq!(split_token("abc"), None);
        assert_eq!(split_token(".def"), None);
    }

    #[test]
    fn ask_timeout_defaults_when_an_older_spec_omits_it() {
        let raw = r#"{"thread_id":"t","conversation_id":"c","title":"T","provider_id":"p","model":"m","system":"","brief":"b","permissions":{},"max_turns":4}"#;
        let spec: ThreadSpec = serde_json::from_str(raw).unwrap();
        assert_eq!(spec.ask_timeout_secs, Some(DEFAULT_ASK_TIMEOUT_SECS));
        let forever = serde_json::json!({
            "thread_id": "t", "conversation_id": "c", "title": "T", "provider_id": "p", "model": "m",
            "system": "", "brief": "b", "permissions": {}, "max_turns": 4, "ask_timeout_secs": null
        });
        assert_eq!(serde_json::from_value::<ThreadSpec>(forever).unwrap().ask_timeout_secs, None);
        let custom = serde_json::json!({
            "thread_id": "t", "conversation_id": "c", "title": "T", "provider_id": "p", "model": "m",
            "system": "", "brief": "b", "permissions": {}, "max_turns": 4, "ask_timeout_secs": 12
        });
        assert_eq!(serde_json::from_value::<ThreadSpec>(custom).unwrap().ask_timeout_secs, Some(12));

        let old = r#"{"type":"approval_result","approval_id":"a","approved":false}"#;
        assert_eq!(
            serde_json::from_str::<HubMessage>(old).unwrap(),
            HubMessage::ApprovalResult { approval_id: "a".into(), approved: false, timeout_secs: None }
        );
        let timed =
            HubMessage::ApprovalResult { approval_id: "a".into(), approved: false, timeout_secs: Some(60) };
        let s = serde_json::to_string(&timed).unwrap();
        assert!(s.contains(r#""timeout_secs":60"#), "{s}");
        assert_eq!(serde_json::from_str::<HubMessage>(&s).unwrap(), timed);
        let plain = HubMessage::ApprovalResult { approval_id: "a".into(), approved: true, timeout_secs: None };
        assert!(!serde_json::to_string(&plain).unwrap().contains("timeout_secs"));
    }

    #[test]
    fn limit_defaults_keep_today_ask_wait_and_no_approval_timeout() {
        let l: Limits = serde_json::from_str("{}").unwrap();
        assert_eq!(l, Limits::default());
        assert_eq!(l.ask_timeout_secs, Some(DEFAULT_ASK_TIMEOUT_SECS));
        assert_eq!(l.approval_timeout_secs, None);
        assert_eq!(l.approval_timeout_action, ApprovalTimeoutAction::Deny);
        let set: Limits = serde_json::from_str(
            r#"{"ask_timeout_secs":null,"approval_timeout_secs":30,"approval_timeout_action":"stop"}"#,
        )
        .unwrap();
        assert_eq!(set.ask_timeout_secs, None);
        assert_eq!(set.approval_timeout_secs, Some(30));
        assert_eq!(set.approval_timeout_action, ApprovalTimeoutAction::Stop);
    }

    #[test]
    fn presence_omits_input_schemas() {
        let msg = HubMessage::Presence {
            devices: vec![DeviceInfo {
                id: "dev1".into(),
                name: "Laptop".into(),
                platform: "windows".into(),
                online: true,
                capabilities: vec![ToolSummary {
                    name: "codex__codex_chat".into(),
                    description: "Chat with Codex".into(),
                }],
                last_seen: Some(123),
                folders: vec![],
            }],
        };
        let value = serde_json::to_value(&msg).unwrap();
        let cap = &value["devices"][0]["capabilities"][0];
        assert_eq!(cap["name"], "codex__codex_chat");
        assert_eq!(cap["description"], "Chat with Codex");
        assert!(cap.get("input_schema").is_none(), "presence must not carry input schemas: {cap}");
        assert_eq!(serde_json::from_value::<HubMessage>(value).unwrap(), msg);

        let info: ToolInfo = serde_json::from_str(
            r#"{"device_id":"dev1","name":"echo","description":"d","permission":"manual"}"#,
        )
        .unwrap();
        assert!(info.available);
        assert_eq!(info.input_schema, default_input_schema());
    }
}
