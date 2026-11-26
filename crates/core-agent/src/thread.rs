//! Runs one subagent thread: the loop, its tools, approvals, `ask_orchestrator`,
//! messages from the user or orchestrator, and stopping. Used on devices and on the hub.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use core_protocol::{AgentEvent, Permission, ThreadSpec, ToolDef, DEFAULT_ASK_TIMEOUT_SECS};
use providers::{Message, Provider};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};

use crate::permission::check_call;
use crate::tools::Workspace;
use crate::{Ending, Gate, Hooks, LoopConfig, Tool, ToolOutput, run_loop};

/// How long `ask_orchestrator` waits when a [`ThreadSpec`] doesn't say otherwise.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(DEFAULT_ASK_TIMEOUT_SECS);

/// What came back from an approval request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approved,
    /// The user said no, or the request was dropped (the thread stopped, the device left).
    Denied,
    /// Nobody answered within `secs`.
    TimedOut { secs: u64 },
}

/// "1 minute", "2 minutes", "1 second", "45 seconds".
pub fn approval_timeout_phrase(secs: u64) -> String {
    if secs > 0 && secs.is_multiple_of(60) {
        let n = secs / 60;
        format!("{n} minute{}", if n == 1 { "" } else { "s" })
    } else {
        let n = secs.max(1);
        format!("{n} second{}", if n == 1 { "" } else { "s" })
    }
}

/// The text after `Not run:` when an approval times out.
pub fn approval_timeout_reason(secs: u64) -> String {
    format!("no answer within {}", approval_timeout_phrase(secs))
}

/// What a thread needs from wherever it runs (a device connection, or the hub itself).
#[async_trait]
pub trait ThreadHost: Send + Sync + 'static {
    fn emit(&self, thread_id: &str, event: AgentEvent);
    /// Ask the user to approve a tool call. Resolves when they answer, or when the hub's
    /// approval timeout denies it.
    async fn approve(&self, thread_id: &str, tool: &str, input: &Value, reason: &str) -> ApprovalDecision;
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
    pending_asks: Mutex<VecDeque<(u64, oneshot::Sender<String>)>>,
    next_ask: AtomicU64,
}

/// Also what a retry of a denied call gets: the hub remembers denials per conversation
/// (until the user next speaks in it) and refuses a repeat without asking.
const DENIED: &str = "the user denied this call. Only the user can approve tool calls (the orchestrator can't), \
                      so don't retry it or ask anyone to allow it. Finish and report that it was declined, or do \
                      the task another way";

struct ThreadHooks {
    thread_id: String,
    permissions: std::collections::HashMap<String, Permission>,
    host: Arc<dyn ThreadHost>,
    state: Arc<State>,
    workspace: Arc<Workspace>,
}

#[async_trait]
impl Hooks for ThreadHooks {
    fn event(&self, event: AgentEvent) {
        self.host.emit(&self.thread_id, event);
    }
    async fn gate(&self, tool: &str, input: &mut Value) -> Gate {
        let level = self.permissions.get(tool).copied().unwrap_or_else(|| Permission::default_for(tool));
        // CLI agents never ask, so their safety arguments are set here, before anything
        // (the log, an approval request, the call) sees them.
        let Some(reason) = check_call(level, tool, input, &self.workspace) else {
            return Gate::Allow;
        };
        match self.host.approve(&self.thread_id, tool, input, &reason).await {
            ApprovalDecision::Approved => Gate::Allow,
            ApprovalDecision::Denied => Gate::Deny(DENIED.into()),
            ApprovalDecision::TimedOut { secs } => Gate::Deny(approval_timeout_reason(secs)),
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
    /// `None` waits until an answer arrives.
    timeout: Option<Duration>,
}

#[async_trait]
impl Tool for AskOrchestrator {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "ask_orchestrator".into(),
            description: "Ask the orchestrator for something you can't get here, such as information from \
                          another device or a decision. Waits for the answer. It can't approve tool calls: only \
                          the user can."
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
        let id = self.state.next_ask.fetch_add(1, Ordering::Relaxed);
        self.state.pending_asks.lock().unwrap().push_back((id, tx));
        self.host.ask_orchestrator(&self.thread_id, q);
        let answered = match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, rx).await {
                Ok(Ok(answer)) => Ok(answer),
                _ => Err(()),
            },
            None => rx.await.map_err(|_| ()),
        };
        match answered {
            Ok(answer) => ToolOutput::ok(answer),
            Err(()) => {
                self.state.pending_asks.lock().unwrap().retain(|(pending, _)| *pending != id);
                ToolOutput::err("the orchestrator didn't answer in time")
            }
        }
    }
}

