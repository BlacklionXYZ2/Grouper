//! When the target device is offline (Proposal v3): the thread waits for its device
//! while the hub does what it can, up to a checkpoint, then hands the work over.
//!
//! - **Case A**, the device is offline when the orchestrator starts a thread on it,
//!   and **case B**, the device disconnects while a thread runs on it, both put the
//!   thread in status [`WAITING_DEVICE`]. The thread keeps its target device.
//! - The hub starts the thread's **hub part**: the same brief (plus, in case B, a
//!   capped summary of its progress from the event log) and [`offline_instruction`],
//!   with file tools in the hub workspace (`<data>/workspaces/<thread>`). The model
//!   defines a checkpoint it can reach without the device, does that work, and ends
//!   with a checkpoint report (at once, when the device is needed for everything).
//! - When the device connects again (its tools known), the hub copies the hub
//!   workspace to the device ([`HubMessage::WorkspaceFile`], capped) — asking first
//!   when the device's `write_file` is on Manual — and continues the **same thread
//!   id** there with the brief, the checkpoint report, messages that arrived while it
//!   waited, and "continue from the checkpoint". The device's approval gate applies.
//! - Messages to a waiting thread go to its hub part while it runs, else they're
//!   queued for the continuation. Waiting threads can be stopped.
//! - Hub restart: waiting threads keep waiting; a hub part that was running is
//!   recorded as stopped at "the hub restarted during the checkpoint".
//!
//! The orchestrator learns of each step through a tool result or a wake-up, and the
//! user through [`EventBody::ThreadWaiting`], [`EventBody::ThreadCheckpoint`] and
//! [`EventBody::ThreadHandedOver`]. The hub prepares at once; a configurable wait
//! (for the user to answer the orchestrator first) is for later.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine;
use core_agent::thread::{ThreadResult, spawn_thread};
use core_agent::tools::{Workspace, file_tools};
use core_protocol::{AgentEvent, EventBody, HubMessage, Permission, ThreadInfo, ThreadSpec};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::db::{Db, now};
use crate::orchestrator::Wake;
use crate::server::{ApprovalAnswer, HUB_DEVICE, HandoverAckRoute, Hub};
use crate::threads::{HubHost, MAX_THREAD_TURNS, thread_system};

/// Thread status while its device is offline.
pub const WAITING_DEVICE: &str = "waiting_device";

/// Added to `start_thread`'s result when the device is offline.
pub(crate) const WAITING_NOTE: &str = "The device is offline, so the thread waits for it (status waiting_device). \
Meanwhile the hub does what it can without the device, up to a checkpoint, and hands the work over when the device \
reconnects. Tell the user; if they'd rather use another device or cancel, stop_thread it.";

/// The checkpoint report of a hub part that the hub's restart cut short.
pub const HUB_RESTARTED: &str = "The hub restarted during the checkpoint, so the hub part stopped there and no \
further hub work will be done. Files it had written are handed over with the thread.";

/// The checkpoint of a thread whose device reconnected before the hub noticed it had gone.
const RECONNECTED: &str = "The device's connection dropped and came back before the hub noticed, so no hub work \
was done.";

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS offline_threads (
  thread_id TEXT PRIMARY KEY, phase TEXT NOT NULL, disconnected INTEGER NOT NULL DEFAULT 0,
  progress TEXT, checkpoint TEXT, queued TEXT NOT NULL DEFAULT '[]', created INTEGER NOT NULL);
";

/// The hub part is running (or about to).
const PREPARING: &str = "preparing";
/// The hub part reached its checkpoint; waiting for the device.
const READY: &str = "ready";
/// Files are being copied to the device.
const HANDING_OVER: &str = "handing_over";

/// Most bytes copied to the device in one hand-over.
pub const MAX_HANDOVER_BYTES: u64 = 20 * 1024 * 1024;
/// Largest single file copied (it travels in one WebSocket frame, base64-encoded).
pub const MAX_HANDOVER_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HANDOVER_FILES: usize = 500;
/// Case B: how much of the thread's progress the hub part (and the continuation) sees.
const PROGRESS_ITEMS: usize = 20;
const PROGRESS_ITEM_CHARS: usize = 600;
const PROGRESS_CHARS: usize = 6_000;
/// How long a reconnecting device has to list its tools before the hand-over goes ahead anyway.
const TOOLS_WAIT: Duration = Duration::from_secs(60);
/// Total time the hub waits for a device's `WorkspaceFileResult` acknowledgements.
pub const HANDOVER_ACK_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum queued messages kept per waiting thread before oldest ones are dropped.
pub const MAX_QUEUED_MESSAGES: usize = 50;

