//! Starting, steering and finishing subagent threads, on devices or on the hub itself.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use core_agent::thread::{ThreadHost, spawn_thread};
use core_agent::tools::{Workspace, file_tools};
use core_protocol::{AgentEvent, EventBody, HubMessage, ThreadInfo, ThreadSpec};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::orchestrator::Wake;
use crate::server::{ApprovalRoute, HUB_DEVICE, Hub};

/// What the orchestrator asks for when it starts a thread.
#[derive(Debug, Default)]
pub struct ThreadRequest {
    pub title: String,
    pub device: String,
    pub goal: String,
    pub context: String,
    pub constraints: String,
    pub expected_output: String,
    pub model: Option<String>,
    pub hub_reason: Option<String>,
}

pub const MAX_THREAD_TURNS: u32 = 40;

fn thread_system(device_name: &str, on_hub: bool) -> String {
    let place = if on_hub {
        "on the Grouper hub (a cloud machine). You have file tools in a private workspace there, no shell".to_string()
    } else {
        format!("on the device \"{device_name}\". You have tools on that device, rooted in a private workspace folder")
    };
    format!(
        "You are a subagent in Grouper, a personal multi-device assistant. The orchestrator gave you one task, \
described in a brief. You are running {place}.\n\n\
- Work only on your brief. You can't see the user's conversation; the brief has what you need.\n\
- If you need something only another device or the user can provide, use ask_orchestrator.\n\
- Some tool calls may wait for the user's approval; if one is denied, find another way or explain.\n\
- Messages from the user or orchestrator may arrive mid-task; follow them.\n\
- When you're done, reply with a short report for the orchestrator: what you did, the result, \
files you wrote (workspace paths), and anything unresolved. That reply ends the thread."
    )
}

fn render_brief(r: &ThreadRequest) -> String {
    let mut s = format!("# Brief: {}\n\n## Goal\n{}\n", r.title, r.goal.trim());
    for (h, v) in [("Context", &r.context), ("Constraints", &r.constraints), ("Expected output", &r.expected_output)] {
        if !v.trim().is_empty() {
            s.push_str(&format!("\n## {h}\n{}\n", v.trim()));
        }
    }
    s
}

impl Hub {
    pub(crate) fn owns_thread(&self, device_id: &str, thread_id: &str) -> bool {
        self.db.thread(thread_id).is_some_and(|t| t.device_id == device_id)
    }

    /// Resolve a device by id or (case-insensitive) name.
    fn resolve_device(&self, key: &str) -> Option<(String, String)> {
        if key.eq_ignore_ascii_case(HUB_DEVICE) {
            return Some((HUB_DEVICE.into(), "Hub".into()));
        }
        self.db
            .devices()
            .into_iter()
            .filter(|d| !d.revoked)
            .find(|d| d.id == key || d.name.eq_ignore_ascii_case(key))
            .map(|d| (d.id, d.name))
    }

    pub async fn start_thread(self: &Arc<Self>, conversation_id: &str, req: ThreadRequest) -> Result<ThreadInfo, String> {
        if req.title.trim().is_empty() || req.goal.trim().is_empty() {
            return Err("title and goal are required".into());
        }
        let (device_id, device_name) =
            self.resolve_device(&req.device).ok_or_else(|| format!("no device called {:?}", req.device))?;
        let on_hub = device_id == HUB_DEVICE;
        if !self.is_online(&device_id).await {
            return Err(format!(
                "{device_name} is offline. Tell the user, or start the thread on device \"hub\" with a hub_reason \
                 so the work is ready when {device_name} is back."
            ));
        }
        let model = match &req.model {
            Some(m) => self.db.find_model(m).ok_or_else(|| format!("no model {m:?} in the registry"))?,
            None => self.db.default_model("thread").ok_or("no models registered")?,
        };
        let reason = req.hub_reason.clone().filter(|_| on_hub);
        let brief = render_brief(&req);
        let info = self
            .db
            .create_thread(conversation_id, req.title.trim(), &device_id, &model.id, reason.as_deref(), &brief)
            .map_err(|e| e.to_string())?;
        self.emit(
            conversation_id,
            Some(&info.id),
            EventBody::ThreadStarted {
                title: info.title.clone(),
                device_id: device_id.clone(),
                model: model.id.clone(),
                reason,
                brief: brief.clone(),
            },
        );
        let spec = ThreadSpec {
            thread_id: info.id.clone(),
            conversation_id: conversation_id.into(),
            title: info.title.clone(),
            provider_id: model.provider_id.clone(),
            model: model.model.clone(),
            system: thread_system(&device_name, on_hub),
            brief,
            permissions: self.db.permissions(&device_id),
            max_turns: MAX_THREAD_TURNS,
        };

        if on_hub {
            self.run_on_hub(spec).await;
        } else if !self.send_to(&device_id, HubMessage::StartThread { spec }).await {
            self.thread_finished(&info.id, false, "The device went offline before the thread started.").await;
        }
        Ok(self.db.thread(&info.id).unwrap_or(info))
    }

