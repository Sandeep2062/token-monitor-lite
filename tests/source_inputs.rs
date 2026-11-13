use ruzstd::encoding::{CompressionLevel, compress_to_vec};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use token_monitor_lite::parsers::jsonl::{JsonlFramer, MAX_BATCH_BYTES, MAX_RECORD_BYTES};
use token_monitor_lite::runtime::MonitoredSource;
use token_monitor_lite::source_inputs::{InputAdapterStatus, SourceInput};
use token_monitor_lite::sources::{
    SourceFormat, SourceId, SourceProgress, SourceReader, SourceSnapshot,
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Source(PathBuf);

impl Source {
    fn new(bytes: &[u8]) -> Self {
        Self::with_suffix(bytes, "jsonl")
    }

    fn with_suffix(bytes: &[u8], suffix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-input-{}-{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            suffix
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

    fn monitored(&self) -> MonitoredSource {
        self.monitored_as(SourceFormat::ClaudeCli, "fixture")
    }

    fn monitored_as(&self, format: SourceFormat, session: &str) -> MonitoredSource {
        MonitoredSource {
            source_id: SourceId::for_path(format, &self.0),
            path: self.0.clone(),
            format,
            session: session.to_owned(),
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        fs::remove_file(&self.0).unwrap();
    }
}

fn codex_source(session: &str, input: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [
        serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session,
                "model_provider": "openai",
                "source": "cli"
            }
        }),
        serde_json::json!({
            "type": "turn_context",
            "timestamp": "2026-11-09T10:00:00Z",
            "payload": { "model": "gpt-5" }
        }),
        serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-11-09T10:00:01Z",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": { "input_tokens": input },
                    "total_token_usage": { "input_tokens": input }
                }
            }
        }),
    ] {
        bytes.extend(serde_json::to_vec(&value).unwrap());
        bytes.push(b'\n');
    }
    bytes
}

fn zstd(bytes: &[u8]) -> Vec<u8> {
    compress_to_vec(bytes, CompressionLevel::Fastest)
}

fn compressed_source(bytes: &[u8], suffix: &str) -> Source {
    Source::with_suffix(&zstd(bytes), suffix)
}

fn publish(reader: &mut SourceReader) -> (Arc<SourceSnapshot>, u64, u64) {
    let mut physical = 0_u64;
    let mut decoded = 0_u64;
    loop {
        let step = reader.step().unwrap();
        physical += step.physical_bytes_read as u64;
        decoded += step.decoded_bytes_read as u64;
        match step.progress {
            SourceProgress::Pending => {}
            SourceProgress::Published { .. } | SourceProgress::Unchanged => {
                return (
                    reader.last_good().expect("published source snapshot"),
                    physical,
                    decoded,
                );
            }
        }
    }
}

