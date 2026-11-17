//! Reconciliation invariants for the client catalog, the reader modules, the
//! inventory's usage-client table, and the test counts it cites.
//!
//! These were previously checked by hand. A hand check is a snapshot of one
//! afternoon's diligence: it found missing evidence rows, three stale duplicate
//! rows, and two wrong test counts, and it will drift again within a couple of
//! commits. Making it a test turns "every catalog client has exactly one
//! evidence row, none missing, none extra, and every cited test file exists with
//! the stated count" into a claim that verifies itself on every run — including
//! as the catalog grows.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    let path = repo().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Catalog client id to the parser module that reads it.
///
/// Most clients map to a module of the same name. The rest are aliases, and
/// each alias is deliberate: the module is shared, so the client id and the
/// module name legitimately differ.
fn module_for(client: &str) -> String {
    match client {
        "prime-agent" => "prime_agent".to_owned(),
        // CodeBuddy and WorkBuddy share the Tencent transcript contract.
        "codebuddy" | "workbuddy" => "tencent_buddy".to_owned(),
        // OMP and Senpi are Pi-format clients read by the Pi reader.
        "omp" | "senpi" => "pi".to_owned(),
        other => other.replace('-', "_"),
    }
}

fn catalog_clients() -> Vec<String> {
    let document: serde_json::Value =
        serde_json::from_str(&read("assets/catalog.json")).expect("catalog is valid JSON");
    document["clients"]
        .as_array()
        .expect("catalog clients is an array")
        .iter()
        .map(|client| {
            client["id"]
                .as_str()
                .expect("every client has an id")
                .to_owned()
        })
        .collect()
}

/// The client ids in the inventory's usage-client table, in document order.
fn inventory_usage_clients() -> Vec<String> {
    let inventory = read("docs/migration/inventory.md");
    let mut clients = Vec::new();
    let mut in_usage_section = false;
    for line in inventory.lines() {
        if line.starts_with("## ") {
            in_usage_section = line.trim() == "## Usage clients";
            continue;
        }
        if !in_usage_section {
            continue;
        }
        // `| [ ] | \`claude\` / Claude Code | ...`
        let Some(rest) = line.strip_prefix("| [") else {
            continue;
        };
        let Some(rest) = rest.split_once("| `").map(|(_, rest)| rest) else {
            continue;
        };
        let Some((id, _)) = rest.split_once('`') else {
            continue;
        };
        clients.push(id.to_owned());
    }
    clients
}

#[test]
fn every_catalog_client_has_exactly_one_usage_evidence_row() {
    let catalog = catalog_clients();
    let inventory = inventory_usage_clients();

    let catalog_set: BTreeSet<&String> = catalog.iter().collect();
    let inventory_set: BTreeSet<&String> = inventory.iter().collect();

    let missing: Vec<&&String> = catalog_set.difference(&inventory_set).collect();
    let extra: Vec<&&String> = inventory_set.difference(&catalog_set).collect();
    assert!(
        missing.is_empty(),
        "catalog clients with no usage evidence row: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "usage evidence rows with no catalog client: {extra:?}"
    );

    // Duplicates are the failure mode a set comparison hides: three stale
    // droid/kimi/grok rows once sat beside the real ones and balanced the count.
    let mut seen = BTreeSet::new();
    let duplicates: Vec<&String> = inventory
        .iter()
        .filter(|id| !seen.insert((*id).clone()))
        .collect();
    assert!(
        duplicates.is_empty(),
        "duplicate usage evidence rows: {duplicates:?}"
    );

    assert_eq!(catalog.len(), inventory.len());
}

#[test]
fn every_catalog_client_has_a_parser_module() {
    let declared = read("src/parsers/mod.rs");
    let mut missing = Vec::new();
    for client in catalog_clients() {
        let module = module_for(&client);
        let file = repo().join(format!("src/parsers/{module}.rs"));
        let declared_module = declared.contains(&format!("pub mod {module};"));
        if !file.is_file() || !declared_module {
            missing.push(format!("{client} -> {module}"));
        }
    }
    assert!(
        missing.is_empty(),
        "catalog clients with no reader module: {missing:?}"
    );
}

