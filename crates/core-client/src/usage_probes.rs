//! Read-only subscription usage probes for Claude Code (Pro/Max) and Codex (ChatGPT).
//!
//! Exposed only as the unlisted internal MCP tool [`USAGE_PROBE_TOOL`] when a device
//! explicitly opts in (`Config::usage_probes` / `grouper connect --usage-probes`).
//! Never listed in `tools/list`, never added to the tool registry, and never offered
//! to threads.
//!
//! Security properties:
//! - Read-only: reads credentials the official CLIs already store on this device and
//!   never modifies or refreshes them.
//! - Hardcoded endpoints only (`api.anthropic.com`, `chatgpt.com`) with HTTP redirects
//!   disabled so bearer tokens can never be forwarded elsewhere.
//! - Tokens and account IDs are never logged, never returned to the hub, and redacted
//!   from `Debug` output.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use core_protocol::{USAGE_PROBE_TOOL, UsageProbeEntry, UsageProbeResponse, UsageSnapshot};
use serde_json::{Value, json};

pub const SUPPORTED_PROVIDERS: &[&str] = &["claude", "codex"];

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_BETA_HEADER: &str = "oauth-2025-04-20";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_PROBE_INTERVAL: Duration = Duration::from_secs(30);

struct ProbeCache {
    completed_at: tokio::time::Instant,
    result: (bool, String),
}

fn probe_state() -> &'static tokio::sync::Mutex<Option<ProbeCache>> {
    static STATE: OnceLock<tokio::sync::Mutex<Option<ProbeCache>>> = OnceLock::new();
    STATE.get_or_init(|| tokio::sync::Mutex::new(None))
}

#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

pub(crate) fn is_internal(tool: &str) -> bool {
    tool == USAGE_PROBE_TOOL
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        // Never follow redirects: bearer tokens must only ever hit the hardcoded host.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("grouper/{}", crate::CLIENT_VERSION))
        .build()
        .map_err(|_| "failed to build HTTP client".to_string())
}

/// Run the internal `grouper_usage_probe` tool.
///
/// `args` may pass `{"providers": ["claude", "codex"]}`; omitted or empty probes all
/// supported providers.
pub(crate) async fn call(args: &Value) -> (bool, String) {
    // Holding this mutex through the provider requests makes probes single-flight.
    // A completed result is reused briefly even if a hostile hub asks repeatedly.
    let mut state = probe_state().lock().await;
    if let Some(cached) = state.as_ref()
        && cached.completed_at.elapsed() < MIN_PROBE_INTERVAL
    {
        return cached.result.clone();
    }
    let result = call_uncached(args).await;
    *state = Some(ProbeCache {
        completed_at: tokio::time::Instant::now(),
        result: result.clone(),
    });
    result
}

async fn call_uncached(args: &Value) -> (bool, String) {
    let requested: Vec<&str> = match args.get("providers").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => {
            let mut list = Vec::new();
            for item in arr {
                if let Some(name) = item.as_str() {
                    let normalized = name.trim().to_ascii_lowercase();
                    for &supported in SUPPORTED_PROVIDERS {
                        if normalized == supported && !list.contains(&supported) {
                            list.push(supported);
                        }
                    }
                }
            }
            if list.is_empty() { SUPPORTED_PROVIDERS.to_vec() } else { list }
        }
        _ => SUPPORTED_PROVIDERS.to_vec(),
    };

    let client = match http_client() {
        Ok(c) => c,
        Err(msg) => return (false, json!({"error": msg}).to_string()),
    };
    let fetched_at = now_secs();
    let futs = requested.into_iter().map(|provider| {
        let client = client.clone();
        async move {
            match provider {
                "claude" => probe_claude_with(&client, CLAUDE_USAGE_URL, None, fetched_at).await,
                "codex" => probe_codex_with(&client, CODEX_USAGE_URL, None, fetched_at).await,
                other => UsageProbeEntry {
                    provider: other.into(),
                    status: "unavailable".into(),
                    message: Some(format!("unavailable: unsupported provider {other}")),
                    snapshots: Vec::new(),
                },
            }
        }
    });
    let results = futures_util::future::join_all(futs).await;
    let payload = UsageProbeResponse { results };
    match serde_json::to_string(&payload) {
        Ok(text) => (true, text),
        Err(_) => (false, json!({"error": "failed to serialize probe results"}).to_string()),
    }
}

// ---- Credentials (read-only, redacted Debug)

