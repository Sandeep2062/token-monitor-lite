//! Command-line and environment parsing for the headless agent.
//!
//! Hand-rolled rather than using `clap`: the surface is six flags, and this
//! project pins every dependency and maintains a generated notices table, so a
//! new supply-chain entry has to earn its place. Parsing six flags does not.
//!
//! Precedence is the documented one (`configuration.md`):
//! **CLI flag → env var (real or `.env`) → built-in default.**

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The resolved configuration for one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfig {
    pub once: bool,
    pub dry_run: bool,
    pub clients: ClientSelection,
    pub timezone: Option<String>,
    pub store: Option<PathBuf>,
    pub json: bool,
}

/// What the client selection says.
///
/// Three states, not two. `All` (flag absent, env absent) and `None` (explicitly
/// empty) must not collapse into the same thing: an empty `--clients ""` means
/// "collect nothing", while an absent flag means "collect everything". Collapsing
/// them would silently turn the first into the second.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ClientSelection {
    /// No flag and no env var: use every discovered client.
    #[default]
    All,
    /// An explicit list, possibly empty.
    Some(Vec<String>),
}

impl ClientSelection {
    /// Parse a comma-separated list. An empty string is `Some(vec![])`, not `All`.
    pub fn parse(value: &str) -> Self {
        let clients = value
            .split(',')
            .map(str::trim)
            .filter(|client| !client.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Self::Some(clients)
    }

    /// Whether a client should be collected.
    pub fn includes(&self, client: &str) -> bool {
        match self {
            Self::All => true,
            Self::Some(clients) => clients.iter().any(|item| item == client),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Some(clients) if clients.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Run with this configuration.
    Run(AgentConfig),
    /// Print this text to stdout and exit 0 (`--help`, `--version`).
    Print(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub const USAGE: &str = "\
token-monitor-lite-agent — collect token usage without a UI

USAGE:
    token-monitor-lite-agent [OPTIONS]

OPTIONS:
    --once                 Collect once, persist, print a summary, exit
    --dry-run              Implies --once. No writes, no network
    --clients <csv>        Comma-separated clients; empty disables all
    --timezone <iana>      Bucket timezone
    --store <dir>          Store root
    --json                 Emit the summary as JSON
    --help                 Show this help
    --version              Show the version

ENVIRONMENT (CLI overrides env):
    TOKEN_MONITOR_CLIENTS      Same as --clients
    TOKEN_MONITOR_TIMEZONE     Same as --timezone
    TOKEN_MONITOR_LITE_DATA_DIR  Same as --store

Hub variables are out of scope for this build and are refused rather than
ignored: TOKEN_MONITOR_HUB_URL, TOKEN_MONITOR_SECRET,
TOKEN_MONITOR_DEVICE_ID, TOKEN_MONITOR_SYNC_SESSION_TITLES.
";

/// Variables that belong to the dropped Hub feature.
///
/// Refused explicitly. Ignoring them would leave someone who configured an upload
/// believing it happened.
pub const DROPPED_HUB_VARS: [&str; 4] = [
    "TOKEN_MONITOR_HUB_URL",
    "TOKEN_MONITOR_SECRET",
    "TOKEN_MONITOR_DEVICE_ID",
    "TOKEN_MONITOR_SYNC_SESSION_TITLES",
];

/// Parse arguments, then fall back to `env` for anything not given.
///
/// `env` is passed in rather than read from the process so the precedence rules
/// are testable without mutating the environment (which is process-global and
/// unsafe to do from parallel tests).
pub fn parse(
    args: &[String],
    env: &BTreeMap<String, String>,
    version: &str,
) -> Result<ParseOutcome, ParseError> {
    let mut once = false;
    let mut dry_run = false;
    let mut clients: Option<ClientSelection> = None;
    let mut timezone: Option<String> = None;
    let mut store: Option<PathBuf> = None;
    let mut json = false;

    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "--help" | "-h" => return Ok(ParseOutcome::Print(USAGE.to_owned())),
            "--version" | "-V" => {
                return Ok(ParseOutcome::Print(format!(
                    "token-monitor-lite-agent {version}\n"
                )));
            }
            "--once" => once = true,
            "--dry-run" => dry_run = true,
            "--json" => json = true,
            "--clients" | "--timezone" | "--store" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| ParseError::new(format!("{argument} needs a value")))?;
                match argument {
                    "--clients" => clients = Some(ClientSelection::parse(value)),
                    "--timezone" => timezone = Some(value.clone()),
                    _ => store = Some(PathBuf::from(value)),
                }
            }
            other => {
                // `--flag=value` is the other spelling people reach for.
                if let Some((flag, value)) = other.split_once('=') {
                    match flag {
                        "--clients" => clients = Some(ClientSelection::parse(value)),
                        "--timezone" => timezone = Some(value.to_owned()),
                        "--store" => store = Some(PathBuf::from(value)),
                        _ => {
                            return Err(ParseError::new(format!(
                                "unknown option {flag}\n\n{USAGE}"
                            )));
                        }
                    }
                } else {
                    return Err(ParseError::new(format!(
                        "unknown argument {other}\n\n{USAGE}"
                    )));
                }
            }
        }
        index += 1;
    }

    if let Some(name) = DROPPED_HUB_VARS.iter().find(|name| {
        env.get(**name)
            .is_some_and(|value| !value.trim().is_empty())
    }) {
        return Err(ParseError::new(format!(
            "{name} is set, but Hub sync was dropped from this build.\n\
             Remove it, or use a build that supports Hub sync. Uploading is not \
             silently skipped: nothing was collected or sent."
        )));
    }

    // A dry run implies a single pass: it can never mean "watch forever without
    // writing", which would be a contradiction.
    let once = once || dry_run;

    Ok(ParseOutcome::Run(AgentConfig {
        once,
        dry_run,
        clients: clients.unwrap_or_else(|| match env.get("TOKEN_MONITOR_CLIENTS") {
            Some(value) => ClientSelection::parse(value),
            None => ClientSelection::All,
        }),
        timezone: timezone.or_else(|| {
            env.get("TOKEN_MONITOR_TIMEZONE")
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        }),
        store: store.or_else(|| {
            // The name discovery itself reads, so the CLI and the desktop mean the
            // same thing by "where the data lives".
            env.get("TOKEN_MONITOR_LITE_DATA_DIR")
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        }),
        json,
    }))
}

/// Read a `.env` file into a map.
///
/// Deliberately minimal — `KEY=value`, `#` comments, optional surrounding quotes,
/// optional `export `. It does not do variable expansion or multi-line values,
/// because this app's own settings are the documented surface and inventing a
/// shell dialect would be a compatibility claim we cannot verify.
pub fn parse_dotenv(contents: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = raw.trim();
        // Strip one layer of matching quotes; keep the contents verbatim.
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        values.insert(key.to_owned(), value.to_owned());
    }
    values
}

