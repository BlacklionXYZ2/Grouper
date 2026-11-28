mod credentials;
mod hub;
mod usage;

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use core_client::core_protocol::{
    AgentEvent, Capability, ChatRequest, ChatResponse, ConversationInfo, DeviceInfo, Event, EventBody, ModelEntry,
    ModelImportRequest, ModelImportResult, ProviderConfig, ProviderInfo, ThreadInfo, ToolCallRequest, ToolCallResponse,
    ToolInfo, WorkspaceListing, default_input_schema,
};
use core_client::{Events, Status, ToolHandler};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Grouper command-line client.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Hub base URL.
    ///
    /// Order: this flag, then GROUPER_HUB, then the hub saved by `grouper login`,
    /// then https://grouper.clockmillbarn.uk.
    #[arg(long, env = "GROUPER_HUB", global = true)]
    hub: Option<String>,
    /// A device token, instead of the one `grouper login` saved (for automation and recovery).
    #[arg(long, env = "GROUPER_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    /// Local cache for `grouper sync` and for reading offline. Default: <local data dir>/grouper/cache.db.
    #[arg(long, env = "GROUPER_CACHE", global = true)]
    cache: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign this computer in: shows a code and a link to approve it in a browser where you're
    /// signed in to Grouper (with Google), then saves the hub URL and this device's own token.
    /// Signing in again from the same computer reuses its device.
    Login {
        /// The name this computer shows in Devices (default: its hostname).
        #[arg(long)]
        name: Option<String>,
        /// Don't try to open the link in a browser; just print it.
        #[arg(long)]
        no_browser: bool,
    },
    /// Forget the saved hub URL and token on this computer (it stays listed in Devices until you revoke it).
    Logout,
    /// Connect this machine as a device: stay online and run threads the orchestrator starts here.
    Connect {
        /// Where thread workspaces go.
        #[arg(long, env = "GROUPER_WORKSPACE")]
        workspace: Option<PathBuf>,
        /// Don't offer run_shell to threads.
        #[arg(long)]
        no_shell: bool,
        /// MCP servers to run here (Claude-style mcp.json). Default: <config dir>/grouper/mcp.json.
        #[arg(long, env = "GROUPER_MCP_CONFIG")]
        mcp_config: Option<PathBuf>,
        /// A scoped folder threads may reach as `@name/…`: name=path[:ro|rw] (read-only unless :rw).
        /// Repeatable; adds to (or replaces by name) the folders file.
        #[arg(long = "folder", value_name = "NAME=PATH[:ro|rw]")]
        folders: Vec<String>,
        /// Scoped folders file. Default: <config dir>/grouper/folders.json.
        #[arg(long, env = "GROUPER_FOLDERS_CONFIG")]
        folders_config: Option<PathBuf>,
        /// Read-only subscription usage probes, ON by default for Claude Code, Codex, Grok and
        /// Antigravity (usage belongs to the whole account). `--usage-probes=claude,grok` limits
        /// them to those providers; a bare `--usage-probes` means all. They use the CLIs' local
        /// sign-ins; tokens stay on this device and are never sent to the hub. Switch off with
        /// `--no-usage-probes`. This can only be set here, never from another device.
        #[arg(
            long,
            env = "GROUPER_USAGE_PROBES",
            value_name = "PROVIDERS",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "all",
            value_parser = core_client::usage_probes::UsageProbeSetting::from_flag
        )]
        usage_probes: Option<core_client::usage_probes::UsageProbeSetting>,
        /// Don't read subscription usage on this device at all.
        #[arg(long, conflicts_with = "usage_probes")]
        no_usage_probes: bool,
    },
    /// List a thread's workspace files, or download one: grouper files <thread> [<path> [-o <out>]]
    Files {
        thread: String,
        path: Option<String>,
        /// Where to save the file (default: its name in the current folder; `-` for stdout).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Talk to Grouper. Prints replies and thread activity until the turn and its threads finish.
    Chat {
        text: String,
        /// Continue an existing conversation.
        #[arg(long, short)]
        conversation: Option<String>,
        /// One model, no subagents (defaults to a free model).
        #[arg(long)]
        standard: bool,
        /// Registry id for this conversation's model (first message, or a new conversation).
        #[arg(long)]
        model: Option<String>,
    },
    /// Print live events (all conversations, or one).
    Watch {
        #[arg(long, short)]
        conversation: Option<String>,
    },
    /// List conversations, or set the model one of them uses.
    Conversations {
        #[command(subcommand)]
        cmd: Option<ConversationsCmd>,
    },
    /// Print a conversation's events so far.
    Log {
        conversation: String,
        #[arg(long)]
        thread: Option<String>,
    },
    /// List threads (optionally for one conversation).
    Threads {
        #[arg(long, short)]
        conversation: Option<String>,
    },
    /// Copy conversations, threads and events (and memories, with an admin token) into the
    /// local cache. `log`, `conversations` and `threads` read it when the hub is unreachable.
    Sync,
    /// Send a message into a running thread (the orchestrator is told).
    Say { thread: String, text: String },
    /// Stop a running thread.
    Stop { thread: String },
    /// Approve a pending tool call.
    Approve { approval: String },
    /// Deny a pending tool call.
    Deny { approval: String },
    /// List devices, their status and capabilities.
    Devices,
    /// Call a device capability directly: grouper call <device-id> <tool> ['<json args>']
    Call {
        device: String,
        tool: String,
        #[arg(default_value = "{}")]
        args: String,
    },
    /// Provider keys for the account (synced to every device).
    #[command(subcommand)]
    Keys(KeysCmd),
    /// The tool registry and permission levels.
    #[command(subcommand)]
    Tools(ToolsCmd),
    /// The model registry.
    #[command(subcommand)]
    Models(ModelsCmd),
    /// Metered token/cost totals and provider-reported balances.
    Usage(usage::UsageArgs),
    /// Long-term memory about you and your projects, shared by every conversation.
    #[command(subcommand)]
    Memory(MemoryCmd),
    /// Rate a finished thread's result: grouper rate <thread> up|down|clear
    Rate { thread: String, rating: String },
    /// Priority rules for choosing models, e.g. "Sensitive data: local only".
    #[command(subcommand)]
    Rules(RulesCmd),
    /// Saved skills: procedures that worked, reused for similar tasks.
    #[command(subcommand)]
    Skills(SkillsCmd),
    /// Limits on running threads, estimated spending, and how long an approval or
    /// ask_orchestrator waits. Spending and approval limits are off unless you set them.
    Limits {
        #[command(subcommand)]
        cmd: Option<LimitsCmd>,
    },
    /// Run and manage a local hub on this machine (Linux / headless host).
    #[command(subcommand)]
    Hub(hub::HubCmd),
}

#[derive(Subcommand)]
enum LimitsCmd {
    /// grouper limits set <key> <value|off>, e.g. `daily_budget_usd 5`, `max_running_threads 3`,
    /// `approval_timeout_secs 600`, `approval_timeout_action stop`, `ask_timeout_secs off`.
    /// Keys: max_running_threads, max_running_threads_per_device, thread_budget_usd,
    /// conversation_budget_usd, daily_budget_usd, warn_at_fraction (default 0.8),
    /// approval_timeout_secs (off waits forever), approval_timeout_action (deny or stop),
    /// ask_timeout_secs (default 1800 seconds; off waits forever).
    Set { key: String, value: String },
}

#[derive(Subcommand)]
enum RulesCmd {
    List,
    /// grouper rules add "Sensitive data: local only" --when-tag sensitive --require local
    Add {
        text: String,
        /// Applies when a thread has this tag
        #[arg(long)]
        when_tag: Option<String>,
        /// Applies to this task type (code, research, writing, data, ops)
        #[arg(long)]
        when_task_type: Option<String>,
        /// Only models that are: local, remote, free or paid
        #[arg(long)]
        require: Option<String>,
        /// No models that are: local, remote, free or paid
        #[arg(long)]
        forbid: Option<String>,
        /// Order models: cheapest, local or free
        #[arg(long)]
        prefer: Option<String>,
    },
    Remove { id: i64 },
    Enable { id: i64 },
    Disable { id: i64 },
}

