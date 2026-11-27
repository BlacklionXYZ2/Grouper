//! Device subscription usage probes end-to-end (no live provider calls):
//! - A device without `--usage-probes` never advertises probes, never lists
//!   `grouper_usage_probe` in the tool registry, and refuses direct calls (404).
//! - Stored probe snapshots carry `device_id`, appear in `GET /v1/usage/snapshots`
//!   and the orchestrator's `list_usage` tool, and stay available with their age
//!   and `device_online: false` once the device disconnects.

use std::sync::Arc;
use std::time::Duration;

use core_client::core_protocol::{
    ChatRequest, DeviceInfo, EventBody, ModelEntry, ProviderConfig, ToolCallRequest, USAGE_PROBE_TOOL,
    UsageProbeEntry, UsageSnapshot,
};
use core_client::{Events, Status, ToolHandler};
use hub::db::Db;
use hub::server::{self, Hub};
use serde_json::{Value, json};

struct NoTools;
impl ToolHandler for NoTools {
    fn call(&self, _tool: &str, args: &str) -> Result<String, String> {
        Ok(args.to_string())
    }
}

struct Quiet;
impl Events for Quiet {
    fn status(&self, _status: Status) {}
    fn presence(&self, _devices: Vec<DeviceInfo>) {}
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_probe_tool_is_unlisted_and_snapshots_persist_when_device_goes_offline() {
    let dir = std::env::temp_dir().join(format!("grouper-usage-probes-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mcp.json"), json!({"includeSubagentChat": false, "mcpServers": {}}).to_string()).unwrap();

    let db = Db::open(&dir.join("data")).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({"inline": {"rules": [
            {"system": "orchestrator of Grouper", "last": "check usage", "reply": [
                {"type": "tool_use", "name": "list_usage", "input": {}}
            ]},
            {"system": "orchestrator of Grouper", "last": "latest_snapshots", "reply": [
                {"type": "text", "text": "Checked usage snapshots."}
            ]}
        ], "default": [{"type": "text", "text": "OK"}]}}),
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
    let (_, admin) = db.add_device("Phone", "ios", true).unwrap();
    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = server::router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });

    // Connect device with usage_probes = false (default off).
    let config = core_client::Config {
        hub_url: base.clone(),
        token: token.clone(),
        capabilities: vec![],
        workspace_root: Some(dir.join("workspaces")),
        allow_shell: false,
        mcp_config: Some(dir.join("mcp.json")),
        folders: vec![],
        usage_probes: false,
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let client = tokio::spawn(core_client::run(config, Arc::new(NoTools), Arc::new(Quiet), stopped));

    tokio::time::timeout(Duration::from_secs(20), async {
        while !hub.devices().await.iter().any(|d| d.id == device.id && d.online && !d.capabilities.is_empty()) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("device connected");

    // Probe tool must NEVER appear in the tool registry or be callable via /call.
    let tools = hub.db.all_tools(Some(&device.id));
    assert!(!tools.iter().any(|t| t.name == USAGE_PROBE_TOOL), "{tools:?}");
    let http = reqwest::Client::new();
    let call_resp = http
        .post(format!("{base}/v1/devices/{}/call", device.id))
        .bearer_auth(&admin)
        .json(&ToolCallRequest { tool: USAGE_PROBE_TOOL.into(), args: json!({}) })
        .send()
        .await
        .unwrap();
    assert_eq!(call_resp.status(), 404);

    // Refresh must skip opted-out device.
    let refreshed = hub::usage::refresh(&hub).await;
    assert_eq!(refreshed.devices_probed, 0);

    // Store a recorded probe result for this device, then verify GET /v1/usage/snapshots.
    let now = hub::db::now();
    hub.db
        .store_device_probe_results(
            &device.id,
            &[UsageProbeEntry {
                provider: "claude".into(),
                status: "ok".into(),
                message: None,
                snapshots: vec![UsageSnapshot {
                    account: "claude-max".into(),
                    provider: "claude".into(),
                    kind: "window".into(),
                    window: "5h".into(),
                    used_pct: Some(42.0),
                    used_amount: None,
                    limit_amount: None,
                    currency: None,
                    resets_at: Some(now + 3_600),
                    source: "probe".into(),
                    device_id: None,
                    fetched_at: now.saturating_sub(90),
                }],
            }],
            now.saturating_sub(90),
        )
        .unwrap();

    let snaps: Vec<UsageSnapshot> = http
        .get(format!("{base}/v1/usage/snapshots"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].device_id.as_deref(), Some(device.id.as_str()));
    assert_eq!(snaps[0].provider, "claude");
    assert_eq!(snaps[0].window, "5h");
    assert_eq!(snaps[0].used_pct, Some(42.0));
    assert_eq!(snaps[0].source, "probe");

    // Disconnect the device: snapshot stays stored and list_usage reports device_online: false and age_secs.
    stop.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), client).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while hub.is_online(&device.id).await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("device offline");

    let snaps_offline: Vec<UsageSnapshot> = http
        .get(format!("{base}/v1/usage/snapshots"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snaps_offline.len(), 1);
    assert_eq!(snaps_offline[0].used_pct, Some(42.0));

    // Verify orchestrator's list_usage tool includes the snapshot with device_online=false and age_secs.
    let conv = hub
        .chat(ChatRequest {
            conversation_id: None,
            mode: Some("project".into()),
            text: "check usage".into(),
            model: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let events = hub.db.events(&conv, 0, None);
            if events
                .iter()
                .any(|e| matches!(&e.body, EventBody::AssistantMessage { text } if text == "Checked usage snapshots."))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("orchestrator finished");

    // History is saved when the turn ends, just after its reply is streamed, so wait for it.
    let mut history = hub.db.history(&conv);
    for _ in 0..100 {
        if serde_json::to_string(&history).unwrap().contains("Checked usage snapshots.") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        history = hub.db.history(&conv);
    }
    let history_json = serde_json::to_string(&history).unwrap();
    let parsed_tool_output: Value = history
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|block| {
            let v = serde_json::to_value(block).ok()?;
            let content = v.get("content").and_then(Value::as_str)?;
            content.contains("latest_snapshots").then(|| serde_json::from_str(content).ok()).flatten()
        })
        .unwrap_or_else(|| panic!("expected list_usage output in history: {history_json}"));
    let snap_obj = &parsed_tool_output["latest_snapshots"][0];
    assert_eq!(snap_obj["provider"], "claude");
    assert_eq!(snap_obj["device_id"], device.id);
    assert_eq!(snap_obj["device_name"], "Laptop");
    assert_eq!(snap_obj["device_online"], false);
    assert!(snap_obj["age_secs"].as_u64().unwrap_or(0) >= 85, "{snap_obj}");

    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}
