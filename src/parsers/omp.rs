//! Oh My Pi JSONL sessions. OMP writes the Pi record format under
//! `~/.omp/agent/sessions`; subagent transcripts are standalone sessions.

use super::pi::PiReader;

pub type OmpReader = PiReader;

pub fn reader(session: impl Into<String>) -> OmpReader {
    PiReader::new("omp", session)
}
