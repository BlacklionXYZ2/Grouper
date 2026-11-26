//! Starting, steering and finishing subagent threads, on devices or on the hub itself.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use core_agent::thread::{ApprovalDecision, ThreadHost, approval_timeout_phrase, spawn_thread};
use core_agent::tools::{Workspace, file_tools};
use core_protocol::{
    AgentEvent, ApprovalTimeoutAction, EventBody, FolderInfo, FolderMode, HubMessage, ThreadInfo, ThreadSpec,
};
use serde_json::Value;
use tokio::sync::oneshot;

/// Identifies one call for remembering a user's denial: the tool and its exact input.
pub(crate) fn denial_key(tool: &str, input: &Value) -> String {
    format!("{tool}\0{input}")
}

use crate::offline::WAITING_DEVICE;
use crate::orchestrator::Wake;
use crate::server::{ApprovalAnswer, ApprovalRoute, HUB_DEVICE, Hub};

/// What the orchestrator asks for when it starts a thread.
#[derive(Debug, Default)]
pub struct ThreadRequest {
    pub title: String,
    /// Device id or name, or `hub`. Empty: choose from [`Self::needs`] and past results.
    pub device: String,
    pub goal: String,
    pub context: String,
    pub constraints: String,
    pub expected_output: String,
    pub model: Option<String>,
    pub hub_reason: Option<String>,
    /// Kind of work (`code`, `research`, `writing`, `data`, `ops`), for model choice and stats.
    pub task_type: Option<String>,
    /// e.g. `sensitive`, `draft`: priority rules match on these.
    pub tags: Vec<String>,
    /// What the work requires: `shell`, `gpu`, `xcode`, `files`, `folder:<name>`, `agent:<name>`.
    pub needs: Vec<String>,
    /// Spending cap for this thread (USD, estimated); never above the user's `thread_budget_usd`.
    pub budget_usd: Option<f64>,
}

pub const MAX_THREAD_TURNS: u32 = 40;