/// The instruction the harness adds to a hub part's brief.
pub fn offline_instruction(device: &str, disconnected: bool) -> String {
    let what = if disconnected {
        format!("\"{device}\" disconnected while this thread was running on it (its progress is above)")
    } else {
        format!("\"{device}\" is disconnected")
    };
    format!(
        "\n## Grouper: the target device is offline\n\
The target device {what}. You're running on the Grouper hub instead, with file tools in a hub workspace and no \
shell or device tools.\n\n\
Define the next checkpoint: the furthest point this task can reach without {device}. Implement it in your hub \
workspace, then stop and wait until the device is available again: reply with a checkpoint report (the checkpoint \
you set, what you did, the files you wrote, and what's left to do on {device}). That reply ends this part.\n\n\
If nothing useful can be done without {device} (for example the task needs its shell, its files or its apps), \
reply at once with a short checkpoint report saying so.\n\n\
When {device} reconnects, Grouper copies your workspace files to it and the thread continues there from your \
checkpoint.\n"
    )
}

#[derive(Debug, Default, Clone)]
struct Waiting {
    disconnected: bool,
    progress: Option<String>,
    checkpoint: Option<String>,
    queued: Vec<String>,
}

impl Db {
    pub(crate) fn thread_brief(&self, id: &str) -> Option<String> {
        self.c().query_row("SELECT brief FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional().ok().flatten()
    }

    fn set_thread_status(&self, id: &str, from: &str, to: &str) -> bool {
        self.c()
            .execute("UPDATE threads SET status = ?1 WHERE id = ?2 AND status = ?3", params![to, id, from])
            .is_ok_and(|n| n > 0)
    }

    fn waiting(&self, id: &str) -> Option<Waiting> {
        self.c()
            .query_row(
                "SELECT disconnected, progress, checkpoint, queued FROM offline_threads WHERE thread_id = ?1",
                [id],
                |r| {
                    let queued: String = r.get(3)?;
                    Ok(Waiting {
                        disconnected: r.get::<_, i64>(0)? != 0,
                        progress: r.get(1)?,
                        checkpoint: r.get(2)?,
                        queued: serde_json::from_str(&queued).unwrap_or_default(),
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    fn put_waiting(&self, id: &str, phase: &str, disconnected: bool, progress: Option<&str>, checkpoint: Option<&str>) {
        let _ = self.c().execute(
            "INSERT OR REPLACE INTO offline_threads (thread_id, phase, disconnected, progress, checkpoint, queued, created)
             VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6)",
            params![id, phase, disconnected, progress, checkpoint, now() as i64],
        );
        if let Some(c) = checkpoint {
            self.set_waiting_summary(id, c);
        }
    }

    fn set_phase(&self, id: &str, from: &str, to: &str) -> bool {
        self.c()
            .execute("UPDATE offline_threads SET phase = ?1 WHERE thread_id = ?2 AND phase = ?3", params![to, id, from])
            .is_ok_and(|n| n > 0)
    }

    /// The hub part ended: record its report (also as the thread's summary for now).
    fn set_checkpoint(&self, id: &str, report: &str) -> bool {
        let done = self
            .c()
            .execute(
                "UPDATE offline_threads SET phase = ?1, checkpoint = ?2 WHERE thread_id = ?3 AND phase = ?4",
                params![READY, report, id, PREPARING],
            )
            .is_ok_and(|n| n > 0);
        if done {
            self.set_waiting_summary(id, report);
        }
        done
    }

    fn set_waiting_summary(&self, id: &str, summary: &str) {
        let _ = self.c().execute(
            "UPDATE threads SET summary = ?1 WHERE id = ?2 AND status = ?3",
            params![summary, id, WAITING_DEVICE],
        );
    }

    fn queue_message(&self, id: &str, message: &str) -> bool {
        let c = self.c();
        let Ok(Some(queued)) = c
            .query_row("SELECT queued FROM offline_threads WHERE thread_id = ?1", [id], |r| r.get::<_, String>(0))
            .optional()
        else {
            return false;
        };
        let mut list: Vec<String> = serde_json::from_str(&queued).unwrap_or_default();
        let mut dropped = list.first().and_then(|s| parse_dropped_note(s)).unwrap_or(0);
        if dropped > 0 {
            list.remove(0);
        }
        list.push(message.to_string());
        if list.len() > MAX_QUEUED_MESSAGES {
            let extra = list.len() - MAX_QUEUED_MESSAGES;
            list.drain(0..extra);
            dropped += extra;
        }
        if dropped > 0 {
            list.insert(0, format_dropped_note(dropped));
        }
        c.execute(
            "UPDATE offline_threads SET queued = ?1 WHERE thread_id = ?2",
            params![serde_json::to_string(&list).unwrap_or_default(), id],
        )
        .is_ok_and(|n| n > 0)
    }

    fn drop_waiting(&self, id: &str) {
        let _ = self.c().execute("DELETE FROM offline_threads WHERE thread_id = ?1", [id]);
    }

    /// Threads waiting for a device, with their phase (`preparing`, `ready`, `handing_over`).
    pub fn waiting_threads(&self) -> Vec<(ThreadInfo, String)> {
        let rows: Vec<(String, String)> = {
            let c = self.c();
            let mut st = c.prepare("SELECT thread_id, phase FROM offline_threads ORDER BY created").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().filter_map(Result::ok).collect()
        };
        rows.into_iter()
            .filter_map(|(id, phase)| self.thread(&id).filter(|t| t.status == WAITING_DEVICE).map(|t| (t, phase)))
            .collect()
    }
}

fn parse_dropped_note(s: &str) -> Option<usize> {
    let rest = s.strip_prefix('(')?;
    let (num, tail) = rest.split_once(" earlier message")?;
    tail.contains("dropped").then(|| num.parse().ok()).flatten()
}

fn format_dropped_note(dropped: usize) -> String {
    let plural = if dropped == 1 { "" } else { "s" };
    format!("({dropped} earlier message{plural} dropped; queue limit is {MAX_QUEUED_MESSAGES})")
}

fn hub_workspace(data_dir: &Path, thread_id: &str) -> PathBuf {
    data_dir.join("workspaces").join(thread_id)
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.replace(['\r', '\n'], " ");
    if flat.chars().count() > max { format!("{}…", flat.chars().take(max).collect::<String>()) } else { flat }
}

/// The thread's recent steps from the event log, most recent last, capped.
fn progress_summary(db: &Db, t: &ThreadInfo) -> String {
    let n = PROGRESS_ITEM_CHARS;
    let items: Vec<String> = db
        .events(&t.conversation_id, 0, Some(&t.id))
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ThreadActivity { event: AgentEvent::Text { text } } => Some(format!("- Said: {}", clip(&text, n))),
            EventBody::ThreadActivity { event: AgentEvent::ToolCall { name, input, .. } } => {
                Some(format!("- Called {name}: {}", clip(&input.to_string(), n)))
            }
            EventBody::ThreadActivity { event: AgentEvent::ToolResult { name, ok, output, .. } } => {
                Some(format!("- {name} {}: {}", if ok { "returned" } else { "failed" }, clip(&output, n)))
            }
            EventBody::ThreadInput { from, text } => Some(format!("- Message from the {from}: {}", clip(&text, n))),
            EventBody::ThreadCheckpoint { summary, .. } => Some(format!("- Earlier hub checkpoint: {}", clip(&summary, n))),
            _ => None,
        })
        .collect();
    let mut kept: Vec<&String> = Vec::new();
    let mut size = 0;
    for item in items.iter().rev().take(PROGRESS_ITEMS) {
        if size + item.len() > PROGRESS_CHARS {
            break;
        }
        size += item.len() + 1;
        kept.push(item);
    }
    kept.reverse();
    let mut out = kept.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n");
    if kept.len() < items.len() {
        out.insert_str(0, "(earlier steps left out)\n");
    }
    out
}

struct HandoverFile {
    /// Workspace-relative, with `/`.
    rel: String,
    path: PathBuf,
}

/// The hub workspace's files, within the caps; what's left out is described in the second list.
fn collect_files(root: &Path) -> (Vec<HandoverFile>, Vec<String>, u64) {
    let mut found: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if kind.is_dir() {
                dirs.push((entry.path(), rel));
            } else if kind.is_file() {
                found.push((rel, entry.path(), entry.metadata().map(|m| m.len()).unwrap_or(0)));
            }
            // Symlinks are left out: they could point outside the workspace.
        }
    }
    found.sort();
    let (mut files, mut skipped, mut total) = (Vec::new(), Vec::new(), 0u64);
    let mb = |b: u64| format!("{:.1} MB", b as f64 / 1_048_576.0);
    for (rel, path, size) in found {
        if size > MAX_HANDOVER_FILE_BYTES {
            skipped.push(format!("{rel} ({}, over the {} per-file limit)", mb(size), mb(MAX_HANDOVER_FILE_BYTES)));
        } else if total + size > MAX_HANDOVER_BYTES {
            skipped.push(format!("{rel} ({}, over the {} total limit)", mb(size), mb(MAX_HANDOVER_BYTES)));
        } else if files.len() >= MAX_HANDOVER_FILES {
            skipped.push(format!("{rel} (over the {MAX_HANDOVER_FILES}-file limit)"));
        } else {
            total += size;
            files.push(HandoverFile { rel, path });
        }
    }
    (files, skipped, total)
}

impl Hub {
    fn device_name(&self, device_id: &str) -> String {
        self.resolve_device(device_id).map(|(_, name)| name).unwrap_or_else(|| device_id.to_string())
    }

