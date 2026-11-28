//! The offline-device flow end to end, in process: a real hub, a real device
//! (core-client over WebSocket) and a scripted model.
//!
//! - Case A: a thread started on an offline device waits for it; the hub works to a
//!   checkpoint; when the device connects the hub's files are copied over and the
//!   same thread continues and finishes there; the orchestrator is woken throughout.
//! - Case B: the device disconnects mid-thread; the hub part sees its progress; on
//!   reconnect the thread continues on the device.
//! - Messages to and stopping a waiting thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_client::core_protocol::{
    AgentEvent, Capability, ChatRequest, DeviceInfo, Event, EventBody, ModelEntry, ProviderConfig, ThreadInfo,
    default_input_schema,
};
use core_client::{Events, Status, ToolHandler};
use hub::db::Db;
use hub::offline::WAITING_DEVICE;
use hub::server::{self, Hub};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Generous: the first run after a build can be slow on Windows.
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

struct Setup {
    dir: PathBuf,
    hub: Arc<Hub>,
    url: String,
    device_id: String,
    token: String,
}

async fn setup(tag: &str, script: Value) -> Setup {
    let dir = std::env::temp_dir().join(format!("grouper-offline-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open(&dir.join("data")).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({ "inline": script }),
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
        ..Default::default()
    })
    .unwrap();
    let (device, token) = db.add_device("Laptop", "windows", false).unwrap();
    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = server::router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    std::fs::write(dir.join("mcp.json"), json!({"includeSubagentChat": false, "mcpServers": {}}).to_string()).unwrap();
    Setup { dir, hub, url, device_id: device.id, token }
}

impl Setup {
    /// Connect the device ("Laptop"), with an `echo` platform tool (Manual by default).
    fn connect(&self) -> (watch::Sender<bool>, JoinHandle<()>) {
        let config = core_client::Config {
            hub_url: self.url.clone(),
            token: self.token.clone(),
            capabilities: vec![Capability {
                name: "echo".into(),
                description: "Returns its arguments".into(),
                input_schema: default_input_schema(),
            }],
            workspace_root: Some(self.device_workspaces()),
            allow_shell: false,
            mcp_config: Some(self.dir.join("mcp.json")),
            folders: Vec::new(),
            usage_probes: false,
            usage_probe_providers: None,
        };
        let (stop, stopped) = watch::channel(false);
        (stop, tokio::spawn(core_client::run(config, Arc::new(Tools), Arc::new(Quiet), stopped)))
    }