fn skippable_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x50, 0x2A, 0x4D, 0x18];
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn adapter_neutral_framer_preserves_raw_bytes_and_defers_incomplete_tails() {
    let mut framer = JsonlFramer::default();
    let first = framer.admit(&[0xff, b'\n', b'p', b'a', b'r']).unwrap();
    assert_eq!(first.records, vec![vec![0xff]]);
    assert!(first.incomplete_tail);
    assert!(!first.more_buffered);

    let second = framer.admit(b"tial\r\n").unwrap();
    assert_eq!(second.records, vec![b"partial".to_vec()]);
    assert!(!second.incomplete_tail);
    assert!(!second.more_buffered);

    assert_eq!(
        framer
            .admit(&vec![b'x'; MAX_BATCH_BYTES + 1])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert!(!format!("{first:?}").contains("255"));
}

#[test]
fn adapter_neutral_framer_bounds_records_and_drains_buffered_suffix() {
    let mut framer = JsonlFramer::default();
    let first = framer.admit(&vec![b'\n'; 1025]).unwrap();
    assert_eq!(first.records.len(), 1024);
    assert!(first.more_buffered);
    assert!(!first.incomplete_tail);
    let second = framer.admit(b"").unwrap();
    assert_eq!(second.records, vec![Vec::<u8>::new()]);
    assert!(!second.more_buffered);
    assert!(!second.incomplete_tail);
}

#[test]
fn adapter_neutral_framer_enforces_exact_record_cap_and_reset_clears_it() {
    let mut framer = JsonlFramer::default();
    let chunk = vec![b'x'; MAX_BATCH_BYTES];
    for _ in 0..MAX_RECORD_BYTES / MAX_BATCH_BYTES {
        let framed = framer.admit(&chunk).unwrap();
        assert!(framed.records.is_empty());
        assert!(framed.incomplete_tail);
    }
    assert!(
        framer
            .admit(b"y")
            .unwrap_err()
            .to_string()
            .contains("16 MiB")
    );
    framer.reset();
    assert_eq!(framer.admit(b"\n").unwrap().records, vec![Vec::<u8>::new()]);
}

#[test]
fn ordinary_jsonl_reports_exact_physical_and_decoded_bytes_without_source_values() {
    let source = Source::new(b"first\npar");
    let mut input = SourceInput::from_monitored_source(&source.monitored());
    assert_eq!(input.status(), InputAdapterStatus::OrdinaryJsonl);
    let diagnostic = format!("{input:?} {:?}", input.status());
    assert!(!diagnostic.contains(source.0.to_str().unwrap()));
    assert!(!diagnostic.contains("first"));

    let first = input.read_batch().unwrap();
    assert_eq!(first.records, vec![b"first".to_vec()]);
    assert_eq!(first.physical_bytes_read, 9);
    assert_eq!(first.decoded_bytes_read, 9);
    assert!(first.incomplete_tail);
    assert!(input.is_current().unwrap());
    assert!(!format!("{first:?}").contains("first"));

    source.append(b"tial\n");
    assert!(!input.is_current().unwrap());
    let second = input.read_batch().unwrap();
    assert_eq!(second.records, vec![b"partial".to_vec()]);
    assert_eq!(second.physical_bytes_read, 5);
    assert_eq!(second.decoded_bytes_read, 5);
    assert!(!second.reset);
}

#[test]
fn ordinary_jsonl_reset_clears_framing_and_accepts_a_truncated_replacement() {
    let source = Source::new(b"old-record\npartial");
    let mut input = SourceInput::from_monitored_source(&source.monitored());
    let first = input.read_batch().unwrap();
    assert_eq!(first.records, vec![b"old-record".to_vec()]);
    assert!(first.incomplete_tail);

    fs::write(&source.0, b"new\n").unwrap();
    input.reset();
    let replacement = input.read_batch().unwrap();
    assert!(replacement.reset);
    assert_eq!(replacement.records, vec![b"new".to_vec()]);
    assert!(!replacement.incomplete_tail);
    assert!(input.is_current().unwrap());
}

#[test]
fn regular_file_mutation_fence_rejects_a_mixed_multibatch_candidate() {
    let source = Source::new(&vec![b'\n'; MAX_BATCH_BYTES + 1]);
    let mut input = SourceInput::from_monitored_source(&source.monitored());
    let first = input.read_batch().unwrap();
    assert_eq!(first.physical_bytes_read, MAX_BATCH_BYTES);
    assert_eq!(first.decoded_bytes_read, MAX_BATCH_BYTES);
    assert!(first.more_available);
    assert!(input.is_current().unwrap());

    source.append(b"changed\n");
    assert!(!input.is_current().unwrap());
    input.reset();
    let replacement = input.read_batch().unwrap();
    assert!(replacement.reset);
    assert_eq!(replacement.physical_bytes_read, MAX_BATCH_BYTES);
    assert_eq!(replacement.decoded_bytes_read, MAX_BATCH_BYTES);
}

#[test]
fn compressed_codex_rollout_matches_ordinary_rows_and_reports_both_counters() {
    let bytes = codex_source("compressed", 7);
    let ordinary = Source::new(&bytes);
    let compressed = compressed_source(&bytes, "jsonl.zst");
    let mut ordinary_reader =
        SourceReader::new(ordinary.0.clone(), SourceFormat::CodexCli, "compressed");
    let mut compressed_reader =
        SourceReader::new(compressed.0.clone(), SourceFormat::CodexCli, "compressed");

    let (ordinary_snapshot, _, _) = publish(&mut ordinary_reader);
    let (compressed_snapshot, physical, decoded) = publish(&mut compressed_reader);
    assert_eq!(ordinary_snapshot.rows, compressed_snapshot.rows);
    assert_eq!(ordinary_snapshot.totals, compressed_snapshot.totals);
    assert_eq!(decoded, bytes.len() as u64);
    assert!(physical > 0);
    assert_eq!(
        SourceInput::from_monitored_source(
            &compressed.monitored_as(SourceFormat::CodexCli, "compressed")
        )
        .status(),
        InputAdapterStatus::CompressedCodexJsonl
    );
}

#[test]
fn pinned_compressed_rollout_matches_uncompressed_supported_rows() {
    let mut uncompressed = include_str!("fixtures/pinned/codex-parser18-prefix.jsonl")
        .lines()
        .take(4)
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    uncompressed.push(b'\n');
    let compressed = include_bytes!("fixtures/pinned/codex-parser18-supported.jsonl.zst");
    let plain = Source::new(&uncompressed);
    let zstd = Source::with_suffix(compressed, "jsonl.zst");
    let mut plain_reader = SourceReader::new(
        plain.0.clone(),
        SourceFormat::CodexCli,
        "synthetic-codex-fixture18",
    );
    let mut zstd_reader = SourceReader::new(
        zstd.0.clone(),
        SourceFormat::CodexCli,
        "synthetic-codex-fixture18",
    );

    let (plain_snapshot, _, _) = publish(&mut plain_reader);
    let (zstd_snapshot, _, decoded) = publish(&mut zstd_reader);
    assert_eq!(plain_snapshot.rows, zstd_snapshot.rows);
    assert_eq!(plain_snapshot.totals, zstd_snapshot.totals);
    assert_eq!(decoded, uncompressed.len() as u64);
    assert_eq!(zstd_snapshot.rows.len(), 1);
}

#[test]
fn pinned_compressed_rollout_matches_record_and_compaction_rows() {
    let uncompressed = include_bytes!("fixtures/pinned/codex-parser18-prefix.jsonl");
    let compressed = include_bytes!("fixtures/pinned/codex-parser18-full.jsonl.zst");
    let plain = Source::new(uncompressed);
    let zstd = Source::with_suffix(compressed, "jsonl.zst");
    let mut plain_reader = SourceReader::new(
        plain.0.clone(),
        SourceFormat::CodexCli,
        "synthetic-codex-fixture18",
    );
    let mut zstd_reader = SourceReader::new(
        zstd.0.clone(),
        SourceFormat::CodexCli,
        "synthetic-codex-fixture18",
    );

    let (plain_snapshot, _, _) = publish(&mut plain_reader);
    let (zstd_snapshot, _, decoded) = publish(&mut zstd_reader);
    assert_eq!(plain_snapshot.rows, zstd_snapshot.rows);
    assert_eq!(plain_snapshot.totals, zstd_snapshot.totals);
    assert_eq!(decoded, uncompressed.len() as u64);
    assert_eq!(zstd_snapshot.rows.len(), 3);
}

#[test]
fn compressed_stream_accepts_concatenated_and_skippable_frames() {
    let bytes = codex_source("concatenated", 11);
    let split = bytes.len() / 2;
    let mut stream = skippable_frame(b"prefix");
    stream.extend_from_slice(&zstd(&bytes[..split]));
    stream.extend_from_slice(&skippable_frame(b"middle"));
    stream.extend_from_slice(&zstd(&bytes[split..]));
    let source = Source::with_suffix(&stream, "jsonl.zst");
    let ordinary = Source::new(&bytes);
    let mut source_reader =
        SourceReader::new(source.0.clone(), SourceFormat::CodexCli, "concatenated");
    let mut ordinary_reader =
        SourceReader::new(ordinary.0.clone(), SourceFormat::CodexCli, "concatenated");

    let (source_snapshot, _, _) = publish(&mut source_reader);
    let (ordinary_snapshot, _, _) = publish(&mut ordinary_reader);
    assert_eq!(source_snapshot.rows, ordinary_snapshot.rows);
    assert_eq!(source_snapshot.totals, ordinary_snapshot.totals);
}

#[test]
fn compressed_stream_failures_retain_last_good_and_do_not_publish() {
    let bytes = codex_source("retained", 13);
    let source = compressed_source(&bytes, "jsonl.zst");
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::CodexCli, "retained");
    let (last_good, _, _) = publish(&mut reader);
    assert_eq!(last_good.totals.input, 13);

    fs::write(&source.0, b"not a zstd stream").unwrap();
    expect_compressed_failure(&mut reader);
    assert_eq!(reader.last_good().unwrap().totals, last_good.totals);
    assert_eq!(reader.last_good().unwrap().rows, last_good.rows);
}

