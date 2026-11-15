# Gate 2A parser provenance

Token Monitor Lite is the only product. The Token Monitor reference and Tokscale pin are read-only evidence: they are not executed, linked, loaded, or packaged. Native code selectively reimplements reviewed behavior and remains covered by the repository's MIT license, Copyright (c) 2026 Sandeep Khadka.

## Immutable evidence pins

| Evidence | Exact revision | Retained location | Verification |
|---|---|---|---|
| Tokscale (`Javis603/tokscale`) | `d3df620b2cdd9502c70aececdbc6ece61fe26fc9` | `/home/azureuser/.local/state/token-monitor-lite/evidence/tokscale-d3df620b2cdd9502c70aececdbc6ece61fe26fc9` | Verification helper checked 31 retained source/fixture entries with `failures: []`; no manifest-only entries. |
| Token Monitor `0.88.0` | `2e55113488888d2e784f159cd92f2d4eee157fbd` | `/home/azureuser/.local/state/token-monitor-lite/evidence/token-monitor-2e55113488888d2e784f159cd92f2d4eee157fbd` | Verification helper checked all 14 manifest entries with `failures: []`; no manifest-only entries. |

The evidence directories contain source and manifests, not runnable reference applications. Behavior was established by read-only source/fixture inspection. Future branches or similarly named files are not acceptable substitutes.

## Tokscale sources used by the matrix

All paths below are relative to the exact Tokscale revision and MIT-covered by `LICENSE` (1,066 bytes, SHA-256 `24a794f325f7625b5f124945dedb6dcec8188f88e76bf4b99557b52d6cc77be9`).

