//! Scoped folders and getting a thread's files back, end to end in process: a real
//! hub, a real device (core-client over WebSocket) with two scoped folders, and a
//! scripted model.
//!
//! - The device reports its folders (names and modes only); the hub lists them.
//! - A thread reads `@test/notes.txt` (Auto, no approval), is refused a write into the
//!   read-only `@test`, and asks before writing into the read_write `@rw`.
//! - `GET /v1/threads/{id}/files[/{path}]` fetch its workspace from the device over the
//!   unlisted MCP tools (and from the hub for hub threads); 409 once the device is gone.
//! - The orchestrator quotes the thread's output with `read_thread_file`.

use std::sync::Arc;
use std::time::Duration;

use core_agent::folders::FolderSpec;
use core_client::core_protocol::{
    AgentEvent, DeviceInfo, Event, EventBody, FolderMode, ModelEntry, ProviderConfig, ToolCallRequest,
    WorkspaceListing,
};
use core_client::{Events, Status, ToolHandler};
use hub::db::Db;
use hub::server::{self, Hub};
use hub::threads::ThreadRequest;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(90);

struct Tools;
impl ToolHandler for Tools {
    fn call(&self, _tool: &str, args: &str) -> Result<String, String> {
        Ok(args.to_string())
    }
}

struct Quiet;
impl Events for Quiet {
    fn status(&self, _status: Status) {}
    fn presence(&self, _devices: Vec<DeviceInfo>) {}
}

