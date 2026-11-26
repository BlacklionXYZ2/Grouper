//! Keeping the [`Store`] in step with the hub.
//!
//! **Catch-up** ([`SyncClient::sync_once`]): list conversations, fetch each one's
//! events after its sync marker (`GET /v1/conversations/{id}/events?after=`), append
//! them, then refresh the thread list (the hub's rows win over what events implied)
//! and, for admin tokens, memories.
//!
//! **Live** ([`SyncClient::run_live`]): open `/v1/events` (all conversations) first,
//! then catch up, so every event is either already in the hub's database when the
//! catch-up reads it or arrives on the socket. The hub's `seq` is one counter for
//! the whole hub, so on an unfiltered stream stored events arrive as
//! `cursor + 1, cursor + 2, …`. A jump is a gap (missed or reordered on the hub);
//! the client catches up over HTTP, which returns everything stored by then, before
//! storing the event that revealed it. Live-only events (`seq` 0) go to the
//! [`Listener`] and are never stored. When the socket drops (hub restart, network)
//! it reconnects with backoff and catches up again. A 401 stops it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use core_protocol::{ConversationInfo, Event, MemoryEntry, ThreadInfo};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

use crate::store::{RawEvent, Store};
use crate::{Result, SyncError};

/// What one catch-up did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub conversations: usize,
    pub threads: usize,
    /// Events stored by this run (0 when already up to date).
    pub new_events: usize,
    /// Memories cached; `None` when the token isn't admin (403).
    pub memories: Option<usize>,
}

/// The live connection's state, for a status line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Connecting,
    /// Connected; catching up over HTTP.
    CatchingUp,
    /// Caught up and following the stream.
    Live,
    /// A gap in the stream was filled by catching up. `after` is the last seq seen
    /// in order, `seen` the seq that revealed the gap.
    GapFilled { after: i64, seen: i64 },
    /// Hub unreachable or the socket dropped; retrying after `retry_in_secs`.
    Offline { reason: String, retry_in_secs: u64 },
    /// The hub rejected the token. `run_live` has stopped.
    Unauthorized,
}

/// Callbacks from [`SyncClient::run_live`], on the task running it. Keep them quick.
pub trait Listener: Send + Sync {
    fn status(&self, _status: &LiveStatus) {}
    /// A persisted event was stored (or a catch-up stored several: see `caught_up`).
    fn stored(&self, _event: &Event) {}
    /// A live-only event (`seq` 0: `AssistantDelta`, thread `TextDelta`) for streaming
    /// drafts. Never stored; the full message follows as a stored event.
    fn live(&self, _event: &Event) {}
    /// A catch-up finished (after connecting, or to fill a gap).
    fn caught_up(&self, _report: &SyncReport) {}
}

/// A listener that ignores everything.
pub struct NoListener;
impl Listener for NoListener {}

const PING_EVERY: Duration = Duration::from_secs(20);
/// No frame (pong included) for this long means the connection is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
/// Refresh conversation and thread lists this often while live (titles and models
/// change without an event).
const LIST_REFRESH_EVERY: Duration = Duration::from_secs(300);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub struct SyncClient {
    base: String,
    token: String,
    http: reqwest::Client,
    store: Arc<Store>,
}