#[derive(Subcommand)]
enum SkillsCmd {
    /// List skills, most recently changed first, or search them.
    List {
        #[arg(long, short)]
        search: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Print a skill's steps and where it worked.
    Show { id: i64 },
    /// Delete a skill.
    Forget { id: i64 },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// List memories, most recently changed first, or search them.
    List {
        #[arg(long, short)]
        search: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Save a memory: grouper memory add "<text>" [--tags a,b]
    Add {
        text: String,
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
    },
    /// Replace a memory's text (and tags, if given).
    Edit {
        id: i64,
        text: String,
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<String>>,
    },
    /// Delete a memory.
    Forget { id: i64 },
}

#[derive(Subcommand)]
enum KeysCmd {
    /// List providers (keys are never shown).
    List,
    /// Add or update a provider. The key is read from GROUPER_PROVIDER_KEY or stdin.
    Set {
        provider: String,
        /// anthropic, openai (any OpenAI-compatible API) or scripted
        #[arg(long, default_value = "openai")]
        kind: String,
        #[arg(long)]
        base_url: Option<String>,
        /// Host this provider on a device (id or name); its base URL is local to that device.
        #[arg(long)]
        via_device: Option<String>,
        /// JSON options, e.g. '{"script": "/path/script.json"}'
        #[arg(long)]
        options: Option<String>,
        /// Don't ask for a key (keyless providers, or keeping the current key).
        #[arg(long)]
        no_key: bool,
    },
}

#[derive(Subcommand)]
enum ToolsCmd {
    /// List tools per device with their permission level.
    List {
        #[arg(long)]
        device: Option<String>,
        /// Include tools a device no longer offers (their saved permissions are kept).
        #[arg(long)]
        all: bool,
    },
    /// Set a tool's permission: manual, auto or bypass.
    Set { device: String, tool: String, permission: String },
}

#[derive(Subcommand)]
enum ModelsCmd {
    List,
    /// Add or update a model.
    Add {
        id: String,
        #[arg(long)]
        provider: String,
        /// The provider's model name, e.g. claude-opus-5-5
        #[arg(long)]
        model: String,
        /// Comma-separated defaults: orchestrator, standard, thread
        #[arg(long, default_value = "")]
        roles: String,
        #[arg(long)]
        free: bool,
        /// USD per million input tokens
        #[arg(long, default_value_t = 0.0)]
        input_price: f64,
        /// USD per million output tokens
        #[arg(long, default_value_t = 0.0)]
        output_price: f64,
        /// USD per million cached input tokens (provider default when omitted)
        #[arg(long)]
        cache_read_price: Option<f64>,
        /// USD per million cache-write tokens (provider default when omitted)
        #[arg(long)]
        cache_write_price: Option<f64>,
        #[arg(long, default_value_t = 0)]
        context_window: u64,
        #[arg(long, default_value = "")]
        use_cases: String,
        /// Runs on your own hardware (for "sensitive → local" priority rules)
        #[arg(long)]
        local: bool,
    },
    /// Pull a model from the provider's catalog (OpenRouter) and draft its use cases.
    Import {
        provider: String,
        /// The provider's model name, e.g. anthropic/claude-sonnet-4
        model: String,
        /// Registry id. Derived from the model name when omitted.
        #[arg(long)]
        id: Option<String>,
        /// Comma-separated defaults: orchestrator, standard, thread
        #[arg(long, default_value = "")]
        roles: String,
        /// Runs on your own hardware (for "sensitive → local" priority rules)
        #[arg(long)]
        local: bool,
        /// Run the built-in probe suite after importing (spends tokens).
        #[arg(long)]
        probe: bool,
    },
    /// Run the built-in deterministic probe suite against a registered model and draft use cases.
    Probe { id: String },
    /// Sync chat-capable models from a provider (or all API providers when omitted).
    Sync {
        /// Provider id to sync. Omit to sync all configured API providers.
        provider: Option<String>,
    },
    Remove { id: String },
    /// How threads went per model and device, by task type.
    Stats,
    /// Use-case changes the hub or orchestrator proposed, waiting for you.
    Proposals {
        /// Include accepted and rejected ones.
        #[arg(long)]
        all: bool,
    },
    /// Accept a proposal: the model's use cases become the proposed text.
    Accept { id: i64 },
    Reject { id: i64 },
}

#[derive(Subcommand)]
enum ConversationsCmd {
    /// Set the model for a conversation: `grouper conversations model <id> <model|default>`.
    Model {
        id: String,
        /// A registry id, or `default` to use the role default again.
        model: String,
    },
}

struct CliTools;
impl ToolHandler for CliTools {
    fn call(&self, tool: &str, args: &str) -> Result<String, String> {
        match tool {
            "echo" => Ok(args.to_string()),
            "system_info" => Ok(json!({
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "hostname": hostname(),
                "client": "grouper-cli",
                "version": core_client::CLIENT_VERSION,
            })
            .to_string()),
            _ => Err(format!("unknown tool {tool}")),
        }
    }
}

pub(crate) fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

struct PrintEvents;
impl Events for PrintEvents {
    fn status(&self, s: Status) {
        println!("status: {s:?}");
    }
    fn presence(&self, devices: Vec<DeviceInfo>) {
        print_devices(&devices);
    }
    fn thread(&self, thread_id: &str, title: &str, state: &str) {
        println!("thread {thread_id} \"{title}\": {state}");
    }
}

fn print_devices(devices: &[DeviceInfo]) {
    println!("devices:");
    for d in devices {
        let tools: Vec<_> = d.capabilities.iter().map(|c| c.name.as_str()).collect();
        println!(
            "  {:<12} {:<20} {:<8} {:<7} [{}]",
            d.id,
            d.name,
            d.platform,
            if d.online { "online" } else { "offline" },
            tools.join(", ")
        );
        if !d.folders.is_empty() {
            let folders: Vec<String> = d.folders.iter().map(|f| format!("@{} ({})", f.name, f.mode.as_str())).collect();
            println!("  {:<12} scoped folders: {}", "", folders.join(", "));
        }
    }
}

/// Percent-encode one URL path segment.
fn url_segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub(crate) struct Api {
    base: String,
    token: String,
    /// Sent as `Grouper-Relay` when the hub address is a relay (see docs/relay-design.md).
    relay_token: Option<String>,
    http: reqwest::Client,
}

/// An HTTP client that sends `Grouper-Relay: <token>` on every request when there is one.
fn http_client(relay_token: Option<&str>, timeout: Option<std::time::Duration>) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(t) = relay_token.map(str::trim).filter(|t| !t.is_empty()) {
        let mut v = reqwest::header::HeaderValue::from_str(t).context("the relay token isn't a valid header value")?;
        v.set_sensitive(true);
        headers.insert("grouper-relay", v);
    }
    let mut b = reqwest::Client::builder().default_headers(headers);
    if let Some(t) = timeout {
        b = b.timeout(t);
    }
    Ok(b.build()?)
}

impl Api {
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req.bearer_auth(&self.token).send().await.context("can't reach the hub")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = body.get("error").and_then(Value::as_str).map(String::from).unwrap_or_else(|| body.to_string());
            bail!("{status}: {msg}");
        }
        Ok(body)
    }
    pub(crate) async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(serde_json::from_value(self.send(self.http.get(format!("{}{path}", self.base))).await?)?)
    }
    pub(crate) async fn post(&self, path: &str, body: &impl serde::Serialize) -> Result<Value> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body)).await
    }
    async fn put(&self, path: &str, body: &impl serde::Serialize) -> Result<Value> {
        self.send(self.http.put(format!("{}{path}", self.base)).json(body)).await
    }
    /// A raw (non-JSON) body, such as a downloaded file.
    async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("can't reach the hub")?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            let msg = body.get("error").and_then(Value::as_str).map(String::from).unwrap_or_else(|| body.to_string());
            bail!("{status}: {msg}");
        }
        Ok(resp.bytes().await?.to_vec())
    }
    async fn delete(&self, path: &str) -> Result<Value> {
        self.send(self.http.delete(format!("{}{path}", self.base))).await
    }

    async fn events(
        &self,
        conversation: Option<&str>,
    ) -> Result<impl futures_util::Stream<Item = Event> + use<>> {
        let mut url = core_client::core_protocol::connect_url(&self.base)
            .map_err(anyhow::Error::msg)?
            .replace(core_client::core_protocol::CONNECT_PATH, core_client::core_protocol::EVENTS_PATH);
        if let Some(c) = conversation {
            url.push_str(&format!("?conversation={c}"));
        }
        let mut req = url.into_client_request()?;
        req.headers_mut().insert("authorization", format!("Bearer {}", self.token).parse()?);
        if let Some(relay) = &self.relay_token {
            req.headers_mut().insert("grouper-relay", relay.trim().parse()?);
        }
        let mut ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        ws_config.max_message_size = Some(core_client::core_protocol::MAX_WS_MESSAGE_BYTES);
        ws_config.max_frame_size = Some(core_client::core_protocol::MAX_WS_MESSAGE_BYTES);
        let (ws, _) = tokio_tungstenite::connect_async_with_config(req, Some(ws_config), false)
            .await
            .context("can't open the event stream")?;
        Ok(ws.filter_map(|m| async move {
            match m {
                Ok(Message::Text(t)) => serde_json::from_str::<Event>(&t).ok(),
                _ => None,
            }
        }))
    }
}

fn short(v: &Value, max: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let s = s.replace('\n', " ⏎ ");
    if s.chars().count() > max { format!("{}…", s.chars().take(max).collect::<String>()) } else { s }
}

