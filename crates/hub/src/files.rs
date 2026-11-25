//! Getting a thread's files back (Proposal v3: "subagents return a summary and write
//! outputs to the shared workspace").
//!
//! - A device thread's workspace is on its device: the hub calls the device's unlisted
//!   MCP tools [`WORKSPACE_LIST_TOOL`] / [`WORKSPACE_READ_TOOL`] (never in the tool
//!   registry, never offered to threads). That includes threads handed over from an
//!   offline checkpoint, which continue on the device. When the device is offline the
//!   answer is 409.
//! - A hub thread's workspace is read here, from `<data>/workspaces/<thread>`.
//! - `GET /v1/threads/{id}/files` lists, `GET /v1/threads/{id}/files/{*path}` downloads
//!   (admin only; files up to [`MAX_WORKSPACE_FILE_BYTES`]). The orchestrator reads text
//!   files through `read_thread_file` ([`ReadThreadFile`]).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use core_agent::files::{FileError, list_workspace, read_workspace_file};
use core_agent::{Tool, ToolOutput};
use core_protocol::tunnel::result_text;
use core_protocol::{
    MAX_WORKSPACE_FILE_BYTES, ThreadInfo, ToolDef, WORKSPACE_LIST_TOOL, WORKSPACE_READ_TOOL, WorkspaceFile,
    WorkspaceListing,
};
use rmcp::ServiceError;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

use crate::registry::deny_unless_admin;
use crate::server::{HUB_DEVICE, Hub, err};

/// How long the device has to list or send a file.
const FILES_TIMEOUT: Duration = Duration::from_secs(60);
/// Most characters of a file the orchestrator gets.
pub const MAX_ORCHESTRATOR_CHARS: usize = 30_000;

#[derive(Debug)]
pub enum FilesError {
    /// From the workspace itself: no such thread/file (404), refused (400), too large (413).
    File(FileError),
    /// The thread's device is offline (409).
    Offline(String),
    /// The device answered with something unexpected, or not in time (502).
    Failed(String),
}

