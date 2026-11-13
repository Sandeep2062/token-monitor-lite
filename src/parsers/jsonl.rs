//! Incremental JSONL framing, without interpreting or repairing JSON.
//! Metadata-observed rewrites reset the cursor. Growth is an append hint, not
//! proof of unchanged earlier bytes: callers must request full reconciliation
//! on ambiguous events and periodically rebaseline mutable histories.

use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
const MAX_BATCH_RECORDS: usize = 1024;

#[derive(Default)]
pub struct JsonlCursor {
    source: Option<SourceStamp>,
    offset: u64,
    framer: JsonlFramer,
}

/// Adapter-neutral complete-record framing. Input remains uninterpreted bytes;
/// UTF-8 and JSON validation belong to the client-specific response reader.
#[derive(Default)]
pub struct JsonlFramer {
    tail: Vec<u8>,
    pending: Vec<u8>,
}

pub struct FramedBatch {
    pub records: Vec<Vec<u8>>,
    pub incomplete_tail: bool,
    pub more_buffered: bool,
}

#[derive(PartialEq, Eq)]
pub(crate) struct SourceStamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl SourceStamp {
    pub(crate) fn new(path: &Path, metadata: &Metadata) -> Self {
        Self {
            path: path.to_path_buf(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn requires_reset(&self, next: &Self) -> bool {
        if self.path != next.path
            || self.created != next.created
            || next.len < self.len
            || (next.len == self.len && next.modified != self.modified)
        {
            return true;
        }
        #[cfg(unix)]
        if self.identity != next.identity {
            return true;
        }
        false
    }
}

pub struct ReadBatch {
    pub records: Vec<Vec<u8>>,
    pub bytes_read: usize,
    pub reset: bool,
    pub incomplete_tail: bool,
    pub more_available: bool,
}

impl fmt::Debug for FramedBatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FramedBatch")
            .field("records", &self.records.len())
            .field("incomplete_tail", &self.incomplete_tail)
            .field("more_buffered", &self.more_buffered)
            .finish()
    }
}

impl fmt::Debug for ReadBatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReadBatch")
            .field("records", &self.records.len())
            .field("bytes_read", &self.bytes_read)
            .field("reset", &self.reset)
            .field("incomplete_tail", &self.incomplete_tail)
            .field("more_available", &self.more_available)
            .finish()
    }
}

impl JsonlFramer {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Admit at most one decoded batch. A record-count-limited suffix remains
    /// buffered and must be drained with an empty admission before more bytes
    /// are supplied.
    pub fn admit(&mut self, bytes: &[u8]) -> io::Result<FramedBatch> {
        if bytes.len() > MAX_BATCH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "decoded batch exceeds 256 KiB limit",
            ));
        }
        if !self.pending.is_empty() && !bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffered records must be drained before admitting bytes",
            ));
        }
        let input = if self.pending.is_empty() {
            bytes
        } else {
            &self.pending
        };
        // Validate the admitted prefix before mutating framing state. Record
        // overflow is visible; it must not publish a partially repaired source.
        let mut length = self.tail.len();
        let mut end = 0;
        let mut rows = 0;
        for byte in input {
            end += 1;
            if *byte == b'\n' {
                length = 0;
                rows += 1;
                if rows == MAX_BATCH_RECORDS {
                    break;
                }
            } else {
                length += 1;
                if length > MAX_RECORD_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "JSONL record exceeds 16 MiB limit",
                    ));
                }
            }
        }
        let mut records = Vec::with_capacity(rows);
        for byte in &input[..end] {
            if *byte == b'\n' {
                let mut record = std::mem::take(&mut self.tail);
                if record.last() == Some(&b'\r') {
                    record.pop();
                }
                records.push(record);
            } else {
                self.tail.push(*byte);
            }
        }
        if self.pending.is_empty() {
            self.pending.extend_from_slice(&bytes[end..]);
        } else {
            self.pending.drain(..end);
        }
        Ok(FramedBatch {
            records,
            incomplete_tail: !self.tail.is_empty(),
            more_buffered: !self.pending.is_empty(),
        })
    }

    pub(crate) fn has_buffered(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl JsonlCursor {
    /// Discard framing state for an explicitly requested full reconciliation.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Fence publication against metadata-observed changes since the last
    /// batch. This does not prove unchanged old bytes; full reconciliation is
    /// still required for ambiguous growth and unobservable in-place edits.
    pub(crate) fn is_current(&self, path: &Path) -> io::Result<bool> {
        let file = File::open(path)?;
        let stamp = SourceStamp::new(path, &file.metadata()?);
        Ok(self.source.as_ref() == Some(&stamp))
    }

    pub fn read_batch(&mut self, path: &Path) -> io::Result<ReadBatch> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source is not a file",
            ));
        }
        let stamp = SourceStamp::new(path, &metadata);
        let reset = self
            .source
            .as_ref()
            .is_none_or(|old| old.requires_reset(&stamp));
        // A replacement is built independently. Failed reads leave the old
        // source cursor intact, just as collector last-good state must remain.
        if reset {
            let mut replacement = Self::default();
            let batch = replacement.read_from(&mut file, stamp, true)?;
            *self = replacement;
            return Ok(batch);
        }
        self.read_from(&mut file, stamp, false)
    }

    fn read_from(
        &mut self,
        file: &mut File,
        stamp: SourceStamp,
        reset: bool,
    ) -> io::Result<ReadBatch> {
        let mut bytes = Vec::new();
        if !self.framer.has_buffered() && self.offset < stamp.len {
            file.seek(SeekFrom::Start(self.offset))?;
            file.take((stamp.len - self.offset).min(MAX_BATCH_BYTES as u64))
                .read_to_end(&mut bytes)?;
        }
        let bytes_read = bytes.len();
        let framed = self.framer.admit(&bytes)?;
        self.offset += bytes_read as u64;
        let more_available = framed.more_buffered || self.offset < stamp.len;
        self.source = Some(stamp);
        Ok(ReadBatch {
            records: framed.records,
            bytes_read,
            reset,
            incomplete_tail: framed.incomplete_tail,
            more_available,
        })
    }
}