fn print_event(e: &Event) {
    let t = e.thread_id.as_deref().unwrap_or("");
    match &e.body {
        EventBody::UserMessage { text } => println!("\nyou: {text}"),
        EventBody::AssistantMessage { text } => println!("\ngrouper: {text}"),
        // Streamed pieces; `Printer` handles these when they arrive live.
        EventBody::AssistantDelta { text } => print!("{text}"),
        EventBody::ThreadStarted { title, device_id, model, reason, .. } => {
            let why = reason.as_ref().map(|r| format!(" ({r})")).unwrap_or_default();
            println!("  ▶ thread {t} \"{title}\" started on {device_id} with {model}{why}");
        }
        EventBody::ThreadActivity { event } => match event {
            AgentEvent::Text { text } => println!("    [{t}] {}", short(&json!(text), 200)),
            AgentEvent::TextDelta { text } => print!("{text}"),
            AgentEvent::ToolCall { name, input, .. } => println!("    [{t}] → {name} {}", short(input, 120)),
            AgentEvent::ToolResult { name, ok, output, .. } => {
                println!("    [{t}] {} {name}: {}", if *ok { "✓" } else { "✗" }, short(&json!(output), 120))
            }
            AgentEvent::Usage { model, input_tokens, output_tokens, .. } => {
                println!("    [{t}] ({model}: {input_tokens} in / {output_tokens} out)")
            }
            AgentEvent::Notice { text } => println!("    [{t}] {text}"),
        },
        EventBody::ThreadInput { from, text } => println!("    [{t}] ← {from}: {text}"),
        EventBody::ThreadFinished { status, summary } => {
            println!("  ■ thread {t} {status}: {}", short(&json!(summary), 300))
        }
        EventBody::ThreadWaiting { device_id, reason } => {
            println!("  ⏸ thread {t} waiting for {device_id} ({reason}); the hub prepares a checkpoint")
        }
        EventBody::ThreadCheckpoint { device_id, summary } => {
            println!("  ◆ thread {t} hub checkpoint, waiting for {device_id}: {}", short(&json!(summary), 300))
        }
        EventBody::ThreadHandedOver { device_id, files, skipped, .. } => {
            let left_out = if skipped.is_empty() { String::new() } else { format!(", {} not copied", skipped.len()) };
            println!("  ▶ thread {t} handed over to {device_id} with {} file(s) from the hub{left_out}", files.len())
        }
        EventBody::ApprovalRequested { approval_id, device_id, tool, input, reason } => {
            println!("  ! approval {approval_id} on {device_id}: {tool} {} — {reason}", short(input, 160));
            println!("    run `grouper approve {approval_id}` or `grouper deny {approval_id}`");
        }
        EventBody::ApprovalResolved { approval_id, approved, reason } => {
            let label = match reason.as_deref() {
                Some("timeout") => "timed out (denied)",
                Some("timeout_stop") => "timed out (denied, thread stopped)",
                Some("dropped") => "dropped",
                _ if *approved => "approved",
                _ => "denied",
            };
            println!("  · approval {approval_id} {label}")
        }
        EventBody::TurnFinished { running_threads } => {
            if *running_threads > 0 {
                println!("  … {running_threads} thread(s) still running");
            }
        }
        EventBody::Error { message } => println!("  ⚠ {message}"),
        EventBody::BudgetWarning { scope, spent, limit } => {
            let what = match scope.as_str() {
                "daily" => "today's".to_string(),
                "conversation" => "this conversation's".to_string(),
                "thread" => format!("thread {t}'s"),
                other => format!("the {other}"),
            };
            println!(
                "  $ budget warning: {what} estimated spend is ${spent:.4} of its ${limit:.2} budget ({:.0}%)",
                spent / limit * 100.0
            )
        }
        EventBody::DevicePresence { device_id, online, .. } => {
            println!("  · device {device_id} {}", if *online { "online" } else { "offline" });
        }
        EventBody::HubMoveProgress { state, target_name, progress_pct, message, error, .. } => {
            let who = target_name.as_deref().unwrap_or("target");
            let pct = progress_pct.map(|p| format!(" ({p}%)")).unwrap_or_default();
            let detail = error.as_deref().or(message.as_deref()).unwrap_or("");
            if detail.is_empty() {
                println!("  · hub move → {who}: {state}{pct}");
            } else {
                println!("  · hub move → {who}: {state}{pct} — {detail}");
            }
        }
    }
}

/// Prints live events, showing streamed text as it arrives: the orchestrator's reply
/// inline after `grouper: `, and thread text a whole line at a time behind the
/// thread id, so threads streaming at once stay readable. The full message that
/// follows streamed text isn't printed again.
#[derive(Default)]
struct Printer {
    /// Put the conversation id in front (`grouper watch` across conversations).
    show_conversation: bool,
    /// The conversation whose reply is streaming on the current, unfinished line.
    open: Option<String>,
    /// `c:<conversation>` / `t:<thread>` whose current message was streamed.
    streamed: HashSet<String>,
    /// Thread text after its last newline, waiting for the rest of the line.
    partial: HashMap<String, String>,
}

impl Printer {
    fn prefix(&self, e: &Event) -> String {
        if self.show_conversation && !e.conversation_id.is_empty() {
            format!("[{}] ", e.conversation_id)
        } else {
            String::new()
        }
    }

    /// Finish a streaming line so other output starts on a line of its own.
    fn close_line(&mut self) {
        if self.open.take().is_some() {
            println!();
        }
    }

    fn thread_line(&mut self, e: &Event, t: &str, line: &str) {
        self.close_line();
        println!("{}    [{t}] {line}", self.prefix(e));
    }

    fn flush_thread(&mut self, e: &Event, t: &str) {
        if let Some(rest) = self.partial.remove(t).filter(|r| !r.is_empty()) {
            self.thread_line(e, t, &rest);
        }
    }

    fn event(&mut self, e: &Event) {
        let conv = &e.conversation_id;
        let t = e.thread_id.clone().unwrap_or_default();
        let (ckey, tkey) = (format!("c:{conv}"), format!("t:{t}"));
        match &e.body {
            EventBody::AssistantDelta { text } => {
                if self.open.as_ref() != Some(conv) {
                    self.close_line();
                    if self.streamed.contains(&ckey) {
                        // Resuming after other output (thread lines): indent under the reply.
                        print!("{}         ", self.prefix(e));
                    } else {
                        print!("\n{}grouper: ", self.prefix(e));
                    }
                    self.open = Some(conv.clone());
                }
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.streamed.insert(ckey);
                return;
            }
            EventBody::AssistantMessage { .. } if self.streamed.remove(&ckey) => {
                if self.open.as_ref() == Some(conv) {
                    self.close_line();
                }
                return;
            }
            EventBody::ThreadActivity { event: AgentEvent::TextDelta { text } } => {
                self.streamed.insert(tkey);
                let mut buf = self.partial.remove(&t).unwrap_or_default();
                buf.push_str(text);
                while let Some(i) = buf.find('\n') {
                    let line: String = buf.drain(..=i).collect();
                    self.thread_line(e, &t, line.trim_end_matches(['\n', '\r']));
                }
                self.partial.insert(t, buf);
                return;
            }
            EventBody::ThreadActivity { event: AgentEvent::Text { .. } } if self.streamed.remove(&tkey) => {
                self.flush_thread(e, &t);
                return;
            }
            EventBody::ThreadFinished { .. } => {
                // A stream cut short leaves a partial line and no full message.
                self.streamed.remove(&tkey);
                self.flush_thread(e, &t);
            }
            EventBody::TurnFinished { .. } | EventBody::Error { .. } if e.thread_id.is_none() => {
                self.streamed.remove(&ckey);
            }
            _ => {}
        }
        self.close_line();
        let p = self.prefix(e);
        match &e.body {
            // Keep the conversation prefix on the message's own line.
            EventBody::UserMessage { text } => println!("\n{p}you: {text}"),
            EventBody::AssistantMessage { text } => println!("\n{p}grouper: {text}"),
            EventBody::TurnFinished { running_threads: 0 } => {}
            _ => {
                print!("{p}");
                print_event(e);
            }
        }
    }
}

/// Where the local cache lives: `--cache`, else `<local data dir>/grouper/cache.db`.
fn cache_path(arg: Option<PathBuf>) -> Result<PathBuf> {
    arg.or_else(|| dirs::data_local_dir().map(|d| d.join("grouper").join("cache.db")))
        .context("no local data folder: pass --cache <path>")
}

/// The hub didn't answer at all (as opposed to answering with an error).
fn hub_unreachable(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_connect() || r.is_timeout() || r.is_request()))
}

/// After the hub didn't answer: the local cache, with a note saying how old it is.
/// Any other error (and a missing cache) is returned as it was.
fn offline_cache(arg: Option<PathBuf>, err: anyhow::Error) -> Result<core_sync::Store> {
    if !hub_unreachable(&err) {
        return Err(err);
    }
    let path = cache_path(arg)?;
    match core_sync::Store::open_existing(&path) {
        Ok(Some(store)) => {
            let when = store.last_update()?.map(core_sync::utc_string).unwrap_or_else(|| "an unknown time".into());
            eprintln!("(offline, cached at {when} in {})", path.display());
            Ok(store)
        }
        Ok(None) => Err(err.context(format!(
            "no offline cache at {} (run `grouper sync` while the hub is reachable)",
            path.display()
        ))),
        Err(e) => Err(err.context(format!("offline cache {}: {e}", path.display()))),
    }
}

fn read_key() -> Result<String> {
    if let Ok(k) = std::env::var("GROUPER_PROVIDER_KEY") {
        return Ok(k);
    }
    let k = if std::io::stdin().is_terminal() {
        rpassword::prompt_password("API key: ")?
    } else {
        let mut k = String::new();
        std::io::stdin().read_line(&mut k)?;
        k
    };
    Ok(k.trim().to_string())
}

/// A hub's JSON answer, or its `{"error": ...}` as an error.
async fn hub_json(r: reqwest::Response) -> Result<Value> {
    let status = r.status();
    let body: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = body["error"].as_str().map(String::from).unwrap_or_else(|| status.to_string());
        bail!("{msg}");
    }
    Ok(body)
}

/// Open an http(s) URL in the default browser. Whether it could be started.
fn open_browser(url: &str) -> bool {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().is_ok()
}