| Relative path | Bytes | SHA-256 | Contract evidence |
|---|---:|---|---|
| `crates/tokscale-core/src/sessions/claudecode.rs` | 229,158 | `343ff95ced60a21ba38f30f30613b8bc7d5f8a535c1cc6136eb4a809caf7062c` | Claude ordinary assembly, replay identity/completeness, cache split and one-hour writes, sidechain ownership, advisor/tool results, headless/result streams, timestamps, provider/model, Desktop/cc-mirror context. |
| `crates/tokscale-core/src/sessions/utils.rs` | retained manifest | `b11f0a509bfcee4c8a20504067b9e65ddd59e1f8ae35b6fcbe335c3c26791c2a` | Shared usage extraction, zstd suffix handling, timestamps, session IDs, and bounded line helpers. |
| `crates/tokscale-core/src/sessions/codex.rs` | 364,659 | `ea5f1ca08fd35a30ade822a5bd5ea2465ca6511c9137171ac5eba8274c6c9e2c` | Codex last/total transitions, stale/reset rules, usage-record settlement, compaction, turn/fork/subagent gates, provider/tier, unknown-model buffering, zstd decode and bounds. |
| `crates/tokscale-core/src/sessions/codex_timing.rs` | 33,804 | `22792fa0b017769ba87ee99f9a2d1a55af2bcaebbe0c88e3446ac6cba8581743` | Codex turn/duration timestamp state and fallback evidence. Duration is not yet a Gate 2A persisted claim. |
| `crates/tokscale-core/src/scanner.rs` | retained manifest | `b37f1d547d8f3d43eecd69b2bfc7b7198b0e4a14fee694f44ec98e573a442a57` | Exact Claude/Codex roots and Codex live/archived `.jsonl.zst` discovery roles. |
| `crates/tokscale-core/src/claude_desktop/mod.rs` | 30,645 | `b5f5dc2aa34b49408928f1b1511ccdce0a702178134d834bc6810ce933273913` | Claude Desktop transcript authority and ownership evidence. Gate 2A does not ingest IndexedDB or arbitrary Desktop storage. |
| `crates/tokscale-core/tests/codex_parent_link.rs` | 4,348 | `273cfdda0fec2b3b1fc95ca1ec066dae96dabb1a68a4bdd250e2d715b0305315` | Parent/subagent ownership and replay suppression fixtures. |
| `crates/tokscale-core/src/sessions/opencodex.rs` | 47,759 | `8a84dcfa4d515e75df042172216565ca1dec298d890bf28fae6af5fe3b6056fb` | Supplemental/native-overlap evidence only. It does not authorize a new Gate 2A input format. |
| `crates/tokscale-core/src/sessions/opencode.rs` | 105,568 | `0c749edc4e7d4fb91c8e51c5c1b4fb9b1e352c21b0e7fff1da1efc7362dd34a7` | OpenCode v1 message parsing, strict cache, model/provider resolution, message-ID identity, and the clamp/truncate behavior the native reader deliberately rejects. Pinned-reader evidence re-read at the exact revision; not part of the original 31-entry retained manifest. |
| `crates/tokscale-core/src/sessions/opencode_schema.rs` | 90,101 | `c913a72caace8b2aa655393e50b2d7674d34598bafa8c9a5a1eb052722d2a219` | OpenCode v1/v2 SQLite structural queries, dual-schema model/provider resolution, strict cache, inclusive-output/reasoning extraction, and fingerprint dedup. Pinned-reader evidence re-read at the exact revision; not part of the original 31-entry retained manifest. |
| `crates/tokscale-core/src/sessions/gemini.rs` | 49,239 | `ce84c732dde2aeeabdd776d4d1c322f98e0530d91d1659fd0f2d4e09a059e6f8` | Gemini session-document, headless JSONL/direct-event, stats/model attribution, token aliases, cache-inclusive prompt normalization, tool-in-input, and file-mtime fallback. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/cursor.rs` | 69,169 | `553240552b92085a16419273c90d1baa00e007a2a203027af395ba598f581fb7` | Cursor read-only usage-cache JSON events, conversation/synthetic session identity, event snapshots and largest-total collapse, token breakdown, and per-file dedup keys. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/cline.rs` | 23,315 | `9573f7eb052f0bafad1d0ffbe1959e644cca87a35d1c918789dbeb8b75cca9e7` | Cline CLI/Desktop messages, gross-input cache subtraction, model/provider/agent attribution, message-ID/ordinal identity, and manifest fallback. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/amp.rs` | 24,116 | `4e03208386ea11742238716a82b2ba18aa4d33ee00b53cdd4ab21a13577bb74f` | Amp thread ledger events, assistant message usage, timestamp fallbacks, component extraction, and cost/credits. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/kilo.rs` | 10,790 | `81862b79dd71773c615162fdf67b08eab1e3027f2bec6cb9d8abf2dfe41144df` | Kilo OpenCode-schema SQLite message selection, top-level model/provider, strict cache, fallback timestamp, and migration-stub exclusion. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/goose.rs` | 6,555 | `6f89caa6cf2832030ecb2d28fd90be8d4180a5ffb73a1bce280dd3c36022b99a` | Goose cumulative session counters, accumulated-zero precedence, model-config parsing, timestamp formats, and the inferred-reasoning remainder the native reader refuses. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/copilot.rs` | 144,473 | `d50e691d6a4612d7081a8253a20d35fd5c6abf4c6b70dbabff535a1ce3d8d07d` | Copilot flat OTel record detection, token attributes, cache-inclusive input normalization, model/session attributes, source priority and trace enrichment. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/tencent_buddy.rs` | 48,854 | `9f1050bb8ab64b75038805cd6d03a53d85df3931aac8ebaa1f1a712699fdd0f9` | Shared CodeBuddy/WorkBuddy JSONL record shape, usage aliases, inclusive-prompt split, identity, extension-log and turn-metrics rules. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/codebuddy.rs` | 4,648 | `0663d3e033e6ad2540872e5959de34f2385163462dda23354fba60bdebaebea8` | CodeBuddy client id, default model and extension-log delegation. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/workbuddy.rs` | 7,775 | `ce2736650b498171e00bcb34c25f469263197d7ccc037a48f92fbe9c3ad06300` | WorkBuddy client id, project roots and aggregate SQLite fallback. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/pi.rs` | 104,874 | `7babe15ca00883e2d573f3632c0e9d301397121a0742332d7cf62034399b292f` | Pi session header, assistant/tool/operation usage, reasoning-subset rule, model-change attribution and cross-session fork identity. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/omp.rs` | 8,424 | `a303a6b15733be9821a952565dc466b27f429cb9dca33fbcf9bf8e54d37d1aff` | OMP client id, fixed sessions root and standalone subagent transcripts over the shared Pi parser. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/senpi.rs` | 16,385 | `f61bf963d1ca92b983d08dd56625a14ef172529916e7ef0242adfadbceb16d32` | Senpi client id, OmO/Senpi roots, task children and the reasoning-subset invariant. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/antigravity.rs` | 7,610 | `98a0a18306e18d1ccb316a56cc5ba891dbd705b14632b5f89b795a3fb37bcb10` | Antigravity cache `session_meta`/`usage` rows, direct counters, model fallback and response identity. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/qwen.rs` | 31,911 | `ec4d5ca5c17f2e6774e0e2e0e2df5f233aaf6dc37e9e1d00f78f6522add744fa` | Qwen assistant `usageMetadata`, prompt/cache split, inclusive candidate/thoughts rule and session fallback. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/reasonix.rs` | 13,221 | `79bb772fa69a1b6d08d2aa918d3ca69cc6d0d2207b4835f28500848d02b8b82e` | Reasonix stats record shape, cache-miss precedence, reasoning split and daily-file partition. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/muse.rs` | 20,163 | `58e02f3e0b765cca1e20ff37f148df18882d19b05b2c20d2f5fb56b0f6c71d90` | Muse `model_completed` events, cached/reasoning subsets, stream identity and sequence dedup. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/mcode.rs` | 13,582 | `3b5d754461da6e2df160b5c148b104f930abc08e8cf763647379288e9aeec00b` | MiniMax Code legacy message/result pairing, envelope usage, model/provider requirement and turn dedup. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/zed.rs` | 18,679 | `81c648d66f31036dbbf64007aaf63fa0f0093924b0d337e8c7d3096bc69a971f` | Zed hosted-thread selection, request/cumulative usage, JSON/zstd payloads, timestamp fallback and thread identity. Pinned-reader evidence re-read at the exact revision. |
| `crates/tokscale-core/src/sessions/hermes.rs` | 35,580 | `dd6137ffcb5a3e61008fb24ae2ab17152f1380a1860c72fe6e128040fd70ed62` | Hermes `session_model_usage` per-model grouping, `sessions` session-total fallback, inclusive-output reasoning subset, NULL-versus-empty provider grouping, and the rotated `logs/agent.log*` cross-day split. Pinned-reader evidence re-read at the exact revision; not part of the original 31-entry retained manifest. The native reader adopts the query shapes and disjointness rule and deliberately omits the log split. |
| `crates/tokscale-core/src/sessions/antigravity_cli.rs` | 138,334 | `bdfedfb74eef5a060f9f33e19a84304730b54f65d8644d1f3140eba5a39e63b7` | Antigravity generation-database protobuf field map, usage counters, model recovery and response identity. Pinned-reader evidence re-read at the exact revision; step/trajectory timestamps are not implemented. |
| `crates/tokscale-core/src/sessions/openclaw.rs` | 101,044 | `326781cdc13250aac90bf7ab8b037e8736e87f6d8675b78e82665ea3811b8c9e` | OpenClaw's dual transcript store: the `transcript_events` schema and zstd payload encoding, the inclusive-output reasoning subset, the transcript-bookkeeping rows, the event-identity dedup construction, the Codex-mirror `idempotencyKey` shape, and the agent-root/`sessions` role layout. Pinned-reader evidence re-read at the exact revision; not part of the original 31-entry retained manifest. |
| `crates/tokscale-core/src/sessions/droid.rs` | 83,663 | `ec7e225c3d0f0ba133f98636c954c647bc8b27e9029cc1515a202fdcebcbdca8` | Droid `*.settings.json` cumulative `tokenUsage` field names, the `custom:`/`[...]` model normalization steps, the `providerLockTimestamp`-as-floor timestamp rule, and the per-reply apportioning the native reader declines. Pinned-reader evidence re-read at the exact revision; not part of the original 31-entry retained manifest. |
| `crates/tokscale-core/src/sessions/grok.rs` | 113,692 | `d2ecfc7548eb05785b67c852c0d9bb18ef84cc73ccee866a14070e39bfd75731` | Grok Build's two usage lanes: the `turn_completed` breakdown with its inclusive-cache/inclusive-reasoning subtraction, and the legacy cumulative `totalTokens` monotonic-delta rule with its `user_message_chunk` turn boundaries. Also the prompt-id snapshot-replacement rule, the single-key `modelUsage` fallback, the timestamp path list, and the `signals.json`/`unified.jsonl` lanes the native reader declines. Pinned-reader evidence re-read at the exact revision; not part of the original 31-entry retained manifest. |

