use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use token_monitor_lite::accounting::{SessionKey, TokenCounts};
use token_monitor_lite::parsers::jsonl::MAX_BATCH_BYTES;
use token_monitor_lite::parsers::{CostStatus, ResponseUsage};
use token_monitor_lite::sources::{
    ObservationSourceState, SourceFormat, SourceId, SourceProgress, SourceReader, SourceSnapshot,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Source(PathBuf);

impl Source {
    fn new(bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-source-{}-{}.jsonl",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        Self(path)
    }

    fn append(&self, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(&self.0)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        fs::remove_file(&self.0).unwrap();
    }
}

fn claude(id: &str, input: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type":"assistant", "timestamp":"2026-11-09T10:00:00Z",
        "message":{"id":id,"model":"claude","usage":{"input_tokens":input}}
    }))
    .unwrap()
}

fn publish(reader: &mut SourceReader) -> i128 {
    loop {
        let step = reader.step().unwrap();
        assert!(step.bytes_read <= MAX_BATCH_BYTES);
        assert_eq!(step.physical_bytes_read, step.bytes_read);
        assert_eq!(step.decoded_bytes_read, step.bytes_read);
        match step.progress {
            SourceProgress::Pending => {}
            SourceProgress::Published { delta } => return delta.input,
            SourceProgress::Unchanged => panic!("expected a publication"),
        }
    }
}

#[test]
fn complete_framed_prefix_append_and_tail_are_published_without_replay() {
    let mut bytes = claude("a", 7);
    bytes.push(b'\n');
    let source = Source::new(&bytes);
    let expected_id = SourceId::for_path(SourceFormat::ClaudeCli, &source.0);
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "a");
    assert_eq!(publish(&mut reader), 7);
    let first = reader.last_good().unwrap();
    assert_eq!(first.source_id, expected_id);
    assert_eq!(first.rows[0].key.client, "claude");
    assert_eq!(first.rows[0].key.session, "a");
    assert_eq!(first.format, SourceFormat::ClaudeCli);
    assert_eq!(first.parser_pin, "d3df620b2cdd9502c70aececdbc6ece61fe26fc9");
    assert!(!first.incomplete_tail);
    let record = claude("b", 3);
    source.append(&record[..record.len() / 2]);
    assert_eq!(publish(&mut reader), 0);
    assert!(reader.last_good().unwrap().incomplete_tail);
    assert_eq!(reader.last_good().unwrap().rows.len(), 1);
    source.append(&record[record.len() / 2..]);
    source.append(b"\n");
    assert_eq!(publish(&mut reader), 3);
    let step = reader.step().unwrap();
    assert_eq!(step.bytes_read, 0);
    assert_eq!(step.progress, SourceProgress::Unchanged);
    assert_eq!(reader.last_good().unwrap().totals.input, 10);
    assert_eq!(first.totals.input, 7); // Published snapshots remain immutable.
    assert!(!format!("{first:?}").contains(source.0.to_str().unwrap()));
}

#[test]
fn source_snapshot_supports_many_sessions_with_exact_checked_totals() {
    let rows: Arc<[ResponseUsage]> = vec![
        ResponseUsage {
            key: SessionKey::new("claude", "one"),
            model: "sonnet".to_owned(),
            tokens: TokenCounts {
                input: 3,
                output: 4,
                cache_read: 1,
                cache_write: 2,
                reasoning: 0,
            },
            cache_write_1h: 2,
            timestamp_ms: 1,
            dedup_key: Some("message:one".to_owned()),
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        },
        ResponseUsage {
            key: SessionKey::new("claude", "two"),
            model: "opus".to_owned(),
            tokens: TokenCounts {
                input: 5,
                output: 6,
                cache_read: 0,
                cache_write: 1,
                reasoning: 0,
            },
            cache_write_1h: 1,
            timestamp_ms: 2,
            dedup_key: Some("message:two".to_owned()),
            billing_provider_hint: None,
            service_tier: None,
            agent: None,
            cost: CostStatus::Unavailable,
        },
    ]
    .into();
    let snapshot = SourceSnapshot {
        source_id: SourceId::from_bytes([7; 16]),
        format: SourceFormat::ClaudeCli,
        parser_pin: "fixture".to_owned(),
        generation: 9,
        rows,
        retained: Arc::from([]),
        totals: TokenCounts {
            input: 8,
            output: 10,
            cache_read: 1,
            cache_write: 3,
            reasoning: 0,
        },
        cache_write_1h: 3,
        incomplete_tail: false,
        has_supported_usage: true,
        source_state: ObservationSourceState::Live,
    };

    assert_eq!(snapshot.rows.len(), 2);
    assert_ne!(snapshot.rows[0].key, snapshot.rows[1].key);
    assert_eq!(snapshot.totals.input, 8);
    assert_eq!(snapshot.cache_write_1h, 3);
}

