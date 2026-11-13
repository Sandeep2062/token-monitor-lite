# Native migration inventory

This is a pinned-reference migration checklist, not a list of features already supported by Token Monitor Lite. It covers observable behavior rather than copying Electron's process architecture or DOM implementation.

## Reference and status

- Upstream: `Javis603/token-monitor`, version `0.88.0`, commit `2e55113488888d2e784f159cd92f2d4eee157fbd`.
- Read-only checkout: `../../token-monitor/`. Do not run migrations, formatters, generators or cleanup against it.
- Parser reference: `Javis603/tokscale`, commit `d3df620b2cdd9502c70aececdbc6ece61fe26fc9`, baseline `4.21.0`, as pinned by `token-monitor/scripts/vendor/tokscale.json`.
- Approved direction: selectively adapt pinned Rust parser code and port application behavior to Rust + Slint (Approach A). Do not silently substitute the complete core library or scanner sidecar.
- Current application state: **tested headless core, mixed Claude/Codex CLI readers, canonical mixed-format aggregation, bounded native monitoring, deterministic discovery, one-writer SQLite persistence/recovery, staged legacy migration, revisioned settings, native credential transfer, and a Rust + Slint desktop slice with Linux display-backed acceptance**. Gate 2A production ingestion covers ordinary/compressed Codex, retained archive SQLite, and Codex local-usage SQLite; exactly 100 sources and 1,000 canonical sessions pass commit, reopen, restart, mutation, recovery, and resource acceptance. Home, Sessions, Source Health, Settings (including select-all), migration report, About, Today/Month/All Time periods, 30-day Trends, and a period-scoped Models breakdown are Linux GUI-accepted. Complete unchecked application features remain **N**. The **X** JS-plugin compatibility boundary preserves data without execution; native equivalent functionality remains unfinished.

| Status | Meaning |
| --- | --- |
| N | Not implemented in the native application. |
| U | Implementation exists but required verification remains outstanding; identify fixture, runtime and platform coverage separately. |
| UI | Visual control exists without a working end-to-end action. This is not parity. |
| W | Working for the explicitly named platform/scenario, with reproducible runtime evidence. |
| X | Incompatible or intentionally excluded; requires an explicit decision and visible migration explanation. |

Unchecked rows are not completed by passing a catalog test. At implementation time, record the native path, fixture/test, command/result, platform and remaining live/manual checks next to each changed row. Upstream tests or statements are **reference evidence**, never Rust verification.

### Verified foundation scope

`src/accounting.rs`, `src/catalog.rs`, `src/config.rs`; tests in `tests/core.rs` (nine) and `tests/config.rs` (six). Remote isolated snapshot `/home/azureuser/.local/state/token-monitor-lite/core-_ep4m0wh` passed whole-catalog differential, `cargo fmt --all -- --check`, headless locked all-target check, tests, strict Clippy and release-library build. This proves exact primitive arithmetic/catalog lookup and bounded settings inspection, not period aggregation, native GUI, archives, credentials migration, file monitoring or reader parity. Settings inspection preserves original bytes and unknown/null/empty selections with redacted diagnostics; no adoption or save path exists. Windows/macOS remain unverified.

## Usage clients

### Fixture-verified CLI subsets (U, not complete client support)

`src/parsers/{jsonl,claude,codex}.rs` and `tests/{jsonl,claude,codex}.rs`: seven framing, eight Claude and nine Codex synthetic tests, plus two exact pinned differential tests. Snapshot `/home/azureuser/.local/state/token-monitor-lite/core-ltrzx0ow` passed all headless gates and 71 total tests. The pinned Claude complete fixture matches every supported expected row. The pinned Codex prefix verifies records 1–4 only and then asserts explicit unchanged-state rejection of its first `token_usage_record`; compaction remains unsupported. Captures use exact bounded counters, explicit unavailable cost and value-redacted unsupported/invalid errors. See [reader contracts](reader-contracts.md) and [provenance](parser-provenance.md). Complete client-root parity, Desktop/ledger semantics, complete Codex fixture handling, actual UI and packaged platform behavior are not verified. Full `claude`/`codex` rows below stay unchecked.

`src/source_inputs.rs`, `tests/source_inputs.rs` and `tests/sqlite_sources.rs` add bounded streaming Codex `.jsonl.zst` plus read-only SQLite archive/local-usage adapters. The compressed path requires exact magic/suffix roles, caps window/decoded/physical bytes, verifies checksums and hashes, and streams through the shared framer. SQLite adapters use fenced private snapshots so WAL read-mark changes never touch source main/WAL/SHM, normalize Codex local-usage rows, and return schema-v1 archive periods in a separate retained type. SQLite discovery/runtime wiring and retained persistence are implemented in schema v4; the mixed release fixture exercises both WAL-only databases as configured sources.

`src/sources.rs`, `tests/sources.rs` and the JSONL metadata fence add eight synthetic one-source integrations. Snapshot `/home/azureuser/.local/state/token-monitor-lite/core-f0aiu6e_` passed fmt, locked all-target check, all 46 tests, strict Clippy and release-library build plus the catalog differential. Source steps bound each framing batch, withhold intermediate/failed candidates, retain immutable last-good data, flag deferred incomplete tails and expose signed source-component corrections. Metadata fences reject observed mixed versions; they do not prove atomic filesystem snapshots. Whole-application serial reconciliation, canonical day/model/session and cross-file dedup, discovery/monitor, source-removal archive retention and GUI remain incomplete. No new dependency was added for this boundary.

`src/aggregation.rs` and `tests/aggregation.rs` add 16 synthetic canonical-ledger contracts. Snapshot `/home/azureuser/.local/state/token-monitor-lite/core-vm7x9ej7` first passed the catalog differential, rustfmt, locked headless all-target check, all 63 tests, strict Clippy and release-library build; the current 71-test gate is recorded above. The ledger uses validated named-IANA/DST day bucketing, checked exact totals and signed replacement/removal deltas, source-order-neutral overlap provenance, stale-generation fencing, client/session/model partitioning and pinned cache-aware Claude completeness merging. Unkeyed rows are never guessed; Codex and unsupported stable-key contradictions remain strict. This verifies canonical aggregation only for the implemented CLI subset, not provider-complete grouping, Month/All-Time persistence, retained/live precedence, source-root discovery, GUI or packaged platforms.

