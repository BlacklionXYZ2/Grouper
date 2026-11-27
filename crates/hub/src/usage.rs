//! Metered usage queries, official provider balance collectors, and device subscription probes.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use core_protocol::tunnel::result_text;
use core_protocol::{
    AgentEvent, EventBody, ProviderConfig, USAGE_PROBE_TOOL, UsageProbeEntry, UsageProbeResponse, UsageSnapshot,
    UsageTotal, mcp_server_of,
};
use providers::{OnDelta, Provider, ProviderError, Request, Response};
use rmcp::model::CallToolRequestParams;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::{Db, now};
use crate::server::Hub;

const COLLECT_INTERVAL: Duration = Duration::from_secs(60 * 60);
const PROBE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROBE_RESULTS: usize = 8;
const MAX_SNAPSHOTS_PER_ENTRY: usize = 32;
const MAX_PROBE_STRING_BYTES: usize = 256;
const PROBE_PROVIDERS: &[&str] = &["claude", "codex"];
const PROBE_KINDS: &[&str] = &["window", "spend", "balance", "status"];

/// OpenRouter's account-wide daily free-model allowance, as a usage snapshot.
pub const FREE_LIMIT_KIND: &str = "requests";
pub const FREE_LIMIT_WINDOW: &str = "free requests/day";
pub const FREE_LIMIT_SOURCE: &str = "OpenRouter rate-limit response";
/// Only this much of an error body (untrusted) is looked at.
const MAX_LIMIT_TEXT_BYTES: usize = 16 * 1024;
/// Header objects longer than this are not searched further.
const MAX_LIMIT_HEADERS: usize = 64;
/// A request limit above this isn't a plausible daily allowance; it is dropped.
const MAX_FREE_LIMIT: u64 = 10_000_000;
/// The same reading again within this many seconds isn't stored twice.
const FREE_LIMIT_DEDUPE_SECS: u64 = 60;

pub use core_registry::cost::parse_time;

