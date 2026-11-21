# Token Monitor Lite

An in-progress native Rust + Slint migration of Token Monitor 0.88.0. The current tree contains a verified native persistence, migration, settings, and credential foundation, but it is **not a feature-complete release**.

## Current status

- Exact checked-integer accounting, catalog identities, read-only settings inspection, named-IANA/DST bucketing, canonical aggregation, native file watching, immutable monitor snapshots, and last-good retention are implemented.
- Narrow Claude CLI and Codex CLI JSONL subsets are fixture-verified. The Gate 2A boundary now also covers ordinary Claude/Codex JSONL, streaming Codex `.jsonl.zst`, schema-v1 retained session archives, and Codex local-usage SQLite/WAL. OpenClaw ownership, non-rollout headless documents, unverifiable fork/record ownership, arbitrary alias tables, Claude Desktop IndexedDB, and path-derived provider guesses remain explicitly unsupported.
- A single-writer SQLite store provides schema migration, compare-and-swap settings, transactional imports, verified backups, corruption recovery, and immutable snapshots. Unknown settings fields and original imported session JSON remain byte-exact.
- Startup now discovers validated Claude/Codex candidates, stages recognized legacy artifacts without modifying them, imports the supported settings/archive subset, retains unsupported artifacts inactive, and starts monitoring only from committed source selections.
- Platform secrets use native `keyring` backends only: Windows Credential Manager, macOS Keychain, or Linux Secret Service. There is no production fake or plaintext fallback. Credential activation is journaled as `Prepared → SecretWritten → Verified → MetadataCommitted`.
- **Headless agent**: `token-monitor-lite-agent` is a separate binary that builds without any window system and shares the desktop's discovery and aggregation. `--once` collects and persists; `--dry-run` runs the same pipeline against a discarding store, so it reports what a real run would find while writing nothing and touching no network (measured: 460 sources, 0 files written). Precedence is CLI → env → default. Hub sync, iCloud and plugins were dropped from this migration, so the agent is **local only** and continuous mode is refused rather than faked — use `--once` from a scheduler. See `docs/migration/headless-spec.md`.
- **Settings** covers General, Appearance (light/dark/system, dense/comfortable rows, reduced motion), Currency (USD/TWD/HKD/CNY) and Language (en, zh-TW, zh-CN, ko, ja, pt-BR plus auto). Everything goes through the existing revisioned Save/Cancel, and **unknown fields — including the reference's `themeColors` — survive a save** with their key set and values intact. Currency is display-only: accounting stays in USD, and the conversion API returns text so it cannot leak into a total. Language catalogs are generated from the upstream 2,428-key catalogue by `scripts/generate-settings-i18n.py`; only the settings sections are translated so far. Contrast is computed in `src/theme.rs`, which found the shipped dark faint text below WCAG AA (4.12:1) and fixed it to 4.66:1. Desktop appearance detection and the AT-SPI re-run are unverified — see `docs/migration/settings-spec.md`.
- **Data export** writes the four upstream files (`token-monitor-export.json`, `-snapshot.csv`, `-daily.csv`, `-daily-models.csv`): UTF-8 BOM, RFC 4180 CRLF quoting, ISO dates, `cost_usd` columns, and no device/account/plan/quota identity. Manual export and a 30 s–60 min auto-export cadence share one writer; an unchanged payload rewrites nothing and a stale conditional file is removed. Byte-exact CSV and JSON goldens live in `tests/export_golden.rs`; see `docs/migration/export-spec.md`. Pickers and synced-folder behaviour are unverified.
- **Trends** spans the reference's ~370-day daily history with monthly rollups and a 53-week heatmap. A day before tracking began renders as **unknown**, never as zero, and is excluded from totals; a month containing an unknown day is marked partial.
- Provider **limits** are implemented for **Claude and Codex only** (`src/limits/`): status vocabulary, last-good retention, bounded concurrency, deadlines, backoff, reset boundaries, and Home quota bars with reset times on a Slint-only surface. Credentials come from the existing keyring service and nowhere else; no token is logged and no plaintext fallback exists. Probes are **ASSUMED**: no live endpoint has been contacted, and every test replays a recorded fixture. No other client's quota lane exists yet, so it yields no bar rather than a guessed one. See `docs/migration/limits-spec.md`.
- The Ledger Rail Home, Sessions, Source Health, Settings, migration report, and About shell compile with Slint 1.18. Home now supports checked Today/Month/All Time totals with retained-archive replacement, a 30-day Trends view, and a period-scoped Models breakdown. Settings Save/Cancel, select-all, stale-edit retention, opaque source selection, redacted migration state, paging, search, refresh, and latest-only UI delivery are headless-tested.
- The monitor worker commits a canonical candidate before publishing it. Blocked writers keep the last committed rows visible with a fixed persistence-delayed state; stale settings-generation commits are never published; unchanged source contents reuse the committed generation, so startup and single-source updates avoid full re-commits.
- About checks the project's GitHub Releases over HTTPS at startup and on demand, selects the platform asset, verifies its SHA-256 digest (GitHub asset digest or release `SHA256SUMS`), and size-bounds the download. Installing requires an explicit click: the Windows Inno installer runs silently, Linux uses `pkexec dpkg -i`, and macOS currently downloads the verified zip for manual replacement. No update is downloaded or executed automatically.
- A system tray shows today's tracked tokens and cost on the icon, opens a menu (Open, Refresh, Quit), toggles the window on left click, and can keep the application running when the window closes. It is built on Slint 1.18's own `system-tray` feature — Win32 `Shell_NotifyIcon`, AppKit `NSStatusItem`, and `ksni`/StatusNotifierItem on Linux, with no GTK — so there is no second event loop, watcher or collector. `closeToTray` is a native settings key written through the same revision CAS as the timezone. What is verified and what is still assumed, including the Linux desktop fallback, is in [`docs/tray.md`](docs/tray.md).

