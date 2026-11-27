//! Hub-side CLI harness registration from device [`DetectedHarness`] scan reports.
//!
//! For each signed-in CLI reported by a device:
//! - Upserts one harness provider row (`id = "<harness>@<device_id>"`, `kind = "harness"`,
//!   `options.via_device = <device_id>`).
//! - Upserts one model row per reported model (`kind = "harness"`, `device_id`,
//!   `can_orchestrate`, `source = "auto"`, `free = false`, `local = false`, prices `0`,
//!   `price_source = "none"`).
//! - Hides (`hidden = true`, `hidden_reason = "gone"`) models or harnesses that disappear,
//!   sign out, or expire — never deleting rows so past threads, usage and outcomes keep
//!   resolving them. `hidden_reason = "user"` is never undone by a report.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Result, bail};
use core_protocol::{
    DetectedHarness, EventBody, HarnessSignIn, HarnessUsage, HubMessage, ModelEntry, ProviderConfig,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::catalog::derive_registry_id;
use crate::db::{Db, now};
use crate::discover::lookup_price;
use crate::orchestrator::Wake;
use crate::server::{Hub, QueueResult};

const HARNESS_META_PREFIX: &str = "device_harnesses:";

/// Outcome counts from applying one device's [`DetectedHarness`] report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HarnessReportOutcome {
    pub added: usize,
    pub updated: usize,
    pub hidden: usize,
}

/// Canonical provider id for a harness CLI on a specific device (`claude-code@<device_id>`).
/// Harness CLIs whose headless mode can't be confined (no read-only or plan mode that Grouper can
/// rely on). Standard-chat turns are refused for these until per-turn approval exists.
pub(crate) const UNCONFINABLE_HARNESSES: &[&str] = &["hermes", "opencode"];

pub fn provider_id(harness: &str, device_id: &str) -> String {
    format!("{}@{}", harness.trim(), device_id.trim())
}

/// Deterministic registry id for a harness model row (`<harness>-<device>-<model>`).
pub fn model_registry_id(harness: &str, device_id: &str, model: &str) -> String {
    let h = derive_registry_id(harness);
    let d = derive_registry_id(device_id);
    let m = derive_registry_id(model);
    format!("{h}-{d}-{m}")
}

