//! `GET /v1/usage?provider=` and `GET /v1/usage/free-requests` over HTTP, with
//! recorded usage rows and a recorded OpenRouter 429 (no provider is called).

use core_client::core_protocol::{ProviderConfig, UsageSnapshot, UsageTotal};
use hub::db::Db;
use hub::server::{self, Hub};
use hub::usage::{FreeRequestsToday, parse_free_limit};
use serde_json::Value;

fn openrouter(id: &str) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        kind: "openai".into(),
        base_url: Some("https://openrouter.ai/api/v1".into()),
        api_key: None,
        options: Value::Null,
    }
}

#[tokio::test]
async fn usage_filters_by_provider_and_reports_free_requests_sent_today() {
    let dir = std::env::temp_dir().join(format!("grouper-usage-free-requests-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).unwrap();
    db.upsert_provider(&openrouter("or")).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "s".into(),
        kind: "scripted".into(),
        base_url: None,
        api_key: None,
        options: Value::Null,
    })
    .unwrap();
    db.record_usage(None, None, "or", "meta/llama:free", 10, 1, 0, 0);
    db.record_usage(None, None, "or", "meta/llama:free", 20, 2, 0, 0);
    db.record_usage(None, None, "or", "openai/gpt-x", 30, 3, 0, 0);
    db.record_usage(None, None, "s", "fake-1", 40, 4, 0, 0);
    // The recorded 429 goes through the same parser and store the hook uses.
    let now = hub::db::now();
    let body = format!(
        r#"{{"error":{{"message":"Rate limit exceeded: free-models-per-day","code":429,"metadata":{{"headers":{{"X-RateLimit-Limit":"50","X-RateLimit-Remaining":"0","X-RateLimit-Reset":"{}"}},"limit_source":"openrouter_free_tier_daily"}}}}}}"#,
        (now + 3_600) * 1000
    );
    let limit = parse_free_limit(&body, now).unwrap();
    assert!(db.record_free_limit("or", &limit, now).unwrap());
    let (_, admin) = db.add_device("Phone", "ios", true).unwrap();

    let hub = Hub::new(db, dir.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, server::router(hub)).await });
    let http = reqwest::Client::new();
    let get = |path: String| {
        let request = http.get(format!("{base}{path}")).bearer_auth(&admin);
        async move { request.send().await.unwrap() }
    };

    let rows: Vec<UsageTotal> = get("/v1/usage?group=model".into()).await.json().await.unwrap();
    assert_eq!(rows.iter().map(|r| r.calls).sum::<u64>(), 4);
    let rows: Vec<UsageTotal> = get("/v1/usage?group=model&provider=or".into()).await.json().await.unwrap();
    assert_eq!(
        rows.iter().map(|r| (r.key.as_str(), r.calls)).collect::<Vec<_>>(),
        vec![("meta/llama:free", 2), ("openai/gpt-x", 1)]
    );
    let rows: Vec<UsageTotal> = get("/v1/usage?group=provider&provider=s".into()).await.json().await.unwrap();
    assert_eq!(rows.iter().map(|r| (r.key.as_str(), r.input_tokens)).collect::<Vec<_>>(), vec![("s", 40)]);
    // An empty value means no filter; an unknown provider has no rows.
    let rows: Vec<UsageTotal> = get("/v1/usage?group=provider&provider=".into()).await.json().await.unwrap();
    assert_eq!(rows.len(), 2);
    let rows: Vec<UsageTotal> = get("/v1/usage?group=provider&provider=nobody".into()).await.json().await.unwrap();
    assert!(rows.is_empty());

    let free: FreeRequestsToday = get("/v1/usage/free-requests".into()).await.json().await.unwrap();
    assert_eq!(free.providers.len(), 1, "only OpenRouter providers: {free:?}");
    assert_eq!((free.providers[0].provider.as_str(), free.providers[0].sent), ("or", 2));
    assert_eq!(free.until - free.since, 86_400);

    let snaps: Vec<UsageSnapshot> = get("/v1/usage/snapshots".into()).await.json().await.unwrap();
    let allowance = snaps.iter().find(|s| s.window == "free requests/day").expect("allowance snapshot");
    assert_eq!((allowance.limit_amount, allowance.used_amount, allowance.resets_at), (Some(50.0), Some(50.0), Some(now + 3_600)));

    // Without the admin token, neither is readable.
    let anonymous = http.get(format!("{base}/v1/usage/free-requests")).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    std::fs::remove_dir_all(dir).ok();
}