`src/runtime.rs` and `tests/runtime.rs` add native monitor lifecycle integrations. The runtime owns deduplicated non-recursive parent-directory watches, exact configured-path event filtering, bounded/coalesced admission, serial reconciliation, immutable generation-tagged snapshots, fixed redacted source health, settings-generation fencing and joined shutdown. Every canonical candidate is committed before activation/publication; blocked writers publish a persistence-delayed state without newer rows; stale settings-generation commits are never published; unchanged parsed contents reuse the committed generation so startup and single-source updates avoid full re-commits. Tests cover append publication, mid-worker dirty retry, coalescing, read/decoding/unsupported/persistence statuses, settings rebaseline/stale rejection, watcher-unavailable manual refresh, recovery without duplicate observations, and shutdown. Windows/macOS watchers remain open.

`src/ui.rs`, `tests/ui.rs`, `src/main.rs` and `ui/app.slint` add the bounded Ledger Rail presentation slice. Tests establish exact `u128` formatting, 50-row paging, five recent rows, search reset/clamping, stable `(client, session, model)` selection, stale-snapshot rejection, source-path redaction, fixed source recovery states, opaque 42-character source IDs, mixed-archive selection, select-all, and latest-only mailbox coalescing. Linux GUI acceptance exercises navigation, settings validation/cancel, 100-source select-all/save, session search/paging/selection, refresh, source error/recovery, About, keyboard focus, reopen persistence, and graceful close under Xvfb/Matchbox/AT-SPI. Windows/macOS rendering and packaging remain open.

`src/discovery.rs` and `tests/discovery.rs` add nine deterministic discovery tests. Remote snapshots `core-0odzjucp` (headless/no defaults) and `core-f5sq6ka8` (desktop/default features) passed the catalog differential, rustfmt, locked all-target checks, all 87 tests, strict Clippy and release builds. Tests cover Windows/macOS/Linux root roles, separate overrides, invalid/overlapping roots, redacted backend types, exact recognized legacy artifacts, inert plugin directories, non-regular rejection, artifact/file/depth/aggregate bounds, selected Claude/Codex reader validation and source capacity. The Windows release also runs its native Windows path-role/override assertions; this establishes deterministic logic and fixture behavior, not macOS runtime access, semantic migration, persistence, source-setting activation or startup integration.

### Gate 1 persistence, migration, settings, and credentials (U)

`src/{storage,migration,application,credentials}.rs`, the Settings projection, and their integration suites implement the Gate 1 boundary. The latest retained snapshots `core-tzesvz6a` (desktop/all features) and `core-uduw9mfg` (no defaults) each passed the catalog differential, rustfmt, locked all-target checks, all 127 ordinary tests, strict Clippy, and optimized release builds. `core-337pt8u1` passed the isolated real Linux Secret Service lifecycle and empty-store check; `core-h9n3r4us` separately returned the expected bounded `unavailable` result with no session bus. Lock SHA-256 is `a378a1beaa77f3d0e50314efb23e2723f6b1d7446f673af7d70817b0966a31ee`.

Generated-fixture coverage includes clean first run/restart, supported and unsupported-only imports, mixed atomic rollback/retry, interrupted credential staging/recovery, every credential-journal transition, stale settings CAS, corruption with and without a verified backup, original-byte retention, schema/table inspection, and secret/private-path sentinel exclusion. Supplemental direct inspection is retained at `task7-inspection-output-1S4gbh/report.json`; it records schema v2 with `quick_check=ok`, source/staging hash equality, byte-exact settings/session reopen, expected committed/pending row counts, and sentinel absence from ordinary native files. Unsupported stores and JS plugins are copied privately and remain inactive; supported schema-v1 archive rows and credential-free settings are transactionally adopted. Linux live Secret Service is accepted through an isolated empty-store lifecycle using `gnome-keyring 46.1-2ubuntu0.2`; no fake/plaintext fallback exists. Windows paths, the real Credential Manager lifecycle, a GNU cross-build GUI launch, and the full Windows suite are accepted on a real Windows 10 host; NTFS ACLs, MSVC packaging and full Windows GUI automation remain open. macOS paths, Keychain, ACLs, GUI and packaging remain unverified; macOS bundles are ad-hoc signed and unnotarized by decision. Linux display-backed GUI/resource behavior is accepted.

The Gate 2A mixed fixture supersedes the Task 1 diagnostic. `scripts/generate-acceptance-fixture.py` schema v2 (`mixed-2026-11-13.1`) generates exactly 100 discovered sources and 1,000 canonical sessions: 33 Claude JSONL, 33 Codex JSONL, 32 streaming zstd Codex, one schema-v1 archive SQLite/WAL with 120 retained periods, and one Codex local-usage SQLite/WAL with 903 threads (902 canonical after native-duplicate suppression). It yields 2,786 parsed observations and 2,784 canonical observations. `tests/acceptance_fixture.rs` discovers and ingests the tree through production adapters/canonicalization/storage, reopens the database with sources offline, restores them without duplicate rows, exercises append/rewrite+growth/tail/rotation/zstd truncation+corruption/schema failure/database replacement, creates and verifies a backup, commits post-backup data, corrupts the active database, restores the exact backup generation, and scans native files for private sentinels. Retained report `task10-fixture-report.json` passed with only `representative-limits-accounts` remaining; `core-bir_sse4`/`core-j4ubtphg` (headless) and `core-qmf4slfr`/`core-fg6breu7` (desktop) passed all matrices. SQLite lock/busy, crash-between-candidate-and-commit, and backup-corruption recovery are additionally covered by focused storage/adapter suites.

Identity, labels, order and defaults come from [clientCatalog.js](../../token-monitor/src/shared/clientCatalog.js). Source resolution comes from [clientSources.js](../../token-monitor/src/shared/clientSources.js) and [clientSourceRegistration.js](../../token-monitor/src/shared/clientSourceRegistration.js). Accounting comes from the pinned parser plus [usage.js](../../token-monitor/src/shared/usage.js), [history.js](../../token-monitor/src/shared/history.js) and the matching provider note.

In the table, **P** means inspect and adapt that client's reader/scanner at the exact parser pin before claiming its format is supported. A catalog-only client is still a real migration task. Root detection is not parsing, detail support or live-process verification.

