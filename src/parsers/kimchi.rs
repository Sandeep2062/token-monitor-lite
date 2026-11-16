//! Kimchi Coding JSONL sessions. Kimchi writes the Pi record format under its
//! own agent directory, so this reuses the shared [`PiReader`] and only stamps
//! the distinct `kimchi` client id — the same shape as `senpi` and `omp`.
//!
//! See docs/migration/parser-provenance.md for the pinned reference.

use super::pi::PiReader;

pub type KimchiReader = PiReader;

pub fn reader(session: impl Into<String>) -> KimchiReader {
    PiReader::new("kimchi", session)
}
