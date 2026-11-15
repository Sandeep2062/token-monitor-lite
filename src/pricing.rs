//! Exact integer pricing over captured token counts.
//!
//! Money is never represented as `f64`: rates are integer nano-USD per million
//! tokens and every charge is rounded half-up at nano precision. The bundled
//! catalog keeps resident scans offline; a background worker may refresh a
//! cached copy without touching source events.

use crate::accounting::TokenCounts;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CATALOG_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
pub const FRESH_MS: u64 = 60 * 60 * 1000;
pub const RECHECK_MS: u64 = 60 * 1000;
const MAX_CATALOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CATALOG_MODELS: usize = 512;
const MAX_REFRESH_JITTER_PERMILLE: u16 = 200;
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("TokenMonitorLite/", env!("CARGO_PKG_VERSION"));
const BUNDLED_CATALOG: &str = include_str!("../assets/pricing-catalog.json");

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum PricingError {
    #[error("pricing catalog is invalid")]
    InvalidCatalog,
    #[error("pricing catalog request is not allowed")]
    InvalidUrl,
    #[error("pricing catalog service is unreachable")]
    Network,
    #[error("pricing catalog storage failed")]
    Io,
    #[error("pricing rate is invalid")]
    InvalidRate,
}

/// Integer nano-USD per one million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rate(i64);

impl Rate {
    pub fn from_per_million(text: &str) -> Result<Self, PricingError> {
        parse_decimal_nano(text).map(Self)
    }

    pub fn from_nano(nano: i64) -> Result<Self, PricingError> {
        if nano < 0 {
            return Err(PricingError::InvalidRate);
        }
        Ok(Self(nano))
    }

    pub fn nano(self) -> i64 {
        self.0
    }

    fn charge(self, tokens: u64) -> i128 {
        (i128::from(self.0) * i128::from(tokens) + 500_000) / 1_000_000
    }

    fn to_text(self) -> String {
        let whole = self.0 / 1_000_000_000;
        let mut fraction = format!("{:09}", self.0 % 1_000_000_000);
        while fraction.ends_with('0') {
            fraction.pop();
        }
        if fraction.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{fraction}")
        }
    }
}

/// Parses a non-negative decimal into nano units, accepting exponent notation.
/// All arithmetic is integer; a scale finer than nano is rejected.
fn parse_decimal_nano(text: &str) -> Result<i64, PricingError> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('-') || text.starts_with('+') {
        return Err(PricingError::InvalidRate);
    }
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(index) => {
            let exponent = text[index + 1..]
                .parse::<i32>()
                .map_err(|_| PricingError::InvalidRate)?;
            (&text[..index], exponent)
        }
        None => (text, 0),
    };
    if exponent.abs() > 30 {
        return Err(PricingError::InvalidRate);
    }
    let (whole, fraction) = match mantissa.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (mantissa, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return Err(PricingError::InvalidRate);
    }
    if !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(PricingError::InvalidRate);
    }
    if fraction.len() > 30 {
        return Err(PricingError::InvalidRate);
    }
    let digits = format!("{whole}{fraction}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return Ok(0);
    }
    let value = significant
        .parse::<i128>()
        .map_err(|_| PricingError::InvalidRate)?;
    // nano = value * 10^(9 - fraction.len() + exponent)
    let scale =
        9_i32 - i32::try_from(fraction.len()).map_err(|_| PricingError::InvalidRate)? + exponent;
    let scaled = if scale >= 0 {
        value
            .checked_mul(
                10_i128
                    .checked_pow(u32::try_from(scale).map_err(|_| PricingError::InvalidRate)?)
                    .ok_or(PricingError::InvalidRate)?,
            )
            .ok_or(PricingError::InvalidRate)?
    } else {
        let divisor = 10_i128
            .checked_pow(u32::try_from(-scale).map_err(|_| PricingError::InvalidRate)?)
            .ok_or(PricingError::InvalidRate)?;
        if value % divisor != 0 {
            return Err(PricingError::InvalidRate);
        }
        value / divisor
    };
    i64::try_from(scaled).map_err(|_| PricingError::InvalidRate)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelRates {
    pub input: Option<Rate>,
    pub output: Option<Rate>,
    pub cache_read: Option<Rate>,
    pub cache_write: Option<Rate>,
    pub cache_write_1h: Option<Rate>,
}