The Linux GUI flow is runtime-accepted under Xvfb/Matchbox/AT-SPI against the mixed 100-source/1,000-session fixture: initial empty state, migration report, settings validation/cancel, select-all, 100-source save, period switching, Trends, Models, sessions search/paging/selection, bounded session detail, refresh, source failure/recovery, About, keyboard focus, reopen persistence, and graceful close. On Windows x86-64, a GNU cross-build launches, renders, stays responsive, and exits cleanly in an isolated profile; its 228 ordinary tests, two real Credential Manager lifecycle tests, and the 1,000-session acceptance fixture pass on a real Windows 10 host. MSVC installer lifecycle, full Windows GUI/ACL/resource acceptance, and macOS runtime/package acceptance remain open.

## Platform builds and signing

Windows and macOS builds run on public GitHub Actions runners (`windows-2022` and `macos-14`). Release artifacts are intentionally unsigned: the Windows installer has no Authenticode certificate and macOS bundles are ad-hoc signed but not notarized, so they show SmartScreen "Unknown publisher" and Gatekeeper warnings. Users can install anyway with **More info → Run anyway** on Windows, or **right-click → Open** / System Settings → Privacy & Security → Open Anyway on macOS; the equivalent command is `xattr -dr com.apple.quarantine "/Applications/Token Monitor Lite.app"`. Code signing and notarization are an explicit non-goal; no Apple Developer account is required.

The platform pipelines are prepared but have not run because the working tree is intentionally uncommitted. They are not evidence of Windows or macOS acceptance until they execute and produce retained artifacts. A tag-driven `release` workflow builds Linux `.deb`/`.tar.zst`, the Windows setup, and the macOS zip, adds `SHA256SUMS`, and publishes the GitHub release the updater reads.

macOS automatic bundle replacement and update settings (auto-download, channel, skipped versions) remain unfinished; macOS updates download and verify but require manual replacement.

## Data locations and migration