| Done | ID / label | Fresh install | Source/contract boundary to port |
| --- | --- | --- | --- |
| [ ] | `claude` / Claude Code | On | P; CLI/custom roots, retained Desktop transcripts and conversation storage; replay dedup, title/context/cache/effort precedence. [Note](../../token-monitor/docs/providers/claude.md). |
| [ ] | `codex` / Codex | On | P; current/archived/compressed rollouts and headless captures; cumulative corrections, continuation/parent identity. Dots ledger is a separate opt-in source. [Note](../../token-monitor/docs/providers/codex.md). |
| [ ] | `opencode` / OpenCode | On | P; current SQLite and legacy stores; compaction/subtask dedup, source ownership and session metadata. [Note](../../token-monitor/docs/providers/opencode.md). |
| [ ] | `hermes` / Hermes Agent | On | P; effective home/profile discovery and metadata; measured reasoning and actual-cost rules. Inspect `src/shared/providers/hermes/`. |
| [ ] | `openclaw` / OpenClaw | On | P; current and legacy agent-root aliases must retain one canonical `openclaw` partition. |
| [ ] | `cursor` / Cursor | On | P; native source plus managed-account self-sync/cache repair. Do not watch generated caches. [Note](../../token-monitor/docs/providers/cursor.md). |
| [ ] | `antigravity` / Antigravity | On | P; Desktop and CLI aliases, native discovery and self-sync; never target `synthetic` or create a cache watch loop. [Note](../../token-monitor/docs/providers/antigravity.md). |
| [ ] | `gemini` / Gemini CLI | On | P; effective Gemini home; disjoint reasoning/cache fixture and normalized period/session totals. |
| [ ] | `cline` / Cline | On | P; IDE and CLI roots; exclusive `CLINE_SESSION_DATA_DIR` > `CLINE_DATA_DIR` > `CLINE_DIR` precedence. [Note](../../token-monitor/docs/providers/cline.md). |
| [ ] | `amp` / Amp | On | P; thread/native/cache roots from the resolver; verify parser and token semantics independently of detection. |
| [ ] | `droid` / Factory Droid | On | P; shared CLI/Desktop session kernel; limits identity is `factory`, not `droid`. [Note](../../token-monitor/docs/providers/droid.md). |
| [ ] | `kimi` / Kimi | On | P; multiple frontends and session-document generations; maintain root and metadata precedence. [Note](../../token-monitor/docs/providers/kimi.md). |
| [ ] | `qwen` / Qwen | On | P; registered project roots; inspect reader and nonzero accounting fixture before implementation. |
| [ ] | `grok` / Grok Build | On | P; metadata fallbacks, disjoint reasoning and matched request throughput. [Note](../../token-monitor/docs/providers/grok.md). |
| [ ] | `copilot` / GitHub Copilot | On | P; CLI/IDE sessions and OTel, including exact configured exporter file; union sources without duplicate accounting. [OTel](../../token-monitor/docs/github-copilot-otel.md). |
| [ ] | `pi` / Pi | On | P; filename-suffix session identity; reasoning is already inside output. Detail operation accounting currently exceeds bundled scanner coverage. [Note](../../token-monitor/docs/providers/pi.md). |
| [ ] | `omp` / Oh My Pi | On | P; independent client after identity split; Pi-format detail is per transcript, not parent/child sum. Raw title cards stay intact outside presentation. [Note](../../token-monitor/docs/providers/omp.md). |
| [ ] | `senpi` / OmO Native | On | P; all registered OmO/Senpi roots and task children; reasoning already belongs to output. Saved lists must not gain this ID automatically. [Note](../../token-monitor/docs/providers/senpi.md). |
| [ ] | `goose` / Goose | On | P; first-existing SQLite database and exclusive absolute override; accumulated zero wins. Cumulative creation-day attribution, inferred non-additive reasoning and estimated cost remain disclosed. [Note](../../token-monitor/docs/providers/goose.md). |
| [ ] | `zed` / Zed | On | P; local thread store distinct from web quota plane. No quota-auth success substitutes for token-parser evidence. [Note](../../token-monitor/docs/providers/zed.md). |
| [ ] | `kilo` / Kilo | On | P; CLI database and IDE task roots from the resolver; inspect duplicate/source rules. |
| [ ] | `commandcode` / Command Code | On | P; project transcripts; usage estimates must remain labeled estimates, not measured tokens. |
| [ ] | `mimo` / Xiaomi MiMo | On | P; `mimocode.db`. Claude imports intentionally overlap Claude usage in the reference; disclose, do not silently deduplicate. Console/membership limits are separate. [Note](../../token-monitor/docs/providers/mimo.md). |
| [ ] | `muse` / Muse Code | On | P; XDG session data; input/cache and output/reasoning split without duplication. Native CLI/web allowance is separate from tokens. [Note](../../token-monitor/docs/providers/muse.md). |
| [ ] | `zcode` / ZCode | On | P; projects and local CLI DB; maps to `zai` limits. [Note](../../token-monitor/docs/providers/zai.md). |
| [ ] | `kiro` / Kiro | On | P; CLI sessions/database and IDE roots, retaining platform variants; inspect reader before merging them. |
| [ ] | `codebuddy` / CodeBuddy | On | P; native transcripts/title/turn rules; no fabricated subagent links or context denominator. [Note](../../token-monitor/docs/providers/codebuddy.md). |
| [ ] | `workbuddy` / WorkBuddy | On | P; both transcript roots and older ungrouped records; bounded streamed detail; credentials are an independent plane. [Note](../../token-monitor/docs/providers/workbuddy.md). |
| [ ] | `proma` / Proma | On | P; native journals/SDK/Pi dedup. Preserve fork-era archived rows but explain their intentional replay exclusion; this is not lossless accounting. [Note](../../token-monitor/docs/providers/proma.md). |
| [ ] | `catpaw` / CatPaw | **Off** | Fork reader; both editions, immediate account SQLite files only, whole-source replacement and model namespaces. No verified Linux/WSL source. [Note](../../token-monitor/docs/providers/catpaw.md). |
| [ ] | `qodercn` / Qoder CN | **Off** | Fork reader; legacy SQLite/current JSONL; input already includes cache read. Credits-only first-party rows cannot establish tokens. No invented cross-format dedup key; no WSL scan. [Note](../../token-monitor/docs/providers/qodercn.md). |
| [ ] | `reasonix` / Reasonix | On | P; aggregate authority versus bounded native-session/sidecar projection; local zero-token rows do not become archived usage. [Note](../../token-monitor/docs/providers/reasonix.md). |
| [ ] | `dsh` / DeepSeek Harness | On | P; compressed source decoding, model/replay accounting and shared detail; limits maps to `deepseek`. [Note](../../token-monitor/docs/providers/dsh.md). |
| [ ] | `cherrystudio` / Cherry Studio | On | P; chat SQLite ledger and separate Agent transcripts; exclude overlapping ledger rows and keep disjoint reasoning. Total-only rows cannot supply invented token buckets. [Note](../../token-monitor/docs/providers/cherrystudio.md). |
| [ ] | `lmstudio` / LM Studio | On | P; registered server-log roots; parser semantics and supported versions need focused inspection. |
| [ ] | `unsloth` / Unsloth | On | P; Studio SQLite, bounded DB/WAL watches and pricing boundaries. [Note](../../token-monitor/docs/providers/unsloth.md). |
| [ ] | `devin` / Devin | On | P; CLI SQLite wins overlapping Desktop ACP events; `acp-messages` is not a supported source. [Note](../../token-monitor/docs/providers/devin.md). |
| [ ] | `fx` / fx | On | P; session store/snapshots and model-specific token expectations from the executable fixture. |
| [ ] | `mcode` / MiniMax Code | On | P plus fork supplement; native runtime/headless turn dedup and symlinked/profile roots; desktop-store inference is not live evidence. Maps to `minimax` limits. [Note](../../token-monitor/docs/providers/mcode.md). |
| [ ] | `mistral` / Mistral Vibe | On | P; Vibe cumulative metadata, not per-message token attribution; split cached subset from prompt count. [Note](../../token-monitor/docs/providers/mistral.md). |
| [ ] | `prime-agent` / Prime Agent | On | P; root/RLM children and explicit native relationships; inspect child discovery without duplicate counting. [Note](../../token-monitor/docs/providers/prime-agent.md). |
| [ ] | `jcode` / Jcode | On | P; snapshot/journal precedence and independent reasoning. [Note](../../token-monitor/docs/providers/jcode.md). |

