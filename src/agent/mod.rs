//! The headless agent: collection without a UI.
//!
//! See `docs/migration/headless-spec.md`. This module is **local only** — Hub
//! sync, iCloud, and plugins were dropped from the migration, so there is no
//! upload path, no device identity, and no title sync.
//!
//! The desktop core is reused rather than reimplemented: `discover()` and
//! `Monitor` are the same functions the window calls.

pub mod config;
pub mod dry_run;
pub mod run;
pub mod summary;

pub use config::{AgentConfig, ClientSelection, ParseOutcome, USAGE, parse, parse_dotenv};
pub use dry_run::NullPersistence;
pub use run::{AgentError, run_once};
pub use summary::Summary;
