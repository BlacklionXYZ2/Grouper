//! End-to-end probe suite tests over the HTTP API, including a device-hosted
//! provider (`via_device`), daily budget refusal, and opt-in `models import --probe`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_client::core_protocol::{
    DeviceInfo, Limits, ModelEntry, ModelImportRequest, ProbeReport, ProviderConfig, RegistryProposal, RegistryStats,
};
use core_client::{Events, Status, ToolHandler};
use hub::catalog::{CatalogModel, import_with_cards};
use hub::db::Db;
use hub::server::{self, Hub};
use serde_json::{Value, json};

struct NoTools;
impl ToolHandler for NoTools {
    fn call(&self, tool: &str, _args: &str) -> Result<String, String> {
        Err(format!("unknown tool {tool}"))
    }
}

struct Quiet;
impl Events for Quiet {
    fn status(&self, _status: Status) {}
    fn presence(&self, _devices: Vec<DeviceInfo>) {}
}

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("grouper-probes-it-{tag}-{}", hub::db::random_hex(4)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Passes code, research, and tool_use; fails writing (uses forbidden word "data") and data (bad JSON).
fn mixed_probe_script() -> Value {
    json!({
        "rules": [
            {"last": "Python snippet", "reply": [{"type": "text", "text": "20"}]},
            {"last": "software backups", "reply": [{"type": "text", "text": "Backups save your data from failure. Restore copies anytime."}]},
            {"last": "second tallest", "reply": [{"type": "text", "text": "Alice"}]},
            {"last": "banana: 9", "reply": [{"type": "text", "text": "{\"total\": 10, \"top\": \"apple\"}"}]},
            {"last": "payments service", "reply": [{"type": "tool_use", "name": "lookup_service", "input": {"service": "payments", "env": "prod"}}]}
        ],
        "default": [{"type": "text", "text": "unexpected"}]
    })
}

/// Passes all 5 probes.
fn passing_probe_script() -> Value {
    json!({
        "rules": [
            {"last": "Python snippet", "reply": [{"type": "text", "text": "20"}]},
            {"last": "software backups", "reply": [{"type": "text", "text": "Backups shield your work from hardware failure and mistakes. Copies let you recover quickly without starting over."}]},
            {"last": "second tallest", "reply": [{"type": "text", "text": "Alice"}]},
            {"last": "banana: 9", "reply": [{"type": "text", "text": "{\"total\": 15, \"top\": \"banana\"}"}]},
            {"last": "payments service", "reply": [{"type": "tool_use", "name": "lookup_service", "input": {"service": "payments", "env": "prod"}}]}
        ],
        "default": [{"type": "text", "text": "unexpected"}]
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_probe_endpoint_auth_stats_proposals_and_budget() {
    let dir = temp("http");
    let db = Db::open(&dir.join("data")).unwrap();
    let (_plain, plain_token) = db.add_device("Reader", "linux", false).unwrap();
    let (_admin, admin_token) = db.add_device("Admin", "macos", true).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "scripted".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: json!({"inline": mixed_probe_script()}),
    })
    .unwrap();
    db.upsert_model(&ModelEntry {
        id: "coder".into(),
        provider_id: "scripted".into(),
        model: "coder-1".into(),
        input_price: 5.0,
        output_price: 15.0,
        cache_read_price: None,
        cache_write_price: None,
        context_window: 32_000,
        use_cases: String::new(),
        free: false,
        roles: vec!["thread".into()],
        local: false,
        modalities: vec!["text".into()],
        ..Default::default()
    })
    .unwrap();

    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, server::router(hub.clone())).into_future());
    let http = reqwest::Client::new();

    // Unauthenticated -> 401; non-admin -> 403; unknown model -> 404.
    let unauth = http.post(format!("http://{addr}/v1/models/coder/probe")).send().await.unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    let non_admin = http
        .post(format!("http://{addr}/v1/models/coder/probe"))
        .bearer_auth(&plain_token)
        .send()
        .await
        .unwrap();
    assert_eq!(non_admin.status(), reqwest::StatusCode::FORBIDDEN);
    let missing = http
        .post(format!("http://{addr}/v1/models/missing/probe"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    // Admin probe succeeds and returns per-probe results plus a pending proposal.
    let resp = http
        .post(format!("http://{addr}/v1/models/coder/probe"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let report: ProbeReport = resp.json().await.unwrap();
    assert_eq!(report.model_id, "coder");
    assert_eq!((report.passed, report.total), (3, 5));
    let proposal_id = report.proposal_id.expect("proposal drafted");
    assert_eq!(
        report.proposal_text.as_deref(),
        Some("Passed code, research and tool-use probes; failed writing and data")
    );

    // /v1/registry/stats exposes probe stats separately from thread stats.
    let stats: RegistryStats = http
        .get(format!("http://{addr}/v1/registry/stats"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(stats.models.is_empty());
    assert!(stats.devices.is_empty());
    assert_eq!(stats.probes.len(), 6);
    assert!(stats.probes.iter().all(|s| s.source == "probe"));

    // Accepting the proposal writes the drafted use cases onto the model.
    let accepted: RegistryProposal = http
        .post(format!("http://{addr}/v1/registry/proposals/{proposal_id}"))
        .bearer_auth(&admin_token)
        .json(&json!({"accept": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accepted.status, "accepted");
    assert_eq!(
        hub.db.find_model("coder").unwrap().use_cases,
        "Passed code, research and tool-use probes; failed writing and data"
    );

    // Spending the daily budget refuses further probes with 429 Too Many Requests.
    hub.db.set_limits(&Limits { daily_budget_usd: Some(0.0001), ..Default::default() }).unwrap();
    let refused = http
        .post(format!("http://{addr}/v1/models/coder/probe"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let body: Value = refused.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("daily budget"), "{body}");

    server.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probes_work_through_a_device_hosted_provider_and_report_offline() {
    let dir = temp("device");
    let db = Db::open(&dir.join("data")).unwrap();
    let (host, host_token) = db.add_device("Gaming PC", "windows", false).unwrap();
    let (_admin, admin_token) = db.add_device("Admin", "macos", true).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "local-lm".into(),
        kind: "scripted".into(),
        base_url: Some("http://127.0.0.1:45123/v1".into()),
        api_key: None,
        options: json!({
            "via_device": "Gaming PC",
            "inline": passing_probe_script()
        }),
    })
    .unwrap();
    db.upsert_model(&ModelEntry {
        id: "gemma-local".into(),
        provider_id: "local-lm".into(),
        model: "gemma-3".into(),
        input_price: 0.0,
        output_price: 0.0,
        cache_read_price: None,
        cache_write_price: None,
        context_window: 32_000,
        use_cases: String::new(),
        free: true,
        roles: vec!["thread".into()],
        local: true,
        modalities: vec!["text".into()],
        ..Default::default()
    })
    .unwrap();

    let hub = Hub::new(db, dir.join("data"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, server::router(hub.clone())).into_future());
    let http = reqwest::Client::new();

    // Before Gaming PC connects: 503 with a clear offline message and no recorded outcomes.
    let offline = http
        .post(format!("http://{addr}/v1/models/gemma-local/probe"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(offline.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = offline.json().await.unwrap();
    assert_eq!(body["error"].as_str(), Some("the model lives on Gaming PC, which is offline"));
    assert!(hub.db.probe_stats(Some("gemma-local"), None).is_empty());

    // Connect Gaming PC as a real core-client hosting the provider.
    let mcp_config = dir.join("mcp.json");
    std::fs::write(&mcp_config, r#"{"includeSubagentChat":false,"mcpServers":{}}"#).unwrap();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let client = tokio::spawn(core_client::run(
        core_client::Config {
            hub_url: format!("http://{addr}"),
            token: host_token,
            capabilities: Vec::new(),
            workspace_root: Some(dir.join("ws")),
            allow_shell: false,
            mcp_config: Some(mcp_config),
            folders: Vec::new(),
            usage_probes: false,
        },
        Arc::new(NoTools),
        Arc::new(Quiet),
        stop_rx,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !hub.is_online(&host.id).await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("hosting device online");
    // Wait briefly for Welcome + Providers to land on the device.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = http
        .post(format!("http://{addr}/v1/models/gemma-local/probe"))
        .bearer_auth(&admin_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let report: ProbeReport = resp.json().await.unwrap();
    assert_eq!((report.passed, report.total), (5, 5));
    assert_eq!(
        report.proposal_text.as_deref(),
        Some("Passed all 5 probes (code, writing, research, data and tool-use)")
    );
    assert_eq!(hub.db.probe_stats(Some("gemma-local"), None).len(), 6);

    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(10), client).await.unwrap().unwrap();
    server.abort();
    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn import_only_probes_when_explicitly_requested_and_merges_catalog_proposal() {
    let dir = temp("import");
    let db = Db::open(&dir.join("data")).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "openrouter".into(),
        kind: "scripted".into(),
        base_url: Some("https://openrouter.ai/api/v1".into()),
        api_key: None,
        options: json!({"inline": mixed_probe_script()}),
    })
    .unwrap();
    let hub = Hub::new(db, dir.join("data"));
    let cards = vec![CatalogModel {
        id: "acme/mini".into(),
        input_price: Some(1.0),
        output_price: Some(2.0),
        cache_read_price: None,
        cache_write_price: None,
        price_error: None,
        context_window: 64_000,
        free: false,
        modalities: vec!["text".into()],
        use_cases_draft: "Fast general model for everyday tasks.".into(),
    }];

    // Default (`probe: false`): drafts catalog proposal and runs ZERO probes.
    let without_probe = import_with_cards(
        &hub,
        "openrouter",
        &cards,
        &ModelImportRequest {
            provider: "openrouter".into(),
            model: "acme/mini".into(),
            id: Some("mini".into()),
            roles: Some(vec!["thread".into()]),
            local: None,
            probe: false,
        },
    )
    .await
    .unwrap();
    assert!(without_probe.probe.is_none());
    assert!(without_probe.probe_error.is_none());
    assert!(hub.db.probe_stats(Some("mini"), None).is_empty(), "must never probe automatically");
    assert!(hub.db.usage_totals(0, i64::MAX as u64, "model", None, None).unwrap().is_empty());
    let catalog_pid = without_probe.proposal_id.unwrap();
    assert_eq!(hub.db.proposal(catalog_pid).unwrap().source, "catalog");

    // Opt-in (`probe: true`): runs probes and updates the pending catalog proposal in place.
    let with_probe = import_with_cards(
        &hub,
        "openrouter",
        &cards,
        &ModelImportRequest {
            provider: "openrouter".into(),
            model: "acme/mini".into(),
            id: Some("mini".into()),
            roles: None,
            local: None,
            probe: true,
        },
    )
    .await
    .unwrap();
    let report = with_probe.probe.expect("probe ran");
    assert_eq!((report.passed, report.total), (3, 5));
    assert_eq!(with_probe.proposal_id, Some(catalog_pid));
    let merged = hub.db.proposal(catalog_pid).unwrap();
    assert_eq!(merged.status, "pending");
    assert_eq!(merged.source, "probe");
    assert_eq!(
        merged.text,
        "Fast general model for everyday tasks; Passed code, research and tool-use probes; failed writing and data"
    );
    assert!(merged.reason.contains("Drafted from the OpenRouter model card.") && merged.reason.contains("3/5 passed"));

    drop(hub);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn old_thread_outcomes_tables_gain_source_and_duration_ms_idempotently() {
    let dir = temp("migrate");
    let db_path = dir.join("grouper.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE thread_outcomes (
           thread_id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, model_id TEXT NOT NULL,
           device_id TEXT NOT NULL, task_type TEXT NOT NULL DEFAULT '', status TEXT NOT NULL,
           grade INTEGER, grade_note TEXT, user_rating INTEGER, duration_s INTEGER NOT NULL DEFAULT 0,
           cost REAL NOT NULL DEFAULT 0, tokens INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL);
         INSERT INTO thread_outcomes (thread_id, conversation_id, model_id, device_id, task_type, status, duration_s, cost, tokens, created)
           VALUES ('t1', 'c1', 'm1', 'hub', 'code', 'done', 5, 0.01, 100, 1);",
    )
    .unwrap();
    drop(conn);

    let db = Db::open(&dir).unwrap();
    let stats = db.registry_stats();
    assert_eq!(stats.models.len(), 2); // (all) + code
    assert_eq!(stats.models[0].source, "thread");
    assert!(stats.probes.is_empty());
    drop(db);
    // Re-opening stays idempotent.
    Db::open(&dir).unwrap();
    std::fs::remove_dir_all(dir).ok();
}