/// The thread's system prompt. `folders` are the device's scoped folders (none on the hub).
pub(crate) fn thread_system(device_name: &str, on_hub: bool, folders: &[FolderInfo]) -> String {
    let place = if on_hub {
        "on the Grouper hub (a cloud machine). You have file tools in a private workspace there, no shell".to_string()
    } else {
        format!("on the device \"{device_name}\". You have tools on that device, rooted in a private workspace folder")
    };
    let mut text = format!(
        "You are a subagent in Grouper, a personal multi-device assistant. The orchestrator gave you one task, \
described in a brief. You are running {place}.\n\n\
- Work only on your brief. You can't see the user's conversation; the brief has what you need.\n\
- If you need something only another device or the user can provide, use ask_orchestrator.\n\
- Some tool calls may wait for the user's approval; if one is denied, find another way or explain.\n\
- Messages from the user or orchestrator may arrive mid-task; follow them.\n\
- When you're done, reply with a short report for the orchestrator: what you did, the result, \
files you wrote (workspace paths), and anything unresolved. That reply ends the thread."
    );
    if !on_hub && !folders.is_empty() {
        let list: Vec<String> = folders
            .iter()
            .map(|f| {
                let mode = if f.mode == FolderMode::ReadWrite { "read and write" } else { "read only" };
                format!("@{} ({mode})", f.name)
            })
            .collect();
        text.push_str(&format!(
            "\n\nScoped folders on this device, outside your workspace: {}. Reach them with file tools as \
             `@<folder>/<path>` (list_files `@<folder>`), or run_shell with `cwd` `@<folder>/<path>`. Writes there and \
             shells there may wait for the user's approval; keep your own outputs in the workspace.",
            list.join(", ")
        ));
    }
    text
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
    pub(crate) fn resolve_device(&self, key: &str) -> Option<(String, String)> {
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
        let (device_id, device_name, device_reason) =
            self.resolve_thread_device(&req.device, req.task_type.as_deref(), &req.needs).await?;
        let on_hub = device_id == HUB_DEVICE;
        // An offline device gets the thread anyway: it waits for the device while the
        // hub prepares the work up to a checkpoint (see `offline.rs`).
        let online = self.is_online(&device_id).await;
        // Running-thread caps and spent budgets (`limits.rs`); held until the thread is recorded.
        let place = if online { (device_id.as_str(), device_name.as_str()) } else { (HUB_DEVICE, "the hub") };
        let admission = crate::limits::admit(self, conversation_id, place.0, place.1, req.budget_usd).await?;
        let cheaper = req.model.is_none() && admission.budget.is_some();
        let route = self.route_model(req.model.as_deref(), req.task_type.as_deref(), &req.tags, cheaper).await?;
        let model = route.model;
        let offline = (!online).then(|| format!("{device_name} is offline; the hub prepares a checkpoint"));
        let reason = [device_reason, req.hub_reason.clone().filter(|_| on_hub), offline, route.reason]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let reason = (!reason.is_empty()).then(|| reason.join("; "));
        let brief = render_brief(&req);
        let info = self
            .db
            .create_thread(conversation_id, req.title.trim(), &device_id, &model.id, reason.as_deref(), &brief)
            .map_err(|e| e.to_string())?;
        self.db.set_thread_meta(&info.id, req.task_type.as_deref().unwrap_or_default(), &req.tags);
        admission.record(&self.db, &info.id);
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
            system: thread_system(
                &device_name,
                on_hub,
                &if on_hub { Vec::new() } else { self.db.device_folders(&device_id) },
            ),
            brief,
            permissions: self.db.permissions(&device_id),
            max_turns: MAX_THREAD_TURNS,
            device_name: device_name.clone(),
            ask_timeout_secs: self.db.limits().ask_timeout_secs,
            fallback_models: self.free_alternates(&model.provider_id, &model.model),
        };

        if on_hub {
            self.run_on_hub(spec).await;
        } else if !online {
            self.wait_for_device(&info.id, &format!("{device_name} was offline when the thread started"), false).await;
        } else if !self.send_to(&device_id, HubMessage::StartThread { spec: Box::new(spec) }).await {
            self.wait_for_device(&info.id, &format!("{device_name} went offline before the thread started"), false)
                .await;
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
        let pcfg = match self.db.provider(&spec.provider_id) {
            Ok(Some(provider)) => provider,
            Ok(None) => return fail(format!("no provider {}", spec.provider_id)).await,
            Err(error) => return fail(error.to_string()).await,
        };
        let provider = match self.build_model_provider(&pcfg, &spec.model) {
            Ok(p) => p,
            Err(e) => return fail(e.to_string()).await,
        };
        let ws = match Workspace::create(self.data_dir.join("workspaces").join(&thread_id)) {
            Ok(w) => w,
            Err(e) => return fail(format!("workspace: {e}")).await,
        };
        // No shell on the hub until hub threads run in sandbox containers.
        let host = Arc::new(HubHost { hub: Arc::downgrade(self) });
        let (control, handle) = spawn_thread(spec, provider, file_tools(&ws), host, ws.clone());
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

    pub(crate) fn thread_event(self: &Arc<Self>, thread_id: &str, event: AgentEvent) {
        let Some(t) = self.db.thread(thread_id) else { return };
        if matches!(event, AgentEvent::TextDelta { .. }) {
            // Streamed text is shown live; the full message arrives as Text and is stored.
            self.emit_live(&t.conversation_id, Some(thread_id), EventBody::ThreadActivity { event });
            return;
        }
        let mut cost = None;
        if let AgentEvent::Usage { provider_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens } =
            &event
        {
            let usage = providers::Usage {
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cache_read_tokens: *cache_read_tokens,
                cache_write_tokens: *cache_write_tokens,
            };
            if !self.consume_relayed_usage(thread_id, usage) {
                cost = Some(self.db.record_usage(
                    Some(&t.conversation_id),
                    Some(thread_id),
                    provider_id,
                    model,
                    *input_tokens,
                    *output_tokens,
                    *cache_read_tokens,
                    *cache_write_tokens,
                ));
            }
        }
        self.emit(&t.conversation_id, Some(thread_id), EventBody::ThreadActivity { event });
        // After the usage event, so a warning or stop follows what caused it.
        if let Some(cost) = cost {
            crate::limits::after_usage(self, &t.conversation_id, Some(&t), cost);
        }
    }

    pub(crate) async fn thread_finished(self: &Arc<Self>, thread_id: &str, ok: bool, summary: &str) {
        self.relayed_usage.lock().unwrap_or_else(|e| e.into_inner()).remove(thread_id);
        let status = if ok { "done" } else { "failed" };
        // Already stopped or finished: ignore late reports.
        if !self.db.finish_thread(thread_id, status, summary) {
            return;
        }
        self.db.record_outcome(thread_id);
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
        self: &Arc<Self>,
        device_id: &str,
        thread_id: &str,
        approval_id: &str,
        tool: &str,
        input: Value,
        reason: &str,
        local: Option<oneshot::Sender<ApprovalAnswer>>,
    ) -> bool {
        let internal = local.is_some();
        if !internal && !approval_id.starts_with(&format!("{thread_id}-")) {
            return false;
        }
        let generation = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut approvals = self.approvals.lock().await;
        // Re-read status while holding the route lock. If a finisher changed it
        // first we reject; if it changes after this check, its clear pass removes us.
        let Some(t) = self.db.thread(thread_id) else { return false };
        if t.status != "running" && !(internal && t.status == WAITING_DEVICE) {
            return false;
        }
        if approvals.contains_key(approval_id) {
            return false;
        }
        // The user already denied this exact call in this conversation (perhaps to
        // another thread): refuse it without asking again. A device gets the usual
        // `ApprovalResult { approved: false }` from its caller when this returns false.
        let key = denial_key(tool, &input);
        if self.denied_calls.lock().unwrap().get(&t.conversation_id).is_some_and(|d| d.contains(&key)) {
            drop(approvals);
            tracing::info!(thread = thread_id, tool, "refused a call the user already denied in this conversation");
            return match local {
                Some(tx) => {
                    let _ = tx.send(ApprovalAnswer::Deny);
                    true
                }
                None => false,
            };
        }
        let requested_at = crate::db::now();
        let timeout_secs = self.db.limits().approval_timeout_secs;
        approvals.insert(
            approval_id.into(),
            ApprovalRoute {
                device_id: device_id.into(),
                conversation_id: t.conversation_id.clone(),
                thread_id: thread_id.into(),
                tool: tool.into(),
                input: input.clone(),
                reason: reason.into(),
                requested_at,
                timeout_secs,
                local,
                generation,
                timer: None,
            },
        );
        drop(approvals);
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
        // The limit at request time. Turning it off before the timer fires leaves
        // the approval pending (`approval_timed_out` checks again).
        if let Some(secs) = timeout_secs {
            let hub = Arc::clone(self);
            let timer_id = approval_id.to_string();
            let route_id = timer_id.clone();
            let task = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                hub.approval_timed_out(timer_id, generation, secs).await;
            });
            let timer = task.abort_handle();
            let mut approvals = self.approvals.lock().await;
            match approvals.get_mut(&route_id) {
                Some(route) if route.generation == generation => route.timer = Some(timer),
                _ => timer.abort(),
            }
        }
        true
    }

    /// The timer for one approval. Does nothing if that approval was already resolved
    /// or cleared, or if the timeout has since been turned off.
    pub(crate) async fn approval_timed_out(self: &Arc<Self>, approval_id: String, generation: u64, secs: u64) {
        if self.db.limits().approval_timeout_secs.is_none() {
            return;
        }
        let mut approvals = self.approvals.lock().await;
        if !approvals.get(&approval_id).is_some_and(|route| route.generation == generation) {
            return;
        }
        let Some(route) = approvals.remove(&approval_id) else { return };
        drop(approvals);
        let action = self.db.limits().approval_timeout_action;
        let phrase = approval_timeout_phrase(secs);
        tracing::info!(approval_id, thread = %route.thread_id, secs, action = action.as_str(), "approval timed out");
        if action == ApprovalTimeoutAction::Stop {
            let summary = format!("Stopped: no answer to an approval within {phrase}.");
            if self.stop_thread_as(&route.thread_id, "approval", &summary).await.is_err() {
                return;
            }
        }
        match route.local {
            Some(tx) => {
                let _ = tx.send(ApprovalAnswer::TimedOut { secs });
            }
            None => {
                self.send_to(
                    &route.device_id,
                    HubMessage::ApprovalResult {
                        approval_id: approval_id.clone(),
                        approved: false,
                        timeout_secs: Some(secs),
                    },
                )
                .await;
            }
        }
        self.emit(
            &route.conversation_id,
            Some(&route.thread_id),
            EventBody::ApprovalResolved { approval_id: approval_id.clone(), approved: false },
        );
        let text = if action == ApprovalTimeoutAction::Stop {
            format!("No answer within {phrase}; stopping the thread.")
        } else {
            format!("No answer within {phrase}; the call was denied.")
        };
        self.emit(
            &route.conversation_id,
            Some(&route.thread_id),
            EventBody::ThreadActivity { event: AgentEvent::Notice { text } },
        );
        if action == ApprovalTimeoutAction::Stop {
            let summary = format!("Stopped: no answer to an approval within {phrase}.");
            let id = route.thread_id.clone();
            if let Some(t) = self.db.thread(&id) {
                self.wake(
                    &t.conversation_id,
                    Wake::ThreadDone { thread_id: t.id, title: t.title, status: "stopped".into(), summary },
                )
                .await;
            }
        }
    }

    pub(crate) async fn thread_ask(self: &Arc<Self>, thread_id: &str, question: &str) {
        let Some(t) = self.db.thread(thread_id) else { return };
        self.wake(&t.conversation_id, Wake::ThreadAsk { thread_id: t.id, title: t.title, question: question.into() })
            .await;
    }

    /// Deliver a message to a running thread.
    async fn send_thread_input(&self, t: &ThreadInfo, from: &str, text: &str) -> Result<(), String> {
        if t.status == WAITING_DEVICE {
            return self.waiting_thread_input(t, from, text).await;
        }
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

    /// Forget the calls the user denied in a conversation: they have spoken since, and
    /// may have changed their mind. Their next approval request decides.
    pub(crate) fn forget_denials(&self, conversation_id: &str) {
        self.denied_calls.lock().unwrap().remove(conversation_id);
    }

    /// The user stepping into a thread. The orchestrator is told.
    pub async fn user_thread_message(self: &Arc<Self>, thread_id: &str, text: &str) -> Result<(), String> {
        let t = self.db.thread(thread_id).ok_or("no such thread")?;
        self.forget_denials(&t.conversation_id);
        self.send_thread_input(&t, "user", text).await?;
        self.wake(&t.conversation_id, Wake::UserStepIn { thread_id: t.id, title: t.title, text: text.into() }).await;
        Ok(())
    }

    /// `by` is `user` or `orchestrator`.
    pub async fn stop_thread(self: &Arc<Self>, thread_id: &str, by: &str) -> Result<(), String> {
        self.stop_thread_as(thread_id, by, &format!("Stopped by the {by}.")).await
    }

    /// Stop with the given summary; `by` is `user`, `orchestrator`, `budget` (`limits.rs`)
    /// or `approval` (an approval timeout whose action is stop).
    pub(crate) async fn stop_thread_as(self: &Arc<Self>, thread_id: &str, by: &str, summary: &str) -> Result<(), String> {
        let t = self.db.thread(thread_id).ok_or("no such thread")?;
        let summary = summary.to_string();
        let stopped = self.db.finish_active_thread(thread_id, "stopped", &summary);
        if !stopped {
            return Err(format!("thread {} is already {}", t.id, t.status));
        }
        self.db.record_outcome(thread_id);
        self.clear_approvals(thread_id).await;
        // Stop a hub-side part whenever one exists. This also covers a concurrent
        // running -> waiting_device transition after the snapshot above.
        if let Some(c) = self.hub_threads.lock().await.get(&t.id) {
            c.stop();
        }
        if t.device_id != HUB_DEVICE {
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
        self.approvals.lock().await.retain(|_, r| {
            if r.thread_id == thread_id {
                if let Some(timer) = r.timer.take() {
                    timer.abort();
                }
                false
            } else {
                true
            }
        });
    }

    /// A device disconnected: its approvals are moot, and its threads wait for it to
    /// come back while the hub works towards a checkpoint (see `offline.rs`).
    pub(crate) async fn device_gone(self: &Arc<Self>, device_id: &str) {
        self.approvals.lock().await.retain(|_, r| {
            if r.device_id == device_id {
                if let Some(timer) = r.timer.take() {
                    timer.abort();
                }
                false
            } else {
                true
            }
        });
        for t in self.db.running_threads_on(device_id) {
            self.wait_for_device(&t.id, "the device disconnected while the thread was running", true).await;
        }
    }
}

/// Host for threads running on the hub itself.
pub(crate) struct HubHost {
    pub(crate) hub: std::sync::Weak<Hub>,
}

#[async_trait]
impl ThreadHost for HubHost {
    fn emit(&self, thread_id: &str, event: AgentEvent) {
        if let Some(hub) = self.hub.upgrade() {
            hub.thread_event(thread_id, event);
        }
    }

    async fn approve(&self, thread_id: &str, tool: &str, input: &Value, reason: &str) -> ApprovalDecision {
        let Some(hub) = self.hub.upgrade() else { return ApprovalDecision::Denied };
        let approval_id = format!("a{}", hub.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        hub.request_approval(HUB_DEVICE, thread_id, &approval_id, tool, input.clone(), reason, Some(tx)).await;
        drop(hub);
        match rx.await {
            Ok(ApprovalAnswer::Allow) => ApprovalDecision::Approved,
            Ok(ApprovalAnswer::TimedOut { secs }) => ApprovalDecision::TimedOut { secs },
            _ => ApprovalDecision::Denied,
        }
    }

    fn ask_orchestrator(&self, thread_id: &str, question: &str) {
        if let Some(hub) = self.hub.upgrade() {
            let (id, q) = (thread_id.to_string(), question.to_string());
            tokio::spawn(async move { hub.thread_ask(&id, &q).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_lists_scoped_folders() {
        let folders = [
            FolderInfo { name: "projects".into(), mode: FolderMode::ReadWrite },
            FolderInfo { name: "docs".into(), mode: FolderMode::Read },
        ];
        let text = thread_system("Laptop", false, &folders);
        assert!(text.contains("@projects (read and write), @docs (read only)"), "{text}");
        assert!(text.contains("`@<folder>/<path>`"), "{text}");
        assert!(!thread_system("Laptop", false, &[]).contains("Scoped folders"));
        assert!(!thread_system("Hub", true, &folders).contains("Scoped folders"));
    }
}