Additional parser-supported IDs are not rejected solely because the display catalog does not manage them. Report unsupported reader IDs explicitly rather than converting them to an empty successful scan. Preserve saved client selections and identity-split migration markers.

## Limits providers

All rows start at **N**. IDs/order come from [providers.js](../../token-monitor/src/shared/limits/providers.js), declarations from [accounts.js](../../token-monitor/src/shared/limits/accounts.js), and intended lanes from [providerPresentation.js](../../token-monitor/src/electron/renderer/limits/providerPresentation.js). Each provider's authoritative implementation is `token-monitor/src/shared/providers/<id>/account.js` and `limits.js`, with app-layer code under `src/electron/providers/<id>/` where present. Keep configured-tool `settingsLabel` overrides separate from live-quota labels: Claude Code for `claude`, Z.ai / GLM for `zai`.

For every row port credential precedence, request/redirect/origin policy, account identity, partial success, normalization, units, reset semantics, cancellation and redacted forms. Capability labels are not proof of live endpoint acceptance.

| Done | ID / label | Independent lanes and important acceptance boundary |
| --- | --- | --- |
| [ ] | `claude` / Claude | Auto OAuth/CLI and manually configured Web; preserve account and prepaid-credit policies. |
| [ ] | `codex` / Codex | Auto App/CLI/managed RPC and account switching; additional windows/reset forecasts are not inferred from local token totals. |
| [ ] | `opencode` / OpenCode | API, Web, opt-in local estimate; ambient/saved-account ownership and alias reconciliation. |
| [ ] | `cursor` / Cursor | Managed/automatic Web accounts; login, self-sync and quota observations retain independent lifecycle. |
| [ ] | `antigravity` / Antigravity | Local RPC and managed standalone OAuth; account dedup, refresh persistence and partial discovery. |
| [ ] | `cline` / Cline | Configured API key wins over read-only Desktop/CLI sign-in. Rejected key does not switch accounts; never refresh Cline-owned credentials. |
| [ ] | `factory` / Factory Droid | API key and Droid-owned discovery; preserve `droid` client mapping. |
| [ ] | `kimi` / Kimi | Code API plus optional Web windows; keep independent sources and precedence. |
| [ ] | `grok` / Grok | Auto CLI/RPC and Web billing; preserve env proxy routing and account identity. |
| [ ] | `copilot` / GitHub Copilot | Explicit login/OAuth/API; browser/device authorization and account state need live/platform verification. |
| [ ] | `zed` / Zed | Manual dashboard Web cookie; usage can survive optional subscription failure; unlimited edit prediction is not zero allowance. |
| [ ] | `commandcode` / Command Code | Manual Web credits/windows; allowance units do not justify measured local token counts. |
| [ ] | `mimo` / Xiaomi MiMo | Console wallet/Token Plan and automatically discovered Desktop membership; distinct Xiaomi services, no duplicate wallet/components. |
| [ ] | `muse` / Muse | Native read-only CLI and manual Web. CLI allowance has upstream live schema evidence; Web live acceptance remains unverified. Anonymous CLI/device and Web winning-observation policies differ. |
| [ ] | `zai` / GLM | Personal Coding Plan API/discovery and ZCode attribution; preserve credential/account/product scopes. |
| [ ] | `zaiteam` / GLM Team | Separate Team API key, organization and project; not a personal-plan alias. |
| [ ] | `kiro` / Kiro | Automatically discovered CLI; supported native launcher/auth paths need platform acceptance. |
| [ ] | `workbuddy` / WorkBuddy | App-owned session, managed official browser OAuth, advanced headless fallback; encryption failures are not sign-out. Verify personal/enterprise and refresh mutation fences. |
| [ ] | `qoder` / Qoder | Manual global/CN Web credit quota; independent of local `qodercn` token identity. |
| [ ] | `deepseek` / DeepSeek | API wallet/usage; `dsh` tokens map here but do not authenticate billing or imply spend. |
| [ ] | `devin` / Devin | Manual Web bearer plus organization; local usage and account credits remain separate. |
| [ ] | `minimax` / Minimax | Local Code OAuth, Token Plan keys and API-wallet keys; per-key region/billing scope, shared-wallet dedup, CREDITS versus cash. |
| [ ] | `mistral` / Mistral | Manual Admin Web budget/credits plus bounded Vibe fallback; no stable-account identity invented from cookies. Live auth remains unverified. |
| [ ] | `typesafe` / TypeSafe | Console Billing action and Usage buckets separately; spend is a website-rate estimate, nearest credit expiry is not monthly reset. Upstream browser evidence does not validate native transport. |
| [ ] | `openrouter` / OpenRouter | Multiple configured API accounts; standard key versus Management-key capabilities and observed wallet ownership. |
| [ ] | `volcengine` / Volcengine | Explicit Ark/access keys or local personal-plan arkcli; configured credentials do not fall back to another account on failure. |
| [ ] | `ollama` / Ollama | API key wins over Web cookie; legacy/monthly windows and purchased balances; invalid API key never falls back to Cookie. |
| [ ] | `trae` / Trae CN | Manual Web cookie and retained legacy IDE-token route; origin restriction and current-auth limitations. |
| [ ] | `alibaba` / Alibaba Cloud | Four console/plan variants; upstream live evidence is CN only. Base quota excludes add-ons; team account totals are not per-seat allocation. |
| [ ] | `stepfun` / StepFun | Manual Oasis token, Coding/Token Plan windows; weighted credit buckets, single retry and credential-fenced rotation. Both live plan variants need acceptance. |
| [ ] | `notion` / Notion AI | Manual cookie + explicit workspace only; internal APIs, fixture-only, live validation requires separate approval. Allowance is neither tokens nor money. |
| [ ] | `manus` / Manus | Manual Web or macOS/Windows plaintext desktop session; encrypted-only stores unavailable, no decryption. Undocumented credits RPC has narrow upstream evidence. |
| [ ] | `opendesign` / OpenDesign | Manual Web current-workspace quotas; verified workspace identity, no all-workspace aggregation or inferred money. |
| [ ] | `thirdparty` / Third-party APIs | Multiple New API account/token, Sub2API and Custom adapters; adapter-specific units, URL/credential policy and spend boundaries. |

