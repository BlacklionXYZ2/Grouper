use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use core_client::core_protocol::{UsageSnapshot, UsageTotal};

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
    /// Show the latest balances and spending windows reported by providers.
    Balances {
        /// Poll supported providers before printing.
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
    println!("{:<14} {:<9} {:<10} {:>14} {:>14} {:>8} {:>12}", "provider", "kind", "window", "used/balance", "limit", "used", "age");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    for row in rows {
        let currency = row.currency.as_deref().unwrap_or("");
        let amount = row.used_amount.map(|value| format!("{value:.4} {currency}")).unwrap_or_else(|| "-".into());
        let limit = row.limit_amount.map(|value| format!("{value:.4} {currency}")).unwrap_or_else(|| "-".into());
        let percent = row.used_pct.map(|value| format!("{value:.1}%")).unwrap_or_else(|| "-".into());
        println!(
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
    Ok(())
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
