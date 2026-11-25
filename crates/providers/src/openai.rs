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

impl OpenAi {
    fn request(&self, req: &Request, stream: bool) -> (reqwest::RequestBuilder, Value) {
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
        if stream {
            body["stream"] = json!(true);
            body["stream_options"] = json!({"include_usage": true});
        }
        let mut http = self.client.post(format!("{}/chat/completions", self.base.trim_end_matches('/')));
        if let Some(k) = &self.key {
            http = http.bearer_auth(k);
        }
        (http, body)
    }
}

/// A chat completion body (or a streamed one, reassembled) as a [`Response`].
fn parse_completion(provider: &str, v: &Value) -> Result<Response, ProviderError> {
    let bad = |m: String| ProviderError::BadResponse { provider: provider.into(), message: m };
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

#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Rebuilds a non-streaming chat completion from `chat.completion.chunk` events.
#[derive(Default)]
struct ChunkAssembler {
    content: String,
    /// Tool calls by their `index`, which is what ties fragments together.
    calls: std::collections::BTreeMap<u64, PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    done: bool,
}

impl ChunkAssembler {
    /// Handle one event's data. Returns true at `[DONE]`.
    fn handle(&mut self, provider: &str, data: &str, on_delta: OnDelta<'_>) -> Result<bool, ProviderError> {
        let bad = |m: String| ProviderError::BadResponse { provider: provider.into(), message: m };
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(true);
        }
        let v: Value = serde_json::from_str(data).map_err(|e| bad(format!("bad stream chunk: {e}")))?;
        if let Some(err) = v.get("error") {
            return Err(bad(err.to_string()));
        }
        // With include_usage, the last chunk has usage and no choices. Some servers
        // send usage on every chunk (cumulative), some never.
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| {
            c.iter().find(|c| c.get("index").and_then(Value::as_u64).unwrap_or(0) == 0)
        }) else {
            return Ok(false);
        };
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
        if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            self.content.push_str(t);
            on_delta(Delta::Text(t.into()));
        }
        // DeepSeek and OpenRouter stream reasoning separately.
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta.get(key).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                on_delta(Delta::Thinking(t.into()));
            }
        }
        for (pos, tc) in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate() {
            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(pos as u64);
            let call = self.calls.entry(index).or_default();
            if let Some(id) = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                call.id = Some(id.to_string());
            }
            if let Some(n) = tc.pointer("/function/name").and_then(Value::as_str) {
                call.name.push_str(n);
            }
            if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                call.arguments.push_str(a);
            }
        }
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(f.to_string());
        }
        Ok(false)
    }

    /// The completion as the non-streaming API would have returned it.
    fn finish(self, provider: &str) -> Result<Value, ProviderError> {
        // Some servers close without `[DONE]`; a finish_reason is enough.
        if !self.done && self.finish_reason.is_none() {
            return Err(ProviderError::Network {
                provider: provider.into(),
                message: "the response stream ended before it finished".into(),
            });
        }
        let mut message = json!({
            "role": "assistant",
            "content": if self.content.is_empty() { Value::Null } else { json!(self.content) },
        });
        if !self.calls.is_empty() {
            message["tool_calls"] = self
                .calls
                .into_values()
                .map(|c| {
                    let mut call = json!({"type": "function", "function": {"name": c.name, "arguments": c.arguments}});
                    if let Some(id) = c.id {
                        call["id"] = json!(id);
                    }
                    call
                })
                .collect();
        }
        Ok(json!({
            "choices": [{"index": 0, "message": message, "finish_reason": self.finish_reason}],
            "usage": self.usage,
        }))
    }
}

#[async_trait]
impl Provider for OpenAi {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let (http, body) = self.request(req, false);
        let v = post_json(&self.id, http, &body).await?;
        parse_completion(&self.id, &v)
    }

    async fn complete_stream(&self, req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
        let (http, body) = self.request(req, true);
        let mut asm = ChunkAssembler::default();
        let streamed = sse::post_sse(&self.id, http, &body, |ev| asm.handle(&self.id, &ev.data, on_delta)).await?;
        match streamed {
            sse::Streamed::Json(v) => {
                // The server ignored "stream": show the whole text at once.
                let resp = parse_completion(&self.id, &v)?;
                let text = resp.message.text();
                if !text.is_empty() {
                    on_delta(Delta::Text(text));
                }
                Ok(resp)
            }
            sse::Streamed::Events => parse_completion(&self.id, &asm.finish(&self.id)?),
        }
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

    fn assemble(events: &[&str]) -> (Result<Value, ProviderError>, Vec<Delta>) {
        let deltas = std::sync::Mutex::new(Vec::new());
        let on = |d: Delta| deltas.lock().unwrap().push(d);
        let mut asm = ChunkAssembler::default();
        let mut res = Ok(());
        for e in events {
            match asm.handle("openai", e, &on) {
                Ok(true) => break,
                Ok(false) => {}
                Err(err) => {
                    res = Err(err);
                    break;
                }
            }
        }
        (res.and_then(|()| asm.finish("openai")), deltas.into_inner().unwrap())
    }

    #[test]
    fn stream_assembles_text_and_tool_calls_by_index() {
        let events = [
            r#"{"id":"c","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}],"usage":null}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"hmm"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Let me "},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"look."},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"list_files","arguments":"{}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\": "}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":null}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":40}}}"#,
            "[DONE]",
        ];
        let (v, deltas) = assemble(&events);
        let resp = parse_completion("openai", &v.unwrap()).unwrap();
        assert_eq!(
            resp.message.content,
            vec![
                Block::Text { text: "Let me look.".into() },
                Block::ToolUse { id: "call_a".into(), name: "read_file".into(), input: json!({"path": "a.txt"}) },
                Block::ToolUse { id: "call_b".into(), name: "list_files".into(), input: json!({}) },
            ]
        );
        assert_eq!(resp.stop, StopReason::ToolUse);
        assert_eq!(resp.usage, Usage { input_tokens: 60, output_tokens: 20, cache_read_tokens: 40, cache_write_tokens: 0 });
        assert_eq!(
            deltas,
            vec![Delta::Thinking("hmm".into()), Delta::Text("Let me ".into()), Delta::Text("look.".into())]
        );
    }

    #[test]
    fn stream_without_usage_or_done() {
        // Servers that ignore include_usage and close without [DONE].
        let events = [
            r#"{"choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        ];
        let (v, _) = assemble(&events);
        let resp = parse_completion("openai", &v.unwrap()).unwrap();
        assert_eq!(resp.message.text(), "Hi");
        assert_eq!(resp.stop, StopReason::EndTurn);
        assert_eq!(resp.usage, Usage::default());

        // Cut off mid-answer: an error, not a short reply.
        let (v, _) = assemble(&events[..1]);
        assert!(matches!(v, Err(ProviderError::Network { .. })));

        // An error object in the stream.
        let (v, _) = assemble(&[r#"{"error":{"message":"boom"}}"#]);
        assert!(matches!(v, Err(ProviderError::BadResponse { .. })));
    }
}