impl Db {
    /// Totals in the half-open time range `[since, until)`, optionally for one
    /// conversation and/or one provider id.
    pub fn usage_totals(
        &self,
        since: u64,
        until: u64,
        group: &str,
        conversation: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Vec<UsageTotal>> {
        core_registry::cost::usage_totals(&self.c(), since, until, group, conversation, provider)
    }

    /// Calls to `:free` models recorded in `[since, until)`, per provider id. The usage
    /// table holds calls that returned usage, so failed calls aren't counted.
    pub fn free_model_calls(&self, since: u64, until: u64) -> Result<HashMap<String, u64>> {
        let c = self.c();
        let mut statement = c.prepare(
            "SELECT provider_id, COUNT(*) FROM usage
             WHERE ts >= ?1 AND ts < ?2 AND model LIKE '%:free'
             GROUP BY provider_id",
        )?;
        Ok(statement
            .query_map(params![since as i64, until as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// Store what an OpenRouter 429 said about the daily free-model allowance. The same
    /// reading again within a minute is skipped, so a burst of failures stays one row.
    /// Returns whether a row was stored.
    pub fn record_free_limit(&self, provider: &str, limit: &FreeLimit, fetched_at: u64) -> Result<bool> {
        let last: Option<(Option<f64>, Option<i64>, i64)> = self
            .c()
            .query_row(
                "SELECT limit_amount, resets_at, fetched_at FROM usage_snapshots
                 WHERE provider = ?1 AND account = ?1 AND kind = ?2 AND window = ?3 AND device_id IS NULL
                 ORDER BY fetched_at DESC, id DESC LIMIT 1",
                params![provider, FREE_LIMIT_KIND, FREE_LIMIT_WINDOW],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((last_limit, last_reset, last_at)) = last
            && last_limit == limit.limit.map(|l| l as f64)
            && last_reset == limit.resets_at.map(|r| r as i64)
            && (last_at.max(0) as u64).saturating_add(FREE_LIMIT_DEDUPE_SECS) > fetched_at
        {
            return Ok(false);
        }
        self.add_usage_snapshots(&[free_limit_snapshot(provider, limit, fetched_at)])?;
        Ok(true)
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
        for entry in results.iter().take(MAX_PROBE_RESULTS) {
            let provider = entry.provider.trim().to_ascii_lowercase();
            if !PROBE_PROVIDERS.contains(&provider.as_str()) {
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
                    if !PROBE_PROVIDERS.contains(&snap_provider.as_str()) {
                        continue;
                    }
                    let kind = snap.kind.trim().to_ascii_lowercase();
                    if !PROBE_KINDS.contains(&kind.as_str()) {
                        continue;
                    }
                    let account = cap_probe_string(if snap.account.trim().is_empty() {
                        snap_provider.clone()
                    } else {
                        snap.account.trim().to_string()
                    });
                    let window = cap_probe_string(snap.window.trim().to_string());
                    let currency = snap.currency.as_deref().map(|s| cap_probe_string(s.trim().to_string()));
                    tx.execute(
                        "INSERT INTO usage_snapshots
                         (account, provider, kind, window, used_pct, used_amount, limit_amount, currency,
                          resets_at, source, device_id, fetched_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            account,
                            snap_provider,
                            kind,
                            window,
                            snap.used_pct,
                            snap.used_amount,
                            snap.limit_amount,
                            currency,
                            snap.resets_at.map(|v| v as i64),
                            "probe",
                            Some(device_id),
                            fetched_at as i64,
                        ],
                    )?;
                    stored += 1;
                }
            } else {
                let status_text = cap_probe_string(
                    entry
                        .message
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&entry.status)
                        .to_string(),
                );
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
                    label: None,
                    account_key: None,
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

fn cap_probe_string(mut value: String) -> String {
    if value.len() <= MAX_PROBE_STRING_BYTES {
        return value;
    }
    let mut end = MAX_PROBE_STRING_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
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
    // Includes the provider called "openrouter" with no base_url, which `providers::openai`
    // points at OpenRouter.
    if is_openrouter(provider) {
        return Some(Collector::OpenRouter);
    }
    let host = provider
        .base_url
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))?;
    (host == "api.deepseek.com").then_some(Collector::DeepSeek)
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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client");
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
    // model prices and API provider model listings refresh on the first pass and then once a day.
    let mut hours_until_prices = 0u32;
    let mut hours_until_sync = 0u32;
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
                if hours_until_sync == 0 {
                    let synced = crate::sync::sync_all_providers(&hub).await;
                    let failed = synced.iter().filter(|r| r.error.is_some()).count();
                    tracing::debug!(providers = synced.len(), failed, "daily provider model sync complete");
                    hours_until_sync = 24;
                }
                if hours_until_sync > 0 {
                    hours_until_sync = hours_until_sync.saturating_sub(1);
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
        label: None,
        account_key: None,
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

// ---- OpenRouter's daily free-model allowance
//
// The allowance is account-wide and its size depends on the account, so Grouper
// never assumes a number. Two real sources exist:
// - When it runs out, OpenRouter answers 429 with `error.metadata.limit_source =
//   "openrouter_free_tier_daily"` and the limit, remaining count and reset time
//   in `error.metadata.headers`. A hook on hub-side OpenRouter providers (and on
//   device thread failures) records that as a snapshot.
// - Grouper's own count of calls it sent to `:free` models today (UTC), from the
//   usage table. That is not the allowance and is labelled as Grouper's count.
//
// `/api/v1/key` isn't used for this: of its fields only `is_free_tier` (whether the
// account ever bought credits) touches it, and turning that into a number would
// mean hard-coding OpenRouter's current policy. Successful completions aren't read
// either: the providers crate doesn't hand response headers back to the hub.

/// What an OpenRouter daily free-model 429 said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreeLimit {
    /// `X-RateLimit-Limit`, when present and plausible.
    pub limit: Option<u64>,
    /// `X-RateLimit-Remaining`; 0 when absent (the 429 itself says it ran out).
    pub remaining: u64,
    /// `X-RateLimit-Reset` in Unix seconds, when present and within a week of now.
    pub resets_at: Option<u64>,
}

/// Whether a provider config talks to OpenRouter.
pub(crate) fn is_openrouter(provider: &ProviderConfig) -> bool {
    if provider.kind != "openai" {
        return false;
    }
    match provider.base_url.as_deref() {
        Some(url) => reqwest::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.eq_ignore_ascii_case("openrouter.ai")))
            .unwrap_or(false),
        // `providers::openai` defaults the provider called "openrouter" to OpenRouter.
        None => provider.id == "openrouter",
    }
}

fn cap_str(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A header value as a whole number: JSON numbers or short numeric strings only.
fn header_number(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) if s.len() <= 32 => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parse OpenRouter's daily free-model 429 from an error body, or from text with that
/// body embedded (a device reports "<provider> returned HTTP 429: <body>"). The text is
/// untrusted: only its first 16 KiB are read and every field is checked.
pub fn parse_free_limit(text: &str, now: u64) -> Option<FreeLimit> {
    let text = cap_str(text, MAX_LIMIT_TEXT_BYTES);
    if !providers::account_wide_limit(text) {
        return None;
    }
    let start = text.find('{')?;
    let value: Value = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>().next()?.ok()?;
    let error = value.get("error")?;
    let metadata = error.get("metadata");
    let daily = metadata.and_then(|m| m.get("limit_source")).and_then(Value::as_str) == Some("openrouter_free_tier_daily")
        || error.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("free-models-per-day"));
    if !daily {
        return None;
    }
    let headers = metadata.and_then(|m| m.get("headers")).and_then(Value::as_object);
    let header = |name: &str| {
        headers?
            .iter()
            .take(MAX_LIMIT_HEADERS)
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| header_number(value))
    };
    let limit = header("x-ratelimit-limit").filter(|l| (1..=MAX_FREE_LIMIT).contains(l));
    let remaining = header("x-ratelimit-remaining").unwrap_or(0);
    let remaining = limit.map_or(remaining, |l| remaining.min(l));
    // OpenRouter sends milliseconds; accept seconds too. Anything not within a day
    // before or a week after now is not a plausible daily reset and is dropped.
    let resets_at = header("x-ratelimit-reset")
        .map(|r| if r >= 100_000_000_000 { r / 1000 } else { r })
        .filter(|r| r.saturating_add(86_400) > now && *r <= now.saturating_add(7 * 86_400));
    Some(FreeLimit { limit, remaining, resets_at })
}

/// The free-model limit in a provider error, if that is what the error is.
pub fn free_limit_from_error(error: &ProviderError, now: u64) -> Option<FreeLimit> {
    match error {
        ProviderError::Http { status: 429, body, .. } => parse_free_limit(body, now),
        // A device-hosted provider's error comes back as text.
        ProviderError::Unavailable(message) if message.contains("HTTP 429") => parse_free_limit(message, now),
        _ => None,
    }
}

pub fn free_limit_snapshot(provider: &str, limit: &FreeLimit, fetched_at: u64) -> UsageSnapshot {
    let used = limit.limit.map(|l| l.saturating_sub(limit.remaining) as f64);
    let used_pct = match limit.limit {
        Some(l) => percentage(used, Some(l as f64)),
        None if limit.remaining == 0 => Some(100.0),
        None => None,
    };
    UsageSnapshot {
        account: provider.into(),
        provider: provider.into(),
        kind: FREE_LIMIT_KIND.into(),
        window: FREE_LIMIT_WINDOW.into(),
        label: None,
        account_key: None,
        used_pct,
        used_amount: used,
        limit_amount: limit.limit.map(|l| l as f64),
        currency: None,
        resets_at: limit.resets_at,
        source: FREE_LIMIT_SOURCE.into(),
        device_id: None,
        fetched_at,
    }
}

fn store_free_limit(db: &Db, provider_id: &str, limit: &FreeLimit, now: u64) -> bool {
    match db.record_free_limit(provider_id, limit, now) {
        Ok(stored) => {
            if stored {
                tracing::info!(
                    provider = %provider_id,
                    limit = ?limit.limit,
                    resets_at = ?limit.resets_at,
                    "OpenRouter daily free-model limit reached"
                );
            }
            stored
        }
        Err(error) => {
            tracing::warn!(provider = %provider_id, "could not record the free-model limit: {error:#}");
            false
        }
    }
}

/// Record the snapshot when `error` is OpenRouter's daily free-model 429.
pub fn note_provider_error(db: &Db, provider_id: &str, error: &ProviderError) -> bool {
    let now = crate::db::now();
    match free_limit_from_error(error, now) {
        Some(limit) => store_free_limit(db, provider_id, &limit, now),
        None => false,
    }
}

/// A device thread failed. Its summary carries the provider error as text ("Error:
/// <provider> returned HTTP 429: <body>"); when that is OpenRouter's daily free-model
/// limit, record it for the thread's provider.
pub(crate) fn note_thread_failure(hub: &Hub, thread_id: &str, summary: &str) -> bool {
    if !summary.contains("HTTP 429") || !providers::account_wide_limit(cap_str(summary, MAX_LIMIT_TEXT_BYTES)) {
        return false;
    }
    let Some(thread) = hub.db.thread(thread_id) else { return false };
    let Some(model) = hub.db.find_model(&thread.model) else { return false };
    let Ok(Some(config)) = hub.db.provider(&model.provider_id) else { return false };
    if !is_openrouter(&config) {
        return false;
    }
    let now = crate::db::now();
    match parse_free_limit(summary, now) {
        Some(limit) => store_free_limit(&hub.db, &config.id, &limit, now),
        None => false,
    }
}

/// Wraps an OpenRouter provider used on the hub so its daily free-model 429 is recorded.
struct WatchFreeLimit {
    inner: Arc<dyn Provider>,
    hub: Weak<Hub>,
    provider_id: String,
}

impl WatchFreeLimit {
    fn check(&self, result: Result<Response, ProviderError>) -> Result<Response, ProviderError> {
        if let Err(error) = &result
            && let Some(hub) = self.hub.upgrade()
        {
            note_provider_error(&hub.db, &self.provider_id, error);
        }
        result
    }
}

#[async_trait]
impl Provider for WatchFreeLimit {
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        self.check(self.inner.complete(req).await)
    }

    async fn complete_stream(&self, req: &Request, on_delta: OnDelta<'_>) -> Result<Response, ProviderError> {
        self.check(self.inner.complete_stream(req, on_delta).await)
    }
}

/// `provider`, watched for OpenRouter's daily free-model 429 when `config` is OpenRouter.
pub(crate) fn watch_free_limit(hub: &Arc<Hub>, config: &ProviderConfig, provider: Arc<dyn Provider>) -> Arc<dyn Provider> {
    if !is_openrouter(config) {
        return provider;
    }
    Arc::new(WatchFreeLimit { inner: provider, hub: Arc::downgrade(hub), provider_id: config.id.clone() })
}

/// Grouper's own count of calls to OpenRouter `:free` models in one UTC day.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FreeRequestsToday {
    /// The UTC day, `[since, until)` in Unix seconds.
    pub since: u64,
    pub until: u64,
    pub providers: Vec<FreeRequestCount>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FreeRequestCount {
    pub provider: String,
    /// Calls Grouper sent to `:free` models through this provider that returned usage.
    pub sent: u64,
}

/// Calls Grouper sent to `:free` models today (UTC) on each OpenRouter provider.
pub fn free_requests_today(db: &Db, now: u64) -> Result<FreeRequestsToday> {
    let since = now - now % 86_400;
    let until = since + 86_400;
    let counts = db.free_model_calls(since, until)?;
    let providers = db
        .providers()?
        .into_iter()
        .filter(is_openrouter)
        .map(|p| FreeRequestCount { sent: counts.get(&p.id).copied().unwrap_or(0), provider: p.id })
        .collect();
    Ok(FreeRequestsToday { since, until, providers })
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

    /// OpenRouter's 429 when the daily free-model allowance runs out, as recorded.
    const RECORDED_FREE_429: &str = r#"{"error":{"message":"Rate limit exceeded: free-models-per-day. Add 10 credits to unlock 1000 free model requests per day","code":429,"metadata":{"headers":{"X-RateLimit-Limit":"50","X-RateLimit-Remaining":"0","X-RateLimit-Reset":"1795651200000"},"limit_source":"openrouter_free_tier_daily","provider_name":null}},"user_id":"user_test"}"#;
    /// A moment on the day before that reset (reset 2026-11-26 00:00 UTC; this is 2026-11-25 10:00 UTC).
    const BEFORE_RESET: u64 = 1_795_651_200 - 14 * 3600;

    #[test]
    fn parses_the_recorded_free_model_429() {
        let limit = parse_free_limit(RECORDED_FREE_429, BEFORE_RESET).unwrap();
        assert_eq!(limit, FreeLimit { limit: Some(50), remaining: 0, resets_at: Some(1_795_651_200) });

        // The same body inside a device's error text, with lower-case numeric headers.
        let text = r#"Error: openrouter returned HTTP 429: {"error":{"message":"Rate limit exceeded: free-models-per-day","code":429,"metadata":{"headers":{"x-ratelimit-limit":1000,"x-ratelimit-remaining":3,"x-ratelimit-reset":1795651200},"limit_source":"openrouter_free_tier_daily"}}} and more"#;
        assert_eq!(
            parse_free_limit(text, BEFORE_RESET),
            Some(FreeLimit { limit: Some(1000), remaining: 3, resets_at: Some(1_795_651_200) })
        );

        let snap = free_limit_snapshot("openrouter", &limit, BEFORE_RESET);
        assert_eq!(snap.kind, FREE_LIMIT_KIND);
        assert_eq!(snap.window, "free requests/day");
        assert_eq!(snap.source, "OpenRouter rate-limit response");
        assert_eq!(snap.limit_amount, Some(50.0));
        assert_eq!(snap.used_amount, Some(50.0));
        assert_eq!(snap.used_pct, Some(100.0));
        assert_eq!(snap.resets_at, Some(1_795_651_200));
        assert_eq!(snap.currency, None);
        assert_eq!(snap.fetched_at, BEFORE_RESET);
    }

    #[test]
    fn free_model_429_parsing_is_defensive() {
        // Upstream per-model 429s aren't the daily allowance.
        assert_eq!(parse_free_limit(r#"{"error":{"message":"Provider returned error","code":429}}"#, BEFORE_RESET), None);
        // The marker without a JSON body, or with broken JSON.
        assert_eq!(parse_free_limit("free-models-per-day", BEFORE_RESET), None);
        assert_eq!(parse_free_limit(r#"{"error":{"message":"free-models-per-day""#, BEFORE_RESET), None);
        // No headers: still the daily limit, with nothing invented.
        assert_eq!(
            parse_free_limit(r#"{"error":{"message":"Rate limit exceeded: free-models-per-day","code":429}}"#, BEFORE_RESET),
            Some(FreeLimit { limit: None, remaining: 0, resets_at: None })
        );
        let no_limit = free_limit_snapshot("or", &FreeLimit { limit: None, remaining: 0, resets_at: None }, 1);
        assert_eq!((no_limit.limit_amount, no_limit.used_amount, no_limit.used_pct), (None, None, Some(100.0)));
        // Implausible values are dropped, not trusted.
        let odd = r#"{"error":{"code":429,"metadata":{"limit_source":"openrouter_free_tier_daily","headers":{
            "X-RateLimit-Limit":"99999999999","X-RateLimit-Remaining":"-4","X-RateLimit-Reset":"4102444800000"}}}}"#;
        assert_eq!(parse_free_limit(odd, BEFORE_RESET), Some(FreeLimit { limit: None, remaining: 0, resets_at: None }));
        let past = RECORDED_FREE_429.replace("1795651200000", "1700000000000");
        assert_eq!(parse_free_limit(&past, BEFORE_RESET).unwrap().resets_at, None);
        let words = RECORDED_FREE_429.replace("\"50\"", "\"fifty\"");
        assert_eq!(parse_free_limit(&words, BEFORE_RESET).unwrap().limit, None);
        // Remaining can't exceed the limit.
        let over = RECORDED_FREE_429.replace("\"X-RateLimit-Remaining\":\"0\"", "\"X-RateLimit-Remaining\":\"80\"");
        assert_eq!(parse_free_limit(&over, BEFORE_RESET).unwrap().remaining, 50);
        // A marker past the 16 KiB that is read doesn't count.
        let padded = format!("{}{}", "x".repeat(MAX_LIMIT_TEXT_BYTES), RECORDED_FREE_429);
        assert_eq!(parse_free_limit(&padded, BEFORE_RESET), None);

        // Only 429s (or a device's "HTTP 429" text) are looked at.
        let http = |status| ProviderError::Http { provider: "or".into(), status, body: RECORDED_FREE_429.into() };
        assert!(free_limit_from_error(&http(429), BEFORE_RESET).is_some());
        assert!(free_limit_from_error(&http(402), BEFORE_RESET).is_none());
        let relayed = ProviderError::Unavailable(format!("openrouter returned HTTP 429: {RECORDED_FREE_429}"));
        assert!(free_limit_from_error(&relayed, BEFORE_RESET).is_some());
        assert!(free_limit_from_error(&ProviderError::Unavailable(RECORDED_FREE_429.into()), BEFORE_RESET).is_none());
    }

    fn openrouter(id: &str) -> ProviderConfig {
        ProviderConfig {
            id: id.into(),
            kind: "openai".into(),
            base_url: Some("https://openrouter.ai/api/v1".into()),
            api_key: None,
            options: Value::Null,
        }
    }

    /// Fails every call with the given error body as an HTTP 429.
    struct Answers429(String);

    #[async_trait]
    impl Provider for Answers429 {
        async fn complete(&self, _req: &Request) -> Result<Response, ProviderError> {
            Err(ProviderError::Http { provider: "or".into(), status: 429, body: self.0.clone() })
        }
    }

    fn request() -> Request {
        Request { model: "x/y:free".into(), system: String::new(), messages: vec![], tools: vec![], max_tokens: 1 }
    }

    #[tokio::test]
    async fn a_free_model_429_on_an_openrouter_provider_records_a_snapshot() {
        let dir = std::env::temp_dir().join(format!("grouper-free-limit-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&openrouter("or")).unwrap();
        let hub = Hub::new(db, dir.clone());
        let config = hub.db.provider("or").unwrap().unwrap();
        let reset_ms = (crate::db::now() + 3_600) * 1000;
        let body = RECORDED_FREE_429.replace("1795651200000", &reset_ms.to_string());

        let watched = watch_free_limit(&hub, &config, Arc::new(Answers429(body.clone())));
        let error = watched.complete(&request()).await.unwrap_err();
        assert!(matches!(error, ProviderError::Http { status: 429, .. }), "the error still reaches the caller");
        // A second failure straight after is the same reading: still one row.
        let _ = watched.complete_stream(&request(), &|_| {}).await;
        let rows: Vec<_> = hub.db.latest_usage_snapshots().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!((row.provider.as_str(), row.kind.as_str(), row.window.as_str()), ("or", "requests", "free requests/day"));
        assert_eq!((row.limit_amount, row.used_amount), (Some(50.0), Some(50.0)));
        assert_eq!(row.resets_at, Some(reset_ms / 1000));
        assert_eq!(row.source, "OpenRouter rate-limit response");
        assert!(row.fetched_at > 0 && row.device_id.is_none());
        let count = |db: &Db| -> i64 {
            db.c().query_row("SELECT COUNT(*) FROM usage_snapshots", [], |r| r.get(0)).unwrap()
        };
        assert_eq!(count(&hub.db), 1);

        // Other errors, and providers that aren't OpenRouter, record nothing.
        let upstream = watch_free_limit(&hub, &config, Arc::new(Answers429(r#"{"error":{"code":429}}"#.into())));
        let _ = upstream.complete(&request()).await;
        let mut other = openrouter("elsewhere");
        other.base_url = Some("https://api.example.com/v1".into());
        let unwatched = watch_free_limit(&hub, &other, Arc::new(Answers429(body)));
        let _ = unwatched.complete(&request()).await;
        assert_eq!(count(&hub.db), 1);

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn a_device_thread_failing_on_the_free_limit_records_a_snapshot() {
        let dir = std::env::temp_dir().join(format!("grouper-free-limit-thread-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&openrouter("or")).unwrap();
        db.upsert_model(&core_protocol::ModelEntry {
            id: "free-one".into(),
            provider_id: "or".into(),
            model: "x/y:free".into(),
            input_price: 0.0,
            output_price: 0.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: String::new(),
            free: true,
            roles: vec![],
            local: false,
            modalities: vec![],
            ..Default::default()
        })
        .unwrap();
        let conv = db.create_conversation("project", "free").unwrap();
        let thread = db.create_thread(&conv, "t", "d1", "free-one", None, "brief").unwrap();
        let hub = Hub::new(db, dir.clone());
        let reset_ms = (crate::db::now() + 600) * 1000;
        let summary = format!(
            "Error: or returned HTTP 429: {}",
            RECORDED_FREE_429.replace("1795651200000", &reset_ms.to_string())
        );
        assert!(!note_thread_failure(&hub, &thread.id, "Error: or returned HTTP 503: free-models-per-day"));
        assert!(note_thread_failure(&hub, &thread.id, &summary));
        let rows = hub.db.latest_usage_snapshots().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].provider.as_str(), rows[0].limit_amount, rows[0].resets_at), ("or", Some(50.0), Some(reset_ms / 1000)));
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn counts_free_model_calls_sent_today_per_openrouter_provider() {
        let dir = std::env::temp_dir().join(format!("grouper-free-count-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&openrouter("or")).unwrap();
        db.upsert_provider(&openrouter("or-idle")).unwrap();
        let mut default_or = openrouter("openrouter");
        default_or.base_url = None;
        db.upsert_provider(&default_or).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "direct".into(),
            kind: "openai".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
        for (provider, model) in [("or", "a/b:free"), ("or", "c/d:free"), ("or", "a/b"), ("openrouter", "e/f:free"), ("direct", "g:free")] {
            db.record_usage(None, None, provider, model, 1, 1, 0, 0);
        }
        // One free call from yesterday doesn't count today.
        db.record_usage(None, None, "or", "a/b:free", 1, 1, 0, 0);
        db.c().execute("UPDATE usage SET ts = ts - 86400 WHERE id = (SELECT MAX(id) FROM usage)", []).unwrap();

        let now = crate::db::now();
        let report = free_requests_today(&db, now).unwrap();
        assert_eq!(report.since % 86_400, 0);
        assert!(report.since <= now && now < report.until && report.until - report.since == 86_400);
        let sent: HashMap<_, _> = report.providers.iter().map(|p| (p.provider.as_str(), p.sent)).collect();
        // Only OpenRouter providers are listed; a provider with none today shows 0.
        assert_eq!(sent, HashMap::from([("or", 2), ("or-idle", 0), ("openrouter", 1)]));
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn usage_totals_filter_by_provider() {
        let dir = std::env::temp_dir().join(format!("grouper-usage-provider-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let conversation = db.create_conversation("project", "filter").unwrap();
        db.record_usage(Some(&conversation), None, "a", "m1", 10, 1, 0, 0);
        db.record_usage(Some(&conversation), None, "a", "m2", 20, 2, 0, 0);
        db.record_usage(Some(&conversation), None, "b", "m1", 40, 4, 0, 0);
        let all = db.usage_totals(0, i64::MAX as u64, "model", None, None).unwrap();
        assert_eq!(all.iter().map(|r| r.calls).sum::<u64>(), 3);
        let only_a = db.usage_totals(0, i64::MAX as u64, "model", None, Some("a")).unwrap();
        assert_eq!(only_a.iter().map(|r| (r.key.as_str(), r.input_tokens)).collect::<Vec<_>>(), vec![("m1", 10), ("m2", 20)]);
        let b_in_chat = db.usage_totals(0, i64::MAX as u64, "provider", Some(&conversation), Some("b")).unwrap();
        assert_eq!(b_in_chat.len(), 1);
        assert_eq!((b_in_chat[0].key.as_str(), b_in_chat[0].calls), ("b", 1));
        assert!(db.usage_totals(0, i64::MAX as u64, "day", None, Some("nobody")).unwrap().is_empty());
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn recognises_openrouter_providers() {
        let mut p = openrouter("x");
        assert!(is_openrouter(&p));
        p.base_url = Some("https://OpenRouter.ai/api/v1".into());
        assert!(is_openrouter(&p));
        p.base_url = Some("https://openrouter.ai.example.com/v1".into());
        assert!(!is_openrouter(&p));
        p.base_url = None;
        assert!(!is_openrouter(&p));
        p.id = "openrouter".into();
        assert!(is_openrouter(&p));
        p.kind = "anthropic".into();
        assert!(!is_openrouter(&p));
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
        assert_eq!(collector(&provider(None, Some("k"))), None);
        // The provider called "openrouter" defaults to OpenRouter's URL, as in `providers::openai`.
        let default_url = ProviderConfig { id: "openrouter".into(), ..provider(None, Some("k")) };
        assert_eq!(collector(&default_url), Some(Collector::OpenRouter));
        assert!(is_openrouter(&default_url));
        assert_eq!(collector(&ProviderConfig { api_key: None, ..default_url }), None);
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
            ..Default::default()
        })
        .unwrap();
        let conversation = db.create_conversation("project", "test").unwrap();
        db.record_usage(Some(&conversation), None, "p", "m", 10, 3, 4, 0);
        db.record_usage(Some(&conversation), Some("t1"), "p", "m", 20, 7, 0, 2);

        let rows = db.usage_totals(0, i64::MAX as u64, "conversation", Some(&conversation), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].calls, 2);
        assert_eq!(rows[0].input_tokens, 30);
        assert_eq!(rows[0].output_tokens, 10);
        assert_eq!(rows[0].cache_read_tokens, 4);
        assert_eq!(rows[0].cache_write_tokens, 2);
        let by_thread = db.usage_totals(0, i64::MAX as u64, "thread", Some(&conversation), None).unwrap();
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
            label: None,
            account_key: None,
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
    fn device_probe_input_is_allowlisted_bounded_and_hub_timestamped() {
        let dir = std::env::temp_dir().join(format!("grouper-probe-bounds-{}", random_hex(4)));
        let db = Db::open(&dir).unwrap();
        let long = "é".repeat(200);
        let results: Vec<UsageProbeEntry> = (0..9)
            .map(|index| UsageProbeEntry {
                provider: "claude".into(),
                status: "ok".into(),
                message: None,
                snapshots: vec![UsageSnapshot {
                    account: long.clone(),
                    provider: "claude".into(),
                    kind: "window".into(),
                    window: if index == 0 { long.clone() } else { format!("window-{index}") },
                    label: None,
                    account_key: None,
                    used_pct: Some(index as f64),
                    used_amount: None,
                    limit_amount: None,
                    currency: Some(long.clone()),
                    resets_at: None,
                    source: "spoofed".into(),
                    device_id: Some("spoofed".into()),
                    fetched_at: 1,
                }],
            })
            .collect();
        assert_eq!(db.store_device_probe_results("device", &results, 42).unwrap(), MAX_PROBE_RESULTS);
        let rows = db.latest_usage_snapshots().unwrap();
        assert_eq!(rows.len(), MAX_PROBE_RESULTS);
        assert!(rows.iter().all(|row| row.fetched_at == 42 && row.source == "probe"));
        assert!(rows.iter().all(|row| row.account.len() <= MAX_PROBE_STRING_BYTES));
        assert!(rows.iter().all(|row| row.window.len() <= MAX_PROBE_STRING_BYTES));
        assert!(rows.iter().all(|row| row.currency.as_ref().is_none_or(|s| s.len() <= MAX_PROBE_STRING_BYTES)));

        let invalid = [UsageProbeEntry {
            provider: "attacker".into(),
            status: "ok".into(),
            message: None,
            snapshots: vec![],
        }];
        assert_eq!(db.store_device_probe_results("device", &invalid, 43).unwrap(), 0);
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
            ..Default::default()
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
            ..Default::default()
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
