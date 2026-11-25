//! The device's MCP host: local MCP servers (stdio) whose tools threads on this
//! device can use, and which the device's own MCP server offers to the hub.
//!
//! Config is Claude-style, so existing configs paste in. It lives at
//! `<OS config dir>/grouper/mcp.json` unless [`Config::mcp_config`](crate::Config),
//! `grouper connect --mcp-config` or `GROUPER_MCP_CONFIG` say otherwise:
//!
//! ```json
//! { "includeSubagentChat": true, "subagentChatRoot": null,
//!   "mcpServers": { "git": { "command": "uvx", "args": ["mcp-server-git"], "env": {}, "disabled": false } } }
//! ```
//!
//! The subagent-chat plugin's servers (Codex, Claude Code, Antigravity, …) are included
//! by default: its root comes from `GROUPER_SUBAGENT_CHAT_ROOT`, then `subagentChatRoot`,
//! then the newest `subagent-chat` folder under `~/.claude/plugins` that holds
//! `scripts/start.py` and `.mcp.json`. Entries in `mcpServers` override its entries by name.
//!
//! Servers start when the client starts (in the background: a slow or broken server
//! never holds up the connection), restart on the next call after a crash, and stop
//! with the client. They never inherit `GROUPER_*` variables. Their tools are named
//! `<server>__<tool>`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use core_agent::{Tool, ToolOutput};
use core_protocol::tunnel::{result_text, tool_from_mcp};
use core_protocol::{MCP_SEPARATOR, ToolDef, mcp_call_timeout, valid_tool_name};
use rmcp::model::{CallToolRequestParams, JsonObject};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{Peer, ServiceError, ServiceExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::sync::watch;

/// Default time a server has to start and list its tools.
pub const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 120;

/// Environment variable naming the config file.
pub const CONFIG_ENV: &str = "GROUPER_MCP_CONFIG";
/// Environment variable naming the subagent-chat plugin folder.
pub const SUBAGENT_CHAT_ENV: &str = "GROUPER_SUBAGENT_CHAT_ROOT";

// ---- config

/// `mcp.json` as written by the user.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpFile {
    #[serde(default = "yes")]
    pub include_subagent_chat: bool,
    #[serde(default)]
    pub subagent_chat_root: Option<PathBuf>,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, ServerEntry>,
}

fn yes() -> bool {
    true
}

impl Default for McpFile {
    fn default() -> Self {
        Self { include_subagent_chat: true, subagent_chat_root: None, mcp_servers: BTreeMap::new() }
    }
}

/// One `mcpServers` entry. Only stdio servers (`command`) are supported for now.
#[derive(Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerEntry {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    /// `stdio` (default), `http`, `sse`: only stdio runs for now.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Grouper's own: how long the server has to start (default 120 s).
    #[serde(default)]
    pub startup_timeout_seconds: Option<u64>,
}

/// A server ready to launch: variables expanded, `GROUPER_*` removed.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub startup_timeout: Duration,
}

/// The config file to read: `explicit`, else `GROUPER_MCP_CONFIG`, else
/// `<OS config dir>/grouper/mcp.json`.
pub fn config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os(CONFIG_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("grouper").join("mcp.json"))
}

/// Read the config and resolve every server to launch. Problems are logged, never fatal:
/// a missing file means the defaults (subagent-chat only).
pub fn load(explicit: Option<&Path>) -> Vec<ServerSpec> {
    let file = match config_path(explicit) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<McpFile>(&text) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("{}: {e}; using no MCP servers", path.display());
                    return Vec::new();
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => McpFile::default(),
            Err(e) => {
                tracing::warn!("{}: {e}; using no MCP servers", path.display());
                return Vec::new();
            }
        },
        None => McpFile::default(),
    };
    let env_root = std::env::var_os(SUBAGENT_CHAT_ENV).filter(|p| !p.is_empty()).map(PathBuf::from);
    let plugins = dirs::home_dir().map(|h| h.join(".claude").join("plugins"));
    resolve(&file, env_root, plugins.as_deref(), &process_env)
}

