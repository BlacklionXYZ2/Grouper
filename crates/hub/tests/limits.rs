//! Limits in process: a real hub, the scripted model and a priced model entry, so
//! every model call has an estimated cost (20 output tokens at $1,000 per million
//! = $0.02 a call).
//!
//! - A looping hub thread is stopped at its budget and the orchestrator is told.
//! - A second thread is refused at `max_running_threads = 1`.
//! - A spent daily budget refuses the next orchestrator turn until it's raised.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_protocol::{ChatRequest, Event, EventBody, Limits, ModelEntry, ProviderConfig};
use hub::db::Db;
use hub::server::Hub;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(60);
const ORCH: &str = "orchestrator of Grouper";
const THREAD: &str = "You are a subagent";
/// What one scripted call costs with the model below.
const CALL: f64 = 0.02;

fn setup(tag: &str, script: Value, limits: Limits) -> (PathBuf, Arc<Hub>) {
    let dir = std::env::temp_dir().join(format!("grouper-limits-{tag}-{}", std::process::id()));
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
        id: "priced".into(),
        provider_id: "s".into(),
        model: "fake-1".into(),
        input_price: 0.0,
        output_price: 1000.0,
        cache_read_price: None,
        cache_write_price: None,
        context_window: 0,
        use_cases: String::new(),
        free: false,
        roles: vec!["orchestrator".into(), "standard".into(), "thread".into()],
        local: false,
        modalities: vec![],
    })
    .unwrap();
    db.set_limits(&limits).unwrap();
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

fn said(e: &Event, text: &str) -> bool {
    matches!(&e.body, EventBody::AssistantMessage { text: t } if t == text)
}

fn usage_calls(hub: &Hub, conv: &str) -> u64 {
    hub.db.usage_totals(0, i64::MAX as u64, "conversation", Some(conv)).unwrap().iter().map(|r| r.calls).sum()
}

