//! Folding stored events into what the screens show. Pure functions over events in
//! `seq` order, so the same code serves the cache, tests and (later) the hub's
//! own HTTP answers.

use std::collections::HashMap;

use core_protocol::{AgentEvent, Event, EventBody, ThreadInfo};
use serde::Serialize;
use serde_json::Value;

/// One row of a conversation's transcript.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    User { seq: i64, ts: u64, text: String },
    Assistant { seq: i64, ts: u64, text: String },
    /// A delegated thread, at the position it started, updated in place by its later events.
    Thread(Box<ThreadCard>),
    /// A conversation-level error (thread errors stay in the thread's activity).
    Error { seq: i64, ts: u64, message: String },
    BudgetWarning { seq: i64, ts: u64, scope: String, spent: f64, limit: f64 },
}

/// A thread chip in the transcript.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ThreadCard {
    pub thread_id: String,
    /// Seq of the event that put the card here (normally `ThreadStarted`).
    pub seq: i64,
    pub ts: u64,
    pub title: String,
    pub device_id: String,
    pub model: String,
    pub reason: Option<String>,
    /// `running`, `waiting_device`, `done`, `failed` or `stopped`.
    pub status: String,
    /// The thread's latest full model text.
    pub latest_text: Option<String>,
    /// The tool it is running now (called, no result yet).
    pub open_tool: Option<String>,
    /// The newest hub checkpoint report while it waited for its device.
    pub checkpoint: Option<String>,
    /// Its final summary once finished.
    pub summary: Option<String>,
    /// Approvals it is waiting on.
    pub pending_approvals: u32,
}

/// A tool call waiting for the person's answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub conversation_id: String,
    pub thread_id: Option<String>,
    pub device_id: String,
    pub tool: String,
    pub input: Value,
    pub reason: String,
    pub seq: i64,
    pub ts: u64,
}

/// One step of a thread, for the thread sheet.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityItem {
    Started { seq: i64, ts: u64, title: String, device_id: String, model: String, reason: Option<String>, brief: String },
    Text { seq: i64, ts: u64, text: String },
    /// A tool call and, once it arrived, its result.
    Tool { seq: i64, ts: u64, call_id: String, name: String, input: Value, ok: Option<bool>, output: Option<String> },
    Input { seq: i64, ts: u64, from: String, text: String },
    Notice { seq: i64, ts: u64, text: String },
    Waiting { seq: i64, ts: u64, device_id: String, reason: String },
    Checkpoint { seq: i64, ts: u64, device_id: String, summary: String },
    HandedOver { seq: i64, ts: u64, device_id: String, files: Vec<String>, skipped: Vec<String> },
    Approval { seq: i64, ts: u64, approval_id: String, tool: String, input: Value, reason: String, approved: Option<bool> },
    Error { seq: i64, ts: u64, message: String },
    Finished { seq: i64, ts: u64, status: String, summary: String },
}

/// The latest user or assistant message, for the conversation list.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MessagePreview {
    /// `user` or `assistant`.
    pub role: String,
    pub text: String,
    pub seq: i64,
    pub ts: u64,
}

const TERMINAL: &[&str] = &["done", "failed", "stopped"];

fn is_terminal(status: &str) -> bool {
    TERMINAL.contains(&status)
}

