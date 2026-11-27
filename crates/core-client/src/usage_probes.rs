//! Read-only subscription usage probes for Claude Code (Pro/Max), Codex (ChatGPT), Grok
//! (SuperGrok) and Antigravity.
//!
//! Exposed only as the unlisted internal MCP tool [`USAGE_PROBE_TOOL`] when a device
//! explicitly opts in (`Config::usage_probes` / `grouper connect --usage-probes`).
//! Never listed in `tools/list`, never added to the tool registry, and never offered
//! to threads.
//!
//! Security properties:
//! - Read-only: reads credentials the official CLIs already store on this device and
//!   never modifies or refreshes them.
//! - Hardcoded endpoints only (`api.anthropic.com`, `chatgpt.com`,
//!   `cli-chat-proxy.grok.com`) with HTTP redirects disabled so bearer tokens can never be
//!   forwarded elsewhere. Response bodies are size-capped and every request times out.
//! - Tokens and account IDs are never logged, never returned to the hub, and redacted
//!   from `Debug` output.
//! - Antigravity has no credential read at all: the installed `agy` CLI is run with fixed
//!   arguments (no shell, no stdin, an empty temporary working directory, capped output,
//!   a timeout) and answers `/usage` locally without spending quota.
//! - Which providers run is decided on this device (`Config::usage_probe_providers`); a
//!   hub asking for a provider switched off here gets nothing.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use core_protocol::{USAGE_PROBE_TOOL, UsageProbeEntry, UsageProbeResponse, UsageSnapshot};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;

pub const SUPPORTED_PROVIDERS: &[&str] = &["claude", "codex", "grok", "antigravity"];

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_BETA_HEADER: &str = "oauth-2025-04-20";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// Used for the Grok client headers when the installed CLI's version can't be read.
const GROK_FALLBACK_VERSION: &str = "1.0.77";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_PROBE_INTERVAL: Duration = Duration::from_secs(30);
/// No provider response body is read past this size.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
/// How long one `agy -p "/usage"` run may take before it is killed.
const AGY_TIMEOUT: Duration = Duration::from_secs(25);
/// `agy` output is read up to this many bytes; more is treated as a failure.
const AGY_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// The only arguments the Antigravity probe ever passes. `/usage` is answered locally by the
/// CLI and spends no quota.
const AGY_ARGS: [&str; 6] = ["-p", "/usage", "--output-format", "json", "--print-timeout", "30s"];
/// 2000-01-01T00:00:00Z. Antigravity reports the year 1970 for "no active window".
const YEAR_2000_SECS: u64 = 946_684_800;

struct ProbeCache {
    completed_at: tokio::time::Instant,
    /// The providers the cached result covers; a request for another set is not served from it.
    providers: Vec<&'static str>,
    result: (bool, String),
}

/// What `--usage-probes[=...]` / `GROUPER_USAGE_PROBES` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageProbeSetting {
    pub enabled: bool,
    /// `None`: every supported provider.
    pub providers: Option<Vec<String>>,
}

impl UsageProbeSetting {
    /// Parse a flag/env value: bare or truthy (`""`, `1`, `true`, `yes`, `on`, `all`) means every
    /// provider, falsy (`0`, `false`, `no`, `off`, `none`) means off, anything else is a
    /// comma-separated provider list such as `claude,grok`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let value = raw.trim().to_ascii_lowercase();
        match value.as_str() {
            "" | "1" | "true" | "yes" | "on" | "all" => return Ok(Self { enabled: true, providers: None }),
            "0" | "false" | "no" | "off" | "none" => return Ok(Self { enabled: false, providers: None }),
            _ => {}
        }
        let mut providers: Vec<String> = Vec::new();
        for name in value.split(',').map(str::trim) {
            if !SUPPORTED_PROVIDERS.contains(&name) {
                return Err(format!(
                    "unknown usage probe provider {name:?}; supported: {}",
                    SUPPORTED_PROVIDERS.join(", ")
                ));
            }
            if !providers.iter().any(|p| p == name) {
                providers.push(name.to_string());
            }
        }
        Ok(Self { enabled: true, providers: Some(providers) })
    }

    /// For `clap`'s `value_parser`.
    pub fn from_flag(raw: &str) -> Result<Self, String> {
        Self::parse(raw)
    }

    /// The "off" setting (`--no-usage-probes`).
    pub fn off() -> Self {
        Self { enabled: false, providers: None }
    }
}

impl Default for UsageProbeSetting {
    /// Subscription usage belongs to the whole account, like harness detection: on, for every
    /// provider, unless the device owner switches it (or a provider) off locally.
    fn default() -> Self {
        Self { enabled: true, providers: None }
    }
}

/// The providers this device advertises and will run: the supported ones that are switched on
/// (`selected`, `None` = all) **and** whose harness is actually present here (its CLI found by the
/// harness scan's lookup, or its sign-in file / keychain entry exists). An explicit list only ever
/// narrows this, so a device that doesn't use Grok never reports a "not signed in" Grok card.
pub fn enabled_providers(selected: Option<&[String]>) -> Vec<&'static str> {
    enabled_providers_with(selected, |p| provider_present(p, None, &|slug| crate::harness::locate_cli(slug).is_some()))
}

fn enabled_providers_with(selected: Option<&[String]>, present: impl Fn(&str) -> bool) -> Vec<&'static str> {
    SUPPORTED_PROVIDERS
        .iter()
        .copied()
        .filter(|p| selected.is_none_or(|list| list.iter().any(|s| s.trim().eq_ignore_ascii_case(p))))
        .filter(|p| present(p))
        .collect()
}

