//! Automatic and on-demand provider model synchronization.
//!
//! Calls [`crate::discover::discover_detailed`] for an API provider's preset, upserts
//! every chat-capable model with prices and metadata, tracks consecutive misses to hide
//! models absent from two successive successful listings (never deleting rows), and picks
//! initial `orchestrator` / `standard` role defaults only when the registry has no visible
//! models yet.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use core_protocol::{ModelEntry, ProviderConfig};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog;
use crate::db::{Db, now};
use crate::discover::{self, DiscoverResult, DiscoveredModel, Preset, PriceSource, ToolsSupport};
use crate::server::Hub;

/// Consecutive successful listings a model must be absent from before it is marked hidden (`gone`).
const MISSES_BEFORE_HIDE: i64 = 2;

/// Role defaults chosen when syncing into an empty registry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
}

impl SyncDefaults {
    fn is_empty(&self) -> bool {
        self.orchestrator.is_none() && self.standard.is_none()
    }
}

/// Summary of one provider model sync, returned by `POST /v1/providers/{id}/sync` and
/// stored on the provider under `options.sync`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncResult {
    pub provider_id: String,
    pub added: usize,
    pub updated: usize,
    pub hidden: usize,
    pub filtered: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<SyncDefaults>,
}

/// Reason a provider cannot be synced directly by the hub's API discovery path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    NotFound(String),
    NotSyncable(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) | Self::NotSyncable(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for SyncError {}

/// Resolve the discovery preset for a saved provider.
pub fn resolve_preset(provider: &ProviderConfig) -> Preset {
    let opt_preset = provider
        .options
        .get("preset")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(raw) = opt_preset
        && let Ok(preset) = Preset::from_str(raw)
    {
        return preset;
    }
    if provider.kind == "anthropic" {
        return Preset::Anthropic;
    }
    let from_url = Preset::resolve(None, provider.base_url.as_deref());
    if from_url != Preset::Generic {
        return from_url;
    }
    if provider.base_url.as_deref().map(str::trim).is_none_or(str::is_empty)
        && let Ok(preset) = Preset::from_str(&provider.id)
    {
        return preset;
    }
    Preset::Generic
}

/// Whether this provider is an API provider that can be synced directly by the hub.
pub fn is_api_provider(provider: &ProviderConfig) -> bool {
    matches!(provider.kind.as_str(), "openai" | "anthropic") && provider.via_device().is_none()
}

/// Whether saving or running the daily tick for this provider should trigger model sync.
pub fn is_syncable(provider: &ProviderConfig) -> bool {
    if !is_api_provider(provider) {
        return false;
    }
    let has_key = provider.api_key.as_deref().is_some_and(|k| !k.trim().is_empty());
    let is_local = provider.base_url.as_deref().is_some_and(core_registry::is_local_url);
    has_key || is_local
}

/// Spawn a background sync after `PUT /v1/providers/{id}` saves a syncable provider.
///
/// Failures are logged and stored in `providers.options.sync`; they never fail the save.
pub fn spawn_provider_sync(hub: Arc<Hub>, provider_id: String) {
    tokio::spawn(async move {
        match sync_provider(&hub, &provider_id).await {
            Ok(result) => {
                if let Some(err) = &result.error {
                    tracing::warn!(provider = %provider_id, error = %err, "background model sync failed");
                } else {
                    tracing::info!(
                        provider = %provider_id,
                        added = result.added,
                        updated = result.updated,
                        hidden = result.hidden,
                        filtered = result.filtered,
                        "background model sync complete"
                    );
                }
            }
            Err(err) => {
                tracing::debug!(provider = %provider_id, error = %err, "skipped background model sync");
            }
        }
    });
}

/// Sync all configured API providers (used by the daily periodic tick).
pub async fn sync_all_providers(hub: &Hub) -> Vec<SyncResult> {
    let providers = match hub.db.providers() {
        Ok(list) => list,
        Err(err) => {
            tracing::warn!("could not read providers for daily model sync: {err:#}");
            return Vec::new();
        }
    };
    let mut results = Vec::new();
    for provider in providers {
        if !is_syncable(&provider) {
            continue;
        }
        match sync_provider(hub, &provider.id).await {
            Ok(result) => results.push(result),
            Err(err) => {
                tracing::warn!(provider = %provider.id, "daily model sync skipped: {err}");
            }
        }
    }
    results
}

/// Sync one provider by id.
///
/// Returns `Err(SyncError::NotFound)` if no such provider exists, or
/// `Err(SyncError::NotSyncable)` for harness/scripted/via_device providers.
/// Upstream HTTP/network/empty-listing failures return `Ok(SyncResult)` with
/// `error: Some(...)`, zero counts, and no changes to the registry.
pub async fn sync_provider(hub: &Hub, provider_id: &str) -> std::result::Result<SyncResult, SyncError> {
    let provider = match hub.db.provider(provider_id) {
        Ok(Some(p)) => p,
        Ok(None) => return Err(SyncError::NotFound(format!("no provider {provider_id}"))),
        Err(err) => {
            return Ok(record_error(&hub.db, provider_id, None, format!("could not load provider: {err:#}"), 0));
        }
    };
    sync_provider_config(&hub.db, &provider).await
}

/// Sync using an already-loaded [`ProviderConfig`].
pub async fn sync_provider_config(
    db: &Db,
    provider: &ProviderConfig,
) -> std::result::Result<SyncResult, SyncError> {
    if provider.via_device().is_some() {
        return Err(SyncError::NotSyncable(format!(
            "provider {} is hosted on a device; via_device model listing is not yet supported",
            provider.id
        )));
    }
    if !matches!(provider.kind.as_str(), "openai" | "anthropic") {
        return Err(SyncError::NotSyncable(format!(
            "provider {} (kind {}) does not support API model discovery",
            provider.id, provider.kind
        )));
    }

    let preset = resolve_preset(provider);
    let discovered = discover::discover_detailed(
        preset,
        provider.base_url.as_deref(),
        provider.api_key.as_deref(),
    )
    .await;

    let synced_at = now();
    match discovered {
        Ok(res) if res.models.is_empty() => {
            let msg = if res.filtered > 0 {
                format!("provider returned no chat-capable models ({} non-chat filtered)", res.filtered)
            } else {
                "provider returned an empty model listing".to_string()
            };
            Ok(record_error(db, &provider.id, provider.api_key.as_deref(), msg, res.filtered))
        }
        Ok(res) => match apply_discovery(db, provider, &res, synced_at) {
            Ok(result) => {
                save_sync_status(db, &provider.id, &result);
                Ok(result)
            }
            Err(err) => {
                let msg = sanitize_error(&format!("{err:#}"), provider.api_key.as_deref());
                Ok(record_error(db, &provider.id, provider.api_key.as_deref(), msg, res.filtered))
            }
        },
        Err(err) => {
            let msg = sanitize_error(&format!("{err:#}"), provider.api_key.as_deref());
            Ok(record_error(db, &provider.id, provider.api_key.as_deref(), msg, 0))
        }
    }
}

fn sanitize_error(msg: &str, api_key: Option<&str>) -> String {
    match api_key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => msg.replace(key, "[redacted]"),
        None => msg.to_string(),
    }
}