impl ModelRates {
    pub fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cache_read.is_none()
            && self.cache_write.is_none()
            && self.cache_write_1h.is_none()
    }

    fn from_json(value: &Value) -> Result<Self, PricingError> {
        let object = value.as_object().ok_or(PricingError::InvalidCatalog)?;
        let mut rates = Self::default();
        for (field, slot) in [
            ("input_per_m", &mut rates.input),
            ("output_per_m", &mut rates.output),
            ("cache_read_per_m", &mut rates.cache_read),
            ("cache_write_per_m", &mut rates.cache_write),
            ("cache_write_1h_per_m", &mut rates.cache_write_1h),
        ] {
            if let Some(value) = object.get(field) {
                *slot = rate_from_value(value)?;
            }
        }
        Ok(rates)
    }

    fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        for (field, value) in [
            ("input_per_m", self.input),
            ("output_per_m", self.output),
            ("cache_read_per_m", self.cache_read),
            ("cache_write_per_m", self.cache_write),
            ("cache_write_1h_per_m", self.cache_write_1h),
        ] {
            if let Some(rate) = value {
                object.insert(field.to_owned(), Value::String(rate.to_text()));
            }
        }
        Value::Object(object)
    }
}

fn rate_from_value(value: &Value) -> Result<Option<Rate>, PricingError> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) if text.is_empty() => Ok(None),
        Value::String(text) => Rate::from_per_million(text).map(Some),
        Value::Number(number) => Rate::from_per_million(&number.to_string()).map(Some),
        _ => Err(PricingError::InvalidRate),
    }
}

/// A custom per-million override scoped to a provider and/or model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomRate {
    pub provider: Option<String>,
    pub model: String,
    pub rates: ModelRates,
}

/// Normalizes `customPricing`-shaped input exactly like the upstream settings
/// boundary: trim ids, drop blank or partially invalid rows, keep explicit
/// zero, deduplicate by (provider, model) with last-wins.
pub fn normalize_custom_rates(value: &Value) -> Vec<CustomRate> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    let mut result: Vec<CustomRate> = Vec::new();
    let mut positions: BTreeMap<(Option<String>, String), usize> = BTreeMap::new();
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let model = object
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let Some(model) = model else {
            continue;
        };
        let provider = object
            .get("provider")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
            .map(|provider| provider.to_ascii_lowercase());
        let mut rates = ModelRates::default();
        let mut invalid = false;
        for (field, slot) in [
            ("inputPerM", &mut rates.input),
            ("outputPerM", &mut rates.output),
            ("cacheReadPerM", &mut rates.cache_read),
            ("cacheWritePerM", &mut rates.cache_write),
            ("cacheWrite1hPerM", &mut rates.cache_write_1h),
        ] {
            match object.get(field) {
                None => {}
                Some(value) => match unit_rate(value) {
                    Ok(rate) => *slot = rate,
                    Err(()) => {
                        invalid = true;
                        break;
                    }
                },
            }
        }
        if invalid || (rates.input.is_none() && rates.output.is_none()) {
            continue;
        }
        let key = (provider.clone(), model.to_owned());
        let normalized = CustomRate {
            provider,
            model: model.to_owned(),
            rates,
        };
        if let Some(index) = positions.get(&key).copied() {
            result[index] = normalized;
        } else {
            positions.insert(key, result.len());
            result.push(normalized);
        }
    }
    result
}

