//! Conversation workers: the orchestrator for project chats, one model for standard chats.
//!
//! Each conversation has a worker that sleeps until something happens (a user
//! message, a thread finishing or asking a question, the user stepping into a
//! thread), then runs one turn of the loop with everything that arrived.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use core_agent::{Ending, Hooks, LoopConfig, Tool, ToolOutput, push_user_text, run_loop};
use core_protocol::{AgentEvent, EventBody, ToolDef};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::server::Hub;
use crate::threads::ThreadRequest;

#[derive(Debug, Clone)]
pub enum Wake {
    User(String),
    ThreadDone { thread_id: String, title: String, status: String, summary: String },
    ThreadAsk { thread_id: String, title: String, question: String },
    UserStepIn { thread_id: String, title: String, text: String },
    /// Something happened to a thread that doesn't end it, e.g. it is waiting for its
    /// device, reached a hub checkpoint, or was handed over (`offline.rs`).
    ThreadUpdate { thread_id: String, title: String, what: String, details: String },
}

impl Wake {
    pub(crate) fn render(&self) -> String {
        match self {
            Wake::User(t) => t.clone(),
            Wake::ThreadUpdate { thread_id, title, what, details } => {
                format!("[Thread {thread_id} \"{title}\" {what}]\n{details}")
            }
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
            Wake::ThreadUpdate { title, details, .. } => format!("{title}\n{details}"),
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
- If a device is offline, you can still start a thread on it: it waits for the device (status waiting_device) \
while the hub does what it can up to a checkpoint, then continues on the device when it reconnects. Tell the user, \
and ask whether to wait, use another device, or cancel (stop_thread) when it matters.
- When a thread asks you something, answer it with message_thread (or start another thread to find out first).
- When results come in, give the user a clear, short summary. Mention files by device and path. To quote or check \
a thread's output file, use read_thread_file instead of starting a thread; the user can download files with \
`grouper files <thread>`.
- Devices may have scoped folders (list_devices): user-allowed folders outside a thread's workspace, read-only or \
read-write. Threads reach them as `@<folder>/<path>`; mention the folder in the brief when the work is there.
- Use list_devices, list_models and list_usage when you need to choose where and with what model to run something.
- Give start_thread a task_type (code, research, writing, data, ops) and tags such as \"sensitive\" (private data) \
or \"draft\". Leave model out and the registry chooses by the user's priority rules and past results; if a rule \
refuses, tell the user.
- Leave device out of start_thread and set needs (shell, gpu, xcode, folder:<name>, agent:<name> such as \
agent:codex). Grouper picks the best online device for that task type from past results, and uses the hub only \
for file-only work. Name a device only to pin it; suggest_device shows the ranking.
- When a thread's result arrives, grade it with grade_thread (1-5, short note). That's how the registry learns.
- When list_models stats clearly contradict a model's use cases, suggest new text with propose_use_cases; the user \
decides.
- You can't change tool permissions or approve a thread's tool calls; only the user can. If a thread asks you to \
allow a call the user denied, tell it the user declined and to stop or find another way. Never say permission is \
granted.

Memory:
- Long-term memory is shared by all of the user's conversations. Memories relevant to a turn are added to it \
automatically in a [Memory] block; use recall to look for something specific.
- Use remember for lasting facts and preferences about the user and their projects (who they are, their devices \
and setups, conventions, decisions, likes and dislikes), not for transient task details or things that only matter \
in this conversation. One self-contained fact per memory. Never store secrets such as passwords or API keys.
- When a memory turns out wrong or outdated, fix it with update_memory. Remove it with forget, and always when the \
user asks you to forget something.

Skills:
- After a multi-step task succeeds, save the procedure with save_skill: steps, tools, and the models and devices \
that handled each step. Matching skills arrive in a [Skills] block; reuse one instead of re-planning when it fits, \
then update_skill (worked: true, plus what changed).

Messages in [brackets] are from Grouper (threads finishing, questions, the user stepping in, status, memories, \
skills), not typed by the user. Never write such bracketed blocks yourself; to save a memory, call remember.";

const STANDARD_SYSTEM: &str = "You are Grouper, a helpful personal assistant. Answer directly and concisely. \
A [Memory] block at the end of a message holds notes saved earlier about the user and their projects, added by \
Grouper rather than typed by the user; use them when relevant. Never write a [Memory] block or any other \
bracketed note yourself: reply only with your answer.";

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
        let threads = self.db.threads(Some(conversation_id));
        let list = |status: &str| -> Vec<String> {
            threads
                .iter()
                .filter(|t| t.status == status)
                .map(|t| format!("{} \"{}\" on {}", t.id, t.title, t.device_id))
                .collect()
        };
        let (running, waiting) = (list("running"), list(crate::offline::WAITING_DEVICE));
        let waiting =
            if waiting.is_empty() { String::new() } else { format!(" Waiting for their device: {}.", waiting.join("; ")) };
        let limits = crate::limits::status_note(self, conversation_id).await.unwrap_or_default();
        format!(
            "[Status] Devices: {}. Running threads: {}.{waiting}{limits}",
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

/// Resolve the model for a conversation turn.
///
/// In a project chat, any harness without `can_orchestrate` (for example a row that predates
/// the rule) is refused with an error event and falls back to the orchestrator default.
pub(crate) async fn resolve_turn_model(
    hub: &Hub,
    conversation_id: &str,
    project: bool,
) -> Option<core_protocol::ModelEntry> {
    let role = if project { "orchestrator" } else { "standard" };
    let chosen = match hub.db.conversation_model(conversation_id) {
        Ok(Some(model)) if project && !crate::db::can_orchestrate_project(&model) => {
            let reason = crate::db::HarnessCannotOrchestrate(model.id.clone());
            hub.emit(
                conversation_id,
                None,
                EventBody::Error { message: format!("{reason}; using the default model instead") },
            );
            None
        }
        Ok(model) => model,
        // The conversation's chosen model was removed from the registry: say so and
        // carry on with the role default rather than failing every turn.
        Err(e) => {
            hub.emit(
                conversation_id,
                None,
                EventBody::Error { message: format!("{e:#}; using the default model instead") },
            );
            None
        }
    };
    let model = match chosen {
        Some(model) => Some(model),
        None => hub.default_model(role).await,
    };
    match model {
        Some(model) if project && !crate::db::can_orchestrate_project(&model) => {
            let reason = crate::db::HarnessCannotOrchestrate(model.id.clone());
            hub.emit(conversation_id, None, EventBody::Error { message: reason.to_string() });
            None
        }
        other => other,
    }
}

async fn run_turn(hub: &Arc<Hub>, conversation_id: &str, wakes: Vec<Wake>) {
    let Some(conv) = hub.db.conversation(conversation_id) else { return };
    let project = conv.mode == "project";
    // Subscription-billed harnesses in standard chats never spend API budgets; API turns still check limits.
    let standard_harness = !project
        && hub
            .db
            .conversation_model(conversation_id)
            .ok()
            .flatten()
            .or_else(|| hub.db.default_model("standard"))
            .is_some_and(|m| m.is_harness());
    if !standard_harness && crate::limits::refuse_turn(hub, conversation_id, &wakes) {
        return;
    }

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
    if project
        && let Some(block) = crate::skills::skills_block(&hub.db, &recall_query, &crate::skills::seen_text(&history))
    {
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
    let Some(model) = resolve_turn_model(hub, conversation_id, project).await else {
        finish(hub, &history);
        // Models may exist but all live on devices that are offline, or only non-orchestrating harnesses exist.
        let message = match hub.db.default_model(role) {
            Some(m) => {
                let raw_host = m
                    .device_id
                    .clone()
                    .or_else(|| {
                        hub.db
                            .provider(&m.provider_id)
                            .ok()
                            .flatten()
                            .and_then(|p| p.via_device().map(String::from))
                    })
                    .unwrap_or_else(|| "a device".into());
                let host = hub
                    .resolve_device(&raw_host)
                    .map(|(_, name)| name)
                    .unwrap_or(raw_host);
                format!(
                    "Every model for this chat runs on a device that's offline ({} runs on {host}). Bring it back \
                     online, or add a model that doesn't depend on it (`grouper models add`).",
                    m.id
                )
            }
            None if project && hub.db.visible_models().iter().any(|m| m.is_harness() && !m.can_orchestrate) => {
                "No model can orchestrate a project chat (registered harnesses don't support project orchestration). \
                 Add an API model or a harness with can_orchestrate, or start a standard chat."
                    .into()
            }
            None => "No models registered. Add a provider key (`grouper keys set`) and a model (`grouper models add`)."
                .into(),
        };
        hub.emit(conversation_id, None, EventBody::Error { message });
        hub.emit(conversation_id, None, EventBody::TurnFinished { running_threads: 0 });
        return;
    };

    if !project && model.is_harness() {
        match hub
            .run_standard_harness_turn(conversation_id, &model, &text, &wakes)
            .await
        {
            Ok(reply) => {
                let clean = without_imitated_notes(&reply.text);
                if !clean.is_empty() {
                    hub.emit(conversation_id, None, EventBody::AssistantMessage { text: clean });
                }
                history.push(providers::Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::Block::Text { text: reply.text }],
                    raw: None,
                });
                finish(hub, &history);
            }
            Err(message) => {
                finish(hub, &history);
                hub.emit(conversation_id, None, EventBody::Error { message });
            }
        }
        hub.emit(conversation_id, None, EventBody::TurnFinished { running_threads: 0 });
        return;
    }

    let provider = match hub
        .db
        .provider(&model.provider_id)
        .map_err(|e| e.to_string())
        .and_then(|p| p.ok_or_else(|| format!("no provider {}", model.provider_id)))
        .and_then(|p| hub.build_model_provider(&p, &model.model).map_err(|e| e.to_string()))
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
                let text = without_imitated_notes(&text);
                if !text.is_empty() {
                    self.hub.emit(&self.conversation_id, None, EventBody::AssistantMessage { text });
                }
            }
            AgentEvent::TextDelta { text } => {
                self.hub.emit_live(&self.conversation_id, None, EventBody::AssistantDelta { text });
            }
            AgentEvent::Usage { provider_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens } => {
                let cost = self.hub.db.record_usage(
                    Some(&self.conversation_id),
                    None,
                    &provider_id,
                    &model,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                );
                crate::limits::after_usage(&self.hub, &self.conversation_id, None, cost);
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
        Arc::new(SuggestDevice(ctx())),
        Arc::new(ListModels(ctx())),
        Arc::new(ListUsage(ctx())),
        Arc::new(crate::files::ReadThreadFile { hub: hub.clone(), conversation_id: conversation_id.to_string() }),
    ];
    tools.extend(crate::memory::memory_tools(hub, conversation_id));
    tools.extend(crate::registry::registry_tools(hub, conversation_id));
    tools.extend(crate::skills::skill_tools(hub, conversation_id));
    tools
}

struct Ctx {
    hub: Arc<Hub>,
    conversation_id: String,
}

fn str_arg(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// A list of strings, or one comma-separated string. Missing is an empty list.
fn string_list(input: &Value, key: &str) -> Result<Vec<String>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => {
            Ok(text.split(',').map(|part| part.trim().to_string()).filter(|part| !part.is_empty()).collect())
        }
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                let Some(text) = item.as_str() else {
                    return Err(format!("{key} must be a list of strings"));
                };
                out.push(text.to_string());
            }
            Ok(out)
        }
        Some(_) => Err(format!("{key} must be a list of strings")),
    }
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
                          or asks something. Call several times in one response to run work in parallel. \
                          Leave device out and set needs to let Grouper choose the device."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "title": {"type": "string", "description": "Short title shown to the user"},
                "device": {"type": "string", "description": "Device id or name, or \"hub\". Leave out and Grouper picks from needs and past results"},
                "goal": {"type": "string"},
                "context": {"type": "string", "description": "Everything the thread needs to know; it can't see this chat"},
                "constraints": {"type": "string"},
                "expected_output": {"type": "string"},
                "model": {"type": "string", "description": "Model id from list_models (optional; the registry chooses otherwise)"},
                "task_type": {"type": "string", "description": "code, research, writing, data or ops"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "e.g. [\"sensitive\"] for private data, [\"draft\"]"},
                "needs": {"type": "array", "items": {"type": "string"}, "description": "What the work requires: shell, gpu, xcode, files, folder:<name>, agent:<name> (for example agent:codex). A named device that lacks one is refused"},
                "hub_reason": {"type": "string", "description": "When device is hub: why it runs there (e.g. the target device is offline)"},
                "budget_usd": {"type": "number", "description": "Optional spending cap for this thread in USD (estimated). It can only lower the user's per-thread budget; the thread is stopped when it's reached"}
            }, "required": ["title", "goal"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let needs = match string_list(&input, "needs") {
            Ok(needs) => needs,
            Err(error) => return ToolOutput::err(error),
        };
        let req = ThreadRequest {
            title: str_arg(&input, "title"),
            device: str_arg(&input, "device"),
            goal: str_arg(&input, "goal"),
            context: str_arg(&input, "context"),
            constraints: str_arg(&input, "constraints"),
            expected_output: str_arg(&input, "expected_output"),
            model: input.get("model").and_then(Value::as_str).map(String::from),
            hub_reason: input.get("hub_reason").and_then(Value::as_str).map(String::from),
            task_type: input.get("task_type").and_then(Value::as_str).map(String::from),
            tags: input
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default(),
            needs,
            budget_usd: input.get("budget_usd").and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().trim_start_matches('$').parse().ok())),
        };
        match self.0.hub.start_thread(&self.0.conversation_id, req).await {
            Ok(t) => {
                let mut out = json!({"thread_id": t.id, "device": t.device_id, "model": t.model, "status": t.status});
                if let Some(budget) = self.0.hub.db.thread_budget(&t.id, &self.0.hub.db.limits()) {
                    out["budget_usd"] = json!(budget);
                }
                if let Some(reason) = &t.reason {
                    out["reason"] = json!(reason);
                }
                if t.status == crate::offline::WAITING_DEVICE {
                    out["note"] = json!(crate::offline::WAITING_NOTE);
                }
                to_json(out)
            }
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
            description: "Send a message to a running thread: answer its question or give it new instructions. \
                          A thread waiting for its device gets it in its hub part, or when it continues on the device."
                .into(),
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
            description: "Stop a running thread, or one waiting for its device.".into(),
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
            description: "List devices (including \"hub\"), whether they're online, the tools threads can \
                          use on each with their permission level, each device's scoped folders \
                          (`@<name>`, read or read_write), outcome stats per task type, and the needs \
                          each device satisfies (shell, gpu, xcode, files, folder:<name>, agent:<name>)."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _input: Value) -> ToolOutput {
        to_json(self.0.hub.device_list_rows().await)
    }
}