/// Merge a `.env` map under real environment values.
///
/// The real environment wins: a `.env` file is a convenience, and an operator who
/// exports a variable in the service unit expects that to take effect.
pub fn merge_environment(
    dotenv: BTreeMap<String, String>,
    real: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut merged = dotenv;
    for (key, value) in real {
        merged.insert(key, value);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_owned()).collect()
    }

    fn envmap(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn config(args: &[&str], env_pairs: &[(&str, &str)]) -> AgentConfig {
        match parse(&argv(args), &envmap(env_pairs), "0.1.0").expect("parse") {
            ParseOutcome::Run(config) => config,
            ParseOutcome::Print(text) => panic!("expected a run, got {text}"),
        }
    }

    #[test]
    fn help_and_version_print_instead_of_running() {
        for flag in ["--help", "-h", "--version", "-V"] {
            let outcome = parse(&argv(&[flag]), &envmap(&[]), "9.9.9").expect("parse");
            assert!(matches!(outcome, ParseOutcome::Print(_)), "{flag}");
        }
        match parse(&argv(&["--version"]), &envmap(&[]), "9.9.9").expect("parse") {
            ParseOutcome::Print(text) => assert!(text.contains("9.9.9")),
            _ => panic!("expected version text"),
        }
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let config = config(&[], &[]);
        assert!(!config.once);
        assert!(!config.dry_run);
        assert_eq!(config.clients, ClientSelection::All);
        assert_eq!(config.timezone, None);
        assert_eq!(config.store, None);
        assert!(!config.json);
    }

    #[test]
    fn dry_run_implies_once() {
        let config = config(&["--dry-run"], &[]);
        assert!(config.dry_run);
        assert!(config.once, "a dry run must be a single pass");
    }

    #[test]
    fn a_cli_flag_beats_the_environment() {
        let config = config(
            &["--clients", "claude"],
            &[("TOKEN_MONITOR_CLIENTS", "codex")],
        );
        assert!(config.clients.includes("claude"));
        assert!(!config.clients.includes("codex"));
    }

    #[test]
    fn the_environment_beats_the_default() {
        let config = config(&[], &[("TOKEN_MONITOR_CLIENTS", "codex,claude")]);
        assert!(config.clients.includes("codex"));
        assert!(config.clients.includes("claude"));
        assert!(!config.clients.includes("gemini"));
    }

    #[test]
    fn an_empty_client_list_disables_everything_and_is_not_all() {
        // The distinction that would silently invert the setting if lost.
        let config = config(&["--clients", ""], &[]);
        assert!(config.clients.is_empty());
        assert!(!config.clients.includes("claude"));
        assert_ne!(config.clients, ClientSelection::All);
    }

    #[test]
    fn an_absent_flag_with_no_env_collects_everything() {
        assert_eq!(config(&[], &[]).clients, ClientSelection::All);
    }

    #[test]
    fn equals_form_is_accepted() {
        let config = config(&["--clients=claude,codex", "--timezone=UTC"], &[]);
        assert!(config.clients.includes("codex"));
        assert_eq!(config.timezone.as_deref(), Some("UTC"));
    }

    #[test]
    fn a_missing_value_is_an_error_not_a_default() {
        let error = parse(&argv(&["--clients"]), &envmap(&[]), "1").expect_err("must fail");
        assert!(error.to_string().contains("needs a value"));
    }

    #[test]
    fn an_unknown_argument_is_an_error() {
        let error = parse(&argv(&["--wat"]), &envmap(&[]), "1").expect_err("must fail");
        assert!(error.to_string().contains("unknown argument"));
    }

    #[test]
    fn hub_variables_are_refused_rather_than_ignored() {
        // Someone who configured an upload must not get silence.
        for name in DROPPED_HUB_VARS {
            let error =
                parse(&argv(&[]), &envmap(&[(name, "x")]), "1").expect_err("a hub var must fail");
            assert!(error.to_string().contains(name), "{name}");
            assert!(error.to_string().contains("dropped"), "{name}");
        }
    }

    #[test]
    fn an_empty_hub_variable_is_not_treated_as_configured() {
        // An empty value is how some managers clear a variable.
        let config = config(&[], &[("TOKEN_MONITOR_HUB_URL", "")]);
        assert!(!config.dry_run);
    }

    #[test]
    fn dotenv_parses_the_common_forms() {
        let values = parse_dotenv(
            "# comment\n\
             TOKEN_MONITOR_CLIENTS=claude,codex\n\
             export TOKEN_MONITOR_LITE_DATA_DIR=/tmp/store\n\
             QUOTED=\"a b\"\n\
             SINGLE='c d'\n\
             \n\
             malformed line\n\
             EMPTY=\n",
        );
        assert_eq!(values["TOKEN_MONITOR_CLIENTS"], "claude,codex");
        assert_eq!(values["TOKEN_MONITOR_LITE_DATA_DIR"], "/tmp/store");
        assert_eq!(values["QUOTED"], "a b");
        assert_eq!(values["SINGLE"], "c d");
        assert_eq!(values["EMPTY"], "");
        assert!(!values.contains_key("malformed line"));
    }

    #[test]
    fn the_real_environment_wins_over_dotenv() {
        let merged = merge_environment(
            parse_dotenv("TOKEN_MONITOR_CLIENTS=from-file\nKEEP=file-only"),
            [("TOKEN_MONITOR_CLIENTS".to_owned(), "from-env".to_owned())],
        );
        assert_eq!(merged["TOKEN_MONITOR_CLIENTS"], "from-env");
        assert_eq!(merged["KEEP"], "file-only");
    }

    #[test]
    fn whitespace_in_a_client_list_is_trimmed() {
        let config = config(&["--clients", " claude , codex "], &[]);
        match &config.clients {
            ClientSelection::Some(clients) => {
                assert_eq!(clients, &vec!["claude".to_owned(), "codex".to_owned()]);
            }
            _ => panic!("expected an explicit list"),
        }
    }
}
