//! Conversation workers: the orchestrator for project chats, one model for standard chats.
//!
//! Each conversation has a worker that sleeps until something happens (a user
//! message, a thread finishing or asking a question, the user stepping into a
//! thread), then runs one turn of the loop with everything that arrived.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use core_agent::{Ending, Hooks, LoopConfig, Tool, ToolOutput, push_user_text, run_loop};
use core_protocol::{AgentEvent, EventBody, Permission, ToolDef};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::server::{HUB_DEVICE, Hub};
use crate::threads::ThreadRequest;

#[derive(Debug, Clone)]
pub enum Wake {
    User(String),
    ThreadDone { thread_id: String, title: String, status: String, summary: String },
    ThreadAsk { thread_id: String, title: String, question: String },
    UserStepIn { thread_id: String, title: String, text: String },
}

impl Wake {
    fn render(&self) -> String {
        match self {
            Wake::User(t) => t.clone(),
            Wake::ThreadDone { thread_id, title, status, summary } => {
                format!("[Thread {thread_id} \"{title}\" {status}]\n{summary}")
            }
            Wake::ThreadAsk { thread_id, title, question } => format!(
                "[Thread {thread_id} \"{title}\" asks]\n{question}\n\
                 (Answer with message_thread. If you need another device first, start a thread for it.)"
            ),
            Wake::UserStepIn { thread_id, title, text } => format!(
                "[The user stepped into thread {thread_id} \"{title}\"]\n{text}\n\
                 (They acted directly; don't undo it or start conflicting work.)"
            ),
        }
    }

    /// What to search long-term memory with: what was said, without Grouper's framing.
    fn recall_text(&self) -> String {
        match self {
            Wake::User(t) => t.clone(),
            Wake::ThreadDone { title, summary, .. } => format!("{title}\n{summary}"),
            Wake::ThreadAsk { title, question, .. } => format!("{title}\n{question}"),
            Wake::UserStepIn { text, .. } => text.clone(),
        }
    }
}

const ORCHESTRATOR_SYSTEM: &str = "You are the orchestrator of Grouper, a personal assistant that works across the \
user's devices (Mac, PC, phone) and many AI models. You talk with the user and get work done by delegating it to \
subagent threads.

