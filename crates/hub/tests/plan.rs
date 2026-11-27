//! `update_plan` in process: a real hub runs the plan demo from `examples/scripted-demo.json`
//! on a hub thread. The plan calls are stored as ordinary thread activity and never ask
//! for approval, even with the tool set to Manual.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_protocol::{AgentEvent, ChatRequest, Event, EventBody, ModelEntry, Permission, ProviderConfig, ToolDef};
use hub::db::Db;
use hub::server::{HUB_DEVICE, Hub};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(60);

fn setup(tag: &str) -> (PathBuf, Arc<Hub>) {
    let dir = std::env::temp_dir().join(format!("grouper-plan-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/scripted-demo.json");
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({ "script": script.to_string_lossy() }),
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
    // Even if a registry lists update_plan and someone sets it to Manual, it never asks.
    db.upsert_tools(
        HUB_DEVICE,
        &[ToolDef {
            name: "update_plan".into(),
            description: "Plan.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }],
    );
    db.set_permission(HUB_DEVICE, "update_plan", Permission::Manual).unwrap();
    let hub = Hub::new(db, dir.clone());
    (dir, hub)
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

#[tokio::test(flavor = "multi_thread")]
async fn the_plan_demo_stores_plan_calls_as_thread_activity_without_approvals() {
    let (dir, hub) = setup("demo");
    let conv = hub
        .chat(ChatRequest {
            conversation_id: None,
            mode: Some("project".into()),
            text: "Show me a plan demo".into(),
            model: None,
        })
        .await
        .unwrap();
    until_event(&hub, &conv, "the plan demo thread finished", |e| {
        matches!(&e.body, EventBody::ThreadFinished { status, .. } if status == "done")
    })
    .await;

    let events = hub.db.events(&conv, 0, None);
    assert!(
        !events.iter().any(|e| matches!(e.body, EventBody::ApprovalRequested { .. })),
        "update_plan must never ask for approval"
    );
    let plans: Vec<Value> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadActivity { event: AgentEvent::ToolCall { name, input, .. } } if name == "update_plan" => {
                Some(input.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 5, "{plans:?}");
    let statuses = |plan: &Value| -> Vec<String> {
        plan["items"].as_array().unwrap().iter().map(|i| i["status"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(statuses(&plans[0]), ["in_progress", "pending", "pending", "pending"]);
    assert_eq!(statuses(&plans[2]), ["completed", "completed", "in_progress", "pending"]);
    assert_eq!(plans[2]["items"][2]["active_form"], "Writing notes.md");
    assert!(statuses(&plans[4]).iter().all(|s| s == "completed"));

    let results: Vec<(bool, String)> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadActivity { event: AgentEvent::ToolResult { name, ok, output, .. } }
                if name == "update_plan" =>
            {
                Some((*ok, output.clone()))
            }
            _ => None,
        })
        .collect();
    let expected: Vec<(bool, String)> = (0..=4).map(|n| (true, format!("Plan updated: {n} of 4 done."))).collect();
    assert_eq!(results, expected);

    let thread = hub.db.threads(Some(&conv)).remove(0);
    assert_eq!(thread.status, "done");
    assert!(dir.join("workspaces").join(&thread.id).join("notes.md").exists());

    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}
