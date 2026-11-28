//! Denials in process: a real hub, a hub thread and the scripted model.
//!
//! The user denies a thread's call. The thread asks the orchestrator, which waits for
//! the user; the user says "go ahead" in the chat and the orchestrator relays it. The
//! retry is asked again (the thread keeps no denial memory of its own; the hub's
//! per-conversation memory was cleared when the user spoke) and runs once approved.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_protocol::{AgentEvent, ChatRequest, Event, EventBody, ModelEntry, Permission, ProviderConfig, ToolDef};
use hub::db::Db;
use hub::server::{Hub, HUB_DEVICE};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(60);
const ORCH: &str = "orchestrator of Grouper";
const THREAD: &str = "You are a subagent";

fn setup(tag: &str, script: Value) -> (PathBuf, Arc<Hub>) {
    let dir = std::env::temp_dir().join(format!("grouper-denials-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
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
        free: false,
        roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
        local: false,
        modalities: vec![],
        ..Default::default()
    })
    .unwrap();
    // `list_files` is Auto unless the registry says otherwise.
    db.upsert_tools(
        HUB_DEVICE,
        &[ToolDef {
            name: "list_files".into(),
            description: "List the workspace.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }],
    );
    db.set_permission(HUB_DEVICE, "list_files", Permission::Manual).unwrap();
    let hub = Hub::new(db, dir.clone());
    (dir, hub)
}

async fn chat(hub: &Arc<Hub>, conversation: Option<&str>, text: &str) -> String {
    hub.chat(ChatRequest {
        conversation_id: conversation.map(String::from),
        mode: Some("project".into()),
        text: text.into(),
        model: None,
    })
    .await
    .unwrap()
}

async fn until_event(hub: &Hub, conv: &str, what: &str, pred: impl Fn(&Event) -> bool) {
    let found = tokio::time::timeout(WAIT, async {
        while !hub.db.events(conv, 0, None).iter().any(&pred) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if found.is_err() {
        let log: Vec<String> = hub.db.events(conv, 0, None).iter().map(|e| format!("{:?}", e.body)).collect();
        panic!("timed out waiting until {what}; events:\n{}", log.join("\n"));
    }
}

fn requested(hub: &Hub, conv: &str) -> Vec<String> {
    hub.db
        .events(conv, 0, None)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ApprovalRequested { approval_id, .. } => Some(approval_id),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_go_ahead_after_the_user_speaks_asks_again() {
    let script = json!({"rules": [
        {"system": ORCH, "last": "please", "reply": [{"type": "tool_use", "name": "start_thread",
            "input": {"title": "Look", "device": "hub", "goal": "List the workspace."}}]},
        {"system": ORCH, "last": "May I retry?", "reply": [{"type": "text", "text": "Asking the user."}]},
        {"system": ORCH, "last": "go ahead", "reply": [{"type": "tool_use", "name": "message_thread",
            "input": {"thread_id": "$THREAD_ID", "text": "The user says go ahead."}}]},
        {"system": THREAD, "last": "(empty)", "reply": [{"type": "text", "text": "Listed."}]},
        {"system": THREAD, "last": "user denied this call", "reply": [
            {"type": "tool_use", "name": "ask_orchestrator", "input": {"question": "May I retry?"}}]},
        {"system": THREAD, "last": "user says go ahead", "reply": [{"type": "tool_use", "name": "list_files", "input": {}}]},
        {"system": THREAD, "last": "# Brief", "reply": [{"type": "tool_use", "name": "list_files", "input": {}}]}
    ], "default": [{"type": "text", "text": "Noted."}]});
    let (dir, hub) = setup("relayed", script);

    let conv = chat(&hub, None, "please").await;
    until_event(&hub, &conv, "the first approval request", |e| matches!(e.body, EventBody::ApprovalRequested { .. }))
        .await;
    let first = requested(&hub, &conv);
    assert!(hub.resolve_approval(&first[0], false).await);
    until_event(&hub, &conv, "the orchestrator heard the thread ask", |e| {
        matches!(&e.body, EventBody::AssistantMessage { text } if text == "Asking the user.")
    })
    .await;
    assert_eq!(requested(&hub, &conv).len(), 1);

    let thread = hub.db.threads(Some(&conv)).remove(0);
    chat(&hub, Some(&conv), &format!("go ahead, Thread {} may retry", thread.id)).await;
    until_event(&hub, &conv, "the retry is asked again", |e| matches!(e.body, EventBody::ApprovalRequested { .. })
        && !first.iter().any(|id| matches!(&e.body, EventBody::ApprovalRequested { approval_id, .. } if approval_id == id)))
    .await;
    let all = requested(&hub, &conv);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(hub.resolve_approval(&all[1], true).await);
    until_event(&hub, &conv, "the thread finished", |e| {
        matches!(&e.body, EventBody::ThreadFinished { status, summary } if status == "done" && summary == "Listed.")
    })
    .await;

    let outputs: Vec<String> = hub
        .db
        .events(&conv, 0, None)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ThreadActivity { event: AgentEvent::ToolResult { name, output, .. } } if name == "list_files" => {
                Some(output)
            }
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 2, "{outputs:?}");
    assert!(outputs[0].contains("don't retry it or ask anyone to allow it"), "{outputs:?}");
    assert_eq!(outputs[1], "(empty)");

    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}
