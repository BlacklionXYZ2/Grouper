//! Metered usage queries, official provider balance collectors, and device subscription probes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_protocol::tunnel::result_text;
use core_protocol::{
    AgentEvent, EventBody, ProviderConfig, USAGE_PROBE_TOOL, UsageProbeEntry, UsageProbeResponse, UsageSnapshot,
    UsageTotal, mcp_server_of,
};
use rmcp::model::CallToolRequestParams;
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};

use crate::db::{Db, now};
use crate::server::Hub;

const COLLECT_INTERVAL: Duration = Duration::from_secs(60 * 60);
const PROBE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SNAPSHOTS_PER_ENTRY: usize = 32;

pub use core_registry::cost::parse_time;

impl Db {
    /// Totals in the half-open time range `[since, until)`.
    pub fn usage_totals(
        &self,
        since: u64,
        until: u64,
        group: &str,
        conversation: Option<&str>,
    ) -> Result<Vec<UsageTotal>> {
        core_registry::cost::usage_totals(&self.c(), since, until, group, conversation)
    }

    pub fn add_usage_snapshots(&self, snapshots: &[UsageSnapshot]) -> Result<()> {
        let mut c = self.c();
        let tx = c.transaction()?;
        for snapshot in snapshots {
            tx.execute(
                "INSERT INTO usage_snapshots
                 (account, provider, kind, window, used_pct, used_amount, limit_amount, currency,
                  resets_at, source, device_id, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    snapshot.account,
                    snapshot.provider,
                    snapshot.kind,
                    snapshot.window,
                    snapshot.used_pct,
                    snapshot.used_amount,
                    snapshot.limit_amount,
                    snapshot.currency,
                    snapshot.resets_at.map(|v| v as i64),
                    snapshot.source,
                    snapshot.device_id,
                    snapshot.fetched_at as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Store one device's probe results, stamping `device_id` and `source = "probe"`.
    ///
    /// Healthy results clear any prior `kind = "status"` rows for `(device_id, provider)`.
    /// Non-ok results (`not_signed_in`, `expired`, `unavailable`) replace the single
    /// `kind = "status"` row for `(device_id, provider)` while keeping any earlier
    /// `kind = "window"` observations intact (marked by their age).
    pub fn store_device_probe_results(
        &self,
        device_id: &str,
        results: &[UsageProbeEntry],
        fetched_at: u64,
    ) -> Result<usize> {
        let mut c = self.c();
        let tx = c.transaction()?;
        let mut stored = 0usize;
        for entry in results {
            let provider = entry.provider.trim().to_ascii_lowercase();
            if provider.is_empty() || provider.len() > 64 {
                continue;
            }
            tx.execute(
                "DELETE FROM usage_snapshots WHERE device_id = ?1 AND provider = ?2 AND kind = 'status'",
                params![device_id, provider],
            )?;
            if entry.status == "ok" && !entry.snapshots.is_empty() {
                for snap in entry.snapshots.iter().take(MAX_SNAPSHOTS_PER_ENTRY) {
                    let snap_provider = if snap.provider.trim().is_empty() {
                        provider.clone()
                    } else {
                        snap.provider.trim().to_ascii_lowercase()
                    };
                    let account = if snap.account.trim().is_empty() {
                        snap_provider.clone()
                    } else {
                        snap.account.trim().to_string()
                    };
                    let ts = if snap.fetched_at > 0 { snap.fetched_at } else { fetched_at };
                    tx.execute(
                        "INSERT INTO usage_snapshots
                         (account, provider, kind, window, used_pct, used_amount, limit_amount, currency,
                          resets_at, source, device_id, fetched_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            account,
                            snap_provider,
                            snap.kind,
                            snap.window,
                            snap.used_pct,
                            snap.used_amount,
                            snap.limit_amount,
                            snap.currency,
                            snap.resets_at.map(|v| v as i64),
                            "probe",
                            Some(device_id),
                            ts as i64,
                        ],
                    )?;
                    stored += 1;
                }
            } else {
                let status_text = entry
                    .message
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&entry.status);
                tx.execute(
                    "INSERT INTO usage_snapshots
                     (account, provider, kind, window, used_pct, used_amount, limit_amount, currency,
                      resets_at, source, device_id, fetched_at)
                     VALUES (?1, ?2, 'status', ?3, NULL, NULL, NULL, NULL, NULL, 'probe', ?4, ?5)",
                    params![provider, provider, status_text, device_id, fetched_at as i64],
                )?;
                stored += 1;
            }
        }
        tx.commit()?;
        Ok(stored)
    }

    /// The newest observation for each provider/account/device/kind/window/currency tuple.
    pub fn latest_usage_snapshots(&self) -> Result<Vec<UsageSnapshot>> {
        let c = self.c();
        let mut statement = c.prepare(
            "SELECT account, provider, kind, window, used_pct, used_amount, limit_amount, currency,
                    resets_at, source, device_id, fetched_at
             FROM usage_snapshots s
             WHERE id = (
               SELECT id FROM usage_snapshots newest
               WHERE newest.account = s.account AND newest.provider = s.provider
                 AND newest.kind = s.kind AND newest.window = s.window
                 AND newest.currency IS s.currency AND newest.device_id IS s.device_id
               ORDER BY fetched_at DESC, id DESC LIMIT 1
             )
             ORDER BY provider, account, device_id, kind, window, currency",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(UsageSnapshot {
                    account: row.get(0)?,
                    provider: row.get(1)?,
                    kind: row.get(2)?,
                    window: row.get(3)?,
                    used_pct: row.get(4)?,
                    used_amount: row.get(5)?,
                    limit_amount: row.get(6)?,
                    currency: row.get(7)?,
                    resets_at: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                    source: row.get(9)?,
                    device_id: row.get(10)?,
                    fetched_at: row.get::<_, i64>(11)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Collector {
    OpenRouter,
    DeepSeek,
}

fn collector(provider: &ProviderConfig) -> Option<Collector> {
    if provider.kind != "openai" || provider.api_key.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    let host = provider
        .base_url
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))?;
    match host.as_str() {
        "openrouter.ai" => Some(Collector::OpenRouter),
        "api.deepseek.com" => Some(Collector::DeepSeek),
        _ => None,
    }
}

#[derive(Debug, Serialize)]
pub struct RefreshResult {
    pub providers_checked: usize,
    pub devices_probed: usize,
    pub snapshots_stored: usize,
}

/// Probe one online opted-in device over its unlisted internal MCP tool and store snapshots.
pub async fn probe_device(hub: &Hub, device_id: &str, providers: Option<&[&str]>) -> usize {
    let peer = {
        let live = hub.live.lock().await;
        match live.get(device_id) {
            Some(l) if l.usage_probes => l.mcp.clone(),
            _ => None,
        }
    };
    let Some(peer) = peer else { return 0 };
    let args = match providers {
        Some(list) if !list.is_empty() => json!({"providers": list}),
        _ => json!({}),
    };
    let Value::Object(arguments) = args else { return 0 };
    let params = CallToolRequestParams::new(USAGE_PROBE_TOOL.to_string()).with_arguments(arguments);
    let result = match tokio::time::timeout(PROBE_TIMEOUT, peer.call_tool(params)).await {
        Ok(Ok(r)) => r,
        Ok(Err(err)) => {
            tracing::warn!(device = %device_id, "device usage probe failed: {err}");
            return 0;
        }
        Err(_) => {
            tracing::warn!(device = %device_id, "device usage probe timed out");
            return 0;
        }
    };
    let (ok, output) = result_text(&result);
    if !ok {
        tracing::warn!(device = %device_id, "device usage probe returned error: {output}");
        return 0;
    }
    let parsed: UsageProbeResponse = match serde_json::from_str(&output) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(device = %device_id, "unreadable device usage probe response: {err}");
            return 0;
        }
    };
    match hub.db.store_device_probe_results(device_id, &parsed.results, now()) {
        Ok(count) => {
            for entry in &parsed.results {
                tracing::debug!(
                    device = %device_id,
                    provider = %entry.provider,
                    status = %entry.status,
                    snapshots = entry.snapshots.len(),
                    "device usage probe complete"
                );
            }
            count
        }
        Err(err) => {
            tracing::warn!(device = %device_id, "failed to store device usage snapshots: {err:#}");
            0
        }
    }
}

/// Probe all online devices that opted in to subscription usage probes.
pub async fn refresh_device_probes(hub: &Hub) -> (usize, usize) {
    let device_ids: Vec<String> = {
        let live = hub.live.lock().await;
        live.iter()
            .filter(|(_, l)| l.usage_probes && l.mcp.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    };
    let mut stored = 0usize;
    for device_id in &device_ids {
        stored += probe_device(hub, device_id, None).await;
    }
    (device_ids.len(), stored)
}

fn classify_probe_provider(provider_id: &str, kind: Option<&str>, model: &str) -> Option<&'static str> {
    if kind == Some("anthropic") {
        return Some("claude");
    }
    let p = provider_id.to_ascii_lowercase();
    let m = model.to_ascii_lowercase();
    if p.contains("anthropic") || p.contains("claude") || m.contains("claude") {
        return Some("claude");
    }
    if p.contains("codex") || p.contains("openai") || p.contains("chatgpt") || m.contains("codex") || m.starts_with("gpt-") {
        return Some("codex");
    }
    None
}

/// Determine which subscription probe providers (`claude`, `codex`) a device thread used.
pub(crate) fn matching_probe_providers(db: &Db, thread_id: &str) -> Vec<&'static str> {
    let mut providers: Vec<&'static str> = Vec::new();
    let mut push = |p: &'static str| {
        if !providers.contains(&p) {
            providers.push(p);
        }
    };
    let Some(t) = db.thread(thread_id) else { return providers };
    if let Some(m) = db.find_model(&t.model) {
        let kind = db.provider(&m.provider_id).ok().flatten().map(|p| p.kind);
        if let Some(p) = classify_probe_provider(&m.provider_id, kind.as_deref(), &m.model) {
            push(p);
        }
    } else if let Some(p) = classify_probe_provider("", None, &t.model) {
        push(p);
    }
    for event in db.events(&t.conversation_id, 0, Some(thread_id)) {
        if let EventBody::ThreadActivity { event } = event.body {
            match event {
                AgentEvent::ToolCall { name, .. } => match mcp_server_of(&name) {
                    Some("claude-code") => push("claude"),
                    Some("codex") => push("codex"),
                    _ => {}
                },
                AgentEvent::Usage { provider_id, model, .. } => {
                    let kind = db.provider(&provider_id).ok().flatten().map(|p| p.kind);
                    if let Some(p) = classify_probe_provider(&provider_id, kind.as_deref(), &model) {
                        push(p);
                    }
                }
                _ => {}
            }
        }
    }
    providers
}