/// `grouper login`: the device link flow (docs/accounts-design.md). Prints a code and a link, waits
/// for the owner to approve it in a browser, then saves the hub, this device's token and the install
/// id. The token is never printed.
async fn login(cli_hub: Option<&str>, name: Option<&str>, no_browser: bool) -> Result<()> {
    // Trailing slash is stripped so `{hub}/v1/...` does not gain a double slash.
    // The saved value is whatever login resolved, without that slash.
    let hub = credentials::resolve_hub(cli_hub)?.trim_end_matches('/').to_string();
    let parsed = reqwest::Url::parse(&hub).context("--hub must be a URL such as https://hub.example.com")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("--hub must be an http:// or https:// URL");
    }
    let install_id = credentials::install_id()?;
    let name = name.map(str::trim).filter(|n| !n.is_empty()).map(String::from).unwrap_or_else(|| {
        let host = hostname();
        if host.is_empty() { "Grouper CLI".into() } else { host }
    });
    let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    let timeout = Some(std::time::Duration::from_secs(30));
    // A relay address needs a relay sign-in first; its token rides on every request below.
    let mut relay_token = None;
    let mut new_relay_token = false;
    if relay_detect(&http_client(None, timeout)?, &hub).await {
        match credentials::relay_token_for(&hub)? {
            Some(saved) if relay_signed_in(&http_client(Some(&saved), timeout)?, &hub).await => relay_token = Some(saved),
            _ => {
                relay_token = Some(relay_sign_in(&http_client(None, timeout)?, &hub, no_browser).await?);
                new_relay_token = true;
            }
        }
    }
    let http = http_client(relay_token.as_deref(), timeout)?;

    let start = http
        .post(format!("{hub}/v1/auth/link/start"))
        .json(&json!({"name": name, "platform": std::env::consts::OS, "install_id": install_id}))
        .send()
        .await
        .with_context(|| format!("couldn't reach the hub at {hub}"))?;
    let start = hub_json(start).await.context("the hub refused to start signing in")?;
    let code = start["code"].as_str().context("the hub sent no code")?.to_string();
    let poll_secret = start["poll_secret"].as_str().context("the hub sent no poll secret")?.to_string();
    let expires_in = start["expires_in"].as_u64().unwrap_or(600);
    let mut wait = start["interval"].as_u64().unwrap_or(3).clamp(1, 30);
    // Only an http(s) link from this hub is shown or opened.
    let verify_url = match start["verify_url"].as_str() {
        Some(u) if u.starts_with('/') => format!("{hub}{u}"),
        Some(u) if reqwest::Url::parse(u).is_ok_and(|p| matches!(p.scheme(), "http" | "https")) => u.to_string(),
        _ => format!("{hub}/#/link?code={code}"),
    };

    println!("To sign this computer in to {hub}, open this link in a browser where you're signed in to Grouper:");
    println!();
    println!("    {verify_url}");
    println!();
    println!("Check that the page shows the code {code} and \"{name}\", then approve it.");
    if !no_browser && open_browser(&verify_url) {
        println!("(Opened it in your browser.)");
    }
    println!("Waiting for approval… (Ctrl-C to cancel)");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(expires_in + 5);
    let mut warned = false;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
            _ = tokio::signal::ctrl_c() => bail!("cancelled; nothing was saved"),
        }
        if std::time::Instant::now() > deadline {
            bail!("the code expired before it was approved; run `grouper login` again");
        }
        let r = match http.post(format!("{hub}/v1/auth/link/poll")).json(&json!({"poll_secret": poll_secret})).send().await {
            Ok(r) => r,
            Err(e) => {
                if !warned {
                    eprintln!("(can't reach the hub right now, still trying: {e})");
                    warned = true;
                }
                continue;
            }
        };
        if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            wait = (wait + 1).min(30);
            continue;
        }
        let body = hub_json(r).await.context("checking the sign-in failed")?;
        match body["status"].as_str() {
            Some("pending") => continue,
            Some("denied") => bail!("the sign-in was denied in the browser; nothing was saved"),
            Some("expired") => bail!("the code expired before it was approved; run `grouper login` again"),
            Some("approved") => {
                let token = body["token"].as_str().context("the hub approved the sign-in but sent no token")?;
                if new_relay_token {
                    credentials::save_relay_token(&hub, relay_token.as_deref())?;
                }
                credentials::save(&hub, token, &install_id)?;
                let device = body["device"]["name"].as_str().unwrap_or(&name);
                println!("Signed in to {hub} as \"{device}\".");
                println!("Saved in {}. Run `grouper connect` to put this computer online.", credentials::path()?.display());
                return Ok(());
            }
            other => bail!("the hub answered with an unknown status {other:?}"),
        }
    }
}

/// Does `hub` answer like a relay? `GET /relay/v1/me` returns JSON with a `signed_in` field.
async fn relay_detect(http: &reqwest::Client, hub: &str) -> bool {
    let Ok(r) = http.get(format!("{hub}/relay/v1/me")).send().await else { return false };
    r.json::<Value>().await.is_ok_and(|v| v.get("signed_in").is_some())
}

/// Is the relay token this client sends accepted by the relay?
async fn relay_signed_in(http: &reqwest::Client, hub: &str) -> bool {
    let Ok(r) = http.get(format!("{hub}/relay/v1/me")).send().await else { return false };
    r.json::<Value>().await.is_ok_and(|v| v["signed_in"].as_bool() == Some(true))
}

/// Where the browser is sent for the CLI's relay sign-in: the relay's own sign-in page, then on to
/// the relay's `cli-token` endpoint, which redirects to this CLI's loopback listener with a
/// single-use code (docs/relay-design.md, Clients).
fn relay_sign_in_url(hub: &str, port: u16, state: &str) -> String {
    let next = format!("/relay/v1/cli-token?port={port}&state={state}");
    let encoded: String = next
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("{hub}/relay/sign-in?next={encoded}")
}

/// The `code` in a loopback callback's request line (`GET /?code=...&state=... HTTP/1.1`), only
/// when its `state` is the one this login made.
fn callback_code(request_line: &str, state: &str) -> Option<String> {
    let target = request_line.strip_prefix("GET ")?.split_whitespace().next()?;
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    let (mut code, mut got_state) = (None, None);
    for (k, v) in url.query_pairs() {
        match &*k {
            "code" => code = Some(v.into_owned()),
            "state" => got_state = Some(v.into_owned()),
            _ => {}
        }
    }
    (got_state.as_deref() == Some(state)).then_some(code).flatten().filter(|c| !c.is_empty() && c.len() <= 256)
}

/// The relay step of `grouper login`: the CLI can't do Google itself, so the browser signs in at the
/// relay and comes back to a loopback listener with a single-use code, which the CLI exchanges for a
/// relay token.
async fn relay_sign_in(http: &reqwest::Client, hub: &str, no_browser: bool) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.context("opening a local port for the relay sign-in")?;
    let port = listener.local_addr()?.port();
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("system randomness: {e}"))?;
    let state = hex::encode(buf);
    let url = relay_sign_in_url(hub, port, &state);

    println!("{hub} is behind the Grouper relay. Sign in to it first, in a browser:");
    println!();
    println!("    {url}");
    println!();
    if !no_browser && open_browser(&url) {
        println!("(Opened it in your browser.)");
    }
    println!("Waiting for the relay sign-in… (Ctrl-C to cancel)");

    let deadline = tokio::time::sleep(std::time::Duration::from_secs(300));
    tokio::pin!(deadline);
    let code = loop {
        let (mut sock, _) = tokio::select! {
            r = listener.accept() => r?,
            _ = &mut deadline => bail!("the relay sign-in timed out; run `grouper login` again"),
            _ = tokio::signal::ctrl_c() => bail!("cancelled; nothing was saved"),
        };
        let mut req = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut req)).await.ok().and_then(Result::ok).unwrap_or(0);
        let line = String::from_utf8_lossy(&req[..n]).lines().next().unwrap_or("").to_string();
        let found = callback_code(&line, &state);
        let (status, body) = if found.is_some() {
            ("200 OK", "Signed in to the relay. You can close this tab and go back to the terminal.")
        } else {
            ("400 Bad Request", "Not the sign-in this terminal is waiting for.")
        };
        let _ = sock
            .write_all(format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
            .await;
        if let Some(code) = found {
            break code;
        }
    };
    let r = http
        .post(format!("{hub}/relay/v1/cli-token/exchange"))
        .json(&json!({"code": code, "state": state}))
        .send()
        .await
        .with_context(|| format!("couldn't reach the relay at {hub}"))?;
    let body = hub_json(r).await.context("the relay refused the sign-in")?;
    Ok(body["relay_token"].as_str().context("the relay sent no relay token")?.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Login { name, no_browser } => return login(cli.hub.as_deref(), name.as_deref(), no_browser).await,
        Cmd::Logout => {
            match credentials::remove()? {
                Some(hub) => println!("Signed out of {hub} on this computer."),
                None => println!("Not signed in."),
            }
            return Ok(());
        }
        Cmd::Hub(cmd) => return hub::run(cli.hub.as_deref(), cli.token.as_deref(), cmd).await,
        cmd => {
            let cache = cli.cache;
            let (hub, token) = credentials::resolve(cli.hub, cli.token, credentials::DEFAULT_HUB)?;
            let relay_token = credentials::relay_token_for(&hub)?;
            let http = http_client(relay_token.as_deref(), None)?;
            let api = Api { base: hub.trim_end_matches('/').to_string(), token, relay_token, http };
            return run_authed_cmd(api, cache, cmd).await;
        }
    }
}

