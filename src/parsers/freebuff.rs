//! Freebuff chat-history reader.
//!
//! Freebuff persists the same `chat-messages.json` shape as Codebuff and is
//! parsed through the same helpers; only the client id differs. The two lanes
//! are partitioned by path: a `free` segment belongs to Freebuff, and
//! everything else to Codebuff, so no chat can be counted twice.

use super::codebuff::CodebuffReader;
pub use super::codebuff::{parse_chat_id_to_millis, session_from_path};

pub const CLIENT: &str = "freebuff";

pub fn reader(session: impl Into<String>) -> CodebuffReader {
    CodebuffReader::new_for(CLIENT, session)
}

/// Freebuff's lane marker: a `free` path segment.
///
/// The upstream marker used to be a fixed `base2-free` prefix and then moved to
/// `base3-free*` plus a `base3-fast-free-*` lane. Matching the segment rather
/// than a prefix is what survives those renames.
pub fn is_freebuff_path(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|segment| segment.eq_ignore_ascii_case("free"))
    })
}