/// Called right after a device thread finishes: if the thread used Claude or Codex
/// and the device opted in to usage probes, refresh the matching probe(s) right away.
pub(crate) fn after_device_thread(hub: &Arc<Hub>, device_id: &str, thread_id: &str) {
    let providers = matching_probe_providers(&hub.db, thread_id);
    if providers.is_empty() {
        return;
    }
    let hub = Arc::clone(hub);
    let device_id = device_id.to_string();
    tokio::spawn(async move {
        probe_device(&hub, &device_id, Some(&providers)).await;
    });
}

/// Refresh all supported providers and online opted-in device probes. One failure doesn't
/// affect the others, and a failed provider writes no partial observations.
pub async fn refresh(hub: &Hub) -> RefreshResult {
    let providers = match hub.db.providers() {
        Ok(providers) => providers,
        Err(error) => {
            tracing::warn!("could not read providers for usage refresh: {error:#}");
            Vec::new()
        }
    };
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("HTTP client");
    let mut result = RefreshResult { providers_checked: 0, devices_probed: 0, snapshots_stored: 0 };
    for provider in providers {
        let Some(kind) = collector(&provider) else { continue };
        result.providers_checked += 1;
        let fetched_at = now();
        let snapshots = match kind {
            Collector::OpenRouter => collect_openrouter(&client, &provider, fetched_at).await,
            Collector::DeepSeek => collect_deepseek(&client, &provider, fetched_at).await,
        };
        match snapshots.and_then(|rows| {
            hub.db.add_usage_snapshots(&rows)?;
            Ok(rows.len())
        }) {
            Ok(count) => result.snapshots_stored += count,
            Err(error) => tracing::warn!(provider = %provider.id, "usage refresh failed: {error:#}"),
        }
    }
    let (devices_probed, probe_snapshots) = refresh_device_probes(hub).await;
    result.devices_probed = devices_probed;
    result.snapshots_stored += probe_snapshots;
    result
}