impl FilesError {
    fn status(&self) -> StatusCode {
        match self {
            Self::File(e) => StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_REQUEST),
            Self::Offline(_) => StatusCode::CONFLICT,
            Self::Failed(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl std::fmt::Display for FilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(e) => f.write_str(e.message()),
            Self::Offline(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

impl Hub {
    fn files_thread(&self, thread_id: &str) -> Result<ThreadInfo, FilesError> {
        self.db.thread(thread_id).ok_or_else(|| FilesError::File(FileError::NotFound("no such thread".into())))
    }

    /// Call one of a device's unlisted workspace tools. Unlike [`Hub::call_tool`] this
    /// doesn't need the tool in the device's list (it never is).
    async fn call_workspace_tool(&self, device_id: &str, tool: &str, args: Value) -> Result<Value, FilesError> {
        let offline = || {
            let name = self.resolve_device(device_id).map(|(_, n)| n).unwrap_or_else(|| device_id.to_string());
            FilesError::Offline(format!("{name} is offline; this thread's files are on it"))
        };
        let peer = {
            let live = self.live.lock().await;
            live.get(device_id).and_then(|l| l.mcp.clone()).ok_or_else(offline)?
        };
        let Value::Object(arguments) = args else { return Err(FilesError::Failed("bad arguments".into())) };
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let result = match tokio::time::timeout(FILES_TIMEOUT, peer.call_tool(params)).await {
            Err(_) => return Err(FilesError::Failed("the device didn't answer in time".into())),
            Ok(Err(ServiceError::TransportClosed | ServiceError::TransportSend(_))) => return Err(offline()),
            Ok(Err(e)) => return Err(FilesError::Failed(e.to_string())),
            Ok(Ok(r)) => r,
        };
        let (ok, output) = result_text(&result);
        let parsed: Option<Value> = serde_json::from_str(&output).ok();
        match (ok, parsed) {
            (true, Some(v)) => Ok(v),
            (false, Some(v)) if v.get("error").is_some() => {
                let status = v.get("status").and_then(Value::as_u64).unwrap_or(400) as u16;
                let message = v["error"].as_str().unwrap_or_default().to_string();
                Err(FilesError::File(FileError::from_status(status, message)))
            }
            _ => Err(FilesError::Failed(format!("the device couldn't do that: {output}"))),
        }
    }

    /// The files in a thread's workspace, wherever it is.
    pub async fn thread_files(&self, thread_id: &str) -> Result<WorkspaceListing, FilesError> {
        let t = self.files_thread(thread_id)?;
        let (files, truncated) = if t.device_id == HUB_DEVICE {
            let root = self.data_dir.join("workspaces").join(&t.id);
            tokio::task::spawn_blocking(move || list_workspace(&root))
                .await
                .map_err(|e| FilesError::Failed(e.to_string()))?
                .map_err(FilesError::File)?
        } else {
            let v = self.call_workspace_tool(&t.device_id, WORKSPACE_LIST_TOOL, json!({"thread_id": t.id})).await?;
            let files: Vec<WorkspaceFile> = serde_json::from_value(v["files"].clone())
                .map_err(|e| FilesError::Failed(format!("unreadable listing from the device: {e}")))?;
            (files, v["truncated"].as_bool().unwrap_or(false))
        };
        Ok(WorkspaceListing { thread_id: t.id, device_id: t.device_id, files, truncated })
    }

    /// One file from a thread's workspace (at most [`MAX_WORKSPACE_FILE_BYTES`]).
    pub async fn thread_file(&self, thread_id: &str, path: &str) -> Result<Vec<u8>, FilesError> {
        let t = self.files_thread(thread_id)?;
        if t.device_id == HUB_DEVICE {
            let root = self.data_dir.join("workspaces").join(&t.id);
            let path = path.to_string();
            return tokio::task::spawn_blocking(move || read_workspace_file(&root, &path, MAX_WORKSPACE_FILE_BYTES))
                .await
                .map_err(|e| FilesError::Failed(e.to_string()))?
                .map_err(FilesError::File);
        }
        let v = self.call_workspace_tool(&t.device_id, WORKSPACE_READ_TOOL, json!({"thread_id": t.id, "path": path})).await?;
        let b64 = v["content_base64"].as_str().ok_or_else(|| FilesError::Failed("the device sent no content".into()))?;
        decode_device_file(b64)
    }
}

fn decode_device_file(b64: &str) -> Result<Vec<u8>, FilesError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| FilesError::Failed(format!("unreadable file from the device: {e}")))?;
    if bytes.len() as u64 > MAX_WORKSPACE_FILE_BYTES {
        return Err(FilesError::File(FileError::TooLarge(format!(
            "the device sent {} bytes; the limit is {MAX_WORKSPACE_FILE_BYTES}",
            bytes.len()
        ))));
    }
    Ok(bytes)
}

// ---- HTTP API (admin only)

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/threads/{id}/files", get(list_files))
        .route("/v1/threads/{id}/files/{*path}", get(get_file))
}

async fn list_files(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.thread_files(&id).await {
        Ok(listing) => Json(listing).into_response(),
        Err(e) => err(e.status(), e),
    }
}

async fn get_file(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    if let Some(r) = deny_unless_admin(&hub, &headers) {
        return r;
    }
    match hub.thread_file(&id, &path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(e) => err(e.status(), e),
    }
}

// ---- orchestrator tool

/// `read_thread_file`: the orchestrator quotes a thread's output without starting
/// another thread. Text files only, capped at [`MAX_ORCHESTRATOR_CHARS`]; read-only.
pub(crate) struct ReadThreadFile {
    pub(crate) hub: Arc<Hub>,
    pub(crate) conversation_id: String,
}

#[async_trait]
impl Tool for ReadThreadFile {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "read_thread_file".into(),
            description: "Read a text file a thread in this conversation wrote to its workspace (on its device or \
                          the hub), to quote or check its result without starting a thread. Leave out path to list \
                          the thread's files. Text only, up to 30,000 characters. Needs the thread's device online."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "thread_id": {"type": "string"},
                "path": {"type": "string", "description": "Workspace path, e.g. out/report.md"}
            }, "required": ["thread_id"]}),
        }
    }

    async fn call(&self, input: Value) -> ToolOutput {
        let id = input.get("thread_id").and_then(Value::as_str).unwrap_or_default();
        if !self.hub.db.thread(id).is_some_and(|t| t.conversation_id == self.conversation_id) {
            return ToolOutput::err("no such thread in this conversation");
        }
        let path = input.get("path").and_then(Value::as_str).unwrap_or_default().trim();
        if path.is_empty() {
            return match self.hub.thread_files(id).await {
                Err(e) => ToolOutput::err(e.to_string()),
                Ok(l) if l.files.is_empty() => ToolOutput::ok("(no files)"),
                Ok(l) => {
                    let mut lines: Vec<String> = l.files.iter().map(|f| format!("{} ({} bytes)", f.path, f.size)).collect();
                    if l.truncated {
                        lines.push("… more files not listed".into());
                    }
                    ToolOutput::ok(lines.join("\n"))
                }
            };
        }
        match self.hub.thread_file(id, path).await {
            Err(e) => ToolOutput::err(e.to_string()),
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) if !text.contains('\0') => ToolOutput::ok(clip_chars(text)),
                Ok(text) => ToolOutput::err(binary_note(path, text.len())),
                Err(e) => ToolOutput::err(binary_note(path, e.as_bytes().len())),
            },
        }
    }
}

fn binary_note(path: &str, len: usize) -> String {
    format!("{path} is not a text file ({len} bytes); the user can download it with `grouper files <thread> {path}`")
}

fn clip_chars(text: String) -> String {
    match text.char_indices().nth(MAX_ORCHESTRATOR_CHARS) {
        None => text,
        Some((cut, _)) => {
            let total = text.chars().count();
            format!("{}\n… [truncated: {} of {total} characters shown]", &text[..cut], MAX_ORCHESTRATOR_CHARS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_by_characters() {
        assert_eq!(clip_chars("short".into()), "short");
        let long = "é".repeat(MAX_ORCHESTRATOR_CHARS + 5);
        let clipped = clip_chars(long);
        assert!(clipped.starts_with(&"é".repeat(MAX_ORCHESTRATOR_CHARS)));
        assert!(clipped.ends_with(&format!("{MAX_ORCHESTRATOR_CHARS} of {} characters shown]", MAX_ORCHESTRATOR_CHARS + 5)));
    }

    #[test]
    fn decoded_device_files_are_capped() {
        let over = vec![0u8; MAX_WORKSPACE_FILE_BYTES as usize + 1];
        let encoded = base64::engine::general_purpose::STANDARD.encode(over);
        assert!(matches!(decode_device_file(&encoded), Err(FilesError::File(FileError::TooLarge(_)))));
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"ok");
        assert_eq!(decode_device_file(&encoded).unwrap(), b"ok");
    }
}