| Platform | Native private data | Legacy user data |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\Token Monitor Lite` | `%APPDATA%\Token Monitor` |
| macOS | `~/Library/Application Support/Token Monitor Lite` | `~/Library/Application Support/Token Monitor` |
| Linux | `${XDG_STATE_HOME:-~/.local/state}/token-monitor-lite` | `${XDG_CONFIG_HOME:-~/.config}/Token Monitor` |

`TOKEN_MONITOR_LITE_DATA_DIR` overrides only the native private root. `TOKEN_MONITOR_SHARED_DIR` overrides only the legacy shared-data root; it does not replace the legacy user-data root. Overrides must be absolute, normalized, and non-overlapping.

Migration follows these rules:

- Legacy files are opened read-only. Recognized files and plugin trees are copied into private, opaque staging directories before validation; originals are never deleted, rewritten, or used as the native working store.
- A raw SQLite/WAL/SHM bundle is retained as evidence. Semantic archive import uses a separate coherent SQLite backup, requires `quick_check`, validates schema-v1 metadata and every row, then commits settings, sessions, and report rows in one native transaction.
- Preservation, semantic import, and activation are independent report states. For example, unsupported stores and JavaScript plugins are `Preserved / Unsupported / Inactive`; they are not treated as empty or executed.
- Credential-bearing settings stay inactive until every supported staged value is written to and read back from the native credential store. Unknown or invalid credential branches remain preserved and explicitly pending/unsupported.
- Settings edits retain unknown JSON fields. Save uses the displayed SQLite revision; a stale save is rejected without overwriting the newer committed value, and Cancel performs no write.

The native database is `token-monitor-lite.sqlite3` (schema v4). Before a schema change, the application creates and verifies a backup under `backups/`. If the active database later fails integrity checking, the corrupt bytes are retained under `recovery/` and the newest verified compatible backup is restored. If no valid backup exists, startup reports recovery required and does not create an empty replacement. The mixed-format acceptance test verifies backup creation, a post-backup commit, deliberate database corruption, restoration to the exact backup generation, and a byte scan proving no private path, credential sentinel, account key, or transcript identity crossed into native files.

## Not yet implemented

Pricing and costs, limits/accounts, session detail/relationships/live activity/titles/projects, quota history, alerts, the complete upstream settings surface, exports, synchronization/Hub, tray/dock/platform surfaces, and native plugin replacements remain unfinished. Today/Month/All Time totals, a 30-day Trends view, a period-scoped Models breakdown, and bounded canonical session detail are implemented only for the supported readers. Windows/Linux update installation and the About update check are implemented; macOS automatic replacement, update preferences and channels remain open. Unsupported reader shapes named above stay visible rather than approximated. Unsigned Windows and ad-hoc-signed macOS runtime/package acceptance remain open; signing and notarization are intentionally omitted. Existing `tmApi:1` JavaScript/DOM/Node plugins are preserved as migration data but are not executed by the native application.

See [`docs/migration/inventory.md`](docs/migration/inventory.md) for the complete evidence and parity ledger.

### Upstream parity

Token Monitor 0.88.0 is cloned into `token-monitor/` as a **reference only** — it is not a build dependency, it is not vendored, and it is expected to be deleted once the migration is finished. Parity against it is:

- **Every one of the 42 tracked clients in `clientCatalog.js` has a reader here.** Verified against `clientSourceRegistration.js`, which is upstream's authoritative `SOURCE_MARKERS` root table, not against a list copied by hand.
- **Eleven extra clients are present** that upstream does not track: `kimchi`, `mux`, `junie`, `augment`, `gjc`, `codebuff`, `freebuff`, `opencodereview`, `trae`, `bb`, `roocode`. These are parser modules shipped in the pinned tokscale reference (or, for `roocode`, a native reader written against Roo Code's own task-log format) but never added to Token Monitor's catalog. They are exposed here because the reader contract, the reconciliation test and the catalog must agree — a parser with no catalog entry fails that test.
- The same is true of `crush`, `warp`, `aside` and `hindsight`: they are **not upstream clients**. Their absence is a catalog decision upstream made, not a gap here.

Known remaining differences from `SOURCE_MARKERS`, left for whoever picks this up:

- **`kiro`'s two credit-only stores**: the IDE/`--v3` window at `~/.kiro/sessions/<workspace>/sess_<uuid>/` and the pre-1.0 mirror under `globalStorage/kiro.kiroagent`. Both report **credits and no token counts** — the turn record is `{"payload":{"type":"usage_summary","promptTurnSummaries":[{"unit":"credit","usage":18.5}]}}`. A row with a real cost and zero tokens is indistinguishable from a measured zero in the token totals, so these are left unread rather than half-reported. The exact schemas are written down in [`docs/migration/reader-contracts.md`](docs/migration/reader-contracts.md) for whoever decides how a cost-only observation should be represented.
- **`copilot` with its OTel exporter enabled**: the exporter lane and the CLI `session-state` lane describe the same calls but share no identifier — the CLI keys on `accounting.usageId`, the OTel span on `gen_ai.response.id` — so the two cannot be reconciled and enabling both double counts. The other Copilot pairing was checked and is disjoint: the CLI and the VS Code editor stores share no session ids at all (7 vs 51 on the development host).
- Two extra `openclaw` homes and `kimi`'s `.kimi-code` lane **were** missing and have been fixed; see commit `5289a56`.

Every other `SOURCE_MARKERS` root is now read. The Cline-family task log is read for `roocode`, `cline` and `kilo`; Copilot contributes three lanes (OTel JSONL, the VS Code chat-session operation log, and the CLI's `session-state` event log); and `kiro-cli/data.sqlite3` is read alongside the `~/.kiro/sessions/cli` tree, with database conversations already reported from a session file dropped so the two cannot double count.

### Readers that are deliberately out of scope

Six further lanes have no byte-slice reader, and are excluded on purpose rather than approximated:

- **Perch** has no local usage format. `~/.perch` holds only MCP configuration, background-task JSON and log files, and its `cli-agents/sessions.json` is empty; the desktop application keeps conversation and usage state in browser-side IndexedDB against `app.perchai.app`. Reading it would mean parsing a LevelDB blob or calling an undocumented authenticated endpoint, so no reader is provided.
- **Aside**, **Crush** and **Warp** document no discovery root in their reference. Aside is not installed on the development host, and Crush and Warp name a database or cache file (`crush.db`, a usage cache) without saying where it lives. Inventing a root would produce a reader that silently discovers nothing, which is worse than an absent one. Crush is additionally **cost-only** — it has reliable session cost but no trustworthy per-message token columns, so a token report would read zero by design.
- **Hindsight** is an HTTP API rather than a file. A reader would introduce the first network and credential boundary into a reader layer that is otherwise pure byte slices.

A `perch`, `aside`, `crush`, `hindsight` or `warp` source therefore stays absent from discovery instead of appearing as a permanently empty client. Adding any of them is a separate piece of work with its own security review.

### Cross-file sources

A source may own more than one file. `MonitoredSource::companions` names files read after the primary into the same reader, in order, so the parts stay one source with one ledger row — reading them separately would count a repeated message twice, under two identities.

**`jcode` uses this.** A session snapshot can have a sibling `*.journal.jsonl` that appends the unsaved suffix and may repeat an entry to correct it; the reader keys messages by identity with the journal winning, and each journal batch re-scopes the billing provider for its own messages so a late route change cannot re-authenticate earlier ones.

**Roo Code does not need it**, despite an earlier note here claiming otherwise. Its `api_conversation_history.json` is an Anthropic `MessageParam[]` with no token counters at all, so all of its usage is in `ui_messages.json` and it is an ordinary single-file reader.

**Prime Agent does not need it either**, despite an earlier note here saying it was the one remaining case. The pinned reader reverses a parent's `child_usage_attributed` aggregate so the copied parent can be deduplicated against the child's own transcript. This reader never emits that aggregate — it is skipped — and the child's transcript is a source in its own right, so the usage is already counted exactly once. Reversing an aggregate we do not emit would be strictly worse. The residual limitation is the opposite one: if a child's artifacts are deleted while the parent's aggregate survives, that usage is not counted; it is recorded in `src/parsers/prime_agent.rs`.

The mechanism and the three ordering rules that make it work are written out in [`docs/migration/source-groups.md`](docs/migration/source-groups.md).

The VS Code `globalStorage` lanes did **not** turn out to need it. Each task log or chat session is a single file, so they are ordinary single-file readers.

## Remote verification

All compilation and tests are intentionally offloaded to `pixvault-vm`:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\remote-core-test.ps1 -Mode desktop-verify
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\remote-core-test.ps1 -Mode verify
```