fn unit_rate(value: &Value) -> Result<Option<Rate>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) if text.is_empty() => Ok(None),
        Value::String(text) => Rate::from_per_million(text).map(Some).map_err(|_| ()),
        Value::Number(number) => Rate::from_per_million(&number.to_string())
            .map(Some)
            .map_err(|_| ()),
        _ => Err(()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricingCatalog {
    revision: String,
    models: BTreeMap<String, ModelRates>,
    custom: Vec<CustomRate>,
}

impl PricingCatalog {
    pub fn bundled() -> Arc<Self> {
        static BUNDLED: OnceLock<Arc<PricingCatalog>> = OnceLock::new();
        BUNDLED
            .get_or_init(|| {
                let document: Value = serde_json::from_str(BUNDLED_CATALOG)
                    .expect("the bundled pricing catalog is a build-time constant");
                let revision = document
                    .get("source")
                    .and_then(|source| source.get("revision"))
                    .and_then(Value::as_str)
                    .unwrap_or("bundled");
                let catalog = Self::from_document(&document, &format!("bundled:{revision}"))
                    .expect("the bundled pricing catalog is valid");
                Arc::new(catalog)
            })
            .clone()
    }

    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            revision: "empty".to_owned(),
            models: BTreeMap::new(),
            custom: Vec::new(),
        })
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty() && self.custom.is_empty()
    }

    pub fn with_custom(mut self, custom: Vec<CustomRate>) -> Self {
        self.custom = custom;
        self
    }

    fn from_document(document: &Value, revision: &str) -> Result<Self, PricingError> {
        let models = document
            .get("models")
            .and_then(Value::as_object)
            .ok_or(PricingError::InvalidCatalog)?;
        let mut entries = BTreeMap::new();
        for (model, value) in models {
            let rates = ModelRates::from_json(value)?;
            if !rates.is_empty() {
                entries.insert(model.to_ascii_lowercase(), rates);
            }
        }
        if entries.is_empty() {
            return Err(PricingError::InvalidCatalog);
        }
        Ok(Self {
            revision: revision.to_owned(),
            models: entries,
            custom: Vec::new(),
        })
    }

    /// Builds a catalog from a LiteLLM `model_prices_and_context_window.json`
    /// document. Only canonical `claude-*`/`gpt-*` entries are retained.
    pub fn from_litellm(document: &Value, source_hash: &str) -> Result<Self, PricingError> {
        let object = document.as_object().ok_or(PricingError::InvalidCatalog)?;
        let mut entries = BTreeMap::new();
        for (model, value) in object {
            if model.contains('/') || !(model.starts_with("claude-") || model.starts_with("gpt-")) {
                continue;
            }
            let Some(fields) = value.as_object() else {
                continue;
            };
            let mut rates = ModelRates::default();
            for (field, slot) in [
                ("input_cost_per_token", &mut rates.input),
                ("output_cost_per_token", &mut rates.output),
                ("cache_read_input_token_cost", &mut rates.cache_read),
                ("cache_creation_input_token_cost", &mut rates.cache_write),
                (
                    "cache_creation_input_token_cost_above_1hr",
                    &mut rates.cache_write_1h,
                ),
            ] {
                let Some(value) = fields.get(field) else {
                    continue;
                };
                let Some(per_token) = rate_from_value(value)? else {
                    continue;
                };
                *slot = Some(Rate::from_nano(
                    per_token
                        .nano()
                        .checked_mul(1_000_000)
                        .ok_or(PricingError::InvalidRate)?,
                )?);
            }
            if !rates.is_empty() {
                entries.insert(model.to_ascii_lowercase(), rates);
            }
        }
        if entries.is_empty() {
            return Err(PricingError::InvalidCatalog);
        }
        if entries.len() > MAX_CATALOG_MODELS {
            entries = entries.into_iter().take(MAX_CATALOG_MODELS).collect();
        }
        let count = entries.len();
        Ok(Self {
            revision: format!("litellm:{source_hash}:{count}"),
            models: entries,
            custom: Vec::new(),
        })
    }

    pub fn resolve(&self, provider: Option<&str>, model: &str) -> Option<&ModelRates> {
        let model = normalize_model(model);
        if model.is_empty() {
            return None;
        }
        let provider = provider
            .map(|provider| provider.trim().to_ascii_lowercase())
            .filter(|provider| !provider.is_empty());
        for require_scoped in [true, false] {
            for custom in &self.custom {
                if normalize_model(&custom.model) != model {
                    continue;
                }
                let matches = match (&custom.provider, &provider) {
                    (Some(scoped), Some(provider)) => {
                        require_scoped && scoped.eq_ignore_ascii_case(provider)
                    }
                    (None, _) => !require_scoped,
                    (Some(_), None) => false,
                };
                if matches {
                    return Some(&custom.rates);
                }
            }
        }
        if let Some(rates) = self.models.get(&model) {
            return Some(rates);
        }
        self.models
            .iter()
            .filter(|(candidate, _)| model.starts_with(&format!("{candidate}-")))
            .max_by_key(|(candidate, _)| candidate.len())
            .map(|(_, rates)| rates)
    }

    pub fn cache_path(root: &Path) -> PathBuf {
        root.join("pricing").join("catalog.json")
    }

    pub fn load_cache(root: &Path) -> Option<CachedCatalog> {
        let document: Value =
            serde_json::from_str(&fs::read_to_string(Self::cache_path(root)).ok()?).ok()?;
        let fetched_ms = document.get("fetched_ms")?.as_u64()?;
        let revision = document.get("revision")?.as_str()?.to_owned();
        let catalog = Self::from_document(&document, &revision).ok()?;
        Some(CachedCatalog {
            catalog,
            fetched_ms,
        })
    }

    pub fn load_or_bundled(root: Option<&Path>) -> (Arc<Self>, Option<u64>) {
        match root.and_then(Self::load_cache) {
            Some(cached) => (Arc::new(cached.catalog), Some(cached.fetched_ms)),
            None => (Self::bundled(), None),
        }
    }

    fn write_cache(&self, root: &Path, fetched_ms: u64) -> Result<(), PricingError> {
        let directory = root.join("pricing");
        fs::create_dir_all(&directory).map_err(|_| PricingError::Io)?;
        let models = self
            .models
            .iter()
            .map(|(model, rates)| (model.clone(), rates.to_json()))
            .collect::<serde_json::Map<_, _>>();
        let document = serde_json::json!({
            "fetched_ms": fetched_ms,
            "revision": self.revision,
            "models": Value::Object(models),
        });
        let bytes = serde_json::to_vec(&document).map_err(|_| PricingError::InvalidCatalog)?;
        let temporary = directory.join(format!(".catalog-{}.tmp", unique_suffix()));
        let mut file = fs::File::create(&temporary).map_err(|_| PricingError::Io)?;
        file.write_all(&bytes).map_err(|_| PricingError::Io)?;
        file.sync_all().map_err(|_| PricingError::Io)?;
        fs::rename(&temporary, Self::cache_path(root)).map_err(|_| {
            let _ = fs::remove_file(&temporary);
            PricingError::Io
        })
    }
}

