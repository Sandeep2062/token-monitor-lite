//! WorkBuddy JSONL transcript reader. See `tencent_buddy` for the shared
//! record contract. The older `workbuddy.db` aggregate fallback and extension
//! log sinks are not read.

use super::tencent_buddy::BuddyReader;

pub type WorkBuddyReader = BuddyReader;

pub fn reader(session: impl Into<String>) -> WorkBuddyReader {
    BuddyReader::new("workbuddy", "workbuddy", session)
}

pub fn sqlite_row_to_usage(
    session: &str,
    model: Option<&str>,
    used: u64,
    updated_at: i64,
) -> Result<super::ResponseUsage, super::ReaderError> {
    use super::{CostStatus, ReaderError, ResponseUsage};
    use crate::accounting::{SessionKey, TokenCounts};
    if !super::tencent_buddy::valid_identity(session) {
        return Err(ReaderError::InvalidRecord);
    }
    if used == 0 || updated_at <= 0 {
        return Err(ReaderError::InvalidRecord);
    }
    let model = model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("auto");
    if !super::tencent_buddy::valid_identity(model) {
        return Err(ReaderError::InvalidRecord);
    }
    let timestamp_ms = if updated_at > 10_000_000_000 {
        updated_at
    } else {
        updated_at
            .checked_mul(1000)
            .ok_or(ReaderError::InvalidTimestamp)?
    };
    Ok(ResponseUsage {
        key: SessionKey::new("workbuddy", session),
        model: model.to_owned(),
        tokens: TokenCounts {
            input: used,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        },
        cache_write_1h: 0,
        timestamp_ms,
        dedup_key: Some(format!("workbuddy:{session}:{updated_at}")),
        billing_provider_hint: None,
        service_tier: None,
        agent: None,
        cost: CostStatus::Unavailable,
    })
}
