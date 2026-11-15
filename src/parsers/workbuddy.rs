//! WorkBuddy JSONL transcript reader. See `tencent_buddy` for the shared
//! record contract. The older `workbuddy.db` aggregate fallback and extension
//! log sinks are not read.

use super::tencent_buddy::BuddyReader;

pub type WorkBuddyReader = BuddyReader;

pub fn reader(session: impl Into<String>) -> WorkBuddyReader {
    BuddyReader::new("workbuddy", "workbuddy", session)
}
