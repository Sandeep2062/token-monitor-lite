//! Command Code session normalization adapted from the pinned Tokscale
//! `commandcode.rs` reader. MIT © 2025 Junho Yeo; see
//! docs/migration/parser-provenance.md.
//!
//! Command Code's version-3 transcript is a stream of typed JSON records, one
//! per line, under `~/.commandcode/projects/<slug>/<session>.jsonl`:
//!
//! - `type: "session"` is the header and carries the session id.
//! - `type: "message"` carries the conversation entry. Assistant responses
//!   carry an authoritative `usage` block (`inputTokens`, `outputTokens`,
//!   `cacheReadTokens`, `cacheWriteTokens`, `costUsd`) and the model in effect.
//! - `type: "model_change"` records a mid-session model switch and emits
//!   nothing itself.
//!
//! The usage buckets are DISJOINT: `inputTokens` is cache-exclusive and the
//! cache buckets are billed on top of it, so no subtraction is applied. That
//! reproduces the vendor's own recorded `costUsd` arithmetic exactly.
//!
//! The transcript is a tree, not a log. `/rewind` moves the leaf pointer
//! backwards while the writer stays append-only, so abandoned assistant entries
//! keep their usage on disk without being on the active branch; the vendor
//! counts only the parent chain from the last entry, and so does this reader.
//! When that chain cannot be reconstructed — a skipped line removes an ancestor
//! — the filter is refused and every message is kept, because truncating at the
//! hole would silently discard already-billed usage.
//!
//! Not implemented, and recorded as unsupported coverage rather than guessed:
//! the ~4-chars-per-token estimation that the pinned reader applies to legacy
//! flat-schema transcripts that record no usage at all. Inventing token counts
//! from message text is exactly the kind of unverifiable number this reader
//! refuses to fabricate.

use super::{CostStatus, ReaderError, ResponseUsage};
use crate::accounting::{SessionKey, TokenCounts};
use crate::parsers::timestamps::parse_timestamp_str;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub const MAX_RESPONSES: usize = 65_536;
const MAX_IDENTITY_BYTES: usize = 4_096;
const MAX_RETAINED_IDENTITY_BYTES: usize = 8 * 1024 * 1024;

pub const CLIENT: &str = "commandcode";
pub const PROVIDER: &str = "command-code";
pub const UNKNOWN_MODEL: &str = "unknown";

/// No single request bills more than this, and no token bucket in a real
/// session reaches it. Values beyond are rejected rather than clamped, so a
/// tampered line cannot slip a fabricated number into an aggregate.
const TOKEN_CEILING: u64 = 1_000_000_000_000;
const COST_CEILING_USD: f64 = 1_000_000.0;

pub struct CommandCodeReader {
    session: String,
    fallback_timestamp_ms: Option<i64>,
    workspace_key: Option<String>,
    workspace_label: Option<String>,
    /// The configured agent model, read from the Command Code root's
    /// `config.json` by the caller, since a byte-slice reader cannot reach a
    /// sibling file itself.
    config_model: Option<String>,
    /// The model most recently named by a `message` or `model_change` record.
    model_id: Option<String>,
    /// The `session` header's id, which wins over the path stem.
    header_session: Option<String>,
    responses: Vec<Candidate>,
    parent_map: HashMap<String, Option<String>>,
    last_entry_id: Option<String>,
    retained_identity_bytes: usize,
}

/// One emitted observation together with the entry id that placed it in the
/// transcript tree. Legacy flat-schema lines carry no id and are always kept.
struct Candidate {
    entry_id: Option<String>,
    response: ResponseUsage,
}

