//! CatPaw assistant local usage, adapted from the pinned Token Monitor fork
//! reader `crates/tokscale-core/src/token_monitor/catpaw.rs`. See
//! docs/migration/parser-provenance.md.
//!
//! CatPaw is one opt-in, fork-only client covering two editions — domestic
//! `catpaw-moon` and overseas `catpaw-overseas` — whose account databases sit
//! directly under the platform app-data root. Discovery admits only immediate
//! `catpaw-memory-<scope>.db` files with a non-empty `[A-Za-z0-9_-]` scope; the
//! exact scope `anon` is excluded, and non-anonymous historical account
//! databases are kept because that is this machine's retained history rather
//! than the current login's usage.
//!
//! Usage is projected from `ui_sdk_messages` joined to `sessions` for the
//! session's model selection. The four buckets are **disjoint and additive**:
//! `promptTokens` is uncached input, and `cacheReadTokens`/`cacheWriteTokens`
//! are separate, so nothing is subtracted. Reasoning and one-hour cache-write
//! tokens are not independently persisted and stay zero rather than being
//! inferred from a residual or a duration.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//!
//! - the fork's whole-snapshot cache with its `db`/`wal` size+mtime
//!   fingerprint, the `catpaw.lock` sidecar, and cross-scan publication
//!   ordering. Those are collector concerns; this reader is per-source.
//! - the `unpriced:catpaw` provider marker. An unmapped, auto, routing,
//!   conflicting or unknown model id is reported verbatim with no provider
//!   hint and no cost, so catalog pricing cannot silently apply to it.
//! - `inferred_provider_from_model` for mapped models.

use super::{CostStatus, ReaderError, ResponseUsage, jsonl::MAX_RECORD_BYTES};
use crate::accounting::{SessionKey, TokenCounts};
use serde_json::Value;

pub const MAX_RESPONSES: usize = 16_384;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "catpaw";

/// Which CatPaw edition a database belongs to.
///
/// Model ids are edition-specific namespaces, so a name/id pair observed for one
/// edition must never transfer to the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edition {
    Domestic,
    Overseas,
}

impl Edition {
    pub fn directory(self) -> &'static str {
        match self {
            Self::Domestic => "catpaw-moon",
            Self::Overseas => "catpaw-overseas",
        }
    }

    pub fn from_database_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("catpaw-moon") {
            return Some(Self::Domestic);
        }
        if name.eq_ignore_ascii_case("catpaw-overseas") {
            return Some(Self::Overseas);
        }
        None
    }

    /// The edition's documented model-id snapshot. `None` means the id is not
    /// mapped, which the caller reports verbatim instead of guessing a catalog
    /// model.
    pub fn model(self, id: i64) -> Option<&'static str> {
        match (self, id) {
            (_, 0) | (Self::Overseas, 10000003) => Some("auto"),
            (Self::Domestic, 63) => Some("deepseek-v4-flash"),
            (Self::Domestic, 64) => Some("deepseek-v4-pro"),
            (Self::Domestic, 70) => Some("MiniMax-M3"),
            (Self::Domestic, 77) | (Self::Overseas, 10000002) => Some("LongCat-2.0"),
            (Self::Domestic, 83) | (Self::Overseas, 10000007) => Some("kimi-k3"),
            (Self::Domestic, 89) | (Self::Overseas, 10000005) => Some("glm-5.3"),
            (Self::Domestic, 91) | (Self::Overseas, 10000006) => Some("glm-5.3-flash"),
            (Self::Domestic, 98) => Some("glm-5.3-flashx"),
            (Self::Overseas, 10000001) => Some("gpt-5.6-terra"),
            _ => None,
        }
    }
}