How you work:
- Plan, then delegate. Do the actual work through threads with start_thread; don't try to do device work yourself.
- Each thread runs on one device (or on \"hub\", your own cloud machine) and sees only the brief you write, never \
this conversation. Put everything it needs in the brief: goal, context, constraints, expected output.
- Start independent pieces of work in parallel by calling start_thread several times in one response.
- Threads run in the background. After starting them, tell the user briefly what you started and end your turn. \
You'll be woken when a thread finishes, asks you something, or the user steps in.
- If a device is offline, ask the user what to do, or start the work on \"hub\" with a hub_reason so it's ready \
when the device is back.
- When a thread asks you something, answer it with message_thread (or start another thread to find out first).
- When results come in, give the user a clear, short summary. Mention files by device and path.
- Use list_devices and list_models when you need to choose where and with what model to run something.
- You can't change tool permissions; only the user can.

Memory:
- Long-term memory is shared by all of the user's conversations. Memories relevant to a turn are added to it \
automatically in a [Memory] block; use recall to look for something specific.
- Use remember for lasting facts and preferences about the user and their projects (who they are, their devices \
and setups, conventions, decisions, likes and dislikes), not for transient task details or things that only matter \
in this conversation. One self-contained fact per memory. Never store secrets such as passwords or API keys.
- When a memory turns out wrong or outdated, fix it with update_memory. Remove it with forget, and always when the \
user asks you to forget something.

Messages in [brackets] are from Grouper (threads finishing, questions, the user stepping in, status, memories), not \
typed by the user.";

const STANDARD_SYSTEM: &str = "You are Grouper, a helpful personal assistant. Answer directly and concisely. \
A [Memory] block at the end of a message holds notes saved earlier about the user and their projects, added by \
Grouper rather than typed by the user; use them when relevant.";

impl Hub {
    /// Deliver something to a conversation's worker, starting the worker if needed.
    pub async fn wake(self: &Arc<Self>, conversation_id: &str, wake: Wake) {
        let mut workers = self.orchestrators.lock().await;
        if let Some(tx) = workers.get(conversation_id)
            && tx.send(wake.clone()).is_ok()
        {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(wake);
        workers.insert(conversation_id.to_string(), tx);
        tokio::spawn(worker(Arc::downgrade(self), conversation_id.to_string(), rx));
    }

    async fn status_line(&self, conversation_id: &str) -> String {
        let devices = self.devices().await;
        let devs: Vec<String> = devices
            .iter()
            .map(|d| format!("{} ({}, {})", d.name, d.id, if d.online { "online" } else { "offline" }))
            .collect();
        let running: Vec<String> = self
            .db
            .threads(Some(conversation_id))
            .into_iter()
            .filter(|t| t.status == "running")
            .map(|t| format!("{} \"{}\" on {}", t.id, t.title, t.device_id))
            .collect();
        format!(
            "[Status] Devices: {}. Running threads: {}.",
            devs.join(", "),
            if running.is_empty() { "none".into() } else { running.join("; ") }
        )
    }
}

async fn worker(hub: Weak<Hub>, conversation_id: String, mut rx: mpsc::UnboundedReceiver<Wake>) {
    while let Some(first) = rx.recv().await {
        let Some(hub) = hub.upgrade() else { return };
        let mut wakes = vec![first];
        while let Ok(w) = rx.try_recv() {
            wakes.push(w);
        }
        run_turn(&hub, &conversation_id, wakes).await;
    }
}

async fn run_turn(hub: &Arc<Hub>, conversation_id: &str, wakes: Vec<Wake>) {
    let Some(conv) = hub.db.conversation(conversation_id) else { return };
    let project = conv.mode == "project";

    let mut text = wakes.iter().map(Wake::render).collect::<Vec<_>>().join("\n\n");
    if project {
        text.push_str("\n\n");
        text.push_str(&hub.status_line(conversation_id).await);
    }

    let mut history = hub.db.history(conversation_id);
    let merged = history.last().is_some_and(|m| m.role == providers::Role::User);
    let from = if merged { history.len() - 1 } else { history.len() };
    let recall_query = wakes.iter().map(Wake::recall_text).collect::<Vec<_>>().join("\n");
    if let Some(block) = crate::memory::recall_block(&hub.db, &recall_query, &crate::memory::seen_text(&history)) {
        text.push_str("\n\n");
        text.push_str(&block);
    }
    push_user_text(&mut history, text);

    let finish = |hub: &Arc<Hub>, history: &[providers::Message]| {
        if let Err(e) = hub.db.save_history(conversation_id, from, &history[from..]) {
            tracing::error!("saving history: {e:#}");
        }
    };

    let role = if project { "orchestrator" } else { "standard" };
    let Some(model) = hub.db.default_model(role) else {
        finish(hub, &history);
        hub.emit(
            conversation_id,
            None,
            EventBody::Error {
                message: "No models registered. Add a provider key (`grouper keys set`) and a model (`grouper models add`)."
                    .into(),
            },
        );
        hub.emit(conversation_id, None, EventBody::TurnFinished { running_threads: 0 });
        return;
    };
    let provider = match hub
        .db
        .provider(&model.provider_id)
        .map_err(|e| e.to_string())
        .and_then(|p| p.ok_or_else(|| format!("no provider {}", model.provider_id)))
        .and_then(|p| providers::build(&p).map_err(|e| e.to_string()))
    {
        Ok(p) => p,
        Err(e) => {
            finish(hub, &history);
            hub.emit(conversation_id, None, EventBody::Error { message: e });
            hub.emit(conversation_id, None, EventBody::TurnFinished { running_threads: 0 });
            return;
        }
    };

    let tools: Vec<Arc<dyn Tool>> = if project { orchestrator_tools(hub, conversation_id) } else { vec![] };
    let cfg = LoopConfig {
        provider_id: model.provider_id.clone(),
        model: model.model.clone(),
        system: if project { ORCHESTRATOR_SYSTEM } else { STANDARD_SYSTEM }.to_string(),
        max_turns: 20,
        max_tokens: 16_000,
    };
    let hooks = ConversationHooks { hub: hub.clone(), conversation_id: conversation_id.to_string() };
    let out = run_loop(provider.as_ref(), &cfg, &tools, &mut history, &hooks).await;
    finish(hub, &history);

    match out.ending {
        Ending::Done => {}
        Ending::MaxTurns => {
            hub.emit(conversation_id, None, EventBody::Error { message: "The orchestrator hit its turn limit.".into() });
        }
        Ending::Refused => {
            hub.emit(conversation_id, None, EventBody::Error { message: "The model declined to answer.".into() });
        }
        Ending::Error(e) => {
            hub.emit(conversation_id, None, EventBody::Error { message: e });
        }
    }
    let running = hub.db.threads(Some(conversation_id)).iter().filter(|t| t.status == "running").count() as u32;
    hub.emit(conversation_id, None, EventBody::TurnFinished { running_threads: running });
}

struct ConversationHooks {
    hub: Arc<Hub>,
    conversation_id: String,
}

#[async_trait]
impl Hooks for ConversationHooks {
    fn event(&self, event: AgentEvent) {
        match event {
            AgentEvent::Text { text } => {
                self.hub.emit(&self.conversation_id, None, EventBody::AssistantMessage { text });
            }
            AgentEvent::TextDelta { text } => {
                self.hub.emit_live(&self.conversation_id, None, EventBody::AssistantDelta { text });
            }
            AgentEvent::Usage { provider_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens } => {
                self.hub.db.record_usage(
                    Some(&self.conversation_id),
                    None,
                    &provider_id,
                    &model,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                );
            }
            _ => {}
        }
    }
}

// ---- orchestrator tools

fn orchestrator_tools(hub: &Arc<Hub>, conversation_id: &str) -> Vec<Arc<dyn Tool>> {
    let ctx = || Ctx { hub: hub.clone(), conversation_id: conversation_id.to_string() };
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(StartThread(ctx())),
        Arc::new(MessageThread(ctx())),
        Arc::new(StopThread(ctx())),
        Arc::new(ListThreads(ctx())),
        Arc::new(ListDevices(ctx())),
        Arc::new(ListModels(ctx())),
    ];
    tools.extend(crate::memory::memory_tools(hub, conversation_id));
    tools
}

struct Ctx {
    hub: Arc<Hub>,
    conversation_id: String,
}

fn str_arg(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn to_json(v: impl serde::Serialize) -> ToolOutput {
    ToolOutput::ok(serde_json::to_string_pretty(&v).unwrap_or_default())
}

struct StartThread(Ctx);
#[async_trait]
impl Tool for StartThread {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "start_thread".into(),
            description: "Start a subagent thread on a device. Returns immediately; you're woken when it finishes \
                          or asks something. Call several times in one response to run work in parallel."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "title": {"type": "string", "description": "Short title shown to the user"},
                "device": {"type": "string", "description": "Device id or name, or \"hub\" for your cloud machine"},
                "goal": {"type": "string"},
                "context": {"type": "string", "description": "Everything the thread needs to know; it can't see this chat"},
                "constraints": {"type": "string"},
                "expected_output": {"type": "string"},
                "model": {"type": "string", "description": "Model id from list_models (optional)"},
                "hub_reason": {"type": "string", "description": "When device is hub: why it runs there (e.g. the target device is offline)"}
            }, "required": ["title", "device", "goal"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let req = ThreadRequest {
            title: str_arg(&input, "title"),
            device: str_arg(&input, "device"),
            goal: str_arg(&input, "goal"),
            context: str_arg(&input, "context"),
            constraints: str_arg(&input, "constraints"),
            expected_output: str_arg(&input, "expected_output"),
            model: input.get("model").and_then(Value::as_str).map(String::from),
            hub_reason: input.get("hub_reason").and_then(Value::as_str).map(String::from),
        };
        match self.0.hub.start_thread(&self.0.conversation_id, req).await {
            Ok(t) => to_json(json!({"thread_id": t.id, "device": t.device_id, "model": t.model, "status": t.status})),
            Err(e) => ToolOutput::err(e),
        }
    }
}

struct MessageThread(Ctx);
#[async_trait]
impl Tool for MessageThread {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "message_thread".into(),
            description: "Send a message to a running thread: answer its question or give it new instructions.".into(),
            input_schema: json!({"type": "object", "properties": {
                "thread_id": {"type": "string"}, "text": {"type": "string"}
            }, "required": ["thread_id", "text"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let id = str_arg(&input, "thread_id");
        if !self.0.hub.db.thread(&id).is_some_and(|t| t.conversation_id == self.0.conversation_id) {
            return ToolOutput::err("no such thread in this conversation");
        }
        match self.0.hub.orchestrator_thread_message(&id, &str_arg(&input, "text")).await {
            Ok(()) => ToolOutput::ok("sent"),
            Err(e) => ToolOutput::err(e),
        }
    }
}

struct StopThread(Ctx);
#[async_trait]
impl Tool for StopThread {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "stop_thread".into(),
            description: "Stop a running thread.".into(),
            input_schema: json!({"type": "object", "properties": {"thread_id": {"type": "string"}}, "required": ["thread_id"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let id = str_arg(&input, "thread_id");
        if !self.0.hub.db.thread(&id).is_some_and(|t| t.conversation_id == self.0.conversation_id) {
            return ToolOutput::err("no such thread in this conversation");
        }
        match self.0.hub.stop_thread(&id, "orchestrator").await {
            Ok(()) => ToolOutput::ok("stopped"),
            Err(e) => ToolOutput::err(e),
        }
    }
}

struct ListThreads(Ctx);
#[async_trait]
impl Tool for ListThreads {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_threads".into(),
            description: "List this conversation's threads with status and summaries.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _input: Value) -> ToolOutput {
        to_json(self.0.hub.db.threads(Some(&self.0.conversation_id)))
    }
}

struct ListDevices(Ctx);
#[async_trait]
impl Tool for ListDevices {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_devices".into(),
            description: "List devices (including \"hub\"), whether they're online, and the tools threads can \
                          use on each with their permission level."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _input: Value) -> ToolOutput {
        let hub = &self.0.hub;
        let list: Vec<Value> = hub
            .devices()
            .await
            .into_iter()
            .map(|d| {
                let tools: Vec<Value> = if d.id == HUB_DEVICE {
                    core_protocol::FILE_TOOLS
                        .iter()
                        .map(|t| json!({"name": t, "permission": Permission::Auto}))
                        .collect()
                } else {
                    hub.db.tools(Some(&d.id)).into_iter().map(|t| json!({"name": t.name, "permission": t.permission})).collect()
                };
                json!({"id": d.id, "name": d.name, "platform": d.platform, "online": d.online, "thread_tools": tools})
            })
            .collect();
        to_json(list)
    }
}

struct ListModels(Ctx);
#[async_trait]
impl Tool for ListModels {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_models".into(),
            description: "List registered models with prices (USD per million tokens), use cases and default roles.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _input: Value) -> ToolOutput {
        to_json(self.0.hub.db.models())
    }
}
