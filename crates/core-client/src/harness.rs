//! Local CLI harness discovery and sign-in state inspection (`claude`, `codex`, `agy`,
//! `grok`, `hermes`, `pi`, `opencode`).
//!
//! Security properties:
//! - Credential checks inspect file presence and expiry timestamps only; tokens, API keys,
//!   and account identifiers never leave this module and are never logged or serialized.
//! - Reuses [`crate::usage_probes`] for Claude Code and Codex credential parsing.
//! - Any subprocess invocations (`--version`, `models`) run with `GROUPER_*` variables
//!   stripped, null stdin, and a short timeout.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use core_protocol::{
    DetectedHarness, HarnessCapabilities, HarnessModel, HarnessSignIn, HarnessUsage, NodeMessage,
};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::usage_probes;

/// How often a connected node re-checks installed harnesses and credential expiry.
const SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// Maximum time a local CLI `--version` or `models` command may run during a scan.
const CLI_CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum stdout bytes read from a CLI listing command.
const MAX_CLI_STDOUT_BYTES: usize = 64 * 1024;

/// Maximum time a single headless harness turn may run before timing out.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);

/// Maximum stdout/stderr bytes retained from a single harness turn.
const MAX_TURN_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// Confinement level for [`turn`].
///
/// Standard chats pass [`HarnessLevel::Confined`] (the most restricted mode each CLI
/// supports, inside an empty per-chat folder).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HarnessLevel {
    /// Most restricted mode the agent offers (`plan`, `read-only`, `--no-tools`).
    #[default]
    Confined,
    /// Confined to editing inside `workdir` where supported (`workspace-write`, `acceptEdits`, `accept-edits`).
    Workspace,
    /// Full tool permissions without interactive prompts (`danger-full-access`, `bypassPermissions`, `--dangerously-skip-permissions`).
    Bypass,
}

/// Output of one headless CLI harness turn from [`turn`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnResult {
    pub text: String,
    pub session_id: String,
    pub usage: HarnessUsage,
}

/// Supported harness specifications in canonical order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarnessDef {
    /// Canonical Grouper harness slug (`claude-code`, `codex`, …).
    pub slug: &'static str,
    pub display_name: &'static str,
    /// Executable base name (`claude`, `codex`, `agy`, …).
    pub binary: &'static str,
    pub login_command: &'static str,
    /// Supports confining built-in shell/edit/web tools so only `mcp__grouper__*` can run.
    pub can_confine_tools: bool,
    pub supports_resume: bool,
    pub supports_mcp: bool,
}

impl HarnessDef {
    pub const fn capabilities(self) -> HarnessCapabilities {
        HarnessCapabilities {
            // Orchestration requires tool confinement, continuous session resume, and MCP.
            can_orchestrate: self.can_confine_tools && self.supports_resume && self.supports_mcp,
            supports_resume: self.supports_resume,
            supports_mcp: self.supports_mcp,
        }
    }
}

pub const HARNESS_DEFS: &[HarnessDef] = &[
    HarnessDef {
        slug: "claude-code",
        display_name: "Claude Code",
        binary: "claude",
        login_command: "claude login",
        // VERIFY: Claude Code `--allowedTools mcp__grouper__*` / `--disallowedTools` confinement flags.
        can_confine_tools: true,
        supports_resume: true,
        supports_mcp: true,
    },
    HarnessDef {
        slug: "codex",
        display_name: "Codex",
        binary: "codex",
        login_command: "codex login",
        // VERIFY: Codex can sandbox read-only, not strip built-in tools yet.
        can_confine_tools: false,
        supports_resume: true,
        supports_mcp: true,
    },
    HarnessDef {
        slug: "antigravity",
        display_name: "Antigravity",
        binary: "agy",
        // VERIFY: Antigravity CLI login subcommand.
        login_command: "agy auth login",
        can_confine_tools: false,
        // VERIFY: Antigravity `--conversation` resume and MCP flags.
        supports_resume: true,
        supports_mcp: true,
    },
    HarnessDef {
        slug: "grok",
        display_name: "Grok",
        binary: "grok",
        // VERIFY: Grok CLI auth subcommand, `--resume <sessionId>` session resume, and MCP support.
        login_command: "grok auth",
        can_confine_tools: false,
        supports_resume: true,
        supports_mcp: false,
    },
    HarnessDef {
        slug: "hermes",
        display_name: "Hermes",
        binary: "hermes",
        // VERIFY: Hermes CLI login subcommand.
        login_command: "hermes login",
        can_confine_tools: false,
        supports_resume: true,
        supports_mcp: true,
    },
    HarnessDef {
        slug: "pi",
        display_name: "pi",
        binary: "pi",
        // VERIFY: pi coding agent login command and session resume flags.
        login_command: "pi /login",
        can_confine_tools: false,
        supports_resume: true,
        supports_mcp: false,
    },
    HarnessDef {
        slug: "opencode",
        display_name: "OpenCode",
        binary: "opencode",
        // VERIFY: OpenCode CLI login subcommand, resume flag, and MCP support.
        login_command: "opencode auth login",
        can_confine_tools: false,
        supports_resume: true,
        supports_mcp: true,
    },
];

/// Options controlling where [`scan_with`] looks for binaries, caches, and credential files.
///
/// Using an explicit `home`, `path_dirs`, and `include_system_dirs: false` isolates tests
/// completely from the host machine's real `$HOME`, `$PATH`, and credentials.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub home: Option<PathBuf>,
    pub path_dirs: Vec<PathBuf>,
    pub include_system_dirs: bool,
    pub run_commands: bool,
    pub now: u64,
}

