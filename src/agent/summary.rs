//! The run summary.
//!
//! A scheduled run's only output. Two rules shape it:
//!
//! 1. **A source that was never read is reported as such.** The failure this
//!    exists to catch is a scheduled job that exits 0 every night while reading
//!    nothing, so silence must not look like success.
//! 2. **No paths, no tokens, no home directory.** Source identity is what
//!    discovery already made opaque, and a client name is enough to act on.

use serde::Serialize;

use crate::runtime::{MonitorSnapshot, SourceHealthStatus};

/// One source's outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SourceSummary {
    pub source: String,
    pub client: String,
    pub status: String,
    /// Whether the source was read at all. `false` with `Ok` overall is the
    /// silent-empty case.
    pub read: bool,
}

/// One client's rollup.
///
/// A real machine has hundreds of sources — 460 on the development host, nearly
/// all of them one client's per-session files. Listing each is not a summary, so
/// the text form aggregates by client and the JSON keeps the per-source detail
/// for anything that wants to diff it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClientSummary {
    pub client: String,
    pub sources: usize,
    pub read: usize,
    pub failed: usize,
}

/// Everything one run reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub dry_run: bool,
    /// Per-client rollups, ordered by client name.
    pub clients: Vec<ClientSummary>,
    /// Per-source detail. Present in JSON; the text form lists only problems.
    pub sources: Vec<SourceSummary>,
    pub tokens: u128,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Sources that were discovered but never successfully read.
    pub unreadable: usize,
    /// Total commits the run made, or would have made under `--dry-run`.
    pub commits: usize,
}

impl Summary {
    /// Build from a monitor snapshot.
    ///
    /// `commits` is passed in because a dry run counts would-be commits, and the
    /// monitor has no notion of that.
    pub fn from_snapshot(snapshot: &MonitorSnapshot, dry_run: bool, commits: usize) -> Self {
        let mut sources = snapshot
            .source_health
            .iter()
            .map(|health| SourceSummary {
                // The opaque identity discovery produced — never a real path.
                source: health.source_id.as_storage_key(),
                client: health.format.client().to_owned(),
                status: status_label(health.status).to_owned(),
                read: is_read(health.status),
            })
            .collect::<Vec<_>>();
        sources.sort_by(|left, right| {
            (&left.client, &left.source).cmp(&(&right.client, &right.source))
        });

        // Roll up per client, so the text form stays readable on a machine with
        // hundreds of session files.
        let mut by_client: std::collections::BTreeMap<&str, ClientSummary> =
            std::collections::BTreeMap::new();
        for source in &sources {
            let entry = by_client
                .entry(source.client.as_str())
                .or_insert_with(|| ClientSummary {
                    client: source.client.clone(),
                    sources: 0,
                    read: 0,
                    failed: 0,
                });
            entry.sources += 1;
            if source.read {
                entry.read += 1;
            } else {
                entry.failed += 1;
            }
        }
        let clients = by_client.into_values().collect::<Vec<_>>();

        let totals = &snapshot.canonical.totals;
        Self {
            dry_run,
            unreadable: sources.iter().filter(|source| !source.read).count(),
            clients,
            sources,
            // The same helper the desktop shows, so the CLI cannot disagree with
            // the window about what a total means.
            tokens: crate::analytics::token_total(*totals),
            input: totals.input,
            output: totals.output,
            cache_read: totals.cache_read,
            cache_write: totals.cache_write,
            reasoning: totals.reasoning,
            commits,
        }
    }

    /// Whether the run found anything worth reporting.
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty() && self.tokens == 0
    }

    /// Human-readable form.
    ///
    /// Aggregated by client, and only failing sources are listed individually —
    /// on a machine with 460 sources, one line each is not a summary a person or a
    /// scheduler's log can use.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(if self.dry_run {
            "Token Monitor Lite — dry run (no writes, no network)\n"
        } else {
            "Token Monitor Lite — collection run\n"
        });

        let total_sources = self
            .clients
            .iter()
            .map(|client| client.sources)
            .sum::<usize>();
        out.push_str(&format!("\nClients: {total_sources} source(s)\n"));
        if self.clients.is_empty() {
            out.push_str("  (none discovered)\n");
        }
        for client in &self.clients {
            if client.failed > 0 {
                out.push_str(&format!(
                    "  {:<14} {:>4} source(s), {} readable, {} failed\n",
                    client.client, client.sources, client.read, client.failed
                ));
            } else {
                out.push_str(&format!(
                    "  {:<14} {:>4} source(s), all readable\n",
                    client.client, client.sources
                ));
            }
        }

        let failures = self
            .sources
            .iter()
            .filter(|source| !source.read)
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            out.push_str("\nUnreadable sources:\n");
            for source in &failures {
                out.push_str(&format!(
                    "  {:<14} {:<22} {}\n",
                    source.client, source.status, source.source
                ));
            }
        }

        out.push_str(&format!(
            "\nTokens: {} (input {}, output {}, cache read {}, cache write {}, reasoning {})\n",
            self.tokens, self.input, self.output, self.cache_read, self.cache_write, self.reasoning
        ));
        out.push_str(&format!(
            "{}: {}\n",
            if self.dry_run {
                "Commits (would write)"
            } else {
                "Commits"
            },
            self.commits
        ));
        if self.unreadable > 0 {
            out.push_str(&format!(
                "\nWarning: {} source(s) could not be read.\n",
                self.unreadable
            ));
        }
        out
    }

    /// Machine-readable form, one JSON object.
    pub fn render_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned());
        text.push('\n');
        text
    }
}

