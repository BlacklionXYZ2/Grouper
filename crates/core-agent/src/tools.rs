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
                    let p = e.path();
                    // Always `/`, so models see the same paths on every OS.
                    let shown = p
                        .strip_prefix(&root)
                        .unwrap_or(&p)
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/");
                    if p.is_dir() {
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

struct RunShell(Arc<Workspace>);
#[async_trait]
impl Tool for RunShell {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "run_shell".into(),
            description: "Run a shell command with your workspace as the working directory. \
                          Returns exit code, stdout and stderr."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "command": {"type": "string"},
                "timeout_secs": {"type": "integer", "description": "Default 120, max 600"}
            }, "required": ["command"]}),
        }
    }
    async fn call(&self, input: Value) -> ToolOutput {
        let Some(cmd) = s(&input, "command") else { return ToolOutput::err("missing command") };
        let secs = input.get("timeout_secs").and_then(Value::as_u64).unwrap_or(120).min(600);
        let mut c = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(&cmd);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(&cmd);
            c
        };
        c.current_dir(self.0.root()).kill_on_drop(true).stdin(std::process::Stdio::null());
        for key in secret_env_keys() {
            c.env_remove(key);
        }
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
        assert!(tools[2].call(json!({})).await.content.contains("a/b.txt"));
        let sh = shell_tool(&ws).call(json!({"command": "cat a/b.txt"})).await;
        assert!(sh.content.contains("hi"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn shell_hides_grouper_secrets() {
        let dir = std::env::temp_dir().join(format!("grouper-env-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        // SAFETY: no other test reads or writes these variables.
        unsafe {
            std::env::set_var("GROUPER_TOKEN", "dev.supersecret");
            std::env::set_var("GROUPER_TEST_VISIBLE_CHECK", "x");
            std::env::set_var("NOT_GROUPER_CHECK", "visible");
        }
        let cmd = if cfg!(windows) { "set" } else { "env" };
        let out = shell_tool(&ws).call(json!({"command": cmd})).await;
        assert!(!out.content.contains("supersecret"), "{}", out.content);
        assert!(!out.content.contains("GROUPER_TEST_VISIBLE_CHECK"));
        assert!(out.content.contains("NOT_GROUPER_CHECK=visible"));
        std::fs::remove_dir_all(dir).ok();
    }
}