impl ScanOptions {
    /// Default options for scanning the live host device.
    pub fn host() -> Self {
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).filter(|d| !d.as_os_str().is_empty()).collect())
            .unwrap_or_default();
        Self {
            home: dirs::home_dir(),
            path_dirs,
            include_system_dirs: true,
            run_commands: true,
            now: now_secs(),
        }
    }

    /// Isolated options rooted at a fake temp home directory (never touches host paths or tokens).
    pub fn isolated(home: impl Into<PathBuf>, path_dirs: Vec<PathBuf>, now: u64) -> Self {
        Self {
            home: Some(home.into()),
            path_dirs,
            include_system_dirs: false,
            run_commands: false,
            now,
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Scan this device for installed CLI harnesses, sign-in states, capabilities, and models.
pub fn scan() -> Vec<DetectedHarness> {
    scan_with(&ScanOptions::host())
}

/// Scan using explicit [`ScanOptions`].
pub fn scan_with(opts: &ScanOptions) -> Vec<DetectedHarness> {
    let mut detected = Vec::new();
    for def in HARNESS_DEFS {
        let Some(bin_path) = find_harness_binary(def, opts) else {
            continue;
        };
        let sign_in = check_sign_in(def, opts, &bin_path);
        let version = if opts.run_commands { detect_version(&bin_path) } else { None };
        let models = if sign_in.is_signed_in() {
            discover_models(def, opts, &bin_path)
        } else {
            Vec::new()
        };
        let login_command = (!sign_in.is_signed_in()).then(|| def.login_command.to_string());
        detected.push(DetectedHarness {
            harness: def.slug.to_string(),
            display_name: def.display_name.to_string(),
            version,
            sign_in,
            login_command,
            capabilities: def.capabilities(),
            models,
        });
    }
    detected
}

/// Background connection task that sends an initial [`NodeMessage::HarnessReport`] on connect
/// and re-sends whenever the scan output changes.
pub(crate) struct HarnessSession {
    task: JoinHandle<()>,
}

impl Drop for HarnessSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HarnessSession {
    pub(crate) fn start(
        mut mcp_changes: watch::Receiver<u64>,
        out: mpsc::Sender<NodeMessage>,
        on_overflow: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        mcp_changes.borrow_and_update();
        let task = tokio::spawn(async move {
            let mut last_sent: Option<Vec<DetectedHarness>> = None;
            let mut ticker = tokio::time::interval(SCAN_INTERVAL);
            // Consume the immediate first tick so the loop below runs once right away, then every SCAN_INTERVAL.
            ticker.tick().await;

            loop {
                let current = match tokio::task::spawn_blocking(scan).await {
                    Ok(list) => list,
                    Err(e) => {
                        tracing::warn!("harness scan task failed: {e}");
                        Vec::new()
                    }
                };

                if last_sent.as_ref() != Some(&current) {
                    let msg = NodeMessage::HarnessReport { harnesses: current.clone() };
                    match out.try_send(msg) {
                        Ok(()) => {
                            last_sent = Some(current);
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            on_overflow();
                            break;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }

                tokio::select! {
                    _ = ticker.tick() => {}
                    changed = mcp_changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self { task }
    }
}

// ---- Binary discovery

fn candidate_binary_names(binary: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            format!("{binary}.exe"),
            format!("{binary}.cmd"),
            format!("{binary}.bat"),
            binary.to_string(),
        ]
    } else {
        vec![binary.to_string()]
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn find_in_dirs(dirs: &[PathBuf], names: &[String]) -> Option<PathBuf> {
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Usual per-user and system install directories for each harness CLI.
fn usual_install_dirs(def: &HarnessDef, opts: &ScanOptions) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = opts.home.as_deref() {
        // Common user-local CLI install prefixes.
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join("bin"));
        dirs.push(home.join(".npm-global").join("bin"));
        dirs.push(home.join(".volta").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join(".cargo").join("bin"));
        dirs.push(home.join("AppData").join("Roaming").join("npm"));
        dirs.push(home.join("AppData").join("Local").join("Microsoft").join("WinGet").join("Links"));

        // VERIFY: Per-vendor install directories under $HOME.
        match def.slug {
            "claude-code" => {
                dirs.push(home.join(".claude").join("local"));
                dirs.push(home.join(".claude").join("bin"));
                dirs.push(home.join("AppData").join("Local").join("Programs").join("claude"));
            }
            "codex" => {
                dirs.push(home.join(".codex").join("bin"));
            }
            "antigravity" => {
                dirs.push(home.join(".antigravity").join("bin"));
                dirs.push(home.join(".gemini").join("antigravity-cli").join("bin"));
                dirs.push(home.join("AppData").join("Local").join("Programs").join("Antigravity").join("bin"));
            }
            "grok" => {
                dirs.push(home.join(".grok").join("bin"));
            }
            "hermes" => {
                dirs.push(home.join(".hermes").join("hermes-agent").join("venv").join("bin"));
                dirs.push(home.join(".hermes").join("hermes-agent").join("venv").join("Scripts"));
                dirs.push(home.join(".hermes").join("bin"));
            }
            "pi" => {
                dirs.push(home.join(".pi").join("bin"));
            }
            "opencode" => {
                dirs.push(home.join(".opencode").join("bin"));
                dirs.push(home.join("go").join("bin"));
            }
            _ => {}
        }
    }

    if opts.include_system_dirs {
        #[cfg(windows)]
        {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                dirs.push(PathBuf::from(appdata).join("npm"));
            }
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                let local = PathBuf::from(local);
                dirs.push(local.join("Microsoft").join("WinGet").join("Links"));
                dirs.push(local.join("Programs").join("Antigravity").join("bin"));
                dirs.push(local.join("Programs").join("claude"));
            }
            dirs.push(PathBuf::from(r"C:\Program Files\nodejs"));
        }
        #[cfg(not(windows))]
        {
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
            dirs.push(PathBuf::from("/usr/bin"));
            dirs.push(PathBuf::from("/snap/bin"));
            if def.slug == "antigravity" {
                dirs.push(PathBuf::from("/Applications/Antigravity.app/Contents/Resources/app/bin"));
            }
        }
    }

    dirs
}

fn find_harness_binary(def: &HarnessDef, opts: &ScanOptions) -> Option<PathBuf> {
    let names = candidate_binary_names(def.binary);
    find_in_dirs(&opts.path_dirs, &names)
        .or_else(|| find_in_dirs(&usual_install_dirs(def, opts), &names))
}

// ---- Sign-in state checks (presence + expiry only, zero token retention)

fn check_sign_in(def: &HarnessDef, opts: &ScanOptions, bin_path: &Path) -> HarnessSignIn {
    // When `include_system_dirs` is false (isolated/temp-home scans), require `home` so we
    // never fall back to host environment variables or the macOS Keychain.
    let Some(home) = opts.home.as_deref() else {
        if !opts.include_system_dirs {
            return HarnessSignIn::NotSignedIn;
        }
        return match def.slug {
            "claude-code" => usage_probes::claude_sign_in_state(None, opts.now),
            "codex" => usage_probes::codex_sign_in_state(None, opts.now),
            _ => HarnessSignIn::NotSignedIn,
        };
    };

    match def.slug {
        "claude-code" => {
            // When scanning the real host (`include_system_dirs`), pass `None` so `CLAUDE_CONFIG_DIR`
            // and macOS Keychain are respected just like `usage_probes.rs`.
            if opts.include_system_dirs {
                usage_probes::claude_sign_in_state(None, opts.now)
            } else {
                usage_probes::claude_sign_in_state(Some(home), opts.now)
            }
        }
        "codex" => {
            let state = if opts.include_system_dirs {
                usage_probes::codex_sign_in_state(None, opts.now)
            } else {
                usage_probes::codex_sign_in_state(Some(home), opts.now)
            };
            if state != HarnessSignIn::NotSignedIn {
                return state;
            }
            // VERIFY: Codex CLI `codex login --api-key` stores `OPENAI_API_KEY` in `auth.json`.
            let auth_path = if opts.include_system_dirs
                && let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty())
            {
                PathBuf::from(dir).join("auth.json")
            } else {
                home.join(".codex").join("auth.json")
            };
            check_json_credential_files(&[auth_path], opts.now)
        }
        "antigravity" => {
            // VERIFY: Antigravity credential file locations and expiry fields.
            let files = [
                home.join(".gemini").join("antigravity-cli").join("auth.json"),
                home.join(".gemini").join("oauth_creds.json"),
                home.join(".antigravity").join("auth.json"),
                home.join(".antigravity").join("credentials.json"),
                home.join(".config").join("antigravity").join("auth.json"),
            ];
            let state = check_json_credential_files(&files, opts.now);
            if state != HarnessSignIn::NotSignedIn || !opts.run_commands {
                return state;
            }
            // VERIFY: Fallback `agy models` exit status check when credentials live in OS keychain.
            if run_cli_stdout(bin_path, &["models"]).is_some() {
                HarnessSignIn::SignedIn
            } else {
                HarnessSignIn::NotSignedIn
            }
        }
        "grok" => {
            // VERIFY: Grok CLI credential/settings file paths and schema.
            let files = [
                home.join(".grok").join("credentials.json"),
                home.join(".grok").join("auth.json"),
                home.join(".grok").join("user-settings.json"),
                home.join(".config").join("grok").join("credentials.json"),
                home.join(".config").join("grok").join("config.json"),
            ];
            let state = check_json_credential_files(&files, opts.now);
            if state != HarnessSignIn::NotSignedIn || !opts.run_commands {
                return state;
            }
            // VERIFY: `grok models` output check when authenticated via environment/keychain.
            match run_cli_stdout(bin_path, &["models"]) {
                Some(out) if !looks_unauthenticated(&out) => HarnessSignIn::SignedIn,
                _ => HarnessSignIn::NotSignedIn,
            }
        }
        "hermes" => {
            // VERIFY: Hermes credential files, `.env`, and `provider_models_cache.json`.
            let cred_files = [
                home.join(".hermes").join("auth.json"),
                home.join(".hermes").join("credentials.json"),
            ];
            let state = check_json_credential_files(&cred_files, opts.now);
            if state != HarnessSignIn::NotSignedIn {
                return state;
            }
            if has_dotenv_api_key(&home.join(".hermes").join(".env"))
                || has_nonempty_json_object(&home.join(".hermes").join("provider_models_cache.json"))
            {
                HarnessSignIn::SignedIn
            } else {
                HarnessSignIn::NotSignedIn
            }
        }
        "pi" => {
            // VERIFY: pi coding agent `auth.json` location (`~/.pi/agent/auth.json`) and schema.
            let files = [
                home.join(".pi").join("agent").join("auth.json"),
                home.join(".pi").join("auth.json"),
                home.join(".config").join("pi").join("auth.json"),
            ];
            check_json_credential_files(&files, opts.now)
        }
        "opencode" => {
            // VERIFY: OpenCode `auth.json` locations (`~/.local/share/opencode/auth.json`) and schema.
            let files = [
                home.join(".local").join("share").join("opencode").join("auth.json"),
                home.join("AppData").join("Local").join("opencode").join("auth.json"),
                home.join(".opencode").join("auth.json"),
                home.join(".config").join("opencode").join("auth.json"),
            ];
            check_json_credential_files(&files, opts.now)
        }
        _ => HarnessSignIn::NotSignedIn,
    }
}

const CREDENTIAL_KEYS: &[&str] = &[
    "access_token",
    "accessToken",
    "access",
    "api_key",
    "apiKey",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "XAI_API_KEY",
    "GEMINI_API_KEY",
    "key",
    "token",
    "id_token",
    "oauth_token",
];

const EXPIRY_KEYS: &[&str] = &[
    "expires_at",
    "expiresAt",
    "expiry_date",
    "expires",
    "expiry",
    "exp",
];

/// Inspect candidate JSON credential files for credential presence and expiry without
/// keeping or returning any secret string.
fn check_json_credential_files(paths: &[PathBuf], now: u64) -> HarnessSignIn {
    let mut saw_expired = false;
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(root) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // Drop the raw file contents right away; `inspect_cred_object` only returns booleans/enum.
        drop(text);
        match inspect_cred_value(&root, now) {
            HarnessSignIn::SignedIn => return HarnessSignIn::SignedIn,
            HarnessSignIn::Expired => saw_expired = true,
            HarnessSignIn::NotSignedIn => {}
        }
    }
    if saw_expired { HarnessSignIn::Expired } else { HarnessSignIn::NotSignedIn }
}

fn inspect_cred_value(value: &Value, now: u64) -> HarnessSignIn {
    let Some(obj) = value.as_object() else {
        return HarnessSignIn::NotSignedIn;
    };

    // Direct credential object (`{"access_token": "...", "expires_at": ...}`).
    let direct_token = CREDENTIAL_KEYS.iter().find_map(|k| {
        obj.get(*k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
    });
    if let Some(tok) = direct_token {
        let explicit_exp = EXPIRY_KEYS.iter().find_map(|k| obj.get(*k).and_then(usage_probes::parse_timestamp));
        let exp = explicit_exp.or_else(|| usage_probes::jwt_exp_secs(tok));
        if let Some(exp_secs) = exp
            && exp_secs <= now
        {
            return HarnessSignIn::Expired;
        }
        return HarnessSignIn::SignedIn;
    }

    // Nested provider/section maps (e.g. `tokens`, `oauth`, `anthropic`, `openai`, `google`).
    let mut saw_expired = false;
    for child in obj.values() {
        if child.is_object() {
            match inspect_cred_value(child, now) {
                HarnessSignIn::SignedIn => return HarnessSignIn::SignedIn,
                HarnessSignIn::Expired => saw_expired = true,
                HarnessSignIn::NotSignedIn => {}
            }
        }
    }
    if saw_expired { HarnessSignIn::Expired } else { HarnessSignIn::NotSignedIn }
}

/// Check whether a `.env` file sets at least one non-empty `*_API_KEY` or `*_TOKEN` line,
/// without keeping the value.
fn has_dotenv_api_key(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return false;
        }
        let line = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((key, val)) = line.split_once('=') else {
            return false;
        };
        let key_upper = key.trim().to_ascii_uppercase();
        let val_clean = val.trim().trim_matches('"').trim_matches('\'').trim();
        !val_clean.is_empty() && (key_upper.ends_with("_API_KEY") || key_upper.ends_with("_TOKEN"))
    })
}

fn has_nonempty_json_object(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.as_object().map(|o| !o.is_empty()))
        .unwrap_or(false)
}

