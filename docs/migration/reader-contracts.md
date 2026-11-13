# Gate 2A reader and source contracts

This document freezes the Token Monitor Lite Gate 2A ingestion contract. The evidence pins are Tokscale `d3df620b2cdd9502c70aececdbc6ece61fe26fc9` and Token Monitor `2e55113488888d2e784f159cd92f2d4eee157fbd` (`0.88.0`). Exact paths, hashes, licenses, and fixture provenance are in [parser provenance](parser-provenance.md).

The tables describe the **selected Gate 2A target**, not the narrower implementation that existed when this contract was frozen. Each admitted shape is exactly one of:

- **supported** — emit the normalized observation stated below;
- **ignored non-usage** — retain only the listed parser state, if any, and emit no usage;
- **visible unsupported** — reject the complete source candidate, retain the prior committed generation, and expose only a fixed redacted health reason.

There is no best-effort or partial-success class. Malformed, over-budget, locked, incomplete, changing, canonicalization-failed, or storage-failed candidates never delete, zero, or partially replace last-good rows.

## Common normalized boundary

Request-like observations carry a canonical client and session, model, observation time, checked non-negative token components, parser-declared identity class, and optional non-secret provider/service-tier hints. Cache read is a subset of inclusive input and reasoning is a subset of inclusive output; normalized uncached input and non-reasoning output are obtained only after validating and splitting those inclusive parents. Cost and currency are unavailable in Gate 2A.

Retained session-archive rows are intentionally different: they are authoritative **period aggregates**, not reconstructed requests. They carry period scope, retained capture authority, inclusive-output reasoning, observed component/model coverage, and an explicit unclassified remainder. They must not be forced into a request timestamp or fabricated request identity.

All arithmetic is checked. Invalid signs, non-integral or unsafe values, inconsistent totals/subsets, overflow, conflicting identities, or an unrepresentable retained aggregate are visible unsupported for the whole candidate.

## Identity and authority classes

| Class | Exact key and scope | Canonical rule |
|---|---|---|
| Claude global response | Assistant `message.id`, qualified by `requestId` when present | Merge replay/fork copies by completeness; never by path, timestamp, model, or equal totals alone. Preserve all participating opaque source IDs. |
| Claude transcript-local tool result | Canonical client + canonical session + explicit tool-result ID | Deduplicate repeats only within that transcript/session. Never collapse equal IDs across sessions. Unkeyed explicit results remain source-local. |
| Claude unkeyed | Opaque source ID + source observation ordinal | Always distinct. |
| Codex cumulative/inherited rollout | Declared thread/parent ownership plus provider, model, cumulative counters, and the pinned event/turn discriminator | Suppress replayed inherited history and exact cumulative duplicates. Equal totals or filenames alone are never identity. |
| Codex response usage record | Declared response/thread/turn identity when available | Pending until confirmed or settled by the exact turn/compaction rules. Unverifiable ownership is visible unsupported. |
| Codex unkeyed | Opaque source ID + source observation ordinal | Always distinct. |
| Session archive retained aggregate | Period + normalized client + session ID | Latest complete retained snapshot wins, including explicit zero. It is not additive with a live row of the same period/session. |
| Codex local-usage row | Source-local `(account_key, thread_id, checkpoint_total)` | Account prevents collisions but is not persisted or shown. The canonical session is `codex:<thread_id>`. No continuation/fork identity is inferred. |

Opaque source IDs, never lexical paths, order provenance. Paths exist only in discovery/watch memory and are absent from SQLite, diagnostics, exports, Slint, and value-bearing `Debug` output.

