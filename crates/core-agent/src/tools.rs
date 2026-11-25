//! Tools a thread gets on a device: files in its workspace, and the shell.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use core_protocol::ToolDef;
use serde_json::{Value, json};

use crate::{Tool, ToolOutput};

/// A thread's own folder. File tools can't reach outside it.
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn create(root: PathBuf) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(&root)?;
        Ok(Arc::new(Self { root: root.canonicalize()? }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a workspace-relative path, refusing anything that escapes it.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = rel.trim();
        let p = Path::new(if rel.is_empty() { "." } else { rel });
        if p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_))) {
            return Err(format!("{rel:?} is outside the workspace; use a relative path without `..`"));
        }
        let full = self.root.join(p);
        // Follow symlinks for whatever part already exists.
        let mut existing = full.as_path();
        while !existing.exists() {
            existing = existing.parent().unwrap_or(&self.root);
        }
        let real = existing.canonicalize().map_err(|e| e.to_string())?;
        if !real.starts_with(&self.root) {
            return Err(format!("{rel:?} leads outside the workspace"));
        }
        Ok(full)
    }
}

fn s(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(String::from)
}

struct ReadFile(Arc<Workspace>);
#[async_trait]
impl Tool for ReadFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_file".into(),
            description: "Read a text file from your workspace.".into(),
            input_schema: json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace"}
            }, "required": ["path"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let Some(rel) = s(&input, "path") else { return ToolOutput::err("missing path") };
        match self.0.resolve(&rel) {
            Err(e) => ToolOutput::err(e),
            Ok(p) => match tokio::fs::read_to_string(&p).await {
                Ok(t) => ToolOutput::ok(t),
                Err(e) => ToolOutput::err(format!("{rel}: {e}")),
            },
        }
    }
}

struct WriteFile(Arc<Workspace>);
#[async_trait]
impl Tool for WriteFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "write_file".into(),
            description: "Create or overwrite a text file in your workspace (folders are created).".into(),
            input_schema: json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path relative to the workspace"},
                "content": {"type": "string"}
            }, "required": ["path", "content"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let (Some(rel), Some(content)) = (s(&input, "path"), s(&input, "content")) else {
            return ToolOutput::err("missing path or content");
        };
        let p = match self.0.resolve(&rel) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        if let Some(parent) = p.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return ToolOutput::err(e.to_string());
        }
        match tokio::fs::write(&p, content.as_bytes()).await {
            Ok(()) => ToolOutput::ok(format!("wrote {} bytes to {rel}", content.len())),
            Err(e) => ToolOutput::err(format!("{rel}: {e}")),
        }
    }
}

/// Whether a directory entry is a symlink or (on Windows) a reparse point such as
/// a junction or mount point. Never follows the link.
fn is_link_entry(entry: &std::fs::DirEntry, ft: &std::fs::FileType) -> bool {
    if ft.is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if let Ok(meta) = std::fs::symlink_metadata(entry.path())
            && (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
        {
            return true;
        }
    }
    false
}

struct ListFiles(Arc<Workspace>);
#[async_trait]
impl Tool for ListFiles {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_files".into(),
            description: "List files in your workspace (recursive, up to 500 entries).".into(),
            input_schema: json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Folder relative to the workspace (default: root)"}
            }}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let rel = s(&input, "path").unwrap_or_default();
        let dir = match self.0.resolve(&rel) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let root = self.0.root().to_path_buf();
        let listing = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let mut stack = vec![dir];
            while let Some(d) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&d) else { continue };
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    let p = e.path();
                    // Always `/`, so models see the same paths on every OS.
                    let shown = p
                        .strip_prefix(&root)
                        .unwrap_or(&p)
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    if is_link_entry(&e, &ft) {
                        out.push(format!("{shown}@"));
                    } else if ft.is_dir() {
                        out.push(format!("{shown}/"));
                        stack.push(p);
                    } else {
                        out.push(shown);
                    }
                    if out.len() >= 500 {
                        return out;
                    }
                }
            }
            out.sort();
            out
        })
        .await
        .unwrap_or_default();
        ToolOutput::ok(if listing.is_empty() { "(empty)".into() } else { listing.join("\n") })
    }
}

fn shell_description() -> &'static str {
    #[cfg(windows)]
    {
        "Run a shell command in PowerShell on Windows, with your workspace as the working directory. \
         Use PowerShell syntax. Returns exit code, stdout and stderr."
    }
    #[cfg(not(windows))]
    {
        "Run a shell command with sh -c and your workspace as the working directory. \
         Returns exit code, stdout and stderr."
    }
}