/// One `ui_sdk_messages` usage row, already joined to its session selection.
pub struct CatpawRow<'a> {
    pub conversation_id: &'a str,
    pub message_id: &'a str,
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub schema_version: i64,
    /// The `payload.extra.contextInfo.usage` JSON object.
    pub usage: &'a [u8],
    /// The sibling `contextInfo.totalUsageTokens`, when it is a number.
    pub recorded_total: Option<i64>,
    /// The session's model-selection JSON, which may be absent.
    pub selection: Option<&'a [u8]>,
    /// The database file name, which namespaces the session identity.
    pub namespace: &'a str,
}

/// One CatPaw source database.
pub struct CatpawReader {
    edition: Edition,
    responses: Vec<ResponseUsage>,
    retained_identity_bytes: usize,
}

impl CatpawReader {
    pub fn new(edition: Edition) -> Self {
        Self {
            edition,
            responses: Vec::new(),
            retained_identity_bytes: 0,
        }
    }

    pub fn ingest(&mut self, row: CatpawRow<'_>) -> Result<(), ReaderError> {
        if row.usage.len() > MAX_RECORD_BYTES {
            return Err(ReaderError::Capacity("CatPaw usage"));
        }
        // A schema this reader does not know is not silently interpreted.
        if row.schema_version != 1 {
            return Err(ReaderError::Unsupported("CatPaw message schema"));
        }
        if row.conversation_id.is_empty() || row.message_id.is_empty() {
            return Err(ReaderError::InvalidRecord);
        }
        let usage: Value =
            serde_json::from_slice(row.usage).map_err(|_| ReaderError::InvalidRecord)?;
        let tokens = tokens(&usage, row.recorded_total)?;
        let selection: Value = match row.selection {
            Some(bytes) => serde_json::from_slice(bytes).map_err(|_| ReaderError::InvalidRecord)?,
            None => Value::Object(Default::default()),
        };
        let (model, mapped) = resolve_model(self.edition, &selection);
        if !valid_identity(&model) {
            return Err(ReaderError::InvalidRecord);
        }
        let timestamp_ms = resolve_timestamp(row.created_at_ms, row.updated_at_ms);
        let session = format!("catpaw:{}:{}", row.namespace, row.conversation_id);
        let identity = format!("{session}:{}", row.message_id);
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::Unsupported("CatPaw message identity"));
        }
        let usage = ResponseUsage {
            key: SessionKey::new(CLIENT, &session),
            model,
            tokens,
            cache_write_1h: 0,
            timestamp_ms,
            dedup_key: Some(identity),
            // A mapped model keeps the client as its provider; an unmapped one
            // gets no hint at all, so automatic catalog pricing cannot apply.
            billing_provider_hint: mapped.then(|| CLIENT.to_owned()),
            service_tier: None,
            agent: None,
            workspace_key: None,
            workspace_label: None,
            title: None,
            cost: CostStatus::Unavailable,
        };
        self.push(usage)
    }

    fn push(&mut self, usage: ResponseUsage) -> Result<(), ReaderError> {
        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("CatPaw responses"));
        }
        let identity_bytes = usage.dedup_key.as_ref().map_or(0, String::len);
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(identity_bytes)
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("CatPaw identities"))?;
        self.responses.push(usage);
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        self.responses.clone()
    }

    pub fn has_supported_usage(&self) -> bool {
        !self.responses.is_empty()
    }
}

/// The four disjoint buckets, anchored to one of the two recorded totals.
///
/// A row is accepted when the bucket sum equals `usage.totalTokens`, or the
/// sibling `totalUsageTokens` — older builds stored the first without the
/// cache-read part, which is why two anchors exist and either may apply.
fn tokens(usage: &Value, recorded_total: Option<i64>) -> Result<TokenCounts, ReaderError> {
    let Some(object) = usage.as_object() else {
        return Err(ReaderError::InvalidRecord);
    };
    let tokens = TokenCounts {
        input: counter(object, "promptTokens")?,
        output: counter(object, "completionTokens")?,
        cache_read: counter(object, "cacheReadTokens")?,
        cache_write: counter(object, "cacheWriteTokens")?,
        reasoning: 0,
    };
    let total = tokens
        .input
        .checked_add(tokens.output)
        .and_then(|value| value.checked_add(tokens.cache_read))
        .and_then(|value| value.checked_add(tokens.cache_write))
        .ok_or(ReaderError::Overflow)?;
    let declared = counter(object, "totalTokens")?;
    let anchored = total == declared
        || recorded_total.is_some_and(|recorded| i64::try_from(total).is_ok_and(|t| t == recorded));
    if !anchored {
        // An unanchored sum means the buckets and the stored total disagree, so
        // the row cannot be interpreted.
        return Err(ReaderError::Unsupported("CatPaw token sum"));
    }
    Ok(tokens)
}