## Claude transcript matrix

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Ordinary `user` row containing genuine human input and no usage | ignored non-usage | Update the bounded request-start timestamp used by the next assistant response. Tool results and system rows do not become human starts. | File session, or declared parent for a sidechain. Missing/invalid optional time is tolerated only where the pinned fallback below applies. |
| Ordinary `assistant` row with supported `usage` counters | supported | One Claude request observation after streaming assembly, cache split, one-hour write clamp, checked arithmetic, and model/provider resolution. | Global response identity. A newly observed response needs its own time or the preceding genuine user start; otherwise use the exact pinned file-mtime fallback only for pinned headless/result forms. |
| Repeated assistant fragments with the same global response identity | supported | Per-field maxima assemble the response. When only one copy exposes cache keys, its `input_tokens` is uncached remainder and supersedes a bare prompt snapshot. Explicit zero proves split reporting; missing input does not. Clamp one-hour writes only after assembly. | Deterministic canonical session; all source provenance retained. Contradictory ownership or invalid counters rejects the candidate. |
| `cache_creation` detail with one-hour and longer-lived writes | supported | Retain the one-hour subset, cap it at total cache-write tokens after completeness merge, and keep the remaining cache write separate. | Same identity as the containing response. |
| Assistant `advisor_message` iterations carrying usage | supported | Emit advisor observations with the pinned model/provider attribution and checked components. | Identity derives from the containing response plus bounded advisor ordinal/identity. A billable iteration with unverifiable attribution is visible unsupported. |
| `user` or `tool_result` row with explicit tool-result token metadata | supported | Emit input-only usage. Explicit tool-result ID is transcript-local; no text/character estimate is allowed for Claude Code transcripts. | Preceding attributable assistant model/provider and canonical session. Model-less usage after a synthetic carrier is visible unsupported. |
| Tool-result text without explicit token metadata | ignored non-usage | No character estimate and no usage row. | Transcript state only. |
| Sidechain rows with `isSidechain`, declared parent `sessionId`, and resolvable agent metadata | supported | Parse ordinary/advisor/tool usage under the parent canonical session while retaining sidechain provenance. | Parent session owns usage. Missing/conflicting parent or unsupported orchestration ownership is visible unsupported. |
| Workflow orchestration journals (`started`/`result` workflow metadata) | ignored non-usage | No usage and no transcript activation. | Never admitted as a usage transcript. |
| Supported headless JSON document or JSONL result/event stream | supported | Emit finalized usage only at the pinned completion boundaries; merge streamed usage state and apply the exact mtime fallback where the format has no event time. | File session unless the payload declares a supported session. An open/incomplete stream is not published. |
| Claude Desktop transcript and `.cc-mirror/<variant>/config/projects` transcript | supported | Same checked observations, with Desktop/variant client ownership and the exact non-secret provider hint. | `cc-mirror/<sanitized-variant>` remains distinct from `claude`; no path-derived provider guess outside the pinned layout. |
| Token-bearing record whose only billable meaning is a cost/currency/price field | visible unsupported | No row; Gate 3 owns pricing. | Prior source generation retained. |
| Known progress/system/queue/file-history record with no supported usage or timing role | ignored non-usage | No row. | No ownership change. |
| Unknown or malformed usage-like record, unsupported provider/model conflict, invalid UTF-8/JSON, oversized record, or cap breach | visible unsupported | Discard the candidate. | Prior committed generation remains active. |