impl SyncClient {
    /// `hub`: base URL (`https://hub.example.com`). Any valid token reads; an admin
    /// token also caches memories.
    pub fn new(hub: &str, token: &str, store: Arc<Store>) -> Result<Self> {
        let base = hub.trim().trim_end_matches('/').to_string();
        core_protocol::connect_url(&base).map_err(SyncError::Protocol)?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| SyncError::Protocol(e.to_string()))?;
        Ok(Self { base, token: token.trim().to_string(), http, store })
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn hub(&self) -> &str {
        &self.base
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SyncError::Unreachable(error_chain(&e)))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SyncError::Unauthorized);
        }
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            let message =
                body.get("error").and_then(Value::as_str).map(String::from).unwrap_or_else(|| body.to_string());
            return Err(SyncError::Http { status: status.as_u16(), message });
        }
        resp.json::<Value>().await.map_err(|e| SyncError::Unreachable(format!("reading {path}: {}", error_chain(&e))))
    }

    async fn get_as<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        serde_json::from_value(self.get(path).await?).map_err(|e| SyncError::Protocol(format!("{path}: {e}")))
    }

    /// Refresh the conversation and thread lists only.
    pub async fn refresh_lists(&self) -> Result<(usize, usize)> {
        let conversations: Vec<ConversationInfo> = self.get_as("/v1/conversations").await?;
        self.store.upsert_conversations(&conversations)?;
        let threads: Vec<ThreadInfo> = self.get_as("/v1/threads").await?;
        self.store.upsert_threads(&threads)?;
        Ok((conversations.len(), threads.len()))
    }

    /// Catch up once: conversations, their new events, threads, and memories when the
    /// token may read them. Safe to repeat; nothing is stored twice.
    pub async fn sync_once(&self) -> Result<SyncReport> {
        // Older hubs have no /v1/hub; then the live seq-collision check is all there is.
        match self.get_as::<core_protocol::HubInfo>("/v1/hub").await {
            Ok(info) => self.store.check_hub_id(&info.hub_id)?,
            Err(SyncError::Http { status: 404, .. }) => {}
            Err(e) => return Err(e),
        }
        let conversations: Vec<ConversationInfo> = self.get_as("/v1/conversations").await?;
        self.store.upsert_conversations(&conversations)?;
        let mut new_events = 0;
        for conv in &conversations {
            let after = self.store.last_seq(&conv.id)?;
            let raw: Vec<Value> =
                self.get_as(&format!("/v1/conversations/{}/events?after={after}", segment(&conv.id))).await?;
            let events = raw.into_iter().map(RawEvent::parse).collect::<Result<Vec<_>>>()?;
            new_events += self.store.append_events(&events)?;
        }
        // After the events, so the hub's rows overwrite what older events implied.
        let threads: Vec<ThreadInfo> = self.get_as("/v1/threads").await?;
        self.store.upsert_threads(&threads)?;
        let memories = match self.get_as::<Vec<MemoryEntry>>("/v1/memories?limit=1000").await {
            Ok(list) => {
                self.store.replace_memories(&list)?;
                Some(list.len())
            }
            Err(SyncError::Http { status: 403, .. }) => {
                self.store.note_memories_forbidden()?;
                None
            }
            Err(e) => return Err(e),
        };
        self.store.note_full_refresh(&self.base)?;
        Ok(SyncReport { conversations: conversations.len(), threads: threads.len(), new_events, memories })
    }

    async fn open_stream(
        &self,
    ) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>> {
        let url = core_protocol::connect_url(&self.base)
            .map_err(SyncError::Protocol)?
            .replace(core_protocol::CONNECT_PATH, core_protocol::EVENTS_PATH);
        let mut req = url.into_client_request().map_err(|e| SyncError::Protocol(e.to_string()))?;
        let auth = format!("Bearer {}", self.token).parse().map_err(|_| SyncError::Unauthorized)?;
        req.headers_mut().insert("authorization", auth);
        let mut config = tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(core_protocol::MAX_WS_MESSAGE_BYTES);
        config.max_frame_size = Some(core_protocol::MAX_WS_MESSAGE_BYTES);
        let connect = tokio_tungstenite::connect_async_with_config(req, Some(config), false);
        match tokio::time::timeout(Duration::from_secs(15), connect).await {
            Err(_) => Err(SyncError::Unreachable("timed out opening the event stream".into())),
            Ok(Ok((ws, _))) => Ok(ws),
            Ok(Err(tungstenite::Error::Http(resp))) if resp.status() == 401 => Err(SyncError::Unauthorized),
            Ok(Err(tungstenite::Error::Http(resp))) => Err(SyncError::Http {
                status: resp.status().as_u16(),
                message: "the hub refused the event stream".into(),
            }),
            Ok(Err(e)) => Err(SyncError::Unreachable(e.to_string())),
        }
    }

    /// Follow the hub until `stop` turns true (returns `Ok`) or the token is rejected
    /// (returns `Err(Unauthorized)`). Unreachable hubs and dropped sockets are retried
    /// with backoff (1 s doubling to 30 s), each reconnect catching up first.
    pub async fn run_live(&self, listener: Arc<dyn Listener>, mut stop: watch::Receiver<bool>) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            listener.status(&LiveStatus::Connecting);
            let reason = match self.live_session(&*listener, &mut stop).await {
                Ok(SessionEnd::Stopped) => return Ok(()),
                Ok(SessionEnd::Dropped { was_live, reason }) => {
                    if was_live {
                        backoff = Duration::from_secs(1);
                    }
                    reason
                }
                Err(SyncError::Unauthorized) => {
                    listener.status(&LiveStatus::Unauthorized);
                    return Err(SyncError::Unauthorized);
                }
                Err(e @ SyncError::Cache(_)) => return Err(e),
                Err(e) => e.to_string(),
            };
            tracing::info!("event stream: {reason}; retrying in {}s", backoff.as_secs());
            listener.status(&LiveStatus::Offline { reason, retry_in_secs: backoff.as_secs() });
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = stop.changed() => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn live_session(&self, listener: &dyn Listener, stop: &mut watch::Receiver<bool>) -> Result<SessionEnd> {
        let mut ws = self.open_stream().await?;
        listener.status(&LiveStatus::CatchingUp);
        let report = self.sync_once().await?;
        listener.caught_up(&report);
        let mut cursor = self.store.max_seq()?;
        listener.status(&LiveStatus::Live);

        let mut ping = tokio::time::interval(PING_EVERY);
        ping.reset();
        let mut lists = tokio::time::interval(LIST_REFRESH_EVERY);
        lists.reset();
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                _ = stop.changed() => {
                    if *stop.borrow() {
                        let _ = ws.close(None).await;
                        return Ok(SessionEnd::Stopped);
                    }
                }
                _ = ping.tick() => {
                    if heard.elapsed() > SILENCE_LIMIT {
                        return Ok(SessionEnd::dropped("no answer from the hub"));
                    }
                    if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return Ok(SessionEnd::dropped("event stream closed"));
                    }
                }
                _ = lists.tick() => {
                    self.refresh_lists().await?;
                }
                frame = ws.next() => {
                    heard = Instant::now();
                    let text = match frame {
                        None => return Ok(SessionEnd::dropped("event stream closed")),
                        Some(Err(e)) => return Ok(SessionEnd::dropped(&format!("event stream error: {e}"))),
                        Some(Ok(Message::Close(_))) => return Ok(SessionEnd::dropped("the hub closed the event stream")),
                        Some(Ok(Message::Text(t))) => t,
                        Some(Ok(_)) => continue,
                    };
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        tracing::warn!("ignoring a frame that isn't JSON");
                        continue;
                    };
                    let raw = match RawEvent::parse(value) {
                        Ok(raw) => raw,
                        Err(e) => {
                            tracing::warn!("ignoring frame: {e}");
                            continue;
                        }
                    };
                    self.handle(raw, &mut cursor, listener).await?;
                }
            }
        }
    }

    /// One frame from the stream.
    async fn handle(&self, raw: RawEvent, cursor: &mut i64, listener: &dyn Listener) -> Result<()> {
        match classify(*cursor, raw.seq) {
            Frame::LiveOnly => {
                if let Some(e) = &raw.event {
                    listener.live(e);
                }
                return Ok(());
            }
            Frame::Seen => {
                // Already stored by a catch-up. A different conversation at the same
                // seq means this cache came from another hub (or the hub was reset).
                if let Some(conv) = self.store.conversation_of(raw.seq)?
                    && conv != raw.conversation_id
                {
                    return Err(SyncError::Cache(format!(
                        "event {} belongs to {} here but {} on the hub: this cache is from another hub \
                         (or the hub was reset); delete it or use another cache file",
                        raw.seq, conv, raw.conversation_id
                    )));
                }
                return Ok(());
            }
            Frame::Next => {}
            Frame::Gap => {
                let after = *cursor;
                let report = self.sync_once().await?;
                listener.caught_up(&report);
                listener.status(&LiveStatus::GapFilled { after, seen: raw.seq });
                *cursor = (*cursor).max(self.store.max_seq()?);
                listener.status(&LiveStatus::Live);
            }
        }
        if !self.store.has_conversation(&raw.conversation_id)? {
            // A conversation that started since the last list refresh.
            self.refresh_lists().await?;
        }
        let added = self.store.append_events(std::slice::from_ref(&raw))?;
        *cursor = (*cursor).max(raw.seq);
        if added > 0
            && let Some(e) = &raw.event
        {
            listener.stored(e);
        }
        Ok(())
    }
}