/// Fold a conversation's stored events (in `seq` order) into its transcript.
/// `threads` (the cached thread rows) wins over the events for a thread's status,
/// title, device and summary, since the hub's row is the source of truth.
pub fn transcript(events: &[Event], threads: &[ThreadInfo]) -> Vec<TranscriptItem> {
    let rows: HashMap<&str, &ThreadInfo> = threads.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut items: Vec<TranscriptItem> = Vec::new();
    let mut card_at: HashMap<String, usize> = HashMap::new();
    // approval id -> thread id, while unanswered.
    let mut open_approvals: HashMap<String, Option<String>> = HashMap::new();

    for e in events {
        if e.seq <= 0 {
            continue;
        }
        let tid = e.thread_id.clone();
        // Every thread event lands on a card; make one if its start is missing.
        let card = |items: &mut Vec<TranscriptItem>, card_at: &mut HashMap<String, usize>, t: &str| -> usize {
            *card_at.entry(t.to_string()).or_insert_with(|| {
                items.push(TranscriptItem::Thread(Box::new(ThreadCard {
                    thread_id: t.to_string(),
                    seq: e.seq,
                    ts: e.ts,
                    title: String::new(),
                    device_id: String::new(),
                    model: String::new(),
                    reason: None,
                    status: "running".into(),
                    latest_text: None,
                    open_tool: None,
                    checkpoint: None,
                    summary: None,
                    pending_approvals: 0,
                })));
                items.len() - 1
            })
        };
        macro_rules! with_card {
            ($t:expr, |$c:ident| $body:block) => {{
                let i = card(&mut items, &mut card_at, $t);
                if let TranscriptItem::Thread($c) = &mut items[i] $body
            }};
        }
        match (&e.body, tid.as_deref()) {
            (EventBody::UserMessage { text }, None) => {
                items.push(TranscriptItem::User { seq: e.seq, ts: e.ts, text: text.clone() })
            }
            (EventBody::AssistantMessage { text }, None) => {
                items.push(TranscriptItem::Assistant { seq: e.seq, ts: e.ts, text: text.clone() })
            }
            (EventBody::Error { message }, None) => {
                items.push(TranscriptItem::Error { seq: e.seq, ts: e.ts, message: message.clone() })
            }
            (EventBody::BudgetWarning { scope, spent, limit }, _) => items.push(TranscriptItem::BudgetWarning {
                seq: e.seq,
                ts: e.ts,
                scope: scope.clone(),
                spent: *spent,
                limit: *limit,
            }),
            (EventBody::ThreadStarted { title, device_id, model, reason, .. }, Some(t)) => with_card!(t, |c| {
                c.title = title.clone();
                c.device_id = device_id.clone();
                c.model = model.clone();
                c.reason = reason.clone();
                c.status = "running".into();
            }),
            (EventBody::ThreadActivity { event }, Some(t)) => with_card!(t, |c| {
                match event {
                    AgentEvent::Text { text } => c.latest_text = Some(text.clone()),
                    AgentEvent::ToolCall { name, .. } => c.open_tool = Some(name.clone()),
                    AgentEvent::ToolResult { .. } => c.open_tool = None,
                    _ => {}
                }
            }),
            (EventBody::ThreadWaiting { .. }, Some(t)) => with_card!(t, |c| {
                c.status = "waiting_device".into();
            }),
            (EventBody::ThreadCheckpoint { summary, .. }, Some(t)) => with_card!(t, |c| {
                c.checkpoint = Some(summary.clone());
            }),
            (EventBody::ThreadHandedOver { device_id, .. }, Some(t)) => with_card!(t, |c| {
                c.status = "running".into();
                c.device_id = device_id.clone();
            }),
            (EventBody::ThreadFinished { status, summary }, Some(t)) => {
                open_approvals.retain(|_, owner| owner.as_deref() != Some(t));
                with_card!(t, |c| {
                    c.status = status.clone();
                    c.summary = Some(summary.clone());
                    c.open_tool = None;
                })
            }
            (EventBody::ApprovalRequested { approval_id, .. }, t) => {
                open_approvals.insert(approval_id.clone(), t.map(String::from));
                if let Some(t) = t {
                    card(&mut items, &mut card_at, t);
                }
            }
            (EventBody::ApprovalResolved { approval_id, .. }, _) => {
                open_approvals.remove(approval_id);
            }
            _ => {}
        }
    }

    for item in &mut items {
        let TranscriptItem::Thread(c) = item else { continue };
        if let Some(row) = rows.get(c.thread_id.as_str()) {
            c.status = row.status.clone();
            if c.title.is_empty() {
                c.title = row.title.clone();
            }
            c.device_id = row.device_id.clone();
            if c.model.is_empty() {
                c.model = row.model.clone();
            }
            if c.reason.is_none() {
                c.reason = row.reason.clone();
            }
            if row.summary.is_some() {
                c.summary = row.summary.clone();
            }
        }
        c.pending_approvals = if is_terminal(&c.status) {
            0
        } else {
            open_approvals.values().filter(|owner| owner.as_deref() == Some(c.thread_id.as_str())).count() as u32
        };
    }
    items
}