    async fn run_on_hub(self: &Arc<Self>, spec: ThreadSpec) {
        let thread_id = spec.thread_id.clone();
        let fail = |msg: String| {
            let hub = self.clone();
            let id = thread_id.clone();
            async move { hub.thread_finished(&id, false, &msg).await }
        };
        let Some(pcfg) = self.db.provider(&spec.provider_id) else {
            return fail(format!("no provider {}", spec.provider_id)).await;
        };
        let provider = match providers::build(&pcfg) {
            Ok(p) => p,
            Err(e) => return fail(e.to_string()).await,
        };
        let ws = match Workspace::create(self.data_dir.join("workspaces").join(&thread_id)) {
            Ok(w) => w,
            Err(e) => return fail(format!("workspace: {e}")).await,
        };
        // No shell on the hub until hub threads run in sandbox containers.
        let host = Arc::new(HubHost { hub: Arc::downgrade(self) });
        let (control, handle) = spawn_thread(spec, provider, file_tools(&ws), host);
        self.hub_threads.lock().await.insert(thread_id.clone(), control);
        let hub = self.clone();
        tokio::spawn(async move {
            let result = handle.await.unwrap_or_else(|e| core_agent::thread::ThreadResult {
                ok: false,
                summary: format!("thread crashed: {e}"),
            });
            hub.hub_threads.lock().await.remove(&thread_id);
            hub.thread_finished(&thread_id, result.ok, &result.summary).await;
        });
    }

    pub(crate) fn thread_event(&self, thread_id: &str, event: AgentEvent) {
        let Some(t) = self.db.thread(thread_id) else { return };
        if let AgentEvent::Usage { provider_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens } =
            &event
        {
            self.db.record_usage(
                Some(&t.conversation_id),
                Some(thread_id),
                provider_id,
                model,
                *input_tokens,
                *output_tokens,
                *cache_read_tokens,
                *cache_write_tokens,
            );
        }
        self.emit(&t.conversation_id, Some(thread_id), EventBody::ThreadActivity { event });
    }