#[derive(Clone, Debug)]
pub struct CachedCatalog {
    pub catalog: PricingCatalog,
    pub fetched_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostProvenance {
    Actual,
    Estimated,
    Free,
    Unpriced,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CostEstimate {
    pub nano_usd: i128,
    pub unpriced_tokens: u64,
    pub provenance: Option<CostProvenance>,
}

impl CostEstimate {
    pub fn free() -> Self {
        Self {
            nano_usd: 0,
            unpriced_tokens: 0,
            provenance: Some(CostProvenance::Free),
        }
    }

    pub fn unpriced(tokens: u64) -> Self {
        Self {
            nano_usd: 0,
            unpriced_tokens: tokens,
            provenance: Some(CostProvenance::Unpriced),
        }
    }

    pub fn merge(self, other: Self) -> Self {
        let nano_usd = self.nano_usd.saturating_add(other.nano_usd);
        let unpriced_tokens = self.unpriced_tokens.saturating_add(other.unpriced_tokens);
        let provenance = if self.provenance == Some(CostProvenance::Actual)
            || other.provenance == Some(CostProvenance::Actual)
        {
            CostProvenance::Actual
        } else if unpriced_tokens > 0 {
            CostProvenance::Unpriced
        } else if nano_usd == 0 {
            CostProvenance::Free
        } else {
            CostProvenance::Estimated
        };
        Self {
            nano_usd,
            unpriced_tokens,
            provenance: Some(provenance),
        }
    }

    pub fn format(&self) -> String {
        if self.unpriced_tokens > 0 {
            if self.nano_usd > 0 {
                format!("{} + ?", format_money(self.nano_usd))
            } else {
                "—".to_owned()
            }
        } else {
            format_money(self.nano_usd)
        }
    }
}

/// Prices one captured row. `source_nano` is a provider-reported cost; when
/// present it is preserved even if the catalog covers only part of the usage.
pub fn estimate(
    rates: Option<&ModelRates>,
    tokens: TokenCounts,
    cache_write_1h: u64,
    source_nano: Option<i128>,
) -> CostEstimate {
    let mut nano_usd = 0_i128;
    let mut unpriced = 0_u64;
    let mut charge = |tokens: u64, rate: Option<Rate>| {
        if tokens == 0 {
            return;
        }
        match rate {
            Some(rate) => nano_usd = nano_usd.saturating_add(rate.charge(tokens)),
            None => unpriced = unpriced.saturating_add(tokens),
        }
    };
    let one_hour = cache_write_1h.min(tokens.cache_write);
    let short_write = tokens.cache_write - one_hour;
    charge(tokens.input, rates.and_then(|rates| rates.input));
    charge(tokens.output, rates.and_then(|rates| rates.output));
    // Reasoning is a disjoint output bucket for the Codex reader and zero for
    // Claude; it is billed at the output rate.
    charge(tokens.reasoning, rates.and_then(|rates| rates.output));
    charge(tokens.cache_read, rates.and_then(|rates| rates.cache_read));
    charge(short_write, rates.and_then(|rates| rates.cache_write));
    charge(
        one_hour,
        rates.and_then(|rates| rates.cache_write_1h.or(rates.cache_write)),
    );
    if let Some(source_nano) = source_nano.filter(|cost| *cost >= 0) {
        return CostEstimate {
            nano_usd: source_nano,
            unpriced_tokens: unpriced,
            provenance: Some(CostProvenance::Actual),
        };
    }
    let provenance = if unpriced > 0 {
        CostProvenance::Unpriced
    } else if nano_usd == 0 {
        CostProvenance::Free
    } else {
        CostProvenance::Estimated
    };
    CostEstimate {
        nano_usd,
        unpriced_tokens: unpriced,
        provenance: Some(provenance),
    }
}

pub fn format_money(nano_usd: i128) -> String {
    let nano = u128::try_from(nano_usd.max(0)).unwrap_or(0);
    let micro = (nano + 500) / 1_000;
    let whole = micro / 1_000_000;
    let mut fraction = format!("{:06}", micro % 1_000_000);
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${whole}.{fraction}")
}

fn normalize_model(model: &str) -> String {
    let mut model = model.trim().to_ascii_lowercase();
    if let Some(index) = model.rfind('/') {
        model = model[index + 1..].to_owned();
    }
    if let Some(index) = model.find('(') {
        model.truncate(index);
    }
    let model = model.trim().trim_end_matches('-');
    if let Some((base, date)) = model.rsplit_once('-')
        && date.len() == 8
        && date.bytes().all(|byte| byte.is_ascii_digit())
    {
        return base.to_owned();
    }
    model.to_owned()
}

pub fn next_delay(fresh: bool, failures: u32, jitter_permille: u16) -> Duration {
    let base_ms = if fresh || failures == 0 {
        RECHECK_MS
    } else {
        let exponent = (failures - 1).min(3);
        300_000_u64.saturating_mul(1_u64 << exponent).min(1_800_000)
    };
    let jitter =
        base_ms.saturating_mul(u64::from(jitter_permille.min(MAX_REFRESH_JITTER_PERMILLE))) / 1000;
    Duration::from_millis(base_ms.saturating_add(jitter))
}

pub trait PricingTransport: Send + Sync {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, PricingError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HttpsPricingTransport;

impl PricingTransport for HttpsPricingTransport {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, PricingError> {
        if url != CATALOG_URL {
            return Err(PricingError::InvalidUrl);
        }
        let response = agent().get(url).call().map_err(|_| PricingError::Network)?;
        if !response.status().is_success() {
            return Err(PricingError::Network);
        }
        let mut response = response;
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|_| PricingError::Network)
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(CHECK_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .new_agent()
    })
}

/// One refresh attempt. Returns a new catalog only when the revision changed.
pub fn refresh_once(
    transport: &dyn PricingTransport,
    root: &Path,
    previous: &PricingCatalog,
    now_ms: u64,
) -> Result<Option<Arc<PricingCatalog>>, PricingError> {
    if let Some(cached) = PricingCatalog::load_cache(root)
        && now_ms.saturating_sub(cached.fetched_ms) < FRESH_MS
    {
        return Ok((cached.catalog.revision != previous.revision).then(|| Arc::new(cached.catalog)));
    }
    let bytes = transport.get(CATALOG_URL, MAX_CATALOG_BYTES)?;
    let hash = sha256_hex(&bytes);
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|_| PricingError::InvalidCatalog)?;
    let catalog = PricingCatalog::from_litellm(&document, &hash)?;
    catalog.write_cache(root, now_ms)?;
    Ok((catalog.revision != previous.revision).then(|| Arc::new(catalog)))
}

#[derive(Debug)]
pub struct RefreshHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RefreshHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for RefreshHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts the catalog worker. It never blocks source scans and publishes only
/// completed revisions.
pub fn spawn(
    root: PathBuf,
    on_update: impl Fn(Arc<PricingCatalog>) + Send + 'static,
) -> RefreshHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let join = std::thread::spawn(move || {
        let mut previous = PricingCatalog::load_cache(&root)
            .map(|cached| Arc::new(cached.catalog))
            .unwrap_or_else(PricingCatalog::bundled);
        let mut failures = 0_u32;
        while !thread_stop.load(Ordering::Relaxed) {
            let fetched_ms = PricingCatalog::load_cache(&root).map(|cached| cached.fetched_ms);
            let fresh =
                fetched_ms.is_some_and(|fetched| now_ms().saturating_sub(fetched) < FRESH_MS);
            let delay = next_delay(fresh, failures, refresh_jitter());
            if !sleep_unless_stopped(&thread_stop, delay) {
                break;
            }
            match refresh_once(&HttpsPricingTransport, &root, &previous, now_ms()) {
                Ok(Some(catalog)) => {
                    previous = catalog.clone();
                    failures = 0;
                    on_update(catalog);
                }
                Ok(None) => failures = 0,
                Err(_) => failures = failures.saturating_add(1),
            }
        }
    });
    RefreshHandle {
        stop,
        join: Some(join),
    }
}

