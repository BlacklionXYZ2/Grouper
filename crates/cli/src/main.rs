mod credentials;
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

const DEFAULT_HUB: &str = "http://127.0.0.1:8080";

/// Grouper command-line client.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Hub base URL, e.g. https://hub.example.com
    #[arg(long, env = "GROUPER_HUB", global = true)]
    hub: Option<String>,
    /// This device's token from `grouper-hub device add`.
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
    /// Save this device's hub URL and token securely for future commands.
    Login,
    /// Remove this device's saved hub URL and token.
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
        /// Enable read-only subscription usage probes (Claude Code, Codex). Off by default:
        /// calls unofficial provider endpoints using the CLIs' local sign-in credentials.
        /// Tokens stay on this device and are never sent to the hub.
        #[arg(long, env = "GROUPER_USAGE_PROBES")]
        usage_probes: bool,
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

fn hostname() -> String {
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
    http: reqwest::Client,
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
        EventBody::ApprovalResolved { approval_id, approved } => {
            println!("  · approval {approval_id} {}", if *approved { "approved" } else { "denied" })
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
        if self.show_conversation { format!("[{}] ", e.conversation_id) } else { String::new() }
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

fn read_device_token() -> Result<String> {
    let token = if std::io::stdin().is_terminal() {
        rpassword::prompt_password("Device token: ")?
    } else {
        let mut token = String::new();
        std::io::stdin().read_line(&mut token)?;
        token
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("device token is empty");
    }
    Ok(token)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Login => {
            let hub = cli.hub.as_deref().context("login requires --hub <url>")?.trim_end_matches('/');
            let token = read_device_token()?;
            credentials::save(hub, &token)?;
            println!("saved credentials for {hub}");
            return Ok(());
        }
        Cmd::Logout => {
            credentials::remove()?;
            println!("removed saved credentials");
            return Ok(());
        }
        _ => {}
    }

    let cache = cli.cache;
    let (hub, token) = credentials::resolve(cli.hub, cli.token, DEFAULT_HUB)?;
    let api = Api { base: hub.trim_end_matches('/').to_string(), token, http: reqwest::Client::new() };

    match cli.cmd {
        Cmd::Login | Cmd::Logout => unreachable!(),
        Cmd::Connect { workspace, no_shell, mcp_config, folders, folders_config, usage_probes } => {
            let workspace = workspace.unwrap_or_else(|| {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Grouper").join("workspaces")
            });
            println!("thread workspaces: {}", workspace.display());
            let folders = core_client::folders::load(folders_config.as_deref(), &folders).map_err(anyhow::Error::msg)?;
            for f in &folders {
                println!("scoped folder @{} ({}): {}", f.name, f.mode.as_str(), f.path.display());
            }
            if usage_probes {
                println!("subscription usage probes: enabled (read-only, unofficial Claude/Codex endpoints)");
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
                usage_probes,
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
            let report = core_sync::SyncClient::new(&api.base, &api.token, store)?.sync_once().await?;
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