- [ ] Preserve `notConfigured`, `unauthorized`, `unavailable`, throttled/stale/ok distinctions and last-good/last-attempt data.
- [ ] Preserve independently missing accounts/windows and explicit removals; consume internal markers before serialization.
- [ ] Use credits/spend metric semantics, not provider whitelists; derived display percentages stay out of wire records.
- [ ] Fixed/adaptive scheduling, bounded concurrency/deadlines/backoff; local token writes and hover do not probe quotas.
- [ ] Credential-save probes use exactly the submitted lane; late results cannot overwrite newer drafts, accounts or credentials.
- [ ] Fresh limits selections are seeded only from currently detected mapped local sources, in provider order; retained usage alone is not detection. Preserve saved selections and ordering.

## Accounting, collection and analytics

Reference: [architecture](../../token-monitor/docs/architecture.md), `src/shared/collector.js`, `usage/`, `sessionDetail.js`, `sessionLive.js` and matching provider readers/tests.

| Done | Feature | Required behavior |
| --- | --- | --- |
| [ ] | Today / Month / All Time | Exact canonical client/session aggregation, signed corrections, explicit zero and retained missing identities. Do not approximate full-period deltas. **Partial:** checked period totals, retained identity replacement, explicit zero days and explicit unclassified remainders are implemented and GUI-accepted for the supported readers; provider-complete aggregation and signed-delta persistence remain open. |
| [ ] | Token components | Provider-specific additive/subset reasoning, cache read/write/one-hour rules, explicit totals and unknown remainder. **Partial:** component totals and per-model retained remainders are surfaced; provider-specific completeness rules remain reader work. |
| [ ] | Cost and pricing | Actual/estimated/free/unpriced provenance; provider-scoped custom rates; incomplete price coverage preserves source cost. Price revision changes invalidate mixed anchors. |
| [ ] | Automatic price catalog | Background refresh independent of source events; resident scans offline; cache/backoff and revision-triggered reconciliation. |
| [ ] | Generation speed | Matched timed-output counters; performance timing is not session duration and is never apportioned by total-token coverage. |
| [ ] | Source health | Exact file/root checks, canonical paths, symlinks, detected/missing/unavailable distinctions without exposing private paths on wire. |
| [ ] | Source settings | Client selection, exclusive/custom roots, provider root rules, projects/history/archive switches and consistent attribution. |
| [ ] | Monitoring | Native events by default, debounce without cooldown, mid-scan retry and bounded continuous-write waiting; root replacement releases old watchers. |
| [ ] | Watch failure | Descriptor exhaustion/budgeted polling or interval fallback; explicit polling policy and clear diagnostics. DB/WAL remains watched; self-written SHM/cache is not a trigger. |
| [ ] | Reconciliation | Serial complete scans, day rollover and source/pricing rebaseline; no publication of partially failed sources as complete empty data. |
| [ ] | WSL | Running distros only; never start/install one; serial full-tick scans with frozen WSL anchor between native Today updates. Respect excluded app-data clients. |
| [ ] | Sessions | Search/filter/sort, latest model, device/project attribution and retained sessions; page complete groups rather than truncating members first. |
| [ ] | Session relationships | Verified native/T3 parent/continuation/delegation links; retain all members on conflicts/cycles; links never invent usage. |
| [ ] | Live activity | Separate bounded generation-fenced lane, leases/transcript fallback; native adapters currently Claude/Codex only. Local zero-token sessions stay out of archives/wire usage. |
| [ ] | Session Detail | Lazy bounded transcript read, 16 MiB record cap, prompt/turn/replay dedup, supported compressed formats and source-specific costs; retain upstream detail/parser discrepancies visibly. **Partial:** bounded canonical rollup detail with per-model share is implemented and GUI-accepted; transcript reads, replay dedup and source-specific costs remain open. |
| [ ] | Context/prompt cache | Provider-owned indexed observations and valid denominators only; disabled features do not launch extra transcript readers. |
| [ ] | Title/project metadata | Explicit title precedence and in-place metadata updates without advancing token/activity baselines; display hiding independent from sync consent. |
| [ ] | Projects / Models | Canonical workspace identity, model aliases and breakdowns; opaque workspace keys do not become fabricated path hashes. **Partial:** period-scoped client/model breakdowns are implemented and GUI-accepted; project identity and aliases remain open. |
| [ ] | History / Trends | Daily/client/model components, charts/heatmap/monthly rollups and retained trends; missing is unknown, not zero. Preserve the reference's roughly 370-day exported daily-history window separately from session archive retention; importing older originals does not authorize pruning them. **Partial:** contiguous 30-day daily totals from live canonical rows plus retained Today are implemented and GUI-accepted; persisted multi-month history, heatmaps and monthly rollups remain open. |
| [ ] | Quota history | Successful fresh observations only, 180-day detail/730-day daily retention, 14-day preview; gaps/reset boundaries and no hover probes. |
| [ ] | Alerts | Quota/reset/burn-rate behavior, account/provider selection and hidden-account policies; no speculative model inference or extra polling loop. |

