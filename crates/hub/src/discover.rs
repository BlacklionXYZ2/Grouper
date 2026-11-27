//! Model discovery, chat filtering, and built-in price lookup per provider preset.
//!
//! Fetches the provider's native model listing endpoint, filters out non-chat models
//! (embeddings, TTS, audio, image/video generation, moderation, legacy completions),
//! and fills prices (USD per million tokens) from OpenRouter's catalog, xAI's list
//! response, or the compiled-in [`prices.json`] prefix table.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::{self, CatalogModel};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_PAGES: usize = 100;
const PRICES_JSON: &str = include_str!("prices.json");

const NON_CHAT_SUBSTRINGS: &[&str] = &[
    "embed",
    "tts",
    "whisper",
    "transcribe",
    "audio",
    "realtime",
    "dall-e",
    "image",
    "imagen",
    "veo",
    "sora",
    "moderation",
    "rerank",
    "aqa",
    "instruct",
    "babbage",
    "davinci",
];

/// Supported provider discovery presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openrouter")]
    OpenRouter,
    Gemini,
    Xai,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Generic,
}

impl Preset {
    /// Canonical preset identifier string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
            Self::Gemini => "gemini",
            Self::Xai => "xai",
            Self::DeepSeek => "deepseek",
            Self::Generic => "generic",
        }
    }

    /// Default API base URL for presets with a fixed upstream host.
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("https://api.anthropic.com"),
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::OpenRouter => Some("https://openrouter.ai/api/v1"),
            Self::Gemini => Some("https://generativelanguage.googleapis.com/v1beta/openai"),
            Self::Xai => Some("https://api.x.ai/v1"),
            Self::DeepSeek => Some("https://api.deepseek.com/v1"),
            Self::Generic => None,
        }
    }

    /// Choose preset from an explicit `options.preset`, else `base_url` host, else `Generic`.
    pub fn resolve(options_preset: Option<&str>, base_url: Option<&str>) -> Self {
        if let Some(raw) = options_preset.map(str::trim).filter(|s| !s.is_empty())
            && let Ok(preset) = Self::from_str(raw)
        {
            return preset;
        }
        if let Some(url) = base_url.map(str::trim).filter(|s| !s.is_empty())
            && let Some(host) = Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        {
            return match host.as_str() {
                "api.anthropic.com" | "anthropic.com" => Self::Anthropic,
                "api.openai.com" => Self::OpenAi,
                h if h == "openrouter.ai" || h.ends_with(".openrouter.ai") => Self::OpenRouter,
                "generativelanguage.googleapis.com" => Self::Gemini,
                "api.x.ai" => Self::Xai,
                "api.deepseek.com" => Self::DeepSeek,
                _ => Self::Generic,
            };
        }
        Self::Generic
    }
}

impl FromStr for Preset {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "openai" | "open_ai" => Ok(Self::OpenAi),
            "openrouter" | "open_router" => Ok(Self::OpenRouter),
            "gemini" | "google" | "google_gemini" => Ok(Self::Gemini),
            "xai" | "x.ai" | "grok" => Ok(Self::Xai),
            "deepseek" | "deep_seek" => Ok(Self::DeepSeek),
            "generic" => Ok(Self::Generic),
            other => bail!("unknown discovery preset {other:?}"),
        }
    }
}

/// Whether a model supports tool/function calling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolsSupport {
    Yes,
    No,
    Unknown,
}

/// Origin of the model's pricing information.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    Catalog,
    Xai,
    Table,
    None,
}

/// One model discovered from a provider's listing endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: u64,
    /// USD per million input tokens (`None` when `price_source == PriceSource::None`).
    pub input_price: Option<f64>,
    /// USD per million output tokens (`None` when `price_source == PriceSource::None`).
    pub output_price: Option<f64>,
    /// USD per million cached input read tokens.
    pub cache_read_price: Option<f64>,
    /// USD per million cached input write tokens.
    pub cache_write_price: Option<f64>,
    pub modalities: Vec<String>,
    pub chat_capable: bool,
    pub free: bool,
    pub tools: ToolsSupport,
    pub price_source: PriceSource,
    /// Relative capability rank from the built-in price table (lower = higher-ranked).
    pub rank: Option<u32>,
    /// Use-case summary drafted from OpenRouter's model card description, if any.
    pub use_cases_draft: Option<String>,
}

/// Listing outcome including how many non-chat models were filtered out.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoverResult {
    pub models: Vec<DiscoveredModel>,
    pub filtered: usize,
}

/// Built-in price table entry (`crates/hub/src/prices.json`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PriceEntry {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    pub context_window: u64,
    pub tools: bool,
    #[serde(default)]
    pub rank: Option<u32>,
}