/// A stable label for a health status.
fn status_label(status: SourceHealthStatus) -> &'static str {
    match status {
        SourceHealthStatus::Pending => "pending",
        SourceHealthStatus::Healthy => "healthy",
        SourceHealthStatus::Retained => "retained",
        SourceHealthStatus::ReadFailed => "read-failed",
        SourceHealthStatus::DecodingFailed => "decode-failed",
        SourceHealthStatus::SourceBusy => "busy",
        SourceHealthStatus::Unsupported => "unsupported",
        SourceHealthStatus::CapacityExceeded => "capacity",
        SourceHealthStatus::CanonicalizationFailed => "canonicalize-failed",
        SourceHealthStatus::PersistenceDelayed => "persistence-delayed",
    }
}

/// Whether a status means the source was actually read.
///
/// `Pending` counts as not read: a source that never got its turn has not been
/// read, and calling that success is exactly the silent-empty bug.
fn is_read(status: SourceHealthStatus) -> bool {
    matches!(
        status,
        SourceHealthStatus::Healthy | SourceHealthStatus::Retained
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_source_is_not_read() {
        // The silent-empty case: discovered, never read, must not look healthy.
        assert!(!is_read(SourceHealthStatus::Pending));
    }

    #[test]
    fn healthy_and_retained_are_read() {
        assert!(is_read(SourceHealthStatus::Healthy));
        assert!(is_read(SourceHealthStatus::Retained));
    }

    #[test]
    fn every_failure_status_is_not_read() {
        for status in [
            SourceHealthStatus::ReadFailed,
            SourceHealthStatus::DecodingFailed,
            SourceHealthStatus::SourceBusy,
            SourceHealthStatus::Unsupported,
            SourceHealthStatus::CapacityExceeded,
            SourceHealthStatus::CanonicalizationFailed,
            SourceHealthStatus::PersistenceDelayed,
        ] {
            assert!(!is_read(status), "{status:?}");
        }
    }

    #[test]
    fn every_status_has_a_stable_label() {
        // A label that varies would break a scheduled job's log parsing.
        assert_eq!(status_label(SourceHealthStatus::Healthy), "healthy");
        assert_eq!(status_label(SourceHealthStatus::ReadFailed), "read-failed");
        assert_eq!(status_label(SourceHealthStatus::Pending), "pending");
    }

    #[test]
    fn an_empty_summary_reports_nothing_found() {
        let summary = Summary {
            dry_run: true,
            clients: Vec::new(),
            sources: Vec::new(),
            tokens: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            unreadable: 0,
            commits: 0,
        };
        assert!(summary.is_empty());
        let text = summary.render_text();
        assert!(text.contains("(none discovered)"));
        assert!(text.contains("dry run"));
    }

    #[test]
    fn the_text_names_the_commit_count_differently_for_a_dry_run() {
        let base = Summary {
            dry_run: false,
            clients: Vec::new(),
            sources: Vec::new(),
            tokens: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            unreadable: 0,
            commits: 3,
        };
        assert!(base.render_text().contains("\nCommits: 3"));
        let dry = Summary {
            dry_run: true,
            ..base
        };
        assert!(dry.render_text().contains("Commits (would write): 3"));
    }

    #[test]
    fn unreadable_sources_are_called_out() {
        let summary = Summary {
            dry_run: false,
            clients: Vec::new(),
            sources: vec![SourceSummary {
                source: "opaque-a".to_owned(),
                client: "claude".to_owned(),
                status: "read-failed".to_owned(),
                read: false,
            }],
            tokens: 0,
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            unreadable: 1,
            commits: 0,
        };
        assert!(summary.render_text().contains("could not be read"));
    }

    #[test]
    fn the_json_is_one_parseable_object_with_a_trailing_newline() {
        let summary = Summary {
            dry_run: true,
            clients: Vec::new(),
            sources: Vec::new(),
            tokens: 5,
            input: 5,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            unreadable: 0,
            commits: 1,
        };
        let text = summary.render_json();
        assert!(text.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["tokens"], 5);
        assert_eq!(value["dry_run"], true);
    }
}
