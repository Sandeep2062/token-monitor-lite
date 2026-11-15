//! CodeBuddy JSONL transcript reader. See `tencent_buddy` for the shared
//! record contract. Extension-log sinks (`ide-extension.log`) are not read.

use super::tencent_buddy::{BuddyLogReader, BuddyReader};

pub type CodeBuddyReader = BuddyReader;

pub type CodeBuddyLogReader = BuddyLogReader;

pub fn reader(session: impl Into<String>) -> CodeBuddyReader {
    BuddyReader::new("codebuddy", "codebuddy", session)
}

pub fn log_reader(session: impl Into<String>) -> CodeBuddyLogReader {
    BuddyLogReader::new("codebuddy", "codebuddy", session)
}