fn price_table() -> &'static Vec<(String, PriceEntry)> {
    static TABLE: OnceLock<Vec<(String, PriceEntry)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let raw: HashMap<String, PriceEntry> =
            serde_json::from_str(PRICES_JSON).expect("compiled-in prices.json must be valid");
        let mut entries: Vec<(String, PriceEntry)> =
            raw.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
        // Sort longest prefix first so dated snapshots inherit their closest family entry.
        entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        entries
    })
}

/// Look up a model in the compiled-in price table by longest matching prefix.
pub fn lookup_price(model_id: &str) -> Option<&'static PriceEntry> {
    let lower = model_id.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    price_table().iter().find_map(|(prefix, entry)| {
        let rest = lower.strip_prefix(prefix.as_str())?;
        let boundary = rest.is_empty()
            || matches!(rest.as_bytes()[0], b'-' | b'_' | b'.' | b':' | b'@' | b'/');
        boundary.then_some(entry)
    })
}

/// Check whether a model ID passes the fallback non-chat deny-list.
pub fn passes_chat_denylist(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    !NON_CHAT_SUBSTRINGS.iter().any(|needle| lower.contains(needle))
}

/// Filter a parsed listing to chat-capable models, returning `(kept, filtered_count)`.
pub fn filter_chat_models(models: Vec<DiscoveredModel>) -> (Vec<DiscoveredModel>, usize) {
    let total = models.len();
    let kept: Vec<DiscoveredModel> = models.into_iter().filter(|m| m.chat_capable).collect();
    let filtered = total - kept.len();
    (kept, filtered)
}

/// Fetch and filter chat-capable models for a provider preset.
///
/// Never logs or embeds `api_key` in error messages.
pub async fn discover(
    preset: Preset,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Vec<DiscoveredModel>> {
    Ok(discover_detailed(preset, base_url, api_key).await?.models)
}

/// Fetch and filter chat-capable models, also returning how many non-chat models were dropped.
///
/// Never logs or embeds `api_key` in error messages.
pub async fn discover_detailed(
    preset: Preset,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<DiscoverResult> {
    let base = resolve_base(preset, base_url)?;
    let key = api_key.map(str::trim).filter(|k| !k.is_empty());
    let client = http_client()?;

    let parsed = match preset {
        Preset::Anthropic => fetch_anthropic(&client, &base, key).await?,
        Preset::OpenAi => fetch_openai_like(&client, &base, key, "OpenAI").await?,
        Preset::OpenRouter => fetch_openrouter(&client, &base, key).await?,
        Preset::Gemini => fetch_gemini(&client, &base, key).await?,
        Preset::Xai => fetch_xai(&client, &base, key).await?,
        Preset::DeepSeek => fetch_openai_like(&client, &base, key, "DeepSeek").await?,
        Preset::Generic => fetch_generic(&client, &base, key).await?,
    };

    let (models, filtered) = filter_chat_models(parsed);
    Ok(DiscoverResult { models, filtered })
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().context("building HTTP client")
}

fn resolve_base(preset: Preset, base_url: Option<&str>) -> Result<String> {
    let raw = base_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| preset.default_base_url())
        .with_context(|| format!("preset {} requires a base_url", preset.as_str()))?;
    Ok(raw.trim_end_matches('/').to_string())
}

fn non_empty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn apply_table_defaults(model: &mut DiscoveredModel) {
    let Some(entry) = lookup_price(&model.id) else {
        return;
    };
    if model.context_window == 0 {
        model.context_window = entry.context_window;
    }
    if model.tools == ToolsSupport::Unknown {
        model.tools = if entry.tools { ToolsSupport::Yes } else { ToolsSupport::No };
    }
    if model.rank.is_none() {
        model.rank = entry.rank;
    }
    if model.price_source == PriceSource::None {
        model.input_price = Some(entry.input);
        model.output_price = Some(entry.output);
        model.cache_read_price = entry.cache_read;
        model.cache_write_price = entry.cache_write;
        model.price_source = PriceSource::Table;
    }
}

fn basic_model(id: String, display_name: Option<String>, chat_capable: bool) -> DiscoveredModel {
    let mut model = DiscoveredModel {
        id,
        display_name,
        context_window: 0,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        modalities: Vec::new(),
        chat_capable,
        free: false,
        tools: ToolsSupport::Unknown,
        price_source: PriceSource::None,
        rank: None,
        use_cases_draft: None,
    };
    apply_table_defaults(&mut model);
    model
}

async fn get_json(req: reqwest::RequestBuilder, label: &str) -> Result<(StatusCode, Value)> {
    let response = req.send().await.with_context(|| format!("requesting {label}"))?;
    let status = response.status();
    if !status.is_success() {
        return Ok((status, Value::Null));
    }
    let body = response.json::<Value>().await.with_context(|| format!("{label} returned invalid JSON"))?;
    Ok((status, body))
}

async fn fetch_anthropic(client: &reqwest::Client, base: &str, key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let list_base = base.strip_suffix("/v1").unwrap_or(base);
    let mut out = Vec::new();
    let mut after_id: Option<String> = None;
    let mut seen_cursors = HashSet::new();

    for _ in 0..MAX_PAGES {
        let mut url = Url::parse(&format!("{list_base}/v1/models"))
            .context("invalid Anthropic base_url")?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("limit", "1000");
            if let Some(cursor) = after_id.as_deref() {
                q.append_pair("after_id", cursor);
            }
        }
        let mut req = client.get(url).header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(k) = key {
            req = req.header("x-api-key", k);
        }
        let (status, body) = get_json(req, "Anthropic /v1/models").await?;
        if !status.is_success() {
            bail!("Anthropic /v1/models returned HTTP {status}");
        }
        out.extend(parse_anthropic_page(&body));
        let has_more = body.get("has_more").and_then(Value::as_bool).unwrap_or(false);
        if !has_more {
            break;
        }
        let next = non_empty_str(body.get("last_id")).or_else(|| {
            body.get("data")
                .and_then(Value::as_array)
                .and_then(|rows| rows.last())
                .and_then(|row| non_empty_str(row.get("id")))
        });
        let Some(cursor) = next else { break };
        if !seen_cursors.insert(cursor.clone()) {
            break;
        }
        after_id = Some(cursor);
    }
    Ok(out)
}

