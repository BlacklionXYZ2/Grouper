//! OpenAI-compatible Chat Completions: OpenAI, OpenRouter, DeepSeek, Qwen, Grok,
//! Ollama, LM Studio and anything else that speaks the same API.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::*;

pub struct OpenAi {
    id: String,
    base: String,
    key: Option<String>,
    max_tokens_field: String,
    client: reqwest::Client,
}

impl OpenAi {
    pub fn new(cfg: &ProviderConfig) -> Result<Self, ProviderError> {
        let base = cfg
            .base_url
            .clone()
            .or_else(|| (cfg.id == "openai").then(|| "https://api.openai.com/v1".to_string()))
            .or_else(|| (cfg.id == "openrouter").then(|| "https://openrouter.ai/api/v1".to_string()))
            .ok_or_else(|| ProviderError::Config(format!("provider {} needs a base_url", cfg.id)))?;
        let default_field = if cfg.id == "openai" { "max_completion_tokens" } else { "max_tokens" };
        Ok(Self {
            id: cfg.id.clone(),
            base,
            key: cfg.api_key.clone().filter(|k| !k.is_empty()),
            max_tokens_field: opt_str(&cfg.options, "max_tokens_field").unwrap_or(default_field).to_string(),
            client: http_client(),
        })
    }
}

fn to_messages(req: &Request) -> Vec<Value> {
    let mut out = Vec::new();
    if !req.system.is_empty() {
        out.push(json!({"role": "system", "content": req.system}));
    }
    for m in &req.messages {
        match m.role {
            Role::User => {
                // Tool results become `tool` messages; any text follows as a user message.
                let mut texts = Vec::new();
                for b in &m.content {
                    match b {
                        Block::ToolResult { tool_use_id, content, is_error } => {
                            let content = if *is_error { format!("Error: {content}") } else { content.clone() };
                            out.push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": content}));
                        }
                        Block::Text { text } => texts.push(text.clone()),
                        Block::ToolUse { .. } => {}
                    }
                }
                if !texts.is_empty() {
                    out.push(json!({"role": "user", "content": texts.join("\n\n")}));
                }
            }
            Role::Assistant => {
                let text = m.text();
                let calls: Vec<Value> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input } => Some(json!({
                            "id": id, "type": "function",
                            "function": {"name": name, "arguments": input.to_string()}
                        })),
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                out.push(msg);
            }
        }
    }
    out
}

#[async_trait]
impl Provider for OpenAi {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let mut body = json!({
            "model": req.model,
            "messages": to_messages(req),
        });
        body[&self.max_tokens_field] = json!(req.max_tokens);
        if !req.tools.is_empty() {
            body["tools"] = json!(req
                .tools
                .iter()
                .map(|t| json!({"type": "function", "function": {
                    "name": t.name, "description": t.description, "parameters": t.input_schema
                }}))
                .collect::<Vec<_>>());
        }
        let mut http = self.client.post(format!("{}/chat/completions", self.base.trim_end_matches('/')));
        if let Some(k) = &self.key {
            http = http.bearer_auth(k);
        }
        let v = post_json(&self.id, http, &body).await?;
        let bad = |m: String| ProviderError::BadResponse { provider: self.id.clone(), message: m };
        if let Some(err) = v.get("error") {
            return Err(bad(err.to_string()));
        }
        let choice = v.pointer("/choices/0").ok_or_else(|| bad(format!("no choices: {v}")))?;
        let msg = choice.get("message").cloned().unwrap_or(json!({}));

        let mut content = Vec::new();
        if let Some(t) = msg.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            content.push(Block::Text { text: t.to_string() });
        }
        for (i, call) in msg.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string();
            let args = call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
            let input = serde_json::from_str(args).unwrap_or_else(|_| json!({ "_unparsed_arguments": args }));
            let id = call.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| format!("call_{i}"));
            content.push(Block::ToolUse { id, name, input });
        }
        let has_calls = content.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        let stop = match choice.get("finish_reason").and_then(Value::as_str).unwrap_or("") {
            _ if has_calls => StopReason::ToolUse,
            "stop" | "" => StopReason::EndTurn,
            "length" => StopReason::MaxTokens,
            "content_filter" => StopReason::Refusal,
            other => StopReason::Other(other.to_string()),
        };
        let u = v.get("usage").cloned().unwrap_or(json!({}));
        let n = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
        let cached = n("/prompt_tokens_details/cached_tokens");
        Ok(Response {
            message: Message { role: Role::Assistant, content, raw: None },
            stop,
            usage: Usage {
                input_tokens: n("/prompt_tokens").saturating_sub(cached),
                output_tokens: n("/completion_tokens"),
                cache_read_tokens: cached,
                cache_write_tokens: 0,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_round_trip_shape() {
        let req = Request {
            model: "m".into(),
            system: "sys".into(),
            max_tokens: 10,
            tools: vec![],
            messages: vec![
                Message::user_text("hi"),
                Message {
                    role: Role::Assistant,
                    content: vec![Block::ToolUse { id: "c1".into(), name: "f".into(), input: json!({"a": 1}) }],
                    raw: None,
                },
                Message {
                    role: Role::User,
                    content: vec![
                        Block::ToolResult { tool_use_id: "c1".into(), content: "ok".into(), is_error: false },
                        Block::Text { text: "also".into() },
                    ],
                    raw: None,
                },
            ],
        };
        let m = to_messages(&req);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[4]["content"], "also");
    }
}
