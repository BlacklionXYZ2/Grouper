//! Import a provider's model card into the registry.
//!
//! OpenRouter publishes prices, context length and input modalities at
//! `GET https://openrouter.ai/api/v1/models`. Prices on that card are per token;
//! the registry stores them per million tokens. The card's description is drafted
//! into a use-case proposal (`source = catalog`) for you to accept. Other providers
//! have no catalog, so importing one does not call out.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use core_protocol::{ModelEntry, ModelImportRequest, ModelImportResult, ProviderConfig};
use serde_json::Value;

use crate::db::Db;
use crate::registry::deny_unless_admin;
use crate::server::{Hub, err};

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const DRAFT_MAX_CHARS: usize = 200;
const CATALOG_REASON: &str = "Drafted from the OpenRouter model card.";

/// One model from a provider catalog, already in registry units.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogModel {
    pub id: String,
    /// USD per million input tokens. `None` when missing or invalid on the card.
    pub input_price: Option<f64>,
    /// USD per million output tokens. `None` when missing or invalid on the card.
    pub output_price: Option<f64>,
    pub cache_read_price: Option<f64>,
    pub cache_write_price: Option<f64>,
    /// Why pricing on the card could not be parsed, if any.
    pub price_error: Option<String>,
    pub context_window: u64,
    pub free: bool,
    pub modalities: Vec<String>,
    /// Use-case text to propose, not written onto the model until you accept it.
    pub use_cases_draft: String,
}

impl CatalogModel {
    fn validated_prices(&self) -> Result<(f64, f64)> {
        if let Some(err) = &self.price_error {
            bail!("model {:?} has invalid pricing: {err}", self.id);
        }
        let Some(input) = self.input_price else {
            bail!("model {:?} is missing prompt price", self.id);
        };
        let Some(output) = self.output_price else {
            bail!("model {:?} is missing completion price", self.id);
        };
        if !input.is_finite() || input < 0.0 {
            bail!("model {:?} has invalid prompt price {input}", self.id);
        }
        if !output.is_finite() || output < 0.0 {
            bail!("model {:?} has invalid completion price {output}", self.id);
        }
        if let Some(p) = self.cache_read_price
            && (!p.is_finite() || p < 0.0)
        {
            bail!("model {:?} has invalid cache read price {p}", self.id);
        }
        if let Some(p) = self.cache_write_price
            && (!p.is_finite() || p < 0.0)
        {
            bail!("model {:?} has invalid cache write price {p}", self.id);
        }
        Ok((input, output))
    }
}

#[derive(Debug)]
pub struct PriceRefresh {
    pub updated: usize,
    pub failed: bool,
}

/// Whether this provider's base URL is OpenRouter. Anthropic and everyone else are not.
pub fn is_openrouter(provider: &ProviderConfig) -> bool {
    provider.base_url.as_deref().is_some_and(is_openrouter_url)
}

fn is_openrouter_url(url: &str) -> bool {
    let Some(host) = reqwest::Url::parse(url).ok().and_then(|url| url.host_str().map(str::to_ascii_lowercase)) else {
        return false;
    };
    host == "openrouter.ai" || host.ends_with(".openrouter.ai")
}

/// Registry id derived from a provider model name: `meta-llama/llama-3.2-3b:free`
/// becomes `meta-llama-llama-3.2-3b-free`.
pub fn derive_registry_id(model: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in model.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// First sentence or two of a model card, capped at about 200 characters.
pub fn draft_use_cases(description: &str) -> String {
    let text = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return String::new();
    }
    let mut sentences = 0;
    let mut end = text.len();
    for (i, c) in text.char_indices() {
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let rest = &text[i + c.len_utf8()..];
        let prev_digit = text[..i].chars().next_back().is_some_and(|ch| ch.is_ascii_digit());
        let next_digit = rest.chars().next().is_some_and(|ch| ch.is_ascii_digit());
        let boundary = rest.is_empty() || rest.starts_with(char::is_whitespace);
        if boundary && !(c == '.' && prev_digit && next_digit) {
            sentences += 1;
            if sentences == 2 {
                end = i + c.len_utf8();
                break;
            }
        }
    }
    cap_chars(text[..end].trim(), DRAFT_MAX_CHARS)
}

fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    if let Some(space) = out.rfind(' ')
        && space >= max / 2
    {
        out.truncate(space);
    }
    out = out.trim_end().to_string();
    out.push('…');
    out
}

/// USD per million tokens from an OpenRouter per-token price (`"0.000003"` → `3.0`).
/// Rejects empty, unparseable, negative, or non-finite values.
fn per_million_tokens(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() || s.starts_with('-') {
        return None;
    }
    let scaled = decimal_per_million(s)
        .or_else(|| s.parse::<f64>().ok().filter(|n| n.is_finite()).map(|n| n * 1_000_000.0))?;
    (scaled.is_finite() && scaled >= 0.0).then_some(scaled)
}

fn decimal_per_million(s: &str) -> Option<f64> {
    if s.starts_with('-') {
        return None;
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.is_empty() || s.contains(['e', 'E']) {
        return None;
    }
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if (whole.is_empty() && frac.is_empty())
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
        || whole.len() > 18
        || frac.len() > 18
    {
        return None;
    }
    let whole: u128 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let frac_val: u128 = if frac.is_empty() { 0 } else { frac.parse().ok()? };
    let scale = 10u128.checked_pow(frac.len() as u32)?;
    let million = 1_000_000u128;
    let num = whole.checked_mul(million)?.checked_mul(scale)?.checked_add(frac_val.checked_mul(million)?)?;
    let value = (num as f64) / (scale as f64);
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn parse_price_field(pricing: &Value, key: &str, required: bool) -> Result<Option<f64>, String> {
    let Some(value) = pricing.get(key) else {
        return if required { Err(format!("missing {key} price")) } else { Ok(None) };
    };
    if value.is_null() {
        return if required { Err(format!("missing {key} price")) } else { Ok(None) };
    }
    if let Some(text) = value.as_str() {
        return per_million_tokens(text).map(Some).ok_or_else(|| format!("invalid {key} price {text:?}"));
    }
    if let Some(n) = value.as_f64() {
        let scaled = n * 1_000_000.0;
        if scaled.is_finite() && scaled >= 0.0 {
            return Ok(Some(scaled));
        }
    }
    Err(format!("invalid {key} price {value}"))
}

fn context_length(row: &Value) -> u64 {
    let Some(value) = row.get("context_length") else { return 0 };
    value
        .as_u64()
        .or_else(|| value.as_i64().map(|n| n.max(0) as u64))
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn modalities_of(row: &Value) -> Vec<String> {
    row.get("architecture")
        .and_then(|arch| arch.get("input_modalities"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_one(row: &Value) -> Option<CatalogModel> {
    let id = row.get("id").and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty())?;
    let pricing = row.get("pricing").cloned().unwrap_or(Value::Null);
    let prompt = parse_price_field(&pricing, "prompt", true);
    let completion = parse_price_field(&pricing, "completion", true);
    let cache_read = parse_price_field(&pricing, "input_cache_read", false);
    let cache_write = parse_price_field(&pricing, "input_cache_write", false);
    let price_error = prompt
        .as_ref()
        .err()
        .or_else(|| completion.as_ref().err())
        .or_else(|| cache_read.as_ref().err())
        .or_else(|| cache_write.as_ref().err())
        .cloned();
    let input_price = prompt.ok().flatten();
    let output_price = completion.ok().flatten();
    let free = id.ends_with(":free")
        || (price_error.is_none()
            && matches!((input_price, output_price), (Some(i), Some(o)) if i == 0.0 && o == 0.0));
    let description = row.get("description").and_then(Value::as_str).unwrap_or("");
    Some(CatalogModel {
        id: id.to_string(),
        input_price,
        output_price,
        cache_read_price: cache_read.ok().flatten(),
        cache_write_price: cache_write.ok().flatten(),
        price_error,
        context_window: context_length(row),
        free,
        modalities: modalities_of(row),
        use_cases_draft: draft_use_cases(description),
    })
}

/// Parse an OpenRouter `GET /api/v1/models` body. Rows without an id are skipped.
pub fn parse_openrouter_models(body: &Value) -> Vec<CatalogModel> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse_one).collect())
        .unwrap_or_default()
}

fn find_card<'a>(cards: &'a [CatalogModel], model: &str) -> Option<&'a CatalogModel> {
    cards.iter().find(|card| card.id == model).or_else(|| cards.iter().find(|card| card.id.eq_ignore_ascii_case(model)))
}

