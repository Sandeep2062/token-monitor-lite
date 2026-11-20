//! `token-monitor-lite-agent` — the headless entry point.
//!
//! A separate binary rather than a mode of the desktop one: `src/main.rs` starts
//! with `slint::include_modules!()` and is gated on the `desktop` feature, so this
//! must not link through it. See `docs/migration/headless-spec.md`.
//!
//! Exit codes: `0` success, `2` bad usage or configuration, `1` runtime failure.

use std::collections::BTreeMap;
use std::process::ExitCode;

use token_monitor_lite::agent::config::merge_environment;
use token_monitor_lite::agent::config::{ParseOutcome, parse, parse_dotenv};
use token_monitor_lite::agent::{AgentError, run_once};
use token_monitor_lite::discovery::HostPaths;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();

    // `.env` sits *under* the real environment: an operator who exports a
    // variable in a service unit expects that to win.
    let dotenv = std::fs::read_to_string(".env")
        .map(|contents| parse_dotenv(&contents))
        .unwrap_or_default();
    let environment = merge_environment(
        dotenv,
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .collect::<BTreeMap<_, _>>(),
    );

    let config = match parse(&args, &environment, VERSION) {
        Ok(ParseOutcome::Print(text)) => {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
        Ok(ParseOutcome::Run(config)) => config,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };

    if !config.once {
        // A watch mode is deliberately not implemented: with Hub sync gone there is
        // no upload to keep current, so a long-running agent would only re-read the
        // same files. Refusing is honest; silently running once and exiting would
        // look like a daemon that died.
        eprintln!(
            "error: continuous mode is not implemented in this build.\n\
             Use --once from a scheduler (cron, systemd timer, Task Scheduler).\n\
             Hub sync — the reason the upstream agent runs continuously — was dropped."
        );
        return ExitCode::from(2);
    }

    let paths = match HostPaths::current() {
        Ok(paths) => paths,
        Err(_) => {
            eprintln!("error: could not determine home and data directories");
            return ExitCode::from(1);
        }
    };

    match run_once(&config, paths) {
        Ok(summary) => {
            if config.json {
                print!("{}", summary.render_json());
            } else {
                print!("{}", summary.render_text());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            match error {
                AgentError::Discovery(_) | AgentError::Store => ExitCode::from(2),
                _ => ExitCode::from(1),
            }
        }
    }
}