async fn run_authed_cmd(api: Api, cache: Option<PathBuf>, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Login { .. } | Cmd::Logout | Cmd::Hub(_) => unreachable!(),
        Cmd::Connect { workspace, no_shell, mcp_config, folders, folders_config, usage_probes, no_usage_probes } => {
            let workspace = workspace.unwrap_or_else(|| {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Grouper").join("workspaces")
            });
            println!("thread workspaces: {}", workspace.display());
            let folders = core_client::folders::load(folders_config.as_deref(), &folders).map_err(anyhow::Error::msg)?;
            for f in &folders {
                println!("scoped folder @{} ({}): {}", f.name, f.mode.as_str(), f.path.display());
            }
            let probe_setting = if no_usage_probes {
                core_client::usage_probes::UsageProbeSetting::off()
            } else {
                usage_probes.unwrap_or_default()
            };
            if probe_setting.enabled {
                println!(
                    "subscription usage probes: on for {} (read-only; switch off with --no-usage-probes)",
                    probe_setting.providers.as_ref().map_or_else(
                        || core_client::usage_probes::SUPPORTED_PROVIDERS.join(", "),
                        |p| p.join(", ")
                    )
                );
            }
            let config = core_client::Config {
                hub_url: api.base.clone(),
                token: api.token.clone(),
                capabilities: vec![
                    Capability {
                        name: "echo".into(),
                        description: "Returns its arguments".into(),
                        input_schema: json!({"type": "object", "additionalProperties": true}),
                    },
                    Capability {
                        name: "system_info".into(),
                        description: "OS, architecture and hostname".into(),
                        input_schema: default_input_schema(),
                    },
                ],
                workspace_root: Some(workspace),
                allow_shell: !no_shell,
                mcp_config,
                folders,
                usage_probes: probe_setting.enabled,
                usage_probe_providers: probe_setting.providers,
                relay_token: api.relay_token.clone(),
            };
            let (tx, rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = tx.send(true);
            });
            core_client::run(config, Arc::new(CliTools), Arc::new(PrintEvents), rx).await;
        }
        Cmd::Chat { text, conversation, standard, model } => {
            // Subscribe first so nothing is missed, then send.
            let mut events = Box::pin(api.events(conversation.as_deref()).await?);
            let resp: ChatResponse = serde_json::from_value(
                api.post(
                    "/v1/chat",
                    &ChatRequest {
                        conversation_id: conversation,
                        mode: Some(if standard { "standard" } else { "project" }.into()),
                        text,
                        model: model.clone(),
                    },
                )
                .await?,
            )?;
            let id = resp.conversation_id;
            println!("conversation {id}");
            if let Some(model) = model {
                println!("model: {model}");
            }
            let mut printer = Printer::default();
            while let Some(e) = events.next().await {
                if e.conversation_id != id || matches!(e.body, EventBody::UserMessage { .. }) {
                    continue;
                }
                printer.event(&e);
                if let EventBody::TurnFinished { running_threads: 0 } = e.body {
                    let threads: Vec<ThreadInfo> = api.get(&format!("/v1/threads?conversation={id}")).await?;
                    if threads.iter().all(|t| t.status != "running") {
                        let cost = usage::conversation_line(&api, &id).await;
                        println!("\n{cost}\ncontinue with: grouper chat -c {id} \"…\"");
                        break;
                    }
                }
            }
        }
        Cmd::Watch { conversation } => {
            let mut events = Box::pin(api.events(conversation.as_deref()).await?);
            let mut printer = Printer { show_conversation: conversation.is_none(), ..Default::default() };
            while let Some(e) = events.next().await {
                printer.event(&e);
            }
        }
        Cmd::Conversations { cmd } => match cmd {
            None => {
                let list = match api.get::<Vec<ConversationInfo>>("/v1/conversations").await {
                    Ok(list) => list,
                    Err(e) => offline_cache(cache, e)?.conversations()?,
                };
                for c in list {
                    let model = c.model.as_deref().unwrap_or("default");
                    println!("{}  {:<8} {:<16} {}", c.id, c.mode, model, c.title);
                }
            }
            Some(ConversationsCmd::Model { id, model }) => {
                let body = if model == "default" { json!({"model": null}) } else { json!({"model": model}) };
                let info: ConversationInfo =
                    serde_json::from_value(api.post(&format!("/v1/conversations/{id}/model"), &body).await?)?;
                println!("{id} model: {}", info.model.as_deref().unwrap_or("default"));
            }
        },
        Cmd::Log { conversation, thread } => {
            let q = thread.as_ref().map(|t| format!("?thread={t}")).unwrap_or_default();
            let events = match api.get::<Vec<Event>>(&format!("/v1/conversations/{conversation}/events{q}")).await {
                Ok(events) => events,
                Err(e) => {
                    let events = offline_cache(cache, e)?.events(&conversation, 0, thread.as_deref())?;
                    if events.is_empty() {
                        eprintln!("(nothing cached for {conversation})");
                    }
                    events
                }
            };
            for e in events {
                print_event(&e);
            }
        }
        Cmd::Threads { conversation } => {
            let q = conversation.as_ref().map(|c| format!("?conversation={c}")).unwrap_or_default();
            let threads = match api.get::<Vec<ThreadInfo>>(&format!("/v1/threads{q}")).await {
                Ok(threads) => threads,
                Err(e) => offline_cache(cache, e)?.threads(conversation.as_deref())?,
            };
            for t in threads {
                let why = t.reason.map(|r| format!(" ({r})")).unwrap_or_default();
                println!("{}  {:<8} {:<12} {:<14} {}{why}", t.id, t.status, t.device_id, t.model, t.title);
            }
        }
        Cmd::Sync => {
            let path = cache_path(cache)?;
            let store = Arc::new(core_sync::Store::open(&path)?);
            let report = core_sync::SyncClient::new(&api.base, &api.token, store)?
                .with_relay_token(api.relay_token.clone())
                .sync_once().await?;
            let memories = match report.memories {
                Some(n) => format!("{n} memories"),
                None => "no memories (they need an admin token)".into(),
            };
            println!(
                "synced {} conversations, {} threads, {} new events and {memories} into {}",
                report.conversations,
                report.threads,
                report.new_events,
                path.display()
            );
        }
        Cmd::Say { thread, text } => {
            api.post(&format!("/v1/threads/{thread}/message"), &json!({"text": text})).await?;
            println!("sent to {thread}");
        }
        Cmd::Stop { thread } => {
            api.post(&format!("/v1/threads/{thread}/stop"), &json!({})).await?;
            println!("stopped {thread}");
        }
        Cmd::Approve { approval } => {
            api.post(&format!("/v1/approvals/{approval}"), &json!({"approve": true})).await?;
            println!("approved {approval}");
        }
        Cmd::Deny { approval } => {
            api.post(&format!("/v1/approvals/{approval}"), &json!({"approve": false})).await?;
            println!("denied {approval}");
        }
        Cmd::Devices => print_devices(&api.get::<Vec<DeviceInfo>>("/v1/devices").await?),
        Cmd::Files { thread, path: None, .. } => {
            let l: WorkspaceListing = api.get(&format!("/v1/threads/{}/files", url_segment(&thread))).await?;
            println!("thread {} files on {}:", l.thread_id, l.device_id);
            if l.files.is_empty() {
                println!("  (none)");
            }
            for f in &l.files {
                println!("  {:>10}  {}", f.size, f.path);
            }
            if l.truncated {
                println!("  … more files not listed");
            }
        }
        Cmd::Files { thread, path: Some(path), output } => {
            let path = path.replace('\\', "/");
            if path.split('/').any(|s| s == ".." || s == ".") {
                bail!("{path}: use a path inside the thread's workspace, without `.` or `..`");
            }
            let encoded: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(url_segment).collect();
            let bytes =
                api.get_bytes(&format!("/v1/threads/{}/files/{}", url_segment(&thread), encoded.join("/"))).await?;
            match output {
                Some(out) if out.as_os_str() == "-" => std::io::stdout().write_all(&bytes)?,
                _ => {
                    let out = output.unwrap_or_else(|| {
                        PathBuf::from(path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("download"))
                    });
                    std::fs::write(&out, &bytes).with_context(|| format!("writing {}", out.display()))?;
                    println!("saved {} ({} bytes)", out.display(), bytes.len());
                }
            }
        }
        Cmd::Call { device, tool, args } => {
            let args: Value = serde_json::from_str(&args).context("args must be JSON")?;
            let r: ToolCallResponse = serde_json::from_value(
                api.post(&format!("/v1/devices/{device}/call"), &ToolCallRequest { tool, args }).await?,
            )?;
            if r.ok {
                println!("{}", r.output);
            } else {
                bail!("tool failed: {}", r.output);
            }
        }
        Cmd::Keys(KeysCmd::List) => {
            for p in api.get::<Vec<ProviderInfo>>("/v1/providers").await? {
                let key = if p.has_key { "key set" } else { "no key" };
                let via = p.via_device.map(|device| format!(" via {device}")).unwrap_or_default();
                println!("{:<14} {:<10} {:<8} {}{via}", p.id, p.kind, key, p.base_url.unwrap_or_default());
            }
        }
        Cmd::Keys(KeysCmd::Set { provider, kind, base_url, via_device, options, no_key }) => {
            let api_key = if no_key { None } else { Some(read_key()?).filter(|k| !k.is_empty()) };
            let mut options = match options {
                Some(o) => serde_json::from_str(&o).context("options must be JSON")?,
                None => Value::Null,
            };
            if let Some(device) = via_device {
                if options.is_null() {
                    options = json!({});
                }
                let Some(object) = options.as_object_mut() else {
                    bail!("--via-device requires --options to be a JSON object");
                };
                object.insert("via_device".into(), Value::String(device));
            }
            api.put(
                &format!("/v1/providers/{provider}"),
                &ProviderConfig { id: provider.clone(), kind, base_url, api_key, options },
            )
            .await?;
            println!("saved {provider}; every connected device has it now");
        }
        Cmd::Tools(ToolsCmd::List { device, all }) => {
            let mut params: Vec<(&str, &str)> = Vec::new();
            if let Some(d) = device.as_deref() {
                params.push(("device", d));
            }
            if all {
                params.push(("all", "true"));
            }
            let url = reqwest::Url::parse_with_params("http://hub/v1/tools", &params)?;
            let path = match url.query().filter(|q| !q.is_empty()) {
                Some(q) => format!("/v1/tools?{q}"),
                None => "/v1/tools".into(),
            };
            let dim = std::io::stdout().is_terminal();
            for t in api.get::<Vec<ToolInfo>>(&path).await? {
                let about = t.description.lines().next().unwrap_or_default();
                let line = if t.available {
                    format!("{:<12} {:<34} {:<7} {}", t.device_id, t.name, t.permission.as_str(), about)
                } else {
                    format!("{:<12} {:<34} {:<7} [unavailable] {}", t.device_id, t.name, t.permission.as_str(), about)
                };
                if !t.available && dim {
                    println!("\x1b[2m{line}\x1b[0m");
                } else {
                    println!("{line}");
                }
            }
        }
        Cmd::Tools(ToolsCmd::Set { device, tool, permission }) => {
            api.put(&format!("/v1/devices/{device}/tools/{tool}"), &json!({"permission": permission})).await?;
            println!("{device} {tool}: {permission}");
        }
        Cmd::Models(ModelsCmd::List) => {
            let hosted: HashMap<String, String> = api
                .get::<Vec<ProviderInfo>>("/v1/providers")
                .await?
                .into_iter()
                .filter_map(|provider| provider.via_device.map(|device| (provider.id, device)))
                .collect();
            for m in api.get::<Vec<ModelEntry>>("/v1/models").await? {
                let via = hosted.get(&m.provider_id).map(|device| format!(" via {device}")).unwrap_or_default();
                println!(
                    "{:<16} {:<12} {:<28} ${}/${} {}{}{}{via}",
                    m.id,
                    m.provider_id,
                    m.model,
                    m.input_price,
                    m.output_price,
                    if m.free { "free " } else { "" },
                    if m.local { "local " } else { "" },
                    m.roles.join(",")
                );
                if !m.use_cases.is_empty() {
                    println!("{:<16} use cases: {}", "", m.use_cases);
                }
                if !m.modalities.is_empty() {
                    println!("{:<16} modalities: {}", "", m.modalities.join(", "));
                }
            }
        }
        Cmd::Models(ModelsCmd::Import { provider, model, id, roles, local, probe }) => {
            let roles: Vec<String> =
                roles.split(',').map(str::trim).filter(|role| !role.is_empty()).map(String::from).collect();
            let result: ModelImportResult = serde_json::from_value(
                api.post(
                    "/v1/models/import",
                    &ModelImportRequest {
                        provider,
                        model,
                        id,
                        roles: (!roles.is_empty()).then_some(roles),
                        local: local.then_some(true),
                        probe,
                    },
                )
                .await?,
            )?;
            let m = &result.model;
            println!(
                "{:<16} {:<12} {:<28} ${}/${} ctx {} {}{}{}",
                m.id,
                m.provider_id,
                m.model,
                m.input_price,
                m.output_price,
                m.context_window,
                if m.free { "free " } else { "" },
                if m.local { "local " } else { "" },
                m.roles.join(",")
            );
            if !m.modalities.is_empty() {
                println!("{:<16} modalities: {}", "", m.modalities.join(", "));
            }
            if let Some(report) = &result.probe {
                print_probe_report(report);
            } else if let Some(err) = &result.probe_error {
                eprintln!("probe failed: {err}");
            }
            if let Some(n) = result.proposal_id {
                println!("use cases drafted: accept with `grouper models accept {n}`");
            }
        }
        Cmd::Models(ModelsCmd::Probe { id }) => {
            let report: core_client::core_protocol::ProbeReport =
                serde_json::from_value(api.post(&format!("/v1/models/{}/probe", url_segment(&id)), &json!({})).await?)?;
            print_probe_report(&report);
            if let Some(n) = report.proposal_id {
                if let Some(text) = &report.proposal_text {
                    println!("proposed use cases (#{n}): {text}");
                }
                println!("accept with `grouper models accept {n}` (or `grouper models reject {n}`)");
            }
        }
        Cmd::Models(ModelsCmd::Sync { provider }) => {
            sync_models_cmd(&api, provider.as_deref()).await?;
        }
        Cmd::Models(ModelsCmd::Add {
            id,
            provider,
            model,
            roles,
            free,
            input_price,
            output_price,
            cache_read_price,
            cache_write_price,
            context_window,
            use_cases,
            local,
        }) => {
            let entry = ModelEntry {
                id: id.clone(),
                provider_id: provider,
                model,
                input_price,
                output_price,
                cache_read_price,
                cache_write_price,
                context_window,
                use_cases,
                free,
                roles: roles.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
                local,
                modalities: vec![],
                ..Default::default()
            };
            api.put(&format!("/v1/models/{id}"), &entry).await?;
            println!("saved model {id}");
        }
        Cmd::Models(ModelsCmd::Remove { id }) => {
            api.delete(&format!("/v1/models/{id}")).await?;
            println!("removed {id}");
        }
        Cmd::Usage(args) => usage::run(&api, args).await?,
        Cmd::Memory(cmd) => memory_cmd(&api, cmd).await?,
        Cmd::Models(cmd @ (ModelsCmd::Stats | ModelsCmd::Proposals { .. } | ModelsCmd::Accept { .. } | ModelsCmd::Reject { .. })) => {
            registry_cmd(&api, cmd).await?
        }
        Cmd::Rate { thread, rating } => {
            api.post(&format!("/v1/threads/{thread}/rating"), &json!({"rating": rating})).await?;
            println!("rated {thread}: {rating}");
        }
        Cmd::Rules(cmd) => rules_cmd(&api, cmd).await?,
        Cmd::Skills(cmd) => skills_cmd(&api, cmd).await?,
        Cmd::Limits { cmd } => limits_cmd(&api, cmd).await?,
    }
    Ok(())
}