## Codex rollout matrix

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `session_meta` with supported thread/source/originator/provider fields | ignored non-usage | Establish immutable thread, parent/fork/subagent, client and provider ownership. A replayed parent `session_meta` cannot repoint a child after ownership is fixed. | Declared thread. OpenClaw originator changes canonical client as pinned. Contradictions are visible unsupported. |
| `turn_context`, `task_started`, thread-settings record, or genuine user-message boundary | ignored non-usage | Set/reset turn ID, model, request-scoped service tier, duration/timestamp anchor, and fork gate exactly as pinned. Thread tier persists; a new turn restores it unless request metadata overrides it. | Current owned thread/turn. |
| `event_msg/token_count` with `last_token_usage` and cumulative total | supported | Prefer the checked last increment; equal cumulative totals emit zero; small stale regressions are ignored; hard reset rebaselines. Split cached input and reasoning once. | Cumulative/inherited identity under declared thread/turn/provider/model. |
| `token_count` with total only | supported | Emit monotone component differences; a decrease is a hard rebaseline, not a negative correction. | Same cumulative identity rules. |
| `token_count` with last only | supported | Follow the distinct last-only baseline; zero increments do not advance usage. | Same owned turn; no equality-only cross-source deduplication. |
| Model-less `token_count` before a model is known | supported | Buffer within the fixed response/identity budget and apply the next valid model according to pinned boundaries. | If unresolved at complete EOF, visible unsupported rather than silently assigning `unknown`. |
| `token_usage_record` followed by a confirming `token_count` | supported | Keep the record pending and emit only the `token_count` row when it reports that response. | Response/thread/turn identity must be verifiable. |
| Unconfirmed `token_usage_record` settled by the next usage record, next turn, or EOF under the pinned rule | supported | Emit exactly one record-derived observation. | Its declared thread owns it; a record from another thread is not charged to the current rollout. |
| `compacted` with a pending usage record | ignored non-usage | Mark the pending record. The next `token_count` settles it: confirming usage counts from `token_count`; non-advancing/different usage counts the pending record once. | No double count across inclusive component and response rows. |
| Fork, continuation, guardian-review, or subagent history before its first owned turn gate | ignored non-usage | Suppress inherited/replayed history while preserving ownership state. | Parent/child relationships come from payload metadata, never path or filename. |
| First and later owned turns in a fork/subagent rollout | supported | Emit only usage belonging to the child/owned turn, with pinned parent-link and history-ordinal suppression. | Child or selected parent according to the exact source metadata. Unverifiable fork ownership is visible unsupported. |
| Request/thread provider or service-tier change | supported | Attribute each emitted row using request tier over thread tier and explicit provider over supported inference; reset only at pinned boundaries. | Conflicting unresolvable attribution is visible unsupported. |
| Missing event timestamp with a valid pinned turn/session/file anchor | supported | Use the pinned timestamp fallback order; no free-form current-time synthesis. | Same owned observation. No available valid anchor is visible unsupported. |
| Supported headless Codex rollout | supported | Same state machine, with `Codex Headless` agent attribution retained outside secret/runtime content. | Declared session/thread. |
| Non-usage `response_item`/`event_msg` kinds (messages, function calls/results, notices, plan updates) | ignored non-usage | May update only explicitly listed turn/activity gates; prompts, output, cwd, and tool content are never persisted. | No usage row. |
| Supplemental ledger row proven to duplicate native rollout usage | ignored non-usage | Mark native-backed overlap; do not add it. | Exact native thread/response identity required. |
| Cost-only, unknown usage-like, malformed/inconsistent counter, unresolved model, unsupported fork/record, invalid UTF-8/JSON, oversized record, or cap breach | visible unsupported | Discard the candidate. | Prior committed generation remains active. |

## Input and source matrix

| Source/input shape | Status | Exact contract |
|---|---|---|
| Ordinary Claude or Codex `.jsonl` at an admitted discovery role | supported | Bounded complete-record framing. Incomplete tail defers publication. Replacement, truncation, earlier-byte mutation, or ambiguous metadata forces full reconciliation. Metadata before/after a candidate is a change fence, not cryptographic proof. |
| Codex rollout `.jsonl.zst` (outer `.zst` case-insensitive) at live or archived Codex rollout roots | supported | Require standard zstd frame magic `28 B5 2F FD`, optionally preceded/interleaved by standard skippable-frame magic `50..5F 2A 4D 18`. Concatenated standard frames are one logical stream and share one ceiling. Decode directly into the shared framer; no `read_to_end`, temporary plaintext, process, or CLI. Decoded ceiling 1 GiB, zstd window-log ceiling 26, and ordinary record/batch limits still apply. |
| `.zst` outside an admitted Codex rollout role, non-JSONL archive name, gzip/zip, reserved/future/unsupported magic, required dictionary, corrupt/truncated frame or checksum, trailing undecodable data, or decoded/physical cap breach | visible unsupported | No format guessing or decompressor fallback. A stream of only skippable frames is not a rollout. Retain last-good rows. |
| Schema-v1 `session-usage-archive.sqlite` main/WAL/SHM logical source | supported | Exact schema below; read-only revision-fenced snapshot; many sessions per source. Sidecars consume no separate source slot. |
| Exact unversioned Token Monitor `0.88.0` `codex-local-usage.sqlite` main/WAL/SHM logical source | supported | Exact structural schema below; joined request/thread rows only; many sessions per source. |
| Other SQLite database, unsupported schema/version, missing/inconsistent sidecar, busy/locked/denied/replaced/changing database, or source requiring a write/checkpoint to read | visible unsupported | Open read-only with `query_only=ON`; never migrate, checkpoint, create a sidecar, chmod, vacuum, rename, or delete. |
| Recognized non-usage side file (`-shm`, `-wal`, journals, indexes) encountered independently | ignored non-usage | It can trigger coalesced reconciliation of its admitted logical main source but is never parsed or counted independently. |