## Persistence and import

Reference: `src/shared/usage/sessionUsageArchiveStore.js`, [archive replay](../../token-monitor/docs/archive-replay.md), [limits history](../../token-monitor/docs/limits-history.md), [configuration](../../token-monitor/docs/configuration.md), `credentialStore.js` and app sync ledgers.

| Status | Existing data | Native import/recovery rule |
| --- | --- | --- |
| U | `settings.json` | Exact bytes/unknown fields are preserved; supported credential-free fields import and activate transactionally. Native edits change only clients, IANA timezone and opaque source IDs through revision CAS. Invalid, unknown-credential, or stale input cannot become an empty replacement. Linux fixture/application coverage passes; complete setting parity and platform runtime acceptance remain open. |
| U | `credentials.json` and legacy embedded secrets | Supported staged values are authoritative and use target-native keyring storage with write/read-back verification before settings activation. Originals remain untouched; unavailable/invalid/unknown branches stay pending and inactive. Fake storage is test-only. Live Secret Service, Credential Manager and Keychain acceptance remains open. |
| U | `session-usage-archive.sqlite`, schema v1 | Raw main/WAL/SHM source files are opened only through a fenced private snapshot. Validated metadata/session rows, exact original JSON and revisions are preserved; retained periods persist separately and rebuild canonical retained rollups. Mixed WAL-only fixtures pass; broader archive semantics and Windows/macOS runtime remain open. |
| U | Legacy session JSON | Recognized JSON archives are preserved but semantic import is currently unsupported and inactive; canonical identity/remainder parity remains N. |
| U | Daily/client archives | Recognized daily history is privately preserved and explicitly reported unsupported/inactive. Historical semantic import and attribution remain N. |
| U | `codex-local-usage.sqlite` | The exact supported four-table ledger is read through a fenced private snapshot; checked request counters become canonical Codex observations, account keys are excluded, and rows for native rollouts are suppressed. Mixed WAL-only fixtures pass; broader ledger/platform semantics remain open. |
| U | `limits-history.sqlite` | Recognized files are privately preserved and explicitly reported unsupported/inactive. Quota-history semantic import remains N. |
| U | Hub `devices.json` | The recognized snapshot is privately preserved and explicitly reported unsupported/inactive. Snapshot/subscription/policy import remains N. |
| [ ] | Sync consent/cleanup journals | Preserve destination-bound revocation obligations and former device IDs; no cleanup/deletion while original destination state is unresolved. |
| U | iCloud writer/revision/tombstone ledgers | Recognized revision data is privately preserved and reported unsupported/inactive. Whole-list, winner, in-flight and tombstone semantic import remains N. |
| U | Plugin packages/settings/data/credentials | Recognized trees are copied without following links and remain unsupported/inactive; no JS/Node is executed or deserialized. Semantic trust/preferences/credential import and native replacements remain N. |
| U | Import report | Preservation, semantic-import and activation states are stored independently and projected through a bounded redacted UI model. Generated fixtures cover recognized supported/unsupported/failure states; display-backed acceptance remains open. |
| U | Crash-safe native writes | One bounded SQLite writer owns WAL transactions, schema upgrades, verified backups, corruption retention/recovery, retryable failure, and post-commit immutable snapshots. Linux headless recovery passes; Windows/macOS filesystem/ACL/runtime evidence remains open. |

## Exports

Reference: [export.md](../../token-monitor/docs/export.md) and `src/shared/export` implementations.

- [ ] `token-monitor-export.json`: numeric snapshot and available history; not a full application backup.
- [ ] `token-monitor-snapshot.csv`: period/tool/model totals.
- [ ] `token-monitor-daily.csv`: conditional on available daily history.
- [ ] `token-monitor-daily-models.csv`: conditional components, including unclassified remainder.
- [ ] UTF-8 BOM/RFC4180 CSV, ISO dates and USD cost columns; exclude device/account/plan/quota identity.
- [ ] Folder picker, immediate export and configured auto-export cadence; unchanged output produces no rewrite.

## Native UI and settings

Reference: `src/electron/main.js`, `runtimeConfig.js`, `renderer/`, [configuration.md](../../token-monitor/docs/configuration.md) and native controller directories. Reproduce actions and state retention in Slint, not DOM/CSS code or Electron runtime behavior.

Current subset: the Ledger Rail Home, Sessions, Source Health, Gate 1 Settings/migration report, About, Trends and Models surfaces are **U** (implemented, adapter-tested, compiled, and Linux GUI-accepted for the supported states). Home supports Today/Month/All Time selection; Trends shows a contiguous 30-day series; Models shows period-scoped client/model share; Sessions exposes a bounded canonical session detail with per-model share. Settings currently covers client selection, opaque discovered-source selection, IANA timezone, revisioned Save/Cancel, conflict retention, credential availability/pending state and import-report labels—not the complete upstream settings surface. Week, Limits, transcript-level detail, projects, alerts and quota history remain unavailable rather than aliased to implemented behavior. Cost remains explicitly unavailable. Startup uses validated discovery plus committed native settings; no fixture values are projected in live startup.