struct ClaudeCredentials {
    access_token: String,
    account_label: String,
}

impl std::fmt::Debug for ClaudeCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCredentials")
            .field("access_token", &"[redacted]")
            .field("account_label", &self.account_label)
            .finish()
    }
}

struct CodexCredentials {
    access_token: String,
    account_id: Option<String>,
}

impl std::fmt::Debug for CodexCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCredentials")
            .field("access_token", &"[redacted]")
            .field("has_account_id", &self.account_id.is_some())
            .finish()
    }
}

#[derive(Debug)]
enum CredLoad<T> {
    Ready(T),
    NotSignedIn,
    Expired,
    Unavailable(String),
}

fn claude_credentials_path(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = home {
        return Some(dir.join(".claude").join(".credentials.json"));
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join(".credentials.json"));
    }
    dirs::home_dir().map(|h| h.join(".claude").join(".credentials.json"))
}

fn codex_credentials_path(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = home {
        return Some(dir.join(".codex").join("auth.json"));
    }
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join("auth.json"));
    }
    dirs::home_dir().map(|h| h.join(".codex").join("auth.json"))
}

#[cfg(target_os = "macos")]
fn read_claude_keychain_json() -> Option<String> {
    let mut accounts: Vec<String> = Vec::new();
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        accounts.push(user);
    }
    accounts.push(String::new());
    for account in accounts {
        if let Ok(bytes) =
            security_framework::passwords::get_generic_password(CLAUDE_KEYCHAIN_SERVICE, &account)
            && let Ok(text) = String::from_utf8(bytes)
            && !text.trim().is_empty()
        {
            return Some(text);
        }
    }
    None
}

fn load_claude_credentials(home: Option<&Path>, now: u64) -> CredLoad<ClaudeCredentials> {
    #[cfg(target_os = "macos")]
    if home.is_none()
        && let Some(text) = read_claude_keychain_json()
    {
        return parse_claude_credentials_json(&text, now);
    }

    let Some(path) = claude_credentials_path(home) else {
        return CredLoad::NotSignedIn;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CredLoad::NotSignedIn,
        Err(_) => return CredLoad::Unavailable("could not read Claude credentials file".into()),
    };
    parse_claude_credentials_json(&text, now)
}

fn parse_claude_credentials_json(text: &str, now: u64) -> CredLoad<ClaudeCredentials> {
    let root: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return CredLoad::Unavailable("invalid Claude credentials JSON".into()),
    };
    let Some(oauth) = root.get("claudeAiOauth").and_then(Value::as_object) else {
        return CredLoad::NotSignedIn;
    };
    let Some(access_token) = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return CredLoad::NotSignedIn;
    };
    if let Some(expires_secs) = oauth.get("expiresAt").and_then(parse_unix_secs)
        && expires_secs <= now
    {
        return CredLoad::Expired;
    }
    let account_label = oauth
        .get("subscriptionType")
        .and_then(Value::as_str)
        .and_then(sanitize_label)
        .map(|plan| format!("claude-{plan}"))
        .unwrap_or_else(|| "claude".into());
    CredLoad::Ready(ClaudeCredentials { access_token: access_token.to_string(), account_label })
}

fn load_codex_credentials(home: Option<&Path>, now: u64) -> CredLoad<CodexCredentials> {
    let Some(path) = codex_credentials_path(home) else {
        return CredLoad::NotSignedIn;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CredLoad::NotSignedIn,
        Err(_) => return CredLoad::Unavailable("could not read Codex auth file".into()),
    };
    parse_codex_credentials_json(&text, now)
}

fn parse_codex_credentials_json(text: &str, now: u64) -> CredLoad<CodexCredentials> {
    let root: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return CredLoad::Unavailable("invalid Codex auth JSON".into()),
    };
    let tokens = root.get("tokens").and_then(Value::as_object);
    let get_str = |key: &str, alt: &str| -> Option<String> {
        tokens
            .and_then(|t| t.get(key).or_else(|| t.get(alt)))
            .or_else(|| root.get(key).or_else(|| root.get(alt)))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let Some(access_token) = get_str("access_token", "accessToken") else {
        return CredLoad::NotSignedIn;
    };
    if let Some(exp) = jwt_exp_secs(&access_token)
        && exp <= now
    {
        return CredLoad::Expired;
    }
    let account_id = get_str("account_id", "accountId");
    CredLoad::Ready(CodexCredentials { access_token, account_id })
}

