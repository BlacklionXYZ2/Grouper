//! Offline hand-over, device side: files the hub prepared for a thread while this
//! device was away ([`HubMessage::WorkspaceFile`](core_protocol::HubMessage::WorkspaceFile))
//! are written into that thread's workspace before the thread starts here.

use std::path::Path;

use base64::Engine;
use core_agent::tools::Workspace;

/// Write one hand-over file into `<root>/<thread_id>/<path>`. The path must stay
/// inside the workspace, as for the thread's own file tools.
pub(crate) fn write_file(root: &Path, thread_id: &str, path: &str, content_base64: &str) -> Result<(), String> {
    if thread_id.is_empty() || !thread_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("bad thread id {thread_id:?}"));
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(content_base64).map_err(|e| format!("{path}: {e}"))?;
    let ws = Workspace::create(root.join(thread_id)).map_err(|e| format!("workspace: {e}"))?;
    let full = ws.resolve(path)?;
    if full == ws.root() || path.trim().is_empty() {
        return Err(format!("{path:?} is not a file path"));
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{path}: {e}"))?;
    }
    std::fs::write(&full, bytes).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_inside_the_thread_workspace_only() {
        let root = std::env::temp_dir().join(format!("grouper-handover-{}", std::process::id()));
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hello");
        write_file(&root, "t1", "dir/a.txt", &b64).unwrap();
        assert_eq!(std::fs::read(root.join("t1").join("dir").join("a.txt")).unwrap(), b"hello");
        assert!(write_file(&root, "t1", "../escape.txt", &b64).is_err());
        assert!(write_file(&root, "../t1", "a.txt", &b64).is_err());
        assert!(write_file(&root, "t1", "", &b64).is_err());
        assert!(write_file(&root, "t1", "b.txt", "not base64!").is_err());
        std::fs::remove_dir_all(root).ok();
    }
}
