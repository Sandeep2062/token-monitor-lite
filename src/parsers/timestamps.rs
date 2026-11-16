//! Shared timestamp parsing for the catalog JSONL readers. A missing or
//! malformed explicit timestamp is a declined record; callers may use a
//! documented file-mtime fallback only where the pinned reader does.
//!
//! A timestamp that carries an explicit offset is absolute and is used as-is.
//! A naive timestamp is the writing host's wall clock and is resolved in the
//! local zone — see `naive_to_epoch_ms`.

use serde_json::Value;

pub fn parse_timestamp_value(value: &Value) -> Option<i64> {
    if let Some(text) = value.as_str() {
        return parse_timestamp_str(text);
    }
    if let Some(number) = value.as_i64() {
        return numeric_ms(number);
    }
    value
        .as_u64()
        .and_then(|number| i64::try_from(number).ok())
        .and_then(numeric_ms)
}

pub fn parse_timestamp_str(value: &str) -> Option<i64> {
    let value = value.trim();
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.timestamp_millis());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(timestamp) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Some(naive_to_epoch_ms(timestamp));
        }
    }
    value.parse::<i64>().ok().and_then(numeric_ms)
}

/// A timestamp with no zone designator is the writing host's local wall clock.
///
/// These sources are read on the machine that wrote them, so the local zone is
/// the only defensible reading. Interpreting them as UTC instead shifts every
/// such record by the host's UTC offset, which silently moves records across
/// midnight and corrupts the day/month buckets even though record ordering is
/// unaffected. The pinned readers resolve these against the host zone, so this
/// matches them.
///
/// A wall clock inside a DST fold or gap is ambiguous or nonexistent. `Local`
/// resolves the fold to the earlier instant and the gap by shifting forward,
/// which is deterministic and keeps the record inside the intended day.
fn naive_to_epoch_ms(timestamp: chrono::NaiveDateTime) -> i64 {
    use chrono::TimeZone;
    match chrono::Local.from_local_datetime(&timestamp) {
        chrono::LocalResult::Single(local) => local.timestamp_millis(),
        chrono::LocalResult::Ambiguous(earlier, _) => earlier.timestamp_millis(),
        chrono::LocalResult::None => timestamp.and_utc().timestamp_millis(),
    }
}

fn numeric_ms(value: i64) -> Option<i64> {
    if value <= 0 {
        return None;
    }
    if value >= 1_000_000_000_000 {
        Some(value)
    } else {
        Some(value.saturating_mul(1000))
    }
}

pub fn counter(value: &Value, key: &str) -> Result<u64, super::ReaderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(super::ReaderError::InvalidRecord),
    }
}

pub fn optional_counter(value: &Value, key: &str) -> Result<Option<u64>, super::ReaderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or(super::ReaderError::InvalidRecord),
    }
}

pub fn tokens_total(tokens: crate::accounting::TokenCounts) -> Result<u64, super::ReaderError> {
    tokens
        .input
        .checked_add(tokens.output)
        .and_then(|value| value.checked_add(tokens.cache_read))
        .and_then(|value| value.checked_add(tokens.cache_write))
        .and_then(|value| value.checked_add(tokens.reasoning))
        .ok_or(super::ReaderError::Overflow)
}