#[test]
fn jsonl_session_hint_rejects_an_unexpected_identity_change() {
    let first = serde_json::to_vec(&serde_json::json!({
        "type": "assistant",
        "sessionId": "expected",
        "timestamp": "2026-11-09T10:00:00Z",
        "message": {"id": "a", "model": "claude", "usage": {"input_tokens": 7}}
    }))
    .unwrap();
    let changed = serde_json::to_vec(&serde_json::json!({
        "type": "assistant",
        "sessionId": "other",
        "timestamp": "2026-11-09T10:00:01Z",
        "message": {"id": "b", "model": "claude", "usage": {"input_tokens": 3}}
    }))
    .unwrap();
    let mut bytes = first;
    bytes.push(b'\n');
    let source = Source::new(&bytes);
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "expected");
    assert_eq!(publish(&mut reader), 7);
    source.append(&changed);
    source.append(b"\n");

    assert!(reader.step().is_err());
    assert_eq!(reader.last_good().unwrap().generation, 1);
    assert_eq!(reader.last_good().unwrap().totals.input, 7);
}

#[test]
fn no_partial_candidate_publishes_and_failed_generation_is_fully_retried() {
    let source = Source::new(b"\n");
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "a");
    assert_eq!(publish(&mut reader), 0);
    let good = reader.last_good().unwrap();
    let mut bytes = vec![b'\n'; MAX_BATCH_BYTES + 10];
    bytes.extend(claude("b", 5));
    bytes.extend(b"\n{secret-marker\n");
    source.append(&bytes);
    let mut saw_pending = false;
    loop {
        match reader.step() {
            Ok(step) => {
                assert_eq!(step.progress, SourceProgress::Pending);
                assert_eq!(reader.last_good().unwrap().generation, good.generation);
                saw_pending = true;
            }
            Err(error) => {
                assert!(!format!("{error:?} {error}").contains("secret-marker"));
                break;
            }
        }
    }
    assert!(saw_pending);
    assert_eq!(reader.last_good().unwrap().totals.input, 0);
    let mut repaired = claude("b", 5);
    repaired.push(b'\n');
    fs::write(&source.0, repaired).unwrap();
    assert_eq!(publish(&mut reader), 5);
    assert_eq!(reader.last_good().unwrap().rows.len(), 1);
}

#[test]
fn full_reconciliation_corrects_old_bytes_plus_growth_with_signed_delta() {
    let mut bytes = claude("a", 100);
    bytes.push(b'\n');
    let source = Source::new(&bytes);
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "a");
    assert_eq!(publish(&mut reader), 100);
    let mut rewrite = claude("a", 75);
    rewrite.extend(b"\n                                             \n");
    assert!(rewrite.len() > bytes.len());
    fs::write(&source.0, &rewrite).unwrap();
    reader.request_full_reconciliation();
    assert_eq!(publish(&mut reader), -25);
    assert_eq!(reader.last_good().unwrap().totals.input, 75);
}

#[test]
fn rotation_resets_and_io_failure_never_becomes_confirmed_zero_or_deletion() {
    let mut bytes = claude("a", 7);
    bytes.push(b'\n');
    let source = Source::new(&bytes);
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "a");
    assert_eq!(publish(&mut reader), 7);
    let previous = source.0.with_extension("old");
    fs::rename(&source.0, &previous).unwrap();
    assert!(reader.step().is_err());
    assert_eq!(reader.last_good().unwrap().totals.input, 7);
    fs::write(&source.0, b"\n").unwrap();
    assert_eq!(publish(&mut reader), -7);
    fs::remove_file(previous).unwrap();
}