/// Approvals requested and not yet resolved, oldest first. An approval whose thread
/// has finished (by its events or by its cached row) can no longer be answered and
/// is left out.
pub fn pending_approvals(events: &[Event], threads: &[ThreadInfo]) -> Vec<PendingApproval> {
    let finished_rows: std::collections::HashSet<&str> =
        threads.iter().filter(|t| is_terminal(&t.status)).map(|t| t.id.as_str()).collect();
    let mut open: Vec<PendingApproval> = Vec::new();
    for e in events {
        match &e.body {
            EventBody::ApprovalRequested { approval_id, device_id, tool, input, reason } => {
                open.retain(|a| a.approval_id != *approval_id);
                open.push(PendingApproval {
                    approval_id: approval_id.clone(),
                    conversation_id: e.conversation_id.clone(),
                    thread_id: e.thread_id.clone(),
                    device_id: device_id.clone(),
                    tool: tool.clone(),
                    input: input.clone(),
                    reason: reason.clone(),
                    seq: e.seq,
                    ts: e.ts,
                });
            }
            EventBody::ApprovalResolved { approval_id, .. } => open.retain(|a| a.approval_id != *approval_id),
            EventBody::ThreadFinished { .. } => {
                if let Some(t) = &e.thread_id {
                    open.retain(|a| a.thread_id.as_ref() != Some(t));
                }
            }
            _ => {}
        }
    }
    open.retain(|a| a.thread_id.as_deref().is_none_or(|t| !finished_rows.contains(t)));
    open.sort_by_key(|a| a.seq);
    open
}

/// One thread's events (in `seq` order) as steps for the thread sheet. Tool calls
/// and their results are one item; usage and streamed deltas are left out.
pub fn thread_activity(events: &[Event]) -> Vec<ActivityItem> {
    let mut items = Vec::new();
    let mut tool_at: HashMap<String, usize> = HashMap::new();
    let mut approval_at: HashMap<String, usize> = HashMap::new();
    for e in events {
        let (seq, ts) = (e.seq, e.ts);
        match &e.body {
            EventBody::ThreadStarted { title, device_id, model, reason, brief } => items.push(ActivityItem::Started {
                seq,
                ts,
                title: title.clone(),
                device_id: device_id.clone(),
                model: model.clone(),
                reason: reason.clone(),
                brief: brief.clone(),
            }),
            EventBody::ThreadActivity { event } => match event {
                AgentEvent::Text { text } => items.push(ActivityItem::Text { seq, ts, text: text.clone() }),
                AgentEvent::ToolCall { call_id, name, input } => {
                    tool_at.insert(call_id.clone(), items.len());
                    items.push(ActivityItem::Tool {
                        seq,
                        ts,
                        call_id: call_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        ok: None,
                        output: None,
                    });
                }
                AgentEvent::ToolResult { call_id, name, ok: done_ok, output: out } => {
                    match tool_at.get(call_id).and_then(|&i| items.get_mut(i)) {
                        Some(ActivityItem::Tool { ok, output, .. }) => {
                            *ok = Some(*done_ok);
                            *output = Some(out.clone());
                        }
                        _ => items.push(ActivityItem::Tool {
                            seq,
                            ts,
                            call_id: call_id.clone(),
                            name: name.clone(),
                            input: Value::Null,
                            ok: Some(*done_ok),
                            output: Some(out.clone()),
                        }),
                    }
                }
                AgentEvent::Notice { text } => items.push(ActivityItem::Notice { seq, ts, text: text.clone() }),
                AgentEvent::TextDelta { .. } | AgentEvent::Usage { .. } => {}
            },
            EventBody::ThreadInput { from, text } => {
                items.push(ActivityItem::Input { seq, ts, from: from.clone(), text: text.clone() })
            }
            EventBody::ThreadWaiting { device_id, reason } => {
                items.push(ActivityItem::Waiting { seq, ts, device_id: device_id.clone(), reason: reason.clone() })
            }
            EventBody::ThreadCheckpoint { device_id, summary } => items.push(ActivityItem::Checkpoint {
                seq,
                ts,
                device_id: device_id.clone(),
                summary: summary.clone(),
            }),
            EventBody::ThreadHandedOver { device_id, files, skipped, .. } => items.push(ActivityItem::HandedOver {
                seq,
                ts,
                device_id: device_id.clone(),
                files: files.clone(),
                skipped: skipped.clone(),
            }),
            EventBody::ApprovalRequested { approval_id, tool, input, reason, .. } => {
                approval_at.insert(approval_id.clone(), items.len());
                items.push(ActivityItem::Approval {
                    seq,
                    ts,
                    approval_id: approval_id.clone(),
                    tool: tool.clone(),
                    input: input.clone(),
                    reason: reason.clone(),
                    approved: None,
                });
            }
            EventBody::ApprovalResolved { approval_id, approved: answer } => {
                if let Some(ActivityItem::Approval { approved, .. }) =
                    approval_at.get(approval_id).and_then(|&i| items.get_mut(i))
                {
                    *approved = Some(*answer);
                }
            }
            EventBody::Error { message } => items.push(ActivityItem::Error { seq, ts, message: message.clone() }),
            EventBody::ThreadFinished { status, summary } => {
                items.push(ActivityItem::Finished { seq, ts, status: status.clone(), summary: summary.clone() })
            }
            _ => {}
        }
    }
    items
}