fn sleep_unless_stopped(stop: &AtomicBool, delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    !stop.load(Ordering::Relaxed)
}

fn refresh_jitter() -> u16 {
    u16::try_from(now_nanos() % u128::from(MAX_REFRESH_JITTER_PERMILLE + 1)).unwrap_or(0)
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
}

fn unique_suffix() -> String {
    format!("{:020}-{}", now_nanos(), std::process::id())
}

#[cfg(test)]
mod tests {
    use super::{
        CATALOG_URL, CostEstimate, CostProvenance, CustomRate, FRESH_MS, ModelRates,
        PricingCatalog, PricingError, PricingTransport, Rate, estimate, format_money, next_delay,
        normalize_custom_rates, normalize_model, parse_decimal_nano, refresh_once,
    };
    use crate::accounting::TokenCounts;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CUSTOM_FIXTURE: &str =
        include_str!("../tests/fixtures/pricing/custom_normalization.json");
    const LITELLM_FIXTURE: &str = include_str!("../tests/fixtures/pricing/litellm_sample.json");

    struct FakeTransport {
        bytes: Vec<u8>,
        calls: AtomicUsize,
    }

    impl FakeTransport {
        fn new(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                bytes: bytes.into(),
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl PricingTransport for FakeTransport {
        fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, PricingError> {
            assert_eq!(url, CATALOG_URL);
            self.calls.fetch_add(1, Ordering::Relaxed);
            (self.bytes.len() as u64 <= limit)
                .then(|| self.bytes.clone())
                .ok_or(PricingError::Network)
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-pricing-{label}-{}-{}",
            std::process::id(),
            super::unique_suffix()
        ));
        std::fs::create_dir_all(&path).expect("mkdir");
        path
    }