Native SQLite adapters currently take a fenced private snapshot of the main/WAL/SHM bytes before opening with `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`; this avoids WAL read-mark mutation of the source `-shm` while preserving bounded read-only semantics. The temporary snapshot is private, fenced before and after, and always removed. Discovery admits exact main-file roles from the shared-data root and treats the sidecars as one logical source.

### Session archive SQLite v1

Required objects are:

- `metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL)`;
- `sessions(session_key TEXT PRIMARY KEY, entry_json TEXT NOT NULL, revision INTEGER NOT NULL)`;
- index `sessions_revision_idx(revision, session_key)`;
- metadata `schema-version=1`, `legacy-migrated=1`, and a non-negative revision fence.

Unknown optional metadata is ignored non-usage. `pruned-day` and `pruned-month` are retention frontiers, not usage rows. A row revision newer than the fenced metadata revision, a revision change during the transaction, duplicate/conflicting normalized identities, malformed JSON, unsupported schema, missing required metadata, or any bound failure rejects the complete candidate.

Each accepted entry is normalized independently of row order. Aliases normalize before keying. Today and month aggregates apply only to the exact retained day/month window; all-time has no fabricated event day. The latest whole snapshot per `(period, client, session)` wins, including explicit zero. Reasoning remains included in output, and incomplete component/model coverage becomes explicit unclassified remainder. Live data of the same period/session has authority over the retained aggregate; the two are never summed.

### Codex local-usage SQLite (unversioned `0.88.0`)

There is deliberately no numeric schema version. The required structural contract is:

- `threads(account_key, thread_id, metadata, checkpoint)`, primary key `(account_key, thread_id)`;
- `requests(account_key, thread_id, checkpoint_total, observed_at, turn_id, model, cwd, usage)`, primary key `(account_key, thread_id, checkpoint_total)`;
- index `requests_thread(thread_id, observed_at)`;
- `observer(singleton INTEGER PRIMARY KEY CHECK(singleton = 1), owner TEXT NOT NULL, pid INTEGER NOT NULL, expires_at INTEGER NOT NULL)`;
- `agent_observer_intent(singleton INTEGER PRIMARY KEY CHECK(singleton = 1), pid INTEGER NOT NULL, expires_at INTEGER NOT NULL)`.

Gate 2A reads only deterministic pages of `requests` joined to `threads` on account/thread; the two observer tables are presence-validated but never queried for usage. Extra future columns are tolerated; missing/changed required tables, keys, columns, constraints, or index are visible unsupported. `usage` must contain safe non-negative components with cache-read plus cache-write no greater than inclusive input, reasoning no greater than inclusive output, and total exactly input plus output.

`observed_at` is the request observation time, stored model is retained (`unknown` stays literal), and the source-local identity is `(account_key, thread_id, checkpoint_total)`. The schema contains no parent/fork/continuation, provider, or service-tier fact, so none is invented. `account_key`, metadata, checkpoint, cwd, turn content, title, environment, observer state, and process data are not persisted or projected. A row marked native-backed, or whose thread UUID is already represented by an exact native Codex session, is ignored non-usage rather than added.

## Native safety bounds

These are acceptance limits, not truncation targets:

- ordinary physical source: 256 MiB;
- decoded Codex zstd source: 1 GiB, independently bounded from physical input;
- SQLite private snapshot bytes (main/WAL/SHM): 512 MiB;
- complete JSONL record or archive `entry_json`: 16 MiB;
- one framing step: 256 KiB and at most 1,024 records;
- request-like observations per logical source generation: 16,384;
- session archive rows: 10,000;
- one identity/provider/model/session string: 4,096 bytes;
- retained identity strings per request-like source: 8 MiB.

SQLite is read in deterministic pages no larger than the framing row limit. Release acceptance is exactly 1,000 canonical sessions across 100 mixed logical sources; passing that fixture does not relax any per-source bound. Cap exhaustion is visible unsupported and never publishes a truncated candidate.

## Current implementation delta at freeze

The existing native readers implement bounded ordinary JSONL, bounded streaming Codex `.jsonl.zst`, read-only Codex local-usage normalization/discovery, ordinary Claude assistant assembly, Claude advisor iterations, explicit Claude tool-result input accounting, bounded headless/result streams, sidechain parent attribution with agent provenance, cc-mirror client ownership, anthropic-prefix model normalization, and file-mtime timestamp fallback. Schema-v1 archive retained periods are discovered, fenced, persisted separately in schema v4, and exposed through canonical retained-period rollups without being folded into request rows. Codex now settles pending `token_usage_record` rows against token_count/compaction/turn boundaries, suppresses replayed fork/subagent history until the child's own turn, buffers model-less usage until a later model, attributes subagent/guardian/headless agent provenance, and ignores supplemental local-usage rows for threads already represented by a native rollout. The readers still intentionally reject arbitrary model-alias tables, Claude Desktop IndexedDB, OpenClaw ownership, non-rollout headless documents, and unverifiable fork/record ownership.

## Runtime and persistence boundary (Gate 2A Tasks 9–11)

- One monitor worker parses, canonicalizes, and submits a candidate; only the committed SQLite response activates it and publishes the returned runtime generation. A blocked writer publishes a fixed `PersistenceDelayed` health state while the prior canonical rows stay visible; queue-busy/runtime-conflict responses retry, closed/invalid responses do not spin.
- Settings/day/timezone replacements use a settings snapshot commit so the application never accepts a snapshot under the wrong generation. A commit prepared under a replaced generation is never published.
- Fixed health states are `Pending`, `Healthy`, `Retained`, `ReadFailed`, `DecodingFailed`, `SourceBusy`, `Unsupported`, `CapacityExceeded`, `CanonicalizationFailed`, and `PersistenceDelayed`. Paths, raw errors, SQL, rows, and source identities never cross the UI boundary.
- Restart rehydrates committed source/canonical state before source I/O. Unavailable sources stay retained/stale rather than zero; a later successful read returns to healthy without duplicate observations. Unchanged parsed contents reuse the committed source generation, so ordinary restarts and single-source updates avoid full re-commits.
- Schema v4 persists runtime sources, request observations, SQLite retained periods/models/provenance, settings generations, imports, and credentials. Before migration it creates a verified backup. Missing/corrupt active databases restore the newest verified compatible backup; without one, startup reports recovery required and does not create an empty database.
- Source files are read-only throughout: ordinary/compressed JSONL is streamed (16 MiB record, 256 KiB/1,024-record steps, 256 MiB physical, 1 GiB decoded, 64 MiB zstd window); SQLite main/WAL/SHM is copied into a private `0700` snapshot without opening the original. Archive retained data and request observations remain separate.
- The mixed release fixture carries exactly 100 discovered sources and 1,000 canonical sessions (33 Claude JSONL, 33 Codex JSONL, 32 zstd Codex, one archive SQLite/WAL, one Codex local-usage SQLite/WAL), 2,786 parsed / 2,784 canonical observations, and 120 retained periods. Production ingestion, reopen without sources, restoration without duplicates, mutation/failure scenarios, backup/corruption recovery, and native-file sentinel scans pass remotely.

The pinned differentials cover the complete supported Codex prefix, including `token_usage_record` settlement and compaction, plus transport equivalence between ordinary JSONL and streaming zstd. No fixture result is a full-reader, live-account, platform, pricing, GUI, or packaging claim.