/// The text a stored event contributes to search, if any.
pub(crate) fn searchable_text(body: &EventBody) -> Option<&str> {
    match body {
        EventBody::UserMessage { text } | EventBody::AssistantMessage { text } | EventBody::ThreadInput { text, .. } => {
            Some(text)
        }
        EventBody::ThreadActivity { event: AgentEvent::Text { text } } => Some(text),
        EventBody::ThreadFinished { summary, .. } | EventBody::ThreadCheckpoint { summary, .. } => Some(summary),
        EventBody::ThreadStarted { title, .. } => Some(title),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(seq: i64, thread: Option<&str>, body: EventBody) -> Event {
        Event { seq, conversation_id: "c".into(), thread_id: thread.map(String::from), ts: 100 + seq as u64, body }
    }

    fn started(seq: i64, t: &str, title: &str) -> Event {
        ev(
            seq,
            Some(t),
            EventBody::ThreadStarted {
                title: title.into(),
                device_id: "laptop".into(),
                model: "fake".into(),
                reason: None,
                brief: "b".into(),
            },
        )
    }

    fn activity(seq: i64, t: &str, event: AgentEvent) -> Event {
        ev(seq, Some(t), EventBody::ThreadActivity { event })
    }

    fn approval(seq: i64, t: &str, id: &str) -> Event {
        ev(
            seq,
            Some(t),
            EventBody::ApprovalRequested {
                approval_id: id.into(),
                device_id: "laptop".into(),
                tool: "run_shell".into(),
                input: json!({"command": "ls"}),
                reason: "shell".into(),
            },
        )
    }

    fn sample() -> Vec<Event> {
        vec![
            ev(1, None, EventBody::UserMessage { text: "two haiku".into() }),
            started(2, "t1", "Haiku Mac"),
            started(3, "t2", "Haiku hub"),
            ev(4, None, EventBody::AssistantMessage { text: "Started both.".into() }),
            ev(5, None, EventBody::TurnFinished { running_threads: 2 }),
            activity(6, "t1", AgentEvent::Text { text: "drafting".into() }),
            activity(
                7,
                "t1",
                AgentEvent::ToolCall { call_id: "k1".into(), name: "run_shell".into(), input: json!({"command": "ls"}) },
            ),
            approval(8, "t1", "a1"),
            approval(9, "t2", "a2"),
            ev(10, Some("t2"), EventBody::ApprovalResolved { approval_id: "a2".into(), approved: true }),
            ev(11, Some("t2"), EventBody::ThreadWaiting { device_id: "laptop".into(), reason: "offline".into() }),
            ev(12, Some("t2"), EventBody::ThreadCheckpoint { device_id: "laptop".into(), summary: "step 1".into() }),
            ev(13, None, EventBody::Error { message: "oops".into() }),
        ]
    }

    #[test]
    fn transcript_folds_threads_into_cards_in_place() {
        let items = transcript(&sample(), &[]);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| match i {
                TranscriptItem::User { .. } => "user",
                TranscriptItem::Assistant { .. } => "assistant",
                TranscriptItem::Thread(_) => "thread",
                TranscriptItem::Error { .. } => "error",
                TranscriptItem::BudgetWarning { .. } => "budget",
            })
            .collect();
        assert_eq!(kinds, ["user", "thread", "thread", "assistant", "error"]);
        let TranscriptItem::Thread(t1) = &items[1] else { panic!() };
        assert_eq!(t1.title, "Haiku Mac");
        assert_eq!(t1.status, "running");
        assert_eq!(t1.latest_text.as_deref(), Some("drafting"));
        assert_eq!(t1.open_tool.as_deref(), Some("run_shell"));
        assert_eq!(t1.pending_approvals, 1);
        let TranscriptItem::Thread(t2) = &items[2] else { panic!() };
        assert_eq!(t2.status, "waiting_device");
        assert_eq!(t2.checkpoint.as_deref(), Some("step 1"));
        assert_eq!(t2.pending_approvals, 0);
    }

    #[test]
    fn transcript_status_follows_finish_and_cached_rows() {
        let mut events = sample();
        events.push(activity(
            14,
            "t1",
            AgentEvent::ToolResult { call_id: "k1".into(), name: "run_shell".into(), ok: true, output: "x".into() },
        ));
        events.push(ev(15, Some("t1"), EventBody::ThreadFinished { status: "done".into(), summary: "two lines".into() }));
        let items = transcript(&events, &[]);
        let TranscriptItem::Thread(t1) = &items[1] else { panic!() };
        assert_eq!((t1.status.as_str(), t1.summary.as_deref()), ("done", Some("two lines")));
        assert_eq!(t1.open_tool, None);
        assert_eq!(t1.pending_approvals, 0, "a finished thread's approvals are gone");

        // The hub's row wins: t2 was handed over and stopped since these events.
        let row = ThreadInfo {
            id: "t2".into(),
            conversation_id: "c".into(),
            title: "Haiku hub".into(),
            device_id: "laptop".into(),
            model: "fake".into(),
            status: "stopped".into(),
            reason: None,
            summary: Some("stopped by you".into()),
            created: 0,
            finished: Some(1),
        };
        let items = transcript(&events, &[row]);
        let TranscriptItem::Thread(t2) = &items[2] else { panic!() };
        assert_eq!((t2.status.as_str(), t2.summary.as_deref()), ("stopped", Some("stopped by you")));
    }

    #[test]
    fn a_thread_without_its_start_still_gets_a_card() {
        let events = vec![activity(5, "tx", AgentEvent::Text { text: "hello".into() })];
        let items = transcript(&events, &[]);
        let TranscriptItem::Thread(c) = &items[0] else { panic!("{items:?}") };
        assert_eq!((c.thread_id.as_str(), c.seq), ("tx", 5));
    }

    #[test]
    fn pending_approvals_drop_resolved_and_finished() {
        let events = sample();
        let open = pending_approvals(&events, &[]);
        assert_eq!(open.iter().map(|a| a.approval_id.as_str()).collect::<Vec<_>>(), ["a1"]);
        assert_eq!(open[0].input, json!({"command": "ls"}));

        let mut finished = events.clone();
        finished.push(ev(20, Some("t1"), EventBody::ThreadFinished { status: "failed".into(), summary: "".into() }));
        assert!(pending_approvals(&finished, &[]).is_empty());

        let row = ThreadInfo {
            id: "t1".into(),
            conversation_id: "c".into(),
            title: "".into(),
            device_id: "".into(),
            model: "".into(),
            status: "stopped".into(),
            reason: None,
            summary: None,
            created: 0,
            finished: None,
        };
        assert!(pending_approvals(&events, &[row]).is_empty());
    }

    #[test]
    fn activity_pairs_tools_and_approvals() {
        let mut events: Vec<Event> = sample().into_iter().filter(|e| e.thread_id.as_deref() == Some("t1")).collect();
        events.push(activity(
            14,
            "t1",
            AgentEvent::ToolResult { call_id: "k1".into(), name: "run_shell".into(), ok: false, output: "denied".into() },
        ));
        events.push(ev(15, Some("t1"), EventBody::ApprovalResolved { approval_id: "a1".into(), approved: false }));
        events.push(activity(16, "t1", AgentEvent::TextDelta { text: "ignored".into() }));
        let items = thread_activity(&events);
        assert_eq!(items.len(), 4, "{items:?}");
        assert!(matches!(&items[0], ActivityItem::Started { title, .. } if title == "Haiku Mac"));
        assert!(matches!(&items[2], ActivityItem::Tool { ok: Some(false), output: Some(o), .. } if o == "denied"));
        assert!(matches!(&items[3], ActivityItem::Approval { approved: Some(false), .. }));
    }
}