/// Extract only the numeric `exp` claim from a JWT access token, if present.
/// Never reads or returns any other claim.
pub(crate) fn jwt_exp_secs(token: &str) -> Option<u64> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp").and_then(parse_unix_secs)
}

/// Check Claude Code credential presence and expiry without returning any token or secret.
pub(crate) fn claude_sign_in_state(home: Option<&Path>, now: u64) -> core_protocol::HarnessSignIn {
    match load_claude_credentials(home, now) {
        CredLoad::Ready(_) => core_protocol::HarnessSignIn::SignedIn,
        CredLoad::Expired => core_protocol::HarnessSignIn::Expired,
        CredLoad::NotSignedIn | CredLoad::Unavailable(_) => core_protocol::HarnessSignIn::NotSignedIn,
    }
}

/// Check Codex credential presence and expiry without returning any token or secret.
pub(crate) fn codex_sign_in_state(home: Option<&Path>, now: u64) -> core_protocol::HarnessSignIn {
    match load_codex_credentials(home, now) {
        CredLoad::Ready(_) => core_protocol::HarnessSignIn::SignedIn,
        CredLoad::Expired => core_protocol::HarnessSignIn::Expired,
        CredLoad::NotSignedIn | CredLoad::Unavailable(_) => core_protocol::HarnessSignIn::NotSignedIn,
    }
}

/// Sanitize a short non-secret label (e.g. plan tier like `max`, `pro`, `plus`).
fn sanitize_label(raw: &str) -> Option<String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() || trimmed.len() > 24 {
        return None;
    }
    if trimmed.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Some(trimmed)
    } else {
        None
    }
}

/// Sanitize a model/feature name into a short window suffix slug.
fn sanitize_slug(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut last_sep = true;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep && !out.is_empty() {
            out.push('-');
            last_sep = true;
        }
        if out.len() >= 24 {
            break;
        }
    }
    let trimmed = out.trim_end_matches('-').to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

// ---- HTTP probe execution

fn status_entry(provider: &str, status: &str, message: impl Into<String>) -> UsageProbeEntry {
    UsageProbeEntry {
        provider: provider.into(),
        status: status.into(),
        message: Some(message.into()),
        snapshots: Vec::new(),
    }
}

fn classify_http_error(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "request timed out"
    } else if err.is_connect() {
        "connection failed"
    } else {
        "request failed"
    }
}

async fn probe_claude_with(
    client: &reqwest::Client,
    url: &str,
    home: Option<&Path>,
    fetched_at: u64,
) -> UsageProbeEntry {
    let creds = match load_claude_credentials(home, fetched_at) {
        CredLoad::Ready(c) => c,
        CredLoad::NotSignedIn => return status_entry("claude", "not_signed_in", "not signed in"),
        CredLoad::Expired => return status_entry("claude", "expired", "expired"),
        CredLoad::Unavailable(msg) => return status_entry("claude", "unavailable", format!("unavailable: {msg}")),
    };

    let response = match client
        .get(url)
        .bearer_auth(&creds.access_token)
        .header("anthropic-beta", CLAUDE_BETA_HEADER)
        .header("accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            return status_entry("claude", "unavailable", format!("unavailable: {}", classify_http_error(&err)));
        }
    };

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return status_entry("claude", "expired", "expired");
    }
    if !status.is_success() {
        return status_entry("claude", "unavailable", format!("unavailable: HTTP {}", status.as_u16()));
    }
    let body: Value = match response.json().await {
        Ok(v) => v,
        Err(_) => return status_entry("claude", "unavailable", "unavailable: invalid JSON response"),
    };
    match parse_claude_usage(&body, &creds.account_label, fetched_at) {
        Ok(snapshots) if !snapshots.is_empty() => UsageProbeEntry {
            provider: "claude".into(),
            status: "ok".into(),
            message: None,
            snapshots,
        },
        Ok(_) => status_entry("claude", "unavailable", "unavailable: no usage windows in response"),
        Err(msg) => status_entry("claude", "unavailable", format!("unavailable: {msg}")),
    }
}

