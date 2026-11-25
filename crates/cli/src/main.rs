use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use core_client::core_protocol::{
    AgentEvent, Capability, ChatRequest, ChatResponse, ConversationInfo, DeviceInfo, Event, EventBody, ModelEntry,
    ProviderConfig, ProviderInfo, ThreadInfo, ToolCallRequest, ToolCallResponse, ToolInfo,
};
use core_client::{Events, Status, ToolHandler};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

/// Grouper command-line client.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Hub base URL, e.g. https://hub.example.com
    #[arg(long, env = "GROUPER_HUB", default_value = "http://127.0.0.1:8080", global = true)]
    hub: String,
    /// This device's token from `grouper-hub device add`.
    #[arg(long, env = "GROUPER_TOKEN", global = true, hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Connect this machine as a device: stay online and run threads the orchestrator starts here.
    Connect {
        /// Where thread workspaces go.
        #[arg(long, env = "GROUPER_WORKSPACE")]
        workspace: Option<PathBuf>,
        /// Don't offer run_shell to threads.
        #[arg(long)]
        no_shell: bool,
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
    },
    /// Print live events (all conversations, or one).
    Watch {
        #[arg(long, short)]
        conversation: Option<String>,
    },
    /// List conversations.
    Conversations,
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
    /// Long-term memory about you and your projects, shared by every conversation.
    #[command(subcommand)]
    Memory(MemoryCmd),
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
        #[arg(long, default_value_t = 0)]
        context_window: u64,
        #[arg(long, default_value = "")]
        use_cases: String,
    },
    Remove { id: String },
}