fn registry_id(req: &ModelImportRequest, card: &CatalogModel) -> Result<String> {
    match req.id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => {
            if id.contains('/') || id.contains(char::is_whitespace) {
                bail!("registry id cannot contain '/' or whitespace");
            }
            Ok(id.to_string())
        }
        None => {
            let id = derive_registry_id(&card.id);
            if id.is_empty() {
                bail!("could not derive a registry id from {:?}; pass id", card.id);
            }
            Ok(id)
        }
    }
}

fn draft_proposal(db: &Db, model_id: &str, draft: &str) -> Option<i64> {
    let draft = draft.trim();
    if draft.is_empty() {
        return None;
    }
    if db.find_model(model_id).is_some_and(|model| model.use_cases.trim() == draft) {
        return None;
    }
    if let Some(pending) = db.proposals(Some("pending")).into_iter().find(|proposal| proposal.model_id == model_id) {
        return Some(pending.id);
    }
    match db.propose_use_cases(model_id, draft, CATALOG_REASON, "catalog") {
        Ok(proposal) => Some(proposal.id),
        Err(error) => {
            tracing::warn!("use-case draft for {model_id}: {error:#}");
            None
        }
    }
}

/// Write a catalog card into the registry. Use cases stay empty until the drafted proposal is accepted.
pub fn import_from_catalog(
    db: &Db,
    provider_id: &str,
    cards: &[CatalogModel],
    req: &ModelImportRequest,
) -> Result<ModelImportResult> {
    let wanted = req.model.trim();
    if wanted.is_empty() {
        bail!("model name is empty");
    }
    let Some(card) = find_card(cards, wanted) else { bail!("OpenRouter has no model {wanted:?}") };
    let (input_price, output_price) = card.validated_prices()?;
    let id = registry_id(req, card)?;
    let existing = db.models().into_iter().find(|model| model.id == id);
    let roles = match &req.roles {
        Some(roles) => roles.iter().map(|role| role.trim()).filter(|role| !role.is_empty()).map(String::from).collect(),
        None => existing.as_ref().map(|model| model.roles.clone()).unwrap_or_default(),
    };
    let local = req.local.unwrap_or_else(|| existing.as_ref().is_some_and(|model| model.local));
    let free = card.id.ends_with(":free") || (input_price == 0.0 && output_price == 0.0);
    let entry = ModelEntry {
        id: id.clone(),
        provider_id: provider_id.to_string(),
        model: card.id.clone(),
        input_price,
        output_price,
        cache_read_price: card.cache_read_price,
        cache_write_price: card.cache_write_price,
        context_window: card.context_window,
        use_cases: existing.as_ref().map(|model| model.use_cases.clone()).unwrap_or_default(),
        free,
        roles,
        local,
        modalities: card.modalities.clone(),
        ..Default::default()
    };
    db.upsert_model(&entry)?;
    let proposal_id = draft_proposal(db, &id, &card.use_cases_draft);
    let model = db.models().into_iter().find(|model| model.id == id).context("saved model is missing")?;
    Ok(ModelImportResult { model, proposal_id, probe: None, probe_error: None })
}

/// Replace prices (and the free flag, which is derived from them) from a catalog card.
pub fn update_prices(db: &Db, model: &ModelEntry, card: &CatalogModel) -> Result<ModelEntry> {
    let (input_price, output_price) = card.validated_prices()?;
    let mut updated = model.clone();
    updated.input_price = input_price;
    updated.output_price = output_price;
    updated.cache_read_price = card.cache_read_price;
    updated.cache_write_price = card.cache_write_price;
    updated.free = card.id.ends_with(":free") || (input_price == 0.0 && output_price == 0.0);
    db.upsert_model(&updated)?;
    db.models().into_iter().find(|saved| saved.id == model.id).context("saved model is missing")
}

