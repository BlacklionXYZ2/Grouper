//! Server-sent events: an incremental parser and a helper that POSTs a streaming
//! request and feeds each event to a callback.
//!
//! The parser follows the HTML spec closely enough for model APIs: lines end in
//! `\n`, `\r\n` or `\r` (and may be split across network chunks, even between the
//! `\r` and `\n`), lines starting with `:` are comments, `data:` lines are joined
//! with `\n`, and a blank line dispatches the event.

use futures_util::StreamExt;
use serde_json::Value;

use crate::ProviderError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// The `event:` field, empty when the server sent none.
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
}

impl SseParser {
    /// Feed bytes; returns every event completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        let mut i = 0;
        while i < self.buf.len() {
            let terminator = match self.buf[i] {
                b'\n' => 1,
                b'\r' => match self.buf.get(i + 1) {
                    Some(b'\n') => 2,
                    Some(_) => 1,
                    // A trailing `\r` may be the first half of `\r\n`: wait for more.
                    None => break,
                },
                _ => {
                    i += 1;
                    continue;
                }
            };
            let line = String::from_utf8_lossy(&self.buf[start..i]).into_owned();
            self.line(&line, &mut out);
            i += terminator;
            start = i;
        }
        self.buf.drain(..start);
        out
    }

    /// The stream ended: flush a final unterminated line and any pending event.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&rest);
            self.line(line.trim_end_matches('\r'), &mut out);
        }
        self.line("", &mut out);
        out.pop()
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.has_data {
                let mut data = std::mem::take(&mut self.data);
                data.pop(); // the trailing '\n'
                out.push(SseEvent { event: std::mem::take(&mut self.event), data });
            }
            self.event.clear();
            self.has_data = false;
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            _ => {} // id, retry, unknown fields
        }
    }
}

/// What a streaming POST returned.
pub(crate) enum Streamed {
    /// The server ignored `"stream": true` and answered with plain JSON.
    Json(Value),
    /// The event stream ran to the end (or the callback said it was done).
    Events,
}

/// POST `body` and call `on_event` for each server-sent event until it returns
/// `Ok(true)` (done) or the stream ends. HTTP errors become [`ProviderError::Http`].
pub(crate) async fn post_sse(
    provider: &str,
    req: reqwest::RequestBuilder,
    body: &Value,
    mut on_event: impl FnMut(SseEvent) -> Result<bool, ProviderError>,
) -> Result<Streamed, ProviderError> {
    let net = |e: reqwest::Error| ProviderError::Network { provider: provider.into(), message: e.to_string() };
    let resp = req.header("accept", "text/event-stream").json(body).send().await.map_err(net)?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        // Cut on a character boundary: `String::truncate` panics inside a UTF-8 sequence.
        let body: String = resp.text().await.map_err(net)?.chars().take(2000).collect();
        return Err(ProviderError::Http { provider: provider.into(), status, body });
    }
    let is_json = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if is_json {
        let text = resp.text().await.map_err(net)?;
        return serde_json::from_str(&text)
            .map(Streamed::Json)
            .map_err(|e| ProviderError::BadResponse { provider: provider.into(), message: e.to_string() });
    }

    let mut parser = SseParser::default();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        for event in parser.push(&chunk.map_err(net)?) {
            if on_event(event)? {
                return Ok(Streamed::Events);
            }
        }
    }
    if let Some(event) = parser.finish() {
        on_event(event)?;
    }
    Ok(Streamed::Events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        let mut out: Vec<SseEvent> = chunks.iter().flat_map(|c| p.push(c)).collect();
        out.extend(p.finish());
        out
    }

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent { event: event.into(), data: data.into() }
    }

    #[test]
    fn splits_anywhere() {
        let whole = b": hello\r\nevent: a\r\ndata: {\"x\":1}\r\n\r\nevent: b\ndata: line1\ndata:line2\n\ndata: tail";
        let want = vec![ev("a", "{\"x\":1}"), ev("b", "line1\nline2"), ev("", "tail")];
        assert_eq!(feed(&[whole]), want);
        // Every possible split point, including between \r and \n.
        for cut in 0..whole.len() {
            assert_eq!(feed(&[&whole[..cut], &whole[cut..]]), want, "cut at {cut}");
        }
        // One byte at a time.
        let bytes: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(feed(&bytes), want);
    }

    #[test]
    fn bare_cr_and_utf8_across_chunks() {
        let text = "data: héllo ✓\r\rdata: [DONE]\r\r".as_bytes();
        for cut in 0..text.len() {
            assert_eq!(feed(&[&text[..cut], &text[cut..]]), vec![ev("", "héllo ✓"), ev("", "[DONE]")], "cut at {cut}");
        }
    }

    #[test]
    fn comments_and_empty_events_are_skipped() {
        assert_eq!(feed(&[b": ping\n\nevent: x\n\n:keepalive\n\n"]), vec![]);
    }
}