struct CliTools;
impl ToolHandler for CliTools {
    fn call(&self, tool: &str, args: &str) -> Result<String, String> {
        match tool {
            "echo" => Ok(args.to_string()),
            "system.info" => Ok(json!({
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
    }
}

struct Api {
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
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Ok(serde_json::from_value(self.send(self.http.get(format!("{}{path}", self.base))).await?)?)
    }
    async fn post(&self, path: &str, body: &impl serde::Serialize) -> Result<Value> {
        self.send(self.http.post(format!("{}{path}", self.base)).json(body)).await
    }
    async fn put(&self, path: &str, body: &impl serde::Serialize) -> Result<Value> {
        self.send(self.http.put(format!("{}{path}", self.base)).json(body)).await
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
        let (ws, _) = tokio_tungstenite::connect_async(req).await.context("can't open the event stream")?;
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
        EventBody::ThreadStarted { title, device_id, model, reason, .. } => {
            let why = reason.as_ref().map(|r| format!(" ({r})")).unwrap_or_default();
            println!("  ▶ thread {t} \"{title}\" started on {device_id} with {model}{why}");
        }
        EventBody::ThreadActivity { event } => match event {
            AgentEvent::Text { text } => println!("    [{t}] {}", short(&json!(text), 200)),
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
    }
}

fn read_key() -> Result<String> {
    if let Ok(k) = std::env::var("GROUPER_PROVIDER_KEY") {
        return Ok(k);
    }
    if std::io::stdin().is_terminal() {
        eprint!("API key (input shown): ");
    }
    let mut k = String::new();
    std::io::stdin().read_line(&mut k)?;
    Ok(k.trim().to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    let cli = Cli::parse();
    let token = cli.token.context("no token: pass --token or set GROUPER_TOKEN")?;
    let api = Api { base: cli.hub.trim_end_matches('/').to_string(), token, http: reqwest::Client::new() };

    match cli.cmd {
        Cmd::Connect { workspace, no_shell } => {
            let workspace = workspace.unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("Grouper").join("workspaces")
            });
            println!("thread workspaces: {}", workspace.display());
            let config = core_client::Config {
                hub_url: api.base.clone(),
                token: api.token.clone(),
                capabilities: vec![
                    Capability { name: "echo".into(), description: "Returns its arguments".into() },
                    Capability { name: "system.info".into(), description: "OS, architecture and hostname".into() },
                ],
                workspace_root: Some(workspace),
                allow_shell: !no_shell,
            };
            let (tx, rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = tx.send(true);
            });
            core_client::run(config, Arc::new(CliTools), Arc::new(PrintEvents), rx).await;
        }
        Cmd::Chat { text, conversation, standard } => {
            // Subscribe first so nothing is missed, then send.
            let mut events = Box::pin(api.events(conversation.as_deref()).await?);
            let resp: ChatResponse = serde_json::from_value(
                api.post(
                    "/v1/chat",
                    &ChatRequest {
                        conversation_id: conversation,
                        mode: Some(if standard { "standard" } else { "project" }.into()),
                        text,
                    },
                )
                .await?,
            )?;
            let id = resp.conversation_id;
            println!("conversation {id}");
            while let Some(e) = events.next().await {
                if e.conversation_id != id || matches!(e.body, EventBody::UserMessage { .. }) {
                    continue;
                }
                print_event(&e);
                if let EventBody::TurnFinished { running_threads: 0 } = e.body {
                    let threads: Vec<ThreadInfo> = api.get(&format!("/v1/threads?conversation={id}")).await?;
                    if threads.iter().all(|t| t.status != "running") {
                        let cost = usage_line(&api, &id).await;
                        println!("\n{cost}\ncontinue with: grouper chat -c {id} \"…\"");
                        break;
                    }
                }
            }
        }
        Cmd::Watch { conversation } => {
            let mut events = Box::pin(api.events(conversation.as_deref()).await?);
            while let Some(e) = events.next().await {
                if conversation.is_none() {
                    print!("[{}] ", e.conversation_id);
                }
                print_event(&e);
            }
        }
        Cmd::Conversations => {
            for c in api.get::<Vec<ConversationInfo>>("/v1/conversations").await? {
                println!("{}  {:<8} {}", c.id, c.mode, c.title);
            }
        }
        Cmd::Log { conversation, thread } => {
            let q = thread.map(|t| format!("?thread={t}")).unwrap_or_default();
            for e in api.get::<Vec<Event>>(&format!("/v1/conversations/{conversation}/events{q}")).await? {
                print_event(&e);
            }
        }
        Cmd::Threads { conversation } => {
            let q = conversation.map(|c| format!("?conversation={c}")).unwrap_or_default();
            for t in api.get::<Vec<ThreadInfo>>(&format!("/v1/threads{q}")).await? {
                let why = t.reason.map(|r| format!(" ({r})")).unwrap_or_default();
                println!("{}  {:<8} {:<12} {:<14} {}{why}", t.id, t.status, t.device_id, t.model, t.title);
            }
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
                println!("{:<14} {:<10} {:<8} {}", p.id, p.kind, key, p.base_url.unwrap_or_default());
            }
        }
        Cmd::Keys(KeysCmd::Set { provider, kind, base_url, options, no_key }) => {
            let api_key = if no_key { None } else { Some(read_key()?).filter(|k| !k.is_empty()) };
            let options = match options {
                Some(o) => serde_json::from_str(&o).context("options must be JSON")?,
                None => Value::Null,
            };
            api.put(
                &format!("/v1/providers/{provider}"),
                &ProviderConfig { id: provider.clone(), kind, base_url, api_key, options },
            )
            .await?;
            println!("saved {provider}; every connected device has it now");
        }
        Cmd::Tools(ToolsCmd::List { device }) => {
            let q = device.map(|d| format!("?device={d}")).unwrap_or_default();
            for t in api.get::<Vec<ToolInfo>>(&format!("/v1/tools{q}")).await? {
                println!("{:<12} {:<14} {:<7} {}", t.device_id, t.name, t.permission.as_str(), t.description);
            }
        }
        Cmd::Tools(ToolsCmd::Set { device, tool, permission }) => {
            api.put(&format!("/v1/devices/{device}/tools/{tool}"), &json!({"permission": permission})).await?;
            println!("{device} {tool}: {permission}");
        }
        Cmd::Models(ModelsCmd::List) => {
            for m in api.get::<Vec<ModelEntry>>("/v1/models").await? {
                println!(
                    "{:<16} {:<12} {:<28} ${}/${} {}{}",
                    m.id,
                    m.provider_id,
                    m.model,
                    m.input_price,
                    m.output_price,
                    if m.free { "free " } else { "" },
                    m.roles.join(",")
                );
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
            context_window,
            use_cases,
        }) => {
            let entry = ModelEntry {
                id: id.clone(),
                provider_id: provider,
                model,
                input_price,
                output_price,
                context_window,
                use_cases,
                free,
                roles: roles.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
            };
            api.put(&format!("/v1/models/{id}"), &entry).await?;
            println!("saved model {id}");
        }
        Cmd::Models(ModelsCmd::Remove { id }) => {
            api.delete(&format!("/v1/models/{id}")).await?;
            println!("removed {id}");
        }
        Cmd::Memory(cmd) => memory_cmd(&api, cmd).await?,
    }
    Ok(())
}

async fn usage_line(api: &Api, conversation: &str) -> String {
    // Thread usage, counted from events (the orchestrator's own usage is recorded on the hub).
    let events: Vec<Event> = api.get(&format!("/v1/conversations/{conversation}/events")).await.unwrap_or_default();
    let (mut i, mut o) = (0u64, 0u64);
    for e in &events {
        if let EventBody::ThreadActivity { event: AgentEvent::Usage { input_tokens, output_tokens, .. } } = &e.body {
            i += input_tokens;
            o += output_tokens;
        }
    }
    format!("threads used {i} input / {o} output tokens")
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