fn turns_finished(hub: &Hub, conv: &str) -> usize {
    hub.db.events(conv, 0, None).iter().filter(|e| matches!(e.body, EventBody::TurnFinished { .. })).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_thread_is_stopped_at_its_budget_and_the_orchestrator_is_told() {
    let script = json!({"rules": [
        {"system": ORCH, "last": "loop please", "reply": [{"type": "tool_use", "name": "start_thread",
            "input": {"title": "Loop", "device": "hub", "goal": "List files forever."}}]},
        {"system": ORCH, "last": "budget of", "reply": [{"type": "text", "text": "Budget stop noted."}]},
        {"system": THREAD, "reply": [{"type": "tool_use", "name": "list_files", "input": {}}]}
    ], "default": [{"type": "text", "text": "OK"}]});
    let (dir, hub) = setup("thread", script, Limits { thread_budget_usd: Some(0.05), ..Default::default() });

    let conv = chat(&hub, None, "loop please").await;
    until_event(&hub, &conv, "the orchestrator heard about the stop", |e| said(e, "Budget stop noted.")).await;

    let threads = hub.db.threads(Some(&conv));
    assert_eq!(threads.len(), 1);
    let t = &threads[0];
    assert_eq!(t.status, "stopped");
    let summary = t.summary.as_deref().unwrap();
    assert!(summary.starts_with("Stopped: budget of $0.05 reached (estimated cost $"), "{summary}");
    let spent = hub.db.spent_by_thread(&t.id);
    // Stopped at the third call ($0.06); one more call may land before the stop does.
    assert!((0.05..=0.05 + 2.0 * CALL + 1e-9).contains(&spent), "spent {spent}");

    let events = hub.db.events(&conv, 0, None);
    assert!(
        events.iter().any(|e| matches!(&e.body, EventBody::ThreadFinished { status, summary: s }
            if status == "stopped" && s.as_str() == summary)),
        "the stop goes through the normal path"
    );
    // One warning at 80% ($0.04), for the thread.
    let warnings: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::BudgetWarning { scope, spent, limit } => Some((e.thread_id.clone(), scope.clone(), *spent, *limit)),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!((warnings[0].0.as_deref(), warnings[0].1.as_str(), warnings[0].3), (Some(t.id.as_str()), "thread", 0.05));
    assert!((warnings[0].2 - 0.04).abs() < 1e-9);
    // The orchestrator saw the budget on start_thread's result and the stop as a wake-up.
    let history = saved_history(&hub, &conv, "Budget stop noted.").await;
    assert!(history.contains(r#"\"budget_usd\": 0.05"#), "{history}");
    assert!(history.contains("stopped]") && history.contains("Stopped: budget of $0.05 reached"), "{history}");
    assert!(history.contains("Limits (spend is estimated, USD): $0.05 per thread."), "status line: {history}");

    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_thread_is_refused_at_the_running_thread_cap() {
    let script = json!({"delay_ms": 100, "rules": [
        {"system": ORCH, "last": "two please", "reply": [
            {"type": "tool_use", "name": "start_thread", "input": {"title": "A", "device": "hub", "goal": "Loop."}},
            {"type": "tool_use", "name": "start_thread", "input": {"title": "B", "device": "hub", "goal": "Loop."}}
        ]},
        {"system": ORCH, "last": "max_running_threads = 1", "reply": [{"type": "text", "text": "One was refused."}]},
        {"system": THREAD, "reply": [{"type": "tool_use", "name": "list_files", "input": {}}]}
    ], "default": [{"type": "text", "text": "OK"}]});
    let (dir, hub) = setup("cap", script, Limits { max_running_threads: Some(1), ..Default::default() });

    let conv = chat(&hub, None, "two please").await;
    until_event(&hub, &conv, "the orchestrator relayed the refusal", |e| said(e, "One was refused.")).await;

    let threads = hub.db.threads(Some(&conv));
    assert_eq!(threads.len(), 1, "both start_thread calls ran in parallel; only one may start: {threads:?}");
    assert_eq!(threads[0].status, "running");
    let history = saved_history(&hub, &conv, "One was refused.").await;
    assert!(history.contains("Not started: 1 thread(s) already running"), "{history}");

    hub.stop_thread(&threads[0].id, "user").await.unwrap();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spent_daily_budget_refuses_turns_until_it_is_raised() {
    let script = json!({"rules": [
        {"system": ORCH, "last": "third", "reply": [{"type": "text", "text": "Back."}]},
        {"system": ORCH, "last": "hello", "reply": [{"type": "text", "text": "Hi."}]}
    ], "default": [{"type": "text", "text": "OK"}]});
    // One orchestrator call ($0.02) spends it.
    let (dir, hub) = setup("daily", script, Limits { daily_budget_usd: Some(0.01), ..Default::default() });

    let conv = chat(&hub, None, "hello").await;
    until_event(&hub, &conv, "the first reply", |e| said(e, "Hi.")).await;
    until_event(&hub, &conv, "the first turn ended", |e| matches!(e.body, EventBody::TurnFinished { .. })).await;
    assert_eq!(usage_calls(&hub, &conv), 1);

    chat(&hub, Some(&conv), "hello again").await;
    until_event(&hub, &conv, "the refusal", |e| {
        matches!(&e.body, EventBody::Error { message } if message.starts_with("The daily budget of $0.01 is spent")
            && message.contains("00:00 UTC") && message.contains("grouper limits set daily_budget_usd"))
    })
    .await;
    assert_eq!(usage_calls(&hub, &conv), 1, "no model call for a refused turn");
    // The refused turn still ends, so `grouper chat` returns.
    tokio::time::timeout(WAIT, async {
        while turns_finished(&hub, &conv) < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the refused turn finished");

    // Raised: the next turn runs and sees what was said while it was refused.
    let mut limits = hub.db.limits();
    limits.daily_budget_usd = Some(1.0);
    hub.db.set_limits(&limits).unwrap();
    chat(&hub, Some(&conv), "third").await;
    until_event(&hub, &conv, "the turn after raising it", |e| said(e, "Back.")).await;
    let history = serde_json::to_string(&hub.db.history(&conv)).unwrap();
    assert!(history.contains("hello again") && history.contains("third"), "{history}");

    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

/// A turn's history is saved when the turn ends, just after its reply is streamed, so
/// wait until it holds `needle` (or give up after 5 s and return what's there).
async fn saved_history(hub: &std::sync::Arc<hub::server::Hub>, conv: &str, needle: &str) -> String {
    let mut history = String::new();
    for _ in 0..100 {
        history = serde_json::to_string(&hub.db.history(conv)).unwrap();
        if history.contains(needle) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    history
}