#[test]
fn checked_source_aggregation_failure_retains_complete_last_good() {
    let mut bytes = claude("a", u64::MAX);
    bytes.push(b'\n');
    let source = Source::new(&bytes);
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "a");
    assert_eq!(publish(&mut reader), i128::from(u64::MAX));
    let mut extra = claude("b", 1);
    extra.push(b'\n');
    source.append(&extra);
    assert!(reader.step().is_err());
    assert_eq!(reader.last_good().unwrap().totals.input, u64::MAX);
    assert_eq!(reader.last_good().unwrap().rows.len(), 1);
}

#[test]
fn codex_supported_zero_is_distinct_from_no_supported_usage() {
    let source = Source::new(b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"a\"}}\n");
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::CodexCli, "a");
    assert_eq!(publish(&mut reader), 0);
    assert!(!reader.last_good().unwrap().has_supported_usage);
    source.append(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":0}}}}\n");
    assert_eq!(publish(&mut reader), 0);
    assert!(reader.last_good().unwrap().has_supported_usage);
    assert!(reader.last_good().unwrap().rows.is_empty());
}

#[test]
fn changing_source_during_multibatch_reconciliation_discards_mixed_candidate() {
    let mut original = claude("a", 100);
    original.push(b'\n');
    original.extend(vec![b'\n'; MAX_BATCH_BYTES]);
    let source = Source::new(&original);
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "a");
    assert_eq!(reader.step().unwrap().progress, SourceProgress::Pending);
    assert!(reader.last_good().is_none());
    let mut revised = claude("a", 75);
    revised.push(b'\n');
    revised.extend(vec![b'\n'; MAX_BATCH_BYTES + 100]);
    fs::write(&source.0, &revised).unwrap();
    assert_eq!(publish(&mut reader), 75);
    assert_eq!(reader.last_good().unwrap().rows.len(), 1);
}

#[test]
fn unsupported_and_oversized_sources_withhold_every_candidate_row() {
    use token_monitor_lite::parsers::jsonl::MAX_RECORD_BYTES;
    let mut bytes = claude("a", 7);
    bytes.push(b'\n');
    let source = Source::new(&bytes);
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::ClaudeCli, "a");
    assert_eq!(publish(&mut reader), 7);
    source.append(b"{\"type\":\"assistant\",\"sessionId\":\"other\"}\n");
    assert!(reader.step().is_err());
    assert_eq!(reader.last_good().unwrap().totals.input, 7);
    fs::write(&source.0, vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
    while let Ok(step) = reader.step() {
        assert_eq!(step.progress, SourceProgress::Pending);
    }
    assert_eq!(reader.last_good().unwrap().totals.input, 7);
    assert_eq!(reader.last_good().unwrap().rows.len(), 1);
}

#[test]
fn cc_mirror_path_sets_client_and_file_mtime_is_the_fallback_timestamp() {
    let root = std::env::temp_dir().join(format!(
        "token-monitor-cc-mirror-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let directory = root.join(".cc-mirror/variant/config/projects");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("mirror.jsonl");
    fs::write(
        &path,
        br#"{"type":"assistant","message":{"id":"mirror-message","model":"claude","usage":{"input_tokens":4}}}"#.as_slice(),
    )
    .unwrap();
    {
        use std::io::Write as _;
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
    }
    let expected_timestamp = fs::metadata(&path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let mut reader = SourceReader::new(path, SourceFormat::ClaudeCli, "session-a");
    while let SourceProgress::Pending = reader.step().unwrap().progress {}
    let snapshot = reader.last_good().unwrap();
    assert_eq!(snapshot.rows[0].key.client, "cc-mirror/variant");
    assert_eq!(snapshot.rows[0].timestamp_ms, expected_timestamp);
    fs::remove_dir_all(root).unwrap();
}
