//! The hub fetching a thread's output files from this device: the internal MCP tools
//! [`WORKSPACE_LIST_TOOL`] and [`WORKSPACE_READ_TOOL`]. They are never listed in
//! `tools/list` (so they stay out of the tool registry and permissions) and threads
//! never get them; only the hub calls them by name. Both are confined to
//! `<workspace root>/<thread_id>`, which must already exist.
//!
//! Success is JSON: `{"files": [{"path", "size"}], "truncated"}` or
//! `{"path", "size", "content_base64"}`. Failure is JSON too, `{"error", "status"}`
//! with an HTTP-style status (404, 400, 413) the hub passes on.

use std::path::Path;

use base64::Engine;
use core_agent::files::{FileError, list_workspace, read_workspace_file};
use core_protocol::{MAX_WORKSPACE_FILE_BYTES, WORKSPACE_LIST_TOOL, WORKSPACE_READ_TOOL};
use serde_json::{Value, json};

pub(crate) fn is_internal(tool: &str) -> bool {
    tool == WORKSPACE_LIST_TOOL || tool == WORKSPACE_READ_TOOL
}

fn failure(e: FileError) -> (bool, String) {
    (false, json!({"error": e.message(), "status": e.status()}).to_string())
}

/// Run one of the internal workspace tools. Blocking (file system).
pub(crate) fn call(root: Option<&Path>, tool: &str, args: &Value) -> (bool, String) {
    let Some(root) = root else {
        return failure(FileError::NotFound("this device doesn't run threads".into()));
    };
    let thread_id = args.get("thread_id").and_then(Value::as_str).unwrap_or_default();
    if thread_id.is_empty() || thread_id.len() > 64 || !thread_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return failure(FileError::Refused(format!("bad thread id {thread_id:?}")));
    }
    let ws = root.join(thread_id);
    if !ws.is_dir() {
        return failure(FileError::NotFound(format!("no workspace for thread {thread_id} on this device")));
    }
    match tool {
        WORKSPACE_LIST_TOOL => match list_workspace(&ws) {
            Ok((files, truncated)) => (true, json!({"files": files, "truncated": truncated}).to_string()),
            Err(e) => failure(e),
        },
        WORKSPACE_READ_TOOL => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
            match read_workspace_file(&ws, path, MAX_WORKSPACE_FILE_BYTES) {
                Ok(bytes) => (
                    true,
                    json!({
                        "path": path,
                        "size": bytes.len(),
                        "content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    })
                    .to_string(),
                ),
                Err(e) => failure(e),
            }
        }
        _ => failure(FileError::NotFound(format!("no tool {tool}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confined_to_known_thread_workspaces() {
        let root = std::env::temp_dir().join(format!("grouper-wsfiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("t1").join("out")).unwrap();
        std::fs::write(root.join("t1").join("out").join("a.txt"), "hello").unwrap();
        std::fs::create_dir_all(root.join("t2")).unwrap();
        std::fs::write(root.join("t2").join("other.txt"), "not yours").unwrap();

        let (ok, out) = call(Some(&root), WORKSPACE_LIST_TOOL, &json!({"thread_id": "t1"}));
        assert!(ok, "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["files"], json!([{"path": "out/a.txt", "size": 5}]));
        let (ok, out) = call(Some(&root), WORKSPACE_READ_TOOL, &json!({"thread_id": "t1", "path": "out/a.txt"}));
        assert!(ok, "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["content_base64"], base64::engine::general_purpose::STANDARD.encode("hello"));

        for (args, status) in [
            (json!({"thread_id": "t9"}), 404),
            (json!({"thread_id": "../t2"}), 400),
            (json!({"thread_id": ""}), 400),
            (json!({"thread_id": "t1", "path": "../t2/other.txt"}), 400),
            (json!({"thread_id": "t1", "path": "missing.txt"}), 404),
        ] {
            let (ok, out) = call(Some(&root), WORKSPACE_READ_TOOL, &args);
            assert!(!ok, "{args}");
            assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["status"], status, "{args}: {out}");
        }
        let (ok, _) = call(None, WORKSPACE_LIST_TOOL, &json!({"thread_id": "t1"}));
        assert!(!ok);
        assert!(is_internal(WORKSPACE_READ_TOOL) && !is_internal("read_file"));
        std::fs::remove_dir_all(root).ok();
    }
}
