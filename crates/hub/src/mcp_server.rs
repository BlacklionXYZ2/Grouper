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
use core_agent::ToolOutput;
use core_protocol::tunnel::{result_from_text, tool_to_mcp};
use core_protocol::{AgentEvent, EventBody};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler};
use serde_json::Value;

use crate::server::{Hub, err};

/// Streamable HTTP service kept on [`Hub`] so MCP sessions survive across requests.
pub(crate) type OrchestratorHttp = StreamableHttpService<OrchestratorServer, LocalSessionManager>;

/// The chat an orchestrator token may act on, attached after the bearer check.
#[derive(Clone, Debug)]
struct OrchestratorChat(String);

pub(crate) struct OrchestratorServer {
    hub: Arc<Hub>,
}

fn chat_of(context: &RequestContext<RoleServer>) -> Result<String, ErrorData> {
    let parts = context.extensions.get::<axum::http::request::Parts>().ok_or_else(|| {
        ErrorData::internal_error("orchestrator request is missing its HTTP parts", None)
    })?;
    parts
        .extensions
        .get::<OrchestratorChat>()
        .map(|chat| chat.0.clone())
        .ok_or_else(|| ErrorData::internal_error("orchestrator request is missing its chat", None))
}

impl ServerHandler for OrchestratorServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("grouper-hub", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = crate::orchestrator::harness_orchestrator_tools(&self.hub, "");
        Ok(ListToolsResult::with_all_items(tools.iter().map(|tool| tool_to_mcp(&tool.def())).collect()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let conversation_id = chat_of(&context)?;
        let args = Value::Object(request.arguments.unwrap_or_default());
        let call_id = match &context.id {
            rmcp::model::NumberOrString::Number(n) => n.to_string(),
            rmcp::model::NumberOrString::String(s) => s.to_string(),
        };
        let name = request.name.clone();
        // The harness keeps its own session. Grouper's transcript is these events.
        self.hub.emit(
            &conversation_id,
            None,
            EventBody::ThreadActivity { event: AgentEvent::ToolCall { call_id: call_id.clone(), name: name.clone(), input: args.clone() } },
        );
        let tools = crate::orchestrator::harness_orchestrator_tools(&self.hub, &conversation_id);
        let out = match tools.iter().find(|tool| tool.def().name == name) {
            Some(tool) => tool.call(args).await,
            None => ToolOutput::err(format!("unknown tool {name}")),
        };
        self.hub.emit(
            &conversation_id,
            None,
            EventBody::ThreadActivity {
                event: AgentEvent::ToolResult { call_id, name, ok: !out.is_error, output: out.content.clone() },
            },
        );
        Ok(result_from_text(!out.is_error, out.content).into())
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
    let response = match tower_service::Service::call(&mut service, req).await {
        Ok(response) => response,
        Err(infallible) => match infallible {},
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

    fn rpc_body(id: i64, method: &str, params: serde_json::Value) -> String {
        let mut msg = serde_json::json!({"jsonrpc": "2.0", "method": method});
        if id >= 0 {
            msg["id"] = serde_json::json!(id);
        }
        if !params.is_null() {
            msg["params"] = params;
        }
        msg.to_string()
    }

    fn json_messages(body: &str) -> Vec<serde_json::Value> {
        body.lines()
            .filter_map(|line| {
                let line = line.trim();
                let payload = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
                payload.starts_with('{').then(|| serde_json::from_str(payload).ok()).flatten()
            })
            .collect()
    }

    fn tool_text(message: &serde_json::Value) -> (bool, String) {
        let result = &message["result"];
        let error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        (error, text)
    }

    struct Session {
        url: String,
        token: String,
        session: String,
        next_id: i64,
    }

    impl Running {
        async fn open_session(&self, conversation_id: &str, token: &str) -> Session {
            let res = self
                .http
                .post(endpoint(&self.base, conversation_id))
                .header("accept", ACCEPT)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(init_body())
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), reqwest::StatusCode::OK);
            let session = res.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()).map(str::to_string);
            let status = res.status();
            let _ = res.text().await;
            let session = session.unwrap_or_else(|| panic!("initialize status {status} opened no session"));
            let url = endpoint(&self.base, conversation_id);
            let opened = Session { url, token: token.to_string(), session, next_id: 2 };
            opened.notify("notifications/initialized").await;
            opened
        }
    }

    impl Session {
        async fn post(&self, body: String) -> reqwest::Response {
            // A fresh client: the session SSE response ends, and a pooled connection
            // still reading it would stall the next call.
            let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap();
            http.post(&self.url)
                .header("accept", ACCEPT)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", self.token))
                .header("mcp-session-id", &self.session)
                .body(body)
                .send()
                .await
                .unwrap()
        }

        async fn notify(&self, method: &str) {
            let res = self.post(rpc_body(-1, method, serde_json::Value::Null)).await;
            assert_eq!(res.status(), reqwest::StatusCode::ACCEPTED, "{method}");
        }

        async fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            let id = self.next_id;
            self.next_id += 1;
            let res = self.post(rpc_body(id, method, params)).await;
            let status = res.status();
            let body = res.text().await.unwrap();
            assert_eq!(status, reqwest::StatusCode::OK, "{method}: {body}");
            json_messages(&body)
                .into_iter()
                .find(|msg| msg.get("id").and_then(|v| v.as_i64()) == Some(id))
                .unwrap_or_else(|| panic!("{method} returned no result: {body}"))
        }

        async fn tool(&mut self, name: &str, arguments: serde_json::Value) -> (bool, String) {
            let msg = self.call("tools/call", serde_json::json!({"name": name, "arguments": arguments})).await;
            assert!(msg.get("error").is_none(), "{name}: {msg}");
            tool_text(&msg)
        }
    }

    /// The latest conversation-level tool call and its result were stored.
    fn assert_recorded(hub: &Hub, chat: &str, name: &str, ok: bool) {
        use core_protocol::{AgentEvent, EventBody};
        let events = hub.db.events(chat, 0, None);
        let result = events.iter().rev().find(|event| {
            event.thread_id.is_none()
                && matches!(&event.body, EventBody::ThreadActivity { event: AgentEvent::ToolResult { name: recorded, .. } } if recorded == name)
        });
        let Some(result) = result else { panic!("no stored tool result for {name}") };
        let EventBody::ThreadActivity { event: AgentEvent::ToolResult { call_id, ok: stored_ok, output, .. } } = &result.body else {
            unreachable!()
        };
        assert_eq!(*stored_ok, ok, "{name}: {output}");
        let found_call = events.iter().any(|event| {
            event.thread_id.is_none()
                && matches!(&event.body, EventBody::ThreadActivity { event: AgentEvent::ToolCall { call_id: id, name: recorded, .. } } if id == call_id && recorded == name)
        });
        assert!(found_call, "no stored tool call for {name}");
    }

    fn scripted_thread_model(hub: &Hub) {
        use core_protocol::{ModelEntry, ProviderConfig};
        hub.db
            .upsert_provider(&ProviderConfig {
                id: "p".into(),
                kind: "scripted".into(),
                base_url: None,
                api_key: None,
                options: serde_json::json!({
                    "inline": {"delay_ms": 30000, "default": [{"type": "text", "text": "OK"}]}
                }),
            })
            .unwrap();
        hub.db
            .upsert_model(&ModelEntry {
                id: "m".into(),
                provider_id: "p".into(),
                model: "m-1".into(),
                roles: vec!["thread".into()],
                ..Default::default()
            })
            .unwrap();
    }

    #[tokio::test]
    async fn orchestrator_mcp_tools_call_the_same_hub_functions() {
        let run = serve().await;
        scripted_thread_model(&run.hub);
        let chat = run.hub.db.create_conversation("project", "Tools").unwrap();
        let other = run.hub.db.create_conversation("project", "Other").unwrap();
        let token = run.hub.db.mint_orchestrator_token(&chat).unwrap();
        let other_token = run.hub.db.mint_orchestrator_token(&other).unwrap();
        let mut session = run.open_session(&chat, &token).await;

        let listed = session.call("tools/list", serde_json::Value::Null).await;
        let mut names: Vec<&str> = listed["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["forget", "list_devices", "list_threads", "message_thread", "recall", "remember", "start_thread", "stop_thread", "update_memory"]
        );

        let (err, devices) = session.tool("list_devices", serde_json::json!({})).await;
        assert!(!err, "{devices}");
        let devices: serde_json::Value = serde_json::from_str(&devices).unwrap();
        assert!(devices.as_array().unwrap().iter().any(|row| row["id"] == "hub"), "{devices}");
        assert_recorded(&run.hub, &chat, "list_devices", true);

        let (err, saved) = session.tool("remember", serde_json::json!({"content": "The barn clock is slow", "tags": ["clock"]})).await;
        assert!(!err, "{saved}");
        assert_recorded(&run.hub, &chat, "remember", true);
        let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
        let id = saved["id"].as_i64().unwrap();
        let (err, found) = session.tool("recall", serde_json::json!({"query": "barn clock"})).await;
        assert!(!err && found.contains("The barn clock is slow"), "{found}");
        assert_recorded(&run.hub, &chat, "recall", true);
        let (err, updated) = session.tool("update_memory", serde_json::json!({"id": id, "content": "The barn clock is five minutes slow"})).await;
        assert!(!err, "{updated}");
        assert_recorded(&run.hub, &chat, "update_memory", true);
        let (err, forgotten) = session.tool("forget", serde_json::json!({"id": id})).await;
        assert!(!err && forgotten.contains(&format!("forgot #{id}")), "{forgotten}");
        assert_recorded(&run.hub, &chat, "forget", true);
        assert!(run.hub.db.memory(id).is_none());

        let (err, started) = session.tool("start_thread", serde_json::json!({"title": "Notes", "goal": "Write the notes."})).await;
        assert!(!err, "{started}");
        assert_recorded(&run.hub, &chat, "start_thread", true);
        let started: serde_json::Value = serde_json::from_str(&started).unwrap();
        let thread_id = started["thread_id"].as_str().unwrap().to_string();
        assert_eq!(started["device"], "hub");
        assert_eq!(run.hub.db.thread(&thread_id).unwrap().conversation_id, chat);

        let (err, threads) = session.tool("list_threads", serde_json::json!({})).await;
        assert!(!err && threads.contains(&thread_id), "{threads}");
        assert_recorded(&run.hub, &chat, "list_threads", true);

        let (err, sent) = session.tool("message_thread", serde_json::json!({"thread_id": thread_id, "text": "Keep it short."})).await;
        assert!(!err && sent == "sent", "{sent}");
        assert_recorded(&run.hub, &chat, "message_thread", true);

        let (err, stopped) = session.tool("stop_thread", serde_json::json!({"thread_id": thread_id})).await;
        assert!(!err && stopped == "stopped", "{stopped}");
        assert_recorded(&run.hub, &chat, "stop_thread", true);
        assert_eq!(run.hub.db.thread(&thread_id).unwrap().status, "stopped");

        let mut other_session = run.open_session(&other, &other_token).await;
        let (err, other_threads) = other_session.tool("list_threads", serde_json::json!({})).await;
        assert!(!err && !other_threads.contains(&thread_id), "{other_threads}");
        assert_recorded(&run.hub, &other, "list_threads", true);
        let (err, refused) = other_session.tool("message_thread", serde_json::json!({"thread_id": thread_id, "text": "no"})).await;
        assert!(err && refused.contains("no such thread"), "{refused}");
        assert_recorded(&run.hub, &other, "message_thread", false);

        drop(run.hub);
        std::fs::remove_dir_all(run.dir).ok();
    }
}
