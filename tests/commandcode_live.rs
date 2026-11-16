//! Live check: the Command Code reader against a real transcript on this
//! machine.
//!
//! This is a manual probe rather than an assertion, because the transcript is
//! user data: its exact counts change as sessions accumulate. It is `#[ignore]`
//! so a normal run stays hermetic, and it prints what it found instead of
//! pinning numbers that would fail on the next session.

use std::path::PathBuf;

use token_monitor_lite::parsers::commandcode::CommandCodeReader;

/// The fixture-independent probe over `$COMMANDCODE_PROBE_FILE`.
#[test]
#[ignore = "reads a real transcript from this machine, not a fixture"]
fn probe_a_real_command_code_transcript() {
    let Some(path) = std::env::var_os("COMMANDCODE_PROBE_FILE").map(PathBuf::from) else {
        eprintln!("COMMANDCODE_PROBE_FILE is not set; nothing to probe");
        return;
    };
    let data = std::fs::read(&path).expect("probe file is readable");
    let mut reader = CommandCodeReader::new("probe");
    for line in data.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        reader.ingest(line).expect("each line is accepted");
    }
    let rows = reader.snapshot();
    let total: u64 = rows
        .iter()
        .map(|row| {
            row.tokens.input
                + row.tokens.output
                + row.tokens.cache_read
                + row.tokens.cache_write
                + row.tokens.reasoning
        })
        .sum();
    eprintln!("rows={} total_tokens={}", rows.len(), total);
    for row in rows.iter().take(3) {
        eprintln!(
            "  model={} session={} in={} out={} cr={} cw={}",
            row.model,
            row.key.session,
            row.tokens.input,
            row.tokens.output,
            row.tokens.cache_read,
            row.tokens.cache_write
        );
    }
    assert!(
        !rows.is_empty(),
        "a real transcript should yield observations"
    );
}
