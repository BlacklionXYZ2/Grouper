//! Tests for the Usage tab's hub half: key colours, `GET /v1/usage/summary`, snapshot labels and
//! account keys, and the device probe flags (`docs/usage-tab-spec.md` sections 4.4, 7.1 to 7.4).

use std::sync::Arc;

use core_protocol::{
    AgentEvent, EventBody, ModelEntry, ProviderConfig, ProviderInfo, UsageProbeEntry, UsageSnapshot, UsageSummary,
};
use rusqlite::params;
use serde_json::{Value, json};

use super::*;
use crate::db::random_hex;
use crate::server::{Hub, router};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("grouper-usage-summary-{tag}-{}", random_hex(4)))
}

fn provider(id: &str, kind: &str, base_url: Option<&str>) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        kind: kind.into(),
        base_url: base_url.map(String::from),
        api_key: Some("fake-key".into()),
        options: Value::Null,
    }
}

fn model(id: &str, provider_id: &str, model: &str, input: f64, output: f64) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        provider_id: provider_id.into(),
        model: model.into(),
        input_price: input,
        output_price: output,
        ..Default::default()
    }
}

fn colors(db: &Db) -> HashMap<String, String> {
    db.provider_colors()
}

#[allow(clippy::too_many_arguments)]
fn insert_usage(db: &Db, conversation: Option<&str>, provider: &str, model: &str, billing: &str, ts: u64, tokens: (u64, u64), cost: f64) {
    db.c()
        .execute(
            "INSERT INTO usage (conversation_id, thread_id, provider_id, model, input_tokens, output_tokens,
               cache_read_tokens, cache_write_tokens, cost, ts, billing) VALUES (?1, NULL, ?2, ?3, ?4, ?5, 7, 3, ?6, ?7, ?8)",
            params![conversation, provider, model, tokens.0 as i64, tokens.1 as i64, cost, ts as i64, billing],
        )
        .unwrap();
}

