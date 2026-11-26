//! `update_plan`: a thread's live checklist. The model sends the whole list on every
//! call; clients show the latest call's input pinned at the top of the thread.
//!
//! The tool touches nothing (no files, no network, no device), so it is added to every
//! thread in [`crate::thread::spawn_thread`], is never gated (see
//! [`crate::permission::UNGATED`]), and behaves the same on the hub and on devices. The
//! call and its result travel as ordinary `ToolCall` / `ToolResult` thread activity.

use async_trait::async_trait;
use core_protocol::ToolDef;
use serde_json::{Value, json};

use crate::{Tool, ToolOutput};

/// The tool's name. Clients look for this name; don't change it.
pub const UPDATE_PLAN: &str = "update_plan";
/// Most steps a plan may have.
pub const MAX_PLAN_ITEMS: usize = 30;
/// Longest `content` or `active_form`, in characters.
pub const MAX_PLAN_TEXT: usize = 200;

const STATUSES: &[&str] = &["pending", "in_progress", "completed"];

/// Check a plan and describe it: `Ok("Plan updated: 2 of 5 done.")`, or `Err(why)` in
/// words the model can act on.
pub fn check_plan(input: &Value) -> Result<String, String> {
    let Some(items) = input.get("items").and_then(Value::as_array) else {
        return Err("items is required: the whole plan as a list of {content, status, active_form?}".into());
    };
    if items.len() > MAX_PLAN_ITEMS {
        return Err(format!(
            "the plan has {} steps; the most is {MAX_PLAN_ITEMS}. Merge small steps",
            items.len()
        ));
    }
    let mut done = 0;
    let mut in_progress = 0;
    for (i, item) in items.iter().enumerate() {
        let n = i + 1;
        let Some(item) = item.as_object() else {
            return Err(format!("step {n} must be an object with content and status"));
        };
        let content = match item.get("content") {
            Some(Value::String(s)) if !s.trim().is_empty() => s,
            Some(Value::String(_)) | None | Some(Value::Null) => return Err(format!("step {n} needs a non-empty content")),
            Some(_) => return Err(format!("step {n}: content must be a string")),
        };
        if content.chars().count() > MAX_PLAN_TEXT {
            return Err(format!("step {n}: content is longer than {MAX_PLAN_TEXT} characters; shorten it"));
        }
        match item.get("status").and_then(Value::as_str) {
            Some("completed") => done += 1,
            Some("in_progress") => in_progress += 1,
            Some("pending") => {}
            _ => return Err(format!("step {n}: status must be one of {}", STATUSES.join(", "))),
        }
        match item.get("active_form") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if s.chars().count() > MAX_PLAN_TEXT => {
                return Err(format!("step {n}: active_form is longer than {MAX_PLAN_TEXT} characters; shorten it"));
            }
            Some(Value::String(_)) => {}
            Some(_) => return Err(format!("step {n}: active_form must be a string")),
        }
    }
    if in_progress > 1 {
        return Err(format!("{in_progress} steps are in_progress; mark at most one in_progress at a time"));
    }
    if items.is_empty() {
        return Ok("Plan cleared.".into());
    }
    Ok(format!("Plan updated: {done} of {} done.", items.len()))
}

pub struct UpdatePlan;

#[async_trait]
impl Tool for UpdatePlan {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: UPDATE_PLAN.into(),
            description: "Show the user your plan as a live checklist. Send the whole list every time; each call \
                          replaces the plan. Keep exactly one step in_progress while you work, and mark a step \
                          completed as soon as it's done. Never needs approval."
                .into(),
            input_schema: json!({"type": "object", "properties": {
                "items": {"type": "array", "maxItems": MAX_PLAN_ITEMS, "description": "The whole plan, in order", "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "The step, imperative (\"Run the tests\")"},
                        "status": {"type": "string", "enum": STATUSES},
                        "active_form": {"type": "string", "description": "Optional present-tense label shown while the step is in progress (\"Running the tests\")"}
                    },
                    "required": ["content", "status"]
                }}
            }, "required": ["items"]}),
        }
    }

    async fn call(&self, input: Value) -> ToolOutput {
        match check_plan(&input) {
            Ok(text) => ToolOutput::ok(text),
            Err(why) => ToolOutput::err(format!("Plan not updated: {why}.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: &str) -> Value {
        json!({"content": content, "status": status})
    }

    #[test]
    fn counts_completed_steps() {
        let plan = json!({"items": [
            item("Read the brief", "completed"),
            item("Draft", "completed"),
            {"content": "Run the tests", "status": "in_progress", "active_form": "Running the tests"},
            item("Write the report", "pending"),
            item("Clean up", "pending"),
        ]});
        assert_eq!(check_plan(&plan).unwrap(), "Plan updated: 2 of 5 done.");
        assert_eq!(check_plan(&json!({"items": [item("a", "pending")]})).unwrap(), "Plan updated: 0 of 1 done.");
        assert_eq!(check_plan(&json!({"items": []})).unwrap(), "Plan cleared.");
    }

    #[test]
    fn rejects_bad_plans() {
        let err = |v: Value| check_plan(&v).unwrap_err();
        assert!(err(json!({})).contains("items is required"));
        assert!(err(json!({"items": "a, b"})).contains("items is required"));
        assert!(err(json!({"items": ["a"]})).contains("step 1 must be an object"));
        assert!(err(json!({"items": [{"status": "pending"}]})).contains("step 1 needs a non-empty content"));
        assert!(err(json!({"items": [item("ok", "pending"), item("  ", "pending")]})).contains("step 2 needs a non-empty"));
        assert!(err(json!({"items": [{"content": 3, "status": "pending"}]})).contains("content must be a string"));
        assert!(err(json!({"items": [item(&"x".repeat(201), "pending")]})).contains("longer than 200"));
        assert!(check_plan(&json!({"items": [item(&"é".repeat(200), "pending")]})).is_ok(), "limit counts characters");
        assert!(err(json!({"items": [item("a", "done")]})).contains("status must be one of pending, in_progress, completed"));
        assert!(err(json!({"items": [{"content": "a"}]})).contains("status must be one of"));
        assert!(err(json!({"items": [{"content": "a", "status": "pending", "active_form": 1}]})).contains("active_form must be a string"));
        assert!(err(json!({"items": [{"content": "a", "status": "pending", "active_form": "y".repeat(201)}]})).contains("active_form is longer"));
        let two = json!({"items": [item("a", "in_progress"), item("b", "in_progress")]});
        assert!(err(two).contains("at most one in_progress"));
        let many: Vec<Value> = (0..31).map(|i| item(&format!("step {i}"), "pending")).collect();
        assert!(err(json!({ "items": many })).contains("the most is 30"));
        let thirty: Vec<Value> = (0..30).map(|i| item(&format!("step {i}"), "pending")).collect();
        assert!(check_plan(&json!({ "items": thirty })).is_ok());
    }

    #[tokio::test]
    async fn invalid_input_is_an_error_result_not_a_crash() {
        let out = UpdatePlan.call(json!({"items": [item("a", "in_progress"), item("b", "in_progress")]})).await;
        assert!(out.is_error);
        assert!(out.content.starts_with("Plan not updated: "), "{}", out.content);
        let out = UpdatePlan.call(json!({"items": [item("a", "completed"), item("b", "in_progress")]})).await;
        assert_eq!(out, ToolOutput::ok("Plan updated: 1 of 2 done."));
    }
}