    /// Put a running thread in `waiting_device`. Returns it, or `None` if it wasn't running.
    fn enter_waiting(
        &self,
        thread_id: &str,
        reason: &str,
        disconnected: bool,
        phase: &str,
        checkpoint: Option<&str>,
    ) -> Option<ThreadInfo> {
        if !self.db.set_thread_status(thread_id, "running", WAITING_DEVICE) {
            return None;
        }
        let t = self.db.thread(thread_id)?;
        let progress = disconnected.then(|| progress_summary(&self.db, &t)).filter(|p| !p.is_empty());
        self.db.put_waiting(thread_id, phase, disconnected, progress.as_deref(), checkpoint);
        self.emit(
            &t.conversation_id,
            Some(thread_id),
            EventBody::ThreadWaiting { device_id: t.device_id.clone(), reason: reason.into() },
        );
        Some(t)
    }

    /// The thread's device is offline (case A) or went away mid-thread (case B,
    /// `disconnected`): wait for it and start the hub part. Boxed because the hub
    /// part's end can lead back here (a hand-over that finds the device gone again).
    pub(crate) fn wait_for_device<'a>(
        self: &'a Arc<Self>,
        thread_id: &'a str,
        reason: &'a str,
        disconnected: bool,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.wait_for_device_inner(thread_id, reason, disconnected))
    }

    async fn wait_for_device_inner(self: &Arc<Self>, thread_id: &str, reason: &str, disconnected: bool) {
        let Some(t) = self.enter_waiting(thread_id, reason, disconnected, PREPARING, None) else { return };
        if disconnected {
            let device = self.device_name(&t.device_id);
            let details = format!(
                "{reason}. The hub is doing what it can without {device}, up to a checkpoint; the thread continues on \
                 {device} when it reconnects. Stop it with stop_thread if waiting isn't what the user wants."
            );
            self.wake(
                &t.conversation_id,
                Wake::ThreadUpdate {
                    thread_id: t.id.clone(),
                    title: t.title.clone(),
                    what: format!("is waiting for {device}"),
                    details,
                },
            )
            .await;
        }
        if let Err(e) = self.start_hub_part(&t).await {
            self.checkpoint_reached(&t.id, false, &e).await;
        }
    }

    /// Run the hub part of a waiting thread: its brief, progress and the offline instruction.
    async fn start_hub_part(self: &Arc<Self>, t: &ThreadInfo) -> Result<(), String> {
        let device = self.device_name(&t.device_id);
        let w = self.db.waiting(&t.id).unwrap_or_default();
        let mut brief = self.db.thread_brief(&t.id).unwrap_or_default();
        if let Some(p) = &w.progress {
            brief.push_str(&format!("\n## Progress on {device} before it disconnected\n{p}\n"));
        }
        brief.push_str(&offline_instruction(&device, w.disconnected));

        let model = self.db.find_model(&t.model).ok_or_else(|| format!("model {} is no longer registered", t.model))?;
        if self.harness_placement(&model)?.is_some() {
            return Err(format!(
                "{device} hosts this harness thread; the hub can't run that CLI while the device is offline"
            ));
        }
        let pcfg = self
            .db
            .provider(&model.provider_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no provider {}", model.provider_id))?;
        let provider = self.build_model_provider(&pcfg, &model.model).map_err(|e| e.to_string())?;
        let ws = Workspace::create(hub_workspace(&self.data_dir, &t.id)).map_err(|e| format!("workspace: {e}"))?;
        let spec = ThreadSpec {
            thread_id: t.id.clone(),
            conversation_id: t.conversation_id.clone(),
            title: t.title.clone(),
            provider_id: model.provider_id.clone(),
            model: model.model.clone(),
            system: thread_system(&device, true, &[]),
            brief,
            // The hub's own (file) tools, not the device's.
            permissions: self.db.permissions(HUB_DEVICE),
            max_turns: MAX_THREAD_TURNS,
            device_name: "Hub".into(),
            ask_timeout_secs: self.db.limits().ask_timeout_secs,
            fallback_models: self.free_alternates(&model.provider_id, &model.model),
            harness: None,
        };
        let host = Arc::new(HubHost { hub: Arc::downgrade(self) });
        let (control, handle) = spawn_thread(spec, provider, file_tools(&ws), host, ws.clone());
        self.hub_threads.lock().await.insert(t.id.clone(), control);
        let (hub, id) = (self.clone(), t.id.clone());
        tokio::spawn(async move {
            let result = handle
                .await
                .unwrap_or_else(|e| ThreadResult { ok: false, summary: format!("thread crashed: {e}") });
            hub.hub_threads.lock().await.remove(&id);
            hub.checkpoint_reached(&id, result.ok, &result.summary).await;
        });
        Ok(())
    }

    /// The hub part ended: keep its report, tell everyone, and hand over if the device is back.
    async fn checkpoint_reached(self: &Arc<Self>, thread_id: &str, ok: bool, summary: &str) {
        let report = match (ok, summary.trim()) {
            (true, "") => "(The hub part ended without a report.)".to_string(),
            (true, s) => s.to_string(),
            (false, s) => format!("The hub part ended without reaching a checkpoint: {s}"),
        };
        // Not when it was stopped meanwhile (its record is gone) or already recorded.
        if !self.db.set_checkpoint(thread_id, &report) {
            return;
        }
        let Some(t) = self.db.thread(thread_id) else { return };
        let device = self.device_name(&t.device_id);
        self.emit(
            &t.conversation_id,
            Some(thread_id),
            EventBody::ThreadCheckpoint { device_id: t.device_id.clone(), summary: report.clone() },
        );
        self.wake(
            &t.conversation_id,
            Wake::ThreadUpdate {
                thread_id: t.id.clone(),
                title: t.title.clone(),
                what: format!("reached its hub checkpoint and waits for {device}"),
                details: report,
            },
        )
        .await;
        if self.tools_known(&t.device_id).await {
            let (hub, id) = (self.clone(), t.id.clone());
            tokio::spawn(async move { hub.hand_over(&id).await });
        }
    }

    /// Online, with its MCP session up (so the hub knows its tools).
    async fn tools_known(&self, device_id: &str) -> bool {
        self.live.lock().await.get(device_id).is_some_and(|l| l.mcp.is_some())
    }

    /// A message for a waiting thread: to its hub part while that runs, else queued
    /// for the continuation on the device.
    pub(crate) async fn waiting_thread_input(&self, t: &ThreadInfo, from: &str, text: &str) -> Result<(), String> {
        let live = match self.hub_threads.lock().await.get(&t.id) {
            Some(c) => {
                c.send(from, text);
                true
            }
            None => false,
        };
        if !live && !self.db.queue_message(&t.id, &format!("[Message from the {from} while the thread waited]\n{text}"))
        {
            return Err(format!("thread {} isn't reachable right now", t.id));
        }
        self.emit(&t.conversation_id, Some(&t.id), EventBody::ThreadInput { from: from.into(), text: text.into() });
        Ok(())
    }

    /// The device is back: copy the hub's files over and continue the thread there.
    pub async fn hand_over(self: &Arc<Self>, thread_id: &str) {
        if !self.db.set_phase(thread_id, READY, HANDING_OVER) {
            return;
        }
        if let Err(e) = self.try_hand_over(thread_id).await {
            // Still waiting: the next connection tries again.
            self.db.set_phase(thread_id, HANDING_OVER, READY);
            tracing::info!(thread = %thread_id, "hand-over postponed: {e}");
        }
    }

    async fn try_hand_over(self: &Arc<Self>, thread_id: &str) -> Result<(), String> {
        let t = self.db.thread(thread_id).filter(|t| t.status == WAITING_DEVICE).ok_or("no longer waiting")?;
        let w = self.db.waiting(thread_id).ok_or("no waiting record")?;
        let device = self.device_name(&t.device_id);
        // Case B: the device's workspace still has its own files; the hub's go beside them.
        let prefix = if w.disconnected { "hub/" } else { "" };
        let (mut files, mut skipped, total) = collect_files(&hub_workspace(&self.data_dir, thread_id));
        let mut names: Vec<String> = files.iter().map(|f| format!("{prefix}{}", f.rel)).collect();

        // Writing into the thread's own workspace is `write_file`: ask when that's on Manual.
        let level =
            self.db.permissions(&t.device_id).get("write_file").copied().unwrap_or(Permission::default_for("write_file"));
        if !files.is_empty() && level == Permission::Manual {
            let approval_id = Hub::new_approval_id();
            let (tx, rx) = oneshot::channel();
            let reason = format!("copy {} file(s) the hub prepared into this thread's workspace on {device}", files.len());
            let input = json!({"files": names, "bytes": total});
            self.request_approval(&t.device_id, thread_id, &approval_id, "write_file", input, &reason, Some(tx)).await;
            if !matches!(rx.await, Ok(ApprovalAnswer::Allow)) {
                if !self.is_online(&t.device_id).await {
                    return Err("the device went offline during the approval".into());
                }
                skipped.push("every file: copying them was denied".into());
                files.clear();
                names.clear();
            }
        }
        let Some(conn_id) = self.live.lock().await.get(&t.device_id).map(|l| l.conn_id) else {
            return Err("the device is offline again".into());
        };
        let mut copied = Vec::new();
        let mut unconfirmed = Vec::new();
        if !files.is_empty() {
            let (ack_tx, mut ack_rx) = mpsc::channel(files.len().saturating_add(16));
            self.handover_acks.lock().await.insert(
                thread_id.to_string(),
                HandoverAckRoute { device_id: t.device_id.clone(), conn_id, tx: ack_tx },
            );
            let mut sent = Vec::new();
            let mut send_err = None;
            for (f, name) in files.iter().zip(names) {
                let bytes = match std::fs::read(&f.path) {
                    Ok(b) => b,
                    Err(e) => {
                        skipped.push(format!("{} (unreadable: {e})", f.rel));
                        continue;
                    }
                };
                let content_base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                let frame = HubMessage::WorkspaceFile { thread_id: thread_id.into(), path: name.clone(), content_base64 };
                if !self.send_to_conn(&t.device_id, conn_id, frame).await {
                    send_err = Some("the device went offline while files were copied".to_string());
                    break;
                }
                sent.push(name);
            }
            if let Some(err) = send_err {
                self.handover_acks.lock().await.remove(thread_id);
                return Err(err);
            }
            let mut pending: HashSet<String> = sent.iter().cloned().collect();
            let mut results: HashMap<String, (bool, Option<String>)> = HashMap::new();
            let deadline = tokio::time::Instant::now() + self.handover_ack_timeout();
            let mut disconnected = false;
            while !pending.is_empty() {
                match tokio::time::timeout_at(deadline, ack_rx.recv()).await {
                    Err(_) => break,
                    Ok(None) => {
                        disconnected = true;
                        break;
                    }
                    Ok(Some((path, ok, error))) => {
                        if pending.remove(&path) {
                            results.insert(path, (ok, error));
                        }
                    }
                }
            }
            self.handover_acks.lock().await.remove(thread_id);
            if disconnected {
                return Err("the device went offline while files were copied".into());
            }
            for path in sent {
                match results.remove(&path) {
                    Some((true, _)) => copied.push(path),
                    Some((false, err)) => {
                        let why = err.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "write failed".into());
                        skipped.push(format!("{path} ({why})"));
                    }
                    None => unconfirmed.push(path),
                }
            }
        }
        if !self.is_live_conn(&t.device_id, conn_id).await {
            return Err("the device went offline during the hand-over".into());
        }

        let Some(model) = self.db.find_model(&t.model).or_else(|| self.db.default_model("thread")) else {
            return Err("no models registered".into());
        };
        // Stopped meanwhile: nothing to continue.
        if !self.db.set_thread_status(thread_id, WAITING_DEVICE, "running") {
            return Ok(());
        }
        self.db.drop_waiting(thread_id);
        let original = self.db.thread_brief(thread_id).unwrap_or_default();
        let brief = continuation_brief(&original, &w, &device, prefix, &copied, &skipped, &unconfirmed);
        let mut event_skipped = skipped.clone();
        event_skipped.extend(unconfirmed.iter().map(|p| format!("{p} (unconfirmed)")));
        self.emit(
            &t.conversation_id,
            Some(thread_id),
            EventBody::ThreadHandedOver {
                device_id: t.device_id.clone(),
                files: copied.clone(),
                skipped: event_skipped,
                brief: brief.clone(),
            },
        );
        let spec = ThreadSpec {
            thread_id: thread_id.into(),
            conversation_id: t.conversation_id.clone(),
            title: t.title.clone(),
            provider_id: model.provider_id.clone(),
            model: model.model.clone(),
            system: thread_system(&device, false, &self.db.device_folders(&t.device_id)),
            brief,
            permissions: self.db.permissions(&t.device_id),
            max_turns: MAX_THREAD_TURNS,
            device_name: device.clone(),
            ask_timeout_secs: self.db.limits().ask_timeout_secs,
            fallback_models: self.free_alternates(&model.provider_id, &model.model),
            harness: match self.harness_placement(&model)? {
                Some(place) => Some(place.spec(self.db.thread_harness_session(thread_id))),
                None => None,
            },
        };
        if !self.send_to_conn(&t.device_id, conn_id, HubMessage::StartThread { spec: Box::new(spec) }).await {
            self.wait_for_device(thread_id, "the device went offline during the hand-over", true).await;
            return Ok(());
        }
        let mut details = format!(
            "{device} reconnected, so the thread continues there from its hub checkpoint, with {} file(s) from the hub \
             workspace{}.",
            copied.len(),
            if prefix.is_empty() { String::new() } else { format!(" (under {prefix})") }
        );
        if !skipped.is_empty() {
            details.push_str(&format!(" Not copied: {}.", skipped.join("; ")));
        }
        if !unconfirmed.is_empty() {
            details.push_str(&format!(" Unconfirmed: {}.", unconfirmed.join("; ")));
        }
        self.wake(
            &t.conversation_id,
            Wake::ThreadUpdate { thread_id: t.id, title: t.title, what: format!("continues on {device}"), details },
        )
        .await;
        Ok(())
    }

    /// After a hub restart: waiting threads keep waiting. A hub part that was running
    /// is recorded as cut short, with no further hub work; an interrupted hand-over
    /// is retried when the device connects.
    pub fn recover_waiting_threads(&self) {
        for (t, phase) in self.db.waiting_threads() {
            if phase == PREPARING && self.db.set_checkpoint(&t.id, HUB_RESTARTED) {
                self.emit(
                    &t.conversation_id,
                    Some(&t.id),
                    EventBody::ThreadCheckpoint { device_id: t.device_id.clone(), summary: HUB_RESTARTED.into() },
                );
            } else if phase == HANDING_OVER {
                self.db.set_phase(&t.id, HANDING_OVER, READY);
            }
        }
        // A waiting thread without a record (shouldn't happen) still gets handed over.
        for t in self.db.threads(None).into_iter().filter(|t| t.status == WAITING_DEVICE) {
            if self.db.waiting(&t.id).is_none() {
                self.db.put_waiting(&t.id, READY, false, None, Some(HUB_RESTARTED));
            }
        }
    }
}