fn require_openrouter(provider: &ProviderConfig) -> Result<()> {
    if is_openrouter(provider) {
        Ok(())
    } else {
        bail!(
            "provider {} has no model catalog (only OpenRouter publishes one); prices and modalities stay as you set them",
            provider.id
        )
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().context("HTTP client")
}

async fn fetch_openrouter_models(client: &reqwest::Client, provider: &ProviderConfig) -> Result<Value> {
    let mut request = client.get(OPENROUTER_MODELS_URL);
    if let Some(key) = provider.api_key.as_deref().filter(|key| !key.is_empty()) {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.context("fetching the OpenRouter model catalog")?;
    let status = response.status();
    if !status.is_success() {
        bail!("OpenRouter /models returned HTTP {status}");
    }
    response.json().await.context("OpenRouter /models returned invalid JSON")
}

/// Import one model from parsed catalog cards and, only when `req.probe` is true,
/// run the built-in probe suite against it afterward.
pub async fn import_with_cards(
    hub: &Arc<Hub>,
    provider_id: &str,
    cards: &[CatalogModel],
    req: &ModelImportRequest,
) -> Result<ModelImportResult> {
    let mut result = import_from_catalog(&hub.db, provider_id, cards, req)?;
    if req.probe {
        match crate::probes::run_probes(hub, &result.model.id).await {
            Ok(report) => {
                if let Some(id) = report.proposal_id {
                    result.proposal_id = Some(id);
                }
                result.probe = Some(report);
            }
            Err(error) => {
                result.probe_error = Some(error.to_string());
            }
        }
    }
    Ok(result)
}

/// Fetch the catalog for an OpenRouter provider and save one model. Other providers error before any request.
pub async fn import(hub: &Arc<Hub>, req: ModelImportRequest) -> Result<ModelImportResult> {
    let Some(provider) = hub.db.provider(&req.provider)? else { bail!("no provider {}", req.provider) };
    require_openrouter(&provider)?;
    let body = fetch_openrouter_models(&http_client()?, &provider).await?;
    import_with_cards(hub, &provider.id, &parse_openrouter_models(&body), &req).await
}

/// Re-pull prices for one registered model. The provider must be OpenRouter.
pub async fn refresh_model_prices(hub: &Hub, id: &str) -> Result<ModelEntry> {
    let Some(model) = hub.db.models().into_iter().find(|model| model.id == id) else { bail!("no model {id}") };
    let Some(provider) = hub.db.provider(&model.provider_id)? else { bail!("no provider {}", model.provider_id) };
    require_openrouter(&provider)?;
    let body = fetch_openrouter_models(&http_client()?, &provider).await?;
    let cards = parse_openrouter_models(&body);
    let Some(card) = find_card(&cards, &model.model) else { bail!("OpenRouter has no model {:?}", model.model) };
    update_prices(&hub.db, &model, card)
}

/// Refresh prices for every OpenRouter model. One provider's failure does not roll back the others.
pub async fn refresh_openrouter_prices(hub: &Hub) -> PriceRefresh {
    let providers = match hub.db.providers() {
        Ok(providers) => providers,
        Err(error) => {
            tracing::warn!("could not read providers for model price refresh: {error:#}");
            return PriceRefresh { updated: 0, failed: true };
        }
    };
    let models = hub.db.models();
    let targets: Vec<ProviderConfig> = providers
        .into_iter()
        .filter(|provider| is_openrouter(provider) && models.iter().any(|model| model.provider_id == provider.id))
        .collect();
    if targets.is_empty() {
        return PriceRefresh { updated: 0, failed: false };
    }
    let client = match http_client() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!("model price refresh: {error:#}");
            return PriceRefresh { updated: 0, failed: true };
        }
    };
    let mut result = PriceRefresh { updated: 0, failed: false };
    for provider in targets {
        match refresh_provider(&hub.db, &client, &provider, &models).await {
            Ok(count) => result.updated += count,
            Err(error) => {
                tracing::warn!(provider = %provider.id, "model price refresh failed: {error:#}");
                result.failed = true;
            }
        }
    }
    result
}

