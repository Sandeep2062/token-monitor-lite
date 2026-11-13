use token_monitor_lite::accounting::{SessionKey, TokenCounts};
use token_monitor_lite::catalog;

#[test]
fn downward_corrections_stay_signed() {
    let old = TokenCounts {
        input: 100,
        output: 20,
        ..Default::default()
    };
    let new = TokenCounts {
        input: 75,
        output: 0,
        ..Default::default()
    };
    assert_eq!(new.difference(old).input, -25);
    assert_eq!(new.difference(old).output, -20);
}

#[test]
fn counts_above_f64_precision_remain_exact() {
    let count = TokenCounts {
        input: 9_007_199_254_740_993,
        ..Default::default()
    };
    assert_eq!(
        count
            .checked_add(TokenCounts {
                input: 1,
                ..Default::default()
            })
            .unwrap()
            .input,
        9_007_199_254_740_994
    );
}

#[test]
fn overflow_is_not_clamped() {
    let count = TokenCounts {
        input: u64::MAX,
        ..Default::default()
    };
    assert!(
        count
            .checked_add(TokenCounts {
                input: 1,
                ..Default::default()
            })
            .is_err()
    );
}

#[test]
fn every_component_preserves_exact_addition_and_delta() {
    let a = TokenCounts {
        input: 1,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        reasoning: 5,
    };
    let b = TokenCounts {
        input: 5,
        output: 4,
        cache_read: 3,
        cache_write: 2,
        reasoning: 1,
    };
    assert_eq!(
        a.checked_add(b).unwrap(),
        TokenCounts {
            input: 6,
            output: 6,
            cache_read: 6,
            cache_write: 6,
            reasoning: 6,
        }
    );
    let delta = a.difference(b);
    assert_eq!(
        (
            delta.input,
            delta.output,
            delta.cache_read,
            delta.cache_write,
            delta.reasoning
        ),
        (-4, -2, 0, 2, 4)
    );
}

#[test]
fn client_id_is_part_of_the_session_partition() {
    assert_ne!(
        SessionKey::new("claude", "same-id"),
        SessionKey::new("codex", "same-id")
    );
}

#[test]
fn catalogs_keep_pinned_order_defaults_and_provider_labels() {
    let data = catalog::load().unwrap();
    assert_eq!(data.clients.len(), 42);
    assert_eq!(data.providers.len(), 34);
    assert_eq!(data.clients.first().unwrap().id, "claude");
    assert_eq!(data.clients.last().unwrap().id, "jcode");
    let off: Vec<_> = data
        .clients
        .iter()
        .filter(|c| !c.default_tracked)
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(off, ["catpaw", "qodercn"]);
    let forks: Vec<_> = data
        .clients
        .iter()
        .filter(|c| c.fork_only)
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(forks, ["catpaw", "qodercn"]);
    assert_eq!(
        data.provider_for_client("claude").unwrap().settings_label(),
        "Claude Code"
    );
    assert_eq!(
        data.provider_for_client("zcode").unwrap().settings_label(),
        "Z.ai / GLM"
    );
}

#[test]
fn provider_mapping_is_catalog_scoped_not_usage_scoped() {
    let data = catalog::load().unwrap();
    for (client, provider) in [
        ("droid", "factory"),
        ("zcode", "zai"),
        ("qodercn", "qoder"),
        ("dsh", "deepseek"),
        ("mcode", "minimax"),
    ] {
        assert_eq!(data.provider_for_client(client).unwrap().id, provider);
    }
    assert!(data.provider_for_client("non-catalog-parser-id").is_none());
    assert!(data.provider_for_client("pi").is_none());
}

#[test]
fn every_component_detects_overflow_and_keeps_full_range_signed_deltas() {
    let max = TokenCounts {
        input: u64::MAX,
        output: u64::MAX,
        cache_read: u64::MAX,
        cache_write: u64::MAX,
        reasoning: u64::MAX,
    };
    for one in [
        TokenCounts {
            input: 1,
            ..Default::default()
        },
        TokenCounts {
            output: 1,
            ..Default::default()
        },
        TokenCounts {
            cache_read: 1,
            ..Default::default()
        },
        TokenCounts {
            cache_write: 1,
            ..Default::default()
        },
        TokenCounts {
            reasoning: 1,
            ..Default::default()
        },
    ] {
        assert!(max.checked_add(one).is_err());
    }
    let delta = TokenCounts::default().difference(max);
    assert_eq!(delta.input, -i128::from(u64::MAX));
    assert_eq!(delta.output, delta.input);
    assert_eq!(delta.cache_read, delta.input);
    assert_eq!(delta.cache_write, delta.input);
    assert_eq!(delta.reasoning, delta.input);
}

#[test]
fn provider_lookup_preserves_reference_normalization_and_identity_cases() {
    let data = catalog::load().unwrap();
    assert_eq!(data.provider_for_client("  DrOiD  ").unwrap().id, "factory");
    assert_eq!(
        data.provider_for_client("openrouter").unwrap().id,
        "openrouter"
    );
    for unknown in ["", "   ", "synthetic", "future-reader"] {
        assert!(data.provider_for_client(unknown).is_none());
    }
}