fn continuation_brief(
    original: &str,
    w: &Waiting,
    device: &str,
    prefix: &str,
    files: &[String],
    skipped: &[String],
    unconfirmed: &[String],
) -> String {
    let mut s = original.trim_end().to_string();
    s.push('\n');
    if let Some(p) = &w.progress {
        s.push_str(&format!("\n## Progress on {device} before it disconnected\n{p}\n"));
    }
    let checkpoint = w.checkpoint.as_deref().unwrap_or("(none recorded)");
    s.push_str(&format!("\n## Checkpoint report from the hub\n{checkpoint}\n\n## Files from the hub\n"));
    if files.is_empty() {
        s.push_str("None.\n");
    }
    for f in files {
        s.push_str(&format!("- {f}\n"));
    }
    if !skipped.is_empty() {
        s.push_str(&format!("Not copied: {}.\n", skipped.join("; ")));
    }
    if !unconfirmed.is_empty() {
        s.push_str(&format!("Unconfirmed (device did not acknowledge): {}.\n", unconfirmed.join("; ")));
    }
    if !w.queued.is_empty() {
        s.push_str("\n## Messages that arrived while the thread waited\n");
        s.push_str(&w.queued.join("\n\n"));
        s.push('\n');
    }
    let place = if prefix.is_empty() {
        "in your workspace".to_string()
    } else {
        format!("in your workspace under {prefix} (your own earlier files are where you left them)")
    };
    s.push_str(&format!(
        "\n[Grouper] {device} is available again and this thread continues here. Continue from the checkpoint; the \
         files from the hub are {place}.\n"
    ));
    s
}