async fn refresh_provider(db: &Db, client: &reqwest::Client, provider: &ProviderConfig, models: &[ModelEntry]) -> Result<usize> {
    let cards = parse_openrouter_models(&fetch_openrouter_models(client, provider).await?);
    Ok(apply_price_refresh(db, &provider.id, &cards, models))
}

fn apply_price_refresh(db: &Db, provider_id: &str, cards: &[CatalogModel], models: &[ModelEntry]) -> usize {
    let mut updated = 0;
    for model in models.iter().filter(|model| model.provider_id == provider_id) {
        let Some(card) = find_card(cards, &model.model) else {
            tracing::warn!(model = %model.id, "not in the OpenRouter catalog; prices left as they are");
            continue;
        };
        match update_prices(db, model, card) {
            Ok(_) => updated += 1,
            Err(error) => tracing::warn!(model = %model.id, "could not save refreshed prices; keeping existing: {error:#}"),
        }
    }
    updated
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new()
        .route("/v1/models/import", post(import_model))
        .route("/v1/models/{id}/refresh-prices", post(refresh_prices))
}

async fn import_model(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(req): Json<ModelImportRequest>,
) -> Response {
    if let Some(response) = deny_unless_admin(&hub, &headers) {
        return response;
    }
    match import(&hub, req).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => err(StatusCode::BAD_REQUEST, error),
    }
}

