//! Claude Messages API (raw HTTP; there is no official Rust SDK).

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::*;

const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_CONTENT_BLOCKS: usize = 1024;

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

impl Anthropic {
    fn request(&self, req: &Request, stream: bool) -> (reqwest::RequestBuilder, Value) {
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
        if stream {
            body["stream"] = json!(true);
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
        (http, body)
    }
}

/// A Messages API response body (or a streamed one, reassembled) as a [`Response`].
fn parse_message(provider: &str, v: &Value) -> Result<Response, ProviderError> {
    let bad = |m: &str| ProviderError::BadResponse { provider: provider.into(), message: m.into() };
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

/// Rebuilds the non-streaming message body from the Messages API's stream events.
#[derive(Default)]
struct StreamAssembler {
    message: Value,
    blocks: Vec<Value>,
    /// `input_json_delta` fragments per block index, parsed when the block stops.
    partial_json: std::collections::HashMap<usize, String>,
    text_blocks: usize,
    done: bool,
}

fn append_str(block: &mut Value, key: &str, s: &str) {
    match block.get_mut(key) {
        Some(Value::String(existing)) => existing.push_str(s),
        _ => block[key] = json!(s),
    }
}

/// An `error` event in the stream. Overloads and server errors stay retryable
/// (the retry wrapper still won't retry once text has been shown).
fn stream_error(provider: &str, err: &Value) -> ProviderError {
    let status = match err.get("type").and_then(Value::as_str).unwrap_or("") {
        "overloaded_error" => 529,
        "rate_limit_error" => 429,
        "api_error" => 500,
        "timeout_error" => 504,
        _ => return ProviderError::BadResponse { provider: provider.into(), message: err.to_string() },
    };
    ProviderError::Http { provider: provider.into(), status, body: err.to_string() }
}

fn block_index(provider: &str, v: &Value, context: &str) -> Result<usize, ProviderError> {
    let bad = |m: String| ProviderError::BadResponse { provider: provider.into(), message: m };
    let raw = v.get("index").and_then(Value::as_u64).ok_or_else(|| bad(format!("{context} without index: {v}")))?;
    let i = usize::try_from(raw).map_err(|_| bad(format!("{context} index {raw} out of range")))?;
    if i >= MAX_CONTENT_BLOCKS {
        return Err(bad(format!("{context} index {i} exceeds maximum of {MAX_CONTENT_BLOCKS} blocks")));
    }
    Ok(i)
}

impl StreamAssembler {
    /// Handle one event's data. Returns true at `message_stop`.
    fn handle(&mut self, provider: &str, v: &Value, on_delta: OnDelta<'_>) -> Result<bool, ProviderError> {
        let bad = |m: String| ProviderError::BadResponse { provider: provider.into(), message: m };
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => self.message = v.get("message").cloned().unwrap_or(json!({})),
            "content_block_start" => {
                let i = block_index(provider, v, "block start")?;
                let block = v.get("content_block").cloned().unwrap_or(json!({}));
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        // Message::text joins text blocks with '\n'; stream the same.
                        if self.text_blocks > 0 {
                            on_delta(Delta::Text("\n".into()));
                        }
                        self.text_blocks += 1;
                        if let Some(t) = block.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            on_delta(Delta::Text(t.into()));
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = block.get("thinking").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                            on_delta(Delta::Thinking(t.into()));
                        }
                    }
                    _ => {}
                }
                if self.blocks.len() <= i {
                    self.blocks.resize(i + 1, Value::Null);
                }
                self.blocks[i] = block;
            }
            "content_block_delta" => {
                let i = block_index(provider, v, "delta")?;
                let block = self
                    .blocks
                    .get_mut(i)
                    .filter(|b| !b.is_null())
                    .ok_or_else(|| bad(format!("delta for unknown block {i}")))?;
                let d = v.get("delta").cloned().unwrap_or(json!({}));
                let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                match d.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        let t = s("text");
                        append_str(block, "text", &t);
                        if !t.is_empty() {
                            on_delta(Delta::Text(t));
                        }
                    }
                    "thinking_delta" => {
                        let t = s("thinking");
                        append_str(block, "thinking", &t);
                        if !t.is_empty() {
                            on_delta(Delta::Thinking(t));
                        }
                    }
                    "signature_delta" => append_str(block, "signature", &s("signature")),
                    "input_json_delta" => self.partial_json.entry(i).or_default().push_str(&s("partial_json")),
                    "citations_delta" => {
                        if let Some(c) = d.get("citation") {
                            match block.get_mut("citations") {
                                Some(Value::Array(list)) => list.push(c.clone()),
                                _ => block["citations"] = json!([c]),
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let i = block_index(provider, v, "block stop")?;
                if let (Some(json), Some(block)) = (self.partial_json.remove(&i), self.blocks.get_mut(i)) {
                    block["input"] = if json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&json).unwrap_or_else(|_| json!({ "_unparsed_input": json }))
                    };
                }
            }
            "message_delta" => {
                if let Some(Value::Object(d)) = v.get("delta") {
                    for (k, val) in d {
                        self.message[k] = val.clone();
                    }
                }
                // Usage counts in message_delta are cumulative: they replace message_start's.
                if let Some(Value::Object(u)) = v.get("usage") {
                    if !self.message.get("usage").is_some_and(Value::is_object) {
                        self.message["usage"] = json!({});
                    }
                    for (k, val) in u.iter().filter(|(_, val)| !val.is_null()) {
                        self.message["usage"][k] = val.clone();
                    }
                }
            }
            "message_stop" => {
                self.done = true;
                return Ok(true);
            }
            "error" => return Err(stream_error(provider, v.get("error").unwrap_or(v))),
            _ => {} // ping, and event types added later
        }
        Ok(false)
    }

    /// The message body as the non-streaming API would have returned it.
    fn finish(mut self, provider: &str) -> Result<Value, ProviderError> {
        if !self.done {
            return Err(ProviderError::Network {
                provider: provider.into(),
                message: "the response stream ended before message_stop".into(),
            });
        }
        self.message["content"] = Value::Array(self.blocks.into_iter().filter(|b| !b.is_null()).collect());
        Ok(self.message)
    }
}

