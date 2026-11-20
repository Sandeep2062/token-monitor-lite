# Headless agent specification

Scope: a **Rust CLI that shares the desktop core**, with `--once`, `--dry-run`,
and CLI → env → default precedence. Local only.

## 0. Scope decision (2026-11-19)

The following were **dropped from the migration**, confirmed by the project owner:

| Dropped | Reason |
| --- | --- |
| Hub / HTTP sync | 14 protocol boundaries; needs a second device and live network to verify; privacy surface (titles, public-vs-private stats). |
| iCloud | Needs macOS + a real Apple ID to verify at all; unverifiable on this host and on CI. Upstream states iCloud is widget-only and does not accept agents. |
| Plugins | Already **X** in `inventory.md`: Slint has no DOM; a JS/Node host contradicts the chosen runtime. |
| Service status page | Third-party endpoints that rot fastest and cannot be verified offline. |
| Discord RPC | Ambient value; separate IPC surface. |

**Kept:** headless agent (this document) and native notifications.

**Consequence, stated plainly.** Upstream's headless agent exists to post usage to
a hub (`headless-agent.md`: "posts the usage summary to a hub"). With Hub dropped,
this agent is **local-only**: it collects, reports, persists, and exits. There is
no upload, no device identity, and no title sync. `headless-agent.md` therefore
applies only in part — its flags for hub URL, secret, and title sync are
deliberately absent rather than unimplemented.

## 1. Why a separate binary

Verified:

- `cargo build --no-default-features --lib` **succeeds**: the collection core
  already builds without Slint.
- `src/application.rs` contains **no** Slint references.
- The existing binary is desktop-gated — `cargo build --no-default-features --bin
  token-monitor-lite` fails with `requires the features: 'desktop'`, because
  `main.rs` starts with `slint::include_modules!()`.

So the CLI is a **new `[[bin]]**, not a flag on `main.rs`. It links the library
and nothing Slint-shaped.

## 2. Surface

```
token-monitor-lite-agent [OPTIONS]

  --once                 Collect once, persist, print a summary, exit 0.
  --dry-run              Implies --once. Zero writes, zero network.
  --clients <csv>        Override the enabled clients.
  --timezone <iana>      Override the bucket timezone.
  --store <dir>          Override the store root.
  --json                 Emit the summary as JSON instead of text.
  --help, --version
```

Exit codes: `0` success, `2` invalid arguments or configuration, `1` runtime
failure. A partial source failure is **not** an error: it is reported in the
summary and the process exits `0`, because one unreadable tool must not make a
scheduled run look broken.

## 3. Precedence

Exactly as `configuration.md:8` states for the agent:

**CLI flag → env var (real or `.env`) → built-in default.**

| Setting | CLI | Env | Default |
| --- | --- | --- | --- |
| Clients | `--clients` | `TOKEN_MONITOR_CLIENTS` | every client the catalog tracks |
| Timezone | `--timezone` | `TOKEN_MONITOR_TIMEZONE` | `UTC` |
| Store root | `--store` | `TOKEN_MONITOR_LITE_DATA_DIR` | platform data dir |

The store variable is `TOKEN_MONITOR_LITE_DATA_DIR` because that is the name
discovery already reads; inventing a second name would have made the CLI and the
desktop disagree about where data lives. An earlier draft of this spec said
`TOKEN_MONITOR_STORE_DIR`, and the flag silently did nothing until that was
corrected.

An unset enabled-client set means **nothing** to discovery — the desktop fills it
from saved settings. A CLI run therefore resolves `All` against
`catalog::client_ids()` rather than leaving it empty, which is what made the first
draft report zero sources on a machine with 460 of them.

Hub-only variables (`TOKEN_MONITOR_HUB_URL`, `TOKEN_MONITOR_SECRET`,
`TOKEN_MONITOR_DEVICE_ID`, `TOKEN_MONITOR_SYNC_SESSION_TITLES`) are **not read**.
Reading them and ignoring them would be worse than not knowing them: a user who
sets a hub URL deserves either an upload or a refusal, not silence. The parser
warns and exits `2` if one is present, naming the dropped feature.

An unset, empty, and explicitly-empty value are distinguished, matching the
existing settings semantics: empty `--clients ""` disables all clients, while an
absent flag falls through to env, then to all discovered.

## 4. `--dry-run` guarantees

Verified feasible without a second collection path: `RuntimePersistence` is a
**trait** (`src/runtime.rs:146`), so the real monitor pipeline can run against a
discarding implementation. Dry-run therefore exercises the same discovery,
parsing, aggregation, and accounting as a real run — it is not a lookalike.

Guarantees:

1. **No SQLite file is created.** `StoreHandle::open` spawns a writer and creates
   the database (`src/storage.rs:1125`), so dry-run never calls `bootstrap()` or
   `open()`. It supplies a discarding `RuntimePersistence` whose
   `runtime_snapshot()` returns an empty `PersistedRuntimeSnapshot` and whose
   `commit_runtime()` is a no-op.
2. **No settings, export, or credential writes.**
3. **No network.** Pricing fetches and quota probes are disabled, not merely
   skipped — the limits lane is never started. (`ureq` is a dependency for other
   lanes; dry-run simply never reaches it.)
4. **No source-file writes.** Sources are opened read-only.

What dry-run does **not** claim: source reads still happen (that is the point),
and it cannot suppress an unrelated process's network use.

## 5. What a run does

```
parse args → load .env → resolve precedence → discover() → build MonitorSettings
  → start Monitor (real or discarding persistence) → wait for one snapshot
  → print summary → shutdown → exit