async fn limits_cmd(api: &Api, cmd: Option<LimitsCmd>) -> Result<()> {
    use core_client::core_protocol::{
        ApprovalTimeoutAction, DEFAULT_ASK_TIMEOUT_SECS, LIMIT_KEYS, LimitsReport,
    };
    let report: LimitsReport = match cmd {
        None => api.get("/v1/limits").await?,
        Some(LimitsCmd::Set { key, value }) => {
            if !LIMIT_KEYS.contains(&key.as_str()) {
                bail!("unknown limit {key:?}; the limits are {}", LIMIT_KEYS.join(", "));
            }
            let raw = value.trim();
            let value = if key == "approval_timeout_action" {
                match raw {
                    "off" | "none" | "null" => Value::Null,
                    v => Value::String(v.to_string()),
                }
            } else {
                match raw.trim_start_matches('$') {
                    "off" | "none" | "null" => Value::Null,
                    v => json!(v.parse::<f64>().with_context(|| format!("{key} takes a number or off"))?),
                }
            };
            serde_json::from_value(api.put("/v1/limits", &serde_json::Map::from_iter([(key, value)])).await?)?
        }
    };
    let l = &report.limits;
    let off = || "off".to_string();
    let money = |v: Option<f64>| v.map(|v| format!("${v}")).unwrap_or_else(off);
    let count = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_else(off);
    println!("max_running_threads             {}", count(l.max_running_threads));
    println!("max_running_threads_per_device  {}", count(l.max_running_threads_per_device));
    println!("thread_budget_usd               {}", money(l.thread_budget_usd));
    println!("conversation_budget_usd         {}", money(l.conversation_budget_usd));
    println!("daily_budget_usd                {}", money(l.daily_budget_usd));
    let default = if l.warn_at_fraction.is_none() { " (default)" } else { "" };
    println!("warn_at_fraction                {}{default}", report.warn_at_fraction);
    println!(
        "{:<30}  {}",
        "approval_timeout_secs",
        l.approval_timeout_secs.map(|n| n.to_string()).unwrap_or_else(off)
    );
    let action_note = if l.approval_timeout_action == ApprovalTimeoutAction::Deny { " (default)" } else { "" };
    println!("{:<30}  {}{action_note}", "approval_timeout_action", l.approval_timeout_action.as_str());
    let ask = match l.ask_timeout_secs {
        Some(n) if n == DEFAULT_ASK_TIMEOUT_SECS => format!("{n} (default)"),
        Some(n) => n.to_string(),
        None => off(),
    };
    println!("{:<30}  {ask}", "ask_timeout_secs");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let left = report.day_resets_at.saturating_sub(now);
    println!(
        "
now: {} thread(s) running; estimated spend today ${:.4} (UTC day, resets in {}h {}m)",
        report.running_threads,
        report.spent_today_usd,
        left / 3600,
        left % 3600 / 60
    );
    Ok(())
}

fn format_latency_s(secs: f64) -> String {
    if secs < 1.0 {
        format!("{:.0}ms", secs * 1000.0)
    } else if secs < 10.0 {
        format!("{secs:.1}s")
    } else {
        format!("{secs:.0}s")
    }
}

fn print_probe_report(report: &core_client::core_protocol::ProbeReport) {
    println!(
        "probe {}: {}/{} passed  {}  {} tok  ${:.4}",
        report.model_id,
        report.passed,
        report.total,
        format_latency_s(report.total_latency_ms as f64 / 1000.0),
        report.total_tokens,
        report.total_cost
    );
    for r in &report.results {
        println!(
            "  {:<10} {:<4} {:>7}  {:>5} tok  ${:.4}  {}",
            r.task_type,
            if r.passed { "pass" } else { "fail" },
            format_latency_s(r.latency_ms as f64 / 1000.0),
            r.input_tokens + r.output_tokens,
            r.cost,
            r.detail
        );
    }
    if let Some(why) = &report.stopped_reason {
        eprintln!("probe stopped early: {why}");
    }
}