/// Is this provider's harness on the device? `home` overrides the home folder (tests);
/// `cli_found` answers whether the CLI for a harness slug is installed.
fn provider_present(provider: &str, home: Option<&Path>, cli_found: &dyn Fn(&str) -> bool) -> bool {
    let (slug, credential_file) = match provider {
        "claude" => ("claude-code", claude_credentials_path(home)),
        "codex" => ("codex", codex_credentials_path(home)),
        "grok" => ("grok", grok_credentials_path(home)),
        // Antigravity has no credential read: the CLI is the only thing to find.
        "antigravity" => ("antigravity", None),
        _ => return false,
    };
    if cli_found(slug) || credential_file.is_some_and(|p| p.is_file()) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if provider == "claude" && home.is_none() && read_claude_keychain_json().is_some() {
        return true;
    }
    false
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


/// The providers a `grouper_usage_probe` call will actually run: those named in `args`
/// (`{"providers": [...]}`; omitted or empty means all supported), limited to the ones switched
/// on locally. A hub cannot make this device read a provider's sign-in that its owner left off.
fn requested_providers(args: &Value, enabled: &[&'static str]) -> Vec<&'static str> {
    let mut list: Vec<&'static str> = Vec::new();
    if let Some(arr) = args.get("providers").and_then(Value::as_array) {
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
    }
    if list.is_empty() {
        list = SUPPORTED_PROVIDERS.to_vec();
    }
    list.retain(|p| enabled.contains(p));
    list
}

/// Run the internal `grouper_usage_probe` tool for the providers in `enabled`.
///
/// `args` may pass `{"providers": ["claude", "codex"]}`; omitted or empty probes every enabled
/// provider.
pub(crate) async fn call(args: &Value, enabled: &[&'static str]) -> (bool, String) {
    let requested = requested_providers(args, enabled);
    // Holding this mutex through the provider requests makes probes single-flight.
    // A completed result is reused briefly even if a hostile hub asks repeatedly.
    let mut state = probe_state().lock().await;
    if let Some(cached) = state.as_ref()
        && cached.providers == requested
        && cached.completed_at.elapsed() < MIN_PROBE_INTERVAL
    {
        return cached.result.clone();
    }
    let result = call_uncached(&requested).await;
    *state = Some(ProbeCache {
        completed_at: tokio::time::Instant::now(),
        providers: requested,
        result: result.clone(),
    });
    result
}

async fn call_uncached(requested: &[&'static str]) -> (bool, String) {
    let client = match http_client() {
        Ok(c) => c,
        Err(msg) => return (false, json!({"error": msg}).to_string()),
    };
    let fetched_at = now_secs();
    let grok_version = if requested.contains(&"grok") { grok_client_version().await } else { String::new() };
    let futs = requested.iter().copied().map(|provider| {
        let client = client.clone();
        let grok_version = grok_version.clone();
        async move {
            match provider {
                "claude" => probe_claude_with(&client, CLAUDE_USAGE_URL, None, fetched_at).await,
                "codex" => probe_codex_with(&client, CODEX_USAGE_URL, None, fetched_at).await,
                "grok" => probe_grok_with(&client, GROK_BILLING_URL, None, &grok_version, fetched_at).await,
                "antigravity" => probe_antigravity(fetched_at).await,
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

struct GrokCredentials {
    access_token: String,
    user_id: String,
}

impl std::fmt::Debug for GrokCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrokCredentials")
            .field("access_token", &"[redacted]")
            .field("user_id", &"[redacted]")
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

/// The Grok CLI's sign-in file: `~/.grok/auth.json` (`%USERPROFILE%\\.grok\\auth.json` on Windows).
fn grok_credentials_path(home: Option<&Path>) -> Option<PathBuf> {
    let home = home.map(Path::to_path_buf).or_else(dirs::home_dir)?;
    Some(home.join(".grok").join("auth.json"))
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

/// With several sign-ins, the one that expires last. Only entries for the x.ai issuer count.
fn select_grok_entry(root: &serde_json::Map<String, Value>) -> Option<&serde_json::Map<String, Value>> {
    let mut best: Option<(u64, &serde_json::Map<String, Value>)> = None;
    for (name, entry) in root {
        if !name.starts_with("https://auth.x.ai") {
            continue;
        }
        let Some(entry) = entry.as_object() else { continue };
        let has_key = entry.get("key").and_then(Value::as_str).is_some_and(|k| !k.trim().is_empty());
        if !has_key {
            continue;
        }
        let expires = entry.get("expires_at").and_then(parse_timestamp).unwrap_or(0);
        if best.is_none_or(|(e, _)| expires > e) {
            best = Some((expires, entry));
        }
    }
    best.map(|(_, entry)| entry)
}

fn load_grok_credentials(home: Option<&Path>, now: u64) -> CredLoad<GrokCredentials> {
    let Some(path) = grok_credentials_path(home) else {
        return CredLoad::NotSignedIn;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return CredLoad::NotSignedIn,
        Err(_) => return CredLoad::Unavailable("could not read Grok auth file".into()),
    };
    parse_grok_credentials_json(&text, now)
}

/// The Grok auth file is a JSON object with one entry per sign-in, keyed `https://auth.x.ai::<id>`;
/// each entry carries `key` (the access token), `user_id`, `auth_mode` and `expires_at` (RFC 3339).
/// Only those four fields are read. Entries for any other issuer are ignored so the token is never
/// sent anywhere it was not issued for. The refresh token is never touched.
fn parse_grok_credentials_json(text: &str, now: u64) -> CredLoad<GrokCredentials> {
    let root: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return CredLoad::Unavailable("invalid Grok auth JSON".into()),
    };
    let Some(root) = root.as_object() else {
        return CredLoad::Unavailable("invalid Grok auth JSON".into());
    };
    let Some(entry) = select_grok_entry(root) else {
        return CredLoad::NotSignedIn;
    };
    if let Some(mode) = entry.get("auth_mode").and_then(Value::as_str)
        && !mode.eq_ignore_ascii_case("oidc")
    {
        return CredLoad::Unavailable("unsupported Grok sign-in type".into());
    }
    if let Some(expires) = entry.get("expires_at").and_then(parse_timestamp)
        && expires <= now
    {
        return CredLoad::Expired;
    }
    let access_token = entry.get("key").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    let Some(user_id) = entry.get("user_id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
        return CredLoad::Unavailable("Grok sign-in has no user id".into());
    };
    CredLoad::Ready(GrokCredentials { access_token: access_token.to_string(), user_id: user_id.to_string() })
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

// ---- Subscription account fingerprints
//
// Each probe tags its rows with an opaque `account_key` so the hub can tell whether the same
// subscription account was read on two devices. It is the first 16 bytes of
// SHA-256("grouper-sub-account:" + provider + ":" + a stable account id the provider's own
// files already hold), hex-encoded. The raw id never leaves this module.

fn account_key(provider: &str, stable_id: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let id = stable_id.trim();
    if id.is_empty() || id.len() > 256 {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"grouper-sub-account:");
    hasher.update(provider.as_bytes());
    hasher.update(b":");
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    Some(digest.iter().take(16).map(|b| format!("{b:02x}")).collect())
}

fn stamp_account_key(snapshots: &mut [UsageSnapshot], key: Option<String>) {
    for snapshot in snapshots {
        snapshot.account_key = key.clone();
    }
}

/// `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`): Claude Code's global state, which
/// holds `oauthAccount`. Same file on macOS.
fn claude_state_path(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = home {
        return Some(dir.join(".claude.json"));
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir).join(".claude.json"));
    }
    dirs::home_dir().map(|h| h.join(".claude.json"))
}

/// Claude: `oauthAccount.accountUuid`, else `organizationUuid`. Only those two keys are read.
fn claude_account_key(home: Option<&Path>) -> Option<String> {
    let path = claude_state_path(home)?;
    if std::fs::metadata(&path).ok()?.len() > 32 * 1024 * 1024 {
        return None;
    }
    let root: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let oauth = root.get("oauthAccount")?.as_object()?;
    let id = ["accountUuid", "organizationUuid"]
        .iter()
        .find_map(|k| oauth.get(*k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()))?;
    account_key("claude", id)
}

/// Codex: `tokens.account_id`, else the `chatgpt_account_id` claim of the access/id token.
fn codex_account_key(home: Option<&Path>) -> Option<String> {
    let text = std::fs::read_to_string(codex_credentials_path(home)?).ok()?;
    let root: Value = serde_json::from_str(&text).ok()?;
    let tokens = root.get("tokens").and_then(Value::as_object);
    let get = |key: &str, alt: &str| -> Option<String> {
        tokens
            .and_then(|t| t.get(key).or_else(|| t.get(alt)))
            .or_else(|| root.get(key).or_else(|| root.get(alt)))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let id = get("account_id", "accountId").or_else(|| {
        ["access_token", "id_token"]
            .iter()
            .find_map(|k| get(k, k).and_then(|jwt| jwt_chatgpt_account_id(&jwt)))
    })?;
    account_key("codex", &id)
}

/// The `chatgpt_account_id` claim (under `https://api.openai.com/auth`) of a JWT; no other claim.
fn jwt_chatgpt_account_id(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

// Antigravity: no account key. `agy` keeps no account identifier in plain files on this device
// (its sign-in lives in the OS credential store; `installation_id` identifies the install, not
// the account), and `/usage` doesn't print one, so those rows carry `account_key: None`.

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

/// Read a response body as JSON, refusing anything over [`MAX_RESPONSE_BYTES`].
async fn read_json_capped(mut response: reqwest::Response) -> Result<Value, &'static str> {
    if response.content_length().is_some_and(|n| n > MAX_RESPONSE_BYTES as u64) {
        return Err("response too large");
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "invalid JSON response")? {
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err("response too large");
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|_| "invalid JSON response")
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
    let body = match read_json_capped(response).await {
        Ok(v) => v,
        Err(msg) => return status_entry("claude", "unavailable", format!("unavailable: {msg}")),
    };
    match parse_claude_usage(&body, &creds.account_label, fetched_at) {
        Ok(mut snapshots) if !snapshots.is_empty() => {
            stamp_account_key(&mut snapshots, claude_account_key(home));
            UsageProbeEntry {
            provider: "claude".into(),
            status: "ok".into(),
            message: None,
            snapshots,
            }
        }
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
    let body = match read_json_capped(response).await {
        Ok(v) => v,
        Err(msg) => return status_entry("codex", "unavailable", format!("unavailable: {msg}")),
    };
    match parse_codex_usage(&body, fetched_at) {
        Ok(mut snapshots) if !snapshots.is_empty() => {
            stamp_account_key(&mut snapshots, codex_account_key(home));
            UsageProbeEntry {
                provider: "codex".into(),
                status: "ok".into(),
                message: None,
                snapshots,
            }
        }
        Ok(_) => status_entry("codex", "unavailable", "unavailable: no usage windows in response"),
        Err(msg) => status_entry("codex", "unavailable", format!("unavailable: {msg}")),
    }
}

/// The version string for the Grok client headers: the installed CLI's, else a fallback.
/// Resolved once per process (it runs `grok --version`).
async fn grok_client_version() -> String {
    static VERSION: OnceLock<String> = OnceLock::new();
    if let Some(v) = VERSION.get() {
        return v.clone();
    }
    let detected = tokio::task::spawn_blocking(|| crate::harness::cli_version("grok")).await.ok().flatten();
    let version = detected.as_deref().and_then(clean_cli_version).unwrap_or_else(|| GROK_FALLBACK_VERSION.into());
    VERSION.get_or_init(|| version).clone()
}

/// The first version-looking token (`1.0.77`) in a `--version` line such as `grok 1.0.77 (50ca579)`.
/// Only plain `[0-9A-Za-z.+-]` tokens are accepted, so the result is always safe in a header.
fn clean_cli_version(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .map(|t| t.trim_start_matches('v'))
        .find(|t| {
            t.len() <= 32
                && t.contains('.')
                && t.starts_with(|c: char| c.is_ascii_digit())
                && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
        })
        .map(String::from)
}

async fn probe_grok_with(
    client: &reqwest::Client,
    url: &str,
    home: Option<&Path>,
    version: &str,
    fetched_at: u64,
) -> UsageProbeEntry {
    // An expired token returns here: no request is made, and the token is never renewed (that is
    // the Grok CLI's job).
    let creds = match load_grok_credentials(home, fetched_at) {
        CredLoad::Ready(c) => c,
        CredLoad::NotSignedIn => return status_entry("grok", "not_signed_in", "not signed in"),
        CredLoad::Expired => return status_entry("grok", "expired", "expired"),
        CredLoad::Unavailable(msg) => return status_entry("grok", "unavailable", format!("unavailable: {msg}")),
    };
    let version = clean_cli_version(version).unwrap_or_else(|| GROK_FALLBACK_VERSION.into());

    let mut user_id = match reqwest::header::HeaderValue::from_str(&creds.user_id) {
        Ok(v) => v,
        Err(_) => return status_entry("grok", "unavailable", "unavailable: Grok sign-in has an invalid user id"),
    };
    user_id.set_sensitive(true);
    // The proxy is behind Cloudflare, which rejects requests without a User-Agent (error 1010).
    let request = client
        .get(url)
        .bearer_auth(&creds.access_token)
        .header("x-userid", user_id)
        .header("x-grok-client-version", &version)
        .header("x-grok-client-mode", "cli")
        .header(reqwest::header::USER_AGENT, format!("grok-cli/{version}"))
        .header("accept", "application/json");
    let response = match request.send().await {
        Ok(r) => r,
        Err(err) => {
            return status_entry("grok", "unavailable", format!("unavailable: {}", classify_http_error(&err)));
        }
    };

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return status_entry("grok", "expired", "expired");
    }
    if !status.is_success() {
        return status_entry("grok", "unavailable", format!("unavailable: HTTP {}", status.as_u16()));
    }
    if status != reqwest::StatusCode::OK {
        return status_entry("grok", "unavailable", format!("unavailable: HTTP {}", status.as_u16()));
    }
    let body = match read_json_capped(response).await {
        Ok(v) => v,
        Err(msg) => return status_entry("grok", "unavailable", format!("unavailable: {msg}")),
    };
    match parse_grok_billing(&body, fetched_at) {
        Ok(mut snapshots) if !snapshots.is_empty() => {
            stamp_account_key(&mut snapshots, account_key("grok", &creds.user_id));
            UsageProbeEntry {
                provider: "grok".into(),
                status: "ok".into(),
                message: None,
                snapshots,
            }
        }
        Ok(_) => status_entry("grok", "unavailable", "unavailable: no usage windows in response"),
        Err(msg) => status_entry("grok", "unavailable", format!("unavailable: {msg}")),
    }
}

async fn probe_antigravity(fetched_at: u64) -> UsageProbeEntry {
    let program = tokio::task::spawn_blocking(|| crate::harness::locate_cli("antigravity")).await.ok().flatten();
    let Some(program) = program else {
        return status_entry("antigravity", "unavailable", "unavailable: Antigravity CLI not found");
    };
    probe_antigravity_with(&program, AGY_TIMEOUT, fetched_at).await
}

async fn probe_antigravity_with(program: &Path, timeout: Duration, fetched_at: u64) -> UsageProbeEntry {
    let output = match run_agy(program, timeout).await {
        Ok(out) => out,
        Err(AgyError::NotFound) => {
            return status_entry("antigravity", "unavailable", "unavailable: Antigravity CLI not found");
        }
        Err(AgyError::Timeout) => {
            return status_entry("antigravity", "unavailable", "unavailable: Antigravity CLI timed out");
        }
        Err(AgyError::TooLarge) => {
            return status_entry("antigravity", "unavailable", "unavailable: Antigravity CLI output too large");
        }
        Err(AgyError::Failed) => {
            return status_entry("antigravity", "unavailable", "unavailable: could not run the Antigravity CLI");
        }
    };
    match output.code {
        Some(0) => {}
        // `agy` exits 2 for `/usage` when signed in with an API key: there are no plan windows.
        Some(2) => {
            return status_entry(
                "antigravity",
                "unavailable",
                "unavailable: signed in with an API key, so it has no plan windows",
            );
        }
        _ => {
            return status_entry("antigravity", "unavailable", "unavailable: Antigravity CLI exited with an error");
        }
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(body) = parse_json_document(&text) else {
        return status_entry("antigravity", "unavailable", "unavailable: invalid JSON output");
    };
    match parse_antigravity_usage(&body, fetched_at) {
        Ok(snapshots) if !snapshots.is_empty() => UsageProbeEntry {
            provider: "antigravity".into(),
            status: "ok".into(),
            message: None,
            snapshots,
        },
        Ok(_) => status_entry("antigravity", "unavailable", "unavailable: no usage windows in output"),
        Err(msg) => status_entry("antigravity", "unavailable", format!("unavailable: {msg}")),
    }
}

struct AgyOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
}

enum AgyError {
    NotFound,
    Timeout,
    TooLarge,
    Failed,
}

/// An empty temporary folder to run `agy` in, removed on drop.
struct EmptyTempDir(PathBuf);

impl EmptyTempDir {
    fn create() -> std::io::Result<Self> {
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).map_err(|_| std::io::Error::other("no randomness"))?;
        let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("grouper-agy-{hex}"));
        std::fs::create_dir(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for EmptyTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `<program> -p "/usage" --output-format json --print-timeout 30s`: fixed arguments, no shell,
/// no stdin, an empty temporary working directory, stdout capped at [`AGY_MAX_OUTPUT_BYTES`],
/// killed after `timeout`.
async fn run_agy(program: &Path, timeout: Duration) -> Result<AgyOutput, AgyError> {
    let workdir = EmptyTempDir::create().map_err(|_| AgyError::Failed)?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(AGY_ARGS)
        .current_dir(&workdir.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().to_ascii_uppercase().starts_with("GROUPER_") {
            cmd.env_remove(key);
        }
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { AgyError::NotFound } else { AgyError::Failed }
    })?;
    let mut stdout = child.stdout.take().ok_or(AgyError::Failed)?;
    let run = async {
        let mut buf: Vec<u8> = Vec::new();
        (&mut stdout)
            .take(AGY_MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .await
            .map_err(|_| AgyError::Failed)?;
        if buf.len() > AGY_MAX_OUTPUT_BYTES {
            return Err(AgyError::TooLarge);
        }
        let status = child.wait().await.map_err(|_| AgyError::Failed)?;
        Ok(AgyOutput { code: status.code(), stdout: buf })
    };
    // Dropping `child` on any early exit kills it (`kill_on_drop`).
    let result = tokio::time::timeout(timeout, run).await.unwrap_or(Err(AgyError::Timeout));
    drop(child);
    drop(workdir);
    result
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
            label: None,
            account_key: None,
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
                label: None,
                account_key: None,
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
        label: None,
        account_key: None,
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
            label: None,
            account_key: None,
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

/// A provider-supplied display name: control characters dropped, at most 64 bytes.
fn clean_display_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.trim().chars().filter(|c| !c.is_control()) {
        if out.len() + ch.len_utf8() > 64 {
            break;
        }
        out.push(ch);
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// `GrokBuild` -> `Grok Build`, `grok_build` -> `Grok build`.
fn split_product_words(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for ch in raw.trim().chars() {
        if ch == '_' || ch == '-' {
            if !out.ends_with(' ') && !out.is_empty() {
                out.push(' ');
            }
        } else {
            if ch.is_uppercase() && prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push(ch);
        }
        prev = Some(ch);
    }
    clean_display_name(&out)
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Parse Grok's `GET /v1/billing?format=credits` response into normalized [`UsageSnapshot`] rows.
///
/// The response also carries `onDemandCap`, `onDemandUsed` and `prepaidBalance` (`{"val": int}`).
/// Their unit is not confirmed (credits or currency minor units), so they are deliberately NOT
/// emitted; wrong-unit money on a meter would be worse than none. Revisit once the unit is known.
pub fn parse_grok_billing(body: &Value, fetched_at: u64) -> Result<Vec<UsageSnapshot>, String> {
    let config = body
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(|| "Grok billing response has no config".to_string())?;
    let period = config
        .get("currentPeriod")
        .and_then(Value::as_object)
        .ok_or_else(|| "Grok billing response has no current period".to_string())?;
    let span = match period.get("type").and_then(Value::as_str) {
        Some("USAGE_PERIOD_TYPE_WEEKLY") => "7d",
        Some("USAGE_PERIOD_TYPE_MONTHLY") => "30d",
        Some("USAGE_PERIOD_TYPE_DAILY") => "1d",
        _ => return Err("unsupported Grok usage period".into()),
    };
    let overall = number(config.get("creditUsagePercent")).ok_or_else(|| "Grok billing response has no credit usage".to_string())?;
    let resets_at = period.get("end").and_then(parse_timestamp);
    let row = |window: String, label: Option<String>, used_pct: f64| UsageSnapshot {
        account: "supergrok".into(),
        provider: "grok".into(),
        kind: "window".into(),
        window,
        label,
        account_key: None,
        used_pct: Some(used_pct.max(0.0)),
        used_amount: None,
        limit_amount: None,
        currency: None,
        resets_at,
        source: "probe".into(),
        device_id: None,
        fetched_at,
    };

    let mut products: Vec<(String, Option<String>, f64)> = Vec::new();
    if let Some(list) = config.get("productUsage").and_then(Value::as_array) {
        for item in list {
            let Some(name) = item.get("product").and_then(Value::as_str) else { continue };
            let Some(id) = sanitize_slug(name) else { continue };
            let Some(pct) = number(item.get("usagePercent")) else { continue };
            if products.iter().any(|(existing, _, _)| *existing == id) {
                continue;
            }
            products.push((id, split_product_words(name), pct));
        }
    }
    let mut snapshots = vec![row(span.to_string(), None, overall)];
    // A single product that just repeats the overall figure adds nothing.
    let redundant = products.len() == 1 && (products[0].2 - overall).abs() < 0.05;
    if !redundant {
        for (id, label, pct) in products {
            snapshots.push(row(format!("{span}:{id}"), label, pct));
        }
    }
    Ok(snapshots)
}

/// The JSON document in `agy`'s stdout: the whole output, else the last line that parses.
fn parse_json_document(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return Some(v);
    }
    text.lines().rev().map(str::trim).filter(|l| l.starts_with('{')).find_map(|l| serde_json::from_str(l).ok())
}

/// `gemini-weekly` -> `gemini`, `3p-5h` -> `third_party`, otherwise the sanitised id prefix.
fn antigravity_scope(bucket_id: &str) -> Option<String> {
    let id = bucket_id.trim().to_ascii_lowercase();
    if id.starts_with("gemini-") {
        return Some("gemini".into());
    }
    if id.starts_with("3p-") {
        return Some("third_party".into());
    }
    let prefix = id.rsplit_once('-').map_or(id.as_str(), |(p, _)| p);
    sanitize_slug(prefix)
}

/// Parse the stdout JSON of `agy -p "/usage" --output-format json` into [`UsageSnapshot`] rows.
///
/// Antigravity reports the share *remaining*; every row here is *used*. A `reset_time` before the
/// year 2000 (Antigravity sends 1970) means "no active window" and becomes `resets_at: None`.
/// The group `description` is not parsed.
pub fn parse_antigravity_usage(body: &Value, fetched_at: u64) -> Result<Vec<UsageSnapshot>, String> {
    let groups = body
        .get("command")
        .and_then(|c| c.get("data"))
        .and_then(|d| d.get("groups"))
        .and_then(Value::as_array)
        .ok_or_else(|| "Antigravity usage output has no groups".to_string())?;
    let mut snapshots: Vec<UsageSnapshot> = Vec::new();
    for group in groups {
        let Some(group) = group.as_object() else { continue };
        let label = group.get("name").and_then(Value::as_str).and_then(clean_display_name);
        let Some(buckets) = group.get("buckets").and_then(Value::as_array) else { continue };
        for bucket in buckets {
            let Some(bucket) = bucket.as_object() else { continue };
            let span = match bucket.get("window").and_then(Value::as_str) {
                Some("5h") => "5h",
                Some("weekly") => "7d",
                _ => continue,
            };
            let Some(scope) = bucket.get("id").and_then(Value::as_str).and_then(antigravity_scope) else { continue };
            let Some(remaining) = number(bucket.get("remaining_fraction")) else { continue };
            let window = format!("{span}:{scope}");
            if snapshots.iter().any(|s| s.window == window) {
                continue;
            }
            let resets_at = bucket.get("reset_time").and_then(parse_timestamp).filter(|t| *t >= YEAR_2000_SECS);
            snapshots.push(UsageSnapshot {
                account: "antigravity".into(),
                provider: "antigravity".into(),
                kind: "window".into(),
                window,
                label: label.clone(),
                account_key: None,
                used_pct: Some(round2((1.0 - remaining.clamp(0.0, 1.0)) * 100.0)),
                used_amount: None,
                limit_amount: None,
                currency: None,
                resets_at,
                source: "probe".into(),
                device_id: None,
                fetched_at,
            });
        }
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
            providers: vec!["claude"],
            result: expected.clone(),
        });
        drop(state);

        let actual = call(&json!({"providers": ["claude"]}), &["claude", "codex"]).await;
        assert_eq!(actual, expected);

        // A request for a provider switched off locally runs nothing, and is not answered from
        // the cache of a different provider set.
        let none = call(&json!({"providers": ["grok"]}), &["claude", "codex"]).await;
        assert_eq!(none, (true, r#"{"results":[]}"#.to_string()));

        let mut state = probe_state().lock().await;
        *state = previous;
    }

    // ---- Grok and Antigravity

    const GROK_TOKEN: &str = "grok-secret-token-abc";
    const GROK_USER_ID: &str = "user-secret-id-123";

    fn grok_fixture() -> Value {
        json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-11-22T17:32:23.062750+00:00",
                    "end": "2026-11-29T17:32:23.062750+00:00"
                },
                "creditUsagePercent": 100.0,
                "productUsage": [{"product": "GrokBuild", "usagePercent": 100.0}],
                "onDemandCap": {"val": 0},
                "onDemandUsed": {"val": 0},
                "prepaidBalance": {"val": 0}
            }
        })
    }

    fn antigravity_fixture() -> Value {
        json!({
            "command": {
                "name": "usage",
                "data": {
                    "groups": [
                        {
                            "name": "Gemini Models",
                            "description": "Gemini 3 family",
                            "buckets": [
                                {"id": "gemini-weekly", "name": "Weekly", "window": "weekly",
                                 "remaining_fraction": 0.00377, "reset_time": "2026-12-03T11:22:12Z"},
                                {"id": "gemini-5h", "name": "5 hour", "window": "5h",
                                 "remaining_fraction": 0.9904, "reset_time": "2026-11-27T16:22:12Z"}
                            ]
                        },
                        {
                            "name": "Claude and GPT models",
                            "description": "Third-party models",
                            "buckets": [
                                {"id": "3p-weekly", "name": "Weekly", "window": "weekly",
                                 "remaining_fraction": 0.5, "reset_time": "2026-12-01T00:00:00Z"},
                                {"id": "3p-5h", "name": "5 hour", "window": "5h",
                                 "remaining_fraction": 1.0, "reset_time": "1970-01-01T00:00:00Z"}
                            ]
                        }
                    ]
                }
            }
        })
    }

    fn write_grok_auth(home: &Path, expires_at: &str) {
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        std::fs::write(
            home.join(".grok").join("auth.json"),
            json!({
                "https://auth.x.ai::00000000-0000-0000-0000-000000000000": {
                    "key": GROK_TOKEN,
                    "auth_mode": "oidc",
                    "user_id": GROK_USER_ID,
                    "email": "fake@example.invalid",
                    "refresh_token": "grok-refresh-secret",
                    "expires_at": expires_at,
                    "oidc_issuer": "https://auth.x.ai",
                    "oidc_client_id": "client",
                    "team_id": "team"
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn parses_recorded_grok_billing_json() {
        let rows = parse_grok_billing(&grok_fixture(), 1_795_600_000).unwrap();
        // One product whose percent equals the overall figure adds no row.
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!((row.provider.as_str(), row.account.as_str(), row.kind.as_str()), ("grok", "supergrok", "window"));
        assert_eq!(row.window, "7d");
        assert_eq!(row.label, None);
        assert_eq!(row.used_pct, Some(100.0));
        assert_eq!(row.resets_at, parse_timestamp(&json!("2026-11-29T17:32:23Z")));
        assert_eq!(row.source, "probe");
        // The on-demand and prepaid amounts have no confirmed unit and are never emitted.
        assert!(rows.iter().all(|r| r.used_amount.is_none() && r.limit_amount.is_none() && r.kind == "window"));

        // Several products: one extra row each, with the product name split into words.
        let mut multi = grok_fixture();
        multi["config"]["creditUsagePercent"] = json!(40.0);
        multi["config"]["productUsage"] = json!([
            {"product": "GrokBuild", "usagePercent": 25.5},
            {"product": "GrokChat", "usagePercent": 14.5}
        ]);
        let rows = parse_grok_billing(&multi, 1).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[1].window.as_str(), rows[1].label.as_deref(), rows[1].used_pct), ("7d:grokbuild", Some("Grok Build"), Some(25.5)));
        assert_eq!((rows[2].window.as_str(), rows[2].label.as_deref()), ("7d:grokchat", Some("Grok Chat")));

        // A single product that differs from the overall figure is kept.
        let mut single = grok_fixture();
        single["config"]["creditUsagePercent"] = json!(60.0);
        assert_eq!(parse_grok_billing(&single, 1).unwrap().len(), 2);

        // Period types map to spans; anything else is unavailable, never a guess.
        for (ty, span) in [("USAGE_PERIOD_TYPE_MONTHLY", "30d"), ("USAGE_PERIOD_TYPE_DAILY", "1d")] {
            let mut f = grok_fixture();
            f["config"]["currentPeriod"]["type"] = json!(ty);
            assert_eq!(parse_grok_billing(&f, 1).unwrap()[0].window, span);
        }
        let mut f = grok_fixture();
        f["config"]["currentPeriod"]["type"] = json!("USAGE_PERIOD_TYPE_YEARLY");
        assert!(parse_grok_billing(&f, 1).is_err());

        // Malformed input.
        for bad in [json!(null), json!([]), json!("x"), json!({}), json!({"config": {}}), json!({"config": {"currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY"}}}),
            json!({"config": {"currentPeriod": {"type": "USAGE_PERIOD_TYPE_WEEKLY"}, "creditUsagePercent": "lots"}})]
        {
            assert!(parse_grok_billing(&bad, 1).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_recorded_antigravity_usage_json() {
        let rows = parse_antigravity_usage(&antigravity_fixture(), 1_795_600_000).unwrap();
        assert_eq!(rows.len(), 4);
        let by = |w: &str| rows.iter().find(|r| r.window == w).unwrap_or_else(|| panic!("no {w}"));
        for r in &rows {
            assert_eq!((r.provider.as_str(), r.account.as_str(), r.kind.as_str(), r.source.as_str()), ("antigravity", "antigravity", "window", "probe"));
        }
        let gw = by("7d:gemini");
        assert_eq!(gw.label.as_deref(), Some("Gemini Models"));
        assert_eq!(gw.used_pct, Some(99.62));
        assert_eq!(gw.resets_at, parse_timestamp(&json!("2026-12-03T11:22:12Z")));
        assert_eq!(by("5h:gemini").used_pct, Some(0.96));
        let tw = by("7d:third_party");
        assert_eq!(tw.label.as_deref(), Some("Claude and GPT models"));
        assert_eq!(tw.used_pct, Some(50.0));
        // "No active window": nothing used, and the 1970 reset time becomes no reset time.
        let t5 = by("5h:third_party");
        assert_eq!((t5.used_pct, t5.resets_at), (Some(0.0), None));

        // Unknown windows are skipped, an unknown id prefix is sanitised, and bad buckets drop out.
        let body = json!({"command": {"data": {"groups": [{"name": "Other", "buckets": [
            {"id": "Foo Bar-weekly", "window": "weekly", "remaining_fraction": 0.25, "reset_time": "2026-12-03T11:22:12Z"},
            {"id": "foo-monthly", "window": "monthly", "remaining_fraction": 0.5},
            {"id": "foo-5h", "window": "5h"},
            {"window": "5h", "remaining_fraction": 0.5},
            "junk"
        ]}, "junk"]}}});
        let rows = parse_antigravity_usage(&body, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].window.as_str(), rows[0].used_pct), ("7d:foo-bar", Some(75.0)));

        // Malformed input.
        for bad in [json!(null), json!([]), json!({}), json!({"command": {}}), json!({"command": {"data": {"groups": "x"}}})] {
            assert!(parse_antigravity_usage(&bad, 1).is_err(), "{bad}");
        }
        assert!(parse_antigravity_usage(&json!({"command": {"data": {"groups": []}}}), 1).unwrap().is_empty());
        assert!(parse_json_document("not json").is_none());
        assert!(parse_json_document("warning: x\n{\"command\": {}}\n").is_some());
    }

    #[test]
    fn reads_grok_credentials_without_leaking_secrets() {
        let home = temp_home("grok-creds");
        let now = parse_timestamp(&json!("2026-11-27T11:00:00Z")).unwrap();
        assert!(matches!(load_grok_credentials(Some(&home), now), CredLoad::NotSignedIn));

        write_grok_auth(&home, "2026-11-27T12:00:00.000000+00:00");
        let creds = match load_grok_credentials(Some(&home), now) {
            CredLoad::Ready(c) => c,
            other => panic!("expected Ready, got {other:?}"),
        };
        assert_eq!((creds.access_token.as_str(), creds.user_id.as_str()), (GROK_TOKEN, GROK_USER_ID));
        let dbg = format!("{creds:?}");
        for secret in [GROK_TOKEN, GROK_USER_ID, "grok-refresh-secret", "fake@example.invalid"] {
            assert!(!dbg.contains(secret), "Debug must redact {secret}: {dbg}");
        }
        // The CredLoad wrapper's Debug is redacted too.
        assert!(!format!("{:?}", CredLoad::Ready(creds)).contains(GROK_TOKEN));

        // Past its expiry: expired (the probe never renews it).
        assert!(matches!(load_grok_credentials(Some(&home), now + 7200), CredLoad::Expired));
        // Not an OIDC sign-in, no user id, other issuers, broken JSON.
        let write = |v: Value| std::fs::write(home.join(".grok").join("auth.json"), v.to_string()).unwrap();
        write(json!({"https://auth.x.ai::1": {"key": "k", "user_id": "u", "auth_mode": "api_key"}}));
        assert!(matches!(load_grok_credentials(Some(&home), now), CredLoad::Unavailable(_)));
        write(json!({"https://auth.x.ai::1": {"key": "k"}}));
        assert!(matches!(load_grok_credentials(Some(&home), now), CredLoad::Unavailable(_)));
        write(json!({"https://evil.example::1": {"key": "k", "user_id": "u"}}));
        assert!(matches!(load_grok_credentials(Some(&home), now), CredLoad::NotSignedIn));
        write(json!([]));
        assert!(matches!(load_grok_credentials(Some(&home), now), CredLoad::Unavailable(_)));
        std::fs::write(home.join(".grok").join("auth.json"), "{nope").unwrap();
        assert!(matches!(load_grok_credentials(Some(&home), now), CredLoad::Unavailable(_)));
        std::fs::remove_dir_all(home).ok();
    }

    #[tokio::test]
    async fn grok_probe_sends_expected_headers_handles_errors_and_never_follows_redirects() {
        let home = temp_home("grok-http");
        let now = 1_795_000_000;
        write_grok_auth(&home, "2099-01-01T00:00:00Z");
        let client = http_client().unwrap();

        // 200: headers, rows and account key.
        let (url, req_rx) = spawn_mock_http(200, "", grok_fixture().to_string()).await;
        let entry = probe_grok_with(&client, &url, Some(&home), "9.9.9", now).await;
        let req = req_rx.await.unwrap().to_ascii_lowercase();
        for header in [
            format!("authorization: bearer {GROK_TOKEN}"),
            format!("x-userid: {GROK_USER_ID}"),
            "x-grok-client-version: 9.9.9".to_string(),
            "x-grok-client-mode: cli".to_string(),
            "user-agent: grok-cli/9.9.9".to_string(),
        ] {
            assert!(req.contains(&header), "missing {header}: {req}");
        }
        assert_eq!(entry.status, "ok");
        assert_eq!(entry.snapshots.len(), 1);
        assert_eq!(entry.snapshots[0].used_pct, Some(100.0));
        assert_eq!(entry.snapshots[0].account_key, account_key("grok", GROK_USER_ID));
        // Nothing sensitive in what would go to the hub.
        let wire = serde_json::to_string(&entry).unwrap();
        for secret in [GROK_TOKEN, GROK_USER_ID, "fake@example.invalid"] {
            assert!(!wire.contains(secret), "{wire}");
        }

        // 401 and 403 -> expired; other codes and shapes -> unavailable.
        for code in [401, 403] {
            let (url, _) = spawn_mock_http(code, "", "{}".into()).await;
            let entry = probe_grok_with(&client, &url, Some(&home), "9.9.9", now).await;
            assert_eq!((entry.status.as_str(), entry.message.as_deref()), ("expired", Some("expired")));
        }
        let (url, _) = spawn_mock_http(500, "", "{}".into()).await;
        let entry = probe_grok_with(&client, &url, Some(&home), "9.9.9", now).await;
        assert_eq!((entry.status.as_str(), entry.message.as_deref()), ("unavailable", Some("unavailable: HTTP 500")));
        let (url, _) = spawn_mock_http(200, "", json!({"config": {"currentPeriod": {"type": "USAGE_PERIOD_TYPE_YEARLY"}, "creditUsagePercent": 1}}).to_string()).await;
        let entry = probe_grok_with(&client, &url, Some(&home), "9.9.9", now).await;
        assert_eq!(entry.status, "unavailable");
        let (url, _) = spawn_mock_http(200, "", "not json".into()).await;
        assert_eq!(probe_grok_with(&client, &url, Some(&home), "9.9.9", now).await.status, "unavailable");
        // A response over the size cap is refused.
        let (url, _) = spawn_mock_http(200, "", "x".repeat(MAX_RESPONSE_BYTES + 1024)).await;
        let entry = probe_grok_with(&client, &url, Some(&home), "9.9.9", now).await;
        assert_eq!((entry.status.as_str(), entry.message.as_deref()), ("unavailable", Some("unavailable: response too large")));

        // A redirect is NOT followed: the "attacker" listener never sees a connection.
        let (steal_url, mut steal_rx) = spawn_mock_http(200, "", "{}".into()).await;
        let location: &'static str = Box::leak(format!("Location: {steal_url}\r\n").into_boxed_str());
        let (url, _) = spawn_mock_http(302, location, "".into()).await;
        let entry = probe_grok_with(&client, &url, Some(&home), "9.9.9", now).await;
        assert_eq!((entry.status.as_str(), entry.message.as_deref()), ("unavailable", Some("unavailable: HTTP 302")));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(steal_rx.try_recv().is_err(), "redirect target must never be contacted");

        std::fs::remove_dir_all(home).ok();
    }

    #[tokio::test]
    async fn expired_grok_token_is_reported_without_any_network_call() {
        let home = temp_home("grok-expired");
        write_grok_auth(&home, "2026-01-01T00:00:00Z");
        let (url, mut req_rx) = spawn_mock_http(200, "", grok_fixture().to_string()).await;
        let entry = probe_grok_with(&http_client().unwrap(), &url, Some(&home), "9.9.9", 1_795_000_000).await;
        assert_eq!((entry.status.as_str(), entry.message.as_deref()), ("expired", Some("expired")));
        assert!(entry.snapshots.is_empty());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(req_rx.try_recv().is_err(), "no request may be made with an expired token");

        // Missing file -> not signed in, also without a request.
        let empty = temp_home("grok-none");
        let entry = probe_grok_with(&http_client().unwrap(), &url, Some(&empty), "9.9.9", 1_795_000_000).await;
        assert_eq!(entry.status, "not_signed_in");
        assert!(req_rx.try_recv().is_err());
        std::fs::remove_dir_all(home).ok();
        std::fs::remove_dir_all(empty).ok();
    }

    #[test]
    fn account_keys_are_opaque_stable_and_per_provider() {
        let key = account_key("grok", "user-1").unwrap();
        assert_eq!(key, "342e992978c3c1d52281ac1a99d47fe8");
        assert_eq!(key.len(), 32);
        assert!(!key.contains("user-1"));
        assert_ne!(account_key("claude", "user-1").unwrap(), key);
        assert_eq!(account_key("grok", "  user-1 ").unwrap(), key);
        assert_eq!(account_key("grok", ""), None);
    }

    #[test]
    fn claude_and_codex_account_keys_come_from_their_local_files() {
        let home = temp_home("acct-keys");
        assert_eq!(claude_account_key(Some(&home)), None);
        assert_eq!(codex_account_key(Some(&home)), None);

        std::fs::write(
            home.join(".claude.json"),
            json!({"oauthAccount": {"accountUuid": "acct-uuid-1", "organizationUuid": "org-uuid-1", "emailAddress": "x@example.invalid"}, "projects": {}}).to_string(),
        )
        .unwrap();
        assert_eq!(claude_account_key(Some(&home)), account_key("claude", "acct-uuid-1"));
        // Falls back to the organization id.
        std::fs::write(home.join(".claude.json"), json!({"oauthAccount": {"organizationUuid": "org-uuid-1"}}).to_string()).unwrap();
        assert_eq!(claude_account_key(Some(&home)), account_key("claude", "org-uuid-1"));
        std::fs::write(home.join(".claude.json"), json!({"oauthAccount": {}}).to_string()).unwrap();
        assert_eq!(claude_account_key(Some(&home)), None);

        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex").join("auth.json"), json!({"tokens": {"access_token": "t", "account_id": "chatgpt-acct-1"}}).to_string()).unwrap();
        assert_eq!(codex_account_key(Some(&home)), account_key("codex", "chatgpt-acct-1"));
        // No account_id: the JWT's chatgpt_account_id claim.
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"exp": 1, "https://api.openai.com/auth": {"chatgpt_account_id": "chatgpt-acct-2"}}).to_string());
        let jwt = format!("h.{payload}.s");
        std::fs::write(home.join(".codex").join("auth.json"), json!({"tokens": {"access_token": jwt}}).to_string()).unwrap();
        assert_eq!(codex_account_key(Some(&home)), account_key("codex", "chatgpt-acct-2"));
        std::fs::remove_dir_all(home).ok();
    }

    #[tokio::test]
    async fn claude_and_codex_rows_carry_the_account_key() {
        let home = temp_home("acct-http");
        let now = 2_000_000_000;
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".claude").join(".credentials.json"),
            json!({"claudeAiOauth": {"accessToken": "tok", "expiresAt": (now + 3600) * 1000}}).to_string(),
        )
        .unwrap();
        std::fs::write(home.join(".claude.json"), json!({"oauthAccount": {"accountUuid": "acct-uuid-1"}}).to_string()).unwrap();
        std::fs::write(
            home.join(".codex").join("auth.json"),
            json!({"tokens": {"access_token": fake_jwt_with_exp(now + 3600), "account_id": "chatgpt-acct-1"}}).to_string(),
        )
        .unwrap();
        let client = http_client().unwrap();
        let (url, _) = spawn_mock_http(200, "", json!({"five_hour": {"utilization": 1.0}, "extra_usage": {"is_enabled": true, "used_credits": 100, "monthly_limit": 1000}}).to_string()).await;
        let entry = probe_claude_with(&client, &url, Some(&home), now).await;
        assert!(entry.snapshots.len() == 2 && entry.snapshots.iter().all(|s| s.account_key == account_key("claude", "acct-uuid-1")));
        let (url, _) = spawn_mock_http(200, "", json!({"rate_limit": {"primary_window": {"used_percent": 1, "limit_window_seconds": 18000}}}).to_string()).await;
        let entry = probe_codex_with(&client, &url, Some(&home), now).await;
        assert!(entry.snapshots.iter().all(|s| s.account_key == account_key("codex", "chatgpt-acct-1")));
        std::fs::remove_dir_all(home).ok();
    }

    /// A fake `agy`: checks it was started with exactly the fixed arguments and an empty working
    /// folder, then prints `stdout_file` and exits with `code` (`sleep`: wait instead).
    fn fake_agy(dir: &Path, stdout_file: &Path, code: i32, sleep: bool) -> PathBuf {
        #[cfg(windows)]
        {
            let path = dir.join("fake-agy.cmd");
            let wait = if sleep { "ping -n 4 127.0.0.1 >nul\r\n" } else { "" };
            std::fs::write(
                &path,
                format!(
                    "@echo off\r\nif not \"%~1\"==\"-p\" exit /b 9\r\nif not \"%~2\"==\"/usage\" exit /b 9\r\nif not \"%~3\"==\"--output-format\" exit /b 9\r\nif not \"%~4\"==\"json\" exit /b 9\r\nif not \"%~5\"==\"--print-timeout\" exit /b 9\r\nif not \"%~6\"==\"30s\" exit /b 9\r\nif not \"%~7\"==\"\" exit /b 9\r\nfor /f %%i in ('dir /b') do exit /b 8\r\n{wait}type \"{}\"\r\nexit /b {code}\r\n",
                    stdout_file.display()
                ),
            )
            .unwrap();
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join("fake-agy.sh");
            let wait = if sleep { "sleep 3\n" } else { "" };
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\n[ \"$1\" = \"-p\" ] && [ \"$2\" = \"/usage\" ] && [ \"$3\" = \"--output-format\" ] && [ \"$4\" = \"json\" ] && [ \"$5\" = \"--print-timeout\" ] && [ \"$6\" = \"30s\" ] && [ -z \"$7\" ] || exit 9\n[ -z \"$(ls -A .)\" ] || exit 8\n{wait}cat '{}'\nexit {code}\n",
                    stdout_file.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    #[tokio::test]
    async fn antigravity_probe_runs_the_cli_and_maps_exit_codes() {
        let dir = temp_home("agy");
        let good = dir.join("good.json");
        std::fs::write(&good, antigravity_fixture().to_string()).unwrap();
        let now = 1_795_600_000;
        let secs = Duration::from_secs(20);

        // Success: fixed args and an empty working folder are enforced by the fake CLI itself.
        let entry = probe_antigravity_with(&fake_agy(&dir, &good, 0, false), secs, now).await;
        assert_eq!(entry.status, "ok", "{:?}", entry.message);
        assert_eq!(entry.snapshots.len(), 4);
        assert_eq!(entry.snapshots[0].account_key, None);

        // Exit code 2: signed in with an API key.
        let entry = probe_antigravity_with(&fake_agy(&dir, &good, 2, false), secs, now).await;
        assert_eq!(entry.status, "unavailable");
        assert_eq!(entry.message.as_deref(), Some("unavailable: signed in with an API key, so it has no plan windows"));
        assert!(entry.snapshots.is_empty());

        // Any other failure, bad output, nothing to run.
        let entry = probe_antigravity_with(&fake_agy(&dir, &good, 1, false), secs, now).await;
        assert_eq!(entry.message.as_deref(), Some("unavailable: Antigravity CLI exited with an error"));
        let junk = dir.join("junk.txt");
        std::fs::write(&junk, "not json at all").unwrap();
        let entry = probe_antigravity_with(&fake_agy(&dir, &junk, 0, false), secs, now).await;
        assert_eq!(entry.message.as_deref(), Some("unavailable: invalid JSON output"));
        let entry = probe_antigravity_with(&dir.join("does-not-exist"), secs, now).await;
        assert_eq!((entry.status.as_str(), entry.message.as_deref()), ("unavailable", Some("unavailable: Antigravity CLI not found")));

        // Output over 64 KB is refused.
        let big = dir.join("big.txt");
        std::fs::write(&big, "x".repeat(AGY_MAX_OUTPUT_BYTES + 1000)).unwrap();
        let entry = probe_antigravity_with(&fake_agy(&dir, &big, 0, false), secs, now).await;
        assert_eq!(entry.message.as_deref(), Some("unavailable: Antigravity CLI output too large"));

        // A hung CLI is cut off.
        let entry = probe_antigravity_with(&fake_agy(&dir, &good, 0, true), Duration::from_millis(500), now).await;
        assert_eq!(entry.message.as_deref(), Some("unavailable: Antigravity CLI timed out"));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn usage_probe_setting_parses_flag_values() {
        let all = UsageProbeSetting { enabled: true, providers: None };
        for v in ["", "all", "1", "true", "ON"] {
            assert_eq!(UsageProbeSetting::parse(v).unwrap(), all, "{v}");
        }
        assert_eq!(UsageProbeSetting::default(), all);
        for v in ["0", "false", "off", "none"] {
            assert_eq!(UsageProbeSetting::parse(v).unwrap(), UsageProbeSetting::off(), "{v}");
        }
        let list = UsageProbeSetting::parse("claude, GROK,claude,antigravity").unwrap();
        assert_eq!(list, UsageProbeSetting { enabled: true, providers: Some(vec!["claude".into(), "grok".into(), "antigravity".into()]) });
        assert!(UsageProbeSetting::parse("claude,bogus").unwrap_err().contains("bogus"));
        assert!(UsageProbeSetting::parse("claude,").is_err());
    }

    #[test]
    fn only_providers_switched_on_here_are_advertised_and_run() {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let everything = |_: &str| true;
        assert_eq!(enabled_providers_with(None, everything), vec!["claude", "codex", "grok", "antigravity"]);
        // Only providers present on the device are advertised, with or without an explicit list.
        let only = |present: &'static [&'static str]| move |p: &str| present.contains(&p);
        assert_eq!(enabled_providers_with(None, only(&["claude", "grok"])), vec!["claude", "grok"]);
        assert!(enabled_providers_with(None, only(&[])).is_empty());
        assert_eq!(enabled_providers_with(Some(&strs(&["grok", "antigravity"])), only(&["grok"])), vec!["grok"]);
        assert!(enabled_providers_with(Some(&strs(&["codex"])), only(&["grok"])).is_empty());
        assert_eq!(enabled_providers_with(Some(&strs(&["codex", "grok"])), everything), vec!["codex", "grok"]);

        // The hub can only narrow, never widen, what is switched on.
        let enabled = ["claude", "grok"];
        assert_eq!(requested_providers(&json!({}), &enabled), vec!["claude", "grok"]);
        assert_eq!(requested_providers(&json!({"providers": ["codex", "Grok"]}), &enabled), vec!["grok"]);
        assert!(requested_providers(&json!({"providers": ["codex"]}), &enabled).is_empty());
        assert_eq!(requested_providers(&json!({"providers": ["nonsense"]}), &enabled), vec!["claude", "grok"]);
    }

    #[test]
    fn cleans_cli_versions_and_display_names() {
        assert_eq!(clean_cli_version("grok 1.0.77 (50ca579fa996)").as_deref(), Some("1.0.77"));
        assert_eq!(clean_cli_version("v2.3.4-beta.1").as_deref(), Some("2.3.4-beta.1"));
        assert_eq!(clean_cli_version("grok (dev)"), None);
        assert_eq!(clean_cli_version("1.2\r\nx-evil: 1"), Some("1.2".into()));
        assert_eq!(clean_cli_version("9.9\u{0}.1"), None);
        assert_eq!(split_product_words("GrokBuild").as_deref(), Some("Grok Build"));
        assert_eq!(split_product_words("grok_build").as_deref(), Some("grok build"));
        assert_eq!(clean_display_name(&"é".repeat(100)).unwrap().len(), 64);
        assert_eq!(clean_display_name("  \u{7} "), None);
    }

    /// Manual check on a real device: `cargo test -p core-client live_probes -- --ignored --nocapture`.
    /// Prints only statuses, percentages and reset times. Grok is one real request (read-only).
    #[tokio::test]
    #[ignore = "uses this device's real Grok sign-in and Antigravity CLI"]
    async fn live_probes_print_percentages_only() {
        let now = now_secs();
        let mut entries = vec![probe_antigravity(now).await];
        entries.push(probe_grok_with(&http_client().unwrap(), GROK_BILLING_URL, None, &grok_client_version().await, now).await);
        for entry in entries {
            println!("{}: {} {}", entry.provider, entry.status, entry.message.as_deref().unwrap_or(""));
            for r in &entry.snapshots {
                println!(
                    "  {} label={:?} used={:?}% resets_at={:?} account_key_set={}",
                    r.window,
                    r.label,
                    r.used_pct,
                    r.resets_at,
                    r.account_key.is_some()
                );
            }
        }
    }

    #[test]
    fn a_provider_counts_as_present_by_its_cli_or_its_sign_in_file() {
        let home = temp_home("present");
        let no_cli = |_: &str| false;
        for p in SUPPORTED_PROVIDERS {
            assert!(!provider_present(p, Some(&home), &no_cli), "{p} must be absent in an empty home");
        }
        // A CLI found by the harness scan counts, under the harness's own slug.
        assert!(provider_present("claude", Some(&home), &|slug| slug == "claude-code"));
        assert!(!provider_present("claude", Some(&home), &|slug| slug == "claude"));
        assert!(provider_present("antigravity", Some(&home), &|slug| slug == "antigravity"));
        // Sign-in files count without the CLI.
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        std::fs::write(home.join(".grok").join("auth.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex").join("auth.json"), "{}").unwrap();
        assert!(provider_present("grok", Some(&home), &no_cli));
        assert!(provider_present("codex", Some(&home), &no_cli));
        assert!(!provider_present("claude", Some(&home), &no_cli));
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude").join(".credentials.json"), "{}").unwrap();
        assert!(provider_present("claude", Some(&home), &no_cli));
        // Antigravity is found by its CLI alone.
        assert!(!provider_present("antigravity", Some(&home), &no_cli));
        assert!(!provider_present("nonsense", Some(&home), &|_| true));
        std::fs::remove_dir_all(home).ok();
    }
}