async fn probe_codex_with(
    client: &reqwest::Client,
    url: &str,
    home: Option<&Path>,
    fetched_at: u64,
) -> UsageProbeEntry {
    let creds = match load_codex_credentials(home, fetched_at) {
        CredLoad::Ready(c) => c,
        CredLoad::NotSignedIn => return status_entry("codex", "not_signed_in", "not signed in"),
        CredLoad::Expired => return status_entry("codex", "expired", "expired"),
        CredLoad::Unavailable(msg) => return status_entry("codex", "unavailable", format!("unavailable: {msg}")),
    };

    let mut req = client.get(url).bearer_auth(&creds.access_token).header("accept", "application/json");
    if let Some(account_id) = &creds.account_id {
        req = req.header("ChatGPT-Account-Id", account_id);
    }
    let response = match req.send().await {
        Ok(r) => r,
        Err(err) => {
            return status_entry("codex", "unavailable", format!("unavailable: {}", classify_http_error(&err)));
        }
    };

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return status_entry("codex", "expired", "expired");
    }
    if !status.is_success() {
        return status_entry("codex", "unavailable", format!("unavailable: HTTP {}", status.as_u16()));
    }
    let body: Value = match response.json().await {
        Ok(v) => v,
        Err(_) => return status_entry("codex", "unavailable", "unavailable: invalid JSON response"),
    };
    match parse_codex_usage(&body, fetched_at) {
        Ok(snapshots) if !snapshots.is_empty() => UsageProbeEntry {
            provider: "codex".into(),
            status: "ok".into(),
            message: None,
            snapshots,
        },
        Ok(_) => status_entry("codex", "unavailable", "unavailable: no usage windows in response"),
        Err(msg) => status_entry("codex", "unavailable", format!("unavailable: {msg}")),
    }
}

// ---- Response parsers

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()))
        .filter(|n| n.is_finite())
}

fn parse_unix_secs(value: &Value) -> Option<u64> {
    let raw = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|v| u64::try_from(v).ok()))
        .or_else(|| value.as_f64().filter(|v| v.is_finite() && *v >= 0.0).map(|v| v as u64))
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())?;
    // Timestamps above 10^10 are milliseconds (e.g. JS Date.now()).
    Some(if raw > 10_000_000_000 { raw / 1000 } else { raw })
}

/// Parse a Unix timestamp (seconds or milliseconds) or an RFC 3339 / ISO 8601 timestamp string.
pub(crate) fn parse_timestamp(value: &Value) -> Option<u64> {
    if let Some(secs) = parse_unix_secs(value) {
        return Some(secs);
    }
    parse_rfc3339(value.as_str()?)
}

fn parse_rfc3339(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't' | b' ') || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u32 = s[11..13].parse().ok()?;
    let minute: u32 = s[14..16].parse().ok()?;
    let second: u32 = s[17..19].parse().ok()?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &after_dot[digits..];
    }
    let offset_secs: i64 = if rest.is_empty() || rest == "Z" || rest == "z" {
        0
    } else {
        let tz = rest.strip_prefix('+').or_else(|| rest.strip_prefix('-'))?;
        let sign: i64 = if rest.starts_with('+') { 1 } else { -1 };
        let (th, tm) = if let Some((h, m)) = tz.split_once(':') {
            (h, m)
        } else if tz.len() == 4 {
            (&tz[0..2], &tz[2..4])
        } else {
            return None;
        };
        let oh: i64 = th.parse().ok()?;
        let om: i64 = tm.parse().ok()?;
        if !(0..=23).contains(&oh) || !(0..=59).contains(&om) {
            return None;
        }
        sign * (oh * 3600 + om * 60)
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second.min(59)) - offset_secs;
    u64::try_from(secs).ok()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let shifted_month = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted_month + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as i64
}

fn claude_window_name(key: &str) -> String {
    match key {
        "five_hour" => "5h".into(),
        "seven_day" => "7d".into(),
        other => {
            if let Some(rest) = other.strip_prefix("seven_day_") {
                format!("7d:{rest}")
            } else if let Some(rest) = other.strip_prefix("five_hour_") {
                format!("5h:{rest}")
            } else {
                other.to_string()
            }
        }
    }
}