| Done | Surface | Acceptance |
| --- | --- | --- |
| [ ] | Main / Home / Dashboard | Functional totals, cards, quotas, sessions, models/projects/trends, detail navigation and refresh. No invented demo data in live mode. |
| [ ] | Subscriptions | Editable backend-scoped list and account binding; healthy unidentified accounts remain selectable. Shared-list revisions and offline-write restrictions also apply to the UI, not only the HTTP endpoint. |
| U | Settings | Gate 1 collection controls have validated drafts and working revisioned Save/Cancel with stale-conflict retention. General, Main, Window, Appearance, Limits, Subscriptions, Sync, search and display-backed acceptance remain N/open. |
| [ ] | Theme / typography | Light/dark/system, colors, units, sizing, reduced motion and background settings; readable focus/contrast, consistent density. |
| [ ] | Localization / currency | en, zh-TW, zh-CN, ko, ja, pt-BR and system auto; USD/TWD/HKD/CNY, auto/manual conversion distinct from USD accounting. |
| [ ] | Window lifecycle | Position/scale/always-on-top/visibility/fullscreen policies, close/minimize behavior; retain navigation, draft, selection and scroll on ordinary changes. |
| [ ] | Tray / menu bar | Icon/stat modes, menu/refresh/shortcut, tray-only popover; no duplicate collectors. |
| [ ] | Floating bubble | Expand/collapse and retained controls/state; disabled surface stops its work. |
| [ ] | Edge dock | Floating/docked placement, handle/input passthrough, detailed cards and fullscreen hiding; native hit testing, physical-DPI geometry and fallback. |
| [ ] | Windows materials/input | Acrylic/Accent/backdrop/transparency, nonactivation and z-order; Rust Win32 implementation needs Windows runtime acceptance. |
| [ ] | macOS notch | Fixed pages/geometry, camera clearance, input/swipes, quota alerts/history and material fallback; macOS acceptance required. |
| [ ] | macOS widget | WidgetKit is an OS extension, not a Slint window. Whether minimal Swift bridge is acceptable under the all-Rust request remains unresolved; do not claim a Rust-only substitute is identical. |
| [ ] | Global shortcut / autostart | OS registration, duplicate handling, shutdown cleanup and saved choice; verify per platform. |
| [ ] | Notifications / Discord | Native notifications and optional Discord RPC; disabled work absent, no credential/content leakage. |
| [ ] | Updates / About / diagnostics | Native release check/install flow with safe quit, version/build identity, licenses/Slint attribution, redacted diagnostics and useful errors. **Partial:** HTTPS GitHub Releases check, stable version comparison, platform asset selection, SHA-256 verification, size bounds and Windows/Linux install are implemented; About exposes checking/available/downloading/manual/installing states. macOS automatic bundle replacement, update preferences/channels, and execution against a published release remain open. |
| [ ] | Previews / hidden pages | Preview reads captured state only; no migration/probe/collector launch. Hidden surfaces release demand without discarding settings or drafts. |

Slint itself is not MIT. Preserve applicable upstream MIT notices and satisfy Slint Royalty-free 2.0 attribution through accessible About/allowed public badge. A custom extension API must not expose Slint APIs under that license without separate review.

## Service status

Reference: [serviceStatus.js](../../token-monitor/src/electron/serviceStatus.js).

- [ ] Statuspage: `claude`, `openai`, `cursor`, `factory`, `kimi`, `devin`, `minimax`, `minimax-intl`.
- [ ] GitHub status scoped to Copilot/model-provider components, not all GitHub services.
- [ ] Zed Instatus, OpenCode custom JSON and official DeepSeek Atom feed.
- [ ] Cache success/error for 60/10 seconds by default, request deadline 5 seconds; explicit unknown/error on failure, maintenance distinct from outage.
- [ ] Status links and display action are functional. No speculative status endpoints for providers without an accessible official source.

## Headless, Hub and synchronization

Reference: [API.md](../../token-monitor/docs/API.md), [headless-agent.md](../../token-monitor/docs/headless-agent.md), `.env.example`, `src/agent/`, `src/hub/server.js`, `src/shared/sync*` and `src/electron/icloudSync*`.

| Done | Boundary | Required compatibility |
| --- | --- | --- |
| [ ] | Rust headless agent | Continuous/once/dry-run, CLI aliases/env precedence, working signals and source health; same collection/accounting core as desktop. |
| [ ] | Dry-run | No archive/PID/cleanup-state writes or Hub/title requests. It does not promise all ordinary collector/provider network reads are suppressed. |
| [ ] | Service ownership | Live agent makes desktop yield matching archive/upload ownership; verify process identity and avoid concurrent writers. |
| [ ] | Local/client/host/iCloud modes | Generation-fenced switching, last-good recovery and shared collectors; no agent iCloud sink invented. |
| [ ] | HTTP auth/admission | Secret/Bearer compatibility, limits/body validation, no-secret Hub loopback-only policy and bounded shutdown. |
| [ ] | Health, ingest and deltas | `GET /api/health`, `POST /api/ingest`, `POST /api/ingest/delta`; acknowledged baseline tokens, recovery/full fallback and normalization before storage. |
| [ ] | Stats/history/devices | `GET /api/stats`, `/api/history`, `/api/devices`; `DELETE /api/devices/:id`. Same IDs replace, different devices add; stale devices remain until eligible explicit removal. |
| [ ] | SSE | `GET /api/stats/stream`; legacy/full, v3 freshness and v4 revisioned patch fallback, reconnect/bootstrap, bounded slow clients and no cross-generation baselines. |
| [ ] | Subscriptions | GET/PUT `/api/subscriptions`, optimistic revision conflict, account binding, backend-scoped one-list persistence and no offline fork. |
| [ ] | Public versus private stats | Subscription/shared-content revision markers are authenticated-only; public stats expose neither private markers nor titles. Preserve public-route compatibility without making private data public. |
| [ ] | Shared content | `GET /api/sync/content`, GET/PUT `/api/sync/settings/:kind`; allowlisted pricing/alias groups, zero versus unset and revision-bound edits. |
| [ ] | Title privacy | PUT `/api/sync/titles/:deviceId`; independent server/sender permission, generation negotiation, OFF abort/purge, durable old-destination cleanup and rejected redirects. |
| [ ] | iCloud | Atomic file publish/reconciliation, former identities/tombstones and whole-list subscription winner; preserve unresolved journals, offline/error distinctions. |
| [ ] | Hub persistence/shutdown | Atomic JSON, configured bounded batch deadline, immediate policy/deletion saves, retry/rollback and admitted-body/SSE draining. |
| [ ] | External Cloudflare Hub | Native client/agent interoperates with existing Worker protocol. Rust desktop must not require executing bundled Worker JS/Node. A separate Worker deployment rewrite is not equivalent to desktop parity. |
| [ ] | Configuration contract | Saved desktop preferences override first-run env; agent/Hub CLI > env/.env > defaults. Unset/empty/explicit client/provider selections differ. |
| [ ] | Proxy policies | App/env/OS policy and bypass behavior, provider-specific request handling and cancellation; sync/release/external browser routes retain their documented boundaries. |