The native implementation does not embed `tokscale-core` and does not invoke a parser subprocess. Pinned code sometimes skips malformed or oversized rows and uses signed/saturating behavior; Gate 2A deliberately selects strict checked non-negative counters and whole-candidate failure. Those native differences are part of the written contract, not accidental drift.

`opencodex.rs` is not the OpenCode usage reader: it is an OpenCodex ledger used to attribute Codex responses and its provenance row therefore remains supplemental only. The OpenCode usage reader at the same pin is the pair `opencode.rs` + `opencode_schema.rs`. Their SHA-256 values above were recomputed from the exact commit while implementing the native OpenCode contract; because they were not in the original 31-entry retained manifest, they are pinned-reader evidence rather than manifest-verified entries. The retained VM evidence directory was unavailable during this implementation, so all three files were fetched read-only from the exact commit; the `opencodex.rs` fetch reproduced its retained manifest hash (`8a84dc…`), confirming the revision. No upstream code was executed.

## Compressed input dependency and fixture

The bounded streaming decoder is `ruzstd` 0.9.1 (MIT), pinned with only `std` and `hash`, and its sole transitive dependency is `twox-hash` 2.1.5 (MIT). Neither crate has a build script or native library linkage. The native adapter performs its own checksum comparison because `ruzstd` exposes frame checksums but does not reject mismatches by itself. No zstd CLI, C library, subprocess, or temporary decoded file is used in production.