When that host is unreachable, the same checks run locally with the pinned toolchain, which is what `rust-toolchain.toml` and the `.gitattributes` LF policy exist to make reproducible:

```powershell
cargo +1.99.1-x86_64-pc-windows-gnu fmt --all -- --check
cargo +1.99.1-x86_64-pc-windows-gnu clippy --locked --no-default-features --all-targets -- -D warnings
cargo +1.99.1-x86_64-pc-windows-gnu test --locked --no-default-features --no-fail-fast
cargo +1.99.1-x86_64-pc-windows-gnu build --locked --no-default-features --release
```

Use the pinned toolchain, not `+stable`: the unpinned `stable` on the development host is clippy 0.1.100, which reports lints the 1.99.1 pin does not, and it would make a green local run meaningless against the gate.

The first command runs formatting, all-feature check/test/strict Clippy, and a release desktop build. The second preserves the `--no-default-features` headless boundary. Both use isolated, content-hashed source snapshots, at most two Cargo jobs, and a shared bounded target under the remote `evidence/` root.

### Text encoding

Every text file is UTF-8. On Windows that is worth stating, because the default is
not: PowerShell 5.1 decodes a `.ps1` file **without a BOM** as the ANSI codepage.
A script containing a multi-byte character is then unparseable, and text written
from that script reaches git as mojibake — one em dash becomes four bytes. Both
happened here; twelve corrupted sequences reached `docs/migration/inventory.md`
and one reached a commit message before anyone noticed, because a terminal
re-encodes what it prints and can hide the damage.