impl CommandCodeReader {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            fallback_timestamp_ms: None,
            workspace_key: None,
            workspace_label: None,
            config_model: None,
            model_id: None,
            header_session: None,
            responses: Vec::new(),
            parent_map: HashMap::new(),
            last_entry_id: None,
            retained_identity_bytes: 0,
        }
    }

    pub fn with_fallback_timestamp(mut self, timestamp_ms: Option<i64>) -> Self {
        self.fallback_timestamp_ms = timestamp_ms.filter(|value| *value >= 0);
        self
    }

    pub fn with_workspace(mut self, key: Option<String>, label: Option<String>) -> Self {
        self.workspace_key = key;
        self.workspace_label = label;
        self
    }

    pub fn with_config_model(mut self, model: Option<String>) -> Self {
        self.config_model = model.filter(|model| !model.trim().is_empty());
        self
    }

    pub fn ingest(&mut self, record: &[u8]) -> Result<(), ReaderError> {
        let value: Value =
            serde_json::from_slice(record).map_err(|_| ReaderError::InvalidRecord)?;
        let Some(entry) = value.as_object() else {
            return Ok(());
        };
        if entry.is_empty() {
            return Ok(());
        }

        let record_type = entry.get("type").and_then(Value::as_str);
        let entry_id = non_empty_str(entry.get("id"));

        // The `session` header names the session. A message record's id is a
        // per-message UUID and is never a session id, so it is not a fallback.
        if record_type == Some("session")
            && let Some(id) = entry_id
        {
            self.header_session = Some(id.to_owned());
        }

        if let Some(model) = non_empty_str(entry.get("model")) {
            self.model_id = Some(model.to_owned());
        }

        // Record the tree edge before anything else can reject the line, so the
        // branch stays reconstructible even for records that emit no usage.
        if let Some(id) = entry_id {
            // An empty `parentId` means a root, exactly like JSON null.
            // Normalizing it is load-bearing: the branch walk distinguishes
            // "reached a root" from "named ancestor is missing", and an
            // un-normalized empty string would disable the abandoned-branch
            // filter for the whole file.
            let parent = non_empty_str(entry.get("parentId")).map(str::to_owned);
            self.parent_map.insert(id.to_owned(), parent);
            self.last_entry_id = Some(id.to_owned());
        }

        match record_type {
            Some("model_change") => return Ok(()),
            Some("session") => return Ok(()),
            _ => {}
        }

        let message = match entry.get("message").and_then(Value::as_object) {
            Some(message) => message,
            // Legacy flat schema: role and content sit at the top level.
            None => entry,
        };
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            return Ok(());
        };
        if role != "assistant" {
            return Ok(());
        }

        let Some(tokens) = usage_tokens(entry.get("usage"))? else {
            return Ok(());
        };
        let cost = reported_cost(entry.get("usage"))?;

        // A reported all-zero block with no money attached records nothing: the
        // vendor's own accounting adds zero buckets, which is a no-op. A
        // *positive* cost alongside zero buckets is the exception — providers
        // that bill per request, or a client that failed to parse a usage
        // block, still record what was charged — so that row is kept.
        if tokens == TokenCounts::default()
            && !matches!(cost, Some(CostStatus::Reported(nano)) if nano > 0)
        {
            return Ok(());
        }

        let timestamp_ms = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_str)
            .or(self.fallback_timestamp_ms)
            .ok_or(ReaderError::Unsupported("Command Code timestamp"))?;

        let raw_model = self
            .model_id
            .clone()
            .or_else(|| self.config_model.clone())
            .unwrap_or_else(|| UNKNOWN_MODEL.to_owned());
        let model = canonicalize_model(&raw_model);
        if model.len() > MAX_IDENTITY_BYTES || model.chars().any(char::is_control) {
            return Err(ReaderError::InvalidRecord);
        }
        let provider = provider_hint(&raw_model).unwrap_or(PROVIDER);

        // A `/fork` or `/clone` copies entries — entry id and timestamp intact —
        // into a new session file and leaves the original, so keying on both
        // collapses the copies across files. The entry id alone is only 32 bits
        // and is unique per session, so the timestamp is what makes it a safe
        // global key. An absent timestamp uses a stable sentinel that a fork
        // preserves byte-for-byte, rather than the per-file mtime, which
        // differs between the original and its copy.
        let dedup_timestamp = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_str)
            .unwrap_or(-1);
        let dedup_key = match entry_id {
            Some(id) => format!("commandcode:{id}:{dedup_timestamp}"),
            None => format!(
                "commandcode:{}:{}:{}",
                self.workspace_key.as_deref().unwrap_or("-"),
                self.session,
                self.responses.len()
            ),
        };
        if dedup_key.len() > MAX_IDENTITY_BYTES {
            return Err(ReaderError::InvalidRecord);
        }
        self.retained_identity_bytes = self
            .retained_identity_bytes
            .checked_add(dedup_key.len())
            .filter(|bytes| *bytes <= MAX_RETAINED_IDENTITY_BYTES)
            .ok_or(ReaderError::Capacity("Command Code identities"))?;

        if self.responses.len() == MAX_RESPONSES {
            return Err(ReaderError::Capacity("Command Code responses"));
        }
        let session = self
            .header_session
            .clone()
            .unwrap_or_else(|| self.session.clone());
        self.responses.push(Candidate {
            entry_id: entry_id.map(str::to_owned),
            response: ResponseUsage {
                key: SessionKey::new(CLIENT, session),
                model,
                tokens,
                cache_write_1h: 0,
                timestamp_ms,
                dedup_key: Some(dedup_key),
                billing_provider_hint: Some(provider.to_owned()),
                service_tier: None,
                agent: None,
                workspace_key: self.workspace_key.clone(),
                workspace_label: self.workspace_label.clone(),
                title: None,
                cost: cost.unwrap_or(CostStatus::Unavailable),
            },
        });
        Ok(())
    }

    /// The entry ids on the active branch: the parent chain from the last entry
    /// back to its root. `None` means the chain could not be reconstructed, in
    /// which case every message is kept.
    fn active_branch(&self) -> Option<HashSet<&str>> {
        let mut branch = HashSet::new();
        let mut current = self.last_entry_id.as_deref()?;
        // Bounded by the map so a cyclic `parentId` cannot loop forever.
        for _ in 0..=self.parent_map.len() {
            if !branch.insert(current) {
                return Some(branch);
            }
            match self.parent_map.get(current) {
                Some(Some(parent)) => current = parent.as_str(),
                // A real root: the chain is complete and trustworthy.
                Some(None) => return Some(branch),
                // A named ancestor is missing from this file — a skipped line
                // left a hole. Refuse the filter rather than truncate the
                // branch and silently drop billed usage from earlier turns.
                None => return None,
            }
        }
        Some(branch)
    }

    pub fn snapshot(&self) -> Vec<ResponseUsage> {
        match self.active_branch() {
            Some(branch) => self
                .responses
                .iter()
                .filter(|candidate| {
                    candidate
                        .entry_id
                        .as_deref()
                        .is_none_or(|id| branch.contains(id))
                })
                .map(|candidate| candidate.response.clone())
                .collect(),
            None => self
                .responses
                .iter()
                .map(|candidate| candidate.response.clone())
                .collect(),
        }
    }

    pub fn has_supported_usage(&self) -> bool {
        match self.active_branch() {
            Some(branch) => self.responses.iter().any(|candidate| {
                candidate
                    .entry_id
                    .as_deref()
                    .is_none_or(|id| branch.contains(id))
            }),
            None => !self.responses.is_empty(),
        }
    }
}