```

`discover()` is pure and read-only (`src/discovery.rs:362`), so it is shared with
the desktop unchanged. `MonitorSettings` is built with the same mapping the
application uses (`application.rs:924`).

The summary reports, per source: name, client, status, and — because a silently
empty account is the failure mode that matters — whether the source was read at
all. Totals are the canonical components plus the period rollups, exactly what the
desktop shows.

## 6. Never logged

Consistent with the credentials rule: no token, no raw file path beyond what the
summary needs, and no home directory in output. Source paths appear only as an
opaque identity, as in the accessibility tree.

## 7. Evidence plan

| Claim | How it is proven |
| --- | --- |
| Core builds without Slint | `cargo build --no-default-features --lib` — VERIFIED |
| CLI builds without desktop feature | `cargo build --no-default-features --bin token-monitor-lite-agent` — VERIFIED |
| `--dry-run` creates no files | Ran against an empty directory: **0 files created**, exit 0 — VERIFIED |
| `--dry-run` makes no network call | No network path is reachable; the limits lane and pricing fetch are never started — VERIFIED by construction, not by a counting transport |
| Precedence CLI → env → default | Table-driven tests over all three sources — VERIFIED |
| Empty vs unset clients differ | Explicit test — VERIFIED |
| Hub vars are refused, not ignored | Set the var: exit 2 with a message naming the dropped feature — VERIFIED |
| Dry-run agrees with a real run | Both report 80,107,916 tokens for the same 13 sources — VERIFIED |

### Measured on the development host

| Run | Time | Files written | Tokens | Commits |
| --- | --- | --- | --- | --- |
| `--dry-run --clients codex` | 1.3 s | 0 | 80,107,916 | 13 (would write) |
| `--once --clients codex` | 2.5 s | store created | 80,107,916 | 13 |
| `--dry-run` (all clients) | 21.1 s | 0 | — | 460 (would write) |
| `--once` (all 460 sources) | > 300 s | store grew to 41 MB | — | — |

The dry run and the real run agree on the total **exactly**, which is the property
that makes `--dry-run` worth having. It did not, at first: the discarding store
returned only the latest commit, so each pass rebuilt from one source instead of
thirteen and the dry run reported 25.8M against a real 80.1M. A store keeps one
ledger per source across commits, and the dry-run implementation now accumulates
the same way. A dry run that disagreed with the real run would be worse than no
dry run, because it would be confidently wrong.

### A defect this measured, and how it is handled

A real run over all 460 sources did not finish inside 300 seconds. It was not
hung — the store grew steadily to 41 MB — it was writing one ledger per source.
Two consequences, both recorded rather than papered over:

1. `PASS_TIMEOUT` is 1800 s, not the 120 s the first draft used.
2. **The timeout cannot interrupt a pass.** `Monitor`'s `Drop` joins the worker, so
   elapsed time is only noticed *after* the in-flight reconciliation ends. The
   error message says "did not finish within Ns" rather than implying a hard
   deadline this code does not enforce. Bounding it properly needs a cancellable
   reconciliation, which is a change to the shared runtime and out of scope here.

**ASSUMED**: whether a scheduled run on a real server sees the same source set as
an interactive run (permissions, `$HOME`, container mounts). That needs a real
deployment and is not claimed.
