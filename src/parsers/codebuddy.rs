//! CodeBuddy JSONL transcript reader. See `tencent_buddy` for the shared
//! record contract. Extension-log sinks (`ide-extension.log`) are not read.

use super::tencent_buddy::BuddyReader;

pub type CodeBuddyReader = BuddyReader;

pub fn reader(session: impl Into<String>) -> CodeBuddyReader {
    BuddyReader::new("codebuddy", "codebuddy", session)
}
