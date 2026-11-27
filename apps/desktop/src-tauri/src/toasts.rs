//! Native toasts for approvals waiting on the owner.
//!
//! The hub's event broadcast is crate-private. The embedded hub exposes the same waiting set
//! in-process through [`hub::server::Hub::pending_approvals`] (what `GET /v1/approvals` returns,
//! and what `ApprovalRequested` on `GET /v1/events` announces). This loop polls that set.
//!
//! A toast names the tool, device, thread and chat only. The command and its input stay out,
//! including the click payload, which is only the chat and approval ids.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use hub::server::Hub;
use serde_json::Value;
use tauri::{AppHandle, Manager, WebviewUrl};
use tauri_plugin_notification::{ActionPerformed, NotificationExt};

use crate::settings::LOCAL_HUB_URL;

const POLL: Duration = Duration::from_secs(1);
const TITLE: &str = "Grouper needs your review";

/// Names safe to show. Built without copying the tool input or the approval reason into the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApprovalNotice {
    tool: String,
    device: String,
    thread: String,
    chat: String,
    conversation_id: String,
    approval_id: String,
}

/// Everything the hub knows about one waiting approval. `input` and `reason` are accepted so a
/// caller cannot forget they exist, and then dropped.
pub(crate) struct ApprovalFields<'a> {
    pub tool: &'a str,
    pub device_name: &'a str,
    pub thread_title: &'a str,
    pub chat_title: &'a str,
    pub conversation_id: &'a str,
    pub approval_id: &'a str,
    pub input: &'a Value,
    pub reason: &'a str,
}

impl ApprovalNotice {
    pub(crate) fn body(&self) -> String {
        let tool = field(&self.tool, 80);
        let tool = if tool.is_empty() { "A tool".to_string() } else { tool };
        let device = field(&self.device, 80);
        let head = if device.is_empty() { tool } else { format!("{tool} on {device}") };
        let mut parts = vec![head];
        for name in [field(&self.thread, 200), field(&self.chat, 200)] {
            if !name.is_empty() {
                parts.push(name);
            }
        }
        parts.join(" · ")
    }

    /// `#/chat/<id>?approval=<approval id>`
    pub(crate) fn hash(&self) -> String {
        approval_hash(&self.conversation_id, &self.approval_id)
    }
}

pub(crate) fn notice_from(fields: &ApprovalFields<'_>) -> ApprovalNotice {
    // The command and the reason are never shown and never stored on the toast.
    let _hidden = (fields.input, fields.reason);
    ApprovalNotice {
        tool: fields.tool.to_string(),
        device: fields.device_name.to_string(),
        thread: fields.thread_title.to_string(),
        chat: fields.chat_title.to_string(),
        conversation_id: fields.conversation_id.to_string(),
        approval_id: fields.approval_id.to_string(),
    }
}

pub(crate) fn approval_hash(conversation_id: &str, approval_id: &str) -> String {
    format!("#/chat/{}?approval={}", enc(conversation_id), enc(approval_id))
}

pub fn start(app: &AppHandle) {
    let clicks = app.clone();
    if let Err(err) = app.notification().on_action(move |action| on_action(&clicks, action)) {
        tracing::warn!("could not listen for notification clicks: {err}");
    }

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut shown: HashSet<String> = HashSet::new();
        loop {
            tokio::time::sleep(POLL).await;
            let Some(hub) = current_hub(&app) else {
                shown.clear();
                continue;
            };
            let pending = hub.pending_approvals(None).await;
            let mut next_shown = HashSet::with_capacity(pending.len());
            for item in &pending {
                if shown.contains(&item.approval_id) {
                    next_shown.insert(item.approval_id.clone());
                    continue;
                }
                let notice = notice_for(
                    &hub,
                    &item.tool,
                    &item.device_id,
                    &item.thread_id,
                    &item.conversation_id,
                    &item.approval_id,
                    &item.input,
                    &item.reason,
                );
                if show_toast(&app, &notice) {
                    next_shown.insert(item.approval_id.clone());
                }
            }
            shown = next_shown;
        }
    });
}

fn current_hub(app: &AppHandle) -> Option<Arc<Hub>> {
    app.try_state::<crate::DesktopState>()?.hub_handle()?.hub()
}

fn notice_for(
    hub: &Hub,
    tool: &str,
    device_id: &str,
    thread_id: &str,
    conversation_id: &str,
    approval_id: &str,
    input: &Value,
    reason: &str,
) -> ApprovalNotice {
    let device = device_name(hub, device_id);
    let thread = hub.db.thread(thread_id).map(|t| t.title).unwrap_or_default();
    let chat = hub.db.conversation(conversation_id).map(|c| c.title).unwrap_or_default();
    notice_from(&ApprovalFields {
        tool,
        device_name: &device,
        thread_title: &thread,
        chat_title: &chat,
        conversation_id,
        approval_id,
        input,
        reason,
    })
}