pub async fn collector_loop(hub: Arc<Hub>) {
    // Official API balances are collected every hour; opted-in device subscription probes
    // run every 10 minutes (plus on connect and after matching device threads). OpenRouter
    // model prices refresh on the first pass and then once a day.
    let mut hours_until_prices = 0u32;
    let mut official_tick = tokio::time::interval(COLLECT_INTERVAL);
    official_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut probe_tick = tokio::time::interval(PROBE_INTERVAL);
    probe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Avoid duplicating the immediate startup pass on the first probe tick.
    probe_tick.tick().await;
    loop {
        tokio::select! {
            _ = official_tick.tick() => {
                let result = refresh(&hub).await;
                tracing::debug!(
                    providers = result.providers_checked,
                    devices = result.devices_probed,
                    snapshots = result.snapshots_stored,
                    "usage refresh complete"
                );
                if hours_until_prices == 0 {
                    let prices = crate::catalog::refresh_openrouter_prices(&hub).await;
                    tracing::debug!(models = prices.updated, failed = prices.failed, "model price refresh complete");
                    if !prices.failed {
                        hours_until_prices = 24;
                    }
                }
                if hours_until_prices > 0 {
                    hours_until_prices = hours_until_prices.saturating_sub(1);
                }
            }
            _ = probe_tick.tick() => {
                let (devices, snapshots) = refresh_device_probes(&hub).await;
                tracing::debug!(devices, snapshots, "device usage probe tick complete");
            }
        }
    }
}