`tests/fixtures/pinned/codex-parser18-supported.jsonl.zst` is the first four supported records of `codex-parser18-prefix.jsonl`, compressed with Zstandard CLI v1.5.5 using `zstd -19 -q --check`. It is 310 bytes, SHA-256 `f60c5b8afd4feab929e721185db1ee04680c26f31e8b770710e88864b3851fea`. The fixture is used only by tests; the production reader streams directly into the shared framer.

`tests/fixtures/pinned/codex-parser18-full.jsonl.zst` is the complete eleven-record `codex-parser18-prefix.jsonl`, compressed with Zstandard CLI v1.5.5 using `zstd -19 -q -f`. It is 506 bytes, SHA-256 `12c9dd24d5f293d51ba984cb71de306e0214b2cdcc0ae4aedc08ee5cc14ebb84`. It proves ordinary JSONL/zstd transport equivalence for the supported usage-record and compaction rows.

## Token Monitor `0.88.0` sources used by the matrix

All manifest-covered paths below are relative to commit `2e55113488888d2e784f159cd92f2d4eee157fbd`. They are MIT-covered by `LICENSE` (1,062 bytes, SHA-256 `250136de456c31d659e54c1804c43a27dadad175336b67226e9509d6310f923c`).

| Relative path | Bytes | SHA-256 | Contract evidence |
|---|---:|---|---|
| `src/shared/providers/codex/localUsage.js` | 8,366 | `21e1f0a1e18db465d6fd6d5f398b8de358eff856a3e27eb8875caa0f99f86c09` | Public local-usage view, canonical `codex:<thread_id>`, native-overlap exclusion, period filtering, and non-secret projection. |
| `src/shared/providers/codex/localUsageSource.js` | 17,462 | `ec00e585831e5a1eef07823b94f894f5e24e8b7f4d9ba2c8b3d6e09b1bdbce0d` | Request observation production, model/timestamp/component semantics, and native-backed marker. |
| `src/shared/providers/codex/localUsageStore.js` | 8,727 | `1db2109c92c67baf9f7653147f98e7828ebae85900f7582526e6913253175773` | Exact unversioned tables, keys, required columns/index, stored usage validation, and request/thread join. |
| `src/shared/sqliteReadOnly.js` | 648 | `678b7f44d54e3b25851f50ca8dd743f15714fd87f828326fa90c598757abac89` | Read-only open, 250 ms busy timeout, `query_only=ON`, and close-on-initialization-failure behavior. |
| `src/shared/usage/sessionUsageArchive.js` | 26,987 | `ba22659da81f3234cc6239494973585e3672215c5b37ac0f123f490de79b7610` | Archive normalization, aliases, latest-whole-period selection, explicit zero, live-vs-retained authority, day/month windows, and unclassified coverage. |
| `src/shared/usage/sessionUsageArchiveStore.js` | 17,286 | `d81f7523dd3992832dab093f50ce2c8875e7a813acd517291ed97973d9530d3a` | Schema-v1 metadata/sessions/index contract and revision-fenced loading. |
| `tests/shared/codexLocalUsage.test.js` | 79,858 | `dabdf163ca4b68a2df94dda6b9d9e592b0a00d437272cb39d7d6c6d7ceeb5458` | Local-usage identity, counter, overlap, time-window, and view behavior. |
| `tests/shared/sessionUsageArchive.test.js` | 41,036 | `8eb45e66594c096f8f3f88230f774f7bd4b9a578decd53cd96e1a42155494570` | Aggregate normalization, periods, aliases, explicit zero, reasoning, and component/model coverage. |
| `tests/shared/sessionUsageArchiveLive.test.js` | 20,935 | `a6f2d5f591191894bba2bf35246dce7317c5c8b46202f7e0ab1de73d6494cd05` | Live-versus-retained authority and overlap behavior. |
| `tests/shared/sessionUsageArchiveStore.test.js` | 52,858 | `cdd4327df58f1eb5a432fb7d4750199e4703b338bc5717b26998c9c0bb88315a` | Schema metadata, migration marker, revisions, WAL/read consistency, and malformed-row behavior. |
| `tests/shared/sqliteReadOnly.test.js` | 1,478 | `f54944723a18e6ee99d4d4caeb3bdc80308914b9b1383c27818a279395adb4b8` | Read-only helper behavior. |