impl Default for CommandCodeReader {
    fn default() -> Self {
        Self::new("commandcode")
    }
}

/// The token breakdown of a usage block, or `None` when the block reports no
/// counts at all.
///
/// `None` (every bucket field absent) is distinct from a reported all-zero
/// block: Command Code seeds its accumulator with zeroes and writes `?? 0` for
/// every field, so all-zero is exactly what the transcript records when the
/// provider reported nothing back. Conflating the two would let a caller
/// estimate tokens over a block that also reports `costUsd: 0`.
fn usage_tokens(usage: Option<&Value>) -> Result<Option<TokenCounts>, ReaderError> {
    let Some(usage) = usage.and_then(Value::as_object) else {
        return Ok(None);
    };
    let fields = [
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
    ];
    if !fields.iter().any(|field| usage.contains_key(*field)) {
        return Ok(None);
    }
    Ok(Some(TokenCounts {
        input: counter(usage, "inputTokens")?,
        output: counter(usage, "outputTokens")?,
        cache_read: counter(usage, "cacheReadTokens")?,
        cache_write: counter(usage, "cacheWriteTokens")?,
        // The v3 block exposes no reasoning bucket; it is not inferred.
        reasoning: 0,
    }))
}

/// An authoritative per-request cost in nano-USD.
///
/// An explicit `0` is a reported cost — free-promo models bill nothing — and
/// stays provider-reported so pricing never reprices it at a paid rate. A
/// negative, non-finite or implausibly large value is rejected rather than
/// clamped, because a fabricated bill is worse than a missing one.
fn reported_cost(usage: Option<&Value>) -> Result<Option<CostStatus>, ReaderError> {
    let Some(cost) = usage
        .and_then(Value::as_object)
        .and_then(|usage| usage.get("costUsd"))
    else {
        return Ok(None);
    };
    if cost.is_null() {
        return Ok(None);
    }
    let cost = cost.as_f64().ok_or(ReaderError::InvalidRecord)?;
    if !cost.is_finite() || cost < 0.0 || cost > COST_CEILING_USD {
        return Err(ReaderError::InvalidRecord);
    }
    let nano = (cost * 1_000_000_000.0).round();
    if !nano.is_finite() || nano < 0.0 || nano > u64::MAX as f64 {
        return Err(ReaderError::InvalidRecord);
    }
    Ok(Some(CostStatus::Reported(nano as u64)))
}

