//! The loop engine shared by every Grouper agent: the orchestrator and standard chat
//! on the hub, and subagent threads on devices or the hub.
//!
//! 1. Send the history and tool definitions to the model.
//! 2. Run every tool it asked for, concurrently, each through the [`Hooks::gate`].
//! 3. Append the results and repeat until the model stops asking for tools.
//!
//! History is only ever appended to, so prompt caching and preserved thinking work.

use std::sync::Arc;

use async_trait::async_trait;
use core_protocol::AgentEvent;
use providers::{Block, Delta, Message, Provider, Request, Role, StopReason, ToolDef, Usage};
use serde_json::Value;

pub mod agents;
pub mod files;
pub mod folders;
pub mod permission;
pub mod thread;
pub mod tools;

pub use providers;

/// Longest tool output handed back to a model.
pub const MAX_TOOL_OUTPUT: usize = 30_000;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }
    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDef;
    async fn call(&self, input: Value) -> ToolOutput;
}

pub enum Gate {
    Allow,
    Deny(String),
}

/// How a loop reports progress and takes outside input.
#[async_trait]
pub trait Hooks: Send + Sync {
    fn event(&self, event: AgentEvent);
    /// Decide whether a tool call may run (approvals live here). It may rewrite the
    /// arguments first (clamping a CLI agent's safety options); the `ToolCall` event,
    /// the approval request and the call itself all see the rewritten arguments.
    async fn gate(&self, _tool: &str, _input: &mut Value) -> Gate {
        Gate::Allow
    }
    /// Messages to add before the next model call (user stepping in, etc.).
    fn inbox(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Clone, Debug)]
