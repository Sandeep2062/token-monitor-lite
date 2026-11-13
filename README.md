# Token Monitor Lite

An in-progress native Rust + Slint migration of Token Monitor 0.88.0. The current tree contains a verified native persistence, migration, settings, and credential foundation, but it is **not a feature-complete release**.

## Current status

- Exact checked-integer accounting, catalog identities, read-only settings inspection, named-IANA/DST bucketing, canonical aggregation, native file watching, immutable monitor snapshots, and last-good retention are implemented.
- Narrow Claude CLI and Codex CLI JSONL subsets are fixture-verified. The Gate 2A boundary now also covers ordinary Claude/Codex JSONL, streaming Codex `.jsonl.zst`, schema-v1 retained session archives, and Codex local-usage SQLite/WAL. OpenClaw ownership, non-rollout headless documents, unverifiable fork/record ownership, arbitrary alias tables, Claude Desktop IndexedDB, and path-derived provider guesses remain explicitly unsupported.
- A single-writer SQLite store provides schema migration, compare-and-swap settings, transactional imports, verified backups, corruption recovery, and immutable snapshots. Unknown settings fields and original imported session JSON remain byte-exact.
- Startup now discovers validated Claude/Codex candidates, stages recognized legacy artifacts without modifying them, imports the supported settings/archive subset, retains unsupported artifacts inactive, and starts monitoring only from committed source selections.
- Platform secrets use native `keyring` backends only: Windows Credential Manager, macOS Keychain, or Linux Secret Service. There is no production fake or plaintext fallback. Credential activation is journaled as `Prepared → SecretWritten → Verified → MetadataCommitted`.
- The Ledger Rail Home, Sessions, Source Health, Settings, migration report, and About shell compile with Slint 1.18. Home now supports checked Today/Month/All Time totals with retained-archive replacement, a 30-day Trends view, and a period-scoped Models breakdown. Settings Save/Cancel, select-all, stale-edit retention, opaque source selection, redacted migration state, paging, search, refresh, and latest-only UI delivery are headless-tested.
- The monitor worker commits a canonical candidate before publishing it. Blocked writers keep the last committed rows visible with a fixed persistence-delayed state; stale settings-generation commits are never published; unchanged source contents reuse the committed generation, so startup and single-source updates avoid full re-commits.
- About checks the project's GitHub Releases over HTTPS at startup and on demand, selects the platform asset, verifies its SHA-256 digest (GitHub asset digest or release `SHA256SUMS`), and size-bounds the download. Installing requires an explicit click: the Windows Inno installer runs silently, Linux uses `pkexec dpkg -i`, and macOS currently downloads the verified zip for manual replacement. No update is downloaded or executed automatically.

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

## Remote verification

All compilation and tests are intentionally offloaded to `pixvault-vm`:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\remote-core-test.ps1 -Mode desktop-verify
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\remote-core-test.ps1 -Mode verify
```

The first command runs formatting, all-feature check/test/strict Clippy, and a release desktop build. The second preserves the `--no-default-features` headless boundary. Both use isolated, content-hashed source snapshots, at most two Cargo jobs, and a shared bounded target under the remote `evidence/` root.

Gate 2A retained snapshots are `core-j4ubtphg` (headless/no default features) and `core-fg6breu7` (desktop/all features). Both complete matrices passed the catalog differential, rustfmt, locked all-target checks, all **220 ordinary tests** with two intentional credential probes ignored, strict Clippy, and optimized release builds. The mixed-format acceptance test in each matrix discovers 100 sources, ingests 1,000 canonical sessions and 2,784 canonical observations, reopens without source files, restores them without duplicates, and recovers a corrupted database from its verified backup. With Gate 3 and the updater, Linux passes **240 ordinary tests** (final headless `core-1gmm_m6r`, desktop `core-jg0ct4e4`), Linux GUI acceptance `core-gd7_v77h` passed all 17 checks, and the Windows GNU cross-build passes **236 ordinary tests**, two real Credential Manager lifecycle tests, the acceptance fixture, and a real GitHub update check on a real Windows host.

Linux GUI acceptance is retained in `core-cdrti43e/linux-resource-setup` and the resource matrix in `core-cdrti43e/linux-resource-report.json`. Against the 100-source/1,000-session fixture it measured cold-startup p95 **727 ms**, warm-startup p95 **630 ms**, idle RSS max **45,512 KiB**, idle CPU **0.1%**, visible append/rewrite/burst/removal/recovery latency **503–716 ms**, shutdown **95 ms**, 54 file descriptors, 11 threads, 3 inotify watches, and zero child processes or network listeners. All approved resource thresholds passed.

The remotely built Linux desktop artifact is a stripped x86-64 PIE ELF (25,422,072 bytes, SHA-256 `6e6bd4518a0fb7274cd807e6fb7bbddc0e126d7c7f2ebd6b5ab82eab74d01ab6`). Direct `NEEDED` entries are only DBus, Fontconfig, GCC support, libm, libc, and the ELF loader; there is no direct `libzstd`/`libsqlite3` link and no `ZSTD_*` dynamic symbol. String inspection found no Electron, Node.js, Chromium, webview, `tokscale-core`, scanner-sidecar, reference-checkout, or zstd/sqlite3 subprocess runtime; the visible `Tokscale` string is the required MIT attribution. Dependency-source panic-path strings from the Cargo registry are build metadata, not runtime inputs. This is Linux evidence, not Windows/macOS runtime or packaging proof.

## Licensing and provenance

New code is MIT licensed, Copyright © 2026 Sandeep Khadka. Adapted catalog/parser portions retain the MIT notices for Token Monitor (Copyright © 2026 Javis) and Tokscale (Copyright © 2025 Junho Yeo). Slint is licensed separately and its required attribution is exposed through the application's About view. See [`docs/migration/parser-provenance.md`](docs/migration/parser-provenance.md).