async fn get_json(client: &reqwest::Client, url: &str, key: &str) -> Result<Value> {
    let response = client.get(url).bearer_auth(key).send().await.context("request failed")?;
    if !response.status().is_success() {
        bail!("provider returned HTTP {}", response.status());
    }
    response.json().await.context("provider returned invalid JSON")
}

async fn collect_openrouter(client: &reqwest::Client, provider: &ProviderConfig, fetched_at: u64) -> Result<Vec<UsageSnapshot>> {
    let key = provider.api_key.as_deref().context("provider has no key")?;
    let key_info = get_json(client, "https://openrouter.ai/api/v1/key", key).await?;
    let credits = get_json(client, "https://openrouter.ai/api/v1/credits", key).await?;
    parse_openrouter(&provider.id, &key_info, &credits, fetched_at)
}

async fn collect_deepseek(client: &reqwest::Client, provider: &ProviderConfig, fetched_at: u64) -> Result<Vec<UsageSnapshot>> {
    let key = provider.api_key.as_deref().context("provider has no key")?;
    let balance = get_json(client, "https://api.deepseek.com/user/balance", key).await?;
    parse_deepseek(&provider.id, &balance, fetched_at)
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
}

fn percentage(used: Option<f64>, limit: Option<f64>) -> Option<f64> {
    let (used, limit) = (used?, limit?);
    (limit > 0.0).then_some(used / limit * 100.0)
}

fn snapshot(provider: &str, kind: &str, window: &str, fetched_at: u64) -> UsageSnapshot {
    UsageSnapshot {
        account: provider.into(),
        provider: provider.into(),
        kind: kind.into(),
        window: window.into(),
        used_pct: None,
        used_amount: None,
        limit_amount: None,
        currency: Some("USD".into()),
        resets_at: None,
        source: "admin_api".into(),
        device_id: None,
        fetched_at,
    }
}