#[async_trait]
impl Provider for Anthropic {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let (http, body) = self.request(req, false);
        let v = post_json(&self.id, http, &body).await?;
        parse_message(&self.id, &v)
    }

    async fn complete_stream(&self, req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
        let (http, body) = self.request(req, true);
        let mut asm = StreamAssembler::default();
        let streamed = sse::post_sse(&self.id, http, &body, |ev| {
            let data: Value = serde_json::from_str(&ev.data).map_err(|e| ProviderError::BadResponse {
                provider: self.id.clone(),
                message: format!("bad stream event {:?}: {e}", ev.event),
            })?;
            asm.handle(&self.id, &data, on_delta)
        })
        .await?;
        match streamed {
            sse::Streamed::Json(v) => {
                // The server ignored "stream": show the whole text at once.
                let resp = parse_message(&self.id, &v)?;
                let text = resp.message.text();
                if !text.is_empty() {
                    on_delta(Delta::Text(text));
                }
                Ok(resp)
            }
            sse::Streamed::Events => parse_message(&self.id, &asm.finish(&self.id)?),
        }
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

    /// A recorded stream: thinking with a signature, two text blocks around a
    /// tool_use whose JSON input arrives in fragments, plus ping and usage.
    const RECORDED: &str = concat!(
        "event: message_start\r\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":7,\"cache_read_input_tokens\":300,\"output_tokens\":1}}}\r\n\r\n",
        "event: content_block_start\r\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\r\n\r\n",
        ": keep-alive comment\r\n\r\n",
        "event: ping\r\ndata: {\"type\": \"ping\"}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me \"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"check.\"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"EqQBCgYIARAB\"}}\r\n\r\n",
        "event: content_block_stop\r\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\r\n\r\n",
        "event: content_block_start\r\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking the \"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"weather — één moment.\"}}\r\n\r\n",
        "event: content_block_stop\r\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\r\n\r\n",
        "event: content_block_start\r\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"get_weather\",\"input\":{}}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\": \\\"Par\"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"is\\\", \\\"days\\\": 3}\"}}\r\n\r\n",
        "event: content_block_stop\r\n",
        "data: {\"type\":\"content_block_stop\",\"index\":2}\r\n\r\n",
        "event: content_block_start\r\n",
        "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\r\n\r\n",
        "event: content_block_delta\r\n",
        "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"text_delta\",\"text\":\"Done.\"}}\r\n\r\n",
        "event: content_block_stop\r\n",
        "data: {\"type\":\"content_block_stop\",\"index\":3}\r\n\r\n",
        "event: message_delta\r\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":87}}\r\n\r\n",
        "event: message_stop\r\n",
        "data: {\"type\":\"message_stop\"}\r\n\r\n",
    );

    /// The same message as the non-streaming API returns it.
    fn non_streaming() -> Value {
        json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
            "content": [
                {"type": "thinking", "thinking": "Let me check.", "signature": "EqQBCgYIARAB"},
                {"type": "text", "text": "Checking the weather — één moment."},
                {"type": "tool_use", "id": "toolu_01", "name": "get_weather", "input": {"city": "Paris", "days": 3}},
                {"type": "text", "text": "Done."}
            ],
            "stop_reason": "tool_use", "stop_sequence": null,
            "usage": {"input_tokens": 120, "cache_creation_input_tokens": 7, "cache_read_input_tokens": 300, "output_tokens": 87}
        })
    }

    fn assemble(chunks: &[&[u8]]) -> (Result<Value, ProviderError>, Vec<Delta>) {
        let deltas = std::sync::Mutex::new(Vec::new());
        let on = |d: Delta| deltas.lock().unwrap().push(d);
        let mut parser = sse::SseParser::default();
        let mut asm = StreamAssembler::default();
        let mut events: Vec<sse::SseEvent> = chunks.iter().flat_map(|c| parser.push(c)).collect();
        events.extend(parser.finish());
        let mut result = Ok(());
        for ev in events {
            let data: Value = serde_json::from_str(&ev.data).unwrap();
            match asm.handle("anthropic", &data, &on) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        let out = result.and_then(|()| asm.finish("anthropic"));
        (out, deltas.into_inner().unwrap())
    }

    #[test]
    fn stream_assembles_to_the_non_streaming_response() {
        let bytes = RECORDED.as_bytes();
        // Split into awkward network chunks (mid-line, mid-UTF-8, between \r and \n).
        let chunks: Vec<&[u8]> = bytes.chunks(7).collect();
        let (v, deltas) = assemble(&chunks);
        let v = v.unwrap();
        assert_eq!(v, non_streaming());

        let streamed = parse_message("anthropic", &v).unwrap();
        let direct = parse_message("anthropic", &non_streaming()).unwrap();
        assert_eq!(streamed.message, direct.message);
        assert_eq!(streamed.message.raw.as_ref().unwrap().value, non_streaming()["content"]);
        assert_eq!(streamed.stop, StopReason::ToolUse);
        assert_eq!(streamed.usage, direct.usage);
        assert_eq!(
            streamed.usage,
            Usage { input_tokens: 120, output_tokens: 87, cache_read_tokens: 300, cache_write_tokens: 7 }
        );

        let text: String = deltas.iter().filter_map(|d| if let Delta::Text(t) = d { Some(t.as_str()) } else { None }).collect();
        assert_eq!(text, streamed.message.text());
        assert_eq!(text, "Checking the weather — één moment.\nDone.");
        let thinking: String =
            deltas.iter().filter_map(|d| if let Delta::Thinking(t) = d { Some(t.as_str()) } else { None }).collect();
        assert_eq!(thinking, "Let me check.");
    }

    #[test]
    fn stream_errors() {
        // An error event mid-stream: overloads stay retryable.
        let cut = RECORDED.find("event: content_block_stop").unwrap();
        let s = format!(
            "{}event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}}}\n\n",
            &RECORDED[..cut]
        );
        let (v, deltas) = assemble(&[s.as_bytes()]);
        assert!(matches!(v, Err(ProviderError::Http { status: 529, .. })));
        assert!(!deltas.is_empty());

        // A stream cut off before message_stop is a network error, not a short answer.
        let cut = RECORDED.find("event: message_delta").unwrap();
        let (v, _) = assemble(&[&RECORDED.as_bytes()[..cut]]);
        assert!(matches!(v, Err(ProviderError::Network { .. })));
    }

    #[test]
    fn rejects_out_of_range_block_index() {
        for bad_index in [json!(MAX_CONTENT_BLOCKS), json!(u64::MAX), json!(-1)] {
            let ev = json!({
                "type": "content_block_start",
                "index": bad_index,
                "content_block": {"type": "text", "text": "x"}
            });
            let mut asm = StreamAssembler::default();
            let err = asm.handle("anthropic", &ev, &|_| {}).unwrap_err();
            assert!(matches!(err, ProviderError::BadResponse { .. }), "index {bad_index}: {err:?}");
        }

        // The maximum allowed index (1023 for 1024 blocks) is accepted.
        let ok_ev = json!({
            "type": "content_block_start",
            "index": MAX_CONTENT_BLOCKS - 1,
            "content_block": {"type": "text", "text": "ok"}
        });
        let mut asm = StreamAssembler::default();
        assert!(!asm.handle("anthropic", &ok_ev, &|_| {}).unwrap());
        assert_eq!(asm.blocks.len(), MAX_CONTENT_BLOCKS);
    }

    /// Serve one HTTP response on a local port: `head` then `body` in small, delayed writes.
    async fn serve_once(head: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read the request: headers, then content-length bytes of body.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            sock.write_all(head.as_bytes()).await.unwrap();
            for chunk in body.as_bytes().chunks(50) {
                sock.write_all(chunk).await.unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    fn provider(base: String) -> Anthropic {
        Anthropic::new(&ProviderConfig {
            id: "anthropic".into(),
            kind: "anthropic".into(),
            base_url: Some(base),
            api_key: Some("test-key".into()),
            options: json!({}),
        })
        .unwrap()
    }

    fn request() -> Request {
        Request { model: "claude-opus-5-5".into(), system: String::new(), messages: vec![Message::user_text("hi")], tools: vec![], max_tokens: 100 }
    }

    #[tokio::test]
    async fn streams_over_http() {
        let base =
            serve_once("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n", RECORDED).await;
        let deltas = std::sync::Mutex::new(Vec::new());
        let resp = provider(base).complete_stream(&request(), &|d| deltas.lock().unwrap().push(d)).await.unwrap();
        assert_eq!(resp.message.raw.unwrap().value, non_streaming()["content"]);
        assert_eq!(resp.usage.output_tokens, 87);
        assert!(deltas.into_inner().unwrap().len() > 3);
    }

    #[tokio::test]
    async fn stream_http_errors_and_plain_json() {
        let base = serve_once(
            "HTTP/1.1 529 Overloaded\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
            r#"{"type":"error","error":{"type":"overloaded_error","message":""}}"#,
        )
        .await;
        let r = provider(base).complete_stream(&request(), &|_| {}).await;
        assert!(matches!(r, Err(ProviderError::Http { status: 529, .. })), "{r:?}");

        // A server (or proxy) that ignores "stream" and answers with JSON.
        let base = serve_once(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
            r#"{"content":[{"type":"text","text":"whole"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}}"#,
        )
        .await;
        let deltas = std::sync::Mutex::new(Vec::new());
        let resp = provider(base).complete_stream(&request(), &|d| deltas.lock().unwrap().push(d)).await.unwrap();
        assert_eq!(resp.message.text(), "whole");
        assert_eq!(deltas.into_inner().unwrap(), vec![Delta::Text("whole".into())]);
    }
}
