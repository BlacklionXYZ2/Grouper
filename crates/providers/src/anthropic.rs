//! Claude Messages API (raw HTTP; there is no official Rust SDK).

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::*;

const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

pub struct Anthropic {
    id: String,
    base: String,
    key: String,
    fallbacks: bool,
    client: reqwest::Client,
}

impl Anthropic {
    pub fn new(cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        let key = cfg.api_key.clone().filter(|k| !k.is_empty()).ok_or_else(|| ProviderError::NoKey(cfg.id.clone()))?;
        Ok(Self {
            id: cfg.id.clone(),
            base: cfg.base_url.clone().unwrap_or_else(|| "https://api.anthropic.com".into()),
            key,
            // Server-side fallback when a safety classifier declines a request.
            fallbacks: opt_bool(&cfg.options, "fallbacks", true),
            client: http_client(),
        })
    }
}

fn block_to_json(b: &Block) -> Value {
    match b {
        Block::Text { text } => json!({"type": "text", "text": text}),
        Block::ToolUse { id, name, input } => json!({"type": "tool_use", "id": id, "name": name, "input": input}),
        Block::ToolResult { tool_use_id, content, is_error } => {
            json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
        }
    }
}

fn message_to_json(m: &Message) -> Value {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content = match &m.raw {
        // Replay exactly what Claude returned, thinking blocks included.
        Some(raw) if raw.kind == "anthropic" => raw.value.clone(),
        _ => Value::Array(m.content.iter().map(block_to_json).collect()),
    };
    json!({"role": role, "content": content})
}

pub(crate) fn parse_content(content: &Value) -> Vec<Block> {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| match b.get("type")?.as_str()? {
                    "text" => Some(Block::Text { text: b.get("text")?.as_str()?.to_string() }),
                    "tool_use" => Some(Block::ToolUse {
                        id: b.get("id")?.as_str()?.to_string(),
                        name: b.get("name")?.as_str()?.to_string(),
                        input: b.get("input").cloned().unwrap_or(json!({})),
                    }),
                    _ => None, // thinking etc. live only in `raw`
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl Provider for Anthropic {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": req.messages.iter().map(message_to_json).collect::<Vec<_>>(),
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools);
        }
        let mut http = self
            .client
            .post(format!("{}/v1/messages", self.base.trim_end_matches('/')))
            .header("x-api-key", &self.key)
            .header("anthropic-version", API_VERSION);
        if self.fallbacks {
            body["fallbacks"] = json!("default");
            http = http.header("anthropic-beta", FALLBACK_BETA);
        }

        let v = post_json(&self.id, http, &body).await?;
        let bad = |m: &str| ProviderError::BadResponse { provider: self.id.clone(), message: m.into() };
        let content = v.get("content").ok_or_else(|| bad("no content"))?;
        let stop = match v.get("stop_reason").and_then(Value::as_str).unwrap_or("") {
            "end_turn" | "stop_sequence" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "refusal" => StopReason::Refusal,
            other => StopReason::Other(other.to_string()),
        };
        let u = v.get("usage").cloned().unwrap_or(json!({}));
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        Ok(Response {
            message: Message {
                role: Role::Assistant,
                content: parse_content(content),
                raw: Some(RawContent { kind: "anthropic".into(), value: content.clone() }),
            },
            stop,
            usage: Usage {
                input_tokens: n("input_tokens"),
                output_tokens: n("output_tokens"),
                cache_read_tokens: n("cache_read_input_tokens"),
                cache_write_tokens: n("cache_creation_input_tokens"),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_content_is_replayed_verbatim() {
        let raw = json!([{"type": "thinking", "thinking": "", "signature": "sig"}, {"type": "text", "text": "hi"}]);
        let m = Message {
            role: Role::Assistant,
            content: parse_content(&raw),
            raw: Some(RawContent { kind: "anthropic".into(), value: raw.clone() }),
        };
        assert_eq!(m.content, vec![Block::Text { text: "hi".into() }]);
        assert_eq!(message_to_json(&m)["content"], raw);
    }
}