fn print_sync_result(result: &::hub::sync::SyncResult) {
    if let Some(err) = &result.error {
        eprintln!("{}: sync failed: {err}", result.provider_id);
        return;
    }
    println!(
        "{}: +{} added, {} updated, {} hidden, {} filtered",
        result.provider_id, result.added, result.updated, result.hidden, result.filtered
    );
    if let Some(defaults) = &result.defaults {
        let mut parts = Vec::new();
        if let Some(orch) = &defaults.orchestrator {
            parts.push(format!("orchestrator={orch}"));
        }
        if let Some(std) = &defaults.standard {
            parts.push(format!("standard={std}"));
        }
        if !parts.is_empty() {
            println!("  defaults: {}", parts.join(", "));
        }
    }
}

async fn sync_models_cmd(api: &Api, provider: Option<&str>) -> Result<()> {
    let targets: Vec<String> = match provider.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => vec![id.to_string()],
        None => api
            .get::<Vec<ProviderInfo>>("/v1/providers")
            .await?
            .into_iter()
            .filter(|p| {
                matches!(p.kind.as_str(), "openai" | "anthropic")
                    && p.via_device.is_none()
                    && (p.has_key || p.base_url.as_deref().is_some_and(::hub::registry::is_local_url))
            })
            .map(|p| p.id)
            .collect(),
    };
    if targets.is_empty() {
        println!("no API providers to sync");
        return Ok(());
    }
    let mut failed = 0usize;
    for id in targets {
        match api.post(&format!("/v1/providers/{}/sync", url_segment(&id)), &json!({})).await {
            Ok(body) => {
                let result: ::hub::sync::SyncResult = serde_json::from_value(body)?;
                if result.error.is_some() {
                    failed += 1;
                }
                print_sync_result(&result);
            }
            Err(err) => {
                failed += 1;
                eprintln!("{id}: sync failed: {err:#}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} provider sync(s) failed");
    }
    Ok(())
}

async fn registry_cmd(api: &Api, cmd: ModelsCmd) -> Result<()> {
    use core_client::core_protocol::{OutcomeStats, RegistryProposal, RegistryStats};
    let print_proposal = |p: &RegistryProposal| {
        println!("#{:<4} {:<9} {:<16} ({}) {}", p.id, p.status, p.model_id, p.source, p.text);
        println!("      why: {}", p.reason);
    };
    match cmd {
        ModelsCmd::Stats => {
            let s: RegistryStats = api.get("/v1/registry/stats").await?;
            let print = |title: &str, rows: &[OutcomeStats]| {
                println!("{title}:");
                if rows.is_empty() {
                    println!("  no finished threads yet");
                }
                for r in rows {
                    let what = if r.task_type.is_empty() { "(all)" } else { r.task_type.as_str() };
                    let rate = r.success_rate.map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "-".into());
                    let grade = r.mean_grade.map(|g| format!("{g:.1}")).unwrap_or_else(|| "-".into());
                    let cost = r.mean_cost.map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into());
                    let time = r.median_duration_s.map(|d| format!("{d:.0}s")).unwrap_or_else(|| "-".into());
                    println!(
                        "  {:<16} {:<10} {:>3}/{:<3} ok {:>5}  stopped {:<2} grade {:<4} {:<9} median {:<6} rating {:+}",
                        r.subject,
                        what,
                        r.successes,
                        r.done + r.failed,
                        rate,
                        r.stopped,
                        grade,
                        cost,
                        time,
                        r.net_rating
                    );
                }
            };
            print("models", &s.models);
            print("devices", &s.devices);
            println!("probes:");
            if s.probes.is_empty() {
                println!("  no probe runs yet");
            }
            for r in &s.probes {
                let what = if r.task_type.is_empty() { "(all)" } else { r.task_type.as_str() };
                let rate = r.success_rate.map(|x| format!("{:.0}%", x * 100.0)).unwrap_or_else(|| "-".into());
                let cost = r.mean_cost.map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into());
                let time = r.median_duration_s.map(format_latency_s).unwrap_or_else(|| "-".into());
                let tokens = r.mean_tokens.map(|t| format!("{t:.0} tok")).unwrap_or_else(|| "-".into());
                println!(
                    "  {:<16} {:<10} {:>3}/{:<3} pass {:>5}  median {:<7} {:<9} {}",
                    r.subject,
                    what,
                    r.successes,
                    r.done + r.failed,
                    rate,
                    time,
                    cost,
                    tokens
                );
            }
        }
        ModelsCmd::Proposals { all } => {
            let q = if all { "?status=all" } else { "" };
            let list: Vec<RegistryProposal> = api.get(&format!("/v1/registry/proposals{q}")).await?;
            if list.is_empty() {
                println!("no {}proposals", if all { "" } else { "pending " });
            }
            for p in &list {
                print_proposal(p);
            }
        }
        ModelsCmd::Accept { id } | ModelsCmd::Reject { id } => {
            let accept = matches!(cmd, ModelsCmd::Accept { .. });
            let p: RegistryProposal =
                serde_json::from_value(api.post(&format!("/v1/registry/proposals/{id}"), &json!({"accept": accept})).await?)?;
            print_proposal(&p);
        }
        _ => unreachable!(),
    }
    Ok(())
}

async fn rules_cmd(api: &Api, cmd: RulesCmd) -> Result<()> {
    use core_client::core_protocol::{PriorityRule, RuleInput, RuleSpec};
    let print = |r: &PriorityRule| {
        let spec = serde_json::to_string(&r.rule).unwrap_or_default();
        println!("#{:<4} {:<8} {}  {spec}", r.id, if r.enabled { "on" } else { "off" }, r.text);
    };
    match cmd {
        RulesCmd::List => {
            let list: Vec<PriorityRule> = api.get("/v1/registry/rules").await?;
            if list.is_empty() {
                println!("no rules");
            }
            for r in &list {
                print(r);
            }
        }
        RulesCmd::Add { text, when_tag, when_task_type, require, forbid, prefer } => {
            let rule = RuleSpec { when_tag, when_task_type, require, forbid, prefer };
            let r: PriorityRule = serde_json::from_value(api.post("/v1/registry/rules", &RuleInput { text, rule }).await?)?;
            print!("added ");
            print(&r);
        }
        RulesCmd::Remove { id } => {
            api.delete(&format!("/v1/registry/rules/{id}")).await?;
            println!("removed rule #{id}");
        }
        RulesCmd::Enable { id } | RulesCmd::Disable { id } => {
            let enabled = matches!(cmd, RulesCmd::Enable { .. });
            api.post(&format!("/v1/registry/rules/{id}"), &json!({"enabled": enabled})).await?;
            println!("rule #{id} {}", if enabled { "on" } else { "off" });
        }
    }
    Ok(())
}

async fn skills_cmd(api: &Api, cmd: SkillsCmd) -> Result<()> {
    use core_client::core_protocol::Skill;
    match cmd {
        SkillsCmd::List { search, limit } => {
            let mut params = vec![("limit", limit.to_string())];
            if let Some(q) = search {
                params.push(("q", q));
            }
            let url = reqwest::Url::parse_with_params("http://hub/v1/skills", &params)?;
            let list: Vec<Skill> = api.get(&format!("/v1/skills?{}", url.query().unwrap_or(""))).await?;
            if list.is_empty() {
                println!("no skills");
            }
            for s in &list {
                println!("#{:<5} {:<32} worked {:<3} {}", s.id, s.name, s.success_count, s.description.replace('\n', " "));
            }
        }
        SkillsCmd::Show { id } => {
            let s: Skill = api.get(&format!("/v1/skills/{id}")).await?;
            println!("#{} {}\n{}\n", s.id, s.name, s.description);
            println!("{}\n", s.steps);
            for (what, list) in [("triggers", &s.triggers), ("tools", &s.tools), ("models", &s.models), ("devices", &s.devices)] {
                if !list.is_empty() {
                    println!("{what}: {}", list.join(", "));
                }
            }
            println!("worked {} time(s)", s.success_count);
        }
        SkillsCmd::Forget { id } => {
            api.delete(&format!("/v1/skills/{id}")).await?;
            println!("forgot skill #{id}");
        }
    }
    Ok(())
}