fn record_error(
    db: &Db,
    provider_id: &str,
    api_key: Option<&str>,
    error: String,
    filtered: usize,
) -> SyncResult {
    let result = SyncResult {
        provider_id: provider_id.to_string(),
        added: 0,
        updated: 0,
        hidden: 0,
        filtered,
        synced_at: Some(now()),
        error: Some(sanitize_error(&error, api_key)),
        defaults: None,
    };
    save_sync_status(db, provider_id, &result);
    result
}

fn price_source_str(src: PriceSource) -> &'static str {
    match src {
        PriceSource::Catalog => "catalog",
        PriceSource::Xai => "xai",
        PriceSource::Table => "table",
        PriceSource::None => "none",
    }
}

fn tools_str(tools: ToolsSupport) -> &'static str {
    match tools {
        ToolsSupport::Yes => "yes",
        ToolsSupport::No => "no",
        ToolsSupport::Unknown => "unknown",
    }
}

fn effective_rank(d: &DiscoveredModel) -> Option<u32> {
    d.rank
        .or_else(|| discover::lookup_price(&d.id).and_then(|e| e.rank))
        .or_else(|| {
            d.id.split_once('/').and_then(|(_, tail)| {
                let base = tail.split(':').next().unwrap_or(tail);
                discover::lookup_price(base).and_then(|e| e.rank)
            })
        })
}

fn allocate_registry_id(provider_id: &str, model_name: &str, used_ids: &mut HashSet<String>) -> Option<String> {
    let base = catalog::derive_registry_id(model_name);
    if base.is_empty() {
        return None;
    }
    if used_ids.insert(base.clone()) {
        return Some(base);
    }
    let prefixed = format!("{provider_id}-{base}");
    if used_ids.insert(prefixed.clone()) {
        return Some(prefixed);
    }
    for n in 2.. {
        let candidate = format!("{prefixed}-{n}");
        if used_ids.insert(candidate.clone()) {
            return Some(candidate);
        }
    }
    None
}