    fn tokens(input: u64, output: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            ..TokenCounts::default()
        }
    }

    fn rate(text: &str) -> Rate {
        Rate::from_per_million(text).expect("rate")
    }

    fn comparable(rates: &[CustomRate]) -> Vec<(Option<String>, String, [Option<i64>; 5])> {
        rates
            .iter()
            .map(|custom| {
                (
                    custom.provider.clone(),
                    custom.model.clone(),
                    [
                        custom.rates.input.map(Rate::nano),
                        custom.rates.output.map(Rate::nano),
                        custom.rates.cache_read.map(Rate::nano),
                        custom.rates.cache_write.map(Rate::nano),
                        custom.rates.cache_write_1h.map(Rate::nano),
                    ],
                )
            })
            .collect()
    }

    fn expected(value: &Value) -> Vec<(Option<String>, String, [Option<i64>; 5])> {
        value
            .as_array()
            .expect("array")
            .iter()
            .map(|row| {
                let model = row["modelId"].as_str().expect("model").to_owned();
                let provider = row
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let field = |name: &str| -> Option<i64> {
                    row.get(name).and_then(|value| match value {
                        Value::String(text) => Rate::from_per_million(text).ok().map(Rate::nano),
                        Value::Number(number) => Rate::from_per_million(&number.to_string())
                            .ok()
                            .map(Rate::nano),
                        Value::Null => None,
                        _ => None,
                    })
                };
                (
                    provider,
                    model,
                    [
                        field("inputPerM"),
                        field("outputPerM"),
                        field("cacheReadPerM"),
                        field("cacheWritePerM"),
                        field("cacheWrite1hPerM"),
                    ],
                )
            })
            .collect()
    }

    #[test]
    fn decimal_rates_parse_exactly_and_reject_finer_than_nano() {
        assert_eq!(parse_decimal_nano("3"), Ok(3_000_000_000));
        assert_eq!(parse_decimal_nano("3.75"), Ok(3_750_000_000));
        assert_eq!(parse_decimal_nano("0.00175"), Ok(1_750_000));
        assert_eq!(parse_decimal_nano("1.75e-6"), Ok(1_750));
        assert_eq!(parse_decimal_nano("0"), Ok(0));
        assert_eq!(
            parse_decimal_nano("0.0000000001"),
            Err(PricingError::InvalidRate)
        );
        assert_eq!(parse_decimal_nano("-1"), Err(PricingError::InvalidRate));
        assert_eq!(parse_decimal_nano("nan"), Err(PricingError::InvalidRate));
    }

    #[test]
    fn bundled_catalog_prices_supported_models() {
        let catalog = PricingCatalog::bundled();
        let sonnet = catalog
            .resolve(Some("claude"), "claude-sonnet-4-5-20250929")
            .expect("sonnet");
        assert_eq!(sonnet.input.map(Rate::nano), Some(3_000_000_000));
        assert_eq!(sonnet.output.map(Rate::nano), Some(15_000_000_000));
        assert_eq!(sonnet.cache_read.map(Rate::nano), Some(300_000_000));
        assert_eq!(sonnet.cache_write.map(Rate::nano), Some(3_750_000_000));
        assert_eq!(sonnet.cache_write_1h.map(Rate::nano), Some(6_000_000_000));
        let gpt = catalog.resolve(Some("codex"), "gpt-5").expect("gpt-5");
        assert_eq!(gpt.input.map(Rate::nano), Some(1_250_000_000));
        assert_eq!(gpt.output.map(Rate::nano), Some(10_000_000_000));
        assert_eq!(gpt.cache_read.map(Rate::nano), Some(125_000_000));
        assert!(catalog.resolve(Some("claude"), "unknown-model").is_none());
    }

