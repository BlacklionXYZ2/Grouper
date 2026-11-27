use std::collections::HashMap;

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use core_client::core_protocol::{DeviceInfo, UsageSnapshot, UsageTotal};

use crate::Api;

#[derive(Args)]
pub struct UsageArgs {
    /// Start of the range: a relative duration such as 7d, a UTC date, or a Unix timestamp.
    #[arg(long)]
    since: Option<String>,
    /// End of the range (a date includes that whole UTC day).
    #[arg(long)]
    until: Option<String>,
    /// How to group metered usage.
    #[arg(long, value_enum, default_value_t = UsageGroup::Day)]
    by: UsageGroup,
    #[command(subcommand)]
    command: Option<UsageCommand>,
}

#[derive(Clone, Copy, ValueEnum)]
enum UsageGroup {
    Day,
    Provider,
    Model,
    Conversation,
    Thread,
}

impl UsageGroup {
    fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Provider => "provider",
            Self::Model => "model",
            Self::Conversation => "conversation",
            Self::Thread => "thread",
        }
    }
}

#[derive(Subcommand)]
enum UsageCommand {
    /// Show the latest provider balances and device subscription usage windows.
    Balances {
        /// Poll supported providers and online opted-in devices before printing.
        #[arg(long)]
        refresh: bool,
    },
}

pub async fn run(api: &Api, args: UsageArgs) -> Result<()> {
    if let Some(UsageCommand::Balances { refresh }) = args.command {
        if refresh {
            api.post("/v1/usage/refresh", &serde_json::json!({})).await?;
        }
        return balances(api).await;
    }

    let mut params = vec![("group", args.by.as_str().to_string())];
    if let Some(since) = args.since {
        params.push(("since", since));
    }
    if let Some(until) = args.until {
        params.push(("until", until));
    }
    let url = reqwest::Url::parse_with_params("http://hub/v1/usage", &params)?;
    let rows: Vec<UsageTotal> = api.get(&format!("/v1/usage?{}", url.query().unwrap_or_default())).await?;
    print_totals(&rows, args.by.as_str());
    Ok(())
}

pub async fn conversation_line(api: &Api, conversation: &str) -> String {
    let url = reqwest::Url::parse_with_params(
        "http://hub/v1/usage",
        [("group", "conversation"), ("conversation", conversation)],
    );
    let Ok(url) = url else { return "usage unavailable".into() };
    let rows: Result<Vec<UsageTotal>, _> = api.get(&format!("/v1/usage?{}", url.query().unwrap_or_default())).await;
    let Ok(rows) = rows else { return "usage unavailable".into() };
    let total = sum(&rows);
    format!(
        "conversation used {} input + {} cached + {} output = {} tokens; estimated cost ${:.6}",
        total.input_tokens,
        total.cache_read_tokens + total.cache_write_tokens,
        total.output_tokens,
        total.total_tokens(),
        total.cost
    )
}

fn print_totals(rows: &[UsageTotal], group: &str) {
    if rows.is_empty() {
        println!("no usage in this range");
        return;
    }
    println!(
        "{:<24} {:>6} {:>12} {:>12} {:>12} {:>12} {:>12}",
        group, "calls", "input", "cache read", "cache write", "output", "cost"
    );
    for row in rows {
        println!(
            "{:<24} {:>6} {:>12} {:>12} {:>12} {:>12} ${:>11.6}",
            truncate(&row.key, 24),
            row.calls,
            row.input_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.output_tokens,
            row.cost
        );
    }
    let total = sum(rows);
    println!(
        "{:<24} {:>6} {:>12} {:>12} {:>12} {:>12} ${:>11.6}",
        "TOTAL",
        total.calls,
        total.input_tokens,
        total.cache_read_tokens,
        total.cache_write_tokens,
        total.output_tokens,
        total.cost
    );
}

async fn balances(api: &Api) -> Result<()> {
    let rows: Vec<UsageSnapshot> = api.get("/v1/usage/snapshots").await?;
    if rows.is_empty() {
        println!("no provider balances available");
        return Ok(());
    }
    let devices: HashMap<String, (String, bool)> = api
        .get::<Vec<DeviceInfo>>("/v1/devices")
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|d| (d.id, (d.name, d.online)))
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    print!("{}", format_balances(&rows, &devices, now));
    Ok(())
}