/// `pwsh` when it is on PATH, otherwise Windows PowerShell.
#[cfg(windows)]
fn windows_powershell() -> &'static str {
    let Some(path) = std::env::var_os("PATH") else {
        return "powershell.exe";
    };
    for dir in std::env::split_paths(&path) {
        if dir.join("pwsh.exe").is_file() || dir.join("pwsh.cmd").is_file() {
            return "pwsh";
        }
    }
    "powershell.exe"
}

fn shell_program(cmd: &str) -> tokio::process::Command {
    #[cfg(windows)]
    let mut command = {
        // A redirected pipe is not a console. Windows PowerShell encodes that pipe as
        // UTF-16 unless OutputEncoding is UTF-8 before the user's command runs.
        let wrapped = format!(
            "$OutputEncoding = [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false; {cmd}"
        );
        let mut c = tokio::process::Command::new(windows_powershell());
        c.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &wrapped]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    hide_grouper_env(&mut command);
    command
}

/// Threads must not see hub tokens or provider keys.
fn hide_grouper_env(cmd: &mut tokio::process::Command) {
    for (key, _) in std::env::vars() {
        if key.to_ascii_uppercase().starts_with("GROUPER_") {
            cmd.env_remove(key);
        }
    }
}

struct RunShell(Arc<Workspace>);
#[async_trait]
impl Tool for RunShell {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "run_shell".into(),
            description: shell_description().into(),
            input_schema: json!({"type": "object", "properties": {
                "command": {"type": "string"},
                "timeout_secs": {"type": "integer", "description": "Default 120, max 600"}
            }, "required": ["command"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let Some(cmd) = s(&input, "command") else { return ToolOutput::err("missing command") };
        let secs = input.get("timeout_secs").and_then(Value::as_u64).unwrap_or(120).min(600);
        let mut c = shell_program(&cmd);
        c.current_dir(self.0.root()).kill_on_drop(true).stdin(std::process::Stdio::null());
        for key in secret_env_keys() {
            c.env_remove(key);
        }
        c.env("GROUPER_THREAD_SHELL", "1");
        match tokio::time::timeout(Duration::from_secs(secs), c.output()).await {
            Err(_) => ToolOutput::err(format!("timed out after {secs}s")),
            Ok(Err(e)) => ToolOutput::err(e.to_string()),
            Ok(Ok(o)) => {
                let code = o.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                let text = format!(
                    "exit code: {code}\n--- stdout\n{}\n--- stderr\n{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if o.status.success() { ToolOutput::ok(text) } else { ToolOutput::err(text) }
            }
        }
    }
}

/// Environment variables a thread's shell must not inherit: everything named
/// `GROUPER_*` (the device token, provider keys passed to the CLI, hub settings).
/// A prompt-injected thread could otherwise read the token with `printenv` and use
/// it against the hub.
pub fn secret_env_keys() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| k.to_string_lossy().to_ascii_uppercase().starts_with("GROUPER_"))
        .collect()
}

pub fn file_tools(ws: &Arc<Workspace>) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ReadFile(ws.clone())), Arc::new(WriteFile(ws.clone())), Arc::new(ListFiles(ws.clone()))]
}

pub fn shell_tool(ws: &Arc<Workspace>) -> Arc<dyn Tool> {
    Arc::new(RunShell(ws.clone()))
}