#[test]
fn every_parser_module_is_reachable_from_a_catalog_client() {
    // The reverse direction. A module nobody can reach is dead weight, and a
    // module for a client that is not in the catalog cannot be configured.
    let mut expected: BTreeSet<String> = catalog_clients().iter().map(|c| module_for(c)).collect();
    // Shared support modules that are not clients themselves.
    expected.insert("jsonl".to_owned());
    expected.insert("timestamps".to_owned());
    // Thin alias modules: each re-exports a shared reader under its own client
    // name, so the client id and the module name legitimately differ.
    for alias in ["codebuddy", "workbuddy", "omp", "senpi"] {
        expected.insert(alias.to_owned());
    }
    // A second reader for a client whose original module keeps the primary
    // name. Copilot is read both from its OTel JSONL trace (`copilot`) and from
    // VS Code's chat-session operation log (`copilot_chat`).
    expected.insert("copilot_chat".to_owned());

    let declared = read("src/parsers/mod.rs");
    let declared_modules: BTreeSet<String> = declared
        .lines()
        .filter_map(|line| line.strip_prefix("pub mod "))
        .filter_map(|line| line.strip_suffix(';'))
        .map(str::to_owned)
        .collect();

    let unreachable: Vec<&String> = declared_modules.difference(&expected).collect();
    assert!(
        unreachable.is_empty(),
        "parser modules no catalog client reaches: {unreachable:?}"
    );
}

/// `tests/<file>.rs` -> the test count the inventory states for it.
fn cited_test_counts() -> BTreeMap<String, usize> {
    let inventory = read("docs/migration/inventory.md");
    let mut counts = BTreeMap::new();
    let mut rest = inventory.as_str();
    // `tests/claude.rs` (14 tests)
    while let Some(start) = rest.find("`tests/") {
        rest = &rest[start + 1..];
        let Some(close) = rest.find('`') else { break };
        let file = &rest[..close];
        let after = &rest[close..];
        let Some(open) = after.find('(') else {
            continue;
        };
        let tail = &after[open + 1..];
        let Some(end) = tail.find(')') else { continue };
        let inner = &tail[..end];
        let digits: String = inner.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(count) = digits.parse::<usize>()
            && inner.contains("test")
        {
            counts.insert(file.to_owned(), count);
        }
    }
    counts
}

#[test]
fn every_cited_test_file_exists_with_the_stated_count() {
    let cited = cited_test_counts();
    assert!(
        !cited.is_empty(),
        "the inventory cites no test counts; the parser broke"
    );

    let mut problems = Vec::new();
    for (file, stated) in &cited {
        let path = repo().join(file);
        if !path.is_file() {
            problems.push(format!("{file}: cited but missing"));
            continue;
        }
        let actual = count_tests(&path);
        if actual != *stated {
            problems.push(format!("{file}: stated {stated}, actual {actual}"));
        }
    }
    assert!(
        problems.is_empty(),
        "cited test counts disagree: {problems:?}"
    );
}

fn count_tests(path: &Path) -> usize {
    let source = fs::read_to_string(path).unwrap_or_default();
    source
        .lines()
        .filter(|line| line.trim() == "#[test]")
        .count()
}

#[test]
fn the_reader_contract_states_the_declined_record_class() {
    // The contract is the authority the readers are written against. If it
    // stops naming the record-level outcome, a reader author has no way to know
    // that rejecting a whole source for one bad record is the wrong choice.
    let contract = read("docs/migration/reader-contracts.md");
    assert!(contract.contains("**declined record**"));
    assert!(contract.contains("`declined_records`"));
    // And the reason, so the class is not quietly collapsed back later.
    assert!(contract.contains("freezes that source's totals permanently"));
}