/// A device connected (`server.rs`): threads its previous connection left running
/// wait for it, and once its tools are known every waiting thread is handed over
/// (or, if its hub part is still running, told to wrap up).
pub(crate) async fn device_connected(hub: Weak<Hub>, device_id: String, conn_id: u64, orphans: Vec<ThreadInfo>) {
    {
        let Some(h) = hub.upgrade() else { return };
        for t in orphans {
            let reason = "the device's connection dropped mid-thread and it reconnected";
            h.enter_waiting(&t.id, reason, true, READY, Some(RECONNECTED));
        }
        if !h.db.waiting_threads().iter().any(|(t, _)| t.device_id == device_id) {
            return;
        }
    }
    let deadline = tokio::time::Instant::now() + TOOLS_WAIT;
    loop {
        let Some(h) = hub.upgrade() else { return };
        {
            let live = h.live.lock().await;
            match live.get(&device_id) {
                Some(l) if l.conn_id == conn_id => {
                    if l.mcp.is_some() && !l.tools.is_empty() {
                        break;
                    }
                }
                _ => return,
            }
        }
        drop(h);
        if tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let Some(h) = hub.upgrade() else { return };
    let device = h.device_name(&device_id);
    for (t, phase) in h.db.waiting_threads().into_iter().filter(|(t, _)| t.device_id == device_id) {
        if phase == READY {
            let (hub, id) = (h.clone(), t.id.clone());
            tokio::spawn(async move { hub.hand_over(&id).await });
        } else if phase == PREPARING {
            let text = format!(
                "{device} is back online. Finish the step you're on, then stop and reply with your checkpoint report."
            );
            if let Some(c) = h.hub_threads.lock().await.get(&t.id) {
                c.send("hub", &text);
            }
            h.emit(&t.conversation_id, Some(&t.id), EventBody::ThreadInput { from: "hub".into(), text });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("grouper-offline-test-{}", crate::db::random_hex(4)))
    }

    /// A hub restart leaves waiting threads waiting: a hub part that was running is
    /// recorded as cut short (no more hub work), one that had finished is untouched,
    /// an interrupted hand-over is retried, and running threads are another matter.
    #[tokio::test]
    async fn restart_keeps_waiting_threads() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let preparing = db.create_thread("c1", "prep", "laptop", "m", None, "b").unwrap();
        let ready = db.create_thread("c1", "ready", "laptop", "m", None, "b").unwrap();
        let handing = db.create_thread("c1", "handing", "laptop", "m", None, "b").unwrap();
        for (t, phase, checkpoint) in
            [(&preparing, PREPARING, None), (&ready, READY, Some("did step 1")), (&handing, HANDING_OVER, Some("did it"))]
        {
            assert!(db.set_thread_status(&t.id, "running", WAITING_DEVICE));
            db.put_waiting(&t.id, phase, false, None, checkpoint);
        }
        drop(db);

        // The restarted hub.
        let hub = Hub::new(Db::open(&dir).unwrap(), dir.clone());
        hub.recover_waiting_threads();
        let waiting = hub.db.waiting_threads();
        let phase = |id: &str| {
            let phase = waiting.iter().find(|(t, _)| t.id == id).map(|(_, p)| p.clone())?;
            Some((phase, hub.db.waiting(id)?.checkpoint))
        };
        assert_eq!(phase(&preparing.id), Some((READY.into(), Some(HUB_RESTARTED.into()))));
        assert_eq!(phase(&ready.id), Some((READY.into(), Some("did step 1".into()))));
        assert_eq!(phase(&handing.id), Some((READY.into(), Some("did it".into()))));
        for t in [&preparing, &ready, &handing] {
            assert_eq!(hub.db.thread(&t.id).unwrap().status, WAITING_DEVICE);
        }
        assert_eq!(hub.db.thread(&preparing.id).unwrap().summary.as_deref(), Some(HUB_RESTARTED));
        let events = hub.db.events("c1", 0, Some(&preparing.id));
        assert!(events.iter().any(|e| matches!(&e.body, EventBody::ThreadCheckpoint { summary, .. } if summary == HUB_RESTARTED)));
        // No hub part was started again.
        assert!(hub.hub_threads.lock().await.is_empty());
        // Running again is idempotent.
        hub.recover_waiting_threads();
        assert_eq!(hub.db.events("c1", 0, Some(&preparing.id)).len(), events.len());
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn handover_files_are_capped() {
        let dir = tmp();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), b"bb").unwrap();
        let big = std::fs::File::create(dir.join("big.bin")).unwrap();
        big.set_len(MAX_HANDOVER_FILE_BYTES + 1).unwrap();
        drop(big);
        let (files, skipped, total) = collect_files(&dir);
        let names: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "sub/b.txt"]);
        assert_eq!(total, 3);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].starts_with("big.bin (8.0 MB, over the 8.0 MB per-file limit)"), "{skipped:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn progress_keeps_the_latest_steps() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let t = db.create_thread("c1", "t", "laptop", "m", None, "b").unwrap();
        for i in 0..30 {
            db.add_event("c1", Some(&t.id), EventBody::ThreadActivity { event: AgentEvent::Text { text: format!("step {i}") } });
        }
        db.add_event(
            "c1",
            Some(&t.id),
            EventBody::ThreadActivity {
                event: AgentEvent::ToolResult { call_id: "1".into(), name: "read_file".into(), ok: true, output: "x".repeat(5000) },
            },
        );
        let p = progress_summary(&db, &t);
        assert!(p.starts_with("(earlier steps left out)\n"), "{p}");
        assert!(!p.contains("step 10\n") && p.contains("step 29"), "{p}");
        assert!(p.contains("- read_file returned: xxx") && p.len() < PROGRESS_CHARS + 100, "{}", p.len());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn queued_messages_are_capped_with_note() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let t = db.create_thread("c1", "t", "laptop", "m", None, "b").unwrap();
        assert!(db.set_thread_status(&t.id, "running", WAITING_DEVICE));
        db.put_waiting(&t.id, READY, false, None, Some("ok"));

        for i in 1..=MAX_QUEUED_MESSAGES {
            assert!(db.queue_message(&t.id, &format!("msg {i}")));
        }
        let w = db.waiting(&t.id).unwrap();
        assert_eq!(w.queued.len(), MAX_QUEUED_MESSAGES);
        assert_eq!(w.queued.first().map(String::as_str), Some("msg 1"));
        assert_eq!(w.queued.last().map(String::as_str), Some("msg 50"));

        for i in (MAX_QUEUED_MESSAGES + 1)..=(MAX_QUEUED_MESSAGES + 5) {
            assert!(db.queue_message(&t.id, &format!("msg {i}")));
        }
        let w = db.waiting(&t.id).unwrap();
        assert_eq!(w.queued.len(), MAX_QUEUED_MESSAGES + 1);
        assert!(w.queued[0].contains("5 earlier messages dropped"), "{:?}", w.queued[0]);
        assert_eq!(w.queued[1], "msg 6");
        assert_eq!(w.queued.last().map(String::as_str), Some("msg 55"));
        std::fs::remove_dir_all(dir).ok();
    }
}