/// A token counter. Absent or null is zero; anything negative or fractional is
/// rejected rather than clamped.
fn counter(usage: &serde_json::Map<String, Value>, field: &str) -> Result<u64, ReaderError> {
    match usage.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => {
            let count = value.as_i64().ok_or(ReaderError::InvalidRecord)?;
            let count = u64::try_from(count).map_err(|_| ReaderError::InvalidRecord)?;
            if count > TOKEN_CEILING {
                return Err(ReaderError::InvalidRecord);
            }
            Ok(count)
        }
    }
}

/// Command Code reports gateway ids such as `MiniMaxAI/MiniMax-M3-Free`. The
/// org segment is not a pricing key, so it is dropped to leave the model id
/// that pricing matches. No `-free` suffix stripping happens: `poolside/
/// laguna-s-2.1-free` is a real catalog id, and stripping the suffix would
/// invent a model that does not exist.
fn canonicalize_model(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_owned()
}

/// Recover the provider that a gateway id's org segment names, so pricing
/// reaches that provider's catalog instead of the client's own non-pricing
/// `command-code` partition. The hint comes from the RAW id because
/// canonicalization discards the org segment that carries it.
fn provider_hint(raw_model: &str) -> Option<&'static str> {
    let (org, _) = raw_model.split_once('/')?;
    let org = org.trim().to_ascii_lowercase();
    KNOWN_ORGS
        .iter()
        .find(|(name, _)| *name == org)
        .map(|(_, provider)| *provider)
}

/// The gateway org segments that name a pricing provider. Anything else leaves
/// the client's own provider in place rather than guessing at a catalog.
const KNOWN_ORGS: [(&str, &str); 17] = [
    ("anthropic", "anthropic"),
    ("deepseek", "deepseek"),
    ("deepseekai", "deepseek"),
    ("google", "google"),
    ("meta", "meta"),
    ("meta-llama", "meta"),
    ("minimax", "minimax"),
    ("minimaxai", "minimax"),
    ("mistralai", "mistral"),
    ("moonshot", "moonshot"),
    ("moonshotai", "moonshot"),
    ("openai", "openai"),
    ("poolside", "poolside"),
    ("qwen", "alibaba"),
    ("alibaba", "alibaba"),
    ("xai", "xai"),
    ("z-ai", "zhipu"),
];

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