/// Parse one page of Anthropic's `GET /v1/models` response.
pub fn parse_anthropic_page(body: &Value) -> Vec<DiscoveredModel> {
    // VERIFY: Anthropic /v1/models returns id, display_name, created_at with no context or price fields;
    // context_window and prices are populated from prices.json.
    body.get("data")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = non_empty_str(row.get("id"))?;
                    let display_name = non_empty_str(row.get("display_name"));
                    let chat_capable = passes_chat_denylist(&id);
                    Some(basic_model(id, display_name, chat_capable))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn fetch_openai_like(
    client: &reqwest::Client,
    base: &str,
    key: Option<&str>,
    label: &str,
) -> Result<Vec<DiscoveredModel>> {
    let mut req = client.get(format!("{base}/models"));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let (status, body) = get_json(req, &format!("{label} /models")).await?;
    if !status.is_success() {
        bail!("{label} /models returned HTTP {status}");
    }
    Ok(parse_openai_models(&body))
}

/// Parse a standard OpenAI-shaped `GET /models` response (`data[].id`).
pub fn parse_openai_models(body: &Value) -> Vec<DiscoveredModel> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = non_empty_str(row.get("id"))?;
                    let chat_capable = passes_chat_denylist(&id);
                    Some(basic_model(id, None, chat_capable))
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn fetch_openrouter(client: &reqwest::Client, base: &str, key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    if let Some(k) = key {
        // VERIFY: Prefer GET {base}/models/user when authenticated so account provider preferences apply;
        // fall back to GET {base}/models if /models/user is unsupported on the target server.
        let req = client.get(format!("{base}/models/user")).bearer_auth(k);
        let (status, body) = get_json(req, "OpenRouter /models/user").await?;
        if status.is_success() {
            return Ok(parse_openrouter_discovery(&body));
        }
        if !matches!(status, StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED) {
            bail!("OpenRouter /models/user returned HTTP {status}");
        }
    }

    let mut req = client.get(format!("{base}/models"));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let (status, body) = get_json(req, "OpenRouter /models").await?;
    if !status.is_success() {
        bail!("OpenRouter /models returned HTTP {status}");
    }
    Ok(parse_openrouter_discovery(&body))
}

/// Parse an OpenRouter `/models` or `/models/user` response by reusing `catalog::parse_openrouter_models`.
pub fn parse_openrouter_discovery(body: &Value) -> Vec<DiscoveredModel> {
    let cards: Vec<CatalogModel> = catalog::parse_openrouter_models(body);
    let raw_rows: Vec<&Value> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter(|row| non_empty_str(row.get("id")).is_some()).collect())
        .unwrap_or_default();

    cards
        .into_iter()
        .zip(raw_rows)
        .map(|(card, row)| openrouter_model_from_card(card, row))
        .collect()
}

