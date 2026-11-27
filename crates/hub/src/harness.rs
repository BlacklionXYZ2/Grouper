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

use anyhow::{Result, bail};
use core_protocol::{DetectedHarness, HarnessSignIn, ModelEntry, ProviderConfig};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::catalog::derive_registry_id;
use crate::db::{Db, now};
use crate::discover::lookup_price;

const HARNESS_META_PREFIX: &str = "device_harnesses:";

/// Outcome counts from applying one device's [`DetectedHarness`] report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HarnessReportOutcome {
    pub added: usize,
    pub updated: usize,
    pub hidden: usize,
}

/// Canonical provider id for a harness CLI on a specific device (`claude-code@<device_id>`).
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
}
