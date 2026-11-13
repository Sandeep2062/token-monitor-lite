use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use token_monitor_lite::parsers::jsonl::{JsonlCursor, MAX_BATCH_BYTES, MAX_RECORD_BYTES};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Source(PathBuf);

impl Source {
    fn new(bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-jsonl-{}-{}.jsonl",
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

#[test]
fn appends_read_only_new_bytes_and_unchanged_reads_are_empty() {
    let source = Source::new(b"first\n");
    let mut cursor = JsonlCursor::default();
    let first = cursor.read_batch(&source.0).unwrap();
    assert!(first.reset);
    assert_eq!(first.records, vec![b"first".to_vec()]);
    assert_eq!(first.bytes_read, 6);
    assert_eq!(cursor.read_batch(&source.0).unwrap().bytes_read, 0);
    source.append(b"second\n");
    let next = cursor.read_batch(&source.0).unwrap();
    assert!(!next.reset);
    assert_eq!(next.records, vec![b"second".to_vec()]);
    assert_eq!(next.bytes_read, 7);
}

#[test]
fn partial_final_record_is_retained_until_newline_without_replay() {
    let source = Source::new(b"one\npar");
    let mut cursor = JsonlCursor::default();
    let first = cursor.read_batch(&source.0).unwrap();
    assert_eq!(first.records, vec![b"one".to_vec()]);
    assert!(first.incomplete_tail);
    source.append(b"tial\r\n");
    let next = cursor.read_batch(&source.0).unwrap();
    assert_eq!(next.records, vec![b"partial".to_vec()]);
    assert!(!next.incomplete_tail);
    assert!(cursor.read_batch(&source.0).unwrap().records.is_empty());
}

#[test]
fn truncation_and_same_size_rewrite_force_new_generation() {
    let source = Source::new(b"original\n");
    let mut cursor = JsonlCursor::default();
    cursor.read_batch(&source.0).unwrap();
    fs::write(&source.0, b"replaced\n").unwrap();
    let replaced = cursor.read_batch(&source.0).unwrap();
    assert!(replaced.reset);
    assert_eq!(replaced.records, vec![b"replaced".to_vec()]);
    fs::write(&source.0, b"x\n").unwrap();
    let shorter = cursor.read_batch(&source.0).unwrap();
    assert!(shorter.reset);
    assert_eq!(shorter.records, vec![b"x".to_vec()]);
}

#[test]
fn replacement_and_path_changes_never_reuse_old_tail() {
    let source = Source::new(b"old-partial");
    let replacement = Source::new(b"new\n");
    let mut cursor = JsonlCursor::default();
    cursor.read_batch(&source.0).unwrap();
    fs::remove_file(&source.0).unwrap();
    fs::rename(&replacement.0, &source.0).unwrap();
    std::mem::forget(replacement);
    let batch = cursor.read_batch(&source.0).unwrap();
    assert!(batch.reset);
    assert_eq!(batch.records, vec![b"new".to_vec()]);
    let other = Source::new(b"other\n");
    let batch = cursor.read_batch(&other.0).unwrap();
    assert!(batch.reset);
    assert_eq!(batch.records, vec![b"other".to_vec()]);
}

#[test]
fn batch_work_is_bounded_and_large_files_resume_without_loss() {
    let bytes = vec![b'\n'; MAX_BATCH_BYTES * 3 + 1];
    let source = Source::new(&bytes);
    let mut cursor = JsonlCursor::default();
    let mut read = 0;
    let mut records = 0;
    loop {
        let batch = cursor.read_batch(&source.0).unwrap();
        assert!(batch.bytes_read <= MAX_BATCH_BYTES);
        assert!(batch.records.len() <= 1024);
        read += batch.bytes_read;
        records += batch.records.len();
        if !batch.more_available {
            break;
        }
    }
    assert_eq!(read, bytes.len());
    assert_eq!(records, bytes.len());
}

#[test]
fn exact_record_cap_is_accepted_and_oversized_record_is_an_error() {
    let source = Source::new(&vec![b'x'; MAX_RECORD_BYTES]);
    let mut cursor = JsonlCursor::default();
    loop {
        let batch = cursor.read_batch(&source.0).unwrap();
        if !batch.more_available {
            break;
        }
    }
    source.append(b"\n");
    assert_eq!(
        cursor.read_batch(&source.0).unwrap().records[0].len(),
        MAX_RECORD_BYTES
    );
    let oversized = Source::new(&vec![b'x'; MAX_RECORD_BYTES + 1]);
    let mut cursor = JsonlCursor::default();
    while let Ok(batch) = cursor.read_batch(&oversized.0) {
        assert!(batch.more_available);
    }
}

#[test]
fn failed_read_preserves_cursor_for_later_append() {
    let source = Source::new(b"first\n");
    let mut cursor = JsonlCursor::default();
    cursor.read_batch(&source.0).unwrap();
    assert!(
        cursor
            .read_batch(&source.0.with_extension("missing"))
            .is_err()
    );
    source.append(b"second\n");
    let batch = cursor.read_batch(&source.0).unwrap();
    assert!(!batch.reset);
    assert_eq!(batch.records, vec![b"second".to_vec()]);
}