struct SuggestDevice(Ctx);
#[async_trait]
impl Tool for SuggestDevice {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "suggest_device".into(),
            description: "Rank devices for a task without starting a thread. Returns the device start_thread \
                          would pick, why, and the full ranking. Pass the same task_type and needs."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "task_type": {"type": "string", "description": "code, research, writing, data or ops"},
                "needs": {"type": "array", "items": {"type": "string"}, "description": "shell, gpu, xcode, files, folder:<name>, agent:<name> (for example agent:codex)"}
            }}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let needs = match string_list(&input, "needs") {
            Ok(needs) => needs,
            Err(error) => return ToolOutput::err(error),
        };
        let task_type = input.get("task_type").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
        match self.0.hub.suggest_device(task_type, &needs).await {
            Ok(suggestion) => to_json(suggestion),
            Err(error) => ToolOutput::err(error),
        }
    }
}

struct ListModels(Ctx);
#[async_trait]
impl Tool for ListModels {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_models".into(),
            description: "List registered models with prices (USD per million tokens), use cases, default roles, \
                          whether they're local, their stats by task type, and the user's priority rules."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _input: Value) -> ToolOutput {
        to_json(crate::registry::models_overview(&self.0.hub.db))
    }
}

struct ListUsage(Ctx);
#[async_trait]
impl Tool for ListUsage {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_usage".into(),
            description: "Read cost and availability information: Grouper's metered totals for the last 7 days, \
                          grouped by provider, plus the latest provider-reported balances, device subscription \
                          usage windows (Claude/Codex, with snapshot age and device online status) and, when \
                          the user set limits, what's left of each budget."
                .into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _input: Value) -> ToolOutput {
        let now = crate::db::now();
        let totals = match self.0.hub.db.usage_totals(now.saturating_sub(7 * 86_400), i64::MAX as u64, "provider", None, None) {
            Ok(rows) => rows,
            Err(error) => return ToolOutput::err(error.to_string()),
        };
        let snapshots = match self.0.hub.db.latest_usage_snapshots() {
            Ok(rows) => rows,
            Err(error) => return ToolOutput::err(error.to_string()),
        };
        let devices = self.0.hub.devices().await;
        let enriched: Vec<Value> = snapshots
            .into_iter()
            .map(|snap| {
                let mut row = serde_json::to_value(&snap).unwrap_or_else(|_| json!({}));
                if let Some(obj) = row.as_object_mut() {
                    obj.insert("age_secs".into(), json!(now.saturating_sub(snap.fetched_at)));
                    if let Some(dev_id) = snap.device_id.as_deref()
                        && let Some(dev) = devices.iter().find(|d| d.id == dev_id)
                    {
                        obj.insert("device_name".into(), json!(dev.name));
                        obj.insert("device_online".into(), json!(dev.online));
                    }
                }
                row
            })
            .collect();
        let mut out = json!({"period": "last_7_days", "totals": totals, "latest_snapshots": enriched});
        if let Some(limits) = crate::limits::overview(&self.0.hub, &self.0.conversation_id).await {
            out["limits"] = limits;
        }
        to_json(out)
    }
}