fn openrouter_model_from_card(card: CatalogModel, row: &Value) -> DiscoveredModel {
    let display_name = non_empty_str(row.get("name"));
    let output_modalities = row
        .get("architecture")
        .and_then(|a| a.get("output_modalities"))
        .and_then(Value::as_array);
    let chat_capable = output_modalities
        .map(|items| items.iter().any(|v| v.as_str().is_some_and(|s| s.trim().eq_ignore_ascii_case("text"))))
        .unwrap_or(false);

    let tools = match row.get("supported_parameters").and_then(Value::as_array) {
        Some(params) => {
            if params.iter().any(|v| v.as_str().is_some_and(|s| s.trim().eq_ignore_ascii_case("tools"))) {
                ToolsSupport::Yes
            } else {
                ToolsSupport::No
            }
        }
        None => ToolsSupport::Unknown,
    };

    let valid_catalog_prices =
        card.price_error.is_none() && card.input_price.is_some() && card.output_price.is_some();
    let (input_price, output_price, cache_read_price, cache_write_price, price_source, free) =
        if valid_catalog_prices {
            (
                card.input_price,
                card.output_price,
                card.cache_read_price,
                card.cache_write_price,
                PriceSource::Catalog,
                card.free,
            )
        } else {
            (None, None, None, None, PriceSource::None, false)
        };

    let use_cases_draft = (!card.use_cases_draft.is_empty()).then_some(card.use_cases_draft);

    DiscoveredModel {
        id: card.id,
        display_name,
        context_window: card.context_window,
        input_price,
        output_price,
        cache_read_price,
        cache_write_price,
        modalities: card.modalities,
        chat_capable,
        free,
        tools,
        price_source,
        rank: None,
        use_cases_draft,
    }
}

fn gemini_native_base(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if let Some(prefix) = trimmed.strip_suffix("/v1beta/openai") {
        format!("{prefix}/v1beta")
    } else if let Some(prefix) = trimmed.strip_suffix("/openai") {
        if prefix.ends_with("/v1beta") { prefix.to_string() } else { format!("{prefix}/v1beta") }
    } else if trimmed.ends_with("/v1beta") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1beta")
    }
}

async fn fetch_gemini(client: &reqwest::Client, base: &str, key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let native_base = gemini_native_base(base);
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    let mut seen_tokens = HashSet::new();

    for _ in 0..MAX_PAGES {
        let mut url = Url::parse(&format!("{native_base}/models")).context("invalid Gemini base_url")?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("pageSize", "1000");
            if let Some(tok) = page_token.as_deref() {
                q.append_pair("pageToken", tok);
            }
        }
        let mut req = client.get(url);
        if let Some(k) = key {
            req = req.header("x-goog-api-key", k);
        }
        let (status, body) = get_json(req, "Gemini /v1beta/models").await?;
        if !status.is_success() {
            bail!("Gemini /v1beta/models returned HTTP {status}");
        }
        out.extend(parse_gemini_page(&body));
        let Some(next) = non_empty_str(body.get("nextPageToken")) else { break };
        if !seen_tokens.insert(next.clone()) {
            break;
        }
        page_token = Some(next);
    }
    Ok(out)
}