## Plugin compatibility decision

**X: unchanged `tmApi:1` JS DOM/CSS UI and optional Node backend execution.** Native Slint has no DOM, and embedding a JS/webview/Node host would contradict the requested runtime. Native functionality can be ported, but this is a package/API breaking change rather than complete unchanged-plugin parity.

- [x] Preserve recognized package/settings/data/credential trees as private inactive artifacts without executing imports or following links; semantic trust/preference adoption remains a separate unfinished feature.
- [ ] Show incompatibility clearly; never turn an unsupported package into a silently empty enabled plugin.
- [ ] Inventory built-in plugin behavior and port useful native equivalents separately, including layouts/skins/data views/notifications/quota modules where required.
- [ ] Keep bounded snapshot/history query and privacy contracts; no plugin-derived Hub/wire/history behavior added accidentally.
- [ ] No speculative Rust/WASM extension ABI in the initial migration. A new custom plugin platform requires its own design and licensing review.

## Verification and resource acceptance

- [ ] Before sync, compare the entire intended local/remote file set; the existing VM mirror lacks Git metadata. Three matching catalog hashes do not authorize overwriting unknown files.
- [ ] All compilation, test suites, parser acquisition, benchmarks and packaging run via SSH on `pixvault-vm`, never `rustdesk` or this local PC.
- [x] Remotely pass rustfmt, locked all-target check/test, strict Clippy and release build for both desktop/all-features and headless/no-default-features. Final Gate 2A snapshots are `core-fg6breu7` (desktop) and `core-j4ubtphg` (headless); all **220 ordinary tests** passed with two intentional credential probes ignored.
- [x] Re-run both matrices after Gate 1 Task 1 discovery: desktop/default features `core-f5sq6ka8` and headless/no-default features `core-0odzjucp` passed all gates and all 87 tests.
- [ ] Differential fixtures for every reader against the exact selected reference; record platform-gated cases and provider-specific component expectations.
  - Exact pinned Claude complete rows and the supported Codex prefix/unsupported boundary pass in `core-ltrzx0ow`; complete Codex and all other readers remain unchecked.
- [x] Integration checks: append/partial record/rotation/truncation/rewrite+growth/correction; WAL-only SQLite; unsupported schema and wrong-role replacement; source removal versus read failure; restart/offline retained state; backup/corruption recovery; no duplicate rows after source restoration. Focused storage/adapter suites additionally cover lock/busy and crash journals.
- [x] Runtime checks: Linux GUI navigation, settings validation/cancel/select-all/save, session search/paging/selection, refresh, source error/recovery, About, keyboard focus, reopen persistence, graceful close, and absence of Electron/Node/webview in the native distribution.
- [x] Inspect the Linux release artifact for forbidden runtimes: stripped x86-64 PIE ELF, 25,422,072 bytes, SHA-256 `6e6bd4518a0fb7274cd807e6fb7bbddc0e126d7c7f2ebd6b5ab82eab74d01ab6`; direct `NEEDED` is DBus/Fontconfig/GCC/libm/libc/loader only, with no direct zstd/sqlite linkage or `ZSTD_*` symbol. Strings contain no Electron, Node.js, Chromium, webview, `tokscale-core`, scanner sidecar, reference checkout, or zstd/sqlite3 subprocess runtime; the `Tokscale` attribution remains. This is Linux evidence, not Windows/macOS acceptance.
- [x] Measure release idle CPU/RSS, startup, update latency, shutdown, thread/descriptor counts against the 100-source/1,000-session fixture: cold-startup p95 727 ms, warm p95 630 ms, idle RSS max 45,512 KiB, idle CPU 0.1%, append 565 ms, rewrite 531 ms, burst 716 ms, removal 503 ms, recovery 702 ms, shutdown 95 ms, 54 descriptors, 11 threads, 3 watches, zero child processes/network listeners. All approved thresholds passed (`core-cdrti43e`).
- [ ] Compare software/native renderer costs when supported; no cosmetic animation keeps an idle/hidden app continuously rendering.
- [x] Windows x86-64 GNU cross-build runtime evidence: the complete suite passed 228 ordinary tests plus two real Credential Manager lifecycle tests, the 1,000-session acceptance fixture passed with the pinned zstd CLI, and the isolated app launched, rendered, stayed responsive and exited 0. Two read-only-sync/open-handle portability bugs were fixed. Portable zip SHA-256 `514529efc133dfd8877c93bb20384c381eecf325ec927af4e92fe0fc351102a3`; no Electron/Chromium/webview/`tokscale-core`/Node.js strings.
- [ ] Windows MSVC release installer lifecycle, NTFS ACL/long-path/non-ASCII checks, full GUI accessibility automation and resource measurements remain open. The prepared GitHub Actions workflow has not run because the tree is uncommitted.
- [ ] macOS Apple Silicon build/runtime/keychain/GUI/resource acceptance remains open; Linux and Windows results do not substitute for a macOS build. Unsigned/ad-hoc distribution is the approved target.
- [ ] Notion live requests require separate approval/manual credentials; avoid real-account state-changing probes or unintended model turns elsewhere.
- [ ] REA MCP analysis remains blocked by current execution routing: connected Windows PTY capture is unavailable, and VM has no REA/Node. No application-analysis Evidence has been produced. Source inspection remains useful but is not REA decompilation/runtime evidence.
- [x] Remote headless GUI prerequisites are resolved on `pixvault-vm`: Xvfb, Matchbox, AT-SPI, xdotool, maim, DBus and Secret Service are installed, and GUI/resource modes run under an isolated display/session. The host may carry unrelated workloads; thresholds were measured with current load recorded.

## Completion rule

The migration is complete only when every in-scope feature has implementation and appropriately scoped verification, and incompatible/platform-blocked rows are explicitly resolved. A polished shell, passing unit tests or all catalog labels appearing in settings is not full migration. Update this checklist at each verified stage.
