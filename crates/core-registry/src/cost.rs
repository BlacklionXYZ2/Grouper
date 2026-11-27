//! Token cost estimation, price validation, usage aggregation and time-range parsing.

use anyhow::{Result, bail};
use core_protocol::{ModelEntry, UsageTotal, UsageTotals};
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

/// Metered totals in the half-open time range `[since, until)` from the `usage` table,
/// optionally only for one conversation and/or one provider.
pub fn usage_totals(
    conn: &Connection,
    since: u64,
    until: u64,
    group: &str,
    conversation: Option<&str>,
    provider: Option<&str>,
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
           AND (?4 IS NULL OR provider_id = ?4)
         GROUP BY {key} ORDER BY {key}"
    );
    let mut statement = conn.prepare(&sql)?;
    Ok(statement
        .query_map(params![since as i64, until as i64, conversation, provider], |row| {
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

/// Metered usage of one `(provider, model)` pair, as grouped by [`usage_by_provider_model`].
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSlice {
    pub provider_id: String,
    pub model: String,
    pub totals: UsageTotals,
}

/// Metered usage in `[since, until)` for one billing kind (`api` or `subscription`), grouped by
/// `(provider_id, model)`, optionally for one conversation.
pub fn usage_by_provider_model(
    conn: &Connection,
    since: u64,
    until: u64,
    conversation: Option<&str>,
    billing: &str,
) -> Result<Vec<UsageSlice>> {
    let mut statement = conn.prepare(
        "SELECT provider_id, model, COUNT(*), COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0), COALESCE(SUM(cost), 0)
         FROM usage
         WHERE ts >= ?1 AND ts < ?2 AND billing = ?3 AND (?4 IS NULL OR conversation_id = ?4)
         GROUP BY provider_id, model ORDER BY provider_id, model",
    )?;
    Ok(statement
        .query_map(params![since as i64, until as i64, billing, conversation], |row| {
            Ok(UsageSlice {
                provider_id: row.get(0)?,
                model: row.get(1)?,
                totals: UsageTotals {
                    calls: row.get::<_, i64>(2)? as u64,
                    input_tokens: row.get::<_, i64>(3)? as u64,
                    output_tokens: row.get::<_, i64>(4)? as u64,
                    cache_read_tokens: row.get::<_, i64>(5)? as u64,
                    cache_write_tokens: row.get::<_, i64>(6)? as u64,
                    cost: row.get(7)?,
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Parse `7d`, a Unix timestamp, an RFC 3339 instant (`2026-11-27T00:00:00+01:00`), or `YYYY-MM-DD`.
/// A date-only `until` includes that whole UTC day by returning the following midnight; an
/// exact instant (a timestamp or RFC 3339) is used as given.
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
    if value.len() > 10 && matches!(value.as_bytes()[10], b'T' | b't') {
        return parse_rfc3339(value);
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

/// An RFC 3339 instant such as `2026-11-27T10:30:00Z`, `…T10:30:00.250+01:00` or `…t10:30z`-style
/// lower case, as Unix seconds (fractions are dropped). A seconds field is required, an offset is too.
fn parse_rfc3339(value: &str) -> Result<u64, String> {
    let bad = || format!("invalid time {value:?}; expected RFC 3339 like 2026-11-27T00:00:00Z");
    let (date, rest) = value.split_once(['T', 't']).ok_or_else(bad)?;
    let mut date_parts = date.split('-');
    let year: i32 = date_parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let month: u32 = date_parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let day: u32 = date_parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(bad());
    }
    // Split the clock from the offset: `Z`, or the last `+`/`-` (a clock has no sign of its own).
    let (clock, offset_secs) = if let Some(clock) = rest.strip_suffix(['Z', 'z']) {
        (clock, 0i64)
    } else {
        let at = rest.rfind(['+', '-']).ok_or_else(bad)?;
        let (clock, offset) = rest.split_at(at);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (hh, mm) = offset[1..].split_once(':').ok_or_else(bad)?;
        let (hh, mm): (i64, i64) = (hh.parse().map_err(|_| bad())?, mm.parse().map_err(|_| bad())?);
        if hh > 23 || mm > 59 || offset.len() != 6 {
            return Err(bad());
        }
        (clock, sign * (hh * 3_600 + mm * 60))
    };
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let minute: i64 = clock_parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let seconds = clock_parts.next().ok_or_else(bad)?;
    if clock_parts.next().is_some() {
        return Err(bad());
    }
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    if !fraction.bytes().all(|b| b.is_ascii_digit()) || (seconds.contains('.') && fraction.is_empty()) {
        return Err(bad());
    }
    let second: i64 = whole.parse().map_err(|_| bad())?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return Err(bad());
    }
    let instant = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs;
    u64::try_from(instant).map_err(|_| "times before 1970 are not supported".to_string())
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

    #[test]
    fn parses_exact_instants_and_rfc3339() {
        // 2026-11-25 is 1_795_564_800 (a UTC midnight); the next day is 86_400 later.
        let midnight = 1_795_564_800 + 86_400;
        assert_eq!(parse_time(Some("1795651200"), 0, true).unwrap(), midnight);
        assert_eq!(parse_time(Some("2026-11-26T00:00:00Z"), 0, false).unwrap(), midnight);
        // An exact instant is not rounded up to the end of the day, even as `until`.
        assert_eq!(parse_time(Some("2026-11-26T00:00:00Z"), 0, true).unwrap(), midnight);
        assert_eq!(parse_time(Some("2026-11-26t00:00:00z"), 0, true).unwrap(), midnight);
        // Local midnight in UTC+01:00 is 23:00 UTC the evening before; UTC-05:00 is 05:00 UTC.
        assert_eq!(parse_time(Some("2026-11-26T00:00:00+01:00"), 0, false).unwrap(), midnight - 3_600);
        assert_eq!(parse_time(Some("2026-11-26T00:00:00-05:00"), 0, false).unwrap(), midnight + 5 * 3_600);
        assert_eq!(parse_time(Some("2026-11-26T00:00:00.999Z"), 0, false).unwrap(), midnight);
        assert_eq!(parse_time(Some("2026-11-26T12:34:56Z"), 0, false).unwrap(), midnight + 12 * 3_600 + 34 * 60 + 56);
        for bad in [
            "2026-11-26T00:00Z",
            "2026-11-26T00:00:00",
            "2026-11-26T25:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-11-26T00:00:00+0100",
            "2026-11-26T00:00:00.Z",
            "1969-12-31T23:59:59Z",
            "2026-11-26Tgarbage",
        ] {
            assert!(parse_time(Some(bad), 0, false).is_err(), "{bad} should not parse");
        }
        // The date and relative forms still work.
        assert_eq!(parse_time(Some("2026-11-26"), 0, true).unwrap(), midnight + 86_400);
    }
}
