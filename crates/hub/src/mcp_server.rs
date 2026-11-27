//! Harness orchestrator over MCP (package H5a).
//!
//! The hub serves Grouper's orchestrator tools at `POST /mcp/orchestrator` (streamable
//! HTTP, loopback only). A harness presents a per-chat bearer token from
//! [`crate::db::Db::mint_orchestrator_token`]. The raw token is never stored.

use std::sync::Arc;

use axum::Router;

use crate::server::Hub;

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use crate::db::Db;

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-mcp-{tag}-{}", crate::db::random_hex(4)))
    }

    fn sha256_hex(secret: &str) -> String {
        hex::encode(Sha256::digest(secret.as_bytes()))
    }

    #[test]
    fn orchestrator_tokens_are_hashed_scoped_and_revocable() {
        let dir = tmp("token");
        let db = Db::open(&dir).unwrap();
        assert!(db.mint_orchestrator_token("missing").unwrap_err().to_string().contains("no such conversation"));

        let chat_a = db.create_conversation("project", "A").unwrap();
        let chat_b = db.create_conversation("project", "B").unwrap();
        let token_a = db.mint_orchestrator_token(&chat_a).unwrap();
        let token_a2 = db.mint_orchestrator_token(&chat_a).unwrap();
        let token_b = db.mint_orchestrator_token(&chat_b).unwrap();

        assert_ne!(token_a, token_b);
        assert_eq!(db.orchestrator_conversation(&token_a).as_deref(), Some(chat_a.as_str()));
        assert_eq!(db.orchestrator_conversation(&token_a2).as_deref(), Some(chat_a.as_str()));
        assert_eq!(db.orchestrator_conversation(&token_b).as_deref(), Some(chat_b.as_str()));
        assert!(db.orchestrator_conversation("not-a-token").is_none());
        assert!(db.orchestrator_conversation(&format!("{token_a}x")).is_none());

        // Only the SHA-256 is stored. The raw token never appears in the table.
        let hashes: Vec<String> = db
            .c()
            .prepare("SELECT token_hash FROM orchestrator_tokens ORDER BY created")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(hashes, vec![sha256_hex(&token_a), sha256_hex(&token_a2), sha256_hex(&token_b)]);
        let raw_hits: i64 = db
            .c()
            .query_row("SELECT COUNT(*) FROM orchestrator_tokens WHERE token_hash = ?1", [&token_a], |row| row.get(0))
            .unwrap();
        assert_eq!(raw_hits, 0);

        assert!(db.revoke_orchestrator_token(&token_a));
        assert!(db.orchestrator_conversation(&token_a).is_none());
        assert_eq!(db.orchestrator_conversation(&token_a2).as_deref(), Some(chat_a.as_str()));
        assert!(!db.revoke_orchestrator_token(&token_a));
        assert!(!db.revoke_orchestrator_token("missing"));

        drop(db);
        let db = Db::open(&dir).unwrap();
        assert!(db.orchestrator_conversation(&token_a).is_none(), "revocation survives reopen");
        assert_eq!(db.orchestrator_conversation(&token_b).as_deref(), Some(chat_b.as_str()));
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