fn looks_unauthenticated(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("not authenticated")
        || lower.contains("not signed in")
        || lower.contains("not logged in")
        || lower.contains("please login")
        || lower.contains("please sign in")
}

// ---- Subprocess helpers (bounded timeout, GROUPER_* stripped)

fn sanitized_command(program: &Path, args: &[&str]) -> Command {
    let mut cmd = if cfg!(windows)
        && program
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "cmd" | "bat"))
    {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(program);
        c
    } else {
        Command::new(program)
    };
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().to_ascii_uppercase().starts_with("GROUPER_") {
            cmd.env_remove(k);
        }
    }
    cmd
}

fn run_cli_stdout(program: &Path, args: &[&str]) -> Option<String> {
    let mut child = sanitized_command(program, args).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(MAX_CLI_STDOUT_BYTES as u64).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + CLI_CMD_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break None,
        }
    }?;
    if !status.success() {
        return None;
    }
    let bytes = reader.join().ok()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn detect_version(bin_path: &Path) -> Option<String> {
    let raw = run_cli_stdout(bin_path, &["--version"])?;
    let first_line = raw.lines().find(|l| !l.trim().is_empty())?.trim();
    // Extract a clean version token if present, capped at 64 chars.
    let cleaned: String = first_line.chars().filter(|c| !c.is_control()).take(64).collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

// ---- Model lists (CLI commands, local model caches, and static fallbacks)

fn model(id: &str, display_name: Option<&str>) -> HarnessModel {
    HarnessModel {
        id: id.to_string(),
        display_name: display_name.map(str::to_string),
    }
}

fn dedup_models(models: Vec<HarnessModel>) -> Vec<HarnessModel> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in models {
        let id = m.id.trim().to_string();
        if id.is_empty() || id.len() > 128 || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
            continue;
        }
        if seen.insert(id.clone()) {
            out.push(HarnessModel { id, display_name: m.display_name });
        }
    }
    out
}