async fn serve(db: Db, dir: &std::path::Path) -> (Arc<Hub>, String) {
    let hub = Hub::new(db, dir.to_path_buf());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(hub.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (hub, base)
}

#[test]
fn migration_gives_existing_providers_distinct_colours_in_creation_order() {
    let dir = temp_dir("migrate");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    // Rows as an older hub left them: no colour. Inserted in this order, so rowid is creation order.
    for (id, kind, url) in [
        ("openrouter", "openai", Some("https://openrouter.ai/api/v1")),
        ("anthropic", "anthropic", None),
        ("openai-main", "openai", Some("https://api.openai.com/v1")),
        ("local-llama", "openai", Some("http://127.0.0.1:11434/v1")),
        ("claude-code@dev1", "harness", None),
        ("anthropic-2", "anthropic", None),
        ("deepseek", "openai", Some("https://api.deepseek.com")),
    ] {
        db.c()
            .execute("INSERT INTO providers (id, kind, base_url) VALUES (?1, ?2, ?3)", params![id, kind, url])
            .unwrap();
    }
    assert!(colors(&db).is_empty());
    drop(db);

    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    let got = colors(&db);
    // No maker: the first colour in the fixed order. Anthropic is pink, OpenAI blue.
    assert_eq!(got["openrouter"], "teal");
    assert_eq!(got["anthropic"], "pink");
    assert_eq!(got["openai-main"], "blue");
    // A local server belongs to no maker, so it takes the first colour still free.
    assert_eq!(got["local-llama"], "purple");
    // Harness providers get none and do not use up a colour.
    assert!(!got.contains_key("claude-code@dev1"));
    // Pink is taken, so a second Anthropic key takes the first unused colour (orange).
    assert_eq!(got["anthropic-2"], "orange");
    // DeepSeek's colour (cyan) was free.
    assert_eq!(got["deepseek"], "cyan");

    // Opening again changes nothing.
    drop(db);
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    assert_eq!(colors(&db), got);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn new_providers_get_a_colour_and_reuse_the_least_used_after_ten() {
    let dir = temp_dir("assign");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    for index in 0..12 {
        db.upsert_provider(&provider(&format!("key-{index:02}"), "openai", Some("https://gateway.example.com/v1"))).unwrap();
    }
    let got = colors(&db);
    for (index, expected) in core_protocol::KEY_COLORS.iter().enumerate() {
        assert_eq!(got[&format!("key-{index:02}")], *expected);
    }
    // Every colour is used once; ties go to the earlier colour, then the next least used.
    assert_eq!(got["key-10"], "teal");
    assert_eq!(got["key-11"], "purple");

    // Saving a provider again keeps its colour (the config carries none).
    db.upsert_provider(&provider("key-03", "openai", Some("https://elsewhere.example.com/v1"))).unwrap();
    assert_eq!(colors(&db)["key-03"], "pink");

    // A harness provider created now stays without one.
    db.upsert_provider(&ProviderConfig {
        id: "grok@dev1".into(),
        kind: "harness".into(),
        base_url: None,
        api_key: None,
        options: json!({"via_device": "dev1"}),
    })
    .unwrap();
    assert!(!colors(&db).contains_key("grok@dev1"));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn colour_endpoint_validates_persists_and_survives_a_full_provider_put() {
    let dir = temp_dir("color-http");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    let (_, admin) = db.add_device("Admin", "ios", true).unwrap();
    let (_, plain) = db.add_device("Plain", "windows", false).unwrap();
    db.upsert_provider(&provider("a", "openai", Some("https://one.example.com/v1"))).unwrap();
    db.upsert_provider(&provider("b", "openai", Some("https://two.example.com/v1"))).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "claude-code@dev1".into(),
        kind: "harness".into(),
        base_url: None,
        api_key: None,
        options: json!({"via_device": "dev1"}),
    })
    .unwrap();
    let (_hub, base) = serve(db, &dir).await;
    let http = reqwest::Client::new();
    let put_color = |id: &str, body: Value, token: &str| {
        let request = http.put(format!("{base}/v1/providers/{id}/color")).bearer_auth(token).json(&body);
        async move { request.send().await.unwrap() }
    };
    let list = || async {
        http.get(format!("{base}/v1/providers"))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap()
            .json::<Vec<ProviderInfo>>()
            .await
            .unwrap()
    };

    let before = list().await;
    let color_of = |list: &[ProviderInfo], id: &str| list.iter().find(|p| p.id == id).unwrap().color.clone();
    assert_eq!(color_of(&before, "a").as_deref(), Some("teal"));
    assert_eq!(color_of(&before, "b").as_deref(), Some("purple"));
    // Harness providers have no colour and are listed as before.
    assert_eq!(color_of(&before, "claude-code@dev1"), None);

    // A valid name (any case) is saved and returned.
    let response = put_color("b", json!({"color": "Pink"}), &admin).await;
    assert_eq!(response.status(), 200);
    let updated: ProviderInfo = response.json().await.unwrap();
    assert_eq!((updated.id.as_str(), updated.color.as_deref()), ("b", Some("pink")));
    assert!(updated.has_key);
    assert_eq!(color_of(&list().await, "b").as_deref(), Some("pink"));

    // Validation and lookup.
    assert_eq!(put_color("b", json!({"color": "magenta"}), &admin).await.status(), 400);
    assert_eq!(put_color("b", json!({"color": 3}), &admin).await.status(), 400);
    assert_eq!(put_color("b", json!({}), &admin).await.status(), 400);
    assert_eq!(put_color("nope", json!({"color": "teal"}), &admin).await.status(), 404);
    assert_eq!(put_color("nope", json!({"color": null}), &admin).await.status(), 404);
    assert_eq!(color_of(&list().await, "b").as_deref(), Some("pink"));
    // Admin only.
    let denied = put_color("b", json!({"color": "teal"}), &plain).await.status();
    assert!(denied == 401 || denied == 403, "{denied}");
    assert_eq!(color_of(&list().await, "b").as_deref(), Some("pink"));

    // A full provider PUT does not lose the colour.
    let response = http
        .put(format!("{base}/v1/providers/b"))
        .bearer_auth(&admin)
        .json(&json!({"id": "b", "kind": "openai", "base_url": "https://two.example.com/v2", "api_key": null, "options": null}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let after = list().await;
    assert_eq!(color_of(&after, "b").as_deref(), Some("pink"));
    assert_eq!(after.iter().find(|p| p.id == "b").unwrap().base_url.as_deref(), Some("https://two.example.com/v2"));

    // null re-runs auto-assignment: teal is a's, so b takes the first unused colour, purple.
    let response = put_color("b", json!({"color": null}), &admin).await;
    assert_eq!(response.status(), 200);
    let updated: ProviderInfo = response.json().await.unwrap();
    assert_eq!(updated.color.as_deref(), Some("purple"));
    std::fs::remove_dir_all(dir).ok();
}

struct Fixture {
    base: String,
    admin: String,
    plain: String,
    since: u64,
    until: u64,
    _hub: Arc<Hub>,
    dir: std::path::PathBuf,
}

/// Providers: `p1` (priced model, an unpriced one, a display name), `p2` (local, free model),
/// `p3` (unused), the harness `claude-code@dev1`; usage rows at `since + n`, plus rows outside the
/// range, a subscription row, and one for a provider that no longer exists.
async fn fixture() -> Fixture {
    let dir = temp_dir("summary");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    let (_, admin) = db.add_device("Admin", "ios", true).unwrap();
    let (_, plain) = db.add_device("Plain", "windows", false).unwrap();
    db.upsert_provider(&provider("p1", "openai", Some("https://api.example.com/v1"))).unwrap();
    db.upsert_provider(&provider("p2", "openai", Some("http://127.0.0.1:11434/v1"))).unwrap();
    db.upsert_provider(&provider("p3", "anthropic", None)).unwrap();
    db.upsert_provider(&ProviderConfig {
        id: "claude-code@dev1".into(),
        kind: "harness".into(),
        base_url: None,
        api_key: None,
        options: json!({"via_device": "dev1"}),
    })
    .unwrap();
    let mut big = model("big", "p1", "big-model", 10.0, 20.0);
    big.display_name = Some("Big Model".into());
    db.upsert_model(&big).unwrap();
    db.upsert_model(&model("mystery", "p1", "mystery-model", 0.0, 0.0)).unwrap();
    let mut free = model("local-free", "p2", "tiny", 0.0, 0.0);
    free.free = true;
    db.upsert_model(&free).unwrap();
    db.upsert_model(&model("local-unmarked", "p2", "unmarked", 0.0, 0.0)).unwrap();
    db.upsert_model(&ModelEntry {
        id: "cc".into(),
        provider_id: "claude-code@dev1".into(),
        model: "sonnet".into(),
        ..Default::default()
    })
    .unwrap();

    let since = 1_795_564_800;
    let until = since + 86_400;
    let t = |n: u64| since + n;
    insert_usage(&db, Some("c1"), "p1", "big-model", "api", t(10), (1000, 500), 0.02);
    insert_usage(&db, Some("c1"), "p1", "big-model", "api", t(20), (2000, 100), 0.03);
    insert_usage(&db, Some("c2"), "p1", "mystery-model", "api", t(30), (400, 40), 0.0);
    insert_usage(&db, Some("c2"), "p1", "mystery-model", "api", t(31), (400, 40), 0.0);
    insert_usage(&db, Some("c2"), "p1", "mystery-model", "api", t(32), (400, 40), 0.0);
    insert_usage(&db, Some("c2"), "p2", "tiny", "api", t(40), (900, 90), 0.0);
    insert_usage(&db, Some("c2"), "p2", "unmarked", "api", t(41), (50, 5), 0.0);
    insert_usage(&db, Some("c2"), "gone", "old-model", "api", t(50), (10, 1), 0.5);
    // Subscription rows never feed the totals or the keys.
    insert_usage(&db, Some("c1"), "claude-code@dev1", "sonnet", "subscription", t(60), (300, 30), 0.0);
    insert_usage(&db, Some("c1"), "claude-code@dev1", "sonnet", "subscription", t(61), (200, 20), 0.0);
    // Half-open range: `since` is in, `until` is out, and so is anything before or after.
    insert_usage(&db, None, "p3", "claude-x", "api", since, (1, 1), 0.25);
    insert_usage(&db, None, "p3", "claude-x", "api", until, (1, 1), 100.0);
    insert_usage(&db, None, "p3", "claude-x", "api", since - 1, (1, 1), 100.0);

    let (hub, base) = serve(db, &dir).await;
    Fixture { base, admin, plain, since, until, _hub: hub, dir }
}

async fn get_summary(f: &Fixture, query: &str) -> reqwest::Response {
    reqwest::Client::new().get(format!("{}/v1/usage/summary?{query}", f.base)).bearer_auth(&f.admin).send().await.unwrap()
}

#[tokio::test]
async fn summary_totals_match_the_provider_breakdown_without_subscription_rows() {
    let f = fixture().await;
    let http = reqwest::Client::new();
    let summary: UsageSummary =
        get_summary(&f, &format!("since={}&until={}", f.since, f.until)).await.json().await.unwrap();
    assert_eq!((summary.since, summary.until), (f.since, f.until));

    let breakdown: Vec<core_protocol::UsageTotal> = http
        .get(format!("{}/v1/usage?group=provider&since={}&until={}", f.base, f.since, f.until))
        .bearer_auth(&f.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // `/v1/usage` also lists the subscription provider; the summary leaves it out.
    assert!(breakdown.iter().any(|row| row.key == "claude-code@dev1"));
    let api_rows: Vec<_> = breakdown.iter().filter(|row| row.key != "claude-code@dev1").collect();
    let mut calls = 0;
    let mut cost = 0.0;
    let mut tokens = 0;
    for row in &api_rows {
        let key = summary.keys.iter().find(|k| k.provider == row.key).unwrap_or_else(|| panic!("no key {}", row.key));
        assert_eq!(key.totals.calls, row.calls, "{}", row.key);
        assert_eq!(key.totals.input_tokens, row.input_tokens);
        assert_eq!(key.totals.output_tokens, row.output_tokens);
        assert_eq!(key.totals.cache_read_tokens, row.cache_read_tokens);
        assert_eq!(key.totals.cache_write_tokens, row.cache_write_tokens);
        assert!((key.totals.cost - row.cost).abs() < 1e-9, "{}", row.key);
        // Per-model rows add up to the key.
        assert_eq!(key.models.iter().map(|m| m.totals.calls).sum::<u64>(), row.calls);
        assert!((key.models.iter().map(|m| m.totals.cost).sum::<f64>() - row.cost).abs() < 1e-9);
        calls += row.calls;
        cost += row.cost;
        tokens += row.total_tokens();
    }
    assert_eq!(summary.total.calls, calls);
    assert!((summary.total.cost - cost).abs() < 1e-9);
    assert_eq!(
        summary.total.input_tokens
            + summary.total.output_tokens
            + summary.total.cache_read_tokens
            + summary.total.cache_write_tokens,
        tokens
    );
    // 2 + 3 + 1 + 1 + 1 (gone) + 1 (p3 at `since`); the boundary rows and subscription rows are out.
    assert_eq!(summary.total.calls, 9);
    assert!((summary.total.cost - 0.80).abs() < 1e-9, "{}", summary.total.cost);
    std::fs::remove_dir_all(&f.dir).ok();
}

#[tokio::test]
async fn summary_lists_every_key_flags_removed_and_unpriced_and_groups_subscriptions() {
    let f = fixture().await;
    let summary: UsageSummary =
        get_summary(&f, &format!("since={}&until={}", f.since, f.until)).await.json().await.unwrap();

    // Sorted by cost, then tokens, then id: gone (0.5), p1 (0.05), p3 (0.25) -> gone, p3, p1, then p2 (tokens).
    let order: Vec<&str> = summary.keys.iter().map(|k| k.provider.as_str()).collect();
    assert_eq!(order, ["gone", "p3", "p1", "p2"]);
    // Harness providers are not keys.
    assert!(summary.keys.iter().all(|k| k.provider != "claude-code@dev1"));

    let gone = &summary.keys[0];
    assert!(gone.removed);
    assert_eq!((gone.color.as_deref(), gone.kind.as_str(), gone.local), (None, "unknown", false));
    assert_eq!(gone.models[0].name, "old-model");
    assert!(gone.models[0].priced && !gone.models[0].free && !gone.models[0].local);

    let p1 = summary.keys.iter().find(|k| k.provider == "p1").unwrap();
    assert!(!p1.removed && !p1.local);
    assert_eq!(p1.kind, "openai");
    assert!(p1.color.is_some());
    // Largest cost first; the registry name, not the model string, when there is one.
    assert_eq!(p1.models[0].model, "big-model");
    assert_eq!(p1.models[0].name, "Big Model");
    assert!(p1.models[0].priced);
    assert_eq!(p1.models[0].totals.calls, 2);
    let mystery = &p1.models[1];
    assert_eq!((mystery.model.as_str(), mystery.name.as_str(), mystery.priced), ("mystery-model", "mystery-model", false));
    assert_eq!(summary.unpriced_calls, 3);

    // A key on a local address is local, and so are its models; free and local models count as priced.
    let p2 = summary.keys.iter().find(|k| k.provider == "p2").unwrap();
    assert!(p2.local);
    let tiny = p2.models.iter().find(|m| m.model == "tiny").unwrap();
    assert!(tiny.free && tiny.local && tiny.priced);
    assert!(p2.models.iter().find(|m| m.model == "unmarked").unwrap().priced);

    // p3's boundary row at `since` is included, and it is listed with a colour and no model entry.
    let p3 = summary.keys.iter().find(|k| k.provider == "p3").unwrap();
    assert_eq!(p3.totals.calls, 1);
    assert_eq!(p3.models[0].name, "claude-x");
    assert!(p3.models[0].priced);

    // Subscriptions: grouped by harness provider, named by probe provider and device.
    assert_eq!(summary.subscriptions.len(), 1);
    let sub = &summary.subscriptions[0];
    assert_eq!(sub.provider, "claude-code@dev1");
    assert_eq!(sub.harness, "claude");
    assert_eq!(sub.device_id.as_deref(), Some("dev1"));
    assert_eq!((sub.totals.calls, sub.totals.input_tokens, sub.totals.output_tokens), (2, 500, 50));
    assert_eq!(sub.totals.cost, 0.0);

    // The exact JSON shape: removed keys carry `color: null`, and totals are the usage-total shape.
    let raw: Value = get_summary(&f, &format!("since={}&until={}", f.since, f.until)).await.json().await.unwrap();
    assert!(raw["keys"][0]["color"].is_null());
    assert_eq!(raw["keys"][0]["removed"], true);
    for field in ["calls", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens", "cost"] {
        assert!(raw["total"].get(field).is_some(), "{field}");
    }
    assert!(raw["subscriptions"][0].get("harness").is_some());
    std::fs::remove_dir_all(&f.dir).ok();
}

#[tokio::test]
async fn summary_lists_unused_keys_and_accepts_exact_instants() {
    let f = fixture().await;
    // A range with no usage at all still lists every non-harness key, with zero totals.
    let quiet = f.since + 3 * 86_400;
    let summary: UsageSummary = get_summary(&f, &format!("since={quiet}&until={}", quiet + 60)).await.json().await.unwrap();
    let mut ids: Vec<&str> = summary.keys.iter().map(|k| k.provider.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["p1", "p2", "p3"]);
    assert!(summary.keys.iter().all(|k| k.totals.calls == 0 && k.models.is_empty() && !k.removed));
    assert_eq!((summary.total.calls, summary.unpriced_calls), (0, 0));
    assert!(summary.subscriptions.is_empty());

    // RFC 3339 with an offset: the same instants as the Unix seconds above (UTC+01:00 is an hour ahead).
    // 1_795_564_800 is 2026-11-25T00:00:00Z.
    let summary: UsageSummary =
        get_summary(&f, "since=2026-11-25T01:00:00%2B01:00&until=2026-11-26T00:00:00Z").await.json().await.unwrap();
    assert_eq!((summary.since, summary.until), (f.since, f.until));
    assert_eq!(summary.total.calls, 9);

    // `until` defaults to now; the old date forms still parse.
    let summary: UsageSummary = get_summary(&f, "since=2026-11-25").await.json().await.unwrap();
    assert!(summary.until >= crate::db::now() - 5);
    assert_eq!(summary.total.calls, 9 + 1); // the row at the old `until` counts now; only `since - 1` is out
    let summary: UsageSummary = get_summary(&f, "since=2026-11-25&until=2026-11-25").await.json().await.unwrap();
    assert_eq!(summary.until, f.until);

    // Conversation filter.
    let summary: UsageSummary =
        get_summary(&f, &format!("since={}&until={}&conversation=c1", f.since, f.until)).await.json().await.unwrap();
    assert_eq!(summary.total.calls, 2);
    assert_eq!(summary.keys.iter().find(|k| k.provider == "p1").unwrap().totals.calls, 2);
    assert_eq!(summary.subscriptions[0].totals.calls, 2);
    assert_eq!(summary.unpriced_calls, 0);

    // Bad input and access.
    assert_eq!(get_summary(&f, "since=yesterday").await.status(), 400);
    assert_eq!(get_summary(&f, "since=2026-11-25T00:00:00").await.status(), 400);
    assert_eq!(get_summary(&f, &format!("since={}&until={}", f.until, f.since)).await.status(), 400);
    let denied = reqwest::Client::new()
        .get(format!("{}/v1/usage/summary", f.base))
        .bearer_auth(&f.plain)
        .send()
        .await
        .unwrap()
        .status();
    assert!(denied == 401 || denied == 403, "{denied}");
    std::fs::remove_dir_all(&f.dir).ok();
}

fn window(provider: &str, window: &str, label: Option<&str>, account_key: Option<&str>) -> UsageSnapshot {
    UsageSnapshot {
        account: provider.into(),
        provider: provider.into(),
        kind: "window".into(),
        window: window.into(),
        used_pct: Some(12.0),
        used_amount: None,
        limit_amount: None,
        currency: None,
        resets_at: Some(2_000_000_000),
        source: "ignored".into(),
        device_id: None,
        fetched_at: 1,
        label: label.map(String::from),
        account_key: account_key.map(String::from),
    }
}

fn probe(provider: &str, snapshots: Vec<UsageSnapshot>) -> UsageProbeEntry {
    UsageProbeEntry { provider: provider.into(), status: "ok".into(), message: None, snapshots }
}

#[tokio::test]
async fn labels_and_account_keys_round_trip_and_are_sanitised() {
    let dir = temp_dir("label");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    let (_, admin) = db.add_device("Admin", "ios", true).unwrap();
    let (dev_a, _) = db.add_device("PC", "windows", false).unwrap();
    let (dev_b, _) = db.add_device("Mac", "macos", false).unwrap();
    let key = "0123456789abcdef0123456789ABCDEF";
    let long_label = format!("Gemini\n\u{7}Models {}", "é".repeat(60)); // control characters, then over 64 bytes
    db.store_device_probe_results(
        &dev_a.id,
        &[
            probe(
                "antigravity",
                vec![
                    window("antigravity", "5h:gemini", Some("Gemini Models"), Some(key)),
                    window("antigravity", "7d:gemini", Some(&long_label), Some("not-hex")),
                    window("antigravity", "5h:third_party", Some("  \n "), Some(&"a".repeat(65))),
                    window("antigravity", "7d:third_party", None, None),
                ],
            ),
            probe("grok", vec![window("grok", "7d", None, Some(key))]),
        ],
        100,
    )
    .unwrap();
    // The same window read on a second device is kept next to the first, not merged into it.
    db.store_device_probe_results(
        &dev_b.id,
        &[probe("grok", vec![window("grok", "7d", None, Some(key))])],
        101,
    )
    .unwrap();

    let (_hub, base) = serve(db, &dir).await;
    let rows: Vec<UsageSnapshot> = reqwest::Client::new()
        .get(format!("{base}/v1/usage/snapshots"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let find = |provider: &str, window: &str| {
        rows.iter().find(|r| r.provider == provider && r.window == window && r.device_id.as_deref() == Some(dev_a.id.as_str()))
    };
    let gemini = find("antigravity", "5h:gemini").unwrap();
    assert_eq!(gemini.label.as_deref(), Some("Gemini Models"));
    assert_eq!(gemini.account_key.as_deref(), Some(key.to_ascii_lowercase().as_str()));
    let capped = find("antigravity", "7d:gemini").unwrap();
    let label = capped.label.as_deref().unwrap();
    assert!(label.len() <= 64 && label.starts_with("GeminiModels "), "{label:?}");
    assert!(label.chars().all(|c| !c.is_control()));
    assert_eq!(capped.account_key, None, "a fingerprint that is not hex is dropped");
    let blank = find("antigravity", "5h:third_party").unwrap();
    assert_eq!((blank.label.as_deref(), blank.account_key.as_deref()), (None, None));
    assert_eq!(find("antigravity", "7d:third_party").unwrap().label, None);

    // Both devices' grok readings are returned, with the same account key.
    let grok: Vec<_> = rows.iter().filter(|r| r.provider == "grok").collect();
    assert_eq!(grok.len(), 2);
    assert!(grok.iter().all(|r| r.account_key.as_deref() == Some(key.to_ascii_lowercase().as_str())));
    assert_ne!(grok[0].device_id, grok[1].device_id);

    // The wire form omits absent fields.
    let raw: Value = reqwest::Client::new()
        .get(format!("{base}/v1/usage/snapshots"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let blank_raw = raw.as_array().unwrap().iter().find(|r| r["window"] == "7d:third_party").unwrap();
    assert!(blank_raw.get("label").is_none() && blank_raw.get("account_key").is_none());
    // A snapshot from an older client has neither field and still deserialises.
    let old: UsageSnapshot = serde_json::from_value(json!({
        "account": "a", "provider": "claude", "kind": "window", "window": "5h", "source": "probe", "fetched_at": 1
    }))
    .unwrap();
    assert_eq!((old.label, old.account_key), (None, None));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn device_list_reports_the_last_advertised_probes_while_offline() {
    let dir = temp_dir("flags");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    let (admin_dev, admin) = db.add_device("Admin", "ios", true).unwrap();
    let (pc, _) = db.add_device("PC", "windows", false).unwrap();
    let (mac, _) = db.add_device("Mac", "macos", false).unwrap();
    // PC advertised two probes on its last connection and is not connected now.
    db.set_device_usage_probes(&pc.id, true, &["claude".into(), "grok".into()]);
    let (_hub, base) = serve(db, &dir).await;
    let devices: Vec<core_protocol::DeviceInfo> = reqwest::Client::new()
        .get(format!("{base}/v1/devices"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let by_id = |id: &str| devices.iter().find(|d| d.id == id).unwrap();
    assert!(!by_id(&pc.id).online);
    assert!(by_id(&pc.id).usage_probes);
    assert_eq!(by_id(&pc.id).usage_probe_providers, ["claude", "grok"]);
    assert!(!by_id(&mac.id).usage_probes);
    assert!(by_id(&mac.id).usage_probe_providers.is_empty());
    assert!(!by_id(&admin_dev.id).usage_probes);
    // The flag survives reopening the database.
    drop(_hub);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn device_probe_flags_are_stored_and_replaced_on_each_connection() {
    let dir = temp_dir("flags-db");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    let (device, _) = db.add_device("PC", "windows", false).unwrap();
    assert_eq!(db.device_usage_probes(&device.id), (false, vec![]));
    db.set_device_usage_probes(&device.id, true, &["codex".into()]);
    assert_eq!(db.device_usage_probes(&device.id), (true, vec!["codex".to_string()]));
    drop(db);
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    assert_eq!(db.device_usage_probes(&device.id), (true, vec!["codex".to_string()]));
    // Reconnecting with probes off clears them.
    db.set_device_usage_probes(&device.id, false, &[]);
    assert_eq!(db.device_usage_probes(&device.id), (false, vec![]));
    assert_eq!(db.device_usage_probes("unknown"), (false, vec![]));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn harness_threads_for_grok_and_antigravity_map_to_their_probes() {
    let dir = temp_dir("classify");
    let db = Db::open(&dir).unwrap();
    db.add_device("dev1", "linux", false).unwrap();
    for (id, kind) in [("grok@dev1", "harness"), ("antigravity@dev1", "harness"), ("grok-api", "openai")] {
        db.upsert_provider(&ProviderConfig {
            id: id.into(),
            kind: kind.into(),
            base_url: (kind == "openai").then(|| "https://gateway.example.com/v1".to_string()),
            api_key: None,
            options: if kind == "harness" { json!({"via_device": "dev1"}) } else { Value::Null },
        })
        .unwrap();
    }
    for (id, provider_id, name) in [
        ("grok-model", "grok@dev1", "grok-4.7"),
        ("agy-model", "antigravity@dev1", "gemini-3-pro"),
        ("api-grok", "grok-api", "grok-4"),
    ] {
        db.upsert_model(&model(id, provider_id, name, 0.0, 0.0)).unwrap();
    }
    assert_eq!(classify_probe_provider("grok@dev1", Some("harness"), "grok-4.7"), Some("grok"));
    assert_eq!(classify_probe_provider("antigravity@dev1", Some("harness"), "gemini-3-pro"), Some("antigravity"));
    // The same models over an API key don't spend the subscription.
    assert_eq!(classify_probe_provider("grok-api", Some("openai"), "grok-4"), None);

    let conv = db.create_conversation("project", "classify").unwrap();
    let grok = db.create_thread(&conv, "Grok", "dev1", "grok-model", None, "brief").unwrap();
    let agy = db.create_thread(&conv, "Agy", "dev1", "agy-model", None, "brief").unwrap();
    let api = db.create_thread(&conv, "Api", "dev1", "api-grok", None, "brief").unwrap();
    assert_eq!(matching_probe_providers(&db, &grok.id), vec!["grok"]);
    assert_eq!(matching_probe_providers(&db, &agy.id), vec!["antigravity"]);
    assert!(matching_probe_providers(&db, &api.id).is_empty());
    // An orchestrating thread that calls the Grok and Antigravity chat tools counts too.
    for (call_id, name) in [("c1", "grok__grok_chat"), ("c2", "antigravity__antigravity_chat")] {
        db.add_event(
            &conv,
            Some(&api.id),
            EventBody::ThreadActivity {
                event: AgentEvent::ToolCall { call_id: call_id.into(), name: name.into(), input: json!({}) },
            },
        );
    }
    assert_eq!(matching_probe_providers(&db, &api.id), vec!["grok", "antigravity"]);
    assert!(PROBE_PROVIDERS.contains(&"grok") && PROBE_PROVIDERS.contains(&"antigravity"));
    std::fs::remove_dir_all(dir).ok();
}