    pub(crate) async fn thread_finished(self: &Arc<Self>, thread_id: &str, ok: bool, summary: &str) {
        let status = if ok { "done" } else { "failed" };
        // Already stopped or finished: ignore late reports.
        if !self.db.finish_thread(thread_id, status, summary) {
            return;
        }
        self.clear_approvals(thread_id).await;
        let Some(t) = self.db.thread(thread_id) else { return };
        self.emit(
            &t.conversation_id,
            Some(thread_id),
            EventBody::ThreadFinished { status: status.into(), summary: summary.into() },
        );
        self.wake(
            &t.conversation_id,
            Wake::ThreadDone { thread_id: t.id, title: t.title, status: status.into(), summary: summary.into() },
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn request_approval(
        &self,
        device_id: &str,
        thread_id: &str,
        approval_id: &str,
        tool: &str,
        input: Value,
        reason: &str,
        local: Option<oneshot::Sender<bool>>,
    ) {
        let Some(t) = self.db.thread(thread_id) else { return };
        self.approvals.lock().await.insert(
            approval_id.into(),
            ApprovalRoute {
                device_id: device_id.into(),
                conversation_id: t.conversation_id.clone(),
                thread_id: thread_id.into(),
                local,
            },
        );
        self.emit(
            &t.conversation_id,
            Some(thread_id),
            EventBody::ApprovalRequested {
                approval_id: approval_id.into(),
                device_id: device_id.into(),
                tool: tool.into(),
                input,
                reason: reason.into(),
            },
        );
    }

    pub(crate) async fn thread_ask(self: &Arc<Self>, thread_id: &str, question: &str) {
        let Some(t) = self.db.thread(thread_id) else { return };
        self.wake(&t.conversation_id, Wake::ThreadAsk { thread_id: t.id, title: t.title, question: question.into() })
            .await;
    }

    /// Deliver a message to a running thread.
    async fn send_thread_input(&self, t: &ThreadInfo, from: &str, text: &str) -> Result<(), String> {
        if t.status != "running" {
            return Err(format!("thread {} is {}", t.id, t.status));
        }
        let delivered = if t.device_id == HUB_DEVICE {
            match self.hub_threads.lock().await.get(&t.id) {
                Some(c) => {
                    c.send(from, text);
                    true
                }
                None => false,
            }
        } else {
            self.send_to(
                &t.device_id,
                HubMessage::ThreadInput { thread_id: t.id.clone(), from: from.into(), text: text.into() },
            )
            .await
        };
        if !delivered {
            return Err(format!("thread {} isn't reachable right now", t.id));
        }
        self.emit(
            &t.conversation_id,
            Some(&t.id),
            EventBody::ThreadInput { from: from.into(), text: text.into() },
        );
        Ok(())
    }

    pub async fn orchestrator_thread_message(&self, thread_id: &str, text: &str) -> Result<(), String> {
        let t = self.db.thread(thread_id).ok_or("no such thread")?;
        self.send_thread_input(&t, "orchestrator", text).await
    }

    /// The user stepping into a thread. The orchestrator is told.
    pub async fn user_thread_message(self: &Arc<Self>, thread_id: &str, text: &str) -> Result<(), String> {
        let t = self.db.thread(thread_id).ok_or("no such thread")?;
        self.send_thread_input(&t, "user", text).await?;
        self.wake(&t.conversation_id, Wake::UserStepIn { thread_id: t.id, title: t.title, text: text.into() }).await;
        Ok(())
    }

    /// `by` is `user` or `orchestrator`.
    pub async fn stop_thread(self: &Arc<Self>, thread_id: &str, by: &str) -> Result<(), String> {
        let t = self.db.thread(thread_id).ok_or("no such thread")?;
        let summary = format!("Stopped by the {by}.");
        if !self.db.finish_thread(thread_id, "stopped", &summary) {
            return Err(format!("thread {} is already {}", t.id, t.status));
        }
        self.clear_approvals(thread_id).await;
        if t.device_id == HUB_DEVICE {
            if let Some(c) = self.hub_threads.lock().await.get(&t.id) {
                c.stop();
            }
        } else {
            self.send_to(&t.device_id, HubMessage::StopThread { thread_id: t.id.clone() }).await;
        }
        self.emit(
            &t.conversation_id,
            Some(&t.id),
            EventBody::ThreadFinished { status: "stopped".into(), summary: summary.clone() },
        );
        if by == "user" {
            self.wake(
                &t.conversation_id,
                Wake::UserStepIn { thread_id: t.id, title: t.title, text: "(the user stopped this thread)".into() },
            )
            .await;
        }
        Ok(())
    }

    /// Pending approvals die with their thread (a hub thread waiting on one gets "denied").
    async fn clear_approvals(&self, thread_id: &str) {
        self.approvals.lock().await.retain(|_, r| r.thread_id != thread_id);
    }

    /// A device disconnected: its threads can't continue, and its approvals are moot.
    pub(crate) async fn device_gone(self: &Arc<Self>, device_id: &str) {
        self.approvals.lock().await.retain(|_, r| r.device_id != device_id);
        for t in self.db.running_threads_on(device_id) {
            self.thread_finished(&t.id, false, "The device disconnected before the thread finished.").await;
        }
    }
}

/// Host for threads running on the hub itself.
struct HubHost {
    hub: std::sync::Weak<Hub>,
}

#[async_trait]
impl ThreadHost for HubHost {
    fn emit(&self, thread_id: &str, event: AgentEvent) {
        if let Some(hub) = self.hub.upgrade() {
            hub.thread_event(thread_id, event);
        }
    }

    async fn approve(&self, thread_id: &str, tool: &str, input: &Value, reason: &str) -> bool {
        let Some(hub) = self.hub.upgrade() else { return false };
        let approval_id = format!("a{}", hub.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        hub.request_approval(HUB_DEVICE, thread_id, &approval_id, tool, input.clone(), reason, Some(tx)).await;
        drop(hub);
        rx.await.unwrap_or(false)
    }

    fn ask_orchestrator(&self, thread_id: &str, question: &str) {
        if let Some(hub) = self.hub.upgrade() {
            let (id, q) = (thread_id.to_string(), question.to_string());
            tokio::spawn(async move { hub.thread_ask(&id, &q).await });
        }
    }
}