    #[test]
    fn cost_uses_exact_per_million_math() {
        let rates = PricingCatalog::bundled()
            .resolve(Some("claude"), "claude-sonnet-4-5")
            .cloned()
            .expect("rates");
        let estimate = estimate(Some(&rates), tokens(1_000_000, 100_000), 0, None);
        assert_eq!(estimate.nano_usd, 3_000_000_000 + 1_500_000_000);
        assert_eq!(estimate.unpriced_tokens, 0);
        assert_eq!(estimate.provenance, Some(CostProvenance::Estimated));
        assert_eq!(estimate.format(), "$4.50");
    }

    #[test]
    fn cache_write_one_hour_is_a_subset_at_its_own_rate() {
        let rates = PricingCatalog::bundled()
            .resolve(Some("claude"), "claude-sonnet-4-5")
            .cloned()
            .expect("rates");
        let counts = TokenCounts {
            cache_write: 1_000_000,
            ..TokenCounts::default()
        };
        let mixed = estimate(Some(&rates), counts, 400_000, None);
        assert_eq!(mixed.nano_usd, 2_250_000_000 + 2_400_000_000);
        let empty = TokenCounts::default();
        assert_eq!(
            estimate(Some(&rates), empty, 0, None).provenance,
            Some(CostProvenance::Free)
        );
    }

    #[test]
    fn reasoning_bills_at_the_output_rate() {
        let rates = ModelRates {
            output: Some(rate("10")),
            ..ModelRates::default()
        };
        let counts = TokenCounts {
            reasoning: 1_000_000,
            ..TokenCounts::default()
        };
        let estimate = estimate(Some(&rates), counts, 0, None);
        assert_eq!(estimate.nano_usd, 10_000_000_000);
        assert_eq!(estimate.provenance, Some(CostProvenance::Estimated));
    }

    #[test]
    fn incomplete_coverage_keeps_the_known_subtotal() {
        let rates = ModelRates {
            input: Some(rate("3")),
            ..ModelRates::default()
        };
        let partial = estimate(Some(&rates), tokens(1_000_000, 1_000_000), 0, None);
        assert_eq!(partial.nano_usd, 3_000_000_000);
        assert_eq!(partial.unpriced_tokens, 1_000_000);
        assert_eq!(partial.provenance, Some(CostProvenance::Unpriced));
        assert_eq!(partial.format(), "$3.00 + ?");
        let unknown = estimate(None, tokens(500, 500), 0, None);
        assert_eq!(unknown.provenance, Some(CostProvenance::Unpriced));
        assert_eq!(unknown.format(), "—");
    }

    #[test]
    fn source_cost_is_preserved_as_actual() {
        let rates = ModelRates {
            input: Some(rate("3")),
            ..ModelRates::default()
        };
        let estimate = estimate(Some(&rates), tokens(1_000_000, 1_000_000), 0, Some(42));
        assert_eq!(estimate.nano_usd, 42);
        assert_eq!(estimate.provenance, Some(CostProvenance::Actual));
        assert_eq!(estimate.unpriced_tokens, 1_000_000);
    }

    #[test]
    fn explicit_zero_rates_are_free_while_unset_rates_are_unpriced() {
        let zero = ModelRates {
            input: Some(rate("0")),
            output: Some(rate("0")),
            ..ModelRates::default()
        };
        let free = estimate(Some(&zero), tokens(10, 10), 0, None);
        assert_eq!(free.provenance, Some(CostProvenance::Free));
        assert_eq!(free.format(), "$0.00");
        let unpriced = estimate(Some(&ModelRates::default()), tokens(10, 10), 0, None);
        assert_eq!(unpriced.provenance, Some(CostProvenance::Unpriced));
        assert_eq!(unpriced.unpriced_tokens, 20);
    }

    #[test]
    fn custom_rates_are_provider_scoped_before_model_only_entries() {
        let catalog = PricingCatalog::bundled().as_ref().clone().with_custom(vec![
            CustomRate {
                provider: None,
                model: "claude-sonnet-4-5".to_owned(),
                rates: ModelRates {
                    input: Some(rate("1")),
                    ..ModelRates::default()
                },
            },
            CustomRate {
                provider: Some("claude".to_owned()),
                model: "claude-sonnet-4-5".to_owned(),
                rates: ModelRates {
                    input: Some(rate("2")),
                    ..ModelRates::default()
                },
            },
        ]);
        let scoped = catalog
            .resolve(Some("claude"), "claude-sonnet-4-5")
            .expect("scoped");
        assert_eq!(scoped.input.map(Rate::nano), Some(2_000_000_000));
        let other = catalog
            .resolve(Some("codex"), "claude-sonnet-4-5")
            .expect("generic");
        assert_eq!(other.input.map(Rate::nano), Some(1_000_000_000));
    }