fn display_provider(provider: &str) -> String {
    match provider {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn format_reset_time(resets_at: u64, now: u64) -> String {
    let days = (resets_at / 86_400) as i64;
    let sod = resets_at % 86_400;
    let hh = sod / 3_600;
    let mm = (sod % 3_600) / 60;
    if resets_at <= now.saturating_add(86_400) && resets_at.saturating_add(86_400) >= now {
        format!("{hh:02}:{mm:02}")
    } else {
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
    }
}

// Inverse of Howard Hinnant's civil-from-days algorithm (public domain).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i64::from(m <= 2);
    (y as i32, m as u32, d as u32)
}

fn device_tag(row: &UsageSnapshot, devices: &HashMap<String, (String, bool)>, now: u64) -> String {
    let age_str = format!("{} ago", age(now.saturating_sub(row.fetched_at)));
    match row.device_id.as_deref() {
        Some(id) => match devices.get(id) {
            Some((name, true)) => format!("[{name}, {age_str}]"),
            Some((name, false)) => format!("[{name}, offline, {age_str}]"),
            None => format!("[{id}, {age_str}]"),
        },
        None => format!("[{age_str}]"),
    }
}

fn format_balances(rows: &[UsageSnapshot], devices: &HashMap<String, (String, bool)>, now: u64) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let (windows, others): (Vec<&UsageSnapshot>, Vec<&UsageSnapshot>) =
        rows.iter().partition(|r| r.kind == "window" || r.kind == "status");

    for row in &windows {
        let provider = display_provider(&row.provider);
        let tag = device_tag(row, devices, now);
        if row.kind == "status" {
            let _ = writeln!(out, "{provider}: {}  {tag}", row.window);
        } else {
            let pct = row
                .used_pct
                .map(|v| {
                    if (v - v.round()).abs() < 0.05 {
                        format!("{:.0}%", v.round())
                    } else {
                        format!("{v:.1}%")
                    }
                })
                .unwrap_or_else(|| "-".into());
            let reset = row
                .resets_at
                .map(|ts| format!(" (resets {})", format_reset_time(ts, now)))
                .unwrap_or_default();
            let _ = writeln!(out, "{provider} {}: {pct}{reset}  {tag}", row.window);
        }
    }

    if !others.is_empty() {
        if !windows.is_empty() {
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "{:<14} {:<9} {:<10} {:>14} {:>14} {:>8} {:>12}",
            "provider", "kind", "window", "used/balance", "limit", "used", "age"
        );
        for row in others {
            let currency = row.currency.as_deref().unwrap_or("");
            let amount = row.used_amount.map(|value| format!("{value:.4} {currency}")).unwrap_or_else(|| "-".into());
            let limit = row.limit_amount.map(|value| format!("{value:.4} {currency}")).unwrap_or_else(|| "-".into());
            let percent = row.used_pct.map(|value| format!("{value:.1}%")).unwrap_or_else(|| "-".into());
            let _ = writeln!(
                out,
                "{:<14} {:<9} {:<10} {:>14} {:>14} {:>8} {:>12}",
                truncate(&row.provider, 14),
                row.kind,
                truncate(&row.window, 10),
                amount,
                limit,
                percent,
                age(now.saturating_sub(row.fetched_at))
            );
        }
    }
    out
}

fn sum(rows: &[UsageTotal]) -> UsageTotal {
    rows.iter().fold(
        UsageTotal {
            key: "TOTAL".into(),
            calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: 0.0,
        },
        |mut total, row| {
            total.calls += row.calls;
            total.input_tokens += row.input_tokens;
            total.output_tokens += row.output_tokens;
            total.cache_read_tokens += row.cache_read_tokens;
            total.cache_write_tokens += row.cache_write_tokens;
            total.cost += row.cost;
            total
        },
    )
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.into()
    } else {
        value.chars().take(width.saturating_sub(1)).chain(std::iter::once('…')).collect()
    }
}

fn age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_subscription_windows_offline_age_status_and_balances() {
        // 2026-11-25T12:00:00Z = 1_795_608_000; 14:05:00Z = 1_795_615_500.
        let now = 1_795_608_000;
        let devices = HashMap::from([
            ("d1".to_string(), ("PC".to_string(), true)),
            ("d2".to_string(), ("Mac".to_string(), false)),
        ]);
        let rows = vec![
            UsageSnapshot {
                account: "claude-max".into(),
                provider: "claude".into(),
                kind: "window".into(),
                window: "5h".into(),
                label: None,
                account_key: None,
                used_pct: Some(42.0),
                used_amount: None,
                limit_amount: None,
                currency: None,
                resets_at: Some(1_795_615_500),
                source: "probe".into(),
                device_id: Some("d1".into()),
                fetched_at: now - 120,
            },
            UsageSnapshot {
                account: "chatgpt-pro".into(),
                provider: "codex".into(),
                kind: "window".into(),
                window: "7d".into(),
                label: None,
                account_key: None,
                used_pct: Some(18.5),
                used_amount: None,
                limit_amount: None,
                currency: None,
                resets_at: Some(now + 3 * 86_400 + 7_500),
                source: "probe".into(),
                device_id: Some("d2".into()),
                fetched_at: now - 3_600,
            },
            UsageSnapshot {
                account: "codex".into(),
                provider: "codex".into(),
                kind: "status".into(),
                window: "unavailable: HTTP 403".into(),
                label: None,
                account_key: None,
                used_pct: None,
                used_amount: None,
                limit_amount: None,
                currency: None,
                resets_at: None,
                source: "probe".into(),
                device_id: Some("d1".into()),
                fetched_at: now - 5,
            },
            UsageSnapshot {
                account: "openrouter".into(),
                provider: "openrouter".into(),
                kind: "balance".into(),
                window: "total".into(),
                label: None,
                account_key: None,
                used_pct: Some(25.0),
                used_amount: Some(12.5),
                limit_amount: Some(50.0),
                currency: Some("USD".into()),
                resets_at: None,
                source: "admin_api".into(),
                device_id: None,
                fetched_at: now - 600,
            },
        ];

        let rendered = format_balances(&rows, &devices, now);
        assert!(rendered.contains("Claude 5h: 42% (resets 14:05)  [PC, 2m ago]"), "{rendered}");
        assert!(rendered.contains("Codex 7d: 18.5% (resets 2026-11-28 14:05 UTC)  [Mac, offline, 1h ago]"), "{rendered}");
        assert!(rendered.contains("Codex: unavailable: HTTP 403  [PC, 5s ago]"), "{rendered}");
        assert!(rendered.contains("openrouter"), "{rendered}");
    }
}