async fn refresh_prices(State(hub): State<Arc<Hub>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(response) = deny_unless_admin(&hub, &headers) {
        return response;
    }
    match refresh_model_prices(&hub, &id).await {
        Ok(model) => Json(model).into_response(),
        Err(error) => {
            let status = if error.to_string().starts_with("no model ") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
            err(status, error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OPENROUTER_MODELS: &str = r#"{
      "data": [
        {
          "id": "anthropic/claude-sonnet-4",
          "description": "Claude Sonnet 4 is a fast model for everyday coding. It reads long documents and images. A third sentence should be dropped.",
          "context_length": 200000,
          "architecture": {
            "modality": "text+image->text",
            "input_modalities": ["text", "image", ""],
            "output_modalities": ["text"]
          },
          "pricing": {
            "prompt": "0.000003",
            "completion": "0.000015",
            "input_cache_read": "0.0000003",
            "input_cache_write": "0.00000375"
          }
        },
        {
          "id": "meta-llama/llama-3.2-3b-instruct:free",
          "description": "A small instruction-tuned model. Good for quick drafts.",
          "context_length": 131072,
          "architecture": { "input_modalities": ["text"] },
          "pricing": { "prompt": "0", "completion": "0" }
        },
        {
          "id": "vendor/promo:free",
          "description": "",
          "context_length": 8192,
          "architecture": { "input_modalities": ["text"] },
          "pricing": { "prompt": "0.000001", "completion": "0.000002" }
        },
        { "name": "skipped, no id" }
      ]
    }"#;

    fn near(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    fn cards() -> Vec<CatalogModel> {
        parse_openrouter_models(&serde_json::from_str(OPENROUTER_MODELS).unwrap())
    }

    fn provider(db: &Db, id: &str, base_url: Option<&str>) {
        db.upsert_provider(&ProviderConfig {
            id: id.into(),
            kind: "openai".into(),
            base_url: base_url.map(str::to_string),
            api_key: None,
            options: Value::Null,
        })
        .unwrap();
    }

    #[test]
    fn parses_recorded_openrouter_models_without_a_live_call() {
        let cards = cards();
        assert_eq!(cards.len(), 3);

        let sonnet = &cards[0];
        assert_eq!(sonnet.id, "anthropic/claude-sonnet-4");
        near(sonnet.input_price.unwrap(), 3.0);
        near(sonnet.output_price.unwrap(), 15.0);
        near(sonnet.cache_read_price.unwrap(), 0.3);
        near(sonnet.cache_write_price.unwrap(), 3.75);
        assert_eq!(sonnet.price_error, None);
        assert_eq!(sonnet.context_window, 200_000);
        assert!(!sonnet.free);
        assert_eq!(sonnet.modalities, vec!["text".to_string(), "image".to_string()]);
        assert_eq!(
            sonnet.use_cases_draft,
            "Claude Sonnet 4 is a fast model for everyday coding. It reads long documents and images."
        );

        let free = &cards[1];
        assert!(free.free);
        assert_eq!(free.input_price, Some(0.0));
        assert_eq!(free.output_price, Some(0.0));
        assert_eq!(free.cache_read_price, None);
        assert_eq!(free.cache_write_price, None);
        assert_eq!(free.context_window, 131_072);
        assert_eq!(free.modalities, vec!["text".to_string()]);
        assert_eq!(free.use_cases_draft, "A small instruction-tuned model. Good for quick drafts.");

        let promo = &cards[2];
        assert!(promo.free, "an id ending in :free is free even when the card lists prices");
        near(promo.input_price.unwrap(), 1.0);
        near(promo.output_price.unwrap(), 2.0);
        assert!(promo.use_cases_draft.is_empty());
    }

    #[test]
    fn drafts_the_first_two_sentences_and_caps_the_length() {
        assert_eq!(
            draft_use_cases("Version 3.5 is the current one. It writes code. And more."),
            "Version 3.5 is the current one. It writes code."
        );
        assert_eq!(draft_use_cases("  just one sentence, no stop  "), "just one sentence, no stop");
        let long = format!("{} end.", "word ".repeat(80));
        let draft = draft_use_cases(&long);
        assert!(draft.chars().count() <= DRAFT_MAX_CHARS, "{}", draft.chars().count());
        assert!(draft.ends_with('…'));
        near(per_million_tokens("3e-6").unwrap(), 3.0);
        near(per_million_tokens("0.00000375").unwrap(), 3.75);
    }

    #[test]
    fn derives_a_registry_id_and_recognises_openrouter() {
        assert_eq!(derive_registry_id("anthropic/claude-sonnet-4"), "anthropic-claude-sonnet-4");
        assert_eq!(derive_registry_id("meta-llama/llama-3.2-3b-instruct:free"), "meta-llama-llama-3.2-3b-instruct-free");
        let provider = |url: Option<&str>| ProviderConfig {
            id: "p".into(),
            kind: "openai".into(),
            base_url: url.map(str::to_string),
            api_key: None,
            options: Value::Null,
        };
        assert!(is_openrouter(&provider(Some("https://openrouter.ai/api/v1"))));
        assert!(is_openrouter(&provider(Some("https://OpenRouter.ai"))));
        assert!(!is_openrouter(&provider(Some("https://api.anthropic.com"))));
        assert!(!is_openrouter(&provider(Some("https://evilopenrouter.ai/api"))));
        assert!(!is_openrouter(&provider(Some("http://127.0.0.1:11434/v1"))));
        assert!(!is_openrouter(&provider(None)));
    }

    #[test]
    fn import_saves_the_card_and_drafts_use_cases_for_confirmation() {
        let dir = std::env::temp_dir().join(format!("grouper-catalog-test-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        provider(&db, "openrouter", Some("https://openrouter.ai/api/v1"));
        let req = ModelImportRequest {
            provider: "openrouter".into(),
            model: "anthropic/claude-sonnet-4".into(),
            id: None,
            roles: Some(vec!["thread".into()]),
            local: None,
            probe: false,
        };
        let saved = import_from_catalog(&db, "openrouter", &cards(), &req).unwrap();
        assert_eq!(saved.model.id, "anthropic-claude-sonnet-4");
        assert_eq!(saved.model.model, "anthropic/claude-sonnet-4");
        near(saved.model.input_price, 3.0);
        near(saved.model.output_price, 15.0);
        near(saved.model.cache_read_price.unwrap(), 0.3);
        assert_eq!(saved.model.context_window, 200_000);
        assert_eq!(saved.model.modalities, vec!["text".to_string(), "image".to_string()]);
        assert!(saved.model.use_cases.is_empty(), "the draft is not written until you accept it");
        assert_eq!(saved.model.roles, vec!["thread".to_string()]);
        let proposal_id = saved.proposal_id.unwrap();
        let proposal = db.proposal(proposal_id).unwrap();
        assert_eq!(proposal.source, "catalog");
        assert_eq!(proposal.status, "pending");
        assert_eq!(proposal.field, "use_cases");
        assert_eq!(proposal.reason, CATALOG_REASON);
        assert!(proposal.text.starts_with("Claude Sonnet 4"));

        let mut cheaper = cards();
        cheaper[0].input_price = Some(4.0);
        let again = import_from_catalog(
            &db,
            "openrouter",
            &cheaper,
            &ModelImportRequest { roles: None, ..req.clone() },
        )
        .unwrap();
        assert_eq!(again.proposal_id, Some(proposal_id), "a pending draft is not replaced");
        near(again.model.input_price, 4.0);
        assert_eq!(again.model.roles, vec!["thread".to_string()]);
        assert!(again.model.use_cases.is_empty());

        db.decide_proposal(proposal_id, true).unwrap();
        let accepted = import_from_catalog(&db, "openrouter", &cards(), &ModelImportRequest { roles: None, ..req }).unwrap();
        assert_eq!(accepted.proposal_id, None, "the accepted text already matches the card");
        assert!(accepted.model.use_cases.starts_with("Claude Sonnet 4"));
        near(accepted.model.input_price, 3.0);

        let free = import_from_catalog(
            &db,
            "openrouter",
            &cards(),
            &ModelImportRequest {
                provider: "openrouter".into(),
                model: "meta-llama/llama-3.2-3b-instruct:free".into(),
                id: Some("free-llama".into()),
                roles: None,
                local: Some(false),
                probe: false,
            },
        )
        .unwrap();
        assert!(free.model.free);
        assert_eq!(free.model.id, "free-llama");
        assert_eq!(free.model.modalities, vec!["text".to_string()]);
        assert!(free.proposal_id.is_some());
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn refreshing_prices_leaves_use_cases_context_and_modalities() {
        let dir = std::env::temp_dir().join(format!("grouper-catalog-price-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        provider(&db, "openrouter", Some("https://openrouter.ai/api/v1"));
        db.upsert_model(&ModelEntry {
            id: "sonnet".into(),
            provider_id: "openrouter".into(),
            model: "anthropic/claude-sonnet-4".into(),
            input_price: 1.0,
            output_price: 1.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 1,
            use_cases: "Kept".into(),
            free: true,
            roles: vec!["orchestrator".into()],
            local: true,
            modalities: vec!["file".into()],
            ..Default::default()
        })
        .unwrap();
        let model = db.find_model("sonnet").unwrap();
        let updated = update_prices(&db, &model, &cards()[0]).unwrap();
        near(updated.input_price, 3.0);
        near(updated.output_price, 15.0);
        near(updated.cache_read_price.unwrap(), 0.3);
        near(updated.cache_write_price.unwrap(), 3.75);
        assert!(!updated.free);
        assert_eq!(updated.context_window, 1);
        assert_eq!(updated.use_cases, "Kept");
        assert_eq!(updated.roles, vec!["orchestrator".to_string()]);
        assert!(updated.local);
        assert_eq!(updated.modalities, vec!["file".to_string()]);
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn anthropic_import_does_not_call_a_catalog() {
        let dir = std::env::temp_dir().join(format!("grouper-catalog-anthropic-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "anthropic".into(),
            kind: "anthropic".into(),
            base_url: Some("https://api.anthropic.com".into()),
            api_key: Some("test".into()),
            options: Value::Null,
        })
        .unwrap();
        let hub = Hub::new(db, dir.clone());
        let error = import(
            &hub,
            ModelImportRequest {
                provider: "anthropic".into(),
                model: "claude-opus-5-5".into(),
                id: None,
                roles: None,
                local: None,
                probe: false,
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("no model catalog"), "{error}");
        assert!(hub.db.models().is_empty());
        let prices = refresh_openrouter_prices(&hub).await;
        assert_eq!((prices.updated, prices.failed), (0, false));
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_catalog_fields_parse_as_empty() {
        let cards = parse_openrouter_models(&json!({"data": [{"id": "plain", "pricing": {"prompt": "0", "completion": "0"}}]}));
        assert_eq!(cards.len(), 1);
        assert!(cards[0].free);
        assert!(cards[0].modalities.is_empty());
        assert_eq!(cards[0].context_window, 0);
        assert_eq!(cards[0].cache_read_price, None);
    }

    #[test]
    fn missing_or_invalid_catalog_prices_fail_import_and_preserve_existing_on_refresh() {
        let dir = std::env::temp_dir().join(format!("grouper-catalog-badprice-{}", crate::db::random_hex(4)));
        let db = Db::open(&dir).unwrap();
        provider(&db, "openrouter", Some("https://openrouter.ai/api/v1"));

        let bad_cases = [
            ("no-pricing", json!({"id": "m/no-pricing"})),
            ("no-prompt", json!({"id": "m/no-prompt", "pricing": {"completion": "0.000001"}})),
            ("no-completion", json!({"id": "m/no-completion", "pricing": {"prompt": "0.000001"}})),
            ("null-prompt", json!({"id": "m/null-prompt", "pricing": {"prompt": null, "completion": "0"}})),
            ("bad-text", json!({"id": "m/bad-text", "pricing": {"prompt": "free", "completion": "0"}})),
            ("empty-text", json!({"id": "m/empty-text", "pricing": {"prompt": "", "completion": "0"}})),
            ("neg-str", json!({"id": "m/neg-str", "pricing": {"prompt": "-1", "completion": "0"}})),
            ("neg-num", json!({"id": "m/neg-num", "pricing": {"prompt": 0.0, "completion": -0.000001}})),
            ("nan-str", json!({"id": "m/nan-str", "pricing": {"prompt": "NaN", "completion": "0"}})),
            ("inf-str", json!({"id": "m/inf-str", "pricing": {"prompt": "Infinity", "completion": "0"}})),
            ("overflow", json!({"id": "m/overflow", "pricing": {"prompt": "1e305", "completion": "0"}})),
            ("neg-cache", json!({"id": "m/neg-cache", "pricing": {"prompt": "0", "completion": "0", "input_cache_read": "-1"}})),
            ("free-suffix-missing", json!({"id": "m/free-suffix-missing:free", "pricing": {"prompt": "0"}})),
        ];

        let rows: Vec<Value> = bad_cases.iter().map(|(_, v)| v.clone()).collect();
        let parsed = parse_openrouter_models(&json!({"data": rows}));
        assert_eq!(parsed.len(), bad_cases.len());

        for (tag, row) in &bad_cases {
            let model_name = row["id"].as_str().unwrap();
            let card = find_card(&parsed, model_name).unwrap();
            assert!(card.price_error.is_some(), "{tag}: expected price_error");
            if !model_name.ends_with(":free") {
                assert!(!card.free, "{tag}: invalid pricing must not be marked free");
            }
            let req = ModelImportRequest {
                provider: "openrouter".into(),
                model: model_name.into(),
                id: None,
                roles: None,
                local: None,
                probe: false,
            };
            assert!(import_from_catalog(&db, "openrouter", &parsed, &req).is_err(), "{tag}: import must fail");
        }
        assert!(db.models().is_empty());

        // Half-zero is not free unless id ends with :free.
        let half_zero = parse_openrouter_models(&json!({"data": [
            {"id": "m/half", "pricing": {"prompt": "0", "completion": "0.000001"}}
        ]}));
        assert!(!half_zero[0].free);

        // Refresh keeps existing prices when the catalog card has missing/invalid prices.
        db.upsert_model(&ModelEntry {
            id: "existing".into(),
            provider_id: "openrouter".into(),
            model: "m/neg-str".into(),
            input_price: 2.5,
            output_price: 7.5,
            cache_read_price: Some(0.25),
            cache_write_price: None,
            context_window: 100_000,
            use_cases: "kept".into(),
            free: false,
            roles: vec![],
            local: false,
            modalities: vec![],
            ..Default::default()
        })
        .unwrap();
        let updated = apply_price_refresh(&db, "openrouter", &parsed, &db.models());
        assert_eq!(updated, 0);
        let kept = db.find_model("existing").unwrap();
        near(kept.input_price, 2.5);
        near(kept.output_price, 7.5);
        near(kept.cache_read_price.unwrap(), 0.25);
        assert!(!kept.free);

        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
