//! Metered usage queries and official provider balance collectors.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_protocol::{ProviderConfig, UsageSnapshot, UsageTotal};
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

use crate::db::{Db, now};
use crate::server::Hub;

const COLLECT_INTERVAL: Duration = Duration::from_secs(60 * 60);

impl Db {
    /// Totals in the half-open time range `[since, until)`.
    pub fn usage_totals(
        &self,
        since: u64,
        until: u64,
        group: &str,
        conversation: Option<&str>,
    ) -> Result<Vec<UsageTotal>> {
        let key = match group {
            "day" => "strftime('%Y-%m-%d', ts, 'unixepoch')",
            "provider" => "provider_id",
            "model" => "model",
            "conversation" => "COALESCE(conversation_id, '(none)')",
            "thread" => "COALESCE(thread_id, '(orchestrator)')",
            _ => bail!("group must be day, provider, model, conversation or thread"),
        };
        let sql = format!(
            "SELECT {key}, COUNT(*), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0), COALESCE(SUM(cost), 0)
             FROM usage
             WHERE ts >= ?1 AND ts < ?2 AND (?3 IS NULL OR conversation_id = ?3)
             GROUP BY {key} ORDER BY {key}"
        );
        let c = self.c();
        let mut statement = c.prepare(&sql)?;
        Ok(statement
            .query_map(params![since as i64, until as i64, conversation], |row| {
                Ok(UsageTotal {
                    key: row.get(0)?,
                    calls: row.get::<_, i64>(1)? as u64,
                    input_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    cache_read_tokens: row.get::<_, i64>(4)? as u64,
                    cache_write_tokens: row.get::<_, i64>(5)? as u64,
                    cost: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
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

    /// The newest observation for each provider/account/kind/window/currency tuple.
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
                 AND newest.currency IS s.currency
               ORDER BY fetched_at DESC, id DESC LIMIT 1
             )
             ORDER BY provider, account, kind, window, currency",
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

/// Parse `7d`, a Unix timestamp, or `YYYY-MM-DD`. Date-only `until` values include
/// that whole UTC day by returning the following midnight.
pub fn parse_time(value: Option<&str>, current: u64, until: bool) -> Result<u64, String> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(if until { i64::MAX as u64 } else { 0 });
    };
    if let Some(days) = value.strip_suffix('d') {
        let days: u64 = days.parse().map_err(|_| format!("invalid relative time {value:?}"))?;
        return Ok(current.saturating_sub(days.saturating_mul(86_400)));
    }
    if let Ok(timestamp) = value.parse::<u64>() {
        return Ok(timestamp.min(i64::MAX as u64));
    }
    let mut parts = value.split('-');
    let year: i32 = parts.next().and_then(|v| v.parse().ok()).ok_or_else(|| format!("invalid date {value:?}"))?;
    let month: u32 = parts.next().and_then(|v| v.parse().ok()).ok_or_else(|| format!("invalid date {value:?}"))?;
    let day: u32 = parts.next().and_then(|v| v.parse().ok()).ok_or_else(|| format!("invalid date {value:?}"))?;
    if parts.next().is_some() || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(format!("invalid date {value:?}"));
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return Err("dates before 1970 are not supported".into());
    }
    Ok(days as u64 * 86_400 + if until { 86_400 } else { 0 })
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Days since 1970-01-01. Adapted from Howard Hinnant's public-domain civil calendar algorithm.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let shifted_month = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
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
    pub snapshots_stored: usize,
}

/// Refresh all supported providers. One provider's failure doesn't affect the others,
/// and a failed provider writes no partial observations.
pub async fn refresh(hub: &Hub) -> RefreshResult {
    let providers = match hub.db.providers() {
        Ok(providers) => providers,
        Err(error) => {
            tracing::warn!("could not read providers for usage refresh: {error:#}");
            return RefreshResult { providers_checked: 0, snapshots_stored: 0 };
        }
    };
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("HTTP client");
    let mut result = RefreshResult { providers_checked: 0, snapshots_stored: 0 };
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
    result
}

pub async fn collector_loop(hub: Arc<Hub>) {
    loop {
        let result = refresh(&hub).await;
        tracing::debug!(
            providers = result.providers_checked,
            snapshots = result.snapshots_stored,
            "usage refresh complete"
        );
        tokio::time::sleep(COLLECT_INTERVAL).await;
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
    fn parses_time_ranges() {
        assert_eq!(parse_time(Some("7d"), 10 * 86_400, false).unwrap(), 3 * 86_400);
        assert_eq!(parse_time(Some("1970-01-01"), 0, false).unwrap(), 0);
        assert_eq!(parse_time(Some("1970-01-01"), 0, true).unwrap(), 86_400);
        assert_eq!(parse_time(Some("2026-11-25"), 0, false).unwrap(), 1_795_564_800);
        assert!(parse_time(Some("2026-02-29"), 0, false).is_err());
    }

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
}