/// A bucket value. Only a nonnegative integral number is accepted.
fn counter(object: &serde_json::Map<String, Value>, key: &str) -> Result<u64, ReaderError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(ReaderError::InvalidRecord),
    }
}

/// Resolve the model from the session's selection.
///
/// The current selection (`persistedModel*`) wins over the legacy initial
/// fields, and `auto`, routing tiers, conflicting and unmapped ids keep an
/// edition-qualified label so they are never priced as a real model.
fn resolve_model(edition: Edition, extra: &Value) -> (String, bool) {
    let Some(extra) = extra.as_object() else {
        return (format!("{}-model-unknown", edition.directory()), false);
    };
    let has_current = [
        "persistedModelId",
        "persistedModelMode",
        "persistedModelSelection",
    ]
    .iter()
    .any(|key| extra.get(*key).is_some_and(|value| !value.is_null()));
    let (id_key, mode_key, selection_key) = if has_current {
        (
            "persistedModelId",
            "persistedModelMode",
            "persistedModelSelection",
        )
    } else {
        (
            "initialSelectedModelId",
            "initialModelMode",
            "initialModelSelection",
        )
    };
    let selection = extra.get(selection_key).cloned().unwrap_or(Value::Null);
    let mode = extra.get(mode_key).and_then(Value::as_str);
    let persisted = extra.get(id_key).and_then(Value::as_i64);
    // The app rejects a `lastSelectedModelId` that is not a safe integer, and a
    // partial selection without `isAuto` is not a selection at all.
    let safe_model_id = |value: &Value| {
        value
            .as_i64()
            .filter(|id| id.unsigned_abs() <= MAX_SAFE_INTEGER as u64)
    };
    let selected = safe_model_id(&selection["modelId"]);
    let selection_auto = selection["isAuto"].as_bool().filter(|_| {
        selected.is_some()
            && selection
                .get("lastSelectedModelId")
                .is_none_or(|value| safe_model_id(value).is_some())
    });
    if selection_auto == Some(true) || (selection_auto.is_none() && mode == Some("auto")) {
        return ("auto".to_owned(), false);
    }
    if matches!((persisted, selected), (Some(a), Some(b)) if a != b) {
        return (format!("{}-model-conflict", edition.directory()), false);
    }
    let id = selected
        .filter(|_| selection_auto.is_some())
        .or(persisted)
        .or(match mode {
            Some("lite") => Some(10001),
            Some("pro") => Some(10002),
            Some("max") => Some(10003),
            _ => None,
        });
    match id {
        Some(id) => match edition.model(id) {
            Some("auto") => ("auto".to_owned(), false),
            Some(name) => (name.to_owned(), true),
            None => (format!("{}-model-{id}", edition.directory()), false),
        },
        None => (format!("{}-model-unknown", edition.directory()), false),
    }
}

/// `created_at_ms`, falling back to `updated_at_ms`.
///
/// A row with neither, or with an out-of-range value, is stored at the epoch —
/// matching the pinned reader, whose outer window then excludes it.
fn resolve_timestamp(created: Option<i64>, updated: Option<i64>) -> i64 {
    let valid = |value: &i64| *value > 0 && *value <= 8_640_000_000_000_000;
    created
        .filter(valid)
        .or_else(|| updated.filter(valid))
        .unwrap_or(0)
}

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_BYTES && !value.chars().any(char::is_control)
}