    #[test]
    fn custom_normalization_matches_the_upstream_cases() {
        let document: Value = serde_json::from_str(CUSTOM_FIXTURE).expect("fixture");
        for case in [
            "normalize",
            "dedupe_last_wins",
            "explicit_zero_cache_read",
            "invalid_rows_are_dropped",
        ] {
            let normalized = normalize_custom_rates(&document[case]["input"]);
            assert_eq!(
                comparable(&normalized),
                expected(&document[case]["expected"]),
                "{case}"
            );
        }
    }

    #[test]
    fn litellm_documents_convert_per_token_rates_and_filter_aliases() {
        let document: Value = serde_json::from_str(LITELLM_FIXTURE).expect("fixture");
        let catalog = PricingCatalog::from_litellm(&document, "abc").expect("catalog");
        assert_eq!(catalog.revision(), "litellm:abc:2");
        let sonnet = catalog
            .resolve(Some("claude"), "claude-sonnet-4-5")
            .expect("sonnet");
        assert_eq!(sonnet.input.map(Rate::nano), Some(3_000_000_000));
        assert_eq!(sonnet.cache_write_1h.map(Rate::nano), Some(6_000_000_000));
        assert!(catalog.resolve(Some("openai"), "gpt-5").is_some());
        assert!(catalog.resolve(None, "sample_spec").is_none());
    }

    #[test]
    fn model_ids_normalize_prefixes_tiers_and_dates() {
        assert_eq!(
            normalize_model("anthropic/Claude-Sonnet-4-5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model("gpt-5(high)"), "gpt-5");
        assert_eq!(
            normalize_model("claude-sonnet-4-5-20250929"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model("  GPT-5  "), "gpt-5");
    }

    #[test]
    fn refresh_backoff_matches_the_upstream_schedule() {
        assert_eq!(next_delay(true, 3, 0).as_secs(), 60);
        assert_eq!(next_delay(false, 0, 0).as_secs(), 60);
        for (failures, seconds) in [(1, 300), (2, 600), (3, 1200), (4, 1800), (5, 1800)] {
            assert_eq!(next_delay(false, failures, 0).as_secs(), seconds);
        }
        assert_eq!(next_delay(false, 1, 200).as_secs(), 360);
    }

    #[test]
    fn refresh_writes_a_fresh_cache_and_skips_downloads_while_fresh() {
        let root = temp_root("refresh");
        let transport = FakeTransport::new(LITELLM_FIXTURE.as_bytes());
        let previous = PricingCatalog::empty();
        let published = refresh_once(&transport, &root, &previous, 1_000_000)
            .expect("refresh")
            .expect("changed");
        assert_eq!(transport.calls.load(Ordering::Relaxed), 1);
        let cached = PricingCatalog::load_cache(&root).expect("cache");
        assert_eq!(cached.fetched_ms, 1_000_000);
        assert_eq!(cached.catalog.revision(), published.revision());
        let cached_rates = cached
            .catalog
            .resolve(Some("claude"), "claude-sonnet-4-5")
            .expect("rates");
        assert_eq!(cached_rates.input.map(Rate::nano), Some(3_000_000_000));
        let quiet = FakeTransport::new(LITELLM_FIXTURE.as_bytes());
        let unchanged =
            refresh_once(&quiet, &root, &published, 1_000_000 + FRESH_MS - 1).expect("fresh");
        assert!(unchanged.is_none());
        assert_eq!(quiet.calls.load(Ordering::Relaxed), 0);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn invalid_refresh_payload_preserves_the_previous_catalog() {
        let root = temp_root("invalid");
        let transport = FakeTransport::new(b"not json".to_vec());
        assert_eq!(
            refresh_once(&transport, &root, &PricingCatalog::empty(), 1),
            Err(PricingError::InvalidCatalog)
        );
        assert!(PricingCatalog::load_cache(&root).is_none());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn money_formatting_trims_to_six_decimals_with_a_minimum_of_two() {
        assert_eq!(format_money(3_000_000_000), "$3.00");
        assert_eq!(format_money(270_000), "$0.00027");
        assert_eq!(format_money(1_500_000), "$0.0015");
        assert_eq!(format_money(0), "$0.00");
        assert_eq!(CostEstimate::free().format(), "$0.00");
        assert_eq!(CostEstimate::unpriced(7).format(), "—");
    }

    #[test]
    fn custom_normalization_accepts_decimal_strings_exactly() {
        let value: Value = serde_json::json!([
            {"modelId": "m", "inputPerM": "0.1", "outputPerM": "0.2", "cacheWrite1hPerM": "0.003"}
        ]);
        let normalized = normalize_custom_rates(&value);
        assert_eq!(
            comparable(&normalized),
            vec![(
                None,
                "m".to_owned(),
                [
                    Some(100_000_000),
                    Some(200_000_000),
                    None,
                    None,
                    Some(3_000_000)
                ]
            )]
        );
    }
}