Three layers prevent it, cheapest first:

1. **Use PowerShell 7** (`pwsh`), which defaults to UTF-8 and is installed on this
   host as 7.7.0. Verified: `pwsh` reads a BOM-less UTF-8 `.ps1` containing an em
   dash correctly (`E2 80 94`), while `powershell.exe` 5.1 fails to parse the same
   file with `Unexpected token`. If you stay on 5.1, save any script containing
   non-ASCII **with a BOM**.
2. **`.editorconfig`** sets `charset = utf-8` for the repository, so editors do not
   silently fall back to the ANSI codepage.
3. **`tests/encoding.rs`** fails the build for either failure mode: any tracked
   text file containing a mojibake sequence, or a PowerShell script with non-ASCII
   text and no BOM. It runs in both feature configurations because it is an
   ordinary integration test, so `cargo test` catches it.

When inspecting text for damage, read bytes rather than terminal output:
`git cat-file commit HEAD` and check for `c3 a2 e2 82 ac`. A terminal may display
the mojibake as a plausible-looking dash, which is how it survived twelve commits.

Gate 2A retained snapshots are `core-j4ubtphg` (headless/no default features) and `core-fg6breu7` (desktop/all features). Both complete matrices passed the catalog differential, rustfmt, locked all-target checks, all **220 ordinary tests** with two intentional credential probes ignored, strict Clippy, and optimized release builds. The mixed-format acceptance test in each matrix discovers 100 sources, ingests 1,000 canonical sessions and 2,784 canonical observations, reopens without source files, restores them without duplicates, and recovers a corrupted database from its verified backup. With Gate 3 and the updater, Linux passes **268 ordinary tests** with two intentional credential probes ignored (headless `core-um9bzab7`, desktop `core-e5zy76pt`), Linux GUI acceptance `core-gd7_v77h` passed all 17 checks, and the Windows GNU cross-build passes **236 ordinary tests**, two real Credential Manager lifecycle tests, the acceptance fixture, and a real GitHub update check on a real Windows host.

Linux GUI acceptance is retained in `core-cdrti43e/linux-resource-setup` and the resource matrix in `core-cdrti43e/linux-resource-report.json`. Against the 100-source/1,000-session fixture it measured cold-startup p95 **727 ms**, warm-startup p95 **630 ms**, idle RSS max **45,512 KiB**, idle CPU **0.1%**, visible append/rewrite/burst/removal/recovery latency **503–716 ms**, shutdown **95 ms**, 54 file descriptors, 11 threads, 3 inotify watches, and zero child processes or network listeners. All approved resource thresholds passed.

The remotely built Linux desktop artifact is a stripped x86-64 PIE ELF (25,422,072 bytes, SHA-256 `6e6bd4518a0fb7274cd807e6fb7bbddc0e126d7c7f2ebd6b5ab82eab74d01ab6`). Direct `NEEDED` entries are only DBus, Fontconfig, GCC support, libm, libc, and the ELF loader; there is no direct `libzstd`/`libsqlite3` link and no `ZSTD_*` dynamic symbol. String inspection found no Electron, Node.js, Chromium, webview, `tokscale-core`, scanner-sidecar, reference-checkout, or zstd/sqlite3 subprocess runtime; the visible `Tokscale` string is the required MIT attribution. Dependency-source panic-path strings from the Cargo registry are build metadata, not runtime inputs. This is Linux evidence, not Windows/macOS runtime or packaging proof.

## Licensing and provenance

New code is MIT licensed, Copyright © 2026 Sandeep Khadka. Adapted catalog/parser portions retain the MIT notices for Token Monitor (Copyright © 2026 Javis) and Tokscale (Copyright © 2025 Junho Yeo). Slint is licensed separately and its required attribution is exposed through the application's About view. See [`docs/migration/parser-provenance.md`](docs/migration/parser-provenance.md).
