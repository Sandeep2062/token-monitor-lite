//! OmO Native (Senpi) JSONL sessions. Senpi and OmO task children use the Pi
//! record format; `usage.reasoning` stays informational because it is a subset
//! of `output`.

use super::pi::PiReader;

pub type SenpiReader = PiReader;

pub fn reader(session: impl Into<String>) -> SenpiReader {
    PiReader::new("senpi", session)
}