/// Parse Claude's `GET /api/oauth/usage` response into normalized [`UsageSnapshot`] rows.
pub fn parse_claude_usage(body: &Value, account: &str, fetched_at: u64) -> Result<Vec<UsageSnapshot>, String> {
    let obj = body.as_object().ok_or_else(|| "Claude usage response is not a JSON object".to_string())?;
    let mut snapshots = Vec::new();

    // Emit primary windows first in canonical order, then any extra/per-model windows sorted.
    let mut ordered_keys: Vec<&String> = Vec::with_capacity(obj.len());
    for primary in ["five_hour", "seven_day"] {
        if let Some((k, _)) = obj.get_key_value(primary) {
            ordered_keys.push(k);
        }
    }
    let mut rest: Vec<&String> = obj
        .keys()
        .filter(|k| !matches!(k.as_str(), "five_hour" | "seven_day" | "extra_usage"))
        .collect();
    rest.sort();
    ordered_keys.extend(rest);

    for key in ordered_keys {
        let Some(entry) = obj.get(key).and_then(Value::as_object) else { continue };
        let Some(utilization) = number(entry.get("utilization")) else { continue };
        let resets_at = entry.get("resets_at").and_then(parse_timestamp);
        snapshots.push(UsageSnapshot {
            account: account.into(),
            provider: "claude".into(),
            kind: "window".into(),
            window: claude_window_name(key),
            used_pct: Some(utilization),
            used_amount: None,
            limit_amount: None,
            currency: None,
            resets_at,
            source: "probe".into(),
            device_id: None,
            fetched_at,
        });
    }

    if let Some(extra) = obj.get("extra_usage").and_then(Value::as_object)
        && extra.get("is_enabled").and_then(Value::as_bool) == Some(true)
    {
        // Anthropic reports extra_usage credits in minor units (cents).
        let used_amount = number(extra.get("used_credits")).map(|cents| cents / 100.0);
        let limit_amount = number(extra.get("monthly_limit")).map(|cents| cents / 100.0);
        let used_pct = number(extra.get("utilization")).or_else(|| match (used_amount, limit_amount) {
            (Some(u), Some(l)) if l > 0.0 => Some(u / l * 100.0),
            _ => None,
        });
        if used_pct.is_some() || used_amount.is_some() || limit_amount.is_some() {
            snapshots.push(UsageSnapshot {
                account: account.into(),
                provider: "claude".into(),
                kind: "spend".into(),
                window: "extra".into(),
                used_pct,
                used_amount,
                limit_amount,
                currency: Some("USD".into()),
                resets_at: None,
                source: "probe".into(),
                device_id: None,
                fetched_at,
            });
        }
    }

    Ok(snapshots)
}

fn format_window_duration(secs: Option<u64>, fallback: &str) -> String {
    match secs {
        Some(18_000) => "5h".into(),
        Some(604_800) => "7d".into(),
        Some(s) if s > 0 && s.is_multiple_of(86_400) => format!("{}d", s / 86_400),
        Some(s) if s > 0 && s.is_multiple_of(3_600) => format!("{}h", s / 3_600),
        Some(s) if s > 0 && s.is_multiple_of(60) => format!("{}m", s / 60),
        _ => fallback.into(),
    }
}

fn push_codex_window(
    snapshots: &mut Vec<UsageSnapshot>,
    account: &str,
    window_obj: &Value,
    default_window: &str,
    suffix: Option<&str>,
    fetched_at: u64,
) {
    let Some(obj) = window_obj.as_object() else { return };
    let Some(used_pct) = number(obj.get("used_percent").or_else(|| obj.get("utilization"))) else { return };
    let limit_secs = obj.get("limit_window_seconds").and_then(Value::as_u64);
    let base = format_window_duration(limit_secs, default_window);
    let window = match suffix {
        Some(s) => format!("{base}:{s}"),
        None => base,
    };
    let resets_at = obj.get("reset_at").and_then(parse_timestamp).or_else(|| {
        obj.get("reset_after_seconds")
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64)))
            .map(|after| fetched_at.saturating_add(after))
    });
    snapshots.push(UsageSnapshot {
        account: account.into(),
        provider: "codex".into(),
        kind: "window".into(),
        window,
        used_pct: Some(used_pct),
        used_amount: None,
        limit_amount: None,
        currency: None,
        resets_at,
        source: "probe".into(),
        device_id: None,
        fetched_at,
    });
}

fn push_codex_rate_limit(
    snapshots: &mut Vec<UsageSnapshot>,
    account: &str,
    rate_limit: &Value,
    suffix: Option<&str>,
    fetched_at: u64,
) {
    if let Some(primary) = rate_limit.get("primary_window") {
        push_codex_window(snapshots, account, primary, "5h", suffix, fetched_at);
    }
    if let Some(secondary) = rate_limit.get("secondary_window") {
        push_codex_window(snapshots, account, secondary, "7d", suffix, fetched_at);
    }
}