enum SessionEnd {
    Stopped,
    Dropped { was_live: bool, reason: String },
}

impl SessionEnd {
    fn dropped(reason: &str) -> Self {
        SessionEnd::Dropped { was_live: true, reason: reason.into() }
    }
}

/// Where a stream frame stands relative to the last seq stored in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    /// `seq` 0: live-only, never stored.
    LiveOnly,
    /// At or below the cursor: already stored.
    Seen,
    /// Exactly the next seq.
    Next,
    /// Beyond the next seq: something in between was missed.
    Gap,
}

pub(crate) fn classify(cursor: i64, seq: i64) -> Frame {
    if seq <= 0 {
        Frame::LiveOnly
    } else if seq <= cursor {
        Frame::Seen
    } else if seq == cursor + 1 {
        Frame::Next
    } else {
        Frame::Gap
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        s.push_str(": ");
        s.push_str(&cause.to_string());
        source = cause.source();
    }
    s
}

/// Percent-encode one URL path segment.
fn segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_classified_against_the_cursor() {
        assert_eq!(classify(5, 0), Frame::LiveOnly);
        assert_eq!(classify(5, 3), Frame::Seen);
        assert_eq!(classify(5, 5), Frame::Seen);
        assert_eq!(classify(5, 6), Frame::Next);
        assert_eq!(classify(5, 7), Frame::Gap);
        assert_eq!(classify(0, 1), Frame::Next);
        assert_eq!(classify(0, 40), Frame::Gap);
    }

    #[test]
    fn bad_hub_urls_are_refused() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        assert!(SyncClient::new("hub.example.com", "a.b", store.clone()).is_err());
        let c = SyncClient::new("https://hub.example.com/ ", "a.b", store).unwrap();
        assert_eq!(c.hub(), "https://hub.example.com");
        assert_eq!(segment("a b/c"), "a%20b%2Fc");
    }
}
