//! Harness orchestrator over MCP (package H5a).
//!
//! The hub serves Grouper's orchestrator tools at `/mcp/orchestrator` (streamable HTTP,
//! loopback only). A harness presents a per-chat bearer token from
//! [`crate::db::Db::mint_orchestrator_token`]. The raw token is never stored. An optional
//! `conversation_id` query must match the token's chat; a token for another chat is refused.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServerHandler;

use crate::server::{Hub, err};

/// Streamable HTTP service kept on [`Hub`] so MCP sessions survive across requests.
pub(crate) type OrchestratorHttp = StreamableHttpService<OrchestratorServer, LocalSessionManager>;

/// The chat an orchestrator token may act on, attached after the bearer check.
#[derive(Clone, Debug)]
struct OrchestratorChat(String);

pub(crate) struct OrchestratorServer {
    #[allow(dead_code)] // read once tools dispatch on this chat
    hub: Arc<Hub>,
}

impl ServerHandler for OrchestratorServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("grouper-hub", env!("CARGO_PKG_VERSION")))
    }
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new().route("/mcp/orchestrator", any(orchestrator_mcp))
}

fn http_service(hub: &Arc<Hub>) -> OrchestratorHttp {
    hub.orchestrator_mcp
        .get_or_init(|| {
            let hub = hub.clone();
            StreamableHttpService::new(
                move || Ok(OrchestratorServer { hub: hub.clone() }),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default().with_sse_keep_alive(None),
            )
        })
        .clone()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").filter(|token| !token.is_empty())
}

fn query_param(uri: &Uri, key: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name == key).then(|| value.to_string())
    })
}

/// `GET`/`POST`/`DELETE /mcp/orchestrator`: streamable HTTP, loopback, per-chat token.
async fn orchestrator_mcp(State(hub): State<Arc<Hub>>, mut req: Request) -> Response {
    if !crate::session::is_loopback_request(req.headers(), req.extensions()) {
        return err(StatusCode::FORBIDDEN, "the orchestrator MCP endpoint only accepts loopback connections");
    }
    let Some(token) = bearer(req.headers()) else {
        return err(StatusCode::UNAUTHORIZED, "an orchestrator token is required");
    };
    let Some(conversation_id) = hub.db.orchestrator_conversation(token) else {
        return err(StatusCode::UNAUTHORIZED, "that orchestrator token isn't valid");
    };
    if let Some(asked) = query_param(req.uri(), "conversation_id")
        && asked != conversation_id
    {
        return err(StatusCode::FORBIDDEN, "this orchestrator token is for a different conversation");
    }
    req.extensions_mut().insert(OrchestratorChat(conversation_id));
    let mut service = http_service(&hub);
    let Ok(response) = tower_service::Service::call(&mut service, req).await else {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "orchestrator MCP failed");
    };
    response.into_response()
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

    use std::net::SocketAddr;
    use std::time::Duration;

    use axum::extract::{ConnectInfo, Request};
    use axum::middleware::Next;

    use crate::server::{Hub, router};

    const ACCEPT: &str = "application/json, text/event-stream";

    fn init_body() -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "grouper-test", "version": "0"}
            }
        })
        .to_string()
    }

    struct Running {
        base: String,
        lan_base: String,
        hub: std::sync::Arc<Hub>,
        dir: std::path::PathBuf,
        http: reqwest::Client,
    }

    async fn serve() -> Running {
        let dir = tmp("http");
        let hub = Hub::new(Db::open(&dir).unwrap(), dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(hub.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let lan_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let lan_base = format!("http://{}", lan_listener.local_addr().unwrap());
        let lan_app = router(hub.clone()).layer(axum::middleware::from_fn(|mut req: Request, next: Next| async move {
            req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([192, 168, 1, 44], 51234))));
            next.run(req).await
        }));
        tokio::spawn(async move {
            let _ = axum::serve(lan_listener, lan_app).await;
        });

        let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap();
        Running { base, lan_base, hub, dir, http }
    }

    fn endpoint(base: &str, conversation_id: &str) -> String {
        format!("{base}/mcp/orchestrator?conversation_id={conversation_id}")
    }

    #[tokio::test]
    async fn orchestrator_mcp_requires_loopback_and_this_chats_token() {
        let run = serve().await;
        let chat_a = run.hub.db.create_conversation("project", "A").unwrap();
        let chat_b = run.hub.db.create_conversation("project", "B").unwrap();
        let token_a = run.hub.db.mint_orchestrator_token(&chat_a).unwrap();
        let token_b = run.hub.db.mint_orchestrator_token(&chat_b).unwrap();
        let body = init_body();

        let post = |url: String, token: Option<String>| {
            let http = run.http.clone();
            let body = body.clone();
            async move {
                let mut req = http.post(url).header("accept", ACCEPT).header("content-type", "application/json");
                if let Some(token) = token {
                    req = req.header("authorization", format!("Bearer {token}"));
                }
                req.body(body).send().await.unwrap()
            }
        };

        let missing = post(endpoint(&run.base, &chat_a), None).await;
        assert_eq!(missing.status(), reqwest::StatusCode::UNAUTHORIZED);

        let other_chat = post(endpoint(&run.base, &chat_a), Some(token_b)).await;
        assert_eq!(other_chat.status(), reqwest::StatusCode::FORBIDDEN);

        let remote = post(endpoint(&run.lan_base, &chat_a), Some(token_a.clone())).await;
        assert_eq!(remote.status(), reqwest::StatusCode::FORBIDDEN);

        let ok = post(endpoint(&run.base, &chat_a), Some(token_a.clone())).await;
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert!(ok.headers().get("mcp-session-id").is_some(), "initialize should open an MCP session");

        assert!(run.hub.db.revoke_orchestrator_token(&token_a));
        let revoked = post(endpoint(&run.base, &chat_a), Some(token_a)).await;
        assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);

        drop(run.hub);
        std::fs::remove_dir_all(run.dir).ok();
    }
}