fn valid_slug(s: &str) -> bool {
    let s = s.trim();
    (1..=48).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Last-known harness scan report stored for `device_id` (including unsigned-in CLIs).
pub fn device_report(db: &Db, device_id: &str) -> Vec<DetectedHarness> {
    let key = format!("{HARNESS_META_PREFIX}{device_id}");
    let raw: Option<String> = db
        .c()
        .query_row("SELECT value FROM hub_meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_device_report(db: &Db, device_id: &str, harnesses: &[DetectedHarness]) {
    if let Ok(text) = serde_json::to_string(harnesses) {
        let key = format!("{HARNESS_META_PREFIX}{device_id}");
        let _ = db.c().execute(
            "INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)",
            params![key, text],
        );
    }
}

fn provider_belongs_to_device(p: &ProviderConfig, device_id: &str, device_name: &str) -> bool {
    if p.kind != "harness" {
        return false;
    }
    if p.id.ends_with(&format!("@{device_id}")) {
        return true;
    }
    p.via_device().is_some_and(|via| {
        via == device_id || (!device_name.is_empty() && via.eq_ignore_ascii_case(device_name))
    })
}

fn build_provider_options(entry: &DetectedHarness, device_id: &str, installed: bool) -> Value {
    let signed_in = installed && entry.sign_in.is_signed_in();
    let sign_in_str = if installed { entry.sign_in.as_str() } else { HarnessSignIn::NotSignedIn.as_str() };
    let mut obj = serde_json::Map::new();
    obj.insert("harness".into(), json!(entry.harness));
    if !entry.display_name.trim().is_empty() {
        obj.insert("display_name".into(), json!(entry.display_name.trim()));
    }
    obj.insert("via_device".into(), json!(device_id));
    obj.insert("installed".into(), json!(installed));
    obj.insert("signed_in".into(), json!(signed_in));
    obj.insert("sign_in".into(), json!(sign_in_str));
    if let Some(v) = entry.version.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        obj.insert("version".into(), json!(v));
    }
    if !signed_in
        && let Some(cmd) = entry.login_command.as_deref().map(str::trim).filter(|s| !s.is_empty())
    {
        obj.insert("login_command".into(), json!(cmd));
    }
    obj.insert("can_orchestrate".into(), json!(entry.capabilities.can_orchestrate));
    obj.insert("supports_resume".into(), json!(entry.capabilities.supports_resume));
    obj.insert("supports_mcp".into(), json!(entry.capabilities.supports_mcp));
    Value::Object(obj)
}

fn hide_provider_models(db: &Db, models: &[ModelEntry], pid: &str) -> Result<usize> {
    let mut count = 0usize;
    for m in models.iter().filter(|m| m.provider_id == pid) {
        if m.hidden {
            continue;
        }
        db.set_model_hidden(&m.id, true, Some("gone"))?;
        count += 1;
    }
    Ok(count)
}

/// Apply a device's [`DetectedHarness`] scan report to the hub's provider and model tables.
pub fn apply_report(
    db: &Db,
    device_id: &str,
    harnesses: &[DetectedHarness],
) -> Result<HarnessReportOutcome> {
    let device_id = device_id.trim();
    let Some(device) = db.devices().into_iter().find(|d| d.id == device_id && !d.revoked) else {
        bail!("unknown or revoked device {device_id:?}");
    };

    let cleaned: Vec<DetectedHarness> = harnesses
        .iter()
        .filter(|h| valid_slug(&h.harness))
        .cloned()
        .collect();
    save_device_report(db, device_id, &cleaned);

    let ts = now();
    let existing_providers: HashMap<String, ProviderConfig> = db
        .providers()?
        .into_iter()
        .map(|p| (p.id.clone(), p))
        .collect();
    let all_models = db.models();
    let mut taken_ids: HashSet<String> = all_models.iter().map(|m| m.id.clone()).collect();

    let mut outcome = HarnessReportOutcome::default();
    let mut seen_provider_ids: HashSet<String> = HashSet::new();

    for entry in &cleaned {
        let slug = entry.harness.trim();
        let pid = provider_id(slug, device_id);
        seen_provider_ids.insert(pid.clone());

        if !entry.sign_in.is_signed_in() {
            // Installed but not signed in / expired: register nothing new; if previously
            // registered, update provider options and hide its models without deleting.
            if existing_providers.contains_key(&pid) {
                db.upsert_provider(&ProviderConfig {
                    id: pid.clone(),
                    kind: "harness".into(),
                    base_url: None,
                    api_key: None,
                    options: build_provider_options(entry, device_id, true),
                })?;
                outcome.hidden += hide_provider_models(db, &all_models, &pid)?;
            }
            continue;
        }

        db.upsert_provider(&ProviderConfig {
            id: pid.clone(),
            kind: "harness".into(),
            base_url: None,
            api_key: None,
            options: build_provider_options(entry, device_id, true),
        })?;

        let existing_for_provider: HashMap<&str, &ModelEntry> = all_models
            .iter()
            .filter(|m| m.provider_id == pid)
            .map(|m| (m.model.as_str(), m))
            .collect();

        let mut seen_models: HashSet<String> = HashSet::new();
        let tools_str = if entry.capabilities.supports_mcp || entry.capabilities.can_orchestrate {
            "yes"
        } else {
            "unknown"
        };

        for hm in &entry.models {
            let model_name = hm.id.trim();
            if model_name.is_empty() || !seen_models.insert(model_name.to_string()) {
                continue;
            }
            let table_ctx = lookup_price(model_name).map(|p| p.context_window).unwrap_or(0);

            if let Some(prev) = existing_for_provider.get(model_name) {
                let keep_user_hidden = prev.hidden && prev.hidden_reason.as_deref() == Some("user");
                let updated_entry = ModelEntry {
                    id: prev.id.clone(),
                    provider_id: pid.clone(),
                    model: model_name.to_string(),
                    input_price: 0.0,
                    output_price: 0.0,
                    cache_read_price: None,
                    cache_write_price: None,
                    context_window: if prev.context_window > 0 { prev.context_window } else { table_ctx },
                    use_cases: prev.use_cases.clone(),
                    free: false,
                    roles: prev.roles.clone(),
                    local: false,
                    modalities: prev.modalities.clone(),
                    kind: "harness".into(),
                    device_id: Some(device_id.to_string()),
                    can_orchestrate: entry.capabilities.can_orchestrate,
                    source: "auto".into(),
                    hidden: keep_user_hidden,
                    hidden_reason: keep_user_hidden.then(|| "user".to_string()),
                    last_seen: Some(ts),
                    display_name: hm.display_name.clone().or_else(|| prev.display_name.clone()),
                    price_source: "none".into(),
                    tools: tools_str.into(),
                };
                db.upsert_model(&updated_entry)?;
                outcome.updated += 1;
            } else {
                let base_id = model_registry_id(slug, device_id, model_name);
                let mut reg_id = base_id.clone();
                let mut suffix = 2u32;
                while taken_ids.contains(&reg_id) {
                    reg_id = format!("{base_id}-{suffix}");
                    suffix += 1;
                }
                taken_ids.insert(reg_id.clone());

                let new_entry = ModelEntry {
                    id: reg_id,
                    provider_id: pid.clone(),
                    model: model_name.to_string(),
                    input_price: 0.0,
                    output_price: 0.0,
                    cache_read_price: None,
                    cache_write_price: None,
                    context_window: table_ctx,
                    use_cases: String::new(),
                    free: false,
                    roles: Vec::new(),
                    local: false,
                    modalities: Vec::new(),
                    kind: "harness".into(),
                    device_id: Some(device_id.to_string()),
                    can_orchestrate: entry.capabilities.can_orchestrate,
                    source: "auto".into(),
                    hidden: false,
                    hidden_reason: None,
                    last_seen: Some(ts),
                    display_name: hm.display_name.clone(),
                    price_source: "none".into(),
                    tools: tools_str.into(),
                };
                db.upsert_model(&new_entry)?;
                outcome.added += 1;
            }
        }

        // Hide any previously registered models on this harness that disappeared from its list.
        for prev in existing_for_provider.values() {
            if !seen_models.contains(&prev.model) && !prev.hidden {
                db.set_model_hidden(&prev.id, true, Some("gone"))?;
                outcome.hidden += 1;
            }
        }
    }

    // Hide models of any harness provider on this device whose CLI disappeared from the report.
    for (pid, prov) in &existing_providers {
        if !provider_belongs_to_device(prov, device_id, &device.name) || seen_provider_ids.contains(pid) {
            continue;
        }
        let mut opts = prov.options.as_object().cloned().unwrap_or_default();
        opts.insert("installed".into(), json!(false));
        opts.insert("signed_in".into(), json!(false));
        opts.insert("sign_in".into(), json!(HarnessSignIn::NotSignedIn.as_str()));
        db.upsert_provider(&ProviderConfig {
            id: pid.clone(),
            kind: "harness".into(),
            base_url: None,
            api_key: None,
            options: Value::Object(opts),
        })?;
        outcome.hidden += hide_provider_models(db, &all_models, pid)?;
    }

    Ok(outcome)
}

// ---- Standard-chat harness turn execution & session continuity

const HARNESS_TURN_TIMEOUT: Duration = Duration::from_secs(630);
const MAX_COMPACT_MSG_CHARS: usize = 2_000;
const MAX_COMPACT_TOTAL_CHARS: usize = 16_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessTurnReply {
    pub text: String,
    pub session_id: String,
    pub usage: HarnessUsage,
}

pub(crate) struct PendingHarnessTurn {
    pub(crate) device_id: String,
    pub(crate) conn_id: u64,
    pub(crate) tx: oneshot::Sender<std::result::Result<HarnessTurnReply, String>>,
}

struct ClearPendingOnDrop {
    hub: Weak<Hub>,
    conversation_id: Option<String>,
}

impl ClearPendingOnDrop {
    fn disarm(&mut self) {
        self.conversation_id = None;
    }
}

impl Drop for ClearPendingOnDrop {
    fn drop(&mut self) {
        let (Some(hub), Some(cid)) = (self.hub.upgrade(), self.conversation_id.take()) else {
            return;
        };
        tokio::spawn(async move {
            hub.harness_turns.lock().await.remove(&cid);
        });
    }
}

/// Canonical CLI agent slug (`claude-code`, `codex`, `antigravity`, `grok`, `hermes`, …) for `model`.
pub fn harness_agent_of(model: &ModelEntry, provider: Option<&ProviderConfig>) -> Option<String> {
    if let Some(slug) = provider
        .and_then(|p| p.options.get("harness"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(slug.to_string());
    }
    model
        .provider_id
        .split_once('@')
        .map(|(slug, _)| slug.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn harness_label(model: &ModelEntry, provider: Option<&ProviderConfig>) -> String {
    let cli = provider
        .and_then(|p| p.options.get("display_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| harness_agent_of(model, provider))
        .unwrap_or_else(|| model.provider_id.clone());
    format!("{cli} ({})", model.model)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut iter = text.chars();
    let head: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn strip_memory_block(text: &str) -> &str {
    match text.find("\n\n[Memory]\n") {
        Some(idx) => &text[..idx],
        None => text,
    }
}

/// Earlier visible `(role, text)` turns in `conversation_id` before `current_wakes`.
///
/// Used when switching into a harness mid-chat so its first turn carries a compact
/// copy of the earlier visible conversation.
pub(crate) fn prior_visible_turns(
    db: &Db,
    conversation_id: &str,
    current_wakes: &[Wake],
) -> Vec<(&'static str, String)> {
    let mut turns: Vec<(&'static str, String)> = db
        .events(conversation_id, 0, None)
        .into_iter()
        .filter_map(|ev| match ev.body {
            EventBody::UserMessage { text } => {
                let t = text.trim().to_string();
                (!t.is_empty()).then_some(("User", t))
            }
            EventBody::AssistantMessage { text } => {
                let t = text.trim().to_string();
                (!t.is_empty()).then_some(("Assistant", t))
            }
            _ => None,
        })
        .collect();

    // `Hub::chat` records the current turn's `UserMessage` event before waking the worker;
    // strip it so the current message is not duplicated in `[Earlier in this conversation]`.
    for wake in current_wakes.iter().rev() {
        if let Wake::User(u) = wake {
            let expected = u.trim();
            if turns.last().is_some_and(|(role, t)| *role == "User" && t == expected) {
                turns.pop();
            }
        }
    }

    if !turns.is_empty() {
        return turns;
    }

    // Fallback when a test calls `run_turn` directly after populating `db.save_history` without events.
    db.history(conversation_id)
        .into_iter()
        .filter_map(|m| {
            let role = match m.role {
                providers::Role::User => "User",
                providers::Role::Assistant => "Assistant",
            };
            let clean = strip_memory_block(&m.text()).trim().to_string();
            (!clean.is_empty()).then_some((role, clean))
        })
        .collect()
}

/// Build the message sent on the first turn when switching into a harness mid-chat.
pub fn format_switched_message(prior_turns: &[(&str, String)], message: &str) -> String {
    if prior_turns.is_empty() {
        return message.to_string();
    }
    let mut selected_rev: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut omitted = 0usize;
    for (idx, (role, text)) in prior_turns.iter().enumerate().rev() {
        let line = format!("{role}: {}", truncate_chars(text.trim(), MAX_COMPACT_MSG_CHARS));
        let len = line.chars().count() + 1;
        if !selected_rev.is_empty() && used + len > MAX_COMPACT_TOTAL_CHARS {
            omitted = idx + 1;
            break;
        }
        used += len;
        selected_rev.push(line);
    }
    selected_rev.reverse();

    let mut out = String::from("[Earlier in this conversation]\n");
    if omitted > 0 {
        out.push_str(&format!("({omitted} earlier message(s) omitted)\n"));
    }
    out.push_str(&selected_rev.join("\n"));
    out.push_str("\n\n[Current message]\n");
    out.push_str(message);
    out
}

fn probe_provider_for_agent(agent: &str) -> Option<&'static str> {
    match agent {
        "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

impl Hub {
    /// Execute one harness turn on the device hosting `model`, resuming the
    /// conversation's stored `harness_session` (or seeding a new session with a compact
    /// copy of earlier visible turns when switching into a harness mid-chat).
    ///
    /// `orchestrator_prompt` is set for a project chat. The harness must then be on this
    /// hub's own device, and the turn carries the chat's loopback MCP token.
    pub(crate) async fn run_standard_harness_turn(
        self: &Arc<Self>,
        conversation_id: &str,
        model: &ModelEntry,
        message: &str,
        wakes: &[Wake],
        orchestrator_prompt: Option<String>,
    ) -> std::result::Result<HarnessTurnReply, String> {
        let provider = self
            .db
            .provider(&model.provider_id)
            .map_err(|e| e.to_string())?;
        let Some(agent) = harness_agent_of(model, provider.as_ref()) else {
            return Err(format!("harness model {} has no CLI agent configured", model.id));
        };
        let label = harness_label(model, provider.as_ref());
        // Hermes ignores the confinement level (its one-shot mode runs every tool without asking)
        // and OpenCode always auto-approves. The design asks for an approval per turn for these;
        // until that exists they are refused rather than run unconfined on the owner's device.
        if UNCONFINABLE_HARNESSES.contains(&agent.as_str()) {
            return Err(format!(
                "{label} can't be confined to a safe mode yet, so Grouper won't run it in a chat. Pick Claude Code, Codex, Antigravity, Grok or pi, or an API model."
            ));
        }
        let host_ref = model
            .device_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| provider.as_ref().and_then(|p| p.via_device()))
            .ok_or_else(|| format!("{label} has no hosting device configured"))?;

        let (device_id, device_name) = self
            .resolve_device(host_ref)
            .ok_or_else(|| format!("{label} names unknown device {host_ref:?}"))?;

        if orchestrator_prompt.is_some() {
            let hub_id = self.db.hub_device_id();
            if device_id != hub_id {
                let hub_name = self.resolve_device(&hub_id).map(|(_, name)| name).unwrap_or(hub_id);
                return Err(format!(
                    "{label} is on {device_name}, not this hub's computer ({hub_name}). A harness can orchestrate a project chat only from the hub's own device."
                ));
            }
        }

        let conn_id = {
            let live = self.live.lock().await;
            let Some(conn) = live.get(&device_id) else {
                return Err(format!(
                    "{label} runs on {device_name}, which is offline. Bring {device_name} back online or switch models to continue."
                ));
            };
            conn.conn_id
        };

        // Clear any uncommitted session if the user switched between two different harness providers,
        // and check whether this turn resumes an existing continuous session.
        let stored_session = self
            .db
            .conversation_harness_session_for(conversation_id, &model.provider_id);

        let wire_message = if stored_session.is_none() {
            let prior = prior_visible_turns(&self.db, conversation_id, wakes);
            format_switched_message(&prior, message)
        } else {
            message.to_string()
        };

        let (mcp, allowed_tools, system_prompt) = if let Some(prompt) = orchestrator_prompt {
            let port = self.loopback_port();
            if port == 0 {
                return Err(
                    "this hub has no loopback port, so it cannot give the harness its orchestrator MCP endpoint"
                        .into(),
                );
            }
            let token = self
                .db
                .replace_orchestrator_token(conversation_id)
                .map_err(|e| e.to_string())?;
            (
                Some(core_protocol::HarnessMcp {
                    url: format!("http://127.0.0.1:{port}/mcp/orchestrator"),
                    bearer_token: token,
                }),
                vec!["mcp__grouper__*".to_string()],
                Some(prompt),
            )
        } else {
            (None, Vec::new(), None)
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut turns = self.harness_turns.lock().await;
            if turns.contains_key(conversation_id) {
                return Err("a harness turn is already running for this conversation".into());
            }
            turns.insert(
                conversation_id.to_string(),
                PendingHarnessTurn {
                    device_id: device_id.clone(),
                    conn_id,
                    tx,
                },
            );
        }
        let mut guard = ClearPendingOnDrop {
            hub: Arc::downgrade(self),
            conversation_id: Some(conversation_id.to_string()),
        };

        let queued = self
            .queue_to_model_host(
                &device_id,
                conn_id,
                HubMessage::HarnessTurn {
                    conversation_id: conversation_id.to_string(),
                    agent: agent.clone(),
                    model: model.model.clone(),
                    session_id: stored_session,
                    message: wire_message,
                    mcp,
                    allowed_tools,
                    system_prompt,
                },
            )
            .await;

        if queued != QueueResult::Sent {
            self.harness_turns.lock().await.remove(conversation_id);
            guard.disarm();
            return Err(if queued == QueueResult::Full {
                format!("{device_name} is busy; try again shortly.")
            } else {
                format!(
                    "{label} runs on {device_name}, which is offline. Bring {device_name} back online or switch models to continue."
                )
            });
        }

        let outcome = tokio::time::timeout(HARNESS_TURN_TIMEOUT, rx).await;
        if !self.harness_turns.lock().await.contains_key(conversation_id) {
            guard.disarm();
        }
        let reply = match outcome {
            Ok(Ok(Ok(reply))) => reply,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(_)) => {
                return Err(format!(
                    "{device_name} disconnected while running {label}."
                ));
            }
            Err(_) => {
                return Err(format!(
                    "{label} on {device_name} timed out."
                ));
            }
        };

        if !reply.session_id.trim().is_empty() {
            self.db.set_conversation_harness_session(
                conversation_id,
                &model.provider_id,
                Some(reply.session_id.trim()),
            );
        }
        self.db.record_subscription_usage(
            Some(conversation_id),
            None,
            &model.provider_id,
            &model.model,
            reply.usage.input_tokens,
            reply.usage.output_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.cache_write_tokens,
        );
        if let Some(probe_name) = probe_provider_for_agent(&agent) {
            let hub = Arc::clone(self);
            let dev = device_id.clone();
            tokio::spawn(async move {
                crate::usage::probe_device(&hub, &dev, Some(&[probe_name])).await;
            });
        }
        Ok(reply)
    }

    pub(crate) async fn harness_finished(
        &self,
        device_id: &str,
        conn_id: u64,
        conversation_id: &str,
        text: String,
        session_id: String,
        usage: HarnessUsage,
    ) {
        let pending = {
            let mut turns = self.harness_turns.lock().await;
            if turns
                .get(conversation_id)
                .is_some_and(|p| p.device_id == device_id && p.conn_id == conn_id)
            {
                turns.remove(conversation_id)
            } else {
                None
            }
        };
        if let Some(p) = pending {
            let _ = p.tx.send(Ok(HarnessTurnReply { text, session_id, usage }));
        }
    }

    pub(crate) async fn harness_failed(
        &self,
        device_id: &str,
        conn_id: u64,
        conversation_id: &str,
        error: String,
    ) {
        let pending = {
            let mut turns = self.harness_turns.lock().await;
            if turns
                .get(conversation_id)
                .is_some_and(|p| p.device_id == device_id && p.conn_id == conn_id)
            {
                turns.remove(conversation_id)
            } else {
                None
            }
        };
        if let Some(p) = pending {
            let _ = p.tx.send(Err(error));
        }
    }

    pub(crate) async fn harness_connection_gone(&self, device_id: &str, conn_id: u64) {
        let removed: Vec<PendingHarnessTurn> = {
            let mut turns = self.harness_turns.lock().await;
            let keys: Vec<String> = turns
                .iter()
                .filter(|(_, p)| p.device_id == device_id && p.conn_id == conn_id)
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter().filter_map(|k| turns.remove(&k)).collect()
        };
        if removed.is_empty() {
            return;
        }
        let device_name = self
            .resolve_device(device_id)
            .map(|(_, name)| name)
            .unwrap_or_else(|| device_id.to_string());
        for p in removed {
            let _ = p.tx.send(Err(format!(
                "{device_name} went offline while running the harness turn."
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::random_hex;
    use core_protocol::{HarnessCapabilities, HarnessModel};

    fn tmp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-hub-harness-{}", random_hex(4)))
    }

    fn claude_entry(sign_in: HarnessSignIn, version: &str, models: &[(&str, &str)]) -> DetectedHarness {
        DetectedHarness {
            harness: "claude-code".into(),
            display_name: "Claude Code".into(),
            version: Some(version.into()),
            sign_in,
            login_command: (!sign_in.is_signed_in()).then(|| "claude login".into()),
            capabilities: HarnessCapabilities {
                can_orchestrate: true,
                supports_resume: true,
                supports_mcp: true,
            },
            models: models
                .iter()
                .map(|(id, name)| HarnessModel { id: (*id).into(), display_name: Some((*name).into()) })
                .collect(),
        }
    }

    fn codex_entry(sign_in: HarnessSignIn, models: &[(&str, &str)]) -> DetectedHarness {
        DetectedHarness {
            harness: "codex".into(),
            display_name: "Codex".into(),
            version: Some("0.9.0".into()),
            sign_in,
            login_command: (!sign_in.is_signed_in()).then(|| "codex login".into()),
            capabilities: HarnessCapabilities {
                can_orchestrate: false,
                supports_resume: true,
                supports_mcp: true,
            },
            models: models
                .iter()
                .map(|(id, name)| HarnessModel { id: (*id).into(), display_name: Some((*name).into()) })
                .collect(),
        }
    }

    #[test]
    fn report_creates_updates_and_hides_harness_rows_without_deleting() {
        let dir = tmp_dir();
        let db = Db::open(&dir).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();

        // 1. Initial report: signed-in Claude Code (2 models), signed-in Codex (1 model),
        //    and unsigned-in Grok (must register no provider/model rows).
        let unsigned_grok = DetectedHarness {
            harness: "grok".into(),
            display_name: "Grok".into(),
            version: Some("1.0".into()),
            sign_in: HarnessSignIn::NotSignedIn,
            login_command: Some("grok auth".into()),
            capabilities: HarnessCapabilities::default(),
            models: Vec::new(),
        };
        let out1 = apply_report(
            &db,
            &pc.id,
            &[
                claude_entry(HarnessSignIn::SignedIn, "1.0.0", &[("sonnet", "Claude Sonnet"), ("opus", "Claude Opus")]),
                codex_entry(HarnessSignIn::SignedIn, &[("gpt-5.2", "GPT-5.2")]),
                unsigned_grok,
            ],
        )
        .unwrap();
        assert_eq!(out1, HarnessReportOutcome { added: 3, updated: 0, hidden: 0 });

        let providers = db.providers().unwrap();
        assert_eq!(providers.len(), 2, "unsigned-in CLI must not create a provider row");
        let cc_prov = db.provider(&provider_id("claude-code", &pc.id)).unwrap().unwrap();
        assert_eq!(cc_prov.kind, "harness");
        assert_eq!(cc_prov.via_device(), Some(pc.id.as_str()));
        assert_eq!(cc_prov.options["signed_in"], json!(true));
        assert_eq!(cc_prov.options["version"], json!("1.0.0"));

        let opus_id = model_registry_id("claude-code", &pc.id, "opus");
        let sonnet_id = model_registry_id("claude-code", &pc.id, "sonnet");
        let codex_id = model_registry_id("codex", &pc.id, "gpt-5.2");

        let opus = db.find_model(&opus_id).unwrap();
        assert_eq!(opus.kind, "harness");
        assert_eq!(opus.device_id.as_deref(), Some(pc.id.as_str()));
        assert!(opus.can_orchestrate);
        assert!(!opus.free);
        assert!(!opus.local);
        assert!(!db.model_is_local(&opus));
        assert_eq!(opus.source, "auto");
        assert_eq!(opus.price_source, "none");

        let codex = db.find_model(&codex_id).unwrap();
        assert_eq!(codex.kind, "harness");
        assert!(!codex.can_orchestrate, "Codex cannot orchestrate without tool confinement");

        // Owner hides sonnet manually; customize opus use_cases to verify updates preserve user fields.
        let mut opus_custom = opus.clone();
        opus_custom.use_cases = "complex Rust refactoring".into();
        db.upsert_model(&opus_custom).unwrap();
        db.set_model_hidden(&sonnet_id, true, Some("user")).unwrap();

        // 2. Second report: Claude Code upgrades to 1.1.0, adds `haiku`, keeps `opus` + `sonnet`;
        //    Codex disappears completely from the device.
        let out2 = apply_report(
            &db,
            &pc.id,
            &[claude_entry(
                HarnessSignIn::SignedIn,
                "1.1.0",
                &[("sonnet", "Claude Sonnet"), ("opus", "Claude Opus 5"), ("haiku", "Claude Haiku")],
            )],
        )
        .unwrap();
        assert_eq!(out2, HarnessReportOutcome { added: 1, updated: 2, hidden: 1 });

        let cc_prov = db.provider(&provider_id("claude-code", &pc.id)).unwrap().unwrap();
        assert_eq!(cc_prov.options["version"], json!("1.1.0"));
        let opus_after = db.find_model(&opus_id).unwrap();
        assert_eq!(opus_after.display_name.as_deref(), Some("Claude Opus 5"));
        assert_eq!(opus_after.use_cases, "complex Rust refactoring", "sync preserves use_cases");
        let sonnet_after = db.find_model(&sonnet_id).unwrap();
        assert!(sonnet_after.hidden && sonnet_after.hidden_reason.as_deref() == Some("user"));

        // Codex model is hidden ("gone"), NOT deleted; provider row also remains with installed=false.
        let codex_after = db.find_model(&codex_id).unwrap();
        assert!(codex_after.hidden);
        assert_eq!(codex_after.hidden_reason.as_deref(), Some("gone"));
        let codex_prov = db.provider(&provider_id("codex", &pc.id)).unwrap().unwrap();
        assert_eq!(codex_prov.options["installed"], json!(false));
        assert_eq!(codex_prov.options["signed_in"], json!(false));

        // 3. Third report: Claude Code credential expires; Codex comes back signed in.
        let out3 = apply_report(
            &db,
            &pc.id,
            &[
                claude_entry(HarnessSignIn::Expired, "1.1.0", &[]),
                codex_entry(HarnessSignIn::SignedIn, &[("gpt-5.2", "GPT-5.2")]),
            ],
        )
        .unwrap();
        // opus + haiku become hidden (sonnet was already hidden by user); codex is updated & unhidden.
        assert_eq!(out3, HarnessReportOutcome { added: 0, updated: 1, hidden: 2 });
        assert!(db.find_model(&opus_id).unwrap().hidden);
        assert_eq!(db.find_model(&sonnet_id).unwrap().hidden_reason.as_deref(), Some("user"));
        assert!(!db.find_model(&codex_id).unwrap().hidden, "reappearing CLI unhides gone model");
        assert_eq!(db.models().len(), 4, "no model rows are ever deleted");

        // Stored report on hub_meta retains latest scan for the device page.
        assert_eq!(device_report(&db, &pc.id).len(), 2);

        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_on_different_devices_stay_isolated() {
        let dir = tmp_dir();
        let db = Db::open(&dir).unwrap();
        let (pc, _) = db.add_device("PC", "windows", false).unwrap();
        let (mac, _) = db.add_device("Mac", "macos", false).unwrap();

        apply_report(&db, &pc.id, &[claude_entry(HarnessSignIn::SignedIn, "1.0", &[("opus", "Opus")])]).unwrap();
        apply_report(&db, &mac.id, &[claude_entry(HarnessSignIn::SignedIn, "1.0", &[("opus", "Opus")])]).unwrap();
        assert_eq!(db.visible_models().len(), 2);

        // Empty report on PC hides PC's harness models only; Mac's stay visible.
        apply_report(&db, &pc.id, &[]).unwrap();
        let visible = db.visible_models();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].device_id.as_deref(), Some(mac.id.as_str()));

        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    const FAKE_CLAUDE_RUST_SRC: &str = r#"
use std::env;
use std::fs;
use std::path::PathBuf;

fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut prompt = String::new();
    let mut resume: Option<String> = None;
    let mut perm_mode = String::new();
    let mut model = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" if i + 1 < args.len() => { prompt = args[i + 1].clone(); i += 2; }
            "--resume" if i + 1 < args.len() => { resume = Some(args[i + 1].clone()); i += 2; }
            "--permission-mode" if i + 1 < args.len() => { perm_mode = args[i + 1].clone(); i += 2; }
            "--model" if i + 1 < args.len() => { model = args[i + 1].clone(); i += 2; }
            _ => { i += 1; }
        }
    }
    if let Ok(home) = env::var("HOME") {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let entry = format!(
            "mode={perm_mode}|model={model}|resume={}|cwd={}|prompt={}\n---\n",
            resume.as_deref().unwrap_or("NONE"),
            cwd.display(),
            prompt
        );
        let log_path = PathBuf::from(home).join("invocations.log");
        let prev = fs::read_to_string(&log_path).unwrap_or_default();
        let _ = fs::write(&log_path, format!("{prev}{entry}"));
    }
    let sid = resume.clone().unwrap_or_else(|| "fake-claude-sess-1".to_string());
    let tag = if resume.is_some() { "resumed" } else { "started" };
    let reply = format!("[{tag}:{sid}:{perm_mode}] {prompt}");
    println!(
        "{{\"type\":\"result\",\"is_error\":false,\"session_id\":\"{}\",\"result\":\"{}\",\"usage\":{{\"input_tokens\":25,\"output_tokens\":15,\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":0}}}}",
        json_escape(&sid),
        json_escape(&reply),
    );
}
"#;

    fn compile_fake_claude(bin_dir: &std::path::Path) {
        std::fs::create_dir_all(bin_dir).unwrap();
        let src = bin_dir.join("fake_claude.rs");
        std::fs::write(&src, FAKE_CLAUDE_RUST_SRC).unwrap();
        let exe = bin_dir.join(if cfg!(windows) { "claude.exe" } else { "claude" });
        let status = std::process::Command::new("rustc")
            .arg(&src)
            .arg("-O")
            .arg("-o")
            .arg(&exe)
            .status()
            .expect("rustc must be available during cargo test");
        assert!(status.success(), "compiling fake claude binary failed");
    }

    async fn wait_for_turn_finished(
        rx: &mut tokio::sync::broadcast::Receiver<core_protocol::Event>,
        conv_id: &str,
    ) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let ev = rx.recv().await.expect("event stream open");
                if ev.conversation_id == conv_id
                    && matches!(ev.body, EventBody::TurnFinished { .. })
                {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for TurnFinished");
    }

    #[tokio::test]
    async fn standard_chat_harness_resumes_session_switches_mid_chat_and_records_subscription_usage() {
        use core_client::harness::{HarnessLevel, ScanOptions, chat_workdir, turn_with};
        use core_protocol::{ChatRequest, PROTOCOL_VERSION};
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let dir = tmp_dir();
        let fake_home = dir.join("fake-home");
        let bin_dir = fake_home.join("bin");
        compile_fake_claude(&bin_dir);
        let ws_root = dir.join("device-workspaces");
        let scan_opts = ScanOptions::isolated(&fake_home, vec![bin_dir], 1_800_000_000);

        let db = Db::open(&dir).unwrap();
        let (pc, pc_token) = db.add_device("Gaming PC", "windows", false).unwrap();
        apply_report(
            &db,
            &pc.id,
            &[claude_entry(HarnessSignIn::SignedIn, "1.0.0", &[("sonnet", "Claude Sonnet")])],
        )
        .unwrap();
        let sonnet_id = model_registry_id("claude-code", &pc.id, "sonnet");

        // Also register a scripted API model so we can test mid-chat switching into the harness.
        db.upsert_provider(&ProviderConfig {
            id: "scripted".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({
                "inline": {
                    "rules": [
                        {"last": "My code word is cobalt", "reply": [{"type": "text", "text": "Noted: cobalt."}]}
                    ],
                    "default": [{"type": "text", "text": "API reply."}]
                }
            }),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "api-std".into(),
            provider_id: "scripted".into(),
            model: "scripted-1".into(),
            input_price: 2.0,
            output_price: 4.0,
            roles: vec!["standard".into()],
            ..Default::default()
        })
        .unwrap();

        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_hub = hub.clone();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, crate::server::router(server_hub)).await;
        });

        // Connect a simulated device over WebSocket that runs `core_client::harness::turn_with`
        // against our fake CLI binary (never touching real CLIs).
        let ws_url = format!("ws://{addr}{}", core_protocol::CONNECT_PATH);
        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {pc_token}").parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        let hello = core_protocol::NodeMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: "0.1.0".into(),
            host: None,
        };
        sink.send(WsMsg::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .unwrap();

        // Wait until Welcome arrives so `pc` is registered in `hub.live`.
        loop {
            let Some(Ok(WsMsg::Text(t))) = stream.next().await else { panic!("ws closed before welcome") };
            if matches!(serde_json::from_str::<HubMessage>(&t), Ok(HubMessage::Welcome { .. })) {
                break;
            }
        }

        let seen_turns = Arc::new(std::sync::Mutex::new(Vec::<(Option<String>, String)>::new()));
        let seen_for_dev = seen_turns.clone();
        let dev_task = tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let WsMsg::Text(t) = msg else { continue };
                if let Ok(HubMessage::HarnessTurn {
                    conversation_id,
                    agent,
                    model,
                    session_id,
                    message,
                    ..
                }) = serde_json::from_str::<HubMessage>(&t)
                {
                    seen_for_dev
                        .lock()
                        .unwrap()
                        .push((session_id.clone(), message.clone()));
                    let workdir = chat_workdir(Some(&ws_root), &conversation_id).unwrap();
                    let reply = match turn_with(
                        &scan_opts,
                        &agent,
                        &model,
                        session_id.as_deref(),
                        &message,
                        &workdir,
                        HarnessLevel::Confined,
                        None,
                    )
                    .await
                    {
                        Ok(out) => core_protocol::NodeMessage::HarnessFinished {
                            conversation_id,
                            text: out.text,
                            session_id: out.session_id,
                            usage: out.usage,
                        },
                        Err(error) => core_protocol::NodeMessage::HarnessFailed {
                            conversation_id,
                            error,
                        },
                    };
                    if sink
                        .send(WsMsg::Text(serde_json::to_string(&reply).unwrap().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });

        let mut events_rx = hub.subscribe_events_for_test();

        // 1. Two-turn continuous harness conversation from the start.
        let conv = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("standard".into()),
                text: "Hello turn one".into(),
                model: Some(sonnet_id.clone()),
            })
            .await
            .unwrap();
        wait_for_turn_finished(&mut events_rx, &conv).await;

        assert_eq!(
            hub.db.conversation_harness_session(&conv).as_deref(),
            Some("fake-claude-sess-1"),
            "turn 1 must store returned session_id in conversations.harness_session"
        );

        hub.chat(ChatRequest {
            conversation_id: Some(conv.clone()),
            mode: None,
            text: "Follow up turn two".into(),
            model: None,
        })
        .await
        .unwrap();
        wait_for_turn_finished(&mut events_rx, &conv).await;

        assert_eq!(
            hub.db.conversation_harness_session(&conv).as_deref(),
            Some("fake-claude-sess-1"),
            "turn 2 must keep the continuous session_id"
        );

        {
            let recorded = seen_turns.lock().unwrap();
            assert_eq!(recorded.len(), 2);
            assert_eq!(recorded[0].0, None, "turn 1 starts a new session");
            assert_eq!(recorded[0].1, "Hello turn one");
            assert_eq!(
                recorded[1].0.as_deref(),
                Some("fake-claude-sess-1"),
                "turn 2 resumes turn 1's session id"
            );
            assert_eq!(recorded[1].1, "Follow up turn two");
        }

        // Verify visible transcript events and zero-cost subscription usage rows.
        let stored_events = hub.db.events(&conv, 0, None);
        let assistant_texts: Vec<&str> = stored_events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::AssistantMessage { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(assistant_texts.len(), 2);
        assert!(assistant_texts[0].contains("[started:fake-claude-sess-1:plan] Hello turn one"));
        assert!(assistant_texts[1].contains("[resumed:fake-claude-sess-1:plan] Follow up turn two"));

        let usage_rows: Vec<(i64, i64, i64, f64, String)> = {
            let c = hub.db.c();
            let mut st = c
                .prepare(
                    "SELECT input_tokens, output_tokens, cache_read_tokens, cost, billing
                     FROM usage WHERE conversation_id = ?1 ORDER BY id",
                )
                .unwrap();
            st.query_map([&conv], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
        };
        assert_eq!(usage_rows.len(), 2);
        for row in &usage_rows {
            assert_eq!(*row, (25, 15, 5, 0.0, "subscription".to_string()));
        }
        assert_eq!(hub.db.spent_by_conversation(&conv), 0.0);

        // 2. Mid-chat switch from API model into harness carries compact visible transcript on the first harness turn only.
        let switched_conv = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("standard".into()),
                text: "My code word is cobalt".into(),
                model: Some("api-std".into()),
            })
            .await
            .unwrap();
        wait_for_turn_finished(&mut events_rx, &switched_conv).await;
        assert_eq!(hub.db.conversation_harness_session(&switched_conv), None);

        hub.db
            .set_conversation_model(&switched_conv, Some(&sonnet_id))
            .unwrap();
        hub.chat(ChatRequest {
            conversation_id: Some(switched_conv.clone()),
            mode: None,
            text: "What was my code word?".into(),
            model: None,
        })
        .await
        .unwrap();
        wait_for_turn_finished(&mut events_rx, &switched_conv).await;

        hub.chat(ChatRequest {
            conversation_id: Some(switched_conv.clone()),
            mode: None,
            text: "One more turn".into(),
            model: None,
        })
        .await
        .unwrap();
        wait_for_turn_finished(&mut events_rx, &switched_conv).await;

        {
            let recorded = seen_turns.lock().unwrap();
            assert_eq!(recorded.len(), 4);
            assert_eq!(recorded[2].0, None, "first turn after switching into harness starts new session");
            assert!(recorded[2].1.contains("[Earlier in this conversation]"), "{}", recorded[2].1);
            assert!(recorded[2].1.contains("User: My code word is cobalt"), "{}", recorded[2].1);
            assert!(recorded[2].1.contains("Assistant: Noted: cobalt."), "{}", recorded[2].1);
            assert!(recorded[2].1.contains("What was my code word?"), "{}", recorded[2].1);
            assert_eq!(recorded[3].0.as_deref(), Some("fake-claude-sess-1"));
            assert!(!recorded[3].1.contains("[Earlier in this conversation]"), "{}", recorded[3].1);
            assert_eq!(recorded[3].1, "One more turn");
        }

        dev_task.abort();
        server_task.abort();
        drop(hub);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn standard_chat_harness_on_offline_device_emits_clear_error() {
        use core_protocol::ChatRequest;

        let dir = tmp_dir();
        let db = Db::open(&dir).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();
        apply_report(
            &db,
            &pc.id,
            &[claude_entry(HarnessSignIn::SignedIn, "1.0.0", &[("opus", "Claude Opus")])],
        )
        .unwrap();
        let opus_id = model_registry_id("claude-code", &pc.id, "opus");

        let hub = Hub::new(db, dir.clone());
        let mut events_rx = hub.subscribe_events_for_test();

        let conv = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("standard".into()),
                text: "Are you online?".into(),
                model: Some(opus_id),
            })
            .await
            .unwrap();
        wait_for_turn_finished(&mut events_rx, &conv).await;

        let events = hub.db.events(&conv, 0, None);
        let err_msg = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::Error { message } => Some(message.as_str()),
                _ => None,
            })
            .expect("expected offline Error event in chat");
        assert!(err_msg.contains("Gaming PC"), "error should name offline device: {err_msg}");
        assert!(err_msg.contains("offline"), "error should mention offline: {err_msg}");
        assert_eq!(hub.db.conversation_harness_session(&conv), None);

        drop(hub);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[derive(Clone, Debug)]
    struct OrchestratorTurn {
        session_id: Option<String>,
        message: String,
        mcp_url: Option<String>,
        mcp_token: Option<String>,
        allowed_tools: Vec<String>,
        system_prompt: Option<String>,
    }

    fn live_orchestrator_tokens(db: &Db, conversation_id: &str) -> i64 {
        db.c()
            .query_row(
                "SELECT COUNT(*) FROM orchestrator_tokens WHERE conversation_id = ?1 AND revoked = 0",
                [conversation_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Project chat on the hub's own device: turn 2 and a thread wake-up resume one session,
    /// each turn carries that chat's loopback MCP token, and deleting the chat revokes it.
    #[tokio::test]
    async fn project_harness_orchestrator_resumes_on_turn_two_and_on_wake() {
        use core_protocol::{ChatRequest, PROTOCOL_VERSION};
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message as WsMsg;

        let dir = tmp_dir();
        let db = Db::open(&dir).unwrap();
        let (pc, pc_token) = db.add_device("Gaming PC", "windows", false).unwrap();
        db.set_hub_device_id(&pc.id).unwrap();
        apply_report(
            &db,
            &pc.id,
            &[claude_entry(HarnessSignIn::SignedIn, "1.0.0", &[("opus", "Claude Opus")])],
        )
        .unwrap();
        let opus_id = model_registry_id("claude-code", &pc.id, "opus");

        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        hub.set_loopback_port(port);
        let server_hub = hub.clone();
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, crate::server::router(server_hub)).await;
        });

        let ws_url = format!("ws://{addr}{}", core_protocol::CONNECT_PATH);
        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {pc_token}").parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        sink.send(WsMsg::Text(
            serde_json::to_string(&core_protocol::NodeMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_version: "0.1.0".into(),
                host: None,
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
        loop {
            let Some(Ok(WsMsg::Text(t))) = stream.next().await else { panic!("ws closed before welcome") };
            if matches!(serde_json::from_str::<HubMessage>(&t), Ok(HubMessage::Welcome { .. })) {
                break;
            }
        }

        let seen_turns = Arc::new(std::sync::Mutex::new(Vec::<OrchestratorTurn>::new()));
        let seen_for_dev = seen_turns.clone();
        let dev_task = tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let WsMsg::Text(t) = msg else { continue };
                let Ok(HubMessage::HarnessTurn {
                    conversation_id,
                    session_id,
                    message,
                    mcp,
                    allowed_tools,
                    system_prompt,
                    ..
                }) = serde_json::from_str::<HubMessage>(&t)
                else {
                    continue;
                };
                let (mcp_url, mcp_token) = match mcp {
                    Some(mcp) => (Some(mcp.url), Some(mcp.bearer_token)),
                    None => (None, None),
                };
                seen_for_dev.lock().unwrap().push(OrchestratorTurn {
                    session_id: session_id.clone(),
                    message,
                    mcp_url,
                    mcp_token,
                    allowed_tools,
                    system_prompt,
                });
                let reply = core_protocol::NodeMessage::HarnessFinished {
                    conversation_id,
                    text: "orchestrator reply".into(),
                    session_id: session_id.unwrap_or_else(|| "orch-sess-1".into()),
                    usage: core_protocol::HarnessUsage {
                        input_tokens: 3,
                        output_tokens: 4,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                };
                if sink
                    .send(WsMsg::Text(serde_json::to_string(&reply).unwrap().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut events_rx = hub.subscribe_events_for_test();
        let mcp_url = format!("http://127.0.0.1:{port}/mcp/orchestrator");
        let expect_mcp = |turn: &OrchestratorTurn, previous: Option<&str>| {
            assert_eq!(turn.mcp_url.as_deref(), Some(mcp_url.as_str()));
            let token = turn.mcp_token.as_deref().expect("orchestrator turn carries the chat token");
            assert_ne!(Some(token), previous);
            assert_eq!(turn.allowed_tools, vec!["mcp__grouper__*".to_string()]);
            assert!(
                turn.system_prompt.as_deref().is_some_and(|p| p.contains("You are the orchestrator")),
                "project turn must send the orchestrator prompt"
            );
            token.to_string()
        };

        let conv = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("project".into()),
                text: "Hello turn one".into(),
                model: Some(opus_id),
            })
            .await
            .unwrap();
        wait_for_turn_finished(&mut events_rx, &conv).await;
        assert_eq!(hub.db.conversation_harness_session(&conv).as_deref(), Some("orch-sess-1"));
        let token_1 = {
            let recorded = seen_turns.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].session_id, None, "turn 1 starts a session");
            assert!(recorded[0].message.contains("Hello turn one"), "{}", recorded[0].message);
            assert!(!recorded[0].message.contains("[Earlier in this conversation]"));
            let token = expect_mcp(&recorded[0], None);
            assert_eq!(hub.db.orchestrator_conversation(&token).as_deref(), Some(conv.as_str()));
            token
        };

        hub.chat(ChatRequest {
            conversation_id: Some(conv.clone()),
            mode: None,
            text: "Follow up turn two".into(),
            model: None,
        })
        .await
        .unwrap();
        wait_for_turn_finished(&mut events_rx, &conv).await;
        assert_eq!(hub.db.conversation_harness_session(&conv).as_deref(), Some("orch-sess-1"));
        let token_2 = {
            let recorded = seen_turns.lock().unwrap();
            assert_eq!(recorded.len(), 2);
            assert_eq!(recorded[1].session_id.as_deref(), Some("orch-sess-1"), "turn 2 resumes");
            assert!(recorded[1].message.contains("Follow up turn two"), "{}", recorded[1].message);
            assert!(!recorded[1].message.contains("[Earlier in this conversation]"));
            let token = expect_mcp(&recorded[1], Some(&token_1));
            assert!(hub.db.orchestrator_conversation(&token_1).is_none(), "turn 2 replaces the previous token");
            assert_eq!(hub.db.orchestrator_conversation(&token).as_deref(), Some(conv.as_str()));
            token
        };

        hub.wake(
            &conv,
            Wake::ThreadDone {
                thread_id: "t1".into(),
                title: "notes".into(),
                status: "finished".into(),
                summary: "The notes are ready.".into(),
            },
        )
        .await;
        wait_for_turn_finished(&mut events_rx, &conv).await;
        assert_eq!(hub.db.conversation_harness_session(&conv).as_deref(), Some("orch-sess-1"));
        let token_3 = {
            let recorded = seen_turns.lock().unwrap();
            assert_eq!(recorded.len(), 3);
            assert_eq!(recorded[2].session_id.as_deref(), Some("orch-sess-1"), "a wake-up resumes the same session");
            assert!(recorded[2].message.contains("The notes are ready."), "{}", recorded[2].message);
            assert!(recorded[2].message.contains("[Thread t1 \"notes\" finished]"), "{}", recorded[2].message);
            assert!(!recorded[2].message.contains("[Earlier in this conversation]"));
            let token = expect_mcp(&recorded[2], Some(&token_2));
            assert!(hub.db.orchestrator_conversation(&token_2).is_none());
            assert_eq!(hub.db.orchestrator_conversation(&token).as_deref(), Some(conv.as_str()));
            token
        };
        assert_eq!(live_orchestrator_tokens(&hub.db, &conv), 1);

        assert!(hub.delete_conversation(&conv).await.unwrap());
        assert!(hub.db.conversation(&conv).is_none());
        assert!(hub.db.orchestrator_conversation(&token_3).is_none(), "deleting the chat revokes its token");
        assert_eq!(live_orchestrator_tokens(&hub.db, &conv), 0);

        dev_task.abort();
        server_task.abort();
        drop(hub);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn project_harness_orchestrator_on_another_device_errors() {
        use core_protocol::ChatRequest;

        let dir = tmp_dir();
        let db = Db::open(&dir).unwrap();
        let (laptop, _) = db.add_device("Laptop", "windows", false).unwrap();
        apply_report(
            &db,
            &laptop.id,
            &[claude_entry(HarnessSignIn::SignedIn, "1.0.0", &[("opus", "Claude Opus")])],
        )
        .unwrap();
        let opus_id = model_registry_id("claude-code", &laptop.id, "opus");
        let hub = Hub::new(db, dir.clone());
        let hub_id = hub.db.hub_device_id();
        assert_ne!(hub_id, laptop.id, "the harness device must not be the hub device");
        let hub_name = hub.resolve_device(&hub_id).map(|(_, name)| name).unwrap();
        let mut events_rx = hub.subscribe_events_for_test();

        let conv = hub
            .chat(ChatRequest {
                conversation_id: None,
                mode: Some("project".into()),
                text: "Orchestrate from the laptop".into(),
                model: Some(opus_id),
            })
            .await
            .unwrap();
        wait_for_turn_finished(&mut events_rx, &conv).await;

        let events = hub.db.events(&conv, 0, None);
        let err_msg = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::Error { message } => Some(message.as_str()),
                _ => None,
            })
            .expect("expected an error event");
        assert!(err_msg.contains("Laptop"), "{err_msg}");
        assert!(err_msg.contains(&hub_name), "{err_msg}");
        assert!(
            err_msg.contains("only from the hub's own device"),
            "{err_msg}"
        );
        assert!(events.iter().all(|e| !matches!(e.body, EventBody::AssistantMessage { .. })));
        assert_eq!(hub.db.conversation_harness_session(&conv), None);
        assert_eq!(live_orchestrator_tokens(&hub.db, &conv), 0);

        drop(hub);
        let _ = std::fs::remove_dir_all(dir);
    }
}