    async fn disconnect(&self, (stop, client): (watch::Sender<bool>, JoinHandle<()>)) {
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(15), client).await.expect("client stopped").unwrap();
        self.until("the device is offline", || async { !self.hub.is_online(&self.device_id).await }).await;
    }

    fn device_workspaces(&self) -> PathBuf {
        self.dir.join("device-workspaces")
    }

    async fn until<F, Fut>(&self, what: &str, mut check: F)
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

    async fn until_event(&self, conv: &str, what: &str, pred: impl Fn(&Event) -> bool) {
        let found = tokio::time::timeout(WAIT, async {
            while !self.hub.db.events(conv, 0, None).iter().any(&pred) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        if found.is_err() {
            let log: Vec<String> = self.hub.db.events(conv, 0, None).iter().map(|e| format!("{:?}", e.body)).collect();
            panic!("timed out waiting until {what}; events:\n{}", log.join("\n"));
        }
    }

    fn only_thread(&self, conv: &str) -> ThreadInfo {
        let threads = self.hub.db.threads(Some(conv));
        assert_eq!(threads.len(), 1, "{threads:?}");
        threads.into_iter().next().unwrap()
    }

    fn history_text(&self, conv: &str) -> String {
        serde_json::to_string(&self.hub.db.history(conv)).unwrap()
    }

    async fn chat(&self, text: &str) -> String {
        self.hub
            .chat(ChatRequest { conversation_id: None, mode: Some("project".into()), text: text.into(), model: None })
            .await
            .unwrap()
    }

    fn cleanup(self) {
        drop(self.hub);
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn reply(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

fn assistant_said(e: &Event, text: &str) -> bool {
    matches!(&e.body, EventBody::AssistantMessage { text: t } if t == text)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

const ORCH: &str = "orchestrator of Grouper";
const HUB_PART: &str = "on the Grouper hub";
const ON_LAPTOP: &str = "on the device \"Laptop\"";

#[tokio::test(flavor = "multi_thread")]
async fn offline_start_prepares_on_the_hub_and_hands_over() {
    let s = setup(
        "a",
        json!({"rules": [
            {"system": ORCH, "last": "build the site on Laptop", "reply": [{"type": "tool_use", "name": "start_thread",
                "input": {"title": "Build site", "device": "Laptop", "goal": "Build the site."}}]},
            {"system": ORCH, "last": "\"status\": \"waiting_device\"", "reply": reply("Laptop is offline; the hub is preparing.")},
            {"system": ORCH, "last": " done]", "reply": reply("All done.")},
            {"system": ORCH, "last": "reached its hub checkpoint", "reply": reply("Checkpoint noted.")},
            {"system": HUB_PART, "last": "Define the next checkpoint", "reply": [{"type": "tool_use", "name": "write_file",
                "input": {"path": "site/plan.md", "content": "PLAN-CONTENT step 1"}}]},
            {"system": HUB_PART, "last": "wrote ", "reply": reply("Checkpoint: site/plan.md has step 1. Left for Laptop: run the build.")},
            {"system": ON_LAPTOP, "last": "Continue from the checkpoint", "reply": [{"type": "tool_use", "name": "read_file",
                "input": {"path": "site/plan.md"}}]},
            {"system": ON_LAPTOP, "last": "PLAN-CONTENT", "reply": reply("Built from the checkpoint.")}
        ], "default": reply("OK")}),
    )
    .await;
    let conv = s.chat("please build the site on Laptop").await;

    // The thread waits for Laptop and keeps it as its device; the hub works to a checkpoint.
    s.until_event(&conv, "the hub checkpoint", |e| matches!(e.body, EventBody::ThreadCheckpoint { .. })).await;
    let t = s.only_thread(&conv);
    assert_eq!((t.status.as_str(), t.device_id.as_str()), (WAITING_DEVICE, s.device_id.as_str()));
    assert!(t.reason.as_deref().unwrap_or_default().contains("Laptop is offline"), "{:?}", t.reason);
    assert_eq!(t.summary.as_deref(), Some("Checkpoint: site/plan.md has step 1. Left for Laptop: run the build."));
    let hub_file = s.dir.join("data").join("workspaces").join(&t.id).join("site").join("plan.md");
    assert_eq!(read(&hub_file), "PLAN-CONTENT step 1");
    let events = s.hub.db.events(&conv, 0, Some(&t.id));
    assert!(events.iter().any(|e| matches!(&e.body,
        EventBody::ThreadWaiting { device_id, reason } if *device_id == s.device_id && reason.contains("offline"))));
    // The orchestrator was told in the tool result, and woken at the checkpoint.
    s.until_event(&conv, "the orchestrator heard of the checkpoint", |e| assistant_said(e, "Checkpoint noted.")).await;
    let history = s.history_text(&conv);
    assert!(history.contains("the thread waits for it (status waiting_device)"), "{history}");
    assert!(history.contains("reached its hub checkpoint and waits for Laptop"));
    assert!(history.contains("Waiting for their device"), "the status line lists waiting threads");

    // The hub part is done, so a message is queued for the continuation.
    s.hub.user_thread_message(&t.id, "Also add a footer").await.unwrap();

    // Laptop connects: the files are copied and the same thread continues and finishes there.
    let device = s.connect();
    s.until("the thread finished on Laptop", || async { s.hub.db.thread(&t.id).unwrap().status == "done" }).await;
    let t = s.hub.db.thread(&t.id).unwrap();
    assert_eq!((t.device_id.as_str(), t.summary.as_deref()), (s.device_id.as_str(), Some("Built from the checkpoint.")));
    assert_eq!(read(&s.device_workspaces().join(&t.id).join("site").join("plan.md")), "PLAN-CONTENT step 1");
    let events = s.hub.db.events(&conv, 0, Some(&t.id));
    let (files, skipped, brief) = events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadHandedOver { device_id, files, skipped, brief } if *device_id == s.device_id => {
                Some((files.clone(), skipped.clone(), brief.clone()))
            }
            _ => None,
        })
        .expect("handed over");
    assert_eq!((files, skipped), (vec!["site/plan.md".to_string()], vec![]));
    for part in [
        "# Brief: Build site",
        "## Checkpoint report from the hub\nCheckpoint: site/plan.md has step 1.",
        "- site/plan.md",
        "[Message from the user while the thread waited]\nAlso add a footer",
        "Continue from the checkpoint; the files from the hub are in your workspace.",
    ] {
        assert!(brief.contains(part), "{part:?} missing from {brief}");
    }
    // The continuation's steps are in the same thread's log, after the hand-over.
    let handed = events.iter().position(|e| matches!(e.body, EventBody::ThreadHandedOver { .. })).unwrap();
    assert!(events[handed..].iter().any(|e| matches!(&e.body,
        EventBody::ThreadActivity { event: AgentEvent::ToolResult { name, ok: true, output, .. } }
            if name == "read_file" && output == "PLAN-CONTENT step 1")));

    // The orchestrator was woken by the hand-over and the result.
    s.until_event(&conv, "the orchestrator heard of the result", |e| assistant_said(e, "All done.")).await;
    let history = s.history_text(&conv);
    assert!(history.contains("continues on Laptop"), "{history}");
    assert!(history.contains("with 1 file(s) from the hub workspace"));

    s.disconnect(device).await;
    s.cleanup();
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_mid_thread_waits_and_continues_on_reconnect() {
    let s = setup(
        "b",
        json!({"rules": [
            {"system": ORCH, "last": "fix the bug on Laptop", "reply": [{"type": "tool_use", "name": "start_thread",
                "input": {"title": "Fix bug", "device": "Laptop", "goal": "Fix the bug."}}]},
            {"system": ORCH, "last": " done]", "reply": reply("All done.")},
            {"system": ON_LAPTOP, "last": "Continue from the checkpoint", "reply": reply("Resumed and finished.")},
            // Says something, then waits on an approval (echo is Manual) until the device drops.
            {"system": ON_LAPTOP, "last": "Fix the bug.", "reply": [
                {"type": "text", "text": "PROGRESS-MARKER: found the bug in parser.rs"},
                {"type": "tool_use", "name": "echo", "input": {}}]},
            // The hub part only matches when it got the thread's progress; it can't add anything.
            {"system": HUB_PART, "last": "PROGRESS-MARKER", "reply": reply("HUB-CHECKPOINT: nothing to add without Laptop's tools.")}
        ], "default": reply("OK")}),
    )
    .await;
    let device = s.connect();
    s.until("Laptop's tools are known", || async { !s.hub.db.tools(Some(&s.device_id)).is_empty() }).await;
    let conv = s.chat("please fix the bug on Laptop").await;
    s.until_event(&conv, "the device thread waits for approval", |e| matches!(e.body, EventBody::ApprovalRequested { .. }))
        .await;
    let t = s.only_thread(&conv);
    assert_eq!(t.status, "running");

    s.disconnect(device).await;
    s.until_event(&conv, "the hub checkpoint", |e| matches!(e.body, EventBody::ThreadCheckpoint { .. })).await;
    let t = s.hub.db.thread(&t.id).unwrap();
    assert_eq!(t.status, WAITING_DEVICE);
    assert_eq!(t.summary.as_deref(), Some("HUB-CHECKPOINT: nothing to add without Laptop's tools."));
    let events = s.hub.db.events(&conv, 0, Some(&t.id));
    assert!(events.iter().any(|e| matches!(&e.body,
        EventBody::ThreadWaiting { reason, .. } if reason.contains("disconnected while the thread was running"))));
    s.until("the orchestrator was woken", || async { s.history_text(&conv).contains("is waiting for Laptop") }).await;

    // Laptop comes back: the thread continues there with its progress and the checkpoint.
    let device = s.connect();
    s.until("the thread finished on Laptop", || async { s.hub.db.thread(&t.id).unwrap().status == "done" }).await;
    assert_eq!(s.hub.db.thread(&t.id).unwrap().summary.as_deref(), Some("Resumed and finished."));
    let brief = s
        .hub
        .db
        .events(&conv, 0, Some(&t.id))
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ThreadHandedOver { files, brief, .. } => {
                assert!(files.is_empty());
                Some(brief)
            }
            _ => None,
        })
        .expect("handed over");
    for part in [
        "## Progress on Laptop before it disconnected\n- Said: PROGRESS-MARKER: found the bug in parser.rs",
        "HUB-CHECKPOINT: nothing to add",
        "the files from the hub are in your workspace under hub/",
    ] {
        assert!(brief.contains(part), "{part:?} missing from {brief}");
    }
    s.until_event(&conv, "the orchestrator heard of the result", |e| assistant_said(e, "All done.")).await;

    s.disconnect(device).await;
    s.cleanup();
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_threads_take_messages_and_can_be_stopped() {
    let s = setup(
        "stop",
        json!({"rules": [
            {"system": ORCH, "last": "draft it on Laptop", "reply": [{"type": "tool_use", "name": "start_thread",
                "input": {"title": "Draft", "device": "Laptop", "goal": "Draft the letter."}}]},
            // The hub part asks the orchestrator, which doesn't answer: it stays busy.
            {"system": HUB_PART, "last": "Define the next checkpoint", "reply": [{"type": "tool_use",
                "name": "ask_orchestrator", "input": {"question": "Which tone?"}}]}
        ], "default": reply("OK")}),
    )
    .await;
    let conv = s.chat("please draft it on Laptop").await;
    s.until_event(&conv, "the hub part asked the orchestrator", |e| {
        matches!(&e.body, EventBody::ThreadActivity { event: AgentEvent::ToolCall { name, .. } } if name == "ask_orchestrator")
    })
    .await;
    let t = s.only_thread(&conv);
    assert_eq!(t.status, WAITING_DEVICE);

    // A message goes to the running hub part.
    s.hub.user_thread_message(&t.id, "keep it short").await.unwrap();
    s.hub.stop_thread(&t.id, "user").await.unwrap();
    let t = s.hub.db.thread(&t.id).unwrap();
    assert_eq!((t.status.as_str(), t.summary.as_deref()), ("stopped", Some("Stopped by the user.")));
    assert!(s.hub.stop_thread(&t.id, "user").await.unwrap_err().contains("already stopped"));
    assert!(s.hub.user_thread_message(&t.id, "hello?").await.is_err());

    // The hub part ends without a checkpoint, and Laptop connecting doesn't revive the thread.
    let device = s.connect();
    s.until("Laptop's tools are known", || async { !s.hub.db.tools(Some(&s.device_id)).is_empty() }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let events = s.hub.db.events(&conv, 0, Some(&t.id));
    assert!(events.iter().any(|e| matches!(&e.body, EventBody::ThreadInput { from, text } if from == "user" && text == "keep it short")));
    assert!(events.iter().any(|e| matches!(&e.body, EventBody::ThreadFinished { status, .. } if status == "stopped")));
    assert!(!events.iter().any(|e| matches!(e.body, EventBody::ThreadCheckpoint { .. } | EventBody::ThreadHandedOver { .. })));
    assert_eq!(s.hub.db.thread(&t.id).unwrap().status, "stopped");
    assert!(s.hub.db.waiting_threads().is_empty());

    s.disconnect(device).await;
    s.cleanup();
}

#[tokio::test(flavor = "multi_thread")]
async fn handover_reports_failed_write_in_continuation_brief() {
    let s = setup(
        "ack-fail",
        json!({"rules": [
            {"system": ORCH, "last": "prepare two files on Laptop", "reply": [{"type": "tool_use", "name": "start_thread",
                "input": {"title": "Two files", "device": "Laptop", "goal": "Prepare two files."}}]},
            {"system": ORCH, "last": " done]", "reply": reply("All done.")},
            {"system": HUB_PART, "last": "Define the next checkpoint", "reply": [
                {"type": "tool_use", "name": "write_file", "input": {"path": "good.txt", "content": "ok"}},
                {"type": "tool_use", "name": "write_file", "input": {"path": "blocked.txt", "content": "nope"}}
            ]},
            {"system": HUB_PART, "last": "wrote ", "reply": reply("Checkpoint: wrote good.txt and blocked.txt.")},
            {"system": ON_LAPTOP, "last": "Continue from the checkpoint", "reply": reply("Finished with available files.")}
        ], "default": reply("OK")}),
    )
    .await;
    let conv = s.chat("please prepare two files on Laptop").await;
    s.until_event(&conv, "the hub checkpoint", |e| matches!(e.body, EventBody::ThreadCheckpoint { .. })).await;
    let t = s.only_thread(&conv);

    // Pre-create a directory at blocked.txt in the device's thread workspace so writing it fails on the device.
    std::fs::create_dir_all(s.device_workspaces().join(&t.id).join("blocked.txt")).unwrap();

    let device = s.connect();
    s.until("the thread finished on Laptop", || async { s.hub.db.thread(&t.id).unwrap().status == "done" }).await;
    assert_eq!(read(&s.device_workspaces().join(&t.id).join("good.txt")), "ok");

    let (files, skipped, brief) = s
        .hub
        .db
        .events(&conv, 0, Some(&t.id))
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ThreadHandedOver { files, skipped, brief, .. } => Some((files, skipped, brief)),
            _ => None,
        })
        .expect("handed over");
    assert_eq!(files, vec!["good.txt".to_string()]);
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(skipped[0].starts_with("blocked.txt ("), "{skipped:?}");
    assert!(brief.contains("- good.txt"), "{brief}");
    assert!(!brief.contains("- blocked.txt"), "{brief}");
    assert!(brief.contains("Not copied: blocked.txt ("), "{brief}");

    s.disconnect(device).await;
    s.cleanup();
}

#[tokio::test(flavor = "multi_thread")]
async fn handover_marks_unacknowledged_files_unconfirmed_on_timeout() {
    use core_client::core_protocol::{HubMessage, NodeMessage, PROTOCOL_VERSION};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMsg;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let s = setup(
        "ack-timeout",
        json!({"rules": [
            {"system": ORCH, "last": "prepare a file on Laptop", "reply": [{"type": "tool_use", "name": "start_thread",
                "input": {"title": "Legacy", "device": "Laptop", "goal": "Prepare a file."}}]},
            {"system": HUB_PART, "last": "Define the next checkpoint", "reply": [
                {"type": "tool_use", "name": "write_file", "input": {"path": "draft.txt", "content": "hello"}}
            ]},
            {"system": HUB_PART, "last": "wrote ", "reply": reply("Checkpoint: wrote draft.txt.")}
        ], "default": reply("OK")}),
    )
    .await;
    // Short ack timeout so the test runs quickly.
    s.hub.set_handover_ack_timeout(Duration::from_millis(300));

    let conv = s.chat("please prepare a file on Laptop").await;
    s.until_event(&conv, "the hub checkpoint", |e| matches!(e.body, EventBody::ThreadCheckpoint { .. })).await;
    let t = s.only_thread(&conv);

    // Connect a minimal protocol v4 device that never sends WorkspaceFileResult.
    let ws_url = core_client::core_protocol::connect_url(&s.url).unwrap();
    let mut req = ws_url.into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {}", s.token).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let hello = NodeMessage::Hello { protocol_version: PROTOCOL_VERSION, client_version: "legacy-v4".into(), host: None };
    ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap().into())).await.unwrap();

    // Wait for Welcome, then trigger hand_over directly (no MCP server needed for this mock socket).
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(WsMsg::Text(text))) = ws.next().await {
            if matches!(serde_json::from_str::<HubMessage>(&text), Ok(HubMessage::Welcome { .. })) {
                break;
            }
        }
    })
    .await
    .expect("welcomed");

    let hub_clone = s.hub.clone();
    let tid = t.id.clone();
    tokio::spawn(async move { hub_clone.hand_over(&tid).await });

    // Read WorkspaceFile (do NOT ack it) and wait for StartThread after timeout.
    let mut saw_file = false;
    let mut start_brief = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(WsMsg::Text(text))) = ws.next().await {
            match serde_json::from_str::<HubMessage>(&text) {
                Ok(HubMessage::WorkspaceFile { path, .. }) if path == "draft.txt" => saw_file = true,
                Ok(HubMessage::StartThread { spec }) if spec.thread_id == t.id => {
                    start_brief = Some(spec.brief);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("handover timed out to unconfirmed and started thread");

    assert!(saw_file);
    let brief = start_brief.unwrap();
    assert!(brief.contains("Unconfirmed (device did not acknowledge): draft.txt."), "{brief}");
    assert!(!brief.contains("- draft.txt"), "{brief}");

    let (files, skipped) = s
        .hub
        .db
        .events(&conv, 0, Some(&t.id))
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ThreadHandedOver { files, skipped, .. } => Some((files, skipped)),
            _ => None,
        })
        .expect("handed over");
    assert!(files.is_empty(), "{files:?}");
    assert_eq!(skipped, vec!["draft.txt (unconfirmed)".to_string()]);

    let _ = ws.close(None).await;
    s.cleanup();
}