/// The process environment, minus `GROUPER_*` (never passed to a server, even when named).
fn process_env(name: &str) -> Option<String> {
    if is_secret(name) { None } else { std::env::var(name).ok() }
}

fn is_secret(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with("GROUPER_")
}

/// Merge subagent-chat's servers (when included and found) with the file's own
/// entries, which win by name, and expand each.
pub fn resolve(
    file: &McpFile,
    env_root: Option<PathBuf>,
    plugins_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<ServerSpec> {
    // (entry, plugin root it came from)
    let mut entries: BTreeMap<String, (ServerEntry, Option<PathBuf>)> = BTreeMap::new();
    if file.include_subagent_chat {
        let root = env_root
            .or_else(|| file.subagent_chat_root.clone())
            .or_else(|| plugins_dir.and_then(find_subagent_chat));
        match root {
            Some(root) => match read_plugin_servers(&root) {
                Ok(servers) => {
                    tracing::info!("subagent-chat: {} ({} servers)", root.display(), servers.len());
                    for (name, entry) in servers {
                        entries.insert(name, (entry, Some(root.clone())));
                    }
                }
                Err(e) => tracing::warn!("subagent-chat at {}: {e}", root.display()),
            },
            None => tracing::info!("subagent-chat not found; set {SUBAGENT_CHAT_ENV} or subagentChatRoot"),
        }
    }
    for (name, entry) in &file.mcp_servers {
        entries.insert(name.clone(), (entry.clone(), None));
    }
    entries
        .into_iter()
        .filter_map(|(name, (entry, root))| match spec_for(&name, &entry, root.as_deref(), env) {
            Ok(spec) => spec,
            Err(e) => {
                tracing::warn!("MCP server {name}: {e}");
                None
            }
        })
        .collect()
}

fn read_plugin_servers(root: &Path) -> Result<BTreeMap<String, ServerEntry>, String> {
    let path = root.join(".mcp.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: McpFile = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(file.mcp_servers)
}

/// `Ok(None)`: disabled. `Err`: can't run.
fn spec_for(
    name: &str,
    entry: &ServerEntry,
    plugin_root: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ServerSpec>, String> {
    if entry.disabled {
        return Ok(None);
    }
    // The tool name `<server>__<tool>` must stay valid.
    if !valid_tool_name(name) || name.contains(MCP_SEPARATOR) {
        return Err("server names may use letters, digits, `-` and single `_` only".into());
    }
    let Some(command) = entry.command.as_deref().filter(|c| !c.trim().is_empty()) else {
        return Err(match (&entry.kind, &entry.url) {
            (_, Some(_)) | (Some(_), _) => "only stdio servers (with a command) are supported for now".into(),
            _ => "no command".into(),
        });
    };
    let root = plugin_root.map(|r| r.to_string_lossy().into_owned());
    let lookup = |var: &str| -> Option<String> {
        if matches!(var, "CLAUDE_PLUGIN_ROOT" | "PLUGIN_ROOT")
            && let Some(r) = &root
        {
            return Some(r.clone());
        }
        env(var)
    };
    let command = python_fallback(expand(command, &lookup));
    let args = entry.args.iter().map(|a| expand(a, &lookup)).collect();
    let mut vars = BTreeMap::new();
    for (k, v) in &entry.env {
        if is_secret(k) {
            tracing::warn!("MCP server {name}: not passing {k} (GROUPER_* stays private)");
            continue;
        }
        vars.insert(k.clone(), expand(v, &lookup));
    }
    Ok(Some(ServerSpec {
        name: name.into(),
        command,
        args,
        env: vars,
        cwd: entry.cwd.as_deref().map(|c| PathBuf::from(expand(c, &lookup))),
        startup_timeout: Duration::from_secs(entry.startup_timeout_seconds.unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS)),
    }))
}

/// Expand `${VAR}` and `${VAR:-default}` (the default is used when VAR is unset or
/// empty, and is itself expanded). Unknown variables become empty; anything else,
/// including an unclosed `${`, is kept as written.
pub fn expand(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        // Find the matching `}`, allowing `${…}` inside a default.
        let mut depth = 1;
        let mut end = None;
        let bytes = after.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'$' if bytes.get(i + 1) == Some(&b'{') => {
                    depth += 1;
                    i += 1;
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let Some(end) = end else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        let value = match inner.split_once(":-") {
            Some((name, default)) => {
                lookup(name).filter(|v| !v.is_empty()).unwrap_or_else(|| expand(default, lookup))
            }
            None => lookup(inner).unwrap_or_default(),
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// On Windows, `python3` is often missing or the Microsoft Store stub: use `python` then.
fn python_fallback(command: String) -> String {
    if !cfg!(windows) || !matches!(command.to_ascii_lowercase().as_str(), "python3" | "python3.exe") {
        return command;
    }
    static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let works = *WORKS.get_or_init(|| {
        std::process::Command::new("python3")
            .args(["-c", "import sys"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    if works { command } else { "python".into() }
}

/// The newest `subagent-chat` folder under `plugins_dir` holding `scripts/start.py`
/// and `.mcp.json`.
pub fn find_subagent_chat(plugins_dir: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<(SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let Ok(kind) = e.file_type() else { continue };
            if !kind.is_dir() {
                continue;
            }
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name == "subagent-chat" && path.join("scripts").join("start.py").is_file() && path.join(".mcp.json").is_file()
            {
                let newest = [path.clone(), path.join(".mcp.json"), path.join("scripts").join("start.py")]
                    .iter()
                    .filter_map(|p| p.metadata().and_then(|m| m.modified()).ok())
                    .max()
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                found.push((newest, path));
            } else if depth > 0 && !matches!(name.as_ref(), "node_modules" | ".git" | "__pycache__" | ".venv" | "venv") {
                walk(&path, depth - 1, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(plugins_dir, 6, &mut found);
    found.into_iter().max_by_key(|(t, _)| *t).map(|(_, p)| p)
}

// ---- running servers

/// The local MCP servers of this device.
pub struct McpHost {
    servers: Vec<Arc<Server>>,
    /// Bumped whenever the combined tool list changes.
    changed: watch::Sender<u64>,
    /// Set once the client stops: starts in progress are abandoned (their process killed).
    stop: watch::Sender<bool>,
}

struct Server {
    spec: ServerSpec,
    /// `<server>__<tool>` definitions from the last listing, and each one's own name.
    tools: Mutex<Vec<(ToolDef, String)>>,
    conn: tokio::sync::Mutex<Option<Session>>,
    /// Numbers sessions, so a failed call only discards the session it used.
    sessions: AtomicU64,
    /// Last lines the server wrote to stderr, for error messages.
    stderr: Arc<Mutex<VecDeque<String>>>,
}

struct Session {
    id: u64,
    running: RunningService<RoleClient, ()>,
}

impl Session {
    /// The process is still there (its stdout hasn't closed).
    fn alive(&self) -> bool {
        !self.running.is_closed() && !self.running.peer().is_transport_closed()
    }
}

impl McpHost {
    /// Start every server in the background and return at once.
    pub fn start(specs: Vec<ServerSpec>) -> Arc<Self> {
        let host = Arc::new(Self {
            servers: specs
                .into_iter()
                .map(|spec| {
                    Arc::new(Server {
                        spec,
                        tools: Mutex::new(Vec::new()),
                        conn: tokio::sync::Mutex::new(None),
                        sessions: AtomicU64::new(0),
                        stderr: Arc::new(Mutex::new(VecDeque::new())),
                    })
                })
                .collect(),
            changed: watch::channel(0).0,
            stop: watch::channel(false).0,
        });
        for server in &host.servers {
            let (host, server) = (Arc::downgrade(&host), server.clone());
            tokio::spawn(async move {
                let Some(h) = host.upgrade() else { return };
                if let Err(e) = h.connect(&server).await {
                    tracing::warn!("MCP server {} didn't start: {e}", server.spec.name);
                }
            });
        }
        host
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// A host with no servers.
    pub fn empty() -> Arc<Self> {
        Self::start(Vec::new())
    }

    /// Every `<server>__<tool>` currently known.
    pub fn tools(&self) -> Vec<ToolDef> {
        self.servers.iter().flat_map(|s| s.tools.lock().unwrap().iter().map(|(d, _)| d.clone()).collect::<Vec<_>>()).collect()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// Changes whenever [`McpHost::tools`] does.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn find(&self, name: &str) -> Option<(Arc<Server>, String)> {
        self.servers.iter().find_map(|s| {
            s.tools.lock().unwrap().iter().find(|(d, _)| d.name == name).map(|(_, own)| (s.clone(), own.clone()))
        })
    }

    /// Run `<server>__<tool>`. Restarts the server first if it has exited. Dropping the
    /// future cancels the call (`notifications/cancelled`). Returns `(ok, output)`.
    pub async fn call(&self, name: &str, args: Value) -> (bool, String) {
        let Some((server, tool)) = self.find(name) else {
            return (false, format!("unknown tool {name}"));
        };
        let arguments: JsonObject = match args {
            Value::Null => JsonObject::new(),
            Value::Object(o) => o,
            _ => return (false, "arguments must be a JSON object".into()),
        };
        let timeout = mcp_call_timeout(&Value::Object(arguments.clone()));
        let (peer, generation) = match self.peer(&server).await {
            Ok(p) => p,
            Err(e) => return (false, format!("the {} MCP server isn't running: {e}", server.spec.name)),
        };
        let params = CallToolRequestParams::new(tool).with_arguments(arguments);
        match tokio::time::timeout(timeout, peer.call_tool(params)).await {
            Err(_) => (false, format!("{name} didn't finish within {} s", timeout.as_secs())),
            Ok(Err(ServiceError::TransportClosed | ServiceError::TransportSend(_))) => {
                // The process is gone: forget the session so the next call restarts it.
                let mut conn = server.conn.lock().await;
                if conn.as_ref().is_some_and(|s| s.id == generation) {
                    conn.take();
                }
                (
                    false,
                    format!("the {} MCP server stopped during the call; it restarts on the next call", server.spec.name),
                )
            }
            Ok(Err(ServiceError::McpError(e))) => (false, e.message.to_string()),
            Ok(Err(e)) => (false, e.to_string()),
            Ok(Ok(result)) => result_text(&result),
        }
    }

    /// The server's live session, (re)starting it if needed.
    async fn peer(&self, server: &Arc<Server>) -> Result<(Peer<RoleClient>, u64), String> {
        {
            let conn = server.conn.lock().await;
            if let Some(s) = conn.as_ref().filter(|s| s.alive()) {
                return Ok((s.running.peer().clone(), s.id));
            }
        }
        if self.stopped() {
            return Err("the device client has stopped".into());
        }
        tracing::info!("restarting MCP server {}", server.spec.name);
        self.connect(server).await
    }

    /// Start the server (unless another caller just did), list its tools, keep the session.
    async fn connect(&self, server: &Arc<Server>) -> Result<(Peer<RoleClient>, u64), String> {
        let mut conn = server.conn.lock().await;
        if let Some(s) = conn.as_ref().filter(|s| s.alive()) {
            return Ok((s.running.peer().clone(), s.id));
        }
        *conn = None;
        if self.stopped() {
            return Err("the device client has stopped".into());
        }
        let spec = &server.spec;
        let mut stop = self.stop.subscribe();
        // Abandoning the start (timeout, or the client stopping) drops the process,
        // which kills it.
        let starting = tokio::time::timeout(spec.startup_timeout, async {
            let running = launch(spec, server.stderr.clone()).await?;
            let tools = running.peer().list_all_tools().await.map_err(|e| format!("tools/list: {e}"))?;
            Ok::<_, String>((running, tools))
        });
        let started = tokio::select! {
            r = starting => r.unwrap_or_else(|_| Err(format!("not ready within {} s", spec.startup_timeout.as_secs()))),
            _ = stop.wait_for(|s| *s) => Err("the device client has stopped".into()),
        };
        let (running, tools) = started.map_err(|e| {
            let tail: Vec<String> = server.stderr.lock().unwrap().iter().cloned().collect();
            if tail.is_empty() { e } else { format!("{e}; stderr: {}", tail.join(" | ")) }
        })?;
        let defs: Vec<(ToolDef, String)> = tools
            .iter()
            .filter_map(|t| {
                let mut def = tool_from_mcp(t);
                let own = std::mem::take(&mut def.name);
                def.name = format!("{}{MCP_SEPARATOR}{own}", spec.name);
                if valid_tool_name(&def.name) {
                    Some((def, own))
                } else {
                    tracing::warn!("MCP server {}: skipping tool {own:?} (name not usable)", spec.name);
                    None
                }
            })
            .collect();
        tracing::info!("MCP server {} ready with {} tools", spec.name, defs.len());
        let peer = running.peer().clone();
        let id = server.sessions.fetch_add(1, Ordering::Relaxed) + 1;
        *conn = Some(Session { id, running });
        drop(conn);
        let changed = {
            let mut tools = server.tools.lock().unwrap();
            let changed = *tools != defs;
            *tools = defs;
            changed
        };
        if changed {
            self.changed.send_modify(|n| *n += 1);
        }
        Ok((peer, id))
    }

    /// Stop every server. Later calls fail instead of restarting them.
    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        for server in &self.servers {
            let session = server.conn.lock().await.take();
            if let Some(mut s) = session {
                let _ = s.running.close_with_timeout(Duration::from_secs(5)).await;
            }
        }
    }
}

/// Spawn the server and initialize an MCP session with it.
async fn launch(spec: &ServerSpec, stderr_tail: Arc<Mutex<VecDeque<String>>>) -> Result<RunningService<RoleClient, ()>, String> {
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(&spec.args);
    for key in core_agent::tools::secret_env_keys() {
        cmd.env_remove(key);
    }
    cmd.envs(&spec.env);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    let mut wrapped = process_wrap::tokio::CommandWrap::from(cmd);
    // Kill the whole tree on stop (on Windows subagent-chat's launcher runs the server
    // as a child), and on drop even without a runtime: on Windows the job object kills
    // every process in it when its handle closes, including when this process dies.
    wrapped.wrap(process_wrap::tokio::KillOnDrop);
    #[cfg(windows)]
    wrapped.wrap(process_wrap::tokio::JobObject);
    #[cfg(unix)]
    wrapped.wrap(process_wrap::tokio::ProcessGroup::leader());
    let (transport, stderr) = TokioChildProcess::builder(wrapped)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("can't run {}: {e}", spec.command))?;
    if let Some(stderr) = stderr {
        let name = spec.name.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "grouper::mcp", server = %name, "{line}");
                let mut tail = stderr_tail.lock().unwrap();
                if tail.len() == 5 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });
    }
    ().serve(transport).await.map_err(|e| format!("initialize: {e}"))
}

/// One of the host's tools, offered to a thread.
pub struct HostTool {
    def: ToolDef,
    host: Arc<McpHost>,
}

impl HostTool {
    pub fn all(host: &Arc<McpHost>) -> Vec<Arc<dyn Tool>> {
        host.tools().into_iter().map(|def| Arc::new(Self { def, host: host.clone() }) as Arc<dyn Tool>).collect()
    }
}

#[async_trait]
impl Tool for HostTool {
    fn def(&self) -> ToolDef {
        self.def.clone()
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let (ok, out) = self.host.call(&self.def.name, input).await;
        if ok { ToolOutput::ok(out) } else { ToolOutput::err(out) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn expansion() {
        let env = vars(&[("A", "1"), ("EMPTY", ""), ("PY", "/opt/py")]);
        assert_eq!(expand("x${A}y", &env), "x1y");
        assert_eq!(expand("${MISSING}", &env), "");
        assert_eq!(expand("${MISSING:-python3}", &env), "python3");
        assert_eq!(expand("${EMPTY:-d}", &env), "d");
        assert_eq!(expand("${PY:-python3}", &env), "/opt/py");
        assert_eq!(expand("${MISSING:-${A}/bin}", &env), "1/bin");
        assert_eq!(expand("$A and ${unclosed", &env), "$A and ${unclosed");
        assert_eq!(expand("${A}${A}", &env), "11");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("grouper-mcp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_plugin(dir: &Path) {
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts").join("start.py"), "").unwrap();
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers": {
                "codex": {"command": "${SUBAGENT_CHAT_PYTHON:-python3}", "args": ["${CLAUDE_PLUGIN_ROOT}/scripts/start.py", "codex"]},
                "catalog": {"command": "${SUBAGENT_CHAT_PYTHON:-python3}", "args": ["${PLUGIN_ROOT}/scripts/start.py", "catalog"]}
            }}"#,
        )
        .unwrap();
    }

    #[test]
    fn discovery_finds_the_plugin() {
        let plugins = temp_dir("discover");
        assert_eq!(find_subagent_chat(&plugins), None);
        // A folder without start.py doesn't count.
        std::fs::create_dir_all(plugins.join("cache").join("subagent-chat")).unwrap();
        assert_eq!(find_subagent_chat(&plugins), None);
        let real = plugins.join("synced").join("abc_def").join("subagent-chat");
        fake_plugin(&real);
        assert_eq!(find_subagent_chat(&plugins), Some(real));
        std::fs::remove_dir_all(plugins).ok();
    }

    #[test]
    fn config_merge_and_override() {
        let plugins = temp_dir("merge");
        let root = plugins.join("marketplace").join("subagent-chat");
        fake_plugin(&root);
        let file: McpFile = serde_json::from_str(
            r#"{"mcpServers": {
                "codex": {"disabled": true},
                "git": {"command": "uvx", "args": ["mcp-server-git", "${HOME_DIR:-~}"],
                        "env": {"TOKEN": "${TOKEN_VAR}", "GROUPER_TOKEN": "leak"}},
                "web": {"type": "http", "url": "https://example.com/mcp"},
                "bad.name": {"command": "x"}
            }}"#,
        )
        .unwrap();
        assert!(file.include_subagent_chat);
        let env = vars(&[("TOKEN_VAR", "t0k"), ("SUBAGENT_CHAT_PYTHON", "/usr/bin/python3.12")]);
        let specs = resolve(&file, None, Some(&plugins), &env);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["catalog", "git"]);
        let catalog = &specs[0];
        assert_eq!(catalog.command, "/usr/bin/python3.12");
        assert_eq!(catalog.args, [format!("{}/scripts/start.py", root.display()), "catalog".to_string()]);
        let git = &specs[1];
        assert_eq!(git.args, ["mcp-server-git", "~"]);
        assert_eq!(git.env.get("TOKEN").map(String::as_str), Some("t0k"));
        assert!(!git.env.contains_key("GROUPER_TOKEN"));

        // Off by config; an explicit root wins over discovery.
        let off = McpFile { include_subagent_chat: false, ..McpFile::default() };
        assert!(resolve(&off, None, Some(&plugins), &env).is_empty());
        let other = plugins.join("elsewhere");
        fake_plugin(&other);
        let specs = resolve(&McpFile::default(), Some(other.clone()), Some(&plugins), &env);
        assert!(specs[0].args[0].starts_with(&other.display().to_string()));
        let with_root = McpFile { subagent_chat_root: Some(other.clone()), ..McpFile::default() };
        let specs = resolve(&with_root, None, None, &env);
        assert_eq!(specs.len(), 2);
        std::fs::remove_dir_all(plugins).ok();
    }

    #[test]
    fn tool_names_are_prefixed() {
        // `<server>__<tool>` is a valid name for every provider; the server part is recoverable.
        let name = format!("codex{MCP_SEPARATOR}codex_chat");
        assert!(valid_tool_name(&name));
        assert_eq!(core_protocol::mcp_server_of(&name), Some("codex"));
        assert_eq!(core_protocol::Permission::default_for(&name), core_protocol::Permission::Auto);
        let file: McpFile = serde_json::from_str(r#"{"mcpServers": {"a__b": {"command": "x"}}}"#).unwrap();
        let off = McpFile { include_subagent_chat: false, ..file };
        assert!(resolve(&off, None, None, &vars(&[])).is_empty());
    }
}
