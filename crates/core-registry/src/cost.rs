//! Token cost estimation, price validation, usage aggregation and time-range parsing.

use anyhow::{Result, bail};
use core_protocol::{ModelEntry, UsageTotal};
use rusqlite::{Connection, params};

/// Validate a USD-per-million-tokens price field.
pub fn check_price(field: &str, value: f64) -> Result<()> {
    if !value.is_finite() || value < 0.0 {
        bail!("{field} must be a non-negative finite number, got {value}");
    }
    Ok(())
}

/// Estimate the USD cost of a model call (cache-aware, using explicit model
/// cache price overrides or provider-kind defaults).
pub fn estimate_cost(
    model: &ModelEntry,
    provider_kind: Option<&str>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> f64 {
    let anthropic = provider_kind == Some("anthropic");
    let cache_read_price = model.cache_read_price.unwrap_or(model.input_price * if anthropic { 0.1 } else { 0.5 });
    let cache_write_price = model.cache_write_price.unwrap_or(model.input_price * if anthropic { 1.25 } else { 1.0 });
    (model.input_price * input_tokens as f64
        + model.output_price * output_tokens as f64
        + cache_read_price * cache_read_tokens as f64
        + cache_write_price * cache_write_tokens as f64)
        / 1_000_000.0
}

/// `(input tokens, output tokens, cost)` for a conversation from the `usage` table.
pub fn usage_total(conn: &Connection, conversation_id: &str) -> (u64, u64, f64) {
    conn.query_row(
        "SELECT COALESCE(SUM(input_tokens + cache_read_tokens + cache_write_tokens), 0),
                COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cost), 0)
         FROM usage WHERE conversation_id = ?1",
        [conversation_id],
        |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get(2)?)),
    )
    .unwrap_or((0, 0, 0.0))
}

/// Metered totals in the half-open time range `[since, until)` from the `usage` table.
pub fn usage_totals(
    conn: &Connection,
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
    let mut statement = conn.prepare(&sql)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_time_ranges() {
        assert_eq!(parse_time(Some("7d"), 10 * 86_400, false).unwrap(), 3 * 86_400);
        assert_eq!(parse_time(Some("1970-01-01"), 0, false).unwrap(), 0);
        assert_eq!(parse_time(Some("1970-01-01"), 0, true).unwrap(), 86_400);
        assert_eq!(parse_time(Some("2026-11-25"), 0, false).unwrap(), 1_795_564_800);
        assert!(parse_time(Some("2026-02-29"), 0, false).is_err());
    }
}