/// Apply a non-empty discovery result to `db` for `provider`.
///
/// - Upserts every chat-capable discovered model with prices and metadata.
/// - Preserves `use_cases`, `roles`, `local`, `price_source == "manual"` prices, and `hidden_reason == "user"`.
/// - Unhides previously `gone` models that reappear.
/// - Increments `missed_syncs` for unseen `auto` models of this provider and hides (`gone`) after 2 misses.
/// - Picks `orchestrator` and `standard` defaults only if the registry had no visible models before this sync.
pub fn apply_discovery(
    db: &Db,
    provider: &ProviderConfig,
    result: &DiscoverResult,
    synced_at: u64,
) -> Result<SyncResult> {
    if result.models.is_empty() {
        return Ok(SyncResult {
            provider_id: provider.id.clone(),
            added: 0,
            updated: 0,
            hidden: 0,
            filtered: result.filtered,
            synced_at: Some(synced_at),
            error: Some("provider returned an empty model listing".into()),
            defaults: None,
        });
    }

    let existing_all = db.models();
    let registry_was_empty = existing_all.iter().all(|m| m.hidden);
    let preset = resolve_preset(provider);
    let is_local_provider =
        preset == Preset::Generic && provider.base_url.as_deref().is_some_and(core_registry::is_local_url);

    let mut used_ids: HashSet<String> = existing_all.iter().map(|m| m.id.clone()).collect();
    let mut existing_by_model: HashMap<String, Vec<ModelEntry>> = HashMap::new();
    for m in &existing_all {
        if m.provider_id == provider.id {
            existing_by_model.entry(m.model.clone()).or_default().push(m.clone());
        }
    }

    // Deduplicate discovered models by upstream id while preserving order.
    let mut seen_upstream: HashSet<String> = HashSet::new();
    let mut unique_discovered: Vec<&DiscoveredModel> = Vec::with_capacity(result.models.len());
    for d in &result.models {
        if !d.chat_capable || d.id.trim().is_empty() {
            continue;
        }
        if seen_upstream.insert(d.id.clone()) {
            unique_discovered.push(d);
        }
    }
    if unique_discovered.is_empty() {
        return Ok(SyncResult {
            provider_id: provider.id.clone(),
            added: 0,
            updated: 0,
            hidden: 0,
            filtered: result.filtered,
            synced_at: Some(synced_at),
            error: Some("provider returned no chat-capable models".into()),
            defaults: None,
        });
    }

    let mut added = 0usize;
    let mut updated = 0usize;
    let mut seen_registry_ids: HashSet<String> = HashSet::new();
    let mut synced_entries: Vec<(ModelEntry, Option<u32>)> = Vec::new();

    for d in unique_discovered {
        let rank = effective_rank(d);
        let free = (d.price_source == PriceSource::Catalog && d.free) || is_local_provider;

        if let Some(matches) = existing_by_model.get(&d.id) {
            for existing in matches {
                let mut entry = existing.clone();
                entry.last_seen = Some(synced_at);
                if d.display_name.is_some() {
                    entry.display_name = d.display_name.clone();
                }
                if d.context_window > 0 {
                    entry.context_window = d.context_window;
                }
                if !d.modalities.is_empty() {
                    entry.modalities = d.modalities.clone();
                }
                if d.tools != ToolsSupport::Unknown {
                    entry.tools = tools_str(d.tools).to_string();
                }
                if entry.price_source != "manual" {
                    entry.input_price = d.input_price.unwrap_or(0.0);
                    entry.output_price = d.output_price.unwrap_or(0.0);
                    entry.cache_read_price = d.cache_read_price;
                    entry.cache_write_price = d.cache_write_price;
                    entry.price_source = price_source_str(d.price_source).to_string();
                    entry.free = free;
                }
                // Unhide models that were previously hidden as `gone`, never undo `user` hides.
                if entry.hidden && entry.hidden_reason.as_deref() != Some("user") {
                    entry.hidden = false;
                    entry.hidden_reason = None;
                }

                db.upsert_model(&entry)
                    .with_context(|| format!("updating model {} for provider {}", entry.id, provider.id))?;
                if entry.hidden {
                    reset_missed_syncs(db, &entry.id);
                }
                seen_registry_ids.insert(entry.id.clone());
                if !entry.hidden {
                    synced_entries.push((entry, rank));
                }
                updated += 1;
            }
        } else {
            let Some(reg_id) = allocate_registry_id(&provider.id, &d.id, &mut used_ids) else {
                continue;
            };
            let entry = ModelEntry {
                id: reg_id.clone(),
                provider_id: provider.id.clone(),
                model: d.id.clone(),
                input_price: d.input_price.unwrap_or(0.0),
                output_price: d.output_price.unwrap_or(0.0),
                cache_read_price: d.cache_read_price,
                cache_write_price: d.cache_write_price,
                context_window: d.context_window,
                use_cases: String::new(),
                free,
                roles: Vec::new(),
                local: false,
                modalities: d.modalities.clone(),
                kind: "api".to_string(),
                device_id: None,
                can_orchestrate: false,
                source: "auto".to_string(),
                hidden: false,
                hidden_reason: None,
                last_seen: Some(synced_at),
                display_name: d.display_name.clone(),
                price_source: price_source_str(d.price_source).to_string(),
                tools: tools_str(d.tools).to_string(),
            };
            db.upsert_model(&entry)
                .with_context(|| format!("inserting model {} for provider {}", entry.id, provider.id))?;
            seen_registry_ids.insert(reg_id);
            synced_entries.push((entry, rank));
            added += 1;
        }
    }

    let hidden = mark_missing_models(db, &provider.id, &seen_registry_ids)?;
    let defaults = if registry_was_empty {
        assign_initial_defaults(db, &synced_entries)?
    } else {
        None
    };

    Ok(SyncResult {
        provider_id: provider.id.clone(),
        added,
        updated,
        hidden,
        filtered: result.filtered,
        synced_at: Some(synced_at),
        error: None,
        defaults,
    })
}

