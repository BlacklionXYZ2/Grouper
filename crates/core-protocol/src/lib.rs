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
pub const PROTOCOL_VERSION: u32 = 4;

/// WebSocket endpoint for devices, relative to the hub's base URL.
pub const CONNECT_PATH: &str = "/v1/connect";

/// WebSocket endpoint streaming conversation [`Event`]s to clients.
pub const EVENTS_PATH: &str = "/v1/events";

/// How often a device sends [`NodeMessage::Ping`].
pub const PING_INTERVAL_SECS: u64 = 20;

/// The hub drops a connection that has been silent for this long.
pub const IDLE_TIMEOUT_SECS: u64 = 60;

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

/// A registered device as the hub sees it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    /// `macos`, `ios`, `windows`, `linux`, …
    pub platform: String,
    pub online: bool,
    /// Capabilities reported on the device's latest connection.
    pub capabilities: Vec<Capability>,
    /// Unix seconds when the device was last heard from, if ever.
    pub last_seen: Option<u64>,
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
    Ping,
    /// Something happened in a thread running on this device.
    ThreadEvent { thread_id: String, event: AgentEvent },
    ThreadFinished { thread_id: String, ok: bool, summary: String },
    /// A thread wants to run a tool that needs the user's approval.
    ApprovalRequest { approval_id: String, thread_id: String, tool: String, input: Value, reason: String },
    /// A thread needs something from the orchestrator (often another device's data).
    AskOrchestrator { thread_id: String, question: String },
}

/// Hub → device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMessage {
    Welcome {
        device_id: String,
        hub_version: String,
        protocol_version: u32,
    },
    /// Full device list, sent on connect and whenever any device comes or goes.
    Presence { devices: Vec<DeviceInfo> },
    /// One MCP JSON-RPC message from the hub's MCP client to the device's server
    /// (`initialize`, `tools/list`, `tools/call`, `notifications/cancelled`, …).
    Mcp { message: Value },
    Pong,
    Error { message: String },
    /// The account's providers and keys, sent on connect and whenever they change.
    Providers { providers: Vec<ProviderConfig> },
    /// Run a subagent thread on this device.
    StartThread { spec: ThreadSpec },
    /// A message for a running thread, from the orchestrator or the user.
    ThreadInput { thread_id: String, from: String, text: String },
    StopThread { thread_id: String },
    ApprovalResult { approval_id: String, approved: bool },
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
    /// `{"script": "/path/to/script.json"}`, `{"fallbacks": false}`.
    #[serde(default)]
    pub options: Value,
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
    ApprovalRequested { approval_id: String, device_id: String, tool: String, input: Value, reason: String },
    ApprovalResolved { approval_id: String, approved: bool },
    /// The orchestrator finished a turn; it wakes again on the next message or thread result.
    TurnFinished { running_threads: u32 },
    Error { message: String },
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
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ThreadInfo {
    pub id: String,
    pub conversation_id: String,
    pub title: String,
    pub device_id: String,
    pub model: String,
    /// `running`, `done`, `failed` or `stopped`.
    pub status: String,
    /// Why it runs where it does, when that isn't its natural device.
    pub reason: Option<String>,
    pub summary: Option<String>,
    pub created: u64,
    pub finished: Option<u64>,
}

/// A row of the tool registry.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolInfo {
    pub device_id: String,
    pub name: String,
    pub description: String,
    pub permission: Permission,
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

/// A provider as listed to clients (keys never leave the hub this way).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProviderInfo {
    pub id: String,
    pub kind: String,
    pub base_url: Option<String>,
    pub has_key: bool,
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

/// How threads did, for one model or device and one task type (`GET /v1/registry/stats`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct OutcomeStats {
    /// Model id or device id.
    pub subject: String,
    /// `code`, `research`, …; empty for every task type together.
    pub task_type: String,
    pub done: u64,
    pub failed: u64,
    pub stopped: u64,
    /// Done, not graded 1–2 by the orchestrator and not rated down by you.
    pub successes: u64,
    /// successes / (done + failed); stopped threads don't count. `None` before any.
    pub success_rate: Option<f64>,
    pub median_duration_s: Option<f64>,
    /// USD per thread.
    pub mean_cost: Option<f64>,
    /// The orchestrator's 1–5 grades.
    pub mean_grade: Option<f64>,
    /// Your thumbs up minus thumbs down.
    pub net_rating: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct RegistryStats {
    pub models: Vec<OutcomeStats>,
    pub devices: Vec<OutcomeStats>,
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
    /// `stats` (the hub's automatic check) or `orchestrator`.
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
    }

    #[test]
    fn mcp_permissions_and_timeouts() {
        assert_eq!(Permission::default_for("codex__codex_chat"), Permission::Auto);
        assert_eq!(Permission::default_for("claude-code__claude_code_chat"), Permission::Auto);
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
}