pub struct LoopConfig {
    pub provider_id: String,
    pub model: String,
    pub system: String,
    pub max_turns: u32,
    pub max_tokens: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ending {
    /// The model gave a final answer.
    Done,
    MaxTurns,
    Refused,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct LoopOutcome {
    pub final_text: String,
    pub usage: Usage,
    pub ending: Ending,
}

/// Add text to the history as the user, merging into a trailing user message so
/// roles keep alternating (and tool results stay in one message).
pub fn push_user_text(history: &mut Vec<Message>, text: String) {
    match history.last_mut() {
        Some(m) if m.role == Role::User => m.content.push(Block::Text { text }),
        _ => history.push(Message::user_text(text)),
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_TOOL_OUTPUT {
        let mut cut = MAX_TOOL_OUTPUT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        let dropped = s.len() - cut;
        s.truncate(cut);
        s.push_str(&format!("\n… [{dropped} more bytes truncated]"));
    }
    s
}

pub async fn run_loop(
    provider: &dyn Provider,
    cfg: &LoopConfig,
    tools: &[Arc<dyn Tool>],
    history: &mut Vec<Message>,
    hooks: &dyn Hooks,
) -> LoopOutcome {
    let defs: Vec<ToolDef> = tools.iter().map(|t| t.def()).collect();
    let mut usage = Usage::default();
    let mut last_text = String::new();

    for _ in 0..cfg.max_turns {
        for text in hooks.inbox() {
            push_user_text(history, text);
        }
        let req = Request {
            model: cfg.model.clone(),
            system: cfg.system.clone(),
            messages: history.clone(),
            tools: defs.clone(),
            max_tokens: cfg.max_tokens,
        };
        // Text reaches clients as it is generated; the full message follows as Text.
        let on_delta = |d: Delta| {
            if let Delta::Text(text) = d {
                hooks.event(AgentEvent::TextDelta { text });
            }
        };
        let resp = match provider.complete_stream(&req, &on_delta).await {
            Ok(r) => r,
            Err(e) => return LoopOutcome { final_text: last_text, usage, ending: Ending::Error(e.to_string()) },
        };
        let text = resp.message.text();
        if !text.is_empty() {
            hooks.event(AgentEvent::Text { text: text.clone() });
            last_text = text;
        }
        usage.add(&resp.usage);
        hooks.event(AgentEvent::Usage {
            provider_id: cfg.provider_id.clone(),
            // A free model swapped for another on the same key bills (and counts) as the one that answered.
            model: resp.model.clone().unwrap_or_else(|| cfg.model.clone()),
            input_tokens: resp.usage.input_tokens,
            output_tokens: resp.usage.output_tokens,
            cache_read_tokens: resp.usage.cache_read_tokens,
            cache_write_tokens: resp.usage.cache_write_tokens,
        });
        let calls: Vec<(String, String, Value)> = resp
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect();
        history.push(resp.message);

        if resp.stop == StopReason::Refusal {
            return LoopOutcome { final_text: last_text, usage, ending: Ending::Refused };
        }
        if calls.is_empty() {
            return LoopOutcome { final_text: last_text, usage, ending: Ending::Done };
        }

        // Run every requested tool concurrently; results go back in one message, in order.
        let runs = calls.iter().map(|(id, name, input)| async move {
            let mut input = input.clone();
            let out = match tools.iter().find(|t| t.def().name == *name) {
                None => {
                    hooks.event(AgentEvent::ToolCall { call_id: id.clone(), name: name.clone(), input });
                    ToolOutput::err(format!("unknown tool {name}"))
                }
                Some(tool) => {
                    // The gate runs first, so the log shows the arguments that actually run.
                    let gate = hooks.gate(name, &mut input).await;
                    hooks.event(AgentEvent::ToolCall { call_id: id.clone(), name: name.clone(), input: input.clone() });
                    match gate {
                        Gate::Deny(reason) => ToolOutput::err(format!("Not run: {reason}")),
                        Gate::Allow => tool.call(input).await,
                    }
                }
            };
            let content = truncate(out.content);
            hooks.event(AgentEvent::ToolResult {
                call_id: id.clone(),
                name: name.clone(),
                ok: !out.is_error,
                output: content.clone(),
            });
            Block::ToolResult { tool_use_id: id.clone(), content, is_error: out.is_error }
        });
        let results = futures_util::future::join_all(runs).await;
        history.push(Message { role: Role::User, content: results, raw: None });
    }
    LoopOutcome { final_text: last_text, usage, ending: Ending::MaxTurns }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_protocol::ProviderConfig;
    use serde_json::json;
    use std::sync::Mutex;

    struct Echo;
    #[async_trait]
    impl Tool for Echo {
        fn def(&self) -> ToolDef {
            ToolDef { name: "echo".into(), description: "echo".into(), input_schema: json!({"type": "object"}) }
        }
        async fn call(&self, input: Value) -> ToolOutput {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            ToolOutput::ok(format!("echoed {input}"))
        }
    }

    #[derive(Default)]
    struct Rec(Mutex<Vec<AgentEvent>>);
    #[async_trait]
    impl Hooks for Rec {
        fn event(&self, e: AgentEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    #[tokio::test]
    async fn parallel_tools_then_answer() {
        let provider = providers::build(&ProviderConfig {
            id: "s".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {"rules": [
                {"last": "go", "reply": [
                    {"type": "tool_use", "name": "echo", "input": {"n": 1}},
                    {"type": "tool_use", "name": "echo", "input": {"n": 2}}
                ]},
                {"last": "echoed", "reply": [{"type": "text", "text": "all done"}]}
            ]}}),
        })
        .unwrap();
        let cfg = LoopConfig { provider_id: "s".into(), model: "m".into(), system: "".into(), max_turns: 5, max_tokens: 100 };
        let mut history = vec![Message::user_text("go")];
        let rec = Rec::default();
        let started = std::time::Instant::now();
        let out = run_loop(provider.as_ref(), &cfg, &[Arc::new(Echo)], &mut history, &rec).await;
        assert_eq!(out.ending, Ending::Done);
        assert_eq!(out.final_text, "all done");
        // two 200 ms tools ran concurrently
        assert!(started.elapsed() < std::time::Duration::from_millis(390));
        assert_eq!(history.len(), 4);
        let results = &history[2].content;
        assert_eq!(results.len(), 2);

        // The answer streamed in pieces, then arrived whole.
        let events = rec.0.lock().unwrap();
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| if let AgentEvent::TextDelta { text } = e { Some(text.as_str()) } else { None })
            .collect();
        assert!(deltas.len() > 1);
        assert_eq!(deltas.concat(), "all done");
        let last_delta = events.iter().rposition(|e| matches!(e, AgentEvent::TextDelta { .. })).unwrap();
        let text = events.iter().position(|e| matches!(e, AgentEvent::Text { .. })).unwrap();
        let last_usage = events.iter().rposition(|e| matches!(e, AgentEvent::Usage { .. })).unwrap();
        assert!(last_delta < text);
        assert!(text < last_usage);
    }
}