`src/shared/usage.js` was acquired separately from the same exact commit because `sessionUsageArchive.js` imports its normalization helpers. It was not in the original 14-entry Token Monitor manifest. Its independently verified SHA-256 is `f7b9907bc88e3cd7a78dcb6524a147d5995d454a899eb10d3bb1abd4bee5d1b6`; it is **supplemental exact-pin evidence**, not a manifest-covered file or repository fixture. It establishes inclusive-output reasoning and that incomplete component/model coverage remains unclassified rather than inferred.

The following OpenCode files are also **supplemental exact-pin evidence** from the same repository commit, outside the original 14-entry manifest:

| Relative path | Bytes | SHA-256 | Contract evidence |
|---|---:|---|---|
| `src/shared/providers/opencode/session.js` | 22,530 | `135bc8386e37d43c180a29dd68c607300850ba9c944b88057d5ef9362549ae84` | Current `session_message` / legacy `message`/`part` shape detection, current-before-legacy precedence, v1 compaction-summary and subtask restoration, and `input + output + cacheRead + cacheWrite` totals with reasoning kept informational. |
| `src/shared/providers/opencode/goLimits.js` | 9,053 | `e8eeb3c05d46ddcc2332e8213d502373ec13448aac3b3133a193bd5a959660c3` | `OPENCODE_DB` override, `XDG_DATA_HOME`/`<home>/.local/share` data root, and `opencode.db` / `opencode-<channel>.db` filename rule. |

The native reader deliberately does not implement the reference's `OPENCODE_DB` override or its summary/subtask restoration; those are recorded as visible unsupported coverage in [reader contracts](reader-contracts.md).

Source-root evidence for the JSON readers is also supplemental exact-pin evidence: `src/shared/clientSources.js` (33,255 bytes, SHA-256 `2bc0f73720bbee23aa422dfb15bef6dd858ba8a042ff665043b1865a907f0849`) and `src/shared/clientSourceRegistration.js` (6,832 bytes, SHA-256 `96587007a3677270c051277093c9099a429338984fb7c19d2ab8d6943dcd75ba`) establish the Gemini `tmp` root, the Cline VS Code globalStorage roots, the Amp thread/cache roots, and the Copilot `otel`/`session-state` roots.