fn discover_models(def: &HarnessDef, opts: &ScanOptions, bin_path: &Path) -> Vec<HarnessModel> {
    let home = opts.home.as_deref();
    let discovered = match def.slug {
        "claude-code" => Vec::new(), // VERIFY: Claude Code has no model-listing subcommand.
        "codex" => home.map(read_codex_models_cache).unwrap_or_default(),
        "antigravity" => {
            if opts.run_commands {
                // VERIFY: `agy models` tab-separated output format.
                run_cli_stdout(bin_path, &["models"])
                    .map(|out| parse_agy_models(&out))
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        "grok" => {
            if opts.run_commands {
                // VERIFY: `grok models` bulleted list output format.
                run_cli_stdout(bin_path, &["models"])
                    .filter(|out| !looks_unauthenticated(out))
                    .map(|out| parse_grok_models(&out))
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        "hermes" => {
            let mut list = home.map(read_hermes_models).unwrap_or_default();
            if list.is_empty() && opts.run_commands {
                // VERIFY: `hermes models` CLI listing command if supported.
                if let Some(out) = run_cli_stdout(bin_path, &["models"]) {
                    list = parse_line_models(&out);
                }
            }
            list
        }
        "pi" => {
            let mut list = home.map(read_pi_models).unwrap_or_default();
            if list.is_empty() && opts.run_commands {
                // VERIFY: `pi --list-models` flag and output format.
                if let Some(out) = run_cli_stdout(bin_path, &["--list-models"]) {
                    list = parse_line_models(&out);
                }
            }
            list
        }
        "opencode" => {
            if opts.run_commands {
                // VERIFY: `opencode models` output format (one `<provider>/<model>` per line).
                run_cli_stdout(bin_path, &["models"])
                    .map(|out| parse_line_models(&out))
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };

    let deduped = dedup_models(discovered);
    if !deduped.is_empty() {
        deduped
    } else {
        static_models(def.slug)
    }
}

/// Static fallback model lists when a harness CLI has no listing command or its local
/// cache has not been populated yet.
pub fn static_models(slug: &str) -> Vec<HarnessModel> {
    // VERIFY: Static model lists per CLI harness against current vendor releases.
    match slug {
        "claude-code" => vec![
            model("sonnet", Some("Claude Sonnet")),
            model("opus", Some("Claude Opus")),
            model("haiku", Some("Claude Haiku")),
        ],
        "codex" => vec![
            model("gpt-5.2", Some("GPT-5.2")),
            model("gpt-5.1", Some("GPT-5.1")),
            model("gpt-5-mini", Some("GPT-5 mini")),
            model("o4-mini", Some("o4-mini")),
        ],
        "antigravity" => vec![
            model("gemini-3.1-pro", Some("Gemini 3.1 Pro")),
            model("gemini-3-flash", Some("Gemini 3 Flash")),
            model("gemini-2.5-pro", Some("Gemini 2.5 Pro")),
            model("gemini-2.5-flash", Some("Gemini 2.5 Flash")),
        ],
        "grok" => vec![
            model("grok-4.7", Some("Grok 4.7")),
            model("grok-4", Some("Grok 4")),
            model("grok-4-fast", Some("Grok 4 Fast")),
            model("grok-code-fast", Some("Grok Code Fast")),
        ],
        "hermes" => vec![
            model("claude-sonnet-5", Some("Claude Sonnet 5")),
            model("gpt-5.2", Some("GPT-5.2")),
            model("gemini-3-pro", Some("Gemini 3 Pro")),
        ],
        "pi" => vec![
            model("claude-sonnet-5", Some("Claude Sonnet 5")),
            model("gpt-5.2", Some("GPT-5.2")),
            model("gemini-2.5-pro", Some("Gemini 2.5 Pro")),
        ],
        "opencode" => vec![
            model("anthropic/claude-sonnet-5", Some("Claude Sonnet 5")),
            model("openai/gpt-5.2", Some("GPT-5.2")),
            model("google/gemini-3-pro", Some("Gemini 3 Pro")),
        ],
        _ => Vec::new(),
    }
}

/// Parse `~/.codex/models_cache.json` (`models[]` where `visibility == "list"`).
fn read_codex_models_cache(home: &Path) -> Vec<HarnessModel> {
    // VERIFY: `~/.codex/models_cache.json` fields (`slug`, `display_name`, `visibility`).
    let path = home.join(".codex").join("models_cache.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(items) = root.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let visible = match item.get("visibility").and_then(Value::as_str) {
            Some(v) => v == "list",
            None => true,
        };
        if !visible {
            continue;
        }
        let Some(slug) = item
            .get("slug")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let display_name = item
            .get("display_name")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        out.push(HarnessModel { id: slug.to_string(), display_name });
    }
    out
}

/// Parse `agy models`: first tab-separated (or whitespace-separated) column per line.
pub(crate) fn parse_agy_models(output: &str) -> Vec<HarnessModel> {
    // VERIFY: `agy models` header and column layout.
    let mut out = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let (id, display) = match trimmed.split_once('\t') {
            Some((first, rest)) => {
                let name = rest.split('\t').next().map(str::trim).filter(|s| !s.is_empty());
                (first.trim(), name)
            }
            None => (trimmed.split_whitespace().next().unwrap_or(""), None),
        };
        if matches!(id.to_ascii_lowercase().as_str(), "id" | "model" | "models" | "name") {
            continue;
        }
        if !id.is_empty() {
            out.push(HarnessModel {
                id: id.to_string(),
                display_name: display.map(String::from),
            });
        }
    }
    dedup_models(out)
}

/// Parse `grok models`: lines starting with `* <name>` or `- <name>`.
pub(crate) fn parse_grok_models(output: &str) -> Vec<HarnessModel> {
    // VERIFY: `grok models` bullet format.
    let mut out = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let rest = trimmed
            .strip_prefix("* ")
            .or_else(|| trimmed.strip_prefix("- "))
            .or_else(|| trimmed.strip_prefix("• "));
        let Some(rest) = rest else { continue };
        let id = rest.split_whitespace().next().unwrap_or("").trim();
        if !id.is_empty() {
            out.push(HarnessModel { id: id.to_string(), display_name: None });
        }
    }
    dedup_models(out)
}

/// Parse plain line-oriented model output (e.g. `opencode models`, `pi --list-models`).
pub(crate) fn parse_line_models(output: &str) -> Vec<HarnessModel> {
    let mut out = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let token = trimmed.split_whitespace().next().unwrap_or("");
        if matches!(token.to_ascii_lowercase().as_str(), "id" | "model" | "models" | "provider" | "available") {
            continue;
        }
        if !token.is_empty() {
            out.push(HarnessModel { id: token.to_string(), display_name: None });
        }
    }
    dedup_models(out)
}

/// Parse `~/.hermes/provider_models_cache.json` (`{provider: {models: []}}`) and optional
/// `custom_providers` in `~/.hermes/config.json`.
fn read_hermes_models(home: &Path) -> Vec<HarnessModel> {
    // VERIFY: Hermes `provider_models_cache.json` and `config.json` custom_providers schema.
    let mut out = Vec::new();
    let cache_path = home.join(".hermes").join("provider_models_cache.json");
    if let Ok(text) = std::fs::read_to_string(cache_path)
        && let Ok(Value::Object(providers)) = serde_json::from_str::<Value>(&text)
    {
        for (_provider, entry) in providers {
            if let Some(arr) = entry.get("models").and_then(Value::as_array) {
                extract_json_model_array(arr, &mut out);
            }
        }
    }
    let cfg_path = home.join(".hermes").join("config.json");
    if let Ok(text) = std::fs::read_to_string(cfg_path)
        && let Ok(root) = serde_json::from_str::<Value>(&text)
        && let Some(custom) = root.get("custom_providers")
    {
        match custom {
            Value::Array(items) => {
                for p in items {
                    if let Some(arr) = p.get("models").and_then(Value::as_array) {
                        extract_json_model_array(arr, &mut out);
                    }
                }
            }
            Value::Object(map) => {
                for (_name, p) in map {
                    if let Some(arr) = p.get("models").and_then(Value::as_array) {
                        extract_json_model_array(arr, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    dedup_models(out)
}

/// Parse `~/.pi/agent/models.json` (or `~/.pi/models.json`).
fn read_pi_models(home: &Path) -> Vec<HarnessModel> {
    // VERIFY: pi coding agent `models.json` schema.
    let paths = [
        home.join(".pi").join("agent").join("models.json"),
        home.join(".pi").join("models.json"),
    ];
    let mut out = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Ok(root) = serde_json::from_str::<Value>(&text) else { continue };
        match &root {
            Value::Array(arr) => extract_json_model_array(arr, &mut out),
            Value::Object(obj) => {
                if let Some(arr) = obj.get("models").and_then(Value::as_array) {
                    extract_json_model_array(arr, &mut out);
                }
                if let Some(providers) = obj.get("providers").and_then(Value::as_object) {
                    for (_prov, p) in providers {
                        if let Some(arr) = p.get("models").and_then(Value::as_array) {
                            extract_json_model_array(arr, &mut out);
                        }
                    }
                }
            }
            _ => {}
        }
        if !out.is_empty() {
            break;
        }
    }
    dedup_models(out)
}

fn extract_json_model_array(arr: &[Value], out: &mut Vec<HarnessModel>) {
    for item in arr {
        match item {
            Value::String(s) => {
                let id = s.trim();
                if !id.is_empty() {
                    out.push(HarnessModel { id: id.to_string(), display_name: None });
                }
            }
            Value::Object(obj) => {
                let Some(id) = obj
                    .get("id")
                    .or_else(|| obj.get("slug"))
                    .or_else(|| obj.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let display_name = obj
                    .get("display_name")
                    .or_else(|| obj.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != id)
                    .map(String::from);
                out.push(HarnessModel { id: id.to_string(), display_name });
            }
            _ => {}
        }
    }
}

// ---- Headless turn execution (`turn`)

struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn create(prefix: &str, ext: &str) -> Result<Self, String> {
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).map_err(|e| format!("randomness failed: {e}"))?;
        let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join("grouper-harness-tmp");
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating temp dir {}: {e}", dir.display()))?;
        Ok(Self { path: dir.join(format!("{prefix}-{hex}.{ext}")) })
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn new_uuid_v4() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Per-conversation empty directory used to confine standard-chat harness turns on a device.
pub fn chat_workdir(workspace_root: Option<&Path>, conversation_id: &str) -> Result<PathBuf, String> {
    let id = conversation_id.trim();
    if id.is_empty()
        || id.len() > 128
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!("invalid conversation_id {conversation_id:?}"));
    }
    let base = match workspace_root {
        Some(root) => root.join("harness-chats"),
        None => std::env::temp_dir().join("grouper-harness-chats"),
    };
    let dir = base.join(id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("creating harness chat folder {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Run one headless turn of `agent`, starting a new session when `session` is `None`
/// or resuming `session` when `Some`.
pub async fn turn(
    agent: &str,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    turn_with(&ScanOptions::host(), agent, model, session, message, workdir, level).await
}

/// Like [`turn`], using explicit [`ScanOptions`] so tests can point at fake CLI binaries
/// in an isolated directory without touching the host's `$PATH` or `$HOME`.
pub async fn turn_with(
    opts: &ScanOptions,
    agent: &str,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    let slug = agent.trim();
    let Some(def) = HARNESS_DEFS.iter().find(|d| d.slug == slug) else {
        return Err(format!("unsupported harness {slug:?}"));
    };
    let Some(bin_path) = find_harness_binary(def, opts) else {
        return Err(format!("{} ({}) is not installed on this device", def.display_name, def.binary));
    };
    std::fs::create_dir_all(workdir)
        .map_err(|e| format!("creating working directory {}: {e}", workdir.display()))?;

    let session_clean = session.map(str::trim).filter(|s| !s.is_empty());
    let model_clean = model.trim();

    match slug {
        "claude-code" => run_claude_turn(&bin_path, opts, model_clean, session_clean, message, workdir, level).await,
        "codex" => run_codex_turn(&bin_path, opts, model_clean, session_clean, message, workdir, level).await,
        "antigravity" => run_antigravity_turn(&bin_path, opts, model_clean, session_clean, message, workdir, level).await,
        "grok" => run_grok_turn(&bin_path, opts, model_clean, session_clean, message, workdir, level).await,
        "hermes" => run_hermes_turn(&bin_path, opts, model_clean, session_clean, message, workdir, level).await,
        "pi" => run_pi_turn(&bin_path, opts, model_clean, session_clean, message, workdir, level).await,
        "opencode" => run_opencode_turn(&bin_path, opts, model_clean, session_clean, message, workdir, level).await,
        _ => Err(format!("unsupported harness {slug:?}")),
    }
}

/// Resolve Windows npm `.cmd`/`.bat` shims to `node <script.js>` or `<target.exe>` when possible
/// so multiline prompt arguments are not truncated by `cmd.exe` at the first newline.
#[cfg(windows)]
fn resolve_windows_shim(shim: &Path, opts: &ScanOptions) -> Option<(PathBuf, Vec<String>)> {
    let ext = shim.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "cmd" | "bat") {
        return None;
    }
    let text = std::fs::read_to_string(shim).ok()?;
    let parent = shim.parent()?;
    for token in text.split(['"', '\r', '\n']).rev() {
        let trimmed = token.trim();
        let rel = trimmed
            .strip_prefix("%~dp0\\")
            .or_else(|| trimmed.strip_prefix("%dp0%\\"))
            .or_else(|| trimmed.strip_prefix("%~dp0"))
            .or_else(|| trimmed.strip_prefix("%dp0%"));
        let Some(rel) = rel else { continue };
        let rel = rel.trim_start_matches(['\\', '/']);
        let lower = rel.to_ascii_lowercase();
        if !(lower.ends_with(".js") || lower.ends_with(".cjs") || lower.ends_with(".mjs") || lower.ends_with(".exe")) {
            continue;
        }
        let target = parent.join(rel);
        if !target.is_file() {
            continue;
        }
        if lower.ends_with(".exe") {
            return Some((target, Vec::new()));
        }
        let node_names = candidate_binary_names("node");
        let node = find_in_dirs(&opts.path_dirs, &node_names).or_else(|| {
            if opts.include_system_dirs {
                let sys = [PathBuf::from(r"C:\Program Files\nodejs")];
                find_in_dirs(&sys, &node_names)
            } else {
                None
            }
        })?;
        return Some((node, vec![target.to_string_lossy().to_string()]));
    }
    None
}

async fn run_turn_process(
    program: &Path,
    opts: &ScanOptions,
    args: &[String],
    workdir: &Path,
    drop_claude_env: bool,
) -> Result<(i32, String, String), String> {
    let mut cmd = {
        #[cfg(windows)]
        {
            if let Some((real_prog, prefix_args)) = resolve_windows_shim(program, opts) {
                let mut c = tokio::process::Command::new(real_prog);
                c.args(prefix_args);
                c
            } else if program
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "cmd" | "bat"))
            {
                let mut c = tokio::process::Command::new("cmd");
                c.arg("/C").arg(program);
                c
            } else {
                tokio::process::Command::new(program)
            }
        }
        #[cfg(not(windows))]
        {
            let _ = opts;
            tokio::process::Command::new(program)
        }
    };

    cmd.args(args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy();
        let upper = name.to_ascii_uppercase();
        if upper.starts_with("GROUPER_") {
            cmd.env_remove(&k);
        } else if drop_claude_env && upper.starts_with("CLAUDE") && upper != "CLAUDE_CONFIG_DIR" {
            // Strip parent Claude Code session variables so `claude -p` never thinks it is nested
            // inside an interactive session; keep `CLAUDE_CONFIG_DIR` for the user's credentials.
            cmd.env_remove(&k);
        }
    }
    if !opts.include_system_dirs
        && let Some(home) = opts.home.as_deref()
    {
        cmd.env("HOME", home);
        cmd.env("USERPROFILE", home);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch {}: {e}", program.display()))?;

    let output = tokio::time::timeout(TURN_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("harness timed out after {}s", TURN_TIMEOUT.as_secs()))?
        .map_err(|e| format!("waiting on harness process failed: {e}"))?;

    let cap = |bytes: &[u8]| -> String {
        let slice = if bytes.len() > MAX_TURN_OUTPUT_BYTES {
            &bytes[bytes.len() - MAX_TURN_OUTPUT_BYTES..]
        } else {
            bytes
        };
        String::from_utf8_lossy(slice).trim().to_string()
    };
    let code = output.status.code().unwrap_or(-1);
    Ok((code, cap(&output.stdout), cap(&output.stderr)))
}

fn parse_jsonl(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('{') {
                serde_json::from_str::<Value>(trimmed).ok()
            } else {
                None
            }
        })
        .collect()
}

fn last_json_object(text: &str) -> Option<Value> {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text.trim()) {
        return Some(Value::Object(map));
    }
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with('{')
            && let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(trimmed)
        {
            return Some(v);
        }
    }
    None
}

fn u64_field(obj: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| {
        obj.get(*k).and_then(|v| v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)))
    })
}