/// Parse ChatGPT/Codex's `GET /backend-api/wham/usage` response into normalized [`UsageSnapshot`] rows.
pub fn parse_codex_usage(body: &Value, fetched_at: u64) -> Result<Vec<UsageSnapshot>, String> {
    let obj = body.as_object().ok_or_else(|| "Codex usage response is not a JSON object".to_string())?;
    let account = obj
        .get("plan_type")
        .and_then(Value::as_str)
        .and_then(sanitize_label)
        .map(|plan| format!("chatgpt-{plan}"))
        .unwrap_or_else(|| "chatgpt".into());

    let mut snapshots = Vec::new();
    if let Some(rl) = obj.get("rate_limit") {
        push_codex_rate_limit(&mut snapshots, &account, rl, None, fetched_at);
    }
    if let Some(rl) = obj.get("code_review_rate_limit") {
        push_codex_rate_limit(&mut snapshots, &account, rl, Some("code_review"), fetched_at);
    }
    if let Some(additional) = obj.get("additional_rate_limits").and_then(Value::as_array) {
        for item in additional {
            let Some(rl) = item.get("rate_limit") else { continue };
            let suffix = item
                .get("limit_name")
                .or_else(|| item.get("metered_feature"))
                .and_then(Value::as_str)
                .and_then(sanitize_slug);
            push_codex_rate_limit(&mut snapshots, &account, rl, suffix.as_deref(), fetched_at);
        }
    }
    if let Some(credits) = obj.get("credits").and_then(Value::as_object)
        && credits.get("has_credits").and_then(Value::as_bool) == Some(true)
        && credits.get("unlimited").and_then(Value::as_bool) != Some(true)
        && let Some(balance) = number(credits.get("balance"))
    {
        snapshots.push(UsageSnapshot {
            account,
            provider: "codex".into(),
            kind: "balance".into(),
            window: "credits".into(),
            used_pct: None,
            used_amount: Some(balance),
            limit_amount: None,
            currency: Some("credits".into()),
            resets_at: None,
            source: "probe".into(),
            device_id: None,
            fetched_at,
        });
    }

    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn temp_home(tag: &str) -> PathBuf {
        let mut rnd = [0u8; 4];
        getrandom::fill(&mut rnd).unwrap();
        let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("grouper-probe-{tag}-{hex}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_jwt_with_exp(exp: u64) -> String {
        let h = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{h}.{p}.sig")
    }

    #[test]
    fn parses_recorded_claude_usage_json() {
        let body = json!({
            "five_hour": {
                "utilization": 42.0,
                "resets_at": "2026-11-25T14:05:00.000000+00:00"
            },
            "seven_day": {
                "utilization": 18.5,
                "resets_at": "2026-11-30T00:00:00Z"
            },
            "seven_day_opus": {
                "utilization": 7.0,
                "resets_at": "2026-11-30T00:00:00Z"
            },
            "seven_day_sonnet": null,
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 5000,
                "used_credits": 1250,
                "utilization": 25.0
            }
        });
        let rows = parse_claude_usage(&body, "claude-max", 1_795_600_000).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].provider, "claude");
        assert_eq!(rows[0].account, "claude-max");
        assert_eq!(rows[0].kind, "window");
        assert_eq!(rows[0].window, "5h");
        assert_eq!(rows[0].used_pct, Some(42.0));
        assert_eq!(rows[0].resets_at, Some(1_795_615_500));
        assert_eq!(rows[0].source, "probe");
        assert_eq!(rows[0].device_id, None);

        assert_eq!(rows[1].window, "7d");
        assert_eq!(rows[1].used_pct, Some(18.5));
        assert_eq!(rows[2].window, "7d:opus");
        assert_eq!(rows[2].used_pct, Some(7.0));

        assert_eq!(rows[3].kind, "spend");
        assert_eq!(rows[3].window, "extra");
        assert_eq!(rows[3].used_amount, Some(12.5));
        assert_eq!(rows[3].limit_amount, Some(50.0));
        assert_eq!(rows[3].used_pct, Some(25.0));
        assert_eq!(rows[3].currency.as_deref(), Some("USD"));

        // Enterprise plans return empty / all-null windows.
        assert!(parse_claude_usage(&json!({}), "claude", 1_795_600_000).unwrap().is_empty());
    }

    #[test]
    fn parses_recorded_codex_usage_json() {
        let body = json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 15,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 7200,
                    "reset_at": 1_795_615_500
                },
                "secondary_window": {
                    "used_percent": 63.5,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 400000
                }
            },
            "additional_rate_limits": [
                {
                    "limit_name": "GPT-5.3-Codex-Spark",
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 8,
                            "limit_window_seconds": 18000,
                            "reset_at": 1_795_620_000
                        }
                    }
                }
            ],
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": "12.50"
            }
        });
        let rows = parse_codex_usage(&body, 1_795_600_000).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].provider, "codex");
        assert_eq!(rows[0].account, "chatgpt-pro");
        assert_eq!(rows[0].kind, "window");
        assert_eq!(rows[0].window, "5h");
        assert_eq!(rows[0].used_pct, Some(15.0));
        assert_eq!(rows[0].resets_at, Some(1_795_615_500));
        assert_eq!(rows[0].source, "probe");

        assert_eq!(rows[1].window, "7d");
        assert_eq!(rows[1].used_pct, Some(63.5));
        assert_eq!(rows[1].resets_at, Some(1_795_600_000 + 400_000));

        assert_eq!(rows[2].window, "5h:gpt-5-3-codex-spark");
        assert_eq!(rows[2].used_pct, Some(8.0));

        assert_eq!(rows[3].kind, "balance");
        assert_eq!(rows[3].window, "credits");
        assert_eq!(rows[3].used_amount, Some(12.5));
    }

    #[test]
    fn parses_rfc3339_and_unix_timestamps() {
        assert_eq!(parse_timestamp(&json!("2026-11-25T14:05:00Z")), Some(1_795_615_500));
        assert_eq!(parse_timestamp(&json!("2026-11-25T09:05:00.123456-05:00")), Some(1_795_615_500));
        assert_eq!(parse_timestamp(&json!("2026-11-25T16:05:00+02:00")), Some(1_795_615_500));
        assert_eq!(parse_timestamp(&json!(1_795_615_500)), Some(1_795_615_500));
        assert_eq!(parse_timestamp(&json!(1_795_615_500_000u64)), Some(1_795_615_500));
        assert_eq!(parse_timestamp(&json!("2026-02-29T00:00:00Z")), None);
        assert_eq!(parse_timestamp(&json!("not-a-date")), None);
    }

    #[test]
    fn reads_claude_and_codex_credentials_from_temp_home_without_leaking_secrets() {
        let home = temp_home("creds");
        let now = 2_000_000_000;

        // Missing files -> NotSignedIn.
        assert!(matches!(load_claude_credentials(Some(&home), now), CredLoad::NotSignedIn));
        assert!(matches!(load_codex_credentials(Some(&home), now), CredLoad::NotSignedIn));

        // Valid Claude credentials (expiresAt in ms).
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude").join(".credentials.json"),
            json!({
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-super-secret",
                    "expiresAt": (now + 3600) * 1000,
                    "subscriptionType": "max",
                    "accountUuid": "11111111-2222-3333-4444-555555555555"
                }
            })
            .to_string(),
        )
        .unwrap();
        let claude = match load_claude_credentials(Some(&home), now) {
            CredLoad::Ready(c) => c,
            other => panic!("expected Ready, got {other:?}"),
        };
        assert_eq!(claude.account_label, "claude-max");
        let dbg = format!("{claude:?}");
        assert!(!dbg.contains("super-secret"), "Debug must redact token: {dbg}");
        assert!(!claude.account_label.contains("11111111"), "label must not contain UUID");

        // Expired Claude credentials -> Expired.
        std::fs::write(
            home.join(".claude").join(".credentials.json"),
            json!({
                "claudeAiOauth": {
                    "accessToken": "sk-ant-oat01-old",
                    "expiresAt": (now - 10) * 1000
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(matches!(load_claude_credentials(Some(&home), now), CredLoad::Expired));

        // Codex API-key-only file -> NotSignedIn.
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("auth.json"),
            json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-test"}).to_string(),
        )
        .unwrap();
        assert!(matches!(load_codex_credentials(Some(&home), now), CredLoad::NotSignedIn));

        // Codex expired JWT -> Expired.
        std::fs::write(
            home.join(".codex").join("auth.json"),
            json!({
                "tokens": {
                    "access_token": fake_jwt_with_exp(now - 60),
                    "account_id": "acct-secret-id"
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(matches!(load_codex_credentials(Some(&home), now), CredLoad::Expired));

        // Codex valid JWT -> Ready, redacted Debug.
        let valid_jwt = fake_jwt_with_exp(now + 3600);
        std::fs::write(
            home.join(".codex").join("auth.json"),
            json!({
                "tokens": {
                    "access_token": valid_jwt,
                    "account_id": "acct-secret-id"
                }
            })
            .to_string(),
        )
        .unwrap();
        let codex = match load_codex_credentials(Some(&home), now) {
            CredLoad::Ready(c) => c,
            other => panic!("expected Ready, got {other:?}"),
        };
        assert_eq!(codex.account_id.as_deref(), Some("acct-secret-id"));
        let dbg = format!("{codex:?}");
        assert!(!dbg.contains(&valid_jwt) && !dbg.contains("acct-secret-id"), "Debug must redact secrets: {dbg}");

        std::fs::remove_dir_all(home).ok();
    }

    async fn spawn_mock_http(status: u16, extra_headers: &'static str, body: String) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = tx.send(req);
            let resp = format!(
                "HTTP/1.1 {status} TEST\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
        (format!("http://{addr}/usage"), rx)
    }

    #[tokio::test]
    async fn probes_send_expected_headers_and_handle_errors_and_redirects() {
        let home = temp_home("http");
        let now = 2_000_000_000;
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".claude").join(".credentials.json"),
            json!({"claudeAiOauth": {"accessToken": "tok-claude-123", "expiresAt": (now + 3600) * 1000, "subscriptionType": "pro"}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            home.join(".codex").join("auth.json"),
            json!({"tokens": {"access_token": fake_jwt_with_exp(now + 3600), "account_id": "acct-42"}}).to_string(),
        )
        .unwrap();

        let client = http_client().unwrap();

        // Claude 200 OK.
        let (url, req_rx) = spawn_mock_http(
            200,
            "",
            json!({"five_hour": {"utilization": 33.0, "resets_at": "2026-11-25T14:05:00Z"}}).to_string(),
        )
        .await;
        let entry = probe_claude_with(&client, &url, Some(&home), now).await;
        let req = req_rx.await.unwrap().to_ascii_lowercase();
        assert!(req.contains("authorization: bearer tok-claude-123"), "{req}");
        assert!(req.contains("anthropic-beta: oauth-2025-04-20"), "{req}");
        assert_eq!(entry.status, "ok");
        assert_eq!(entry.snapshots.len(), 1);
        assert_eq!(entry.snapshots[0].window, "5h");
        assert_eq!(entry.snapshots[0].used_pct, Some(33.0));

        // Codex 200 OK with ChatGPT-Account-Id.
        let (url, req_rx) = spawn_mock_http(
            200,
            "",
            json!({"plan_type": "plus", "rate_limit": {"primary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_at": now + 3600}}}).to_string(),
        )
        .await;
        let entry = probe_codex_with(&client, &url, Some(&home), now).await;
        let req = req_rx.await.unwrap().to_ascii_lowercase();
        assert!(req.contains("chatgpt-account-id: acct-42"), "{req}");
        assert_eq!(entry.status, "ok");
        assert_eq!(entry.snapshots[0].account, "chatgpt-plus");

        // HTTP 401 -> expired.
        let (url, _) = spawn_mock_http(401, "", "{}".into()).await;
        let entry = probe_claude_with(&client, &url, Some(&home), now).await;
        assert_eq!(entry.status, "expired");
        assert_eq!(entry.message.as_deref(), Some("expired"));

        // HTTP 403 -> unavailable: HTTP 403.
        let (url, _) = spawn_mock_http(403, "", "{}".into()).await;
        let entry = probe_codex_with(&client, &url, Some(&home), now).await;
        assert_eq!(entry.status, "unavailable");
        assert_eq!(entry.message.as_deref(), Some("unavailable: HTTP 403"));

        // HTTP 302 redirect -> NOT followed, reports unavailable: HTTP 302.
        let (url, _) = spawn_mock_http(302, "Location: http://127.0.0.1:9/steal\r\n", "".into()).await;
        let entry = probe_claude_with(&client, &url, Some(&home), now).await;
        assert_eq!(entry.status, "unavailable");
        assert_eq!(entry.message.as_deref(), Some("unavailable: HTTP 302"));

        std::fs::remove_dir_all(home).ok();
    }

    #[tokio::test]
    async fn probe_calls_inside_minimum_interval_return_the_cached_result() {
        let expected = (true, r#"{"results":[]}"#.to_string());
        let mut state = probe_state().lock().await;
        let previous = state.take();
        *state = Some(ProbeCache {
            completed_at: tokio::time::Instant::now(),
            result: expected.clone(),
        });
        drop(state);

        let actual = call(&json!({"providers": ["claude"]})).await;
        assert_eq!(actual, expected);

        let mut state = probe_state().lock().await;
        *state = previous;
    }
}