fn parse_openrouter(provider: &str, key_info: &Value, credits: &Value, fetched_at: u64) -> Result<Vec<UsageSnapshot>> {
    let key = key_info.get("data").context("OpenRouter key response has no data")?;
    let credit = credits.get("data").context("OpenRouter credits response has no data")?;
    let used = number(key.get("usage"));
    let limit = number(key.get("limit"));
    let mut spend = snapshot(
        provider,
        "spend",
        key.get("limit_reset").and_then(Value::as_str).unwrap_or("total"),
        fetched_at,
    );
    spend.used_amount = used;
    spend.limit_amount = limit;
    spend.used_pct = percentage(used, limit);

    let total_credits = number(credit.get("total_credits"));
    let total_usage = number(credit.get("total_usage"));
    let mut balance = snapshot(provider, "balance", "total", fetched_at);
    balance.used_amount = total_usage;
    balance.limit_amount = total_credits;
    balance.used_pct = percentage(total_usage, total_credits);
    Ok(vec![spend, balance])
}

fn parse_deepseek(provider: &str, balance: &Value, fetched_at: u64) -> Result<Vec<UsageSnapshot>> {
    let entries = balance
        .get("balance_infos")
        .and_then(Value::as_array)
        .context("DeepSeek balance response has no balance_infos")?;
    let mut snapshots = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut row = snapshot(provider, "balance", "current", fetched_at);
        row.currency = entry.get("currency").and_then(Value::as_str).map(String::from);
        row.used_amount = number(entry.get("total_balance"));
        snapshots.push(row);
    }
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::random_hex;
    use serde_json::json;

    #[test]
    fn parses_recorded_openrouter_json() {
        let key = json!({"data": {
            "label": "sk-or-v1-test", "usage": 12.5, "limit": 50.0,
            "limit_remaining": 37.5, "limit_reset": "monthly"
        }});
        let credits = json!({"data": {"total_credits": 100.0, "total_usage": 28.25}});
        let rows = parse_openrouter("openrouter", &key, &credits, 123).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, "spend");
        assert_eq!(rows[0].window, "monthly");
        assert_eq!(rows[0].used_pct, Some(25.0));
        assert_eq!(rows[1].limit_amount, Some(100.0));
        assert_eq!(rows[1].used_amount, Some(28.25));
    }

    #[test]
    fn parses_recorded_deepseek_json() {
        let body = json!({
            "is_available": true,
            "balance_infos": [
                {"currency": "CNY", "total_balance": "110.00", "granted_balance": "10.00", "topped_up_balance": "100.00"},
                {"currency": "USD", "total_balance": "2.50", "granted_balance": "0.00", "topped_up_balance": "2.50"}
            ]
        });
        let rows = parse_deepseek("deepseek", &body, 456).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].currency.as_deref(), Some("CNY"));
        assert_eq!(rows[0].used_amount, Some(110.0));
        assert_eq!(rows[1].used_amount, Some(2.5));
    }

    #[test]
    fn only_matching_keyed_providers_get_collectors() {
        let provider = |base_url: Option<&str>, key: Option<&str>| ProviderConfig {
            id: "p".into(),
            kind: "openai".into(),
            base_url: base_url.map(String::from),
            api_key: key.map(String::from),
            options: Value::Null,
        };
        assert_eq!(collector(&provider(Some("https://openrouter.ai/api/v1"), Some("k"))), Some(Collector::OpenRouter));
        assert_eq!(collector(&provider(Some("https://api.deepseek.com"), Some("k"))), Some(Collector::DeepSeek));
        assert_eq!(collector(&provider(Some("https://api.openai.com/v1"), Some("k"))), None);
        assert_eq!(collector(&provider(Some("https://openrouter.ai/api/v1"), None)), None);
    }

    #[tokio::test]
    async fn refresh_skips_when_no_provider_matches() {
        let dir = std::env::temp_dir().join(format!("grouper-usage-skip-test-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "other".into(),
            kind: "openai".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            api_key: Some("not-sent".into()),
            options: Value::Null,
        })
        .unwrap();
        let hub = Hub::new(db, dir.clone());
        let result = refresh(&hub).await;
        assert_eq!(result.providers_checked, 0);
        assert_eq!(result.snapshots_stored, 0);
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn aggregates_orchestrator_and_thread_usage_from_the_database() {
        let dir = std::env::temp_dir().join(format!("grouper-usage-test-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: Some("k".into()),
            options: Value::Null,
        })
        .unwrap();
        db.upsert_model(&core_protocol::ModelEntry {
            id: "m".into(),
            provider_id: "p".into(),
            model: "m".into(),
            input_price: 1.0,
            output_price: 2.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: vec![],
            local: false,
            modalities: vec![],
        })
        .unwrap();
        let conversation = db.create_conversation("project", "test").unwrap();
        db.record_usage(Some(&conversation), None, "p", "m", 10, 3, 4, 0);
        db.record_usage(Some(&conversation), Some("t1"), "p", "m", 20, 7, 0, 2);

        let rows = db.usage_totals(0, i64::MAX as u64, "conversation", Some(&conversation)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].calls, 2);
        assert_eq!(rows[0].input_tokens, 30);
        assert_eq!(rows[0].output_tokens, 10);
        assert_eq!(rows[0].cache_read_tokens, 4);
        assert_eq!(rows[0].cache_write_tokens, 2);
        let by_thread = db.usage_totals(0, i64::MAX as u64, "thread", Some(&conversation)).unwrap();
        assert_eq!(by_thread.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(), vec!["(orchestrator)", "t1"]);
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn stores_and_deduplicates_device_probe_snapshots_and_status_rows() {
        let dir = std::env::temp_dir().join(format!("grouper-probe-store-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let (dev_a, _) = db.add_device("PC", "windows", false).unwrap();
        let (dev_b, _) = db.add_device("Mac", "macos", false).unwrap();

        let snap = |provider: &str, account: &str, window: &str, pct: f64, fetched_at: u64| UsageSnapshot {
            account: account.into(),
            provider: provider.into(),
            kind: "window".into(),
            window: window.into(),
            used_pct: Some(pct),
            used_amount: None,
            limit_amount: None,
            currency: None,
            resets_at: Some(fetched_at + 3600),
            source: "spoofed".into(), // hub must overwrite to "probe"
            device_id: Some("wrong".into()), // hub must overwrite to actual device_id
            fetched_at,
        };

        // Store healthy probe on dev_a and an unavailable status on dev_b.
        let stored_a = db
            .store_device_probe_results(
                &dev_a.id,
                &[UsageProbeEntry {
                    provider: "claude".into(),
                    status: "ok".into(),
                    message: None,
                    snapshots: vec![
                        snap("claude", "claude-max", "5h", 42.0, 1000),
                        snap("claude", "claude-max", "7d", 15.0, 1000),
                    ],
                }],
                1000,
            )
            .unwrap();
        assert_eq!(stored_a, 2);

        db.store_device_probe_results(
            &dev_b.id,
            &[
                UsageProbeEntry {
                    provider: "claude".into(),
                    status: "ok".into(),
                    message: None,
                    snapshots: vec![snap("claude", "claude-max", "5h", 10.0, 1005)],
                },
                UsageProbeEntry {
                    provider: "codex".into(),
                    status: "not_signed_in".into(),
                    message: Some("not signed in".into()),
                    snapshots: vec![],
                },
            ],
            1005,
        )
        .unwrap();

        let latest = db.latest_usage_snapshots().unwrap();
        assert_eq!(latest.len(), 4);
        // Both devices keep their own 5h snapshot independently.
        let a_5h = latest.iter().find(|s| s.device_id.as_deref() == Some(&dev_a.id) && s.window == "5h").unwrap();
        let b_5h = latest.iter().find(|s| s.device_id.as_deref() == Some(&dev_b.id) && s.window == "5h").unwrap();
        assert_eq!(a_5h.used_pct, Some(42.0));
        assert_eq!(a_5h.source, "probe");
        assert_eq!(b_5h.used_pct, Some(10.0));
        let b_codex = latest.iter().find(|s| s.device_id.as_deref() == Some(&dev_b.id) && s.provider == "codex").unwrap();
        assert_eq!(b_codex.kind, "status");
        assert_eq!(b_codex.window, "not signed in");

        // Later error on dev_a adds a status row without erasing its last window snapshots.
        db.store_device_probe_results(
            &dev_a.id,
            &[UsageProbeEntry {
                provider: "claude".into(),
                status: "unavailable".into(),
                message: Some("unavailable: HTTP 429".into()),
                snapshots: vec![],
            }],
            1600,
        )
        .unwrap();
        let latest = db.latest_usage_snapshots().unwrap();
        assert!(latest.iter().any(|s| s.device_id.as_deref() == Some(&dev_a.id) && s.kind == "window" && s.window == "5h"));
        assert!(latest.iter().any(|s| s.device_id.as_deref() == Some(&dev_a.id) && s.kind == "status" && s.window == "unavailable: HTTP 429"));

        // Recovery on dev_a clears the status row and updates the 5h window.
        db.store_device_probe_results(
            &dev_a.id,
            &[UsageProbeEntry {
                provider: "claude".into(),
                status: "ok".into(),
                message: None,
                snapshots: vec![snap("claude", "claude-max", "5h", 55.0, 2200)],
            }],
            2200,
        )
        .unwrap();
        let latest = db.latest_usage_snapshots().unwrap();
        assert!(!latest.iter().any(|s| s.device_id.as_deref() == Some(&dev_a.id) && s.kind == "status"));
        let a_5h = latest.iter().find(|s| s.device_id.as_deref() == Some(&dev_a.id) && s.window == "5h").unwrap();
        assert_eq!(a_5h.used_pct, Some(55.0));
        assert_eq!(a_5h.fetched_at, 2200);

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn matches_probe_providers_from_thread_model_and_agent_tools() {
        let dir = std::env::temp_dir().join(format!("grouper-probe-match-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "ant".into(),
            kind: "anthropic".into(),
            base_url: None,
            api_key: Some("k".into()),
            options: Value::Null,
        })
        .unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "local".into(),
            kind: "openai".into(),
            base_url: Some("http://127.0.0.1:11434/v1".into()),
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
        db.upsert_model(&core_protocol::ModelEntry {
            id: "opus".into(),
            provider_id: "ant".into(),
            model: "claude-opus-4-6".into(),
            input_price: 0.0,
            output_price: 0.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: false,
            roles: vec![],
            local: false,
            modalities: vec![],
        })
        .unwrap();
        db.upsert_model(&core_protocol::ModelEntry {
            id: "llama".into(),
            provider_id: "local".into(),
            model: "llama3".into(),
            input_price: 0.0,
            output_price: 0.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: true,
            roles: vec![],
            local: true,
            modalities: vec![],
        })
        .unwrap();
        let conv = db.create_conversation("project", "match").unwrap();
        let t1 = db.create_thread(&conv, "Claude thread", "d1", "opus", None, "brief").unwrap();
        assert_eq!(matching_probe_providers(&db, &t1.id), vec!["claude"]);

        let t2 = db.create_thread(&conv, "Local calling codex", "d1", "llama", None, "brief").unwrap();
        assert!(matching_probe_providers(&db, &t2.id).is_empty());
        db.add_event(
            &conv,
            Some(&t2.id),
            EventBody::ThreadActivity {
                event: AgentEvent::ToolCall {
                    call_id: "c1".into(),
                    name: "codex__codex_chat".into(),
                    input: json!({}),
                },
            },
        );
        db.add_event(
            &conv,
            Some(&t2.id),
            EventBody::ThreadActivity {
                event: AgentEvent::ToolCall {
                    call_id: "c2".into(),
                    name: "claude-code__claude_code_chat".into(),
                    input: json!({}),
                },
            },
        );
        assert_eq!(matching_probe_providers(&db, &t2.id), vec!["codex", "claude"]);

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