/// Parse one page of Gemini's `GET /v1beta/models` response.
pub fn parse_gemini_page(body: &Value) -> Vec<DiscoveredModel> {
    body.get("models")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let raw_name = non_empty_str(row.get("name"))?;
                    // VERIFY: confirm the stripped Gemini model id (without "models/") is accepted
                    // by Gemini's OpenAI-compatible chat completions endpoint.
                    let id = raw_name.strip_prefix("models/").unwrap_or(&raw_name).trim().to_string();
                    if id.is_empty() {
                        return None;
                    }
                    let display_name = non_empty_str(row.get("displayName"));
                    let context_window = row.get("inputTokenLimit").and_then(Value::as_u64).unwrap_or(0);
                    let methods = string_array(row.get("supportedGenerationMethods"));
                    let chat_capable = methods.iter().any(|m| m == "generateContent");
                    let mut model = basic_model(id, display_name, chat_capable);
                    if context_window > 0 {
                        model.context_window = context_window;
                    }
                    Some(model)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_xai_price(row: &Value, key: &str) -> Option<f64> {
    let raw = row.get(key)?;
    let n = raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    // VERIFY: xAI /language-models price fields are in USD cents per 100M tokens
    // (1e-10 USD/token = divide by 10_000.0 for USD per million tokens); verify against live API.
    let per_million = n / 10_000.0;
    (per_million.is_finite() && per_million >= 0.0).then_some(per_million)
}

async fn fetch_xai(client: &reqwest::Client, base: &str, key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let mut req = client.get(format!("{base}/language-models"));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let (status, body) = get_json(req, "xAI /language-models").await?;
    if status.is_success() {
        return Ok(parse_xai_language_models(&body));
    }
    if !matches!(status, StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED) {
        bail!("xAI /language-models returned HTTP {status}");
    }
    fetch_openai_like(client, base, key, "xAI").await
}

/// Parse an xAI `GET /language-models` response.
pub fn parse_xai_language_models(body: &Value) -> Vec<DiscoveredModel> {
    let rows = body
        .get("models")
        .or_else(|| body.get("data"))
        .and_then(Value::as_array);
    let Some(rows) = rows else { return Vec::new() };

    rows.iter()
        .filter_map(|row| {
            let id = non_empty_str(row.get("id"))?;
            let modalities = string_array(row.get("input_modalities"));
            let output_modalities = string_array(row.get("output_modalities"));
            let chat_capable = if output_modalities.is_empty() {
                true
            } else {
                output_modalities.iter().any(|m| m.eq_ignore_ascii_case("text"))
            };

            let input_price = parse_xai_price(row, "prompt_text_token_price");
            let output_price = parse_xai_price(row, "completion_text_token_price");
            let cache_read_price = parse_xai_price(row, "cached_prompt_text_token_price");
            let has_xai_prices = input_price.is_some() && output_price.is_some();

            let mut model = DiscoveredModel {
                id,
                display_name: None,
                context_window: 0,
                input_price: if has_xai_prices { input_price } else { None },
                output_price: if has_xai_prices { output_price } else { None },
                cache_read_price: if has_xai_prices { cache_read_price } else { None },
                cache_write_price: None,
                modalities,
                chat_capable,
                free: false,
                tools: ToolsSupport::Unknown,
                price_source: if has_xai_prices { PriceSource::Xai } else { PriceSource::None },
                rank: None,
                use_cases_draft: None,
            };
            apply_table_defaults(&mut model);
            Some(model)
        })
        .collect()
}

async fn fetch_generic(client: &reqwest::Client, base: &str, key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let mut req = client.get(format!("{base}/models"));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let (status, body) = get_json(req, "Generic /models").await?;
    if !status.is_success() {
        bail!("Generic /models returned HTTP {status}");
    }
    Ok(parse_generic_models(&body))
}

/// Parse a generic OpenAI-compatible `/models` response (`data[]`, `models[]`, or bare array).
pub fn parse_generic_models(body: &Value) -> Vec<DiscoveredModel> {
    // VERIFY: confirm bare-array and top-level `models` response shapes for LM Studio and Ollama.
    let rows = if let Some(arr) = body.as_array() {
        arr
    } else if let Some(arr) = body.get("data").and_then(Value::as_array) {
        arr
    } else if let Some(arr) = body.get("models").and_then(Value::as_array) {
        arr
    } else {
        return Vec::new();
    };

    rows.iter()
        .filter_map(|row| {
            let id = if let Some(s) = row.as_str() {
                let s = s.trim();
                (!s.is_empty()).then(|| s.to_string())?
            } else {
                non_empty_str(row.get("id"))
                    .or_else(|| non_empty_str(row.get("name")))
                    .or_else(|| non_empty_str(row.get("model")))?
            };
            let chat_capable = passes_chat_denylist(&id);
            Some(basic_model(id, None, chat_capable))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::Query;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use tokio::net::TcpListener;

    const ANTHROPIC_P1: &str = include_str!("../tests/fixtures/discover/anthropic_page1.json");
    const ANTHROPIC_P2: &str = include_str!("../tests/fixtures/discover/anthropic_page2.json");
    const OPENAI_FIXTURE: &str = include_str!("../tests/fixtures/discover/openai.json");
    const OPENROUTER_FIXTURE: &str = include_str!("../tests/fixtures/discover/openrouter.json");
    const GEMINI_P1: &str = include_str!("../tests/fixtures/discover/gemini_page1.json");
    const GEMINI_P2: &str = include_str!("../tests/fixtures/discover/gemini_page2.json");
    const XAI_LM_FIXTURE: &str = include_str!("../tests/fixtures/discover/xai_language_models.json");
    const XAI_FALLBACK_FIXTURE: &str = include_str!("../tests/fixtures/discover/xai_models_fallback.json");
    const DEEPSEEK_FIXTURE: &str = include_str!("../tests/fixtures/discover/deepseek.json");
    const GENERIC_OPENAI_FIXTURE: &str = include_str!("../tests/fixtures/discover/generic_openai.json");
    const GENERIC_MODELS_KEY_FIXTURE: &str = include_str!("../tests/fixtures/discover/generic_models_key.json");
    const GENERIC_BARE_ARRAY_FIXTURE: &str = include_str!("../tests/fixtures/discover/generic_bare_array.json");

    fn near(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    async fn spawn_mock(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[test]
    fn resolves_presets_from_option_or_host() {
        assert_eq!(Preset::resolve(Some("anthropic"), Some("https://proxy.example.com")), Preset::Anthropic);
        assert_eq!(Preset::resolve(None, Some("https://api.anthropic.com")), Preset::Anthropic);
        assert_eq!(Preset::resolve(None, Some("https://api.openai.com/v1")), Preset::OpenAi);
        assert_eq!(Preset::resolve(None, Some("https://openrouter.ai/api/v1")), Preset::OpenRouter);
        assert_eq!(Preset::resolve(None, Some("https://sub.openrouter.ai/api/v1")), Preset::OpenRouter);
        assert_eq!(
            Preset::resolve(None, Some("https://generativelanguage.googleapis.com/v1beta/openai")),
            Preset::Gemini
        );
        assert_eq!(Preset::resolve(None, Some("https://api.x.ai/v1")), Preset::Xai);
        assert_eq!(Preset::resolve(None, Some("https://api.deepseek.com/v1")), Preset::DeepSeek);
        assert_eq!(Preset::resolve(None, Some("http://127.0.0.1:11434/v1")), Preset::Generic);
        assert_eq!(Preset::resolve(None, None), Preset::Generic);
    }

    #[test]
    fn price_table_uses_longest_prefix_and_covers_all_providers() {
        // Dated snapshot inherits longest family prefix.
        let mini = lookup_price("gpt-4o-mini-2024-07-18").unwrap();
        near(mini.input, 0.15);
        near(mini.output, 0.6);

        let gpt4o = lookup_price("gpt-4o-2024-11-20").unwrap();
        near(gpt4o.input, 2.5);
        near(gpt4o.output, 10.0);

        let opus = lookup_price("claude-opus-4-6-20260205").unwrap();
        near(opus.input, 5.0);
        near(opus.output, 25.0);
        near(opus.cache_read.unwrap(), 0.5);
        near(opus.cache_write.unwrap(), 6.25);

        let gemini = lookup_price("gemini-2.5-flash-preview-05-20").unwrap();
        near(gemini.input, 0.3);
        near(gemini.output, 2.5);

        let grok = lookup_price("grok-3-mini-fast").unwrap();
        near(grok.input, 0.3);
        near(grok.output, 0.5);

        let deepseek = lookup_price("deepseek-chat").unwrap();
        near(deepseek.input, 0.28);
        near(deepseek.output, 0.42);

        // Boundary check: "o1" must not match "o10".
        assert!(lookup_price("o10-preview").is_none());
        assert!(!lookup_price("o1-mini-2024-09-12").unwrap().tools);
        assert!(lookup_price("unknown-model-xyz").is_none());
    }

    #[test]
    fn parses_openrouter_fixture_and_reuses_catalog_units() {
        let body: Value = serde_json::from_str(OPENROUTER_FIXTURE).unwrap();
        let all = parse_openrouter_discovery(&body);
        assert_eq!(all.len(), 4);

        let sonnet = &all[0];
        assert_eq!(sonnet.id, "anthropic/claude-sonnet-4");
        assert_eq!(sonnet.display_name.as_deref(), Some("Anthropic: Claude Sonnet 4"));
        assert!(sonnet.chat_capable);
        assert_eq!(sonnet.tools, ToolsSupport::Yes);
        assert_eq!(sonnet.price_source, PriceSource::Catalog);
        near(sonnet.input_price.unwrap(), 3.0);
        near(sonnet.output_price.unwrap(), 15.0);
        near(sonnet.cache_read_price.unwrap(), 0.3);
        near(sonnet.cache_write_price.unwrap(), 3.75);
        assert_eq!(sonnet.context_window, 200_000);
        assert_eq!(sonnet.modalities, vec!["text".to_string(), "image".to_string()]);
        assert!(!sonnet.free);
        assert!(sonnet.use_cases_draft.as_deref().unwrap().starts_with("Claude Sonnet 4"));

        // Instruct-named chat model on OpenRouter is kept because output_modalities includes text.
        let free_llama = &all[1];
        assert!(free_llama.chat_capable);
        assert!(free_llama.free);
        assert_eq!(free_llama.tools, ToolsSupport::No);
        assert_eq!(free_llama.price_source, PriceSource::Catalog);

        // Non-text output modality is marked non-chat and dropped by filter_chat_models.
        let flux = &all[2];
        assert!(!flux.chat_capable);

        // Missing pricing is PriceSource::None and never marked free.
        let unpriced = &all[3];
        assert!(unpriced.chat_capable);
        assert_eq!(unpriced.price_source, PriceSource::None);
        assert_eq!(unpriced.input_price, None);
        assert_eq!(unpriced.output_price, None);
        assert!(!unpriced.free);
        assert_eq!(unpriced.tools, ToolsSupport::Unknown);

        let (kept, filtered) = filter_chat_models(all);
        assert_eq!(kept.len(), 3);
        assert_eq!(filtered, 1);
    }

    #[tokio::test]
    async fn discovers_anthropic_with_pagination_and_headers() {
        let seen_headers = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let seen_ref = seen_headers.clone();

        let app = Router::new().route(
            "/v1/models",
            get(move |headers: HeaderMap, Query(params): Query<HashMap<String, String>>| {
                let seen = seen_ref.clone();
                async move {
                    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    let version =
                        headers.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    seen.lock().unwrap().push((api_key, version));
                    let body = if params.get("after_id").map(String::as_str) == Some("claude-sonnet-4-5-20250929") {
                        ANTHROPIC_P2
                    } else {
                        assert_eq!(params.get("limit").map(String::as_str), Some("1000"));
                        ANTHROPIC_P1
                    };
                    (StatusCode::OK, [("content-type", "application/json")], body)
                }
            }),
        );

        let base = spawn_mock(app).await;
        let models = discover(Preset::Anthropic, Some(&base), Some("sk-ant-secret")).await.unwrap();
        assert_eq!(models.len(), 4);
        assert_eq!(seen_headers.lock().unwrap().len(), 2);
        for (k, v) in seen_headers.lock().unwrap().iter() {
            assert_eq!(k, "sk-ant-secret");
            assert_eq!(v, "2023-06-01");
        }

        assert_eq!(models[0].id, "claude-opus-4-6-20260205");
        assert_eq!(models[0].display_name.as_deref(), Some("Claude Opus 4.6"));
        assert_eq!(models[0].price_source, PriceSource::Table);
        near(models[0].input_price.unwrap(), 5.0);
        near(models[0].output_price.unwrap(), 25.0);
        assert_eq!(models[0].context_window, 200_000);
        assert_eq!(models[0].tools, ToolsSupport::Yes);

        let unlisted = &models[3];
        assert_eq!(unlisted.id, "claude-experimental-unlisted");
        assert_eq!(unlisted.price_source, PriceSource::None);
        assert_eq!(unlisted.input_price, None);
        assert!(!unlisted.free);
        assert_eq!(unlisted.tools, ToolsSupport::Unknown);
    }

    #[tokio::test]
    async fn discovers_openai_and_filters_non_chat_models() {
        let app = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-openai-test");
                (StatusCode::OK, [("content-type", "application/json")], OPENAI_FIXTURE)
            }),
        );

        let base = format!("{}/v1", spawn_mock(app).await);
        let result = discover_detailed(Preset::OpenAi, Some(&base), Some("sk-openai-test")).await.unwrap();

        let ids: Vec<&str> = result.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-4o-2024-11-20",
                "gpt-4o-mini-2024-07-18",
                "o3-mini-2025-01-31",
                "o1-mini-2024-09-12",
                "gpt-future-unpriced",
            ]
        );
        assert_eq!(result.filtered, 12);
        near(result.models[1].input_price.unwrap(), 0.15);
        assert_eq!(result.models[3].tools, ToolsSupport::No);
        assert_eq!(result.models[4].price_source, PriceSource::None);
        assert!(!result.models[4].free);
    }

    #[tokio::test]
    async fn discovers_openrouter_preferring_user_models_with_fallback() {
        // Case 1: /models/user succeeds
        let app_user = Router::new()
            .route(
                "/api/v1/models/user",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer sk-or-test");
                    (StatusCode::OK, [("content-type", "application/json")], OPENROUTER_FIXTURE)
                }),
            )
            .route("/api/v1/models", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));

        let base_user = format!("{}/api/v1", spawn_mock(app_user).await);
        let models = discover(Preset::OpenRouter, Some(&base_user), Some("sk-or-test")).await.unwrap();
        assert_eq!(models.len(), 3);

        // Case 2: /models/user returns 404 -> falls back to /models
        let app_fallback = Router::new()
            .route("/api/v1/models/user", get(|| async { StatusCode::NOT_FOUND }))
            .route(
                "/api/v1/models",
                get(|| async { (StatusCode::OK, [("content-type", "application/json")], OPENROUTER_FIXTURE) }),
            );

        let base_fallback = format!("{}/api/v1", spawn_mock(app_fallback).await);
        let models = discover(Preset::OpenRouter, Some(&base_fallback), Some("sk-or-test")).await.unwrap();
        assert_eq!(models.len(), 3);
    }

    #[tokio::test]
    async fn discovers_gemini_native_list_with_pagination_and_prefix_strip() {
        let app = Router::new().route(
            "/v1beta/models",
            get(|headers: HeaderMap, Query(params): Query<HashMap<String, String>>| async move {
                assert_eq!(headers.get("x-goog-api-key").unwrap(), "goog-secret-key");
                assert_eq!(params.get("pageSize").map(String::as_str), Some("1000"));
                let body = if params.get("pageToken").map(String::as_str) == Some("page-2-token") {
                    GEMINI_P2
                } else {
                    GEMINI_P1
                };
                (StatusCode::OK, [("content-type", "application/json")], body)
            }),
        );

        let base = format!("{}/v1beta/openai", spawn_mock(app).await);
        let result = discover_detailed(Preset::Gemini, Some(&base), Some("goog-secret-key")).await.unwrap();
        assert_eq!(result.filtered, 3);
        assert_eq!(result.models.len(), 2);

        assert_eq!(result.models[0].id, "gemini-2.5-pro");
        assert_eq!(result.models[0].display_name.as_deref(), Some("Gemini 2.5 Pro"));
        assert_eq!(result.models[0].context_window, 1_048_576);
        assert_eq!(result.models[0].price_source, PriceSource::Table);
        near(result.models[0].input_price.unwrap(), 1.25);

        assert_eq!(result.models[1].id, "gemini-2.5-flash-001");
        near(result.models[1].input_price.unwrap(), 0.3);
    }

    #[tokio::test]
    async fn discovers_xai_language_models_and_models_fallback() {
        // Case 1: /language-models succeeds with xAI price conversion + table fallback
        let app_lm = Router::new().route(
            "/v1/language-models",
            get(|| async { (StatusCode::OK, [("content-type", "application/json")], XAI_LM_FIXTURE) }),
        );
        let base_lm = format!("{}/v1", spawn_mock(app_lm).await);
        let models = discover(Preset::Xai, Some(&base_lm), Some("xai-key")).await.unwrap();
        assert_eq!(models.len(), 2);

        assert_eq!(models[0].id, "grok-4.7-0709");
        assert_eq!(models[0].price_source, PriceSource::Xai);
        near(models[0].input_price.unwrap(), 2.0);
        near(models[0].output_price.unwrap(), 6.0);
        near(models[0].cache_read_price.unwrap(), 0.5);
        assert_eq!(models[0].context_window, 500_000);
        assert_eq!(models[0].modalities, vec!["text".to_string(), "image".to_string()]);

        // Missing prices in /language-models fall back to prices.json
        assert_eq!(models[1].id, "grok-3-mini");
        assert_eq!(models[1].price_source, PriceSource::Table);
        near(models[1].input_price.unwrap(), 0.3);
        near(models[1].output_price.unwrap(), 0.5);

        // Case 2: /language-models returns 404 -> falls back to /models and filters image models
        let app_fb = Router::new()
            .route("/v1/language-models", get(|| async { StatusCode::NOT_FOUND }))
            .route(
                "/v1/models",
                get(|| async { (StatusCode::OK, [("content-type", "application/json")], XAI_FALLBACK_FIXTURE) }),
            );
        let base_fb = format!("{}/v1", spawn_mock(app_fb).await);
        let result = discover_detailed(Preset::Xai, Some(&base_fb), Some("xai-key")).await.unwrap();
        assert_eq!(result.models.len(), 1);
        assert_eq!(result.models[0].id, "grok-3");
        assert_eq!(result.filtered, 1);
        assert_eq!(result.models[0].price_source, PriceSource::Table);
        near(result.models[0].input_price.unwrap(), 3.0);
    }

    #[tokio::test]
    async fn discovers_deepseek_and_generic_shapes() {
        let app_ds = Router::new().route(
            "/v1/models",
            get(|| async { (StatusCode::OK, [("content-type", "application/json")], DEEPSEEK_FIXTURE) }),
        );
        let base_ds = format!("{}/v1", spawn_mock(app_ds).await);
        let ds = discover_detailed(Preset::DeepSeek, Some(&base_ds), Some("ds-key")).await.unwrap();
        assert_eq!(ds.models.len(), 2);
        assert_eq!(ds.filtered, 1);
        near(ds.models[0].input_price.unwrap(), 0.28);
        near(ds.models[0].output_price.unwrap(), 0.42);
        near(ds.models[0].cache_read_price.unwrap(), 0.028);

        for (fixture, expected_id) in [
            (GENERIC_OPENAI_FIXTURE, "llama-3.3-70b"),
            (GENERIC_MODELS_KEY_FIXTURE, "qwen2.5-coder:32b"),
            (GENERIC_BARE_ARRAY_FIXTURE, "mistral-small-24b"),
        ] {
            let app = Router::new().route(
                "/v1/models",
                get(move || async move { (StatusCode::OK, [("content-type", "application/json")], fixture) }),
            );
            let base = format!("{}/v1", spawn_mock(app).await);
            let res = discover_detailed(Preset::Generic, Some(&base), None).await.unwrap();
            assert_eq!(res.models.len(), 1);
            assert_eq!(res.models[0].id, expected_id);
            assert_eq!(res.models[0].price_source, PriceSource::None);
            assert!(!res.models[0].free);
        }
    }

    #[tokio::test]
    async fn errors_never_leak_api_key() {
        let secret = "sk-super-secret-do-not-leak-999";
        let app = Router::new().route("/v1/models", get(|| async { StatusCode::UNAUTHORIZED }));
        let base = spawn_mock(app).await;
        let err = discover(Preset::Anthropic, Some(&base), Some(secret))
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains(secret), "error leaked api key: {err}");
        assert!(err.contains("401"), "{err}");

        let generic_err = discover(Preset::Generic, None, Some(secret))
            .await
            .unwrap_err()
            .to_string();
        assert!(!generic_err.contains(secret), "error leaked api key: {generic_err}");
    }
}