async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(WAIT, async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

fn find_event(events: &[Event], pred: impl Fn(&EventBody) -> bool) -> Option<&Event> {
    events.iter().find(|e| pred(&e.body))
}

fn script() -> Value {
    json!({"rules": [
        // Device thread: read a scoped file, write outputs (workspace, read-only folder, read_write folder).
        {"system": "You are a subagent", "last": "go-files", "reply": [
            {"type": "tool_use", "name": "read_file", "input": {"path": "@test/notes.txt"}},
            {"type": "tool_use", "name": "write_file", "input": {"path": "out/result.md", "content": "result body from thread"}},
            {"type": "tool_use", "name": "write_file", "input": {"path": "@test/planted.txt", "content": "x"}},
            {"type": "tool_use", "name": "write_file", "input": {"path": "@rw/answer.txt", "content": "approved write"}}
        ]},
        {"system": "You are a subagent", "last": "notes from the test folder", "reply": [
            {"type": "text", "text": "thread finished: wrote out/result.md"}
        ]},
        // Hub thread.
        {"system": "You are a subagent", "last": "go-hub", "reply": [
            {"type": "tool_use", "name": "write_file", "input": {"path": "hub.txt", "content": "made on the hub"}}
        ]},
        {"system": "You are a subagent", "last": "to hub.txt", "reply": [{"type": "text", "text": "hub work complete"}]},
        // The orchestrator quotes the device thread's output.
        {"system": "orchestrator of Grouper", "last": "thread finished", "reply": [
            {"type": "tool_use", "name": "read_thread_file", "input": {"thread_id": "$THREAD_ID", "path": "out/result.md"}}
        ]},
        {"system": "orchestrator of Grouper", "last": "result body from thread", "reply": [
            {"type": "text", "text": "Quoted the thread's result."}
        ]}
    ], "default": [{"type": "text", "text": "OK"}]})
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_folders_and_thread_files() {
    let dir = std::env::temp_dir().join(format!("grouper-thread-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (scoped, rw) = (dir.join("scoped-test"), dir.join("scoped-rw"));
    std::fs::create_dir_all(&scoped).unwrap();
    std::fs::create_dir_all(&rw).unwrap();
    std::fs::write(scoped.join("notes.txt"), "notes from the test folder").unwrap();

    let db = Db::open(&dir.join("data")).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({"inline": script()}),
    })
    .unwrap();
    db.upsert_model(&ModelEntry {
        id: "fake".into(),
        provider_id: "s".into(),
        model: "fake-1".into(),
        input_price: 0.0,
        output_price: 0.0,
        cache_read_price: None,
        cache_write_price: None,
        context_window: 0,
        use_cases: String::new(),
        free: true,
        roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
        local: false,
        modalities: vec![],
    })
    .unwrap();
    let (device, token) = db.add_device("Laptop", "windows", false).unwrap();
    let (_, admin) = db.add_device("Phone", "ios", true).unwrap();
    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = server::router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    std::fs::write(dir.join("mcp.json"), json!({"includeSubagentChat": false, "mcpServers": {}}).to_string()).unwrap();

    let config = core_client::Config {
        hub_url: base.clone(),
        token: token.clone(),
        capabilities: vec![],
        workspace_root: Some(dir.join("device-workspaces")),
        allow_shell: false,
        mcp_config: Some(dir.join("mcp.json")),
        folders: vec![
            FolderSpec { name: "test".into(), path: scoped.clone(), mode: FolderMode::Read },
            FolderSpec { name: "rw".into(), path: rw.clone(), mode: FolderMode::ReadWrite },
        ],
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let client = tokio::spawn(core_client::run(config, Arc::new(Tools), Arc::new(Quiet), stopped));

    // The device's folders reach the hub: names and modes, never paths.
    until("the device reported its folders and tools", || async {
        let devices = hub.devices().await;
        let me = devices.iter().find(|d| d.id == device.id).unwrap();
        me.folders.len() == 2 && me.capabilities.iter().any(|c| c.name == "read_file")
    })
    .await;
    let http = reqwest::Client::new();
    let devices: Value =
        http.get(format!("{base}/v1/devices")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
    let me = devices.as_array().unwrap().iter().find(|d| d["id"] == device.id).unwrap();
    assert_eq!(me["folders"], json!([{"name": "test", "mode": "read"}, {"name": "rw", "mode": "read_write"}]));
    assert!(!devices.to_string().contains("scoped-test"), "paths must stay on the device: {devices}");
    // The workspace tools are never listed (registry, permissions) nor callable directly.
    let all = hub.db.all_tools(Some(&device.id));
    assert!(!all.iter().any(|t| t.name.starts_with("grouper_workspace")), "{all:?}");
    let r = http
        .post(format!("{base}/v1/devices/{}/call", device.id))
        .bearer_auth(&admin)
        .json(&ToolCallRequest { tool: "grouper_workspace_list".into(), args: json!({"thread_id": "t1"}) })
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // A device thread: read in @test runs, a write there is refused, a write in @rw asks.
    let conv = hub.db.create_conversation("project", "files").unwrap();
    let t = hub
        .start_thread(&conv, ThreadRequest { title: "Files".into(), device: "Laptop".into(), goal: "go-files".into(), ..Default::default() })
        .await
        .unwrap();
    until("the write into @rw asks for approval", || async {
        find_event(&hub.db.events(&conv, 0, Some(&t.id)), |b| matches!(b, EventBody::ApprovalRequested { .. })).is_some()
    })
    .await;
    let events = hub.db.events(&conv, 0, Some(&t.id));
    let approvals: Vec<(String, String, Value)> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ApprovalRequested { approval_id, tool, reason, input, .. } if tool == "write_file" => {
                Some((approval_id.clone(), reason.clone(), input.clone()))
            }
            EventBody::ApprovalRequested { tool, .. } => panic!("unexpected approval for {tool}"),
            _ => None,
        })
        .collect();
    assert_eq!(approvals.len(), 1, "only the @rw write asks: {approvals:?}");
    assert_eq!(approvals[0].1, "writes to @rw on Laptop");
    assert_eq!(approvals[0].2["path"], "@rw/answer.txt");
    assert!(!rw.join("answer.txt").exists());
    assert!(hub.resolve_approval(&approvals[0].0, true).await);

    until("the device thread finished", || async { hub.db.thread(&t.id).unwrap().status == "done" }).await;
    assert_eq!(std::fs::read_to_string(rw.join("answer.txt")).unwrap(), "approved write");
    assert!(!scoped.join("planted.txt").exists());
    let results: Vec<(String, bool, String)> = hub
        .db
        .events(&conv, 0, Some(&t.id))
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadActivity { event: AgentEvent::ToolResult { name, ok, output, .. } } => {
                Some((name.clone(), *ok, output.clone()))
            }
            _ => None,
        })
        .collect();
    assert!(results.iter().any(|(n, ok, o)| n == "read_file" && *ok && o == "notes from the test folder"), "{results:?}");
    assert!(results.iter().any(|(n, ok, o)| n == "write_file" && !*ok && o.contains("read-only")), "{results:?}");

    // The thread's files, from the device, over the hub's API (admin only).
    let get = |path: String, tok: &str| http.get(format!("{base}{path}")).bearer_auth(tok.to_string()).send();
    let listing: WorkspaceListing = get(format!("/v1/threads/{}/files", t.id), &admin).await.unwrap().json().await.unwrap();
    assert_eq!(listing.device_id, device.id);
    let paths: Vec<&str> = listing.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["out/result.md"]);
    let r = get(format!("/v1/threads/{}/files/out/result.md", t.id), &admin).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"result body from thread");
    assert_eq!(get(format!("/v1/threads/{}/files", t.id), &token).await.unwrap().status(), 403);
    assert_eq!(get(format!("/v1/threads/{}/files/missing.txt", t.id), &admin).await.unwrap().status(), 404);
    assert_eq!(get(format!("/v1/threads/{}/files/out/..%2F..%2Fx", t.id), &admin).await.unwrap().status(), 400);
    assert_eq!(get("/v1/threads/tnope/files".into(), &admin).await.unwrap().status(), 404);

    // The orchestrator, woken by the finished thread, quoted the file with read_thread_file.
    until("the orchestrator quoted the result", || async {
        find_event(&hub.db.events(&conv, 0, None), |b| {
            matches!(b, EventBody::AssistantMessage { text } if text == "Quoted the thread's result.")
        })
        .is_some()
    })
    .await;
    // History is saved when the turn ends, just after its reply is streamed, so wait for it.
    let mut history = String::new();
    for _ in 0..100 {
        history = serde_json::to_string(&hub.db.history(&conv)).unwrap();
        if history.contains("Quoted the thread's result.") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(history.contains("read_thread_file") && history.contains("result body from thread"), "{history}");

    // A hub thread's files are read from the hub's own workspace.
    let h = hub
        .start_thread(&conv, ThreadRequest { title: "Hub".into(), device: "hub".into(), goal: "go-hub".into(), ..Default::default() })
        .await
        .unwrap();
    until("the hub thread finished", || async { hub.db.thread(&h.id).unwrap().status == "done" }).await;
    let listing: WorkspaceListing = get(format!("/v1/threads/{}/files", h.id), &admin).await.unwrap().json().await.unwrap();
    assert_eq!((listing.device_id.as_str(), listing.files.len()), ("hub", 1));
    let r = get(format!("/v1/threads/{}/files/hub.txt", h.id), &admin).await.unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"made on the hub");

    // Device gone: 409.
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(15), client).await.expect("client stopped").unwrap();
    until("the device is offline", || async { !hub.is_online(&device.id).await }).await;
    let r = get(format!("/v1/threads/{}/files", t.id), &admin).await.unwrap();
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("Laptop is offline"), "{body}");
    // Folders stay listed (last reported) while the device is offline.
    assert_eq!(hub.devices().await.iter().find(|d| d.id == device.id).unwrap().folders.len(), 2);

    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}
