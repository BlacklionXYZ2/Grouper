//! Runs one subagent thread: the loop, its tools, approvals, `ask_orchestrator`,
//! messages from the user or orchestrator, and stopping. Used on devices and on the hub.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use core_protocol::{AgentEvent, Permission, ThreadSpec, ToolDef};
use providers::{Message, Provider};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use crate::permission::needs_approval;
use crate::{Ending, Gate, Hooks, LoopConfig, Tool, ToolOutput, run_loop};

/// How long `ask_orchestrator` waits for an answer.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What a thread needs from wherever it runs (a device connection, or the hub itself).
#[async_trait]
pub trait ThreadHost: Send + Sync + 'static {
    fn emit(&self, thread_id: &str, event: AgentEvent);
    /// Ask the user to approve a tool call. Resolves when they answer.
    async fn approve(&self, thread_id: &str, tool: &str, input: &Value, reason: &str) -> bool;
    /// Send a question to the orchestrator; the answer arrives as a thread input from `orchestrator`.
    fn ask_orchestrator(&self, thread_id: &str, question: &str);
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadResult {
    pub ok: bool,
    pub summary: String,
}

/// Handle for steering a running thread.
#[derive(Clone)]
pub struct ThreadControl {
    inbox: mpsc::UnboundedSender<(String, String)>,
    stop: Arc<watch::Sender<bool>>,
}

impl ThreadControl {
    /// `from` is `user` or `orchestrator`.
    pub fn send(&self, from: &str, text: &str) {
        let _ = self.inbox.send((from.to_string(), text.to_string()));
    }
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

struct State {
    queue: Mutex<Vec<String>>,
    pending_ask: Mutex<Option<oneshot::Sender<String>>>,
}

struct ThreadHooks {
    thread_id: String,
    permissions: std::collections::HashMap<String, Permission>,
    host: Arc<dyn ThreadHost>,
    state: Arc<State>,
}

#[async_trait]
impl Hooks for ThreadHooks {
    fn event(&self, event: AgentEvent) {
        self.host.emit(&self.thread_id, event);
    }
    async fn gate(&self, tool: &str, input: &Value) -> Gate {
        let level = self.permissions.get(tool).copied().unwrap_or_else(|| Permission::default_for(tool));
        match needs_approval(level, tool, input) {
            None => Gate::Allow,
            Some(reason) => {
                if self.host.approve(&self.thread_id, tool, input, &reason).await {
                    Gate::Allow
                } else {
                    Gate::Deny("the user denied this call".into())
                }
            }
        }
    }
    fn inbox(&self) -> Vec<String> {
        std::mem::take(&mut *self.state.queue.lock().unwrap())
    }
}

struct AskOrchestrator {
    thread_id: String,
    host: Arc<dyn ThreadHost>,
    state: Arc<State>,
}

#[async_trait]
impl Tool for AskOrchestrator {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "ask_orchestrator".into(),
            description: "Ask the orchestrator for something you can't get here, such as information from \
                          another device or a decision. Waits for the answer."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "question": {"type": "string"}
            }, "required": ["question"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let Some(q) = input.get("question").and_then(Value::as_str) else {
            return ToolOutput::err("missing question");
        };
        let (tx, rx) = oneshot::channel();
        *self.state.pending_ask.lock().unwrap() = Some(tx);
        self.host.ask_orchestrator(&self.thread_id, q);
        match tokio::time::timeout(ASK_TIMEOUT, rx).await {
            Ok(Ok(answer)) => ToolOutput::ok(answer),
            _ => {
                self.state.pending_ask.lock().unwrap().take();
                ToolOutput::err("the orchestrator didn't answer in time")
            }
        }
    }
}

/// Start a thread. `tools` are the device's tools; `ask_orchestrator` is added here.
pub fn spawn_thread(
    spec: ThreadSpec,
    provider: Arc<dyn Provider>,
    mut tools: Vec<Arc<dyn Tool>>,
    host: Arc<dyn ThreadHost>,
) -> (ThreadControl, tokio::task::JoinHandle<ThreadResult>) {
    let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel::<(String, String)>();
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let state = Arc::new(State { queue: Mutex::new(Vec::new()), pending_ask: Mutex::new(None) });

    // Route incoming messages: an orchestrator reply answers a pending ask, everything
    // else is added before the thread's next model call.
    let st = state.clone();
    tokio::spawn(async move {
        while let Some((from, text)) = inbox_rx.recv().await {
            if from == "orchestrator"
                && let Some(tx) = st.pending_ask.lock().unwrap().take()
            {
                let _ = tx.send(text);
                continue;
            }
            st.queue.lock().unwrap().push(format!("[Message from the {from}]\n{text}"));
        }
    });

    tools.push(Arc::new(AskOrchestrator { thread_id: spec.thread_id.clone(), host: host.clone(), state: state.clone() }));
    let hooks = ThreadHooks {
        thread_id: spec.thread_id.clone(),
        permissions: spec.permissions.clone(),
        host: host.clone(),
        state,
    };
    let cfg = LoopConfig {
        provider_id: spec.provider_id.clone(),
        model: spec.model.clone(),
        system: spec.system.clone(),
        max_turns: spec.max_turns,
        max_tokens: 16_000,
    };

    let handle = tokio::spawn(async move {
        let mut history = vec![Message::user_text(spec.brief.clone())];
        let run = run_loop(provider.as_ref(), &cfg, &tools, &mut history, &hooks);
        tokio::select! {
            out = run => match out.ending {
                Ending::Done => ThreadResult { ok: true, summary: out.final_text },
                Ending::MaxTurns => ThreadResult {
                    ok: false,
                    summary: format!("Stopped after {} turns without finishing. Last message:\n{}", cfg.max_turns, out.final_text),
                },
                Ending::Refused => ThreadResult { ok: false, summary: "The model declined this task.".into() },
                Ending::Error(e) => ThreadResult { ok: false, summary: format!("Error: {e}") },
            },
            _ = stop_rx.wait_for(|s| *s) => ThreadResult { ok: false, summary: "Stopped.".into() },
        }
    });
    (ThreadControl { inbox: inbox_tx, stop: Arc::new(stop_tx) }, handle)
}