fn reset_missed_syncs(db: &Db, model_id: &str) {
    let _ = db.c().execute("UPDATE models SET missed_syncs = 0 WHERE id = ?1", [model_id]);
}

/// Increment `missed_syncs` for visible `auto` rows of `provider_id` absent from this listing,
/// hiding rows that reach [`MISSES_BEFORE_HIDE`] (`hidden = 1, hidden_reason = 'gone'`).
fn mark_missing_models(db: &Db, provider_id: &str, seen_ids: &HashSet<String>) -> Result<usize> {
    let mut c = db.c();
    let tx = c.transaction()?;
    let candidates: Vec<(String, i64, String)> = {
        let mut st = tx.prepare(
            "SELECT id, missed_syncs, roles FROM models
             WHERE provider_id = ?1 AND source = 'auto' AND hidden = 0",
        )?;
        st.query_map([provider_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut newly_hidden = 0usize;
    for (id, missed, roles) in candidates {
        if seen_ids.contains(&id) {
            continue;
        }
        let next = missed.saturating_add(1);
        if next >= MISSES_BEFORE_HIDE {
            tx.execute(
                "UPDATE models SET missed_syncs = ?1, hidden = 1, hidden_reason = 'gone' WHERE id = ?2",
                params![next, id],
            )?;
            newly_hidden += 1;
            if !roles.trim().is_empty() {
                tracing::warn!(
                    provider = %provider_id,
                    model = %id,
                    roles = %roles,
                    "default model no longer listed by provider and was hidden; pick a new default"
                );
            }
        } else {
            tx.execute("UPDATE models SET missed_syncs = ?1 WHERE id = ?2", params![next, id])?;
        }
    }
    tx.commit()?;
    Ok(newly_hidden)
}

/// Pick initial `orchestrator` and `standard` defaults on an empty registry.
/// Never moves a role that any visible model already holds.
fn assign_initial_defaults(db: &Db, candidates: &[(ModelEntry, Option<u32>)]) -> Result<Option<SyncDefaults>> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let visible = db.visible_models();
    let has_orchestrator = visible.iter().any(|m| m.roles.iter().any(|r| r == "orchestrator"));
    let has_standard = visible.iter().any(|m| m.roles.iter().any(|r| r == "standard"));

    let mut chosen = SyncDefaults::default();

    if !has_orchestrator
        && let Some(id) = pick_orchestrator_default(candidates)
    {
        add_role(db, &id, "orchestrator")?;
        chosen.orchestrator = Some(id);
    }

    if !has_standard
        && let Some(id) = pick_standard_default(candidates)
    {
        add_role(db, &id, "standard")?;
        chosen.standard = Some(id);
    }

    Ok((!chosen.is_empty()).then_some(chosen))
}

fn add_role(db: &Db, model_id: &str, role: &str) -> Result<()> {
    let Some(mut entry) = db.find_model(model_id) else {
        bail!("model {model_id} missing when assigning default role {role}");
    };
    if !entry.roles.iter().any(|r| r == role) {
        entry.roles.push(role.to_string());
        db.upsert_model(&entry)?;
    }
    Ok(())
}

/// Pick the highest-ranked tools-capable model (`tools != "no"`).
///
/// Preference order:
/// 1. `tools == "yes"` with a built-in rank (lowest `rank` number wins).
/// 2. `tools == "yes"` without a built-in rank (largest `context_window`, then id).
/// 3. `tools == "unknown"` (lowest `rank` if any, then largest `context_window`, then id).
///
/// Models with `tools == "no"` are never chosen.
fn pick_orchestrator_default(candidates: &[(ModelEntry, Option<u32>)]) -> Option<String> {
    candidates
        .iter()
        .filter(|(m, _)| !m.hidden && !m.is_harness() && m.tools != "no")
        .min_by(|(a, rank_a), (b, rank_b)| {
            let tier = |m: &ModelEntry, rank: Option<u32>| -> u8 {
                match (m.tools.as_str(), rank.is_some()) {
                    ("yes", true) => 0,
                    ("yes", false) => 1,
                    (_, true) => 2,
                    (_, false) => 3,
                }
            };
            tier(a, *rank_a)
                .cmp(&tier(b, *rank_b))
                .then_with(|| rank_a.unwrap_or(u32::MAX).cmp(&rank_b.unwrap_or(u32::MAX)))
                .then_with(|| b.context_window.cmp(&a.context_window))
                .then_with(|| a.id.cmp(&b.id))
        })
        .map(|(m, _)| m.id.clone())
}

/// Pick a free model if there is one, else the cheapest ranked model.
///
/// Falls back to the cheapest priced model (if none are ranked), then to the first
/// candidate by context window / id so an empty generic registry still gets a default.
fn pick_standard_default(candidates: &[(ModelEntry, Option<u32>)]) -> Option<String> {
    let visible: Vec<&(ModelEntry, Option<u32>)> =
        candidates.iter().filter(|(m, _)| !m.hidden && !m.is_harness()).collect();
    if visible.is_empty() {
        return None;
    }

    // 1. Prefer a free model (OpenRouter zero-priced or local provider; never price_source="none").
    if let Some((best_free, _)) = visible
        .iter()
        .copied()
        .filter(|(m, _)| m.free)
        .min_by(|(a, rank_a), (b, rank_b)| {
            rank_a
                .unwrap_or(u32::MAX)
                .cmp(&rank_b.unwrap_or(u32::MAX))
                .then_with(|| b.context_window.cmp(&a.context_window))
                .then_with(|| a.id.cmp(&b.id))
        })
    {
        return Some(best_free.id.clone());
    }

    let combined_price = |m: &ModelEntry| m.input_price + m.output_price;

    // 2. Otherwise the cheapest ranked model with known prices.
    if let Some((best_ranked, _)) = visible
        .iter()
        .copied()
        .filter(|(m, rank)| rank.is_some() && m.price_source != "none")
        .min_by(|(a, rank_a), (b, rank_b)| {
            combined_price(a)
                .total_cmp(&combined_price(b))
                .then_with(|| rank_a.unwrap_or(u32::MAX).cmp(&rank_b.unwrap_or(u32::MAX)))
                .then_with(|| a.id.cmp(&b.id))
        })
    {
        return Some(best_ranked.id.clone());
    }

    // 3. Otherwise cheapest priced model without rank.
    if let Some((best_priced, _)) = visible
        .iter()
        .copied()
        .filter(|(m, _)| m.price_source != "none")
        .min_by(|(a, _), (b, _)| combined_price(a).total_cmp(&combined_price(b)).then_with(|| a.id.cmp(&b.id)))
    {
        return Some(best_priced.id.clone());
    }

    // 4. Unpriced fallback (e.g. remote generic provider with unknown model ids).
    visible
        .into_iter()
        .min_by(|(a, _), (b, _)| b.context_window.cmp(&a.context_window).then_with(|| a.id.cmp(&b.id)))
        .map(|(m, _)| m.id.clone())
}

/// Store the latest sync summary on `providers.options["sync"]` without touching `api_key`.
fn save_sync_status(db: &Db, provider_id: &str, result: &SyncResult) {
    let c = db.c();
    let existing: Option<String> = c
        .query_row("SELECT options FROM providers WHERE id = ?1", [provider_id], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    let Some(raw) = existing else { return };
    let mut obj = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    if let Ok(val) = serde_json::to_value(result) {
        obj.insert("sync".into(), val);
    }
    let updated = Value::Object(obj).to_string();
    let _ = c.execute("UPDATE providers SET options = ?1 WHERE id = ?2", params![updated, provider_id]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;
    use tokio::net::TcpListener;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-sync-test-{}", crate::db::random_hex(4)))
    }

    /// Scripted fake listing server: each GET request pops the next `(StatusCode, Value)` response.
    async fn spawn_scripted_server(
        path: &'static str,
        responses: Vec<(StatusCode, Value)>,
    ) -> (String, Arc<Mutex<VecDeque<(StatusCode, Value)>>>) {
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let q = queue.clone();
        let router = Router::new().route(
            path,
            get(move || {
                let q = q.clone();
                async move {
                    let next = q
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or((StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "script exhausted"})));
                    (next.0, axum::Json(next.1)).into_response()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{addr}"), queue)
    }

    fn openai_listing(ids: &[&str]) -> Value {
        json!({
            "data": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn first_sync_adds_models_filters_non_chat_and_sets_initial_defaults() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (base, _) = spawn_scripted_server(
            "/v1/models",
            vec![
                (
                    StatusCode::OK,
                    openai_listing(&[
                        "gpt-5.4",
                        "gpt-5-mini",
                        "o1-mini",
                        "custom-unpriced-chat",
                        "text-embedding-3-small",
                    ]),
                ),
                (
                    StatusCode::OK,
                    openai_listing(&[
                        "gpt-5.4",
                        "gpt-5-mini",
                        "o1-mini",
                        "custom-unpriced-chat",
                        "text-embedding-3-small",
                    ]),
                ),
            ],
        )
        .await;

        let provider = ProviderConfig {
            id: "openai".into(),
            kind: "openai".into(),
            base_url: Some(format!("{base}/v1")),
            api_key: Some("sk-secret-test".into()),
            options: json!({"preset": "openai"}),
        };
        db.upsert_provider(&provider).unwrap();

        let r1 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r1.error, None);
        assert_eq!(r1.added, 4);
        assert_eq!(r1.updated, 0);
        assert_eq!(r1.hidden, 0);
        assert_eq!(r1.filtered, 1);
        assert_eq!(
            r1.defaults,
            Some(SyncDefaults {
                orchestrator: Some("gpt-5.4".into()),
                standard: Some("gpt-5-mini".into()),
            })
        );

        let gpt54 = db.find_model("gpt-5.4").unwrap();
        assert_eq!(gpt54.source, "auto");
        assert_eq!(gpt54.price_source, "table");
        assert_eq!(gpt54.tools, "yes");
        assert!((gpt54.input_price - 2.5).abs() < 1e-9);
        assert!(!gpt54.free);
        assert_eq!(gpt54.roles, vec!["orchestrator"]);
        assert!(gpt54.last_seen.is_some());

        let mini = db.find_model("gpt-5-mini").unwrap();
        assert_eq!(mini.roles, vec!["standard"]);

        let o1 = db.find_model("o1-mini").unwrap();
        assert_eq!(o1.tools, "no");

        let unpriced = db.find_model("custom-unpriced-chat").unwrap();
        assert_eq!(unpriced.price_source, "none");
        assert_eq!(unpriced.input_price, 0.0);
        assert_eq!(unpriced.output_price, 0.0);
        assert!(!unpriced.free, "unpriced rows must never be marked free");

        assert!(db.find_model("text-embedding-3-small").is_none());

        // Sync result is persisted on provider.options.sync without touching api_key.
        let saved_provider = db.provider("openai").unwrap().unwrap();
        assert_eq!(saved_provider.api_key.as_deref(), Some("sk-secret-test"));
        assert_eq!(saved_provider.options["sync"]["added"], 4);
        assert_eq!(saved_provider.options["sync"]["filtered"], 1);

        // Second identical sync updates all 4 and does not reassign defaults.
        let r2 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r2.error, None);
        assert_eq!(r2.added, 0);
        assert_eq!(r2.updated, 4);
        assert_eq!(r2.hidden, 0);
        assert_eq!(r2.filtered, 1);
        assert_eq!(r2.defaults, None);

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn openrouter_empty_registry_picks_free_for_standard_and_ranked_tools_for_orchestrator() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let body = json!({
            "data": [
                {
                    "id": "anthropic/claude-opus-5",
                    "name": "Claude Opus 5",
                    "context_length": 200000,
                    "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
                    "supported_parameters": ["tools", "temperature"],
                    "pricing": {"prompt": "0.000004", "completion": "0.00002"}
                },
                {
                    "id": "meta-llama/llama-3.2-3b:free",
                    "name": "Llama 3.2 3B Free",
                    "context_length": 131072,
                    "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                    "supported_parameters": ["temperature"],
                    "pricing": {"prompt": "0", "completion": "0"}
                },
                {
                    "id": "stability/sdxl",
                    "name": "SDXL",
                    "context_length": 4096,
                    "architecture": {"input_modalities": ["text"], "output_modalities": ["image"]},
                    "pricing": {"prompt": "0", "completion": "0"}
                }
            ]
        });
        let (base, _) = spawn_scripted_server("/api/v1/models/user", vec![(StatusCode::OK, body)]).await;
        let provider = ProviderConfig {
            id: "openrouter".into(),
            kind: "openai".into(),
            base_url: Some(format!("{base}/api/v1")),
            api_key: Some("sk-or-test".into()),
            options: json!({"preset": "openrouter"}),
        };
        db.upsert_provider(&provider).unwrap();

        let res = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(res.error, None);
        assert_eq!(res.added, 2);
        assert_eq!(res.filtered, 1);
        assert_eq!(
            res.defaults,
            Some(SyncDefaults {
                orchestrator: Some("anthropic-claude-opus-5".into()),
                standard: Some("meta-llama-llama-3.2-3b-free".into()),
            })
        );
        let free_row = db.find_model("meta-llama-llama-3.2-3b-free").unwrap();
        assert!(free_row.free);
        assert_eq!(free_row.tools, "no");

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn failed_or_empty_listing_changes_nothing() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (base, _) = spawn_scripted_server(
            "/v1/models",
            vec![
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini"])),
                (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "upstream down"})),
                (StatusCode::OK, openai_listing(&[])),
                // Next real listing misses gpt-5-mini once: must still be visible (1 miss, not 2+).
                (StatusCode::OK, openai_listing(&["gpt-5.4"])),
            ],
        )
        .await;

        let provider = ProviderConfig {
            id: "openai".into(),
            kind: "openai".into(),
            base_url: Some(format!("{base}/v1")),
            api_key: Some("sk-secret-key-123".into()),
            options: json!({"preset": "openai"}),
        };
        db.upsert_provider(&provider).unwrap();

        let r1 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r1.added, 2);
        let snapshot_after_1 = db.models();

        // HTTP 500 failure: no model changed, no secret leaked.
        let r2 = sync_provider_config(&db, &provider).await.unwrap();
        assert!(r2.error.is_some());
        assert!(!r2.error.as_deref().unwrap().contains("sk-secret-key-123"));
        assert_eq!((r2.added, r2.updated, r2.hidden), (0, 0, 0));
        assert_eq!(db.models(), snapshot_after_1);

        // Empty listing: no model changed.
        let r3 = sync_provider_config(&db, &provider).await.unwrap();
        assert!(r3.error.is_some());
        assert_eq!((r3.added, r3.updated, r3.hidden), (0, 0, 0));
        assert_eq!(db.models(), snapshot_after_1);

        // First real miss after outages: still visible because failures did not increment missed_syncs.
        let r4 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r4.error, None);
        assert_eq!(r4.hidden, 0);
        assert!(!db.find_model("gpt-5-mini").unwrap().hidden);

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn hides_after_two_successive_misses_never_deletes_and_preserves_manual_edits() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (base, _) = spawn_scripted_server(
            "/v1/models",
            vec![
                // 1: initial sync
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini"])),
                // 2: miss #1 for gpt-5-mini
                (StatusCode::OK, openai_listing(&["gpt-5.4"])),
                // 3: gpt-5-mini reappears -> resets consecutive miss counter
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini"])),
                // 4: miss #1 again
                (StatusCode::OK, openai_listing(&["gpt-5.4"])),
                // 5: miss #2 -> hidden (gone), never deleted
                (StatusCode::OK, openai_listing(&["gpt-5.4"])),
                // 6: miss #3 -> already hidden, not double-counted
                (StatusCode::OK, openai_listing(&["gpt-5.4"])),
                // 7: gpt-5-mini returns -> unhidden
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini"])),
            ],
        )
        .await;

        let provider = ProviderConfig {
            id: "openai".into(),
            kind: "openai".into(),
            base_url: Some(format!("{base}/v1")),
            api_key: Some("k".into()),
            options: json!({"preset": "openai"}),
        };
        db.upsert_provider(&provider).unwrap();

        assert_eq!(sync_provider_config(&db, &provider).await.unwrap().added, 2);

        // Add a manual model not in the listing (must never be auto-hidden).
        db.upsert_model(&ModelEntry {
            id: "my-manual".into(),
            provider_id: "openai".into(),
            model: "gpt-3.5-turbo-instruct".into(),
            input_price: 1.5,
            output_price: 2.0,
            source: "manual".into(),
            price_source: "manual".into(),
            ..Default::default()
        })
        .unwrap();

        // Customize gpt-5.4 with manual price, use_cases, local flag, and user hide.
        let mut custom = db.find_model("gpt-5.4").unwrap();
        custom.input_price = 9.99;
        custom.price_source = "manual".into();
        custom.use_cases = "Owner curated use cases".into();
        custom.local = true;
        custom.hidden = true;
        custom.hidden_reason = Some("user".into());
        db.upsert_model(&custom).unwrap();

        // Sync 2: miss #1 for gpt-5-mini -> still visible.
        let r2 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r2.hidden, 0);
        assert!(!db.find_model("gpt-5-mini").unwrap().hidden);
        // Manual price, use_cases, local, and user hide are preserved on gpt-5.4.
        let after2 = db.find_model("gpt-5.4").unwrap();
        assert!((after2.input_price - 9.99).abs() < 1e-9);
        assert_eq!(after2.price_source, "manual");
        assert_eq!(after2.use_cases, "Owner curated use cases");
        assert!(after2.local);
        assert!(after2.hidden);
        assert_eq!(after2.hidden_reason.as_deref(), Some("user"));

        // Sync 3: reappears -> resets miss counter.
        assert_eq!(sync_provider_config(&db, &provider).await.unwrap().hidden, 0);

        // Sync 4: miss #1 again -> still visible (proving non-consecutive misses reset).
        let r4 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r4.hidden, 0);
        assert!(!db.find_model("gpt-5-mini").unwrap().hidden);

        // Sync 5: miss #2 -> hidden as `gone`, row still exists in db.models().
        let r5 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r5.hidden, 1);
        let gone = db.find_model("gpt-5-mini").unwrap();
        assert!(gone.hidden);
        assert_eq!(gone.hidden_reason.as_deref(), Some("gone"));
        assert!(!db.visible_models().iter().any(|m| m.id == "gpt-5-mini"));
        // Manual row is still visible.
        assert!(!db.find_model("my-manual").unwrap().hidden);

        // Sync 6: miss #3 -> already hidden, not double-counted.
        assert_eq!(sync_provider_config(&db, &provider).await.unwrap().hidden, 0);

        // Sync 7: reappears -> unhidden.
        let r7 = sync_provider_config(&db, &provider).await.unwrap();
        assert_eq!(r7.hidden, 0);
        let back = db.find_model("gpt-5-mini").unwrap();
        assert!(!back.hidden);
        assert_eq!(back.hidden_reason, None);

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn sync_never_moves_existing_defaults_and_prefixes_colliding_ids() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (base1, _) = spawn_scripted_server("/v1/models", vec![(StatusCode::OK, openai_listing(&["gpt-5.4"]))]).await;
        let (base2, _) = spawn_scripted_server("/v1/models", vec![(StatusCode::OK, openai_listing(&["gpt-5.4"]))]).await;

        db.upsert_provider(&ProviderConfig {
            id: "seed".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({}),
        })
        .unwrap();
        db.upsert_model(&ModelEntry {
            id: "existing-default".into(),
            provider_id: "seed".into(),
            model: "seed-1".into(),
            roles: vec!["orchestrator".into(), "standard".into()],
            ..Default::default()
        })
        .unwrap();

        let p1 = ProviderConfig {
            id: "openai".into(),
            kind: "openai".into(),
            base_url: Some(format!("{base1}/v1")),
            api_key: Some("k1".into()),
            options: json!({"preset": "openai"}),
        };
        let p2 = ProviderConfig {
            id: "openai-alt".into(),
            kind: "openai".into(),
            base_url: Some(format!("{base2}/v1")),
            api_key: Some("k2".into()),
            options: json!({"preset": "openai"}),
        };
        db.upsert_provider(&p1).unwrap();
        db.upsert_provider(&p2).unwrap();

        let r1 = sync_provider_config(&db, &p1).await.unwrap();
        assert_eq!(r1.added, 1);
        assert_eq!(r1.defaults, None, "existing defaults must never be moved");
        assert_eq!(db.default_model("orchestrator").unwrap().id, "existing-default");
        assert_eq!(db.default_model("standard").unwrap().id, "existing-default");
        assert!(db.find_model("gpt-5.4").unwrap().roles.is_empty());

        let r2 = sync_provider_config(&db, &p2).await.unwrap();
        assert_eq!(r2.added, 1);
        assert_eq!(r2.defaults, None);
        let collided = db.find_model("openai-alt-gpt-5.4").unwrap();
        assert_eq!(collided.provider_id, "openai-alt");
        assert_eq!(collided.model, "gpt-5.4");
        assert_eq!(db.find_model("gpt-5.4").unwrap().provider_id, "openai");

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn put_provider_triggers_background_sync_and_sync_endpoint_enforces_admin_auth() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (_, admin_token) = db.add_device("Admin", "macos", true).unwrap();
        let (pc, plain_token) = db.add_device("Gaming PC", "windows", false).unwrap();

        let (upstream, _) = spawn_scripted_server(
            "/v1/models",
            vec![
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini", "dall-e-3"])),
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini", "gpt-5-nano", "dall-e-3"])),
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini", "gpt-5-nano", "dall-e-3"])),
                (StatusCode::OK, openai_listing(&["gpt-5.4", "gpt-5-mini", "gpt-5-nano", "dall-e-3"])),
            ],
        )
        .await;

        let hub = Hub::new(db, dir.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::router(hub.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");
        let http = reqwest::Client::new();

        // Save provider via PUT /v1/providers/openai -> background sync registers models.
        let put_resp = http
            .put(format!("{base}/v1/providers/openai"))
            .bearer_auth(&admin_token)
            .json(&json!({
                "id": "openai",
                "kind": "openai",
                "base_url": format!("{upstream}/v1"),
                "api_key": "sk-test",
                "options": {"preset": "openai"}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(put_resp.status(), StatusCode::OK);

        // Wait for the background sync task to complete.
        for _ in 0..100 {
            if hub.db.visible_models().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(hub.db.visible_models().len(), 2);
        assert_eq!(hub.db.default_model("orchestrator").unwrap().id, "gpt-5.4");
        assert_eq!(hub.db.default_model("standard").unwrap().id, "gpt-5-mini");

        // Auth checks on POST /v1/providers/{id}/sync:
        let no_auth = http.post(format!("{base}/v1/providers/openai/sync")).send().await.unwrap();
        assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);

        let non_admin = http
            .post(format!("{base}/v1/providers/openai/sync"))
            .bearer_auth(&plain_token)
            .send()
            .await
            .unwrap();
        assert_eq!(non_admin.status(), StatusCode::FORBIDDEN);

        // Missing provider -> 404.
        let missing = http
            .post(format!("{base}/v1/providers/does-not-exist/sync"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        // Harness provider -> 400 (not an API listing provider).
        let harness_id = format!("claude-code@{}", pc.id);
        hub.db
            .upsert_provider(&ProviderConfig {
                id: harness_id.clone(),
                kind: "harness".into(),
                base_url: None,
                api_key: None,
                options: json!({"harness": "claude-code", "via_device": "Gaming PC"}),
            })
            .unwrap();
        let not_syncable = http
            .post(format!("{base}/v1/providers/{harness_id}/sync"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap();
        assert_eq!(not_syncable.status(), StatusCode::BAD_REQUEST);

        // Admin on-demand sync -> 200 with counts and adds gpt-5-nano without moving defaults.
        let sync_resp = http
            .post(format!("{base}/v1/providers/openai/sync"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap();
        assert_eq!(sync_resp.status(), StatusCode::OK);
        let result: SyncResult = sync_resp.json().await.unwrap();
        assert_eq!(result.provider_id, "openai");
        assert_eq!(result.added, 1);
        assert_eq!(result.updated, 2);
        assert_eq!(result.hidden, 0);
        assert_eq!(result.filtered, 1);
        assert_eq!(result.error, None);
        assert_eq!(result.defaults, None);
        assert_eq!(hub.db.default_model("orchestrator").unwrap().id, "gpt-5.4");
        assert_eq!(hub.db.default_model("standard").unwrap().id, "gpt-5-mini");

        // Alias route POST /v1/providers/{id}/sync-models works identically.
        let alias_resp = http
            .post(format!("{base}/v1/providers/openai/sync-models"))
            .bearer_auth(&admin_token)
            .send()
            .await
            .unwrap();
        assert_eq!(alias_resp.status(), StatusCode::OK);

        // Daily tick helper sync_all_providers syncs API providers and skips harness providers.
        let all = sync_all_providers(&hub).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].provider_id, "openai");
        assert_eq!(all[0].error, None);

        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