async fn memory_cmd(api: &Api, cmd: MemoryCmd) -> Result<()> {
    use core_client::core_protocol::{MemoryEntry, MemoryInput};
    let print = |m: &MemoryEntry| {
        let tags = if m.tags.is_empty() { String::new() } else { format!("  [{}]", m.tags.join(", ")) };
        println!("#{:<5} {}{tags}", m.id, m.content.replace('\n', " ⏎ "));
    };
    match cmd {
        MemoryCmd::List { search, limit } => {
            let mut params = vec![("limit", limit.to_string())];
            if let Some(q) = search {
                params.push(("q", q));
            }
            // Built with Url so the search text is percent-encoded.
            let url = reqwest::Url::parse_with_params("http://hub/v1/memories", &params)?;
            let list: Vec<MemoryEntry> = api.get(&format!("/v1/memories?{}", url.query().unwrap_or(""))).await?;
            if list.is_empty() {
                println!("no memories");
            }
            for m in &list {
                print(m);
            }
        }
        MemoryCmd::Add { text, tags } => {
            let m: MemoryEntry =
                serde_json::from_value(api.post("/v1/memories", &MemoryInput { content: text, tags: Some(tags) }).await?)?;
            print!("saved ");
            print(&m);
        }
        MemoryCmd::Edit { id, text, tags } => {
            let m: MemoryEntry =
                serde_json::from_value(api.put(&format!("/v1/memories/{id}"), &MemoryInput { content: text, tags }).await?)?;
            print!("updated ");
            print(&m);
        }
        MemoryCmd::Forget { id } => {
            api.delete(&format!("/v1/memories/{id}")).await?;
            println!("forgot #{id}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn login_needs_no_arguments_and_help_names_the_default_hub() {
        let _lock = credentials::test_env::lock();
        {
            let _hub = credentials::test_env::VarGuard::unset("GROUPER_HUB");
            let cli = Cli::try_parse_from(["grouper", "login"]).expect("grouper login has no required arguments");
            assert!(cli.hub.is_none());
            assert!(matches!(cli.cmd, Cmd::Login { name: None, no_browser: false }));
            let devices = Cli::try_parse_from(["grouper", "devices"]).expect("grouper devices");
            assert!(devices.hub.is_none());

            let help = Cli::command().render_long_help().to_string();
            assert!(help.contains(credentials::DEFAULT_HUB), "{help}");
            assert!(help.contains("GROUPER_HUB"), "{help}");
        }
        {
            let _hub = credentials::test_env::VarGuard::set("GROUPER_HUB", "https://env.example");
            let from_env = Cli::try_parse_from(["grouper", "login"]).expect("login");
            assert_eq!(from_env.hub.as_deref(), Some("https://env.example"));
            let from_flag = Cli::try_parse_from(["grouper", "--hub", "https://flag.example", "devices"]).expect("devices");
            assert_eq!(from_flag.hub.as_deref(), Some("https://flag.example"));
        }
    }

    #[test]
    fn hub_subcommands_parse() {
        let init = Cli::try_parse_from(["grouper", "hub", "init", "--data", "/tmp/grouper-hub"]).expect("hub init");
        assert!(matches!(init.cmd, Cmd::Hub(hub::HubCmd::Init { data: Some(p) }) if p == std::path::Path::new("/tmp/grouper-hub")));

        let run = Cli::try_parse_from(["grouper", "hub", "run"]).expect("hub run");
        assert!(matches!(run.cmd, Cmd::Hub(hub::HubCmd::Run { bind, .. }) if bind.to_string() == hub::DEFAULT_BIND));

        let status = Cli::try_parse_from(["grouper", "hub", "status"]).expect("hub status");
        assert!(matches!(status.cmd, Cmd::Hub(hub::HubCmd::Status)));

        let mv = Cli::try_parse_from(["grouper", "hub", "move", "my-mac"]).expect("hub move");
        assert!(matches!(mv.cmd, Cmd::Hub(hub::HubCmd::Move { device }) if device == "my-mac"));

        let take = Cli::try_parse_from(["grouper", "hub", "take"]).expect("hub take");
        assert!(matches!(take.cmd, Cmd::Hub(hub::HubCmd::Take)));
    }

    #[test]
    fn usage_probe_flags_parse_and_default_to_on() {
        use core_client::usage_probes::UsageProbeSetting;
        let _lock = credentials::test_env::lock();
        let _env = credentials::test_env::VarGuard::unset("GROUPER_USAGE_PROBES");
        let connect = |args: &[&str]| {
            let mut full = vec!["grouper", "connect"];
            full.extend_from_slice(args);
            Cli::try_parse_from(full).map(|cli| match cli.cmd {
                Cmd::Connect { usage_probes, no_usage_probes, no_shell, .. } => (usage_probes, no_usage_probes, no_shell),
                _ => panic!("not connect"),
            })
        };
        let all = UsageProbeSetting { enabled: true, providers: None };

        // Not given: no setting here, which resolves to "on for every provider".
        assert_eq!(connect(&[]).unwrap(), (None, false, false));
        assert_eq!(UsageProbeSetting::default(), all);
        // Bare flag: all providers (and it doesn't swallow the next flag).
        assert_eq!(connect(&["--usage-probes", "--no-shell"]).unwrap(), (Some(all.clone()), false, true));
        // A list.
        let (list, _, _) = connect(&["--usage-probes=claude,codex,grok,antigravity"]).unwrap();
        assert_eq!(list.unwrap().providers.unwrap(), ["claude", "codex", "grok", "antigravity"]);
        assert_eq!(connect(&["--usage-probes=grok"]).unwrap().0.unwrap().providers.unwrap(), ["grok"]);
        assert!(connect(&["--usage-probes=claude,bogus"]).is_err());
        // The local off switch, which can't be combined with the flag.
        assert_eq!(connect(&["--no-usage-probes"]).unwrap(), (None, true, false));
        assert!(connect(&["--no-usage-probes", "--usage-probes"]).is_err());

        // The environment variable takes the same values.
        {
            let _env = credentials::test_env::VarGuard::set("GROUPER_USAGE_PROBES", "0");
            assert_eq!(connect(&[]).unwrap().0, Some(UsageProbeSetting::off()));
        }
        {
            let _env = credentials::test_env::VarGuard::set("GROUPER_USAGE_PROBES", "claude,grok");
            assert_eq!(connect(&[]).unwrap().0.unwrap().providers.unwrap(), ["claude", "grok"]);
        }

        // `grouper hub run` has the same flags.
        let run = Cli::try_parse_from(["grouper", "hub", "run", "--usage-probes=grok"]).expect("hub run");
        assert!(matches!(run.cmd, Cmd::Hub(hub::HubCmd::Run { usage_probes: Some(ref s), .. }) if s.providers.as_deref() == Some(&["grok".to_string()][..])));
        let run = Cli::try_parse_from(["grouper", "hub", "run", "--no-usage-probes"]).expect("hub run");
        assert!(matches!(run.cmd, Cmd::Hub(hub::HubCmd::Run { no_usage_probes: true, usage_probes: None, .. })));
    }

    #[test]
    fn models_sync_subcommand_parses() {
        let all = Cli::try_parse_from(["grouper", "models", "sync"]).expect("models sync");
        assert!(matches!(all.cmd, Cmd::Models(ModelsCmd::Sync { provider: None })));

        let one = Cli::try_parse_from(["grouper", "models", "sync", "openai"]).expect("models sync openai");
        assert!(matches!(
            one.cmd,
            Cmd::Models(ModelsCmd::Sync { provider: Some(ref p) }) if p == "openai"
        ));
    }

    #[tokio::test]
    async fn models_sync_command_calls_hub_sync_endpoint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Fake upstream OpenAI-compatible listing server (no extra dev-dependencies needed).
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let upstream_body = json!({
            "data": [
                {"id": "gpt-5.4"},
                {"id": "gpt-5-mini"},
                {"id": "text-embedding-3-small"}
            ]
        })
        .to_string();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = upstream_listener.accept().await {
                let body = upstream_body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!("grouper-cli-sync-{}", ::hub::db::random_hex(4)));
        let handle = ::hub::host::start(&dir, &[std::net::SocketAddr::from(([127, 0, 0, 1], 0))]).await.unwrap();
        let hub_addr = handle.binds()[0];
        let h = handle.hub().unwrap();
        let (_, admin_token) = h.db.add_device("Admin", "windows", true).unwrap();
        h.db.upsert_provider(&ProviderConfig {
            id: "openai".into(),
            kind: "openai".into(),
            base_url: Some(format!("http://{upstream_addr}/v1")),
            api_key: Some("test-key".into()),
            options: json!({"preset": "openai"}),
        })
        .unwrap();

        let api = Api {
            base: format!("http://{hub_addr}"),
            token: admin_token,
            relay_token: None,
            http: reqwest::Client::new(),
        };
        sync_models_cmd(&api, Some("openai")).await.unwrap();
        let models = h.db.visible_models();
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.id == "gpt-5.4" && m.roles.contains(&"orchestrator".to_string())));
        assert!(models.iter().any(|m| m.id == "gpt-5-mini" && m.roles.contains(&"standard".to_string())));

        // Syncing all providers (None) also succeeds cleanly.
        sync_models_cmd(&api, None).await.unwrap();

        handle.stop().await;
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn relay_sign_in_url_goes_through_the_relay_sign_in_page() {
        let url = relay_sign_in_url("https://relay.example", 4242, "abcd");
        assert_eq!(
            url,
            "https://relay.example/relay/sign-in?next=/relay/v1/cli-token%3Fport%3D4242%26state%3Dabcd"
        );
    }

    #[test]
    fn callback_code_needs_the_matching_state() {
        assert_eq!(callback_code("GET /?code=abc&state=s1 HTTP/1.1", "s1").as_deref(), Some("abc"));
        assert_eq!(callback_code("GET /?state=s1&code=a%20b HTTP/1.1", "s1").as_deref(), Some("a b"));
        assert_eq!(callback_code("GET /?code=abc&state=other HTTP/1.1", "s1"), None);
        assert_eq!(callback_code("GET /?code=abc HTTP/1.1", "s1"), None);
        assert_eq!(callback_code("GET /?code=&state=s1 HTTP/1.1", "s1"), None);
        assert_eq!(callback_code("POST /?code=abc&state=s1 HTTP/1.1", "s1"), None);
        assert_eq!(callback_code("", "s1"), None);
    }

    #[tokio::test]
    async fn http_client_sends_the_relay_header_only_with_a_token() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn one_request(client: reqwest::Client) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase()
            });
            client.get(format!("http://{addr}/v1/x")).send().await.unwrap();
            server.await.unwrap()
        }
        let with = one_request(http_client(Some(" rt_abc "), None).unwrap()).await;
        assert!(with.contains("grouper-relay: rt_abc\r\n"), "{with}");
        let without = one_request(http_client(None, None).unwrap()).await;
        assert!(!without.contains("grouper-relay"), "{without}");
    }
}