The Hermes home/profile resolver is supplemental exact-pin evidence from the same application commit: `src/shared/providers/hermes/profiles.js` (1,969 bytes, SHA-256 `5e311f1b5d7eb37a6da6cc1c6660690b072f1e7f9f750dc918eb43a9c414ac5a`) establishes the `HERMES_HOME` > `~/.hermes/state.db` > Windows `%LOCALAPPDATA%\hermes` precedence and the `profiles/<name>/state.db` watch set that the native resolver reproduces.

`scripts/vendor/tokscale.json` (9,427 bytes, SHA-256 `07f9c905b51ebc4bdac55565bab9d624338b6b7c55f843fd37b18afbef86aa07`) records the application-side parser pin. It is provenance evidence, not code loaded by Token Monitor Lite.

## Differential fixtures

The retained upstream fixture manifest is:

`/home/azureuser/.local/state/token-monitor-lite/evidence/tokscale-d3df620b2cdd9502c70aececdbc6ece61fe26fc9/differential-fixtures-manifest.json`

| Exact upstream relative path | Bytes | SHA-256 | Repository status |
|---|---:|---|---|
| `crates/tokscale-core/tests/fixtures/billing-cache/claude-parser2/complete.jsonl` | 1,268 | `24aa573ea585ecc6c5e06a21caf97a5942d86575cc83c8892df5b1b5a66ba7d5` | Copied byte-for-byte as `tests/fixtures/pinned/claude-parser2-complete.jsonl`. |
| `crates/tokscale-core/tests/fixtures/billing-cache/claude-parser2/expected-complete-messages.json` | 2,231 | `7a3a63a75c5e2bcc2013ab2e9b990455a05e4c93b8afffb5fd36f5862d095333` | Copied byte-for-byte as `tests/fixtures/pinned/claude-parser2-expected.json`. |
| `crates/tokscale-core/tests/fixtures/billing-cache/codex-parser18/prefix.jsonl` | 2,004 | `ce3db19fd867f3a2bc7be1a0be652a2f6b87fb5a0688b20f50602b1b21b5117e` | Copied byte-for-byte as `tests/fixtures/pinned/codex-parser18-prefix.jsonl`. |
| `crates/tokscale-core/tests/fixtures/billing-cache/codex-parser18/append-settled.jsonl` | 362 | `9b76b86808e7d78f6b6f69b6515a038b45d2dac295a1412e93d16f8a797fb2d8` | Retained evidence only; verified present, not a repository fixture. |
| `crates/tokscale-core/tests/fixtures/codex_duration_timing.jsonl` | 2,116 | `212a6d74e16ee8b4a5f92414d0894fb9334eb40041ed9cc0b78c5962391cbf77` | Retained evidence only; verified present, not a repository fixture. |

The Task 1 verification helper rehashed every retained fixture and expected-output file and reported `failures: []`. Pinned names may be used only for these exact bytes. New native edge cases must be clearly named synthetic fixtures and must not be presented as upstream differential evidence.

The Claude pair covers the currently overlapping response fields. Codex `prefix.jsonl` is deliberately not a complete positive fixture: the current native reader supports the first four records and visibly rejects record five (`token_usage_record`) without mutating prior state; later compaction is also unimplemented at freeze. Fixture presence does not broaden implementation coverage.

There is no OpenCode fixture file at the pin. The pinned `opencode_schema.rs` tests construct synthetic SQLite databases in Rust rather than reading retained fixture bytes, so no byte-for-byte OpenCode differential fixture exists to copy. `tests/opencode.rs` marks its upstream-adapted payload case as synthetic; it is not presented as differential evidence.

## Application catalog provenance

`assets/catalog.json` is adapted from read-only Token Monitor files `src/shared/clientCatalog.js` (SHA-256 `34bb705e0daa4e5f4ea04ec25fc6f69ad6c1c35d951429646fe9df4b30e33986`) and `src/shared/limits/providers.js` (SHA-256 `1304087cc8d9fff1d7b941499a6830b232f41533850bf4e70a3a8ff58dd9d1d9`) at the same application commit. Catalog identities, labels, defaults, fork flags, and provider settings labels are data provenance only; they do not prove parser support or live provider acceptance.

## License notices

### Tokscale MIT notice

Copyright (c) 2025 Junho Yeo

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

### Token Monitor MIT notice

Copyright (c) 2026 Javis

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