#[test]
fn compressed_stream_rejects_truncation_corruption_trailing_data_and_dictionary() {
    let bytes = codex_source("invalid", 17);
    let encoded = zstd(&bytes);

    let mut truncated = encoded.clone();
    truncated.truncate(truncated.len() - 2);
    assert_compressed_input_failure(&truncated);

    let mut corrupt_checksum = encoded.clone();
    *corrupt_checksum.last_mut().unwrap() ^= 0xFF;
    assert_compressed_input_failure(&corrupt_checksum);

    let mut trailing = encoded.clone();
    trailing.extend_from_slice(b"garbage");
    assert_compressed_input_failure(&trailing);

    let mut unsupported_magic = encoded.clone();
    unsupported_magic[..4].copy_from_slice(&[0, 0, 0, 0]);
    assert_compressed_input_failure(&unsupported_magic);

    let mut reserved_header = encoded.clone();
    reserved_header[4] |= 0x08;
    assert_compressed_input_failure(&reserved_header);

    assert_compressed_input_failure(&add_dictionary_id(&encoded));
    assert_compressed_input_failure(&skippable_frame(b"only-skippable"));
}

#[test]
fn compressed_source_replacement_during_decode_discards_mixed_candidate() {
    let mut original = codex_source("replacement", 7);
    original.extend(vec![b'\n'; MAX_BATCH_BYTES * 2]);
    let source = compressed_source(&original, "jsonl.zst");
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::CodexCli, "replacement");
    assert_eq!(reader.step().unwrap().progress, SourceProgress::Pending);

    fs::write(&source.0, zstd(&codex_source("replacement", 19))).unwrap();
    let (snapshot, _, _) = publish(&mut reader);
    assert_eq!(snapshot.totals.input, 19);
    assert_eq!(snapshot.rows.len(), 1);
}

fn assert_compressed_input_failure(bytes: &[u8]) {
    let source = Source::with_suffix(bytes, "jsonl.zst");
    let mut reader = SourceReader::new(source.0.clone(), SourceFormat::CodexCli, "invalid");
    expect_compressed_failure(&mut reader);
}

fn expect_compressed_failure(reader: &mut SourceReader) {
    for _ in 0..16 {
        match reader.step() {
            Ok(step) => match step.progress {
                SourceProgress::Pending => {}
                SourceProgress::Published { .. } | SourceProgress::Unchanged => {
                    panic!("invalid compressed source unexpectedly published")
                }
            },
            Err(_) => return,
        }
    }
    panic!("invalid compressed source did not fail");
}

fn add_dictionary_id(frame: &[u8]) -> Vec<u8> {
    assert_eq!(frame[4] & 0x03, 0);
    let single_segment = frame[4] & 0x20 != 0;
    let insert_at = if single_segment { 5 } else { 6 };
    let mut frame = frame.to_vec();
    frame[4] |= 0x01;
    frame.insert(insert_at, 1);
    frame
}