/// Definitions of the built-in thread tools, for a device to advertise.
pub fn builtin_defs(with_shell: bool) -> Vec<ToolDef> {
    let ws = Arc::new(Workspace { root: PathBuf::from(".") });
    let mut defs: Vec<ToolDef> = file_tools(&ws).iter().map(|t| t.def()).collect();
    if with_shell {
        defs.push(shell_tool(&ws).def());
    }
    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn workspace_confinement() {
        let dir = std::env::temp_dir().join(format!("grouper-ws-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let tools = file_tools(&ws);
        let w = tools[1].call(json!({"path": "a/b.txt", "content": "hi"})).await;
        assert!(!w.is_error, "{w:?}");
        assert_eq!(tools[0].call(json!({"path": "a/b.txt"})).await.content, "hi");
        assert!(tools[0].call(json!({"path": "../x"})).await.is_error);
        assert!(tools[0].call(json!({"path": "/etc/passwd"})).await.is_error);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", dir.join("link")).unwrap();
            assert!(tools[0].call(json!({"path": "link/passwd"})).await.is_error);
        }
        let listing = tools[2].call(json!({})).await.content.replace('\\', "/");
        assert!(listing.contains("a/b.txt"), "{listing}");
        let sh = shell_tool(&ws).call(json!({"command": "cat a/b.txt"})).await;
        assert!(sh.content.contains("hi"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn list_files_does_not_follow_links() {
        let base = std::env::temp_dir().join(format!("grouper-list-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws_dir = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "top-secret").unwrap();
        let ws = Workspace::create(ws_dir.clone()).unwrap();
        std::fs::write(ws.root().join("inside.txt"), "ok").unwrap();

        let link = ws_dir.join("escape");
        #[cfg(unix)]
        let created = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let created = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());

        if !created {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let tools = file_tools(&ws);
        let listing = tools[2].call(json!({})).await;
        assert!(!listing.is_error, "{listing:?}");
        let lines: Vec<&str> = listing.content.lines().collect();
        assert!(lines.contains(&"inside.txt"), "{lines:?}");
        assert!(lines.contains(&"escape@"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("secret.txt") || *l == "escape/"), "{lines:?}");
        // Reading through the link is still refused.
        assert!(tools[0].call(json!({"path": "escape/secret.txt"})).await.is_error);

        #[cfg(windows)]
        let _ = std::fs::remove_dir(&link);
        #[cfg(unix)]
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn run_shell_def_names_the_shell() {
        let desc = builtin_defs(true).into_iter().find(|d| d.name == "run_shell").expect("run_shell").description;
        #[cfg(windows)]
        assert!(desc.contains("PowerShell on Windows"), "{desc}");
        #[cfg(not(windows))]
        assert!(desc.contains("sh -c"), "{desc}");
    }

    fn env_list_command() -> &'static str {
        #[cfg(windows)]
        {
            "Get-ChildItem env: | ForEach-Object { $_.Name + '=' + $_.Value }"
        }
        #[cfg(not(windows))]
        {
            "env"
        }
    }

    struct SavedEnv(Vec<(String, Option<String>)>);

    impl SavedEnv {
        fn set(pairs: &[(&str, &str)]) -> Self {
            let saved = pairs
                .iter()
                .map(|(key, value)| {
                    let prev = std::env::var(key).ok();
                    // SAFETY: the test process is the only writer, and GROUPER_ENV_LOCK
                    // keeps the other shell tests from reading or restoring these at the same time.
                    unsafe { std::env::set_var(key, value) };
                    ((*key).to_string(), prev)
                })
                .collect();
            Self(saved)
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            for (key, prev) in self.0.drain(..) {
                // SAFETY: same lock as `set`; restoring the previous value cannot race another test.
                unsafe {
                    match prev {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    static GROUPER_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(clippy::await_holding_lock)]
    async fn assert_grouper_hidden(ws: &Arc<Workspace>) {
        let _guard = GROUPER_ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let _env = SavedEnv::set(&[
            ("GROUPER_TOKEN", "grouper-test-secret-token"),
            ("GROUPER_PROVIDER_KEY", "grouper-test-secret-key"),
        ]);
        let sh = shell_tool(ws).call(json!({"command": env_list_command()})).await;
        assert!(!sh.is_error, "{sh:?}");
        let lower = sh.content.to_ascii_lowercase();
        assert!(lower.contains("path="), "{sh:?}");
        assert!(!lower.contains("grouper_token="), "{sh:?}");
        assert!(!lower.contains("grouper_provider_key="), "{sh:?}");
        // The only GROUPER_ variable a thread shell sees is the marker that stops the
        // CLI from using stored credentials inside it.
        assert!(lower.contains("grouper_thread_shell=1"), "{sh:?}");
        assert!(!sh.content.contains("grouper-test-secret-token"), "{sh:?}");
        assert!(!sh.content.contains("grouper-test-secret-key"), "{sh:?}");
    }

    #[tokio::test]
    async fn shell_hides_grouper_secrets() {
        let dir = std::env::temp_dir().join(format!("grouper-env-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        assert_grouper_hidden(&ws).await;
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn run_shell_uses_powershell() {
        let dir = std::env::temp_dir().join(format!("grouper-ps-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let sh = shell_tool(&ws).call(json!({"command": "$PSVersionTable.PSVersion.Major"})).await;
        assert!(!sh.is_error, "{sh:?}");
        let stdout = sh.content.split("--- stdout\n").nth(1).unwrap_or("");
        let stdout = stdout.split("--- stderr").next().unwrap_or(stdout).trim();
        assert!(!stdout.is_empty() && stdout.chars().all(|c| c.is_ascii_digit()), "{sh:?}");
        assert!(shell_tool(&ws).def().description.contains("PowerShell on Windows"));
        assert_grouper_hidden(&ws).await;
        std::fs::remove_dir_all(dir).ok();
    }
}