/// Small models sometimes copy the bracketed notes Grouper adds to the conversation
/// (`[Thread t1a2b "x" done]`, `[Status] ...`, `[Memory]`, `[Skills]`) into their own
/// replies. Drop those lines from what the user sees; the model's history is untouched.
fn without_imitated_notes(text: &str) -> String {
    let imitated = |line: &str| {
        let l = line.trim_start();
        ["[Status]", "[Memory]", "[Skills]", "[Thread t", "[The user stepped into thread"]
            .iter()
            .any(|p| l.starts_with(p))
    };
    if !text.lines().any(imitated) {
        return text.to_string();
    }
    text.lines().filter(|l| !imitated(l)).collect::<Vec<_>>().join("
").trim().to_string()
}

#[cfg(test)]
mod imitated_notes_tests {
    use super::without_imitated_notes;

    #[test]
    fn drops_copied_grouper_notes_only() {
        let text = "I've started it.
[Thread t2ebaabc2 (Gaming PC) is running: \"Sea Poem\"]";
        assert_eq!(without_imitated_notes(text), "I've started it.");
        assert_eq!(without_imitated_notes("[Memory]
* likes tea"), "* likes tea");
        let normal = "Use [brackets] freely.
- [x] done";
        assert_eq!(without_imitated_notes(normal), normal);
        assert_eq!(without_imitated_notes("[Status] Devices: none."), "");
    }
}

#[cfg(test)]
mod harness_rule_tests {
    use super::{Wake, resolve_turn_model, run_turn};
    use crate::db::Db;
    use crate::server::Hub;
    use core_protocol::{EventBody, ModelEntry, ProviderConfig};
    use serde_json::json;

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-orch-h2-{tag}-{}", crate::db::random_hex(4)))
    }

    #[tokio::test]
    async fn project_turn_refuses_legacy_non_orchestrating_harness_and_falls_back_to_api_default() {
        let dir = tmp("fallback");
        let db = Db::open(&dir).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();

        db.upsert_provider(&ProviderConfig {
            id: "scripted".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({
                "inline": {
                    "rules": [
                        {"last": "hello project", "reply": [{"type": "text", "text": "Handled by API fallback."}]}
                    ]
                }
            }),
        })
        .unwrap();
        let codex_pid = format!("codex@{}", pc.id);
        db.upsert_provider(&ProviderConfig {
            id: codex_pid.clone(),
            kind: "harness".into(),
            base_url: None,
            api_key: None,
            options: json!({"harness": "codex", "via_device": pc.id, "signed_in": true}),
        })
        .unwrap();
        let claude_pid = format!("claude-code@{}", pc.id);
        db.upsert_provider(&ProviderConfig {
            id: claude_pid.clone(),
            kind: "harness".into(),
            base_url: None,
            api_key: None,
            options: json!({"harness": "claude-code", "via_device": pc.id, "signed_in": true}),
        })
        .unwrap();

        db.upsert_model(&ModelEntry {
            id: "api-orch".into(),
            provider_id: "scripted".into(),
            model: "scripted-api".into(),
            roles: vec!["orchestrator".into()],
            ..Default::default()
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "codex-o3".into(),
            provider_id: codex_pid,
            model: "o3".into(),
            kind: "harness".into(),
            device_id: Some(pc.id.clone()),
            can_orchestrate: false,
            roles: vec!["standard".into()],
            ..Default::default()
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "cc-opus".into(),
            provider_id: claude_pid,
            model: "opus".into(),
            kind: "harness".into(),
            device_id: Some(pc.id.clone()),
            can_orchestrate: true,
            roles: vec![],
            ..Default::default()
        })
        .unwrap();

        // Simulate a legacy project chat whose DB row points at a non-orchestrating harness.
        let project_conv = db.create_conversation("project", "Legacy project").unwrap();
        db.c()
            .execute("UPDATE conversations SET model = 'codex-o3' WHERE id = ?1", [&project_conv])
            .unwrap();

        let hub = Hub::new(db, dir.clone());
        run_turn(&hub, &project_conv, vec![Wake::User("hello project".into())]).await;

        let events = hub.db.events(&project_conv, 0, None);
        assert!(
            events.iter().any(|e| matches!(
                &e.body,
                EventBody::Error { message }
                    if message.contains("codex-o3 is a harness and can't orchestrate a project chat")
                    && message.contains("using the default model instead")
            )),
            "expected harness fallback error event, got: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                &e.body,
                EventBody::AssistantMessage { text } if text.contains("Handled by API fallback.")
            )),
            "expected turn to finish on API default, got: {events:?}"
        );

        // A project chat pinned to a harness WITH can_orchestrate = true is NOT rejected by resolve_turn_model.
        let orch_harness_conv = hub.db.create_conversation("project", "Confinable harness project").unwrap();
        hub.db.set_conversation_model(&orch_harness_conv, Some("cc-opus")).unwrap();
        let resolved = resolve_turn_model(&hub, &orch_harness_conv, true).await;
        assert_eq!(resolved.unwrap().id, "cc-opus");
        assert!(hub.db.events(&orch_harness_conv, 0, None).is_empty());

        // A standard chat pinned to a harness WITHOUT can_orchestrate IS allowed by resolve_turn_model.
        let standard_conv = hub.db.create_conversation("standard", "Standard harness").unwrap();
        hub.db.set_conversation_model(&standard_conv, Some("codex-o3")).unwrap();
        let resolved_std = resolve_turn_model(&hub, &standard_conv, false).await;
        assert_eq!(resolved_std.unwrap().id, "codex-o3");
        assert!(hub.db.events(&standard_conv, 0, None).is_empty());

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn project_turn_stops_cleanly_when_only_non_orchestrating_harnesses_exist() {
        let dir = tmp("only-harness");
        let db = Db::open(&dir).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();
        let codex_pid = format!("codex@{}", pc.id);
        db.upsert_provider(&ProviderConfig {
            id: codex_pid.clone(),
            kind: "harness".into(),
            base_url: None,
            api_key: None,
            options: json!({"harness": "codex", "via_device": pc.id, "signed_in": true}),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "codex-o3".into(),
            provider_id: codex_pid,
            model: "o3".into(),
            kind: "harness".into(),
            device_id: Some(pc.id.clone()),
            can_orchestrate: false,
            roles: vec!["standard".into()],
            ..Default::default()
        })
        .unwrap();

        let project_conv = db.create_conversation("project", "No API model").unwrap();
        let hub = Hub::new(db, dir.clone());
        run_turn(&hub, &project_conv, vec![Wake::User("hello".into())]).await;

        let events = hub.db.events(&project_conv, 0, None);
        assert!(
            events.iter().any(|e| matches!(
                &e.body,
                EventBody::Error { message } if message.contains("orchestrate a project chat")
            )),
            "expected clear error event, got: {events:?}"
        );
        assert!(events.iter().any(|e| matches!(&e.body, EventBody::TurnFinished { running_threads: 0 })));

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