/// Start a thread. `tools` are the device's tools; `ask_orchestrator` and `update_plan`
/// are added here, so hub and device threads get the same ones.
/// `workspace` is the thread's folder: the permission gate uses it to keep CLI agents in it.
pub fn spawn_thread(
    spec: ThreadSpec,
    provider: Arc<dyn Provider>,
    mut tools: Vec<Arc<dyn Tool>>,
    host: Arc<dyn ThreadHost>,
    workspace: Arc<Workspace>,
) -> (ThreadControl, tokio::task::JoinHandle<ThreadResult>) {
    let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel::<(String, String)>();
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let state = Arc::new(State {
        queue: Mutex::new(Vec::new()),
        pending_asks: Mutex::new(VecDeque::new()),
        next_ask: AtomicU64::new(1),
    });

    // Route incoming messages: an orchestrator reply answers a pending ask, everything
    // else is added before the thread's next model call.
    let st = state.clone();
    tokio::spawn(async move {
        while let Some((from, text)) = inbox_rx.recv().await {
            if from == "orchestrator"
                && let Some((_, tx)) = st.pending_asks.lock().unwrap().pop_front()
            {
                let _ = tx.send(text);
                continue;
            }
            st.queue.lock().unwrap().push(format!("[Message from the {from}]\n{text}"));
        }
    });

    let ask_timeout = spec.ask_timeout_secs.map(Duration::from_secs);
    tools.push(Arc::new(AskOrchestrator {
        thread_id: spec.thread_id.clone(),
        host: host.clone(),
        state: state.clone(),
        timeout: ask_timeout,
    }));
    tools.push(Arc::new(crate::plan::UpdatePlan));
    let hooks = ThreadHooks {
        thread_id: spec.thread_id.clone(),
        permissions: spec.permissions.clone(),
        host: host.clone(),
        state,
        workspace,
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
        // `biased` so a stop that lands as an approval resolves still lets this loop
        // record that decision (the tool result) before the stop drops it.
        tokio::select! {
            biased;
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use core_protocol::{AgentEvent, Permission, ProviderConfig, ThreadSpec};
    use serde_json::{Value, json};

    use super::*;
    use crate::tools::{Workspace, file_tools};

    struct Rec {
        events: Mutex<Vec<AgentEvent>>,
        decision: ApprovalDecision,
    }

    #[async_trait]
    impl ThreadHost for Rec {
        fn emit(&self, _: &str, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
        async fn approve(&self, _: &str, _: &str, _: &Value, _: &str) -> ApprovalDecision {
            self.decision.clone()
        }
        fn ask_orchestrator(&self, _: &str, _: &str) {}
    }

    #[derive(Default)]
    struct AskRec {
        events: Mutex<Vec<AgentEvent>>,
        questions: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ThreadHost for AskRec {
        fn emit(&self, _: &str, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
        async fn approve(&self, _: &str, _: &str, _: &Value, _: &str) -> ApprovalDecision {
            ApprovalDecision::Denied
        }
        fn ask_orchestrator(&self, _: &str, question: &str) {
            self.questions.lock().unwrap().push(question.to_string());
        }
    }

    fn spec(ask: Option<u64>) -> ThreadSpec {
        ThreadSpec {
            thread_id: "t1".into(),
            conversation_id: "c".into(),
            title: "T".into(),
            provider_id: "s".into(),
            model: "fake".into(),
            system: String::new(),
            brief: "go".into(),
            permissions: std::collections::HashMap::from([("list_files".into(), Permission::Manual)]),
            max_turns: 4,
            device_name: "Laptop".into(),
            ask_timeout_secs: ask,
            fallback_models: Vec::new(),
        }
    }

    fn provider(script: Value) -> Arc<dyn providers::Provider> {
        providers::build(&ProviderConfig {
            id: "s".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({ "inline": script }),
        })
        .unwrap()
    }

    #[test]
    fn timeout_phrases() {
        assert_eq!(approval_timeout_phrase(1), "1 second");
        assert_eq!(approval_timeout_phrase(45), "45 seconds");
        assert_eq!(approval_timeout_phrase(60), "1 minute");
        assert_eq!(approval_timeout_phrase(120), "2 minutes");
        assert_eq!(approval_timeout_reason(60), "no answer within 1 minute");
    }

    #[tokio::test]
    async fn a_timed_out_approval_is_not_run_and_the_thread_continues() {
        let dir = std::env::temp_dir().join(format!("grouper-approval-phrase-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let host = Arc::new(Rec { events: Mutex::new(Vec::new()), decision: ApprovalDecision::TimedOut { secs: 60 } });
        let script = json!({"rules": [
            {"last": "no answer within 1 minute", "reply": [{"type": "text", "text": "Continued."}]},
            {"reply": [{"type": "tool_use", "name": "list_files", "input": {}}]}
        ]});
        let (_control, handle) = spawn_thread(spec(Some(1800)), provider(script), file_tools(&ws), host.clone(), ws);
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap();
        assert_eq!(result.summary, "Continued.");
        let outputs: Vec<_> = host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs, vec!["Not run: no answer within 1 minute".to_string()]);
        std::fs::remove_dir_all(dir).ok();
    }

    /// The thread keeps no memory of denials: every call goes to the host, which (on the
    /// hub) remembers them per conversation and forgets them when the user next speaks in
    /// the chat. So a retry the orchestrator relays after the user said yes is asked again.
    /// The hub side is covered in `hub/tests/denials.rs`.
    #[tokio::test]
    async fn a_denied_call_is_asked_again_and_the_host_decides() {
        struct Count {
            decisions: Mutex<Vec<ApprovalDecision>>,
            events: Mutex<Vec<AgentEvent>>,
        }
        #[async_trait]
        impl ThreadHost for Count {
            fn emit(&self, _: &str, event: AgentEvent) {
                self.events.lock().unwrap().push(event);
            }
            async fn approve(&self, _: &str, _: &str, _: &Value, _: &str) -> ApprovalDecision {
                self.decisions.lock().unwrap().remove(0)
            }
            fn ask_orchestrator(&self, _: &str, _: &str) {}
        }
        let dir = std::env::temp_dir().join(format!("grouper-denied-again-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let host = Arc::new(Count {
            decisions: Mutex::new(vec![ApprovalDecision::Denied, ApprovalDecision::Approved]),
            events: Mutex::new(Vec::new()),
        });
        let script = json!({"rules": [
            {"last": "(empty)", "reply": [{"type": "text", "text": "Listed."}]},
            {"last": "user denied this call", "reply": [
                {"type": "tool_use", "name": "ask_orchestrator", "input": {"question": "May I retry?"}}]},
            {"last": "user says go ahead", "reply": [{"type": "tool_use", "name": "list_files", "input": {}}]},
            {"last": "go", "reply": [{"type": "tool_use", "name": "list_files", "input": {}}]}
        ]});
        let (control, handle) = spawn_thread(spec(Some(10)), provider(script), file_tools(&ws), host.clone(), ws);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let asked = host.events.lock().unwrap().iter().any(|e| {
                    matches!(e, AgentEvent::ToolCall { name, .. } if name == "ask_orchestrator")
                });
                if asked {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the thread never asked after the denial");
        control.send("orchestrator", "The user says go ahead.");
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap();
        assert_eq!(result.summary, "Listed.");
        assert!(host.decisions.lock().unwrap().is_empty(), "the retry must go to the host");
        let outputs: Vec<_> = host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { name, output, .. } if name == "list_files" => Some(output.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs.len(), 2, "{outputs:?}");
        assert!(outputs[0].contains("the user denied this call"), "{outputs:?}");
        assert!(outputs[0].contains("don't retry it or ask anyone to allow it"), "{outputs:?}");
        assert_eq!(outputs[1], "(empty)");
        std::fs::remove_dir_all(dir).ok();
    }

    /// `update_plan` is in every thread and never asks, even set to Manual.
    #[tokio::test]
    async fn update_plan_runs_without_approval() {
        let dir = std::env::temp_dir().join(format!("grouper-update-plan-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let host = Arc::new(Rec { events: Mutex::new(Vec::new()), decision: ApprovalDecision::Denied });
        let mut spec = spec(Some(10));
        spec.permissions.insert("update_plan".into(), Permission::Manual);
        let script = json!({"rules": [
            {"last": "1 of 2 done", "reply": [{"type": "text", "text": "Planned."}]},
            {"last": "steps are in_progress", "reply": [{"type": "tool_use", "name": "update_plan", "input": {"items": [
                {"content": "Read", "status": "completed"},
                {"content": "Write", "status": "in_progress", "active_form": "Writing"}]}}]},
            {"reply": [{"type": "tool_use", "name": "update_plan", "input": {"items": [
                {"content": "Read", "status": "in_progress"}, {"content": "Write", "status": "in_progress"}]}}]}
        ]});
        let (_control, handle) = spawn_thread(spec, provider(script), vec![], host.clone(), ws);
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap();
        assert_eq!(result.summary, "Planned.");
        let results: Vec<(bool, String)> = host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { name, ok, output, .. } if name == "update_plan" => Some((*ok, output.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2, "{results:?}");
        assert!(!results[0].0 && results[0].1.contains("at most one in_progress"), "{results:?}");
        assert_eq!(results[1], (true, "Plan updated: 1 of 2 done.".to_string()));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn ask_timeout_comes_from_the_spec() {
        let dir = std::env::temp_dir().join(format!("grouper-ask-timeout-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let host = Arc::new(Rec { events: Mutex::new(Vec::new()), decision: ApprovalDecision::Denied });
        let script = json!({"rules": [
            {"last": "didn't answer", "reply": [{"type": "text", "text": "Gave up."}]},
            {"reply": [{"type": "tool_use", "name": "ask_orchestrator", "input": {"question": "Which tone?"}}]}
        ]});
        let (_control, handle) = spawn_thread(spec(Some(1)), provider(script), vec![], host.clone(), ws);
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await.expect("ask did not time out").unwrap();
        assert_eq!(result.summary, "Gave up.");
        let outputs: Vec<_> = host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs, vec!["the orchestrator didn't answer in time".to_string()]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn ask_without_a_limit_does_not_give_up() {
        let dir = std::env::temp_dir().join(format!("grouper-ask-forever-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let host = Arc::new(Rec { events: Mutex::new(Vec::new()), decision: ApprovalDecision::Denied });
        let script = json!({"default": [{"type": "tool_use", "name": "ask_orchestrator", "input": {"question": "hi"}}]});
        let (control, mut handle) = spawn_thread(spec(None), provider(script), vec![], host.clone(), ws);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let asked = host.events.lock().unwrap().iter().any(|e| {
                    matches!(e, AgentEvent::ToolCall { name, .. } if name == "ask_orchestrator")
                });
                if asked {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ask never started");
        let still_waiting = tokio::time::timeout(Duration::from_millis(400), &mut handle).await;
        assert!(still_waiting.is_err(), "null ask_timeout_secs waits, it does not use a short default");
        control.stop();
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        assert!(!result.ok);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn parallel_asks_are_answered_oldest_first() {
        let dir = std::env::temp_dir().join(format!("grouper-parallel-asks-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let host = Arc::new(AskRec::default());
        let script = json!({"rules": [
            {"last": "first answer", "reply": [{"type": "text", "text": "Both answered."}]},
            {"last": "go", "reply": [
                {"type": "tool_use", "name": "ask_orchestrator", "input": {"question": "first?"}},
                {"type": "tool_use", "name": "ask_orchestrator", "input": {"question": "second?"}}
            ]}
        ]});
        let (control, handle) = spawn_thread(spec(Some(10)), provider(script), vec![], host.clone(), ws);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if host.questions.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("parallel asks never started");
        assert_eq!(*host.questions.lock().unwrap(), ["first?", "second?"]);
        control.send("orchestrator", "first answer");
        control.send("orchestrator", "second answer");
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await.unwrap().unwrap();
        assert_eq!(result.summary, "Both answered.");
        let outputs: Vec<String> = host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolResult { name, output, .. } if name == "ask_orchestrator" => Some(output.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(outputs, ["first answer", "second answer"]);
        std::fs::remove_dir_all(dir).ok();
    }
}