fn parse_usage_value(usage: Option<&Value>) -> HarnessUsage {
    let Some(u) = usage else { return HarnessUsage::default() };
    let mut input = u64_field(u, &["input_tokens", "prompt_tokens", "input"]).unwrap_or(0);
    let output = u64_field(u, &["output_tokens", "completion_tokens", "output"]).unwrap_or(0);
    let cache_read = u64_field(u, &["cache_read_input_tokens", "cache_read_tokens", "cached_input_tokens", "cacheRead"])
        .or_else(|| u.get("cache").and_then(|c| u64_field(c, &["read"])))
        .unwrap_or(0);
    let cache_write = u64_field(u, &["cache_creation_input_tokens", "cache_write_tokens", "cacheWrite"])
        .or_else(|| u.get("cache").and_then(|c| u64_field(c, &["write"])))
        .unwrap_or(0);
    if input == 0
        && output == 0
        && cache_read == 0
        && cache_write == 0
        && let Some(total) = u64_field(u, &["total_tokens", "totalTokens", "total"])
    {
        input = total;
    }
    HarnessUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

fn tail_detail(out: &str, err: &str) -> String {
    let src = if !err.is_empty() { err } else { out };
    let chars: Vec<char> = src.chars().collect();
    if chars.len() > 600 {
        chars[chars.len() - 600..].iter().collect()
    } else {
        src.to_string()
    }
}

async fn run_claude_turn(
    bin: &Path,
    opts: &ScanOptions,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    // VERIFY: Claude Code `--permission-mode` values (`plan`, `acceptEdits`, `bypassPermissions`).
    let permission_mode = match level {
        HarnessLevel::Confined => "plan",
        HarnessLevel::Workspace => "acceptEdits",
        HarnessLevel::Bypass => "bypassPermissions",
    };
    // VERIFY: Claude Code headless flags (`claude -p <message> --output-format json --permission-mode <mode>`).
    let mut args = vec![
        "-p".to_string(),
        message.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--permission-mode".to_string(),
        permission_mode.to_string(),
    ];
    if !model.is_empty() {
        // VERIFY: Claude Code `--model <model>`.
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(sid) = session {
        // VERIFY: Claude Code `--resume <session_id>`.
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    let (code, out, err) = run_turn_process(bin, opts, &args, workdir, true).await?;
    let Some(result) = last_json_object(&out) else {
        return Err(format!("Claude Code failed (exit {code}): {}", tail_detail(&out, &err)));
    };
    let is_error = result.get("is_error").and_then(Value::as_bool).unwrap_or(false) || code != 0;
    if is_error {
        let detail = result
            .get("result")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| result.get("api_error_code").and_then(Value::as_str))
            .or_else(|| result.get("subtype").and_then(Value::as_str))
            .map(String::from)
            .unwrap_or_else(|| tail_detail(&out, &err));
        return Err(format!("Claude Code failed: {detail}"));
    }
    let session_id = result
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(session)
        .ok_or_else(|| "Claude Code did not return a session_id".to_string())?
        .to_string();
    let text = result
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let usage = parse_usage_value(result.get("usage"));
    Ok(TurnResult { text, session_id, usage })
}

async fn run_codex_turn(
    bin: &Path,
    opts: &ScanOptions,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    // VERIFY: Codex `-c sandbox_mode="..."` values (`read-only`, `workspace-write`, `danger-full-access`).
    let sandbox = match level {
        HarnessLevel::Confined => "read-only",
        HarnessLevel::Workspace => "workspace-write",
        HarnessLevel::Bypass => "danger-full-access",
    };
    // VERIFY: Codex headless flags (`--json --skip-git-repo-check -m <model> -c sandbox_mode=... -c approval_policy="never"`).
    let mut common = vec![
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
        "-c".to_string(),
        format!("sandbox_mode={}", serde_json::to_string(sandbox).unwrap()),
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
    ];
    if !model.is_empty() {
        common.push("-m".to_string());
        common.push(model.to_string());
    }
    let mut args = vec!["exec".to_string()];
    if let Some(sid) = session {
        // VERIFY: Codex `codex exec resume <opts> <thread_id> <message>`.
        args.push("resume".to_string());
        args.extend(common);
        args.push(sid.to_string());
        args.push(message.to_string());
    } else {
        // VERIFY: Codex `codex exec <opts> -C <workdir> <message>`.
        args.extend(common);
        args.push("-C".to_string());
        args.push(workdir.to_string_lossy().to_string());
        args.push(message.to_string());
    }

    let (code, out, err) = run_turn_process(bin, opts, &args, workdir, false).await?;
    let mut thread_id: Option<String> = None;
    let mut texts: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut usage = HarnessUsage::default();
    let mut failed = false;

    for ev in parse_jsonl(&out) {
        match ev.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                if let Some(tid) = ev.get("thread_id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                    thread_id = Some(tid.to_string());
                }
            }
            Some("item.completed") => {
                if let Some(item) = ev.get("item") {
                    match item.get("type").and_then(Value::as_str) {
                        Some("agent_message") => {
                            if let Some(t) = item.get("text").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                                texts.push(t.to_string());
                            }
                        }
                        Some("error") => {
                            let msg = item.get("message").and_then(Value::as_str).unwrap_or("");
                            if !msg.contains("service tier") && !msg.contains("Model metadata for") && !msg.is_empty() {
                                errors.push(msg.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("turn.completed") => {
                usage = parse_usage_value(ev.get("usage"));
            }
            Some("turn.failed") => {
                failed = true;
                if let Some(msg) = ev.get("error").and_then(|e| e.get("message")).and_then(Value::as_str) {
                    errors.push(msg.to_string());
                }
            }
            Some("error") => {
                failed = true;
                if let Some(msg) = ev.get("message").and_then(Value::as_str) {
                    errors.push(msg.to_string());
                }
            }
            _ => {}
        }
    }

    if failed || code != 0 {
        let detail = if !errors.is_empty() {
            errors.join("; ")
        } else {
            tail_detail(&out, &err)
        };
        return Err(format!("Codex failed (exit {code}): {detail}"));
    }

    let session_id = thread_id
        .as_deref()
        .or(session)
        .ok_or_else(|| "Codex did not return a thread_id".to_string())?
        .to_string();
    Ok(TurnResult {
        text: texts.join("\n\n"),
        session_id,
        usage,
    })
}

async fn run_antigravity_turn(
    bin: &Path,
    opts: &ScanOptions,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    // VERIFY: Antigravity headless invocation (`agy -p <message> --output-format json --print-timeout <secs>s`).
    let mut args = vec![
        "-p".to_string(),
        message.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--print-timeout".to_string(),
        format!("{}s", TURN_TIMEOUT.as_secs()),
    ];
    if !model.is_empty() {
        // VERIFY: Antigravity `--model <model>`.
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(sid) = session {
        // VERIFY: Antigravity `--conversation <id>` resume flag.
        args.push("--conversation".to_string());
        args.push(sid.to_string());
    }
    match level {
        // VERIFY: Antigravity `--mode plan`, `--mode accept-edits`, and `--dangerously-skip-permissions`.
        HarnessLevel::Confined => {
            args.push("--mode".to_string());
            args.push("plan".to_string());
        }
        HarnessLevel::Workspace => {
            args.push("--mode".to_string());
            args.push("accept-edits".to_string());
        }
        HarnessLevel::Bypass => {
            args.push("--dangerously-skip-permissions".to_string());
        }
    }

    let (code, out, err) = run_turn_process(bin, opts, &args, workdir, false).await?;
    let Some(result) = last_json_object(&out) else {
        return Err(format!("Antigravity failed (exit {code}): {}", tail_detail(&out, &err)));
    };
    let status = result.get("status").and_then(Value::as_str);
    if code != 0 || status.is_some_and(|s| s != "SUCCESS") {
        return Err(format!(
            "Antigravity failed (status {}, exit {code}): {}",
            status.unwrap_or("unknown"),
            tail_detail(&out, &err)
        ));
    }
    let session_id = result
        .get("conversation_id")
        .or_else(|| result.get("session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(session)
        .ok_or_else(|| "Antigravity did not return a conversation_id".to_string())?
        .to_string();
    let text = result
        .get("response")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let usage = parse_usage_value(result.get("usage"));
    Ok(TurnResult { text, session_id, usage })
}

async fn run_grok_turn(
    bin: &Path,
    opts: &ScanOptions,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    let prompt_file = TempFileGuard::create("grok-prompt", "txt")?;
    std::fs::write(&prompt_file.path, message)
        .map_err(|e| format!("writing Grok prompt file: {e}"))?;

    // VERIFY: Grok Build `--permission-mode` (`plan` vs `bypassPermissions`).
    let perm_mode = match level {
        HarnessLevel::Confined => "plan",
        HarnessLevel::Workspace | HarnessLevel::Bypass => "bypassPermissions",
    };
    // VERIFY: Grok Build headless flags (`grok --prompt-file <f> --output-format json --cwd <dir> --no-auto-update --permission-mode <mode>`).
    let mut args = vec![
        "--prompt-file".to_string(),
        prompt_file.path.to_string_lossy().to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--cwd".to_string(),
        workdir.to_string_lossy().to_string(),
        "--no-auto-update".to_string(),
        "--permission-mode".to_string(),
        perm_mode.to_string(),
    ];
    if !model.is_empty() {
        // VERIFY: Grok `--model <model>`.
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(sid) = session {
        // VERIFY: Grok `--resume <sessionId>` flag.
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    let (code, out, err) = run_turn_process(bin, opts, &args, workdir, false).await?;
    let Some(result) = last_json_object(&out) else {
        return Err(format!("Grok failed (exit {code}): {}", tail_detail(&out, &err)));
    };
    if code != 0 || result.get("type").and_then(Value::as_str) == Some("error") {
        let msg = result
            .get("message")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| tail_detail(&out, &err));
        return Err(format!("Grok failed (exit {code}): {msg}"));
    }
    let session_id = result
        .get("sessionId")
        .or_else(|| result.get("session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(session)
        .ok_or_else(|| "Grok did not return a sessionId".to_string())?
        .to_string();
    let text = result
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let usage = parse_usage_value(result.get("usage"));
    Ok(TurnResult { text, session_id, usage })
}

async fn run_hermes_turn(
    bin: &Path,
    opts: &ScanOptions,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    _level: HarnessLevel,
) -> Result<TurnResult, String> {
    let usage_file = TempFileGuard::create("hermes-usage", "json")?;
    // VERIFY: Hermes CLI headless invocation (`hermes -z <message> [-m <model>] --usage-file <path> --in <dir> [--resume <id>]`).
    let mut args = vec![
        "-z".to_string(),
        message.to_string(),
        "--usage-file".to_string(),
        usage_file.path.to_string_lossy().to_string(),
        "--in".to_string(),
        workdir.to_string_lossy().to_string(),
    ];
    if !model.is_empty() {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    if let Some(sid) = session {
        // VERIFY: Hermes `--resume <session_id>` flag.
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    let (code, out, err) = run_turn_process(bin, opts, &args, workdir, false).await?;
    let usage_json: Option<Value> = std::fs::read_to_string(&usage_file.path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let failed_flag = usage_json
        .as_ref()
        .and_then(|u| u.get("failed"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if code != 0 || failed_flag {
        return Err(format!("Hermes failed (exit {code}): {}", tail_detail(&out, &err)));
    }
    let session_id = usage_json
        .as_ref()
        .and_then(|u| u.get("session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(session)
        .ok_or_else(|| "Hermes did not return a session_id".to_string())?
        .to_string();
    let usage = parse_usage_value(usage_json.as_ref());
    Ok(TurnResult {
        text: out,
        session_id,
        usage,
    })
}

async fn run_pi_turn(
    bin: &Path,
    opts: &ScanOptions,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    let owned_session;
    let sid = match session {
        Some(s) => s,
        None => {
            owned_session = new_uuid_v4();
            &owned_session
        }
    };
    // VERIFY: pi headless invocation (`pi -p --mode json --session-id <uuid> [--model <m>] [--no-tools|--exclude-tools ...] -- <message>`).
    let mut args = vec![
        "-p".to_string(),
        "--mode".to_string(),
        "json".to_string(),
        "--session-id".to_string(),
        sid.to_string(),
    ];
    if !model.is_empty() {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    match level {
        HarnessLevel::Confined => args.push("--no-tools".to_string()),
        HarnessLevel::Workspace => {
            args.push("--exclude-tools".to_string());
            args.push("bash,edit,write".to_string());
        }
        HarnessLevel::Bypass => {}
    }
    args.push("--".to_string());
    args.push(message.to_string());

    let (code, out, err) = run_turn_process(bin, opts, &args, workdir, false).await?;
    let mut last_msg: Option<Value> = None;
    let mut usage = HarnessUsage::default();
    for ev in parse_jsonl(&out) {
        if ev.get("type").and_then(Value::as_str) == Some("message_end")
            && let Some(msg) = ev.get("message")
            && msg.get("role").and_then(Value::as_str) == Some("assistant")
        {
            let step = parse_usage_value(msg.get("usage"));
            usage.input_tokens += step.input_tokens;
            usage.output_tokens += step.output_tokens;
            usage.cache_read_tokens += step.cache_read_tokens;
            usage.cache_write_tokens += step.cache_write_tokens;
            last_msg = Some(msg.clone());
        }
    }
    let Some(last) = last_msg else {
        return Err(format!("pi failed (exit {code}): {}", tail_detail(&out, &err)));
    };
    if code != 0 || last.get("stopReason").and_then(Value::as_str) == Some("error") {
        let msg = last
            .get("errorMessage")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| tail_detail(&out, &err));
        return Err(format!("pi failed: {msg}"));
    }
    let text = last
        .get("content")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(TurnResult {
        text,
        session_id: sid.to_string(),
        usage,
    })
}

async fn run_opencode_turn(
    bin: &Path,
    opts: &ScanOptions,
    model: &str,
    session: Option<&str>,
    message: &str,
    workdir: &Path,
    level: HarnessLevel,
) -> Result<TurnResult, String> {
    // VERIFY: OpenCode headless invocation (`opencode run --format json [-m <m>] [--agent plan] --dir <dir> --auto [-s <id>] -- <msg>`).
    let mut args = vec![
        "run".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--dir".to_string(),
        workdir.to_string_lossy().to_string(),
        "--auto".to_string(),
    ];
    if !model.is_empty() {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    if level == HarnessLevel::Confined {
        args.push("--agent".to_string());
        args.push("plan".to_string());
    }
    if let Some(sid) = session {
        // VERIFY: OpenCode `-s <sessionID>` flag.
        args.push("-s".to_string());
        args.push(sid.to_string());
    }
    args.push("--".to_string());
    args.push(message.to_string());

    let (code, out, err) = run_turn_process(bin, opts, &args, workdir, false).await?;
    let mut found_session: Option<String> = None;
    let mut texts: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut usage = HarnessUsage::default();

    for ev in parse_jsonl(&out) {
        if found_session.is_none()
            && let Some(sid) = ev.get("sessionID").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
        {
            found_session = Some(sid.to_string());
        }
        let part = ev.get("part");
        match ev.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = part.and_then(|p| p.get("text")).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                    texts.push(t.to_string());
                }
            }
            Some("step_finish") => {
                let step = parse_usage_value(part.and_then(|p| p.get("tokens")));
                usage.input_tokens += step.input_tokens;
                usage.output_tokens += step.output_tokens;
                usage.cache_read_tokens += step.cache_read_tokens;
                usage.cache_write_tokens += step.cache_write_tokens;
            }
            Some("error") => {
                if let Some(msg) = ev.get("error").and_then(|e| e.get("message")).and_then(Value::as_str) {
                    errors.push(msg.to_string());
                }
            }
            _ => {}
        }
    }

    if code != 0 || (!errors.is_empty() && texts.is_empty()) {
        let detail = if !errors.is_empty() {
            errors.join("; ")
        } else {
            tail_detail(&out, &err)
        };
        return Err(format!("opencode failed (exit {code}): {detail}"));
    }
    let session_id = found_session
        .as_deref()
        .or(session)
        .ok_or_else(|| "opencode did not return a sessionID".to_string())?
        .to_string();
    Ok(TurnResult {
        text: texts.join("\n\n"),
        session_id,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::json;

    const FIXED_NOW: u64 = 1_800_000_000;

    struct TempHome {
        path: PathBuf,
    }

    impl TempHome {
        fn new(tag: &str) -> Self {
            let mut rnd = [0u8; 4];
            getrandom::fill(&mut rnd).unwrap();
            let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
            let path = std::env::temp_dir().join(format!("grouper-harness-{tag}-{hex}"));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let full = self.path.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
            full
        }

        fn install_fake_cli(&self, dir_rel: &str, binary: &str) -> PathBuf {
            let name = if cfg!(windows) { format!("{binary}.cmd") } else { binary.to_string() };
            let rel = format!("{dir_rel}/{name}");
            let _full = self.write(&rel, if cfg!(windows) { "@echo off\r\necho 1.0.0\r\n" } else { "#!/bin/sh\necho 1.0.0\n" });
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&_full).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&_full, perms).unwrap();
            }
            self.path.join(dir_rel)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn fake_jwt_with_exp(exp: u64) -> String {
        let h = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
        let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{h}.{p}.fakesig")
    }

    #[test]
    fn claude_sign_in_states_and_capabilities_in_temp_home() {
        let home = TempHome::new("claude");
        let bin_dir = home.install_fake_cli("bin", "claude");
        let opts = ScanOptions::isolated(&home.path, vec![bin_dir], FIXED_NOW);

        // 1. Not signed in (no credential file).
        let res = scan_with(&opts);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].harness, "claude-code");
        assert_eq!(res[0].sign_in, HarnessSignIn::NotSignedIn);
        assert_eq!(res[0].login_command.as_deref(), Some("claude login"));
        assert!(res[0].models.is_empty());
        assert!(res[0].capabilities.can_orchestrate);
        assert!(res[0].capabilities.supports_resume);
        assert!(res[0].capabilities.supports_mcp);

        // 2. Expired credential file.
        let secret_token = "fake-claude-secret-token-never-send";
        home.write(
            ".claude/.credentials.json",
            &json!({
                "claudeAiOauth": {
                    "accessToken": secret_token,
                    "expiresAt": (FIXED_NOW - 60) * 1000,
                    "subscriptionType": "max"
                }
            })
            .to_string(),
        );
        let res = scan_with(&opts);
        assert_eq!(res[0].sign_in, HarnessSignIn::Expired);
        assert!(res[0].models.is_empty());
        assert!(!serde_json::to_string(&res).unwrap().contains(secret_token));

        // 3. Signed in (unexpired).
        home.write(
            ".claude/.credentials.json",
            &json!({
                "claudeAiOauth": {
                    "accessToken": secret_token,
                    "expiresAt": (FIXED_NOW + 3600) * 1000,
                    "subscriptionType": "max"
                }
            })
            .to_string(),
        );
        let res = scan_with(&opts);
        assert_eq!(res[0].sign_in, HarnessSignIn::SignedIn);
        assert_eq!(res[0].login_command, None);
        let ids: Vec<&str> = res[0].models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["sonnet", "opus", "haiku"]);
        // Secret token never appears anywhere in serialized output.
        let wire = serde_json::to_string(&NodeMessage::HarnessReport { harnesses: res }).unwrap();
        assert!(!wire.contains(secret_token));
    }

    #[test]
    fn codex_sign_in_states_and_models_cache_in_temp_home() {
        let home = TempHome::new("codex");
        let bin_dir = home.install_fake_cli("bin", "codex");
        let opts = ScanOptions::isolated(&home.path, vec![bin_dir], FIXED_NOW);

        // 1. Not signed in.
        let res = scan_with(&opts);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].harness, "codex");
        assert_eq!(res[0].sign_in, HarnessSignIn::NotSignedIn);
        assert!(!res[0].capabilities.can_orchestrate, "only Claude Code can orchestrate for now");
        assert!(res[0].capabilities.supports_resume);
        assert!(res[0].capabilities.supports_mcp);

        // 2. Expired JWT in auth.json.
        let expired_jwt = fake_jwt_with_exp(FIXED_NOW - 10);
        home.write(
            ".codex/auth.json",
            &json!({ "tokens": { "access_token": expired_jwt } }).to_string(),
        );
        let res = scan_with(&opts);
        assert_eq!(res[0].sign_in, HarnessSignIn::Expired);
        assert!(!serde_json::to_string(&res).unwrap().contains(&expired_jwt));

        // 3. Valid JWT + models_cache.json filtering (`visibility == "list"`).
        let valid_jwt = fake_jwt_with_exp(FIXED_NOW + 7200);
        home.write(
            ".codex/auth.json",
            &json!({ "tokens": { "access_token": valid_jwt } }).to_string(),
        );
        home.write(
            ".codex/models_cache.json",
            &json!({
                "models": [
                    { "slug": "gpt-5.3-codex", "display_name": "GPT-5.3 Codex", "visibility": "list" },
                    { "slug": "internal-hidden", "display_name": "Hidden", "visibility": "hidden" },
                    { "slug": "o4-mini", "display_name": "o4-mini", "visibility": "list" }
                ]
            })
            .to_string(),
        );
        let res = scan_with(&opts);
        assert_eq!(res[0].sign_in, HarnessSignIn::SignedIn);
        let ids: Vec<&str> = res[0].models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.3-codex", "o4-mini"]);
        assert!(!serde_json::to_string(&res).unwrap().contains(&valid_jwt));
    }

    #[test]
    fn other_harnesses_detect_usual_install_places_and_sign_in_states() {
        let home = TempHome::new("others");
        // Install binaries in usual home-relative locations (empty PATH).
        home.install_fake_cli(".antigravity/bin", "agy");
        home.install_fake_cli(".grok/bin", "grok");
        home.install_fake_cli(".hermes/bin", "hermes");
        home.install_fake_cli(".pi/bin", "pi");
        home.install_fake_cli(".opencode/bin", "opencode");

        let opts = ScanOptions::isolated(&home.path, Vec::new(), FIXED_NOW);

        // Initially all 5 are found in usual install places and report NotSignedIn.
        let res = scan_with(&opts);
        let slugs: Vec<&str> = res.iter().map(|h| h.harness.as_str()).collect();
        assert_eq!(slugs, vec!["antigravity", "grok", "hermes", "pi", "opencode"]);
        for h in &res {
            assert_eq!(h.sign_in, HarnessSignIn::NotSignedIn, "{} should be not_signed_in", h.harness);
            assert!(!h.capabilities.can_orchestrate, "{} must not claim can_orchestrate", h.harness);
        }

        // Mark pi expired, OpenCode signed in, Antigravity signed in, Grok expired, Hermes signed in with model cache.
        home.write(
            ".pi/agent/auth.json",
            &json!({ "anthropic": { "access": "fake-pi-tok", "expires": FIXED_NOW - 100 } }).to_string(),
        );
        home.write(
            ".local/share/opencode/auth.json",
            &json!({ "anthropic": { "access": "fake-oc-tok", "expires": FIXED_NOW + 3600 } }).to_string(),
        );
        home.write(
            ".gemini/antigravity-cli/auth.json",
            &json!({ "access_token": "fake-agy-tok", "expires_at": FIXED_NOW + 3600 }).to_string(),
        );
        home.write(
            ".grok/credentials.json",
            &json!({ "apiKey": "fake-grok-tok", "expiresAt": FIXED_NOW - 5 }).to_string(),
        );
        home.write(
            ".hermes/provider_models_cache.json",
            &json!({ "openrouter": { "models": ["anthropic/claude-sonnet-5", "deepseek/deepseek-chat"] } }).to_string(),
        );

        let res = scan_with(&opts);
        let by_slug = |s: &str| res.iter().find(|h| h.harness == s).unwrap();
        assert_eq!(by_slug("pi").sign_in, HarnessSignIn::Expired);
        assert_eq!(by_slug("grok").sign_in, HarnessSignIn::Expired);
        assert_eq!(by_slug("opencode").sign_in, HarnessSignIn::SignedIn);
        assert!(!by_slug("opencode").models.is_empty());
        assert_eq!(by_slug("antigravity").sign_in, HarnessSignIn::SignedIn);
        assert_eq!(by_slug("hermes").sign_in, HarnessSignIn::SignedIn);
        let hermes_models: Vec<&str> = by_slug("hermes").models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(hermes_models, vec!["anthropic/claude-sonnet-5", "deepseek/deepseek-chat"]);

        let serialized = serde_json::to_string(&res).unwrap();
        for secret in ["fake-pi-tok", "fake-oc-tok", "fake-agy-tok", "fake-grok-tok"] {
            assert!(!serialized.contains(secret), "leaked secret {secret}");
        }
    }

    #[test]
    fn cli_model_output_parsers_handle_agy_and_grok_formats() {
        let agy = parse_agy_models("MODEL\tNAME\ngemini-3.1-pro\tGemini 3.1 Pro\ngemini-3-flash\tGemini 3 Flash\n");
        assert_eq!(
            agy,
            vec![
                model("gemini-3.1-pro", Some("Gemini 3.1 Pro")),
                model("gemini-3-flash", Some("Gemini 3 Flash")),
            ]
        );

        let grok = parse_grok_models("Available models:\n* grok-4.7 (default)\n- grok-code-fast\n");
        assert_eq!(grok, vec![model("grok-4.7", None), model("grok-code-fast", None)]);
    }
}
