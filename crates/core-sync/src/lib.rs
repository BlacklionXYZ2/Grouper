//! A client's local copy of the hub: conversations, threads, the append-only event
//! log and (for admin tokens) memories, in SQLite, so every client shows the same
//! chats and keeps reading them when the hub is unreachable.
//!
//! - [`Store`]: the cache file. Writes come only from the hub; reads are what the
//!   UIs show (conversation list with previews and unread counts, folded
//!   transcripts, thread activity, pending approvals, full-text search).
//! - [`SyncClient`]: fills the store. [`SyncClient::sync_once`] catches up over
//!   HTTP; [`SyncClient::run_live`] follows `/v1/events`, stores persisted events in
//!   order, hands live-only deltas (`seq` 0) to a [`Listener`] for streaming, and
//!   fills gaps and reconnects by catching up again.
//!
//! Read-only for now: offline writes (sending while the hub is away, then merging)
//! wait for a conflict policy.

mod client;
mod fold;
mod store;

pub use client::{LiveStatus, Listener, NoListener, SyncClient, SyncReport};
pub use core_protocol;
pub use fold::{
    ActivityItem, MessagePreview, PendingApproval, ThreadCard, TranscriptItem, pending_approvals, thread_activity,
    transcript,
};
pub use store::{ConversationSummary, SCHEMA_VERSION, SearchHit, Store};

/// Why a sync or a cache read failed.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// No answer from the hub (connection refused, DNS, timeout, dropped mid-response).
    #[error("can't reach the hub: {0}")]
    Unreachable(String),
    /// The hub rejected the token (HTTP 401). Retrying won't help.
    #[error("the hub rejected the token (401)")]
    Unauthorized,
    /// Any other non-success answer, with the hub's `error` string.
    #[error("hub answered {status}: {message}")]
    Http { status: u16, message: String },
    /// The hub answered with something this client can't read.
    #[error("unexpected answer from the hub: {0}")]
    Protocol(String),
    /// The cache file.
    #[error("cache: {0}")]
    Cache(String),
}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        SyncError::Cache(e.to_string())
    }
}

pub type Result<T, E = SyncError> = std::result::Result<T, E>;

/// Unix seconds now.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `2026-11-25 10:31 UTC`, for "cached at …" notes.
pub fn utc_string(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn utc_dates() {
        assert_eq!(super::utc_string(0), "1970-01-01 00:00 UTC");
        assert_eq!(super::utc_string(1_795_609_860), "2026-11-25 12:31 UTC");
        assert_eq!(super::utc_string(951_782_400), "2000-02-29 00:00 UTC");
    }
}
