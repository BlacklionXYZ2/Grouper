//! Long-term memory and saved skills over SQLite + FTS5.
//!
//! Reusable by the hub and by device/phone caches without HTTP, async runtime,
//! or provider dependencies.

pub mod memory;
pub mod skills;

pub use core_protocol;
pub use memory::{MAX_MEMORY_CHARS, RECALL_MAX_CHARS, RECALL_TOP, fts_query};
pub use skills::SKILLS_TOP;

use anyhow::Result;
use rusqlite::Connection;

/// Create the `memories`, `memories_fts`, `skills` and `skills_fts` tables and triggers.
pub fn migrate(conn: &Connection) -> Result<()> {
    memory::migrate(conn)?;
    skills::migrate(conn)?;
    Ok(())
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}