fn device_name(hub: &Hub, device_id: &str) -> String {
    if device_id == hub::server::HUB_DEVICE {
        return "Hub".to_string();
    }
    hub.db
        .devices()
        .into_iter()
        .find(|device| device.id == device_id)
        .map(|device| device.name)
        .unwrap_or_else(|| device_id.to_string())
}

fn show_toast(app: &AppHandle, notice: &ApprovalNotice) -> bool {
    match app
        .notification()
        .builder()
        .title(TITLE)
        .body(notice.body())
        .extra("conversation_id", &notice.conversation_id)
        .extra("approval_id", &notice.approval_id)
        .show()
    {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("could not show approval toast: {err}");
            false
        }
    }
}

fn on_action(app: &AppHandle, action: &ActionPerformed) {
    if action.action_id() != "tap" {
        return;
    }
    let Some(note) = action.notification() else {
        let _ = crate::show_main_window(app);
        return;
    };
    let conversation_id = note.extra().get("conversation_id").and_then(Value::as_str);
    let approval_id = note.extra().get("approval_id").and_then(Value::as_str);
    match (conversation_id, approval_id) {
        (Some(conversation_id), Some(approval_id)) => open_chat(app, conversation_id, approval_id),
        _ => {
            let _ = crate::show_main_window(app);
        }
    }
}

fn open_chat(app: &AppHandle, conversation_id: &str, approval_id: &str) {
    let hash = approval_hash(conversation_id, approval_id);
    let target = format!("{LOCAL_HUB_URL}/{hash}");
    let Ok(url) = target.parse() else {
        let _ = crate::show_main_window(app);
        return;
    };
    if let Some(win) = app.get_webview_window(crate::MAIN_WINDOW_LABEL) {
        let on_local_hub = win.url().ok().is_some_and(|current| {
            current.host_str() == Some("127.0.0.1") && current.port() == Some(crate::LOCAL_HUB_PORT)
        });
        if on_local_hub {
            if let Ok(hash_json) = serde_json::to_string(&hash) {
                let _ = win.eval(&format!("location.hash = {hash_json};"));
            }
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            return;
        }
    }
    let _ = crate::open_target_in_main_window(app, WebviewUrl::External(url));
}

/// Collapse whitespace and cap length, matching the web push payload.
fn field(value: &str, max: usize) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if max == 0 || count <= max {
        return collapsed;
    }
    let mut clipped: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn enc(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_toast_names_the_tool_device_thread_and_chat_only() {
        let command = "Remove-Item -Recurse C:\\secret";
        let input = json!({"command": command, "path": "C:\\Users\\secret\\keys.txt"});
        let reason = format!("runs `{command}` on the machine");
        let notice = notice_from(&ApprovalFields {
            tool: "run_shell",
            device_name: "Laptop",
            thread_title: "Pack the trip",
            chat_title: "Holiday",
            conversation_id: "conv1",
            approval_id: "a_abc",
            input: &input,
            reason: &reason,
        });
        let body = notice.body();
        let hash = notice.hash();
        assert_eq!(body, "run_shell on Laptop · Pack the trip · Holiday");
        assert_eq!(hash, "#/chat/conv1?approval=a_abc");
        assert!(!body.contains(command));
        assert!(!body.contains("keys.txt"));
        assert!(!body.contains("runs"));
        assert!(!hash.contains(command));
        assert_eq!(TITLE, "Grouper needs your review");
    }

    #[test]
    fn an_empty_tool_or_device_still_omits_the_input() {
        let input = json!({"argv": ["format", "C:"]});
        let notice = notice_from(&ApprovalFields {
            tool: "  ",
            device_name: "",
            thread_title: "",
            chat_title: "Chat",
            conversation_id: "c/1",
            approval_id: "a b",
            input: &input,
            reason: "format C:",
        });
        assert_eq!(notice.body(), "A tool · Chat");
        assert_eq!(notice.hash(), "#/chat/c%2F1?approval=a%20b");
        assert!(!notice.body().contains("format"));
    }

    #[test]
    fn long_names_are_clipped_without_the_rest_of_the_string() {
        let long = "n".repeat(90);
        let notice = notice_from(&ApprovalFields {
            tool: &long,
            device_name: "PC",
            thread_title: "",
            chat_title: "",
            conversation_id: "c",
            approval_id: "a",
            input: &Value::Null,
            reason: "",
        });
        let body = notice.body();
        assert!(body.contains('…'));
        assert!(body.contains(" on PC"));
        assert!(!body.contains(&"n".repeat(80)));
    }
}
