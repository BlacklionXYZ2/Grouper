//! A fake model for tests and demos: replies come from a JSON script.
//!
//! ```json
//! { "rules": [
//!     { "system": "orchestrator", "last": "haiku",
//!       "reply": [ {"type": "tool_use", "name": "start_thread", "input": {...}} ] },
//!     { "last": "finished", "reply": [ {"type": "text", "text": "Done."} ] }
//!   ],
//!   "default": [ {"type": "text", "text": "OK"} ] }
//! ```
//! The first rule whose `system` and `last` substrings both match (system prompt and
//! the text of the last message, tool results included) wins. Stateless, so the
//! same script drives the hub and every device. In tool inputs, `$THREAD_ID` is
//! replaced by the first `Thread t…` id mentioned in the last message.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::*;

pub struct Scripted {
    script: Value,
    counter: AtomicU64,
}

impl Scripted {
    pub fn new(cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        let script = if let Some(inline) = cfg.options.get("inline") {
            inline.clone()
        } else {
            let path = opt_str(&cfg.options, "script")
                .ok_or_else(|| ProviderError::Config("scripted provider needs options.script or options.inline".into()))?;
            let text = std::fs::read_to_string(path).map_err(|e| ProviderError::Config(format!("{path}: {e}")))?;
            serde_json::from_str(&text).map_err(|e| ProviderError::Config(format!("{path}: {e}")))?
        };
        Ok(Self { script, counter: AtomicU64::new(1) })
    }
}

fn last_text(m: Option<&Message>) -> String {
    m.map(|m| {
        m.content
            .iter()
            .map(|b| match b {
                Block::Text { text } => text.clone(),
                Block::ToolResult { content, .. } => content.clone(),
                Block::ToolUse { name, .. } => name.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
    .unwrap_or_default()
}

/// The id in the first "Thread t1a2b3c4d" mention.
fn thread_id_in(text: &str) -> Option<String> {
    let start = text.find("Thread t")? + "Thread ".len();
    let id: String = text[start..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (id.len() > 1).then_some(id)
}

fn substitute(v: &Value, thread_id: &Option<String>) -> Value {
    match (v, thread_id) {
        (Value::String(s), Some(id)) => Value::String(s.replace("$THREAD_ID", id)),
        (Value::Object(m), _) => Value::Object(m.iter().map(|(k, v)| (k.clone(), substitute(v, thread_id))).collect()),
        (Value::Array(a), _) => Value::Array(a.iter().map(|v| substitute(v, thread_id)).collect()),
        _ => v.clone(),
    }
}

#[async_trait]
impl Provider for Scripted {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let last = last_text(req.messages.last());
        let matches = |rule: &Value, key: &str, hay: &str| {
            rule.get(key).and_then(Value::as_str).is_none_or(|needle| hay.contains(needle))
        };
        let reply = self
            .script
            .get("rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|r| matches(r, "system", &req.system) && matches(r, "last", &last))
            .and_then(|r| r.get("reply"))
            .or_else(|| self.script.get("default"))
            .cloned()
            .unwrap_or(json!([{"type": "text", "text": "(no scripted reply)"}]));

        let mentioned = thread_id_in(&last);
        let mut content = Vec::new();
        for b in reply.as_array().into_iter().flatten() {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_use") => content.push(Block::ToolUse {
                    id: format!("call_{}", self.counter.fetch_add(1, Ordering::Relaxed)),
                    name: b.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    input: substitute(&b.get("input").cloned().unwrap_or(json!({})), &mentioned),
                }),
                _ => content.push(Block::Text {
                    text: b.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                }),
            }
        }
        if let Some(ms) = self.script.get("delay_ms").and_then(Value::as_u64) {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        let has_calls = content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        let chars: usize = req.messages.iter().map(|m| last_text(Some(m)).len()).sum::<usize>() + req.system.len();
        Ok(Response {
            message: Message { role: Role::Assistant, content, raw: None },
            stop: if has_calls { StopReason::ToolUse } else { StopReason::EndTurn },
            usage: Usage { input_tokens: (chars / 4) as u64, output_tokens: 20, ..Default::default() },
        })
    }
}
