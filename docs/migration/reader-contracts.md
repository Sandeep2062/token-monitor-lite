# Gate 2A reader and source contracts

This document freezes the Token Monitor Lite Gate 2A ingestion contract. The evidence pins are Tokscale `d3df620b2cdd9502c70aececdbc6ece61fe26fc9` and Token Monitor `2e55113488888d2e784f159cd92f2d4eee157fbd` (`0.88.0`). Exact paths, hashes, licenses, and fixture provenance are in [parser provenance](parser-provenance.md).

The tables describe the **selected Gate 2A target**, not the narrower implementation that existed when this contract was frozen. Each admitted shape is exactly one of:

- **supported** — emit the normalized observation stated below;
- **ignored non-usage** — retain only the listed parser state, if any, and emit no usage;
- **declined record** — the record alone cannot be interpreted. Skip it, count it, and publish every other record in the same generation. The count is exposed as `declined_records` on the source health row, so the totals are visibly short by that many records rather than silently short. A record that parses but carries no measurable counters (Kiro's Auto agent writes zero to mean "not measured") is not declined, but it is counted separately as `unmeasured_records` for the same reason: silence about a known gap is the failure mode this contract exists to prevent;
- **visible unsupported** — reject the complete source candidate, retain the prior committed generation, and expose only a fixed redacted health reason.

The **declined record** class exists because a source is re-read from byte zero on every pass. Rejecting the whole candidate for one uninterpretable record therefore freezes that source's totals permanently: the next pass hits the same record and fails identically, and a log only grows, so the source never recovers on its own. That is the failure the pinned LM Studio reader was explicitly written to avoid, and it is the worst failure mode a usage monitor can have — quiet under-reporting behind a status badge.

The split is by scope, not by severity:

- **declined record** — the failure is confined to one record: a missing or conflicting identity, an unparseable timestamp, a counter that does not reconcile, a schema variant on one row.
- **visible unsupported** — the source as a whole cannot be interpreted: framing, ownership, a structural or schema contract, a capacity or overflow breach.

A reader that declines a record must also reset whatever *pending per-record* state it was building, through `on_declined`. Session-scoped state (a current model, a seen set, an ordinal that only advances on success) is unaffected by a skip and needs no override.

There is no best-effort or partial-success class beyond the counted decline. Malformed, over-budget, locked, incomplete, changing, canonicalization-failed, or storage-failed candidates never delete, zero, or partially replace last-good rows.

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
| OpenCode message | Canonical client + row session ID + row message ID | The message ID is stable across the current and legacy SQLite representations, so the same ID in two channel databases is one observation. Content-fingerprint merging of forked copies with different IDs is not implemented and stays unsupported. |
| Gemini message | Canonical client + in-file session ID + message ID when present | Missing message IDs remain source-local. No cross-file content merge or filename-derived identity is inferred. |
| Cursor usage event | Canonical client + `conversationId` (or account-day fallback) + `usageEventId` | Repeated snapshots of one event share `cursor-event:<id>` and collapse to the largest token total. Id-less rows may join a matching identified event by session/model/timestamp; unmatched rows stay source-local. |
| Cline CLI message | Canonical client + session ID + message ID or assistant ordinal | Stable within one messages file. The ordinal fallback counts emitted assistant usage rows. |
| Amp thread record | Canonical client + thread ID + ledger ordinal or message ID | Ledger rows use their stable per-thread ordinal; message rows use `messageId` or the message index. Identical thread copies merge only on exact identity/content. |
| Kilo message | Canonical client + payload/row session ID + row ID | Stable across repeated reads of one database. Exact duplicate copies merge only on identical identity/content. |
| Goose session | Canonical client + session row ID | One cumulative observation per row. The accumulated counter wins independently over the non-accumulated counter. |
| Copilot OTel record | Canonical client + conversation/session attribute (or trace ID) + response/span ID | A file that mixes record sources keeps only the highest-priority source present. Duplicate identities collapse to the largest token total. |
| CodeBuddy/WorkBuddy message | Canonical client + session ID + provider message/trace/row ID | Missing IDs use a stable source-local line ordinal. Repeated identities keep the largest token total within one source. |
| Pi-format response | Canonical client + `responseId`, or entry ID plus immutable event fields | The identity is deliberately session-independent so copied fork records merge only when their content agrees; conflicts are visible unsupported. |
| Antigravity usage row | Canonical client + session + `responseId`, else source-local ordinal | A repeated response identity is not collapsed across files. |
| Qwen assistant row | Canonical client + session + emitted ordinal | Qwen JSONL carries no per-message id, so the stable ordinal is the identity. |
| Reasonix stat | Canonical client + stats file session + line ordinal + request/total counts | The daily stats file partitions otherwise identical rows. |
| Muse completion | Canonical client + stream session + `sequence` and token components | Replayed events collapse while distinct calls cannot share one. |
| MiniMax Code envelope | Canonical client + session + turn + pending index + token components | Usage waits for the model-bearing result and never gets a guessed model. |

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
| Unknown or malformed usage-like record, unsupported provider/model conflict, invalid UTF-8/JSON, oversized record, or cap breach | declined record, or visible unsupported on a cap/framing breach | An uninterpretable record is skipped and counted; a cap or framing breach still rejects the whole candidate. | Prior committed generation remains active. |

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
| Cost-only, unknown usage-like, malformed/inconsistent counter, unresolved model, unsupported fork/record, invalid UTF-8/JSON, oversized record, or cap breach | declined record, or visible unsupported on a cap/framing breach | An uninterpretable record is skipped and counted; a cap or framing breach still rejects the whole candidate. | Prior committed generation remains active. |

## OpenCode SQLite matrix

OpenCode has no JSONL transcript. Current databases keep metadata in `session_v2` and messages in `session_message`, where the `type` column carries the role. Older/legacy databases use the `session`/`message`/`part` triple. After migration the legacy triple is frozen while `session_message` keeps growing. Only the exact SQLite main-file roles below are admitted; OpenCode's older JSON message directory is not an admitted source.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `session_message` row with `type='assistant'` (or `type='compaction'`) and explicit usable `$.tokens` | supported | One OpenCode observation. `$.model.id` (v2) or top-level `$.modelID` (v1) supplies the model; `$.model.providerID` or top-level `$.providerID` supplies the optional provider hint. `$.tokens.input`, inclusive `output`, `reasoning`, `$.tokens.cache.read`, and `$.tokens.cache.write` are checked non-negative components; reasoning is split out of inclusive output exactly once. `$.time.created` is the millisecond timestamp. | Current row session ID. Row message ID is the stable identity. Contradictory role, missing/partial cache, reasoning greater than output, negative or fractional counters, missing model/time, and malformed JSON reject the complete candidate. |
| `session_message` row with `type='compaction'` and no usable tokens | ignored non-usage | No usage row. | Current transcript state only. |
| Current `user`, `idle`, `system`, settings-switch, and other non-assistant rows | ignored non-usage | No usage row. | No ownership change. |
| Legacy-only session: `message` rows with `$.role='assistant'` and usable tokens, where the session has no `session_message` rows | supported | Same OpenCode observation using top-level model/provider fields and the `time_created` column when `$.time.created` is absent. | Legacy row session ID. |
| Legacy assistant rows for a session that already has current rows | ignored non-usage | The frozen legacy history is not added a second time. A non-current legacy summary or subtask assistant that the v1 migration would have restored is visible unsupported instead. | Current transcript wins. |
| Migrated legacy summary/subtask assistant that would supply omitted compaction or task usage | visible unsupported | No row; the reader does not reconstruct the two histories. | Prior source generation retained. |
| Assistant payload with `tokens` absent or `null` | ignored non-usage | No usage row. A `cost`-only payload is also ignored because Gate 3 owns pricing. | No usage row. |
| Assistant payload where `tokens` exists but `input`, `output`, `cache.read`, or `cache.write` is missing, non-integral, negative, or unsafe | declined record | Skip and count this record; every other record in the source still publishes. The pinned reader clamps negatives and truncates fractional timestamps; the native contract deliberately does not. | Prior rows retained; `declined_records` increments. |
| `$.time.created` missing but a non-negative `time_created` column exists | supported | Use the column value as milliseconds. Fractional millisecond values, negative values, and overflow are visible unsupported. Epoch seconds are not guessed or scaled. | Same row session. |
| Same message ID in two admitted channel databases | supported | Identical rows collapse through the stable message identity. Conflicting content for the same identity is a candidate conflict, not a merge. | Source-qualified provenance is retained. |
| Forked copies with different message IDs but identical content | visible unsupported | No content-fingerprint merge is implemented, so the native reader does not silently choose one copy. | Prior source generation retained. |
| Agent/mode, duration, workspace, title, cost/currency, and provider canonicalization | ignored non-selection | These fields are not persisted or projected. Only the explicit provider ID is retained as a bounded non-secret hint. | No usage mutation. |
| Missing required table columns, a malformed `session_message`/`message` table, invalid UTF-8/JSON, an oversized row, or a cap breach | declined record, or visible unsupported on a cap/framing breach | An uninterpretable record is skipped and counted; a cap or framing breach still rejects the whole candidate. | Prior committed generation remains active. |
| OpenCode legacy JSON message files, `OPENCODE_DB` path overrides, and unknown SQLite shapes | visible unsupported | Not discovered or admitted; no format guessing. | No source slot is created. |

OpenCode's stored components follow the common inclusive-parent rule: `input` excludes cache read/write, and `reasoning` is a subset of inclusive `output`. The native boundary keeps input/cache components as stored and splits reasoning out of output exactly once, so the canonical component sum matches `input + output + cache_read + cache_write` without double-counting reasoning.

## Gemini CLI transcript matrix

Gemini CLI keeps JSON session documents and JSONL recordings under `~/.gemini/tmp`. The native reader supports the session-document and direct-event/stat shapes below. `GEMINI_CLI_HOME` relocation is not implemented; discovery uses the default `<home>/.gemini/tmp` root on Windows, macOS and Linux.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Session document with top-level `sessionId`/`session_id` and `messages[]`, where a message has both `tokens` and `model` | supported | One observation per message. Token aliases (`input|prompt|input_tokens|prompt_tokens|promptTokenCount`, `output|candidates|output_tokens|completion_tokens|candidatesTokenCount`, cached/thoughts/tool/total) are checked non-negative. When `total` proves prompt is cache-inclusive, cached overlap is removed once; tool tokens are added to input. `timestamp` is RFC3339; file mtime is the fallback. | In-file session ID. Message `id` is the stable identity when present; otherwise source-local. |
| Session message without `tokens` or `model` | ignored non-usage | No row. | No ownership change. |
| JSONL `init` line | ignored non-usage | Establish session ID and current model. | Current JSONL source. |
| JSONL direct `gemini`/`tokens` event with a resolved model | supported | Same token normalization as a session message. Event/session `timestamp` or file mtime. | Session from the event or `init`; message `id` when present. |
| JSONL/JSON `stats` with `models` or a direct stats object | supported | One row per model. Prompt-style counters are treated as cache-inclusive and split once; `input` without prompt counters is treated as net. | Session from the payload; stats-derived rows are source-local because they carry no message identity. |
| Negative, fractional, unsafe, or overflowing counters | declined record | Skip and count this record; every other record in the source still publishes. The pinned reader clamps negatives; the native reader does not. | Prior rows retained; `declined_records` increments. |
| Direct event or stats usage with no resolvable model | visible unsupported | No fabricated `unknown` model. | Prior committed generation remains active. |
| Malformed JSON line, non-array `messages`, invalid session/message ID, or unknown usage-like shape | declined record | Skip and count this record; every other record in the source still publishes. The pinned reader skips malformed JSONL lines; the native reader does not. | Prior rows retained; `declined_records` increments. |
| A non-session JSON file under `tmp` that is not `session-*.json` or `tmp/<id>/chats/*.json` | ignored non-usage | Not discovered. | No source slot. |
| Cross-file duplicate or forked Gemini transcripts, `GEMINI_CLI_HOME`, cost/currency, and model-specific pricing | visible unsupported / ignored | No content merge or environment relocation is guessed. Cost is Gate 3. | Prior source generation retained. |

## Cursor usage-cache matrix

Cursor token history is cloud-derived in the reference product: Token Monitor's self-sync writes a tokscale cache, and the provider note forbids watching that generated cache. The native reader therefore consumes an already-present read-only JSON cache and does not implement Cursor cloud sync, desktop state, or CSV export. A cache that does not exist is simply not discovered.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `usage.json` / `usage.<account>.json` with a `usageEventsDisplay` array and event `model`, `timestamp`, `tokenUsage` | supported | One observation per accepted event. `inputTokens`, `outputTokens`, `cacheReadTokens`, `cacheWriteTokens` are checked non-negative; reasoning is zero and cost is unavailable. Provider hint is `cursor`. Numeric-string timestamps are accepted. | `conversationId` when present, else `cursor-<account>-<UTC-day>`. `usageEventId` is the stable event identity. |
| Repeated snapshots sharing `usageEventId` | supported | Collapse within the source to the largest token total (earliest on exact ties). The surviving row retains the event identity. | Same conversation; no double count. |
| Id-less legacy row matching an identified event's session/model/timestamp | supported | Join that event's `cursor-event:<id>` group and participate in the largest-total collapse. | Matched conversation. |
| Id-less row with no matching identified event | supported | Source-local observation with no dedup identity. | Synthetic account-day session. |
| Event without `model` or a usable positive timestamp | ignored non-usage | No row. | No ownership change. |
| `usage.csv`, `archive/` caches, and other files in the cache directory | ignored non-usage | Not discovered. CSV export and archival collapse stay unsupported. | No source slot. |
| Missing/invalid `usageEventsDisplay`, non-object event, negative/fractional counter, invalid timestamp, or oversized file | visible unsupported | Discard the candidate. The pinned reader skips malformed rows and clamps negatives; the native reader does not. | Prior committed generation remains active. |
| Cursor cloud self-sync, desktop title/turn-state, cost/currency, and `TOKSCALE_CONFIG_DIR` relocation | visible unsupported / ignored | No network sync, desktop DB read, pricing, or environment relocation is guessed. | Prior source generation retained. |

## Cline CLI message matrix

The supported source is Cline CLI/Desktop's shared `~/.cline/data/sessions/<session>/<session>.messages.json`. The VS Code extension's `tasks/` logs use the separate Roo/Kilo text-payload format and are not admitted by this reader.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant message with `modelInfo` (or a previously resolved model) and `metrics` | supported | `inputTokens` is gross: cache read + cache write are subtracted once; `outputTokens` is retained; reasoning is zero; cost is unavailable. `modelInfo.id/provider` and file `agent` become model/provider/agent hints. | `sessionId` or the source file-stem fallback. Message `id`, else a stable assistant ordinal. |
| Message with zero input/output and no cost | ignored non-usage | No row. | No ownership change. |
| Non-assistant message, or assistant without `metrics` | ignored non-usage | No row. | No ownership change. |
| Cache read + cache write greater than `inputTokens` | declined record | Skip and count this record; every other record in the source still publishes. The pinned reader saturating-subtracts and can under-count; the native reader refuses that shape. | Prior rows retained; `declined_records` increments. |
| Assistant with metrics but no resolvable model | visible unsupported | No fabricated `unknown` model. | Prior committed generation remains active. |
| Invalid JSON, missing `messages`, non-object message/metrics, negative/fractional counter, or invalid timestamp | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| VS Code `tasks/**/ui_messages.json`, `api_conversation_history.json`, manifest-only model fallback, cost/currency, and the Roo/Kilo text-payload format | visible unsupported / ignored | Not discovered or not parsed; no format guessing. | No source slot. |

## Amp thread matrix

Amp threads live under `<XDG_DATA_HOME or home/.local/share>/amp/threads` and the tokscale sync cache `<TOKSCALE_CONFIG_DIR or home/.config/tokscale>/amp-cache/threads`. The native reader treats the usage ledger as authoritative when it yields token rows and otherwise falls back to assistant per-message usage. It does not merge the two sources.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `usageLedger.events[]` with `model` and token counters | supported | One observation per event with token usage. `input`, `output`, `cacheReadInputTokens`, `cacheCreationInputTokens` are checked non-negative; reasoning is zero and cost is unavailable. Timestamp is RFC3339, else thread `created`, else file mtime. | Thread `id` or file stem. Ledger event ordinal is the stable identity (`amp:<thread>:ledger:<n>`). |
| Assistant message `usage` with `model` and token counters, when the ledger yields no token rows | supported | Same component checks; `inputTokens`/`outputTokens` aliases are used. Timestamp from usage, else thread created + message ID, else file mtime. | Thread `id`. `messageId` or message index is the identity. |
| Ledger event or assistant message with zero tokens | ignored non-usage | No row. | No ownership change. |
| Event/message without a model, non-assistant message, or messages absent | ignored non-usage | No row. | No ownership change. |
| Negative/fractional counters, invalid JSON, non-object ledger/message, or invalid timestamp shape | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| Ledger and message reconciliation, credits/cost, reasoning, provider inference, and overlapping non-identical thread copies | visible unsupported / ignored | No fabricated merge or provider. Cost is Gate 3. | Prior source generation retained. |

## Kilo SQLite matrix

Kilo CLI stores assistant turns in OpenCode's message schema at `<XDG_DATA_HOME or home/.local/share>/kilo/kilo.db`. The supported source is the `message(id, session_id, data)` table. Kilo's VS Code task roots, migration-stub reconciliation, cost, and provider inference are not implemented.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant payload with top-level `modelID`, strict `tokens.cache.read`/`write`, `input`, `output`, optional `reasoning`, and `time.created` | supported | One observation. Reason is split out of inclusive output once; input/cache stay as stored; provider uses explicit `providerID` only; cost is unavailable. Payload `session_id` overrides the row column. | Row ID is the stable identity (`kilo:<session>:<row>`). Missing `time.created` uses the database file mtime. |
| Zero-token assistant row (including legacy migration stubs) | ignored non-usage | No row. | No ownership change. |
| Non-assistant row or row without `tokens` | ignored non-usage | Not selected by the query. | No ownership change. |
| Missing/partial cache, missing model, negative/fractional counter, reasoning greater than output, or invalid ID/timestamp | declined record | Skip and count this record; every other record in the source still publishes. The pinned reader clamps counters; the native reader does not. | Prior rows retained; `declined_records` increments. |
| Kilo IDE task roots, provider inference/fallback, cost/currency, and migration-stub reconstruction | visible unsupported / ignored | No guessed provider or task-log format. | No source slot. |

## Cherry Studio matrix

Cherry Studio keeps two unrelated ledgers. Its Agent / Claude Code mode writes ordinary Claude Code transcripts under the `.claude/projects` roots, which the `claude` client already reads. Its built-in chat surfaces record one row per model invocation in `ai_usage_record`, at `<app-data>/CherryStudio/Data/cherrystudio.sqlite`. **Only the second is read here.** The transcript lane is not a gap: reading it would report the same billed calls twice.

The ledger's `input_tokens` is the whole prompt and `output_tokens` the whole completion, with the cache and reasoning shares counted again in their own columns. `TokenCounts` models five non-overlapping buckets, so the shares are moved out rather than added on top; adding them would count every cache write and reasoning token twice.

The schema is probed with `pragma_table_info` and the statement assembled from what the file actually has. Naming a column a build lacks would fail the whole statement to prepare, which would silently take the lane to zero.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `message_kind = 'chat'` invocation row | supported | One observation from `input - cache_read - cache_write`, `output - reasoning`, and the cache/reasoning columns. | Session is the provider id, else `unknown`. |
| Row with `no_cache_tokens` present | supported, preferred | The ledger's own uncached count is used directly instead of subtracting, because it is exactly the modelled bucket and is the app's own arithmetic. | As above. |
| `message_kind = 'agent-session'` row | ignored | Skipped: the `claude` transcripts already report those calls. | No source slot. |
| `record_kind = 'legacy-aggregate'` row | excluded in SQL | Pre-summed history for the same calls; reading it beside per-invocation rows double counts. | No source slot. |
| Row predating the `message_kind` column | supported | Read. Those rows carry no transcript, so they are exactly what this lane adds. | As above. |
| Cache read plus cache write larger than the prompt | clamped | `input` floors at zero. The pinned reader clamps too; here the prompt is monotonic, so there is no inconsistent record being hidden. | As above. |
| Reasoning larger than completion | clamped | `output` floors at zero. | As above. |
| All-zero row with no positive authoritative USD cost | ignored | The ledger recorded no spend. | No source slot. |
| All-zero row with a positive USD cost | supported | Retained: it is a real charge that happened to record no tokens, and dropping it would hide the spend. | As above. |
| Nonzero `cost` in a currency other than USD, or with no currency | supported, cost unavailable | Tokens are kept and the USD cost is left to pricing. The nullable currency has no USD default, so copying the number as USD would invent a price. | As above. |
| `cost` explicitly zero in any currency | supported, cost reported | Authoritative: the call was billed at zero, so pricing must not re-estimate it. | As above. |
| Model id `<synthetic>` | ignored | Not a billed model. | No source slot. |
| Missing or blank model | supported | Reported as `unknown` rather than dropping the row: the tokens were still spent. | As above. |
| Negative counter | declined record | Skip and count this record; every other record in the source still publishes. The ledger writes unsigned counters, so a negative value means the row is not trustworthy. | Prior rows retained; `declined_records` increments. |
| `total_tokens` only, with no individual bucket columns | visible unsupported | Not guessed. A total cannot supply individual buckets. | No source slot. |
| Conversation title from the optional `message`/`topic` join | supported | Presentation metadata only; never persisted, consistent with the `title` privacy invariant. | As above. |

## LM Studio matrix

LM Studio has no usage ledger. Its OpenAI-compatible local server writes pretty-printed responses beneath `~/.lmstudio/server-logs/`, and the only usage recorded is the `"usage": { … }` object printed for each completed response. Because the log is pretty-printed that object spans several lines, so the source is scanned for balanced objects rather than parsed as line-delimited JSON. Prompt and completion bodies are never deserialized or retained — only the usage object and the identity ahead of it are read.

The counters are inclusive: `prompt_tokens` already contains both cache buckets and `completion_tokens` already contains reasoning, so the shares are subtracted rather than added on top.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| A balanced `"usage": { … }` object, whatever its line breaks | supported | One observation from `total - completion - cache_read - cache_write`, `completion - reasoning`, and the cache/reasoning details. | Session is the log file stem. |
| `prompt_tokens_details` / `output_tokens_details` | supported, preferred | Nested details are read, and the flat `cached_tokens` / `cache_creation_input_tokens` / `reasoning_tokens` spellings too; the larger of each pair wins, because builds have shipped either. | As above. |
| A reported `total_tokens` larger than the bucket sum | supported | Trusted, so a server that counts differently does not have its usage shortened. | As above. |
| Cache larger than the prompt | clamped | `cache_read` is capped at the prompt and `cache_write` at the remainder; `input` floors at zero. | As above. |
| Reasoning larger than completion | clamped | `output` floors at zero. | As above. |
| `total_tokens: 0` | ignored non-usage | No observation. | No source slot. |
| A trailing object that never closes | ignored | Treated as an incomplete write rather than charged. | No source slot. |
| An escaped `"usage"` inside a string value | ignored | Not a marker; a prompt that quotes the key must not start a record. | As above. |
| `"id"` without a `chatcmpl-` / `cmpl-` / `resp_` prefix | supported, content-derived key | The log is full of request and model-file ids, so only a response id may key the record; otherwise the key is derived from the record's own content, which stays stable because a whole revision is re-scanned from the start. | As above. |
| A date inside the model's own answer | ignored | Timestamps come only from the log header shape, never from a bare bracketed date later in the file. | As above. |
| No readable header and no file mtime | declined record | The record is skipped and counted; the rest of the log still publishes. | Prior rows retained; `declined_records` increments. |
| Negative counter, malformed usage object | declined record | The record is skipped and counted; the rest of the log still publishes. | As above. |
| A log larger than the 8 MiB tail window | supported, windowed | The adapter reads the last 8 MiB aligned forward to a newline, matching the pinned reader's streaming window. Because a record's identity always precedes its `usage` object, no eviction carry is needed: the window boundary is a record boundary. The truncation is reported as `truncated_head` and shown on the source row, so the oldest records are visibly out of scope rather than silently missing. | Prior rows retained; the window is re-scanned each revision. |
| `is_turn_start` | not implemented | This codebase has no turn-start field. | No source slot. |
| Host-local resolution of the naive header | supported | A header with no zone designator is the writing host's wall clock and is resolved in the local zone, matching the pinned reader. Reading it as UTC would shift every record by the host's offset and move records across midnight, corrupting the day and month buckets. | As above. |

## Unsloth matrix

Unsloth Studio keeps durable chat and API usage in `studio.db` beneath `$UNSLOTH_STUDIO_HOME` (normally `~/.unsloth/studio`). Two lanes are read, and neither selects message content, prompts, attachments or response previews — only scalar usage and routing fields cross the database boundary.

The counters are inclusive, so cache and reasoning are moved out of prompt/completion rather than added on top.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `chat_messages` assistant row with `contextUsage` | supported | One observation from the inclusive prompt/completion minus the cache and reasoning shares. | Session is the thread id, else `unsloth:chat:<message>`. |
| `responseDetails.responseModelId` | supported, preferred | The responding model wins over `contextUsage.modelId`, then the thread's `model_id`. | As above. |
| `responseDetails.providerType: "local"` | supported | Authoritative zero cost: the model ran on this machine. | As above. |
| A recognized metered route (`anthropic`, `deepseek`, `gemini`, `huggingface`, `kimi`, `mistral`, `openai`, `openrouter`, `qwen`) | supported | Keeps its catalog provider namespace (`gemini`→`google`, `kimi`→`moonshotai`, `mistral`→`mistralai`) with no cost, so pricing applies. | As above. |
| Custom, subscription-backed or unknown route | supported, unpriced | No provider hint at all, so nothing is billed against a guess. The pinned reader's `unpriced:<route>` pseudo-provider is not reproduced; the outcome is the same. | As above. |
| No `providerType` recorded | supported, unpriced | No provider hint and no cost. | As above. |
| `api_usage_events` row | supported | One observation. The table stores only prompt/completion/total, so cache and reasoning are zero because the data does not exist — not because it was measured at zero. | Session is the fixed `unsloth:api`. |
| The `api_usage_events` table is absent | supported | An older Studio build; the chat lane still imports. | No source slot for the API lane. |
| `total_tokens` larger than the bucket sum | supported | Trusted, so a provider that counts differently is not shortened. | As above. |
| Cache larger than the prompt, reasoning larger than completion | clamped | Shares are capped; input and output floor at zero. | As above. |
| Timestamp zero or negative | ignored non-usage | No observation. | No source slot. |
| Negative counter, empty id | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| `is_turn_start` | not implemented | This codebase has no such field. | No source slot. |

## Devin matrix

Devin CLI keeps sessions in `~/.local/share/devin/cli/sessions.db`. Usage lives in `message_nodes.chat_message`, a JSON blob whose `metadata.metrics` carries the counters. The separate `metadata` SQL column is always NULL in real Devin databases and is never read.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant `chat_message` with `metadata.metrics` | supported | One observation from `input_tokens`, `output_tokens`, `cache_read_tokens`, `cache_creation_tokens`. Reasoning is not recorded by Devin and stays zero. | Session is the CLI `session_id`. |
| Assistant row with empty metrics but `metadata.num_tokens` | supported | `num_tokens` is attributed to output, so an older row is still counted rather than dropped. | As above. |
| Assistant row with no attributable usage | ignored non-usage | No observation. Such a row must not become a precedence marker for anything else. | No source slot. |
| `metadata.generation_model` | supported, preferred | Wins over `sessions.model`. | As above. |
| `sessions.model` | supported, fallback | Used only when no generation model exists. | As above. |
| `sessions.model` equal to `adaptive` | ignored | A Devin routing mode, not a model id; the row is skipped rather than reported under a fictitious model. | No source slot. |
| `metadata.request_id` | supported | The dedup identity: Devin can persist one response in several `message_nodes` rows, so copies collapse on the request rather than counting twice. | As above. |
| No `request_id` | supported | Falls back to the row id, which is the only identity older records have. | As above. |
| `created_at` plus `metrics.total_time_ms` | supported | `created_at` is the turn's **end** (the row is written once the message is final), so the start is back-calculated from the elapsed generation time. | As above. |
| No own `created_at` | supported | The database mtime is used and is **not** back-anchored: subtracting a turn duration from a file timestamp would move the record to the wrong day. | As above. |
| `working_directory` | supported | Only the final path component survives, as a label. A raw absolute path is refused in every runtime identity column, so `workspace_key` stays empty. | As above. |
| `message_nodes` or `sessions` absent | supported | An older or partial database is empty history, not a query failure. | No source slot. |
| Malformed JSON, negative counter, empty session id | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| Desktop `acp-events/*.ndjson` | visible unsupported | Not read. Those file names are independent UUIDs, so attributing them needs a title-to-session map built from this database and reconciled across sibling files — impossible for a per-source byte reader without guessing. | No source slot. |
| `duration_ms`, `is_turn_start`, and `inferred_provider_from_model` | not implemented | This codebase's observation has no duration or turn-start field, and an unrecognized model keeps no provider hint rather than being attributed to `devin`. | No source slot. |

## fx matrix

fx aggregates per-request token usage into one snapshot per session at `~/.fx/sessions/<sessionId>/usage-v2.json`, so this reader emits one observation per `(session × model)` entry — the same session-level shape as Kilo, Goose and Mux.

A snapshot carries both a per-model breakdown and top-level session aggregates. When the breakdown is empty, or every entry reports zero tokens, the session totals are attributed to the synthetic `fx-unknown` model rather than dropped, so a session's usage is never silently lost.

`total_cost` is optional at **both** levels and its presence is preserved rather than defaulted. A snapshot that never carried a cost must not be submitted as an authoritative $0.00, which would be indistinguishable from a session fx really did bill nothing for.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Per-model entry with non-zero tokens | supported | One observation from its `input`/`output`/`cache_read`/`cache_write`/`reasoning` counters. | Session is the snapshot's `session_id`, else the containing directory. |
| Per-model entry with zero tokens | ignored non-usage | Skipped, so it cannot mask the aggregate fallback. | No source slot. |
| Empty `models`, or all entries zero | supported | The top-level aggregates become one `fx-unknown` observation, emitted only when it carries tokens, requests or a positive cost. | As above. |
| `<provider>/<model>` id | supported | Split into the model and a provider billing hint. | As above. |
| An id with no separator, or an empty side | supported | Kept whole as the model with no provider hint. | As above. |
| `total_cost` present, finite and non-negative | supported | Reported as authoritative USD. | As above. |
| `total_cost` absent or explicit JSON `null` | supported, unpriced | Reported as unavailable, never as an authoritative zero — the snapshot never carried a cost. | As above. |
| `total_cost` negative or non-finite | supported, unpriced | Treated as unreported rather than clamped to zero, so the observation stays eligible for repricing. | As above. |
| `total_cost` explicitly `0.0` | supported, cost reported | Authoritative: fx really did bill nothing. | As above. |
| Document with no `snapshot` | ignored non-usage | Nothing to attribute. | No source slot. |
| Malformed JSON, negative counter | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| Sibling `session.json`, shared `index.json`, and the global `usage.jsonl` | visible unsupported | Not read. The first two are cross-file reads a per-source byte reader cannot perform, so the timestamp falls back to the usage file's mtime and no workspace or title is reported; the global stream carries no session id or workspace, exactly as the pinned reader declines it. | No source slot. |

## Mistral Vibe matrix

Mistral Vibe (Mistral AI's terminal coding agent) stores session logs under `~/.vibe/logs/session/` (or `$VIBE_HOME/logs/session/`): a `session_YYYYMMDD_HHMMSS_<id>/meta.json` for Vibe 2.x, or a flat `session_*.json` for 1.x whose record sits under a `metadata` key. The tracked client id is `mistral`; the pinned reader calls the format `vibe`.

A Vibe transcript carries no per-message model, timestamp or token counts, so **one observation per session is the finest row the format supports**. That is a property of the file, not a limitation of this reader.

`session_cached_tokens` is a **subset** of `session_prompt_tokens`, not an addition: upstream types the field that way and its own cost function subtracts the cached count before charging the input rate. So the split is `input = prompt - cached`.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `meta.json` `stats` (Vibe 2.x) | supported | One observation: `input = prompt - cached`, `output = completion`, `cache_read = cached`. `cache_write` and `reasoning` are never recorded and stay zero. | Session is the record's `session_id`, else the caller's directory-derived one. |
| Flat `session_*.json` with a top-level `metadata` object (Vibe 1.x) | supported | Same accounting; the record is read from inside `metadata`. | As above. |
| Cached count larger than the prompt | clamped | `input` floors at zero and `cache_read` keeps its value, so the two still reflect what was recorded. | As above. |
| A document with no `stats` | ignored non-usage | Nothing to attribute. | No source slot. |
| All-zero statistics | ignored non-usage | No observation. | No source slot. |
| `config.active_model` set | supported | Used verbatim. | As above. |
| `config.active_model` blank | supported, falls through | Upstream's explicit "unpinned" sentinel meaning "follow the configured default". Accepting it would leave the model blank and price the session at zero. | As above. |
| `config.models` with one entry | supported | Uses the record's `alias`, or its map key when the alias is blank. | As above. |
| `config.models` with several entries and nothing pinned | supported | Deterministic: the published `mistral-medium-latest` default if present, else the first alias in sorted order — never map iteration order. | As above. |
| Nothing pinned and no `config.models` | supported | The published `mistral-medium-latest` default alias. | As above. |
| `end_time` / `start_time` | supported | `end_time` wins, then `start_time`, then the file mtime. | As above. |
| `origin_directory`, or `environment.working_directory` | supported | Only the final path component survives, as a label. A raw absolute path is refused in every runtime identity column, so `workspace_key` stays empty. | As above. |
| `title` | supported | Presentation metadata only; never persisted. | As above. |
| Malformed JSON, negative counter | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| `message_count`, provider inference | not implemented | This codebase's observation has no message-count field, and no provider hint is emitted rather than attributing the session to `mistral`. | No source slot. |

## Prime Agent matrix

Prime Agent stores root sessions in `~/.prime/agent/sessions/*.jsonl` and RLM child sessions below the sibling `session-artifacts` tree. Both use the Pi append-only JSONL record format, so token extraction is shared with the `PiReader` under this client's id — the same delegation Proma uses for its Pi-format sessions.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Pi `session` header | supported | Names the session, so the id comes from the record rather than the path. | Session is the header's `id`. |
| `title` line before the header | tolerated | Skipped; the header may follow. | As above. |
| `message` entry with `message.usage` | supported | One observation with the model and provider the message carries. | As above. |
| `model_change` entry | supported | Sets the model for later model-less usage entries. | As above. |
| `child_usage_attributed` record | **ignored** | Never a message. Its usage belongs to the child's own transcript, which is a separate source; emitting it here would count the child twice. | No source slot. |
| Assistant `message` with no `usage` | visible unsupported | The Pi format requires it, so a malformed record is reported rather than silently skipped. | Prior committed generation remains active. |
| RLM child transcript under `session-artifacts/` | supported | Each child file is its own source, scanned directly. | As above. |
| Fork aggregate reversal on the parent | not implemented | Needs the child transcript the parent's aggregate covers — a cross-file read this per-source contract cannot perform, so the parent copy is skipped (above) rather than adjusted by a guess. | No source slot. |

## Jcode matrix

Jcode writes one compact JSON session snapshot per session at `~/.jcode/sessions/session_*.json`. Authoritative assistant token usage sits on each message under `token_usage`; user and tool messages carry none and are skipped.

Cache accounting is decided from the **message-local** usage shape, never from the session. Jcode stores `provider_key` and `model` only at session scope, so either may describe a later route after a mid-session switch.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Message with `token_usage` | supported | One observation from its counters. | Session is the snapshot's `id`, else the file stem. |
| Usage carrying `cache_creation_input_tokens` (even `0`) | supported, split shape | The cache buckets are already separate, so `input_tokens` is used verbatim. The field's **presence** is the signal, not its value. | As above. |
| Usage with `cache_read_input_tokens` but no creation field | supported, subset shape | Cache reads are a subset of `input_tokens`, so the overlap is removed: `input = input - min(cache_read, input)`. Charging both would count the cached reads twice. | As above. |
| `cache_read > input` with no creation field | supported, split shape | Impossible under subset accounting, so it proves the split shape without needing the creation field. | As above. |
| Message with no `token_usage` | ignored non-usage | User and tool messages carry none. | No source slot. |
| All-zero usage | ignored non-usage | No observation. | No source slot. |
| Message `timestamp` plus `tool_duration_ms` | supported | The timestamp is the turn's **end**, so the start is back-calculated from the elapsed duration. | As above. |
| No own timestamp | supported | The session/file fallback is used and is **not** back-anchored: subtracting a turn duration from it would move the message into the wrong day. | As above. |
| `working_dir` | supported | Only the final path component survives, as a label; a raw absolute path is refused in every runtime identity column. | As above. |
| Blank session `id`, `model`, or `provider_key` | supported | Falls back to the file stem, `unknown`, and `jcode` respectively. | As above. |
| Message with no `id` | supported | A `snapshot:<ordinal>` fallback keeps a custom or malformed file parseable without colliding. | As above. |
| Malformed JSON, negative counter | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| Sibling journal (`*.journal.jsonl`) | visible unsupported | Not read. Merging it means appending the unsaved suffix, replacing a snapshot entry in place when the journal repeats its id, and re-scoping the billing provider per append batch — a cross-file merge this per-source contract cannot perform. The snapshot alone is self-contained. | No source slot. |
| `is_turn_start`, `message_count`, provider canonicalization | not implemented | No such fields on this codebase's observation; `provider_key` is reported verbatim as the billing hint. | No source slot. |

## Kimchi matrix

Kimchi Coding writes the Pi append-only JSONL record format under its own agent directory (`~/.kimchi/agent/sessions/`, or `$KIMCHI_CODING_AGENT_DIR`). Because the format is Pi's, the extraction is shared: this client is `PiReader` under the `kimchi` id, the same relationship `omp` and `senpi` have.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Pi `session` header | supported | Names the session, so the id comes from the record rather than the path. | Session is the header's `id`. |
| `title` line before the header | tolerated | Skipped; the header may follow. | As above. |
| `message` entry with `message.usage` | supported | One observation carrying the message's model and provider. | As above. |
| `model_change` entry | supported | Sets the model for later model-less usage entries. | As above. |
| Assistant `message` with no `usage` | visible unsupported | The Pi format requires it, so a malformed record is reported rather than skipped. | Prior committed generation remains active. |
| Dedup key prefix | supported | `kimchi:` rather than `pi:`. A shared prefix would let one client's row suppress the other's on a machine running both. | As above. |

## Mux matrix

Mux keeps one aggregate counter document per workspace at `~/.mux/sessions/<workspaceId>/session-usage.json`, split by model under `byModel`. It is **not** a transcript: there is no per-request record and no message text, so one observation per `(workspace × model)` entry is the finest row the format supports.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `byModel.<key>` with any non-zero counter | supported | One observation from the `input`/`cached`/`cacheCreate`/`output`/`reasoning` buckets. | Session is the workspace id, i.e. the containing directory. |
| `byModel.<key>` with every counter zero | ignored non-usage | Not an observation. | No source slot. |
| `provider:model` key | supported | Split at the **first** colon; the prefix becomes the billing hint. A model whose own name contains colons keeps them. | As above. |
| Key with no prefix | supported | The whole key is the model and **no** provider hint is emitted — a blank provider string would be a fabricated pricing key. | As above. |
| Bucket `cost_usd` values | supported | Summed across all five buckets and carried as a source-reported cost. A document with no cost anywhere reports nothing, which stays distinct from a genuine zero. | As above. |
| Negative counter | clamped | Floors at zero without discarding the entry: this is an aggregate document, so one contradictory bucket must not throw away a workspace's other usage. | As above. |
| `lastRequest.timestamp` | supported | Used as the observation timestamp; otherwise the file mtime. | As above. |
| Absent `byModel`, or an empty one | ignored non-usage | Nothing to attribute. | No source slot. |
| Malformed JSON | visible unsupported | Discard the candidate. | Prior committed generation remains active. |
| Dedup key | supported | `mux:<workspaceId>:<modelKey>`, deliberately workspace-scoped: a positional key made the first model in every workspace collide, and the map's iteration order was not stable across re-parses. | As above. |

## Junie matrix

Junie stores local sessions under `~/.junie/sessions/<session-id>/events.jsonl`. Each `LlmResponseMetadataEvent` carries per-model usage rows under `event.agentEvent.modelUsage`, and each row is one observation.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `LlmResponseMetadataEvent` with `modelUsage` rows | supported | One observation per row from its counters. | Session is the containing directory, since the file is always `events.jsonl`. |
| `UserPromptEvent` | ignored non-usage | Marks a turn; carries no counters. No turn structure is emitted because this observation type has no turn field. | No source slot. |
| State-snapshot or other kind | ignored non-usage | Skipped. The substring pre-filter only avoids parsing large snapshots; the authoritative skip is on the parsed `kind`, so a usage line that merely *mentions* a skipped kind is not dropped. | No source slot. |
| Row with no `model` | ignored non-usage | Skipped; there is nothing to attribute it to. | No source slot. |
| Counter spellings | supported | First present wins per bucket: `inputTokens`/`input`, `outputTokens`/`output`, three cache-read spellings, three cache-write spellings, three reasoning spellings. | As above. |
| Recorded `timestampMs` plus `usage.time` | supported | `timestampMs` is the call's **end** and `time` its latency in ms, so the start is back-calculated. | As above. |
| No recorded `timestampMs` | supported | The file mtime is used and is **not** back-anchored: subtracting a latency from it would move the message into the wrong day. | As above. |
| `cost` positive | supported, cost reported | Carried as source-reported USD. | As above. |
| `cost` absent or zero | supported, unpriced | Reports nothing — zero here means "none recorded", not "billed nothing". | As above. |
| Row with no tokens and no cost | ignored non-usage | Not an observation. | No source slot. |
| Replayed identical event | supported | Collapses: the dedup suffix is the row's position **within its event**, not a file-global counter, so a replay reproduces the same key while distinct rows in one event stay distinct. | As above. |
| Malformed JSON on a relevant line | visible unsupported | Declined record. | Prior committed generation remains active. |

## Augment Code matrix

Auggie writes one session snapshot per session at `~/.augment/sessions/<sessionId>.json`. Each completed turn exposes an `exchange` whose `responseNodes[]` may carry a `tokenUsage` block, and one observation is emitted per completed turn.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Turn with `completed: true`, an `exchange`, and a non-empty usage node | supported | One observation from its counters. | Session is the snapshot's `sessionId`, else the file stem. |
| Turn with `completed: false`, or with no `completed` flag | ignored non-usage | Snapshots retain aborted and in-progress turns that already carry a partial usage, so an unflagged turn is not counted either. | No source slot. |
| Several usage nodes in one turn | supported | The **last** non-empty one wins; summing repeated cumulative values would double-count. | As above. |
| `cacheReadInputTokens` / `cacheCreationInputTokens` | supported | Reported as independent buckets, never subtracted from `input`: Auggie uses Anthropic-style split accounting. | As above. |
| Turn with no `exchange`, or no non-empty usage node | ignored non-usage | Nothing to attribute. | No source slot. |
| `exchange.modelId` | supported | Wins; a missing or blank value falls back to `agentState.modelId`, then `unknown`. | As above. |
| `finishedAt` | supported | The observation is anchored at the turn's **end** — the schema has no start or duration, so duration-based metrics stay empty. Missing or unparseable falls back to the file mtime. | As above. |
| Cost | never claimed | Reports nothing rather than a zero: the pinned reader emits `cost = 0` so downstream pricing can estimate public list prices from the model id. Augment credits and `subAgentCostUsd` are ignored. | As above. |
| Provider | no hint | Auggie records no vendor, so none is emitted rather than a pseudo-provider `augment` key that no pricing table can match. | As above. |
| Turn identity | supported | The exchange's `requestId`, else the turn's `sequenceId`, else the turn's position — so a re-read collapses while distinct turns stay distinct. | As above. |
| Malformed JSON | visible unsupported | Discard the candidate. | Prior committed generation remains active. |

## gajae-code matrix

gajae-code (`gjc`) writes JSONL sessions at `~/.gjc/agent/sessions/<project-slug>/*.jsonl`, with depth-2 per-pass sub-agent children at `<slug>/<session>/N-*.jsonl`. Each line is tagged by `type`.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `{"type":"session"}` header | supported, emits nothing | Carries `id` and `cwd`; the header itself is not an observation. | Session is the header's `id`. |
| `{"type":"message"}` with `role: "assistant"` | supported | One observation from its `usage` block. | As above. |
| `message` with any other role | ignored non-usage | User and tool turns carry no billable usage. | No source slot. |
| `service_tier_change`, or any other `type` | ignored non-usage | Skipped. | No source slot. |
| `usage.cost.total` finite and non-negative | supported, cost reported | Reused verbatim as an authoritative USD figure. | As above. |
| `usage.cost.total` absent, negative or non-finite | supported, unpriced | Reports nothing so the pricing layer can price from tokens. A negative total is treated as unreported rather than clamped. | As above. |
| `message.timestamp` (unix ms) | supported | Preferred; falls back to the entry's ISO `timestamp`, then the file mtime. | As above. |
| `cwd` | supported | Only the final path component survives, as a label. A raw absolute path is refused in every runtime identity column, so `workspace_key` stays empty. | As above. |
| Message with `id` | supported | Dedup key `gjc:<session>:<message id>`. | As above. |
| Message with no `id` | supported | A deterministic key folding session, timestamp, model, provider, the token breakdown and a hash of the line, so a depth-1/depth-2 replay collapses while distinct messages stay apart. | As above. |
| File with no `session` header | supported | Falls back to the caller's session (the file stem). | As above. |
| Missing `provider` | supported | No provider hint at all rather than an invented `gjc` pseudo-provider that no pricing row can match. | As above. |
| Malformed JSON on a line | visible unsupported | Declined record. | Prior committed generation remains active. |

## DSH matrix

DeepSeek Harness writes one append-only JSONL transcript per session under `<DSH_HOME>/sessions/<encoded-cwd>/<session-id>/session.jsonl` (`<DSH_HOME>` defaults to `~/.dsh`). The `.zstd` suffix marks the physical encoding only: a backend configured with `compression: none` writes the same rows to a plain `session.jsonl`, so the input layer probes the zstd frame magic rather than trusting the name. The session id comes from the containing directory, because the file itself is always `session.jsonl`.

`outputTokens` already includes `reasoningTokens`, so the reasoning bucket is subtracted from output exactly once. DSH never embeds a cost, so pricing is the only cost source.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `assistant/message` with `data.usage` | supported | One observation from `inputTokens`, `outputTokens` minus `reasoningTokens`, `cacheReadTokens`, `cacheWriteTokens` and `reasoningTokens`. | Session is the transcript directory name, else `data.id` from the header. |
| `assistant/message` with no `data.usage` but a `data.stream` carrying a `usage` chunk | supported | The **last** `usage` chunk wins; earlier chunks of the same call are superseded samples, not extra calls. | As above. |
| `assistant/attempt` | supported | Always settles from its stream, never from a top-level usage object. | As above. |
| `compaction/summary` | supported | A real provider call: DSH sends the shadowed range to the model. It shares no `(turn, step)` with a loop step, so it is counted **in addition** to its neighbours and never replaces one. | As above. |
| A later settlement for the same `(turn, step)` | supported, replaced | The later sample replaces the earlier one, because one call that reports twice is one call. Summing both would double-count it. | As above. |
| `served_model` present on `replayState.response.responseModel` | supported, overriding | Wins over the configured `source.model`, because a floating alias or provider substitution resolves to a different model. Only the known `keepalive` placeholder is discarded; the response id alone is not grounds to discard a genuine model. | As above. |
| No `source` on the record | supported | Falls back to the most recent `request/header` routing, then to `unknown`. | As above. |
| Zero usage, or a row with no usable `time` | ignored non-usage | No observation. | No ownership change. |
| Legacy `seedLength` on the header | supported | Events whose `seq` is below the cut are inherited from the parent and dropped; charging them again would bill the parent's calls twice. A present field owns the decision even at zero. | As above. |
| Current `isSeeded: true` header | supported | The cut is the **last** tagged `session/end-seed { inherited: true }` marker, because a nested fork can inherit an ancestor's marker. | As above. |
| `isSeeded: true` with no readable tagged cut | visible unsupported | The whole transcript is refused: inherited usage cannot be distinguished from child-owned usage, so charging the copied prefix to the child is worse than omitting it until a later scan reads the marker. | Prior committed generation remains active. |
| Identical replayed rows within one file | deduplicated | Dropped by the full identity envelope. A repeated `message.id` with different time, usage or routing still separates, because a non-unique id must not fold two real calls. | As above. |
| Malformed JSON, a negative or non-integral counter | declined record | Skip and count this record; every other record in the source still publishes. | Prior rows retained; `declined_records` increments. |
| Per-call generation latency (`duration_ms`), `is_turn_start`, and the compressed-source cache with its fingerprint | visible unsupported | Not guessed. Latency needs the `step/start` anchor and turn-start needs the `(turn, step)` slot map; both are aggregation concerns in this codebase rather than properties of one observation. | No source slot. |

## Qoder CN matrix

Qoder CN is one opt-in, fork-only client with two lanes. The legacy store is a `chat_message` table at `<app-support>/QoderCN/SharedClientCache/cache/db/local.db`, where the app-support root is `~/Library/Application Support` on macOS, the roaming app-data directory on Windows, and `XDG_CONFIG_HOME` (falling back to `<home>/.config`) elsewhere. The current lane is Claude-compatible JSONL under `~/.qoder-cn/projects`.

**Both lanes report a cache-inclusive prompt count.** The same two arithmetic rules therefore apply in each: `input` is `prompt - cached` clamped at zero, and `cache_read` is `min(prompt, cached)`. Without the split the cache read would be counted twice, once inside the prompt and once as its own bucket. `cache_write` and `reasoning` are never persisted by the app and stay zero rather than being inferred.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant database row with `token_info` present, non-empty and not `{}` | supported | One observation from `prompt_tokens` minus `cached_tokens`, plus `completion_tokens`. | Session is `qodercn:db:<database file>:<session_id or request_id or id or rowid>`; identity appends `id`. |
| Assistant transcript line with a `message.usage` object | supported | The same split over `input_tokens`, `cache_read_input_tokens` and `output_tokens`. | Session is `qodercn:jsonl:<file stem>:<sessionId or unknown>`; identity appends `message.id`, else `uuid`. |
| `cached` larger than `prompt` | supported, clamped | `input` saturates at zero and `cache_read` takes the prompt, so the row stays internally consistent instead of going negative. | As above. |
| Row whose prompt and output are both zero | ignored non-usage | No observation: a plan-billed first-party row records credits and a context ratio but no tokens, which is not measured zero usage. An output-only row is kept. | No ownership change. |
| Internal model code such as `qmodel` or `gm51model` | supported, priced | The app's own display name (`Qwen3.7-Plus`, `GLM-5.2`), with the client as provider hint. A `qoder-custom-<profile>/` prefix is stripped first; the remaining model may itself be provider-qualified and is not split further. | As above. |
| Unmapped code, or a routing tier (`auto`, `ultimate`, `performance`, `efficient`, `lite`) | supported, unpriced | The code or the tier name is kept **with no provider hint**, so catalog pricing cannot attach a price to a tier name and no cheaper model is substituted. | As above. |
| Missing model | supported | `qoder-agent`, the pinned reader's documented fallback. | As above. |
| Bare `project_name`, or a `cwd` path | supported | The bare name becomes the workspace label. A path never becomes `workspace_key`: the store rejects absolute-path text in identity columns, so only the final component survives, as a label. | As above. |
| Project name exactly `.` | ignored | The app's "no project" sentinel is not a label. | As above. |
| Negative, non-numeric or missing required counter; non-object `token_info`/`usage`; control character in an identity | declined record | Skip and count this record; every other record in the source still publishes. A fractional counter truncates, matching the pinned `Number(value)` adapter, because the value is a real reading rather than a malformed one. | Prior rows retained; `declined_records` increments. |
| `gmt_create`, or the transcript `timestamp` | supported | Stored as recorded; an unparseable transcript timestamp falls back to the epoch, which falls outside any window. | As above. |
| Missing `chat_session` table or `project_name` column | supported | The project subquery is replaced by `NULL`; the probe decides, rather than inferring from a failed prepare. | As above. |
| Per-source last-complete-snapshot cache with db/WAL fingerprints, the `unpriced:qodercn` marker, cross-format dedup, and the `TOKEN_MONITOR_QODER_CN_*` overrides | visible unsupported / ignored | Not guessed. A failed read is a visible failure that keeps the prior committed generation, which the runtime already guarantees; no second copy is maintained. | No source slot. |

## CatPaw matrix

CatPaw is one opt-in, fork-only client covering two editions — domestic `catpaw-moon` and overseas `catpaw-overseas` — whose account databases sit directly under the platform app-data root (`~/Library/Application Support` on macOS, the native roaming app-data directory on Windows). **Linux reads no CatPaw source**, because no product root has been verified there and a WSL scan would recount a Windows host's usage. The edition is derived from the containing directory name, never inferred from a model id, because the same numeric id names different models per edition.

Discovery admits only immediate `catpaw-memory-<scope>.db` files whose scope is non-empty and limited to ASCII letters, digits, `_` and `-`; the exact scope `anon` is excluded. Non-anonymous historical account databases are kept: that is the machine's retained history, not the current login's usage, and no login state is read to filter it.

Usage is projected from `ui_sdk_messages` joined to `sessions` for the session's model selection. The four buckets are **disjoint and additive**, so nothing is subtracted.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant row whose `payload.extra.contextInfo.usage` carries the four buckets | supported | One observation from `promptTokens` (uncached input), `completionTokens`, `cacheReadTokens` and `cacheWriteTokens`. Reasoning and one-hour cache-write tokens are not persisted by the app and stay zero rather than being inferred. | Session is `catpaw:<database file>:<conversation_id>`; identity is that plus the message id, so identical ids in different account databases stay distinct. |
| Bucket sum equal to `usage.totalTokens` | supported | Accepted as-is. | As above. |
| Bucket sum equal to the sibling `contextInfo.totalUsageTokens` instead | supported | Accepted; older builds stored `totalTokens` without the cache-read part, so either anchor may prove the row. | As above. |
| Current `persistedModel*` selection present | supported | Wins over the legacy `initial*` fields. A valid `isAuto` plus safe-integer `modelId` selects `auto` or a mapped model; a partial or invalid selection is not a selection. | As above. |
| `lite`/`pro`/`max` routing tier, unmapped id, or a conflicting current-versus-selected id | supported, unpriced | Keeps an edition-qualified label such as `catpaw-moon-model-10002`, `catpaw-overseas-model-91` or `catpaw-moon-model-conflict`. No provider hint is attached, so automatic catalog pricing cannot apply, and no lower-priced model is substituted. | As above. |
| Mapped model id | supported, priced | The edition's documented model name, with the client as provider hint. | As above. |
| Bucket sum disagreeing with both anchors, a negative or non-integral bucket, `schema_version` other than 1, or a missing identity | visible unsupported | Discard the whole candidate. | Prior committed generation remains active. |
| `created_at_ms`, then `updated_at_ms` | supported | The first in-range value wins; otherwise the observation is stored at the epoch, which falls outside any window. | As above. |
| Non-assistant row, or a row whose usage is missing or explicitly JSON-null | ignored non-usage | No row. | No ownership change. |
| Whole-snapshot cache with db/WAL fingerprints, the `catpaw.lock` sidecar, cross-scan publication ordering, and the `unpriced:catpaw` marker | visible unsupported / ignored | Not guessed; these are collector concerns, and an unmapped model simply carries no provider hint. | No source slot. |

## Proma matrix

Proma keeps usage in three shapes under `~/.proma` and `~/.proma-dev`: the `agent-sessions/*.jsonl` display journals, and the legacy SDK transcripts under `sdk-config/sessions` and `sdk-config/projects`. This reader walks all six trees and decides a file's shape from its first line: a `type: "session"` record means native Pi format, handled by the Pi reader under the `proma` client; anything else is a legacy SDK transcript.

The pinned reader's cross-file reconciliation is deliberately not implemented. It loads every source, reads the sibling `agent-sessions.json` index, gives native Pi sessions precedence over their display-journal mirrors, and de-duplicates copied fork history by `(uuid, response id)`. That is a whole-root operation over several files plus an index document, which a per-source byte reader cannot perform, so each file is reported independently and no content or token-count fingerprint is invented as an identity.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| SDK `assistant` record with a `message.usage` object | supported | One observation from `input_tokens`, `output_tokens`, `cache_read_input_tokens` and `cache_creation_input_tokens`. Reasoning stays zero because Proma does not persist it separately. | `session_id` (or `sessionId`) wins over the file stem. |
| SDK `result` record | ignored non-usage | No row. It is a session aggregate, not another API request. | No ownership change. |
| Record whose model is `<synthetic>` | ignored non-usage | No row. It marks a locally constructed message that was never billed. | No ownership change. |
| Record with no `model` | supported | Model reported as `unknown`; the counts are still real. | As above. |
| First line of `type: "session"` | supported | The whole file is native Pi format and is delegated to the Pi reader under the `proma` client, including Pi's own model-change and tool-result rules. | Pi's own session ownership applies. |
| All-zero counters | ignored non-usage | No row. | No ownership change. |
| Negative or non-integral counter, or a missing timestamp with no file mtime | declined record | Skip and count this record; every other record in the source still publishes. Counters are never clamped. | Prior rows retained; `declined_records` increments. |
| `_createdAt`, then `timestamp`, then the file mtime | supported | The first usable timestamp wins; the chain matches the pinned reader. | As above. |
| Cross-file reconciliation, the `agent-sessions.json` index, journal-over-native precedence, fork-history dedup, `inferred_provider_from_model`, and the Pi per-call USD cost | visible unsupported / ignored | Not guessed; see the note above. | No source slot. |

## Kiro matrix

Kiro keeps usage in four independently-shaped stores. The supported one is the CLI session document at `~/.kiro/sessions/cli/<session>.json`, whose `session_state.conversation_metadata.user_turn_metadatas` array holds one entry per turn. The remaining three — the SQLite `conversations_v2` table, the IDE `sessions/<workspace>/sess_<uuid>/` layout, and the IDE `globalStorage` snapshots — are not read.

The pinned reader's hybrid estimation model is deliberately not implemented. Kiro's Auto agent commonly persists **zero** counters; where the pinned reader estimates from the sibling `messages.jsonl` (`user_prompt_length` plus order-attributed `ToolResults` bytes for input, `context_usage_percentage * context_window` for cache, `assistant_response_length / 4` for output), the native single-document reader cannot reach that sibling and does not fabricate counts.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Turn with `input_token_count` and/or `output_token_count` greater than zero | supported | One observation. Real counters are authoritative; `cache_read_input_token_count` and `cache_write_input_token_count` are carried when present. Reasoning is zero because Kiro reports none separately. | Document `session_id` owns the turn, falling back to the file stem. Identity is `<session>:<turn index>`. |
| Turn whose `metering_usage` carries entries with `unit == "credit"` | supported | Provider-reported cost: the credit values are summed and multiplied by $0.04 per credit. A turn with no credit entries, or whose credits sum to zero, is unpriced (`Unavailable`) rather than free. | As above. |
| Turn where every counter is zero or absent | ignored non-usage, counted | No row. This is the Auto-agent case the pinned reader estimates; the native reader contributes nothing to the totals, but the turn is real work with no recorded size, so it is counted and surfaced as `unmeasured_records` on the source row. Silence here would be the same under-reporting the declined-record class exists to prevent. | No ownership change. |
| `end_timestamp` absent, with a usable file mtime | supported | The file mtime is the timestamp. | As above. |
| Negative counter, fractional counter, or a negative/non-finite credit value | visible unsupported | Discard the whole candidate. Counters and credits are never clamped. | Prior committed generation remains active. |
| Missing session state, or a turn timestamp with no file mtime | visible unsupported | Discard the candidate. | Prior committed generation remains active. |
| Document with no `user_turn_metadatas` array | ignored non-usage | No rows, and the source is not a usage source. | No ownership change. |
| Session `cwd` | supported, label only | Only the final path component becomes `workspace_label`; `workspace_key` stays unset. The absolute path is never persisted, because the store rejects absolute-path text in every runtime identity column and a home directory in the database would leak more than it explains. | No ownership effect. |
| `duration_ms`, `total_request_count`, `is_turn_start`, credit redistribution to the nearest emitted turn, `agent`/`mode` | visible unsupported / ignored | Not guessed; `is_turn_start` is an aggregation concern in this codebase. | No source slot. |
| SQLite `conversations_v2`, IDE `sessions/<workspace>/`, IDE `globalStorage` | visible unsupported / ignored | Not read; each is a separate store shape. | No source slot. |

## ZCode matrix

ZCode keeps usage in two stores. Transcript JSONL lives at `~/.zcode/projects/<slug>/<session>.jsonl`; the authoritative per-request store is the `model_usage` table at `~/.zcode/cli/db/db.sqlite`. Both are separate sources. The pinned reader's ~4-chars-per-token estimation fallback, `is_turn_start` assignment, per-request duration, and `agent`/`mode` attribution are not implemented.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant transcript line with a `usage` or `token_usage` block | supported | One observation. The first block that yields non-zero tokens wins, so an empty `usage` cannot shadow a populated `token_usage`. Field aliases (`input_tokens`/`prompt_tokens`/`inputTokens`, `completion_tokens`, `cache_read_tokens`, `cache_creation_input_tokens`, `reasoningTokens`, `totalTokens`) are accepted. | Line `sessionId` wins over the caller-supplied session; the file stem is the fallback. |
| `model_usage` row with a non-zero counter sum | supported | One observation, anchored at `started_at` and falling back to `completed_at` then the file mtime. Model is lowercased; identity is `zcode-sqlite:<row>`. | Row `session_id`, defaulting to `unknown`. |
| Reported `computed_total_tokens` equal to `input + output` while differing from the fully additive sum | supported | Proves the row is cache/reasoning-inclusive, so the overlap is subtracted exactly once from input and output. | As above. |
| `computed_total_tokens` present but equal to the additive sum | supported | Already exclusive; counters pass through unchanged. | As above. |
| Legacy schema with no `computed_total_tokens` column | supported | Every row is assumed inclusive and the overlap is subtracted unconditionally, matching the pinned reader's documented sampled behavior. | As above. |
| Modern schema row whose `computed_total_tokens` is NULL | supported | The shape cannot be detected, so counters pass through unchanged. Subtracting here would undercount an already-exclusive row. | As above. |
| Assistant transcript line with no usage block | ignored non-usage | No row. The pinned reader estimates from content here; the native reader does not fabricate a measurement. | No ownership change. |
| All-zero usage block or row | ignored non-usage | No row. | No ownership change. |
| Cache overlap larger than the input it was contained in, or a negative/fractional counter | visible unsupported | Discard the candidate. The pinned reader saturates the overlap; the native reader rejects it as internally inconsistent. | Prior committed generation remains active. |
| Missing model | supported | The documented default `glm-5.2` is used, matching the pinned reader. | As above. |
| Transcript line with no timestamp and no file mtime | visible unsupported | Discard the candidate. | Prior committed generation remains active. |
| `is_turn_start`, `duration_ms`, `agent`/`mode`, and content-based token estimation | visible unsupported / ignored | Not guessed; `is_turn_start` is an aggregation concern in this codebase. | No source slot. |

## MiMo SQLite matrix

MiMo Code and Xiaomi MiMo AI share one engine, and that engine stores assistant turns in OpenCode's message schema at `<XDG_DATA_HOME or home/.local/share>/mimocode/*.db`, most commonly `mimocode.db`. The supported source is the `message(id, session_id, data)` table. Discovery admits every `.db` in that directory, because channel-suffixed databases (`mimocode-<channel>.db`) sit beside the primary store and each is its own source. MiMo's `session.version` desktop split, agent/mode attribution, and duration are not implemented.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant payload with `modelID` (or nested `model.id`), `tokens.input`, `tokens.output`, optional `tokens.reasoning`, and `time.created` | supported | One observation. Reasoning is split out of inclusive output exactly once; input and cache stay as stored. Provider uses explicit `providerID` (or nested `model.providerID`) only; no inference. | Payload `session_id` owns the observation, and the identity namespaces the database file (`mimo:<db>:<session>:<row>`) so channel-suffixed stores with overlapping row ids stay distinct. |
| Assistant payload omitting `tokens.cache`, or omitting `read`/`write` inside it | supported | The missing block or member is zero. This is the deliberate `strict_cache: false` departure from OpenCode, which would reject the row and silently drop its usage. | As above. |
| `time.created` at or below 1e12 | supported | Treated as epoch seconds and scaled to milliseconds (`normalize_epoch_seconds`), because some MiMo builds write seconds where OpenCode writes milliseconds. | As above. |
| Explicit `cost` value, including `0.0` | supported | Reported nano-USD. A reported zero is authoritative and survives repricing, so a free-promo turn is not silently charged. | As above. |
| Negative, non-finite, or above-ceiling `cost`; or negative/fractional counter; or reasoning greater than output | visible unsupported | Discard the whole candidate. Neither counter nor cost is clamped. | Prior committed generation remains active. |
| Missing model, non-assistant role, or control character in an identity string | visible unsupported | Discard the whole candidate. | Prior committed generation remains active. |
| Row without `tokens`, or non-assistant row | ignored non-usage | Not selected by the query. | No ownership change. |
| MiMo `session.version` desktop re-stamp, agent/mode attribution, duration, and provider fallback | visible unsupported / ignored | Not guessed. Every MiMo session is reported as the single catalog client `mimo`. | No source slot. |

## Goose session matrix

Goose stores one cumulative row per session in `sessions.db`. The reader opens the first existing database from the documented ordered candidates, or exactly the `GOOSE_PATH_ROOT` override when it is a raw absolute non-whitespace value. Databases are never unioned.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Session row with valid `model_config_json.model_name` and non-zero counters | supported | One cumulative observation. Each `accumulated_*` counter wins independently when non-null; an accumulated zero is authoritative. Input/output are checked non-negative; cache is zero and cost is unavailable. | Session row `id`; identity `goose:<id>`. `created_at` accepts RFC3339, SQLite datetime, or date-only; database mtime is the fallback. |
| `total_tokens` greater than input + output | visible unsupported | Discard the candidate. Goose has no reasoning column, and the native reader does not infer a remainder as reasoning. | Prior committed generation remains active. |
| Missing/empty/invalid model config, or an all-zero counter row | ignored non-usage | No row. | No ownership change. |
| Negative counter, invalid timestamp/ID, or malformed required column | visible unsupported | Discard the candidate. | Prior source generation retained. |
| Multiple candidate databases, WAL sidecars, inferred reasoning, cost/currency, conversation content | ignored / visible unsupported | Only the first existing database is read; sidecars are one logical source. No reasoning, pricing or content is invented. | No additional source slot. |

## Hermes Agent matrix

Hermes records usage only as per-session aggregates in `state.db`. `session_model_usage` breaks a session down per model and provider; builds that predate that table carry totals on `sessions` alone. The native reader prefers the per-model rows and falls back to session totals only for sessions the child table does not cover, so the two passes are disjoint by session and cannot double count. `reasoning_tokens` is a subset of the inclusive `output_tokens`.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `session_model_usage` row joined to `sessions`, non-empty model, non-zero counters | supported | One observation per (session, model, provider). Output is stored inclusively, so the additive output bucket is `output - reasoning` while `reasoning` is kept separately. Cost is unavailable. | Session `sessions.id`; identity `hermes:<session>:<model>:<provider>`, where a NULL provider renders as `<null>` so SQLite's separate NULL and empty-string groups stay apart. |
| `sessions` row with no covering `session_model_usage` row | supported fallback | One observation credited to `sessions.model`. |
| `reasoning_tokens` greater than `output_tokens` | visible unsupported | Discard the candidate. The native reader does not invent a negative additive output. |
| Missing/empty model, invalid session identity, negative counter, unusable `started_at` | visible unsupported | Discard the candidate. `started_at` is Unix seconds; the database mtime is the fallback timestamp. |
| All-zero counter row | ignored non-usage | No row. |
| `logs/agent.log*` cross-day split, `sessions.title`, actual/estimated cost columns, WAL-active writes | visible unsupported / ignored | The pinned reader re-attributes an aggregate across days using the rotated per-call logs; the native reader deliberately keeps the whole session on its recorded `started_at` instead of trusting a second unvalidated input. Titles are presentation-only and never persisted. |

## OpenClaw matrix

OpenClaw keeps transcripts in two stores that share one event shape (`{"type":"message","id":…,"message":{"role":…,"usage":…}}`): a per-agent SQLite database (`~/.openclaw/agents/<agentId>/agent/openclaw-agent.sqlite`, table `transcript_events`) and, from legacy installs and current `.deleted`/`.reset` archives, one JSONL file per session under `<agentId>/sessions/`. Both feed one interpretation. `reasoningTokens` is a subset of the inclusive `output`.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant event with usage, a model (own, from `model_change`/`model-snapshot`, or carried forward), and non-zero counters | supported | One observation. Additive output is `output - reasoning`. Provider is the event's own, else the carried-forward one, else `unknown`. | JSONL: the file stem. SQLite: the row's `session_id`, and a session change clears the carried model/provider. Identity is `openclaw:<event id>:<timestamp>:<input>:<output>`. |
| Same event with no timestamp of its own | supported | The store's timestamp is used for the observation, but **not** in the identity: `openclaw:<event id>:<input>:<output>`, because the two stores fill it in differently. | This is what lets a migrated SQLite row and its retained JSONL line collapse to one observation. |
| Assistant row OpenClaw authored for transcript bookkeeping (`api: openclaw-transcript`, or provider `openclaw` with model `delivery-mirror`/`gateway-injected`) | ignored non-usage | No row. |
| Assistant row that mirrors a Codex app-server turn (`idempotencyKey: codex-app-server:<thread>:<turn>:assistant`) | supported | One observation, keyed `openclaw:codex-mirror:<thread>:<turn>:…` so a caller can replace it with the rollout's record of that turn. |
| User, tool-result, `session`, and non-`model-snapshot` `custom` events; all-zero usage | ignored non-usage | No row. |
| Missing model with nothing carried forward, negative/non-integer counter, reasoning above output | visible unsupported | Discard the candidate. |
| Malformed `event_json` / non-object event | ignored for that row | The rest of the store is still read; a decode failure of the whole store is visible unsupported. |
| `event_zstd` payload above the 4 MiB declared length, or a decoded length that disagrees with `event_utf8_bytes` | visible unsupported | Refused before allocating. |
| `session_windows` provider/model fallback, the Codex rollout lane and mirror replacement, `/fork` copy collapse, `.zst` transcript archives, WAL-active stores | visible unsupported / ignored | The native reader requires each event to name its own model rather than borrowing a session's current route, and does not read agent-owned Codex rollouts. |

## Droid matrix

Droid writes one `<uuid>.settings.json` per session under `~/.factory/sessions/`, carrying the session's cumulative `tokenUsage`. Its `.jsonl` transcript beside that file records when replies happened but never how many tokens each cost.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `*.settings.json` with a `tokenUsage` block, a model, and non-zero counters | supported | **One** observation carrying the session's cumulative totals: `inputTokens`, `outputTokens`, `cacheReadTokens`, `cacheCreationTokens` and `thinkingTokens` map to input, output, cache read, cache write and reasoning. Cost is unavailable. | Session is the file stem before `.settings`; identity `droid:<session>`. |
| Model spelled Droid-style (`custom:Claude-Opus-4.5-Thinking-[Anthropic]-0`) | supported | Normalized exactly as the pinned reader does: `custom:` dropped, `[...]` removed, trailing hyphens trimmed, lowercased, dots to hyphens, hyphen runs collapsed. `providerLock` names the provider; otherwise the model family does (`claude`/`opus`/`sonnet`/`haiku` → anthropic, `gpt`/`o1`/`o3` → openai, `gemini` → google), else `unknown`. |
| Missing or all-zero `tokenUsage` | ignored non-usage | No row. |
| Negative counter | visible unsupported | Discard the candidate rather than clamp. |
| Missing model, or no usable timestamp | visible unsupported | Discard the candidate. The file mtime anchors the record, floored by `providerLockTimestamp` (usage cannot predate provider selection, and a rewound mtime must not place it earlier). |
| Per-reply apportioning across `.jsonl` replies, `effectiveFactoryRouterModel` for `auto`, JSONL model extraction, cost | visible unsupported | The pinned reader splits the cumulative total across replies by context/output byte weight. The native reader reports the **identical total** as one record — the pinned reader's own fallback when no transcript is readable — and therefore loses only per-reply date resolution, never token magnitude. |

## Kimi matrix

Kimi CLI writes `wire.jsonl` per session under `~/.kimi/sessions/<GROUP>/<SESSION>/`. Only `StatusUpdate` messages carry usage; the wire names no model, so the documented default stands.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `{"message":{"type":"StatusUpdate","payload":{"token_usage":{...}}}}` with non-zero counters | supported | One observation per message: `input_other`, `output`, `input_cache_read`, `input_cache_creation` → input, output, cache read, cache write. Reasoning is always zero because the wire exposes none. Model is `kimi-for-coding`, provider `moonshot`. | Session is the containing `<SESSION>` directory; identity `kimi:<message_id>` when present. |
| camelCase aliases (`inputOther`, `inputCacheRead`, `inputCacheCreation`) | supported | Same mapping; the first present alias decides. |
| Duplicate `message_id` | supported | The larger total wins; a later partial report never lowers a count. |
| `metadata`, `TurnBegin`, `ToolCall`, `ContentPart` and other message types | ignored non-usage | No row. |
| Negative or non-integer counter | visible unsupported | Discard the candidate rather than clamp. |
| Missing or unusable wire timestamp | supported | Falls back to the file mtime; with neither, the candidate is visible unsupported. |
| `kimi-code` `usage.record` shape, the sibling `config.json` model, response duration, cost | visible unsupported | Not read; the default model stands rather than a guessed one. |

## Command Code matrix

Command Code writes one version-3 JSONL transcript per session under `~/.commandcode/projects/<slug>/<session>.jsonl`. Assistant lines carry authoritative per-request usage and cost.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `type: "session"` header | supported | Names the session. A message record's `id` is a per-message UUID and is never used as a session id. | The header wins over the path stem; with no header the stem stands. |
| `type: "message"`, `role: "assistant"`, with a `usage` block naming any bucket | supported | One observation. `inputTokens`, `outputTokens`, `cacheReadTokens` and `cacheWriteTokens` are **disjoint** — `inputTokens` is cache-exclusive and the cache buckets are billed on top — so nothing is subtracted. The buckets reproduce the vendor's own recorded `costUsd` arithmetic exactly; subtracting cache from input does not. | Session from the header; identity `commandcode:<entry-id>:<timestamp>`. |
| `costUsd` present, finite, in `0..=1e6` | supported | Recorded as a provider-reported nano-USD cost. An explicit `0` is a reported cost (free-promo models bill nothing) and is never repriced. | Out-of-range or negative rejects the candidate. |
| `usage` block present with every bucket `0` and no positive cost | ignored non-usage | No row. Command Code seeds its accumulator with zeroes and writes `?? 0` per field, so all-zero is "the provider reported nothing"; adding zero is a no-op in the vendor's accounting too. | A **positive** `costUsd` beside zero buckets is the exception and is kept as a zero-token reported-cost row. |
| `usage` block with **no** bucket field at all | ignored non-usage | No row. The pinned reader estimates ~4 chars/token from message text here; the native reader refuses to fabricate counts. |
| `type: "model_change"` | ignored non-usage | Updates the model in effect; emits nothing. A message's own `model` wins over an earlier change. |
| Message with no model anywhere | supported | Falls back to `<root>/config.json`'s configured agent model, then `unknown`. |
| `type: "message"` with any non-assistant role, including `tool_result` under `role: "user"` | ignored non-usage | No row. |
| Negative, fractional or above-ceiling counter | visible unsupported | Discard the candidate rather than clamp. |
| An assistant entry not on the active branch (`/rewind` abandoned it) | supported | Dropped, matching the vendor's `getBranch()` iteration. |
| A branch chain naming an ancestor missing from the file | supported | **Every** message is kept. Truncating at the hole would silently discard already-billed usage; this deliberately diverges from the vendor's cosmetic `buildSessionPath`. |
| `<session>.checkpoints.jsonl` | ignored non-usage | Not collected as a source; it is a snapshot stream, not a transcript. |
| Missing timestamp | supported | Falls back to the file mtime. |

## Grok matrix

Grok Build writes `updates.jsonl` per session under `~/.grok/sessions/<workspace>/<session-id>/`. Two lanes carry usage; the authoritative breakdown wins where present.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `params.update.sessionUpdate == "turn_completed"` with a `usage` block | supported | One observation per completion. Grok's `inputTokens` includes `cachedReadTokens` and its `outputTokens` includes `reasoningTokens`, so both nested subsets are lifted out to keep the buckets additive. Model is the declared model, else a single `modelUsage` key, else `grok-unknown`. Provider is `xai`. | Session is the containing `<session-id>` directory; identity `grok:<session>:prompt:<promptId>` when a prompt id exists, else the file ordinal plus event id. |
| A retransmitted prompt id | supported | The larger total replaces the earlier snapshot; a smaller retransmission never lowers a count. |
| Legacy cumulative `_meta.totalTokens` | supported | The positive per-turn delta becomes input tokens, since the counter proves the magnitude but no input/output split. The cumulative total is treated as monotonic; a rewound counter is ignored. `user_message_chunk` opens the turn boundary. |
| `totalTokens` present but disagreeing with the inclusive input+output sum | visible unsupported | Discard the candidate rather than trust a breakdown that cannot be reconciled. |
| Negative or non-integer counter | visible unsupported | Discard the candidate rather than clamp. |
| Non-usage `sessionUpdate` kinds (`agent_thought_chunk`, `tool_call`, …) | ignored non-usage | No row. |
| Missing or unusable timestamp | supported | Falls back to the file mtime; with neither, the candidate is visible unsupported. |
| `signals.json` rollup top-up, `~/.grok/logs/unified.jsonl` per-inference lane with subagent attribution, cost ticks | visible unsupported | Not read. The rollup and the unified log are sibling documents the byte-slice reader is not given, so no guess is made. |

## Copilot OTel matrix

The supported source is a flat OTel JSONL file under `<home>/.copilot/otel`. Each line must carry its own `attributes`; cross-line trace enrichment and VS Code session-state stores are not implemented. A file that mixes chat spans, inference logs, agent-turn logs and agent summary spans uses only the highest-priority source present, preventing one turn from being counted twice.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Chat span (`type: span` / span identity + `chat` operation or `chat …` name) with token attributes | supported | One observation. `gen_ai.usage.input_tokens` is cache-inclusive, so cache read is split out once; output/cache write/reasoning stay as reported. Model is `gen_ai.response.model` then `gen_ai.request.model`. | Conversation/session attribute, else `traceId`, else the source file stem. Response ID, else trace+span, else source-local ordinal. |
| Inference log (`event.name=gen_ai.client.inference.operation.details` or `GenAI inference:` body) with token attributes | supported | Same token normalization; used only when no higher-priority chat span exists. | Same session/identity hierarchy. |
| Agent-turn log or agent summary span | supported fallback | Used only when no chat/inference (turn), or no higher source (summary). | Same hierarchy; `gen_ai.agent.id` is retained as an optional agent hint. |
| Token-less record, unrelated OTel record, or a record without `attributes` | ignored non-usage | No row. | No ownership change. |
| Negative/fractional/unsafe counter, missing model, invalid session/identity, or malformed JSON | visible unsupported | Discard the candidate. The pinned reader clamps and enriches across lines; the native reader does neither. | Prior committed generation remains active. |
| Cross-line trace enrichment, VS Code session-state files, configured exporter paths, agent-relationship merging, and cost/currency | visible unsupported / ignored | No network, cross-file or trace-state guessing. Cost is Gate 3. | No additional source slot. |

## CodeBuddy and WorkBuddy transcript matrix

Both clients share one JSONL record shape under their own project roots. CodeBuddy reads `<home>/.codebuddy/projects/**/*.jsonl` plus `CodeBuddyExtension/Logs/**/*.log`; WorkBuddy reads `<home>/.workbuddy/projects/**/*.jsonl` and `<home>/.workbuddy-ai/projects/**/*.jsonl`. When no detailed JSONL source exists, WorkBuddy falls back to `<home>/.workbuddy/workbuddy.db` or `.workbuddy-ai/workbuddy.db`; the database is never added beside a JSONL source.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Completed assistant `message` row with `message.usage` | supported | One observation. Cache read/write and reasoning aliases are read; missing counters are zero. A reported `total_tokens` proves cache-inclusive prompt input only when it equals input+output and differs from the fully additive total, in which case cache read is split out once. | `sessionId`, else the source file stem. Identity is `providerData.messageId`, `traceId`, or row `id`; otherwise a stable source-local line ordinal. |
| `function_call` row with `providerData.usage` or `providerData.rawUsage` | supported | Same normalization. Model is `providerData.model`, `requestModelId`, or `message.model`, else the client default. | Same identity hierarchy. |
| `cachedMissTokens` / `cacheMissTokens` present | supported | The miss counter already excludes cached input and becomes the uncached input. | Same record. |
| Non-completed status, user/turn-metrics/control row, unrelated row, or zero usage | ignored non-usage | No row. | No ownership change. |
| Repeated message identity | supported | The larger token total replaces the earlier row; equal totals keep the first. | Within one source only. |
| Negative/fractional counter, malformed JSON, invalid identity, or absent timestamp and file-mtime fallback | visible unsupported | Discard the candidate. The pinned reader clamps and skips; the native reader does neither. | Prior committed generation remains active. |
| CodeBuddy extension log settlement | supported | `Model prepared:` anchors the model by agent; `AgentReporter � usage:` supplies the usage object. Identity uses agent, settlement second and token components so mirrored sinks collapse. | Agent id is the session; a log with no settlement produces no rows. |
| WorkBuddy aggregate `session_usage` + `sessions` database | supported fallback | One observation per row: `used` is input, `sessions.model` is the model (else `auto`), `updated_at` is normalized seconds/milliseconds. Identity is `workbuddy:<session>:<updated_at>`. Admitted only when no detailed JSONL source exists. | Session id; no workspace, provider or cost projection. |
| WorkBuddy workspace labels, turn-duration performance, provider inference, and cost | visible unsupported / ignored | No workspace projection, timing or pricing. | No additional source slot. |

## Pi-format matrix (Pi, Oh My Pi, OmO Native)

Pi, OMP and Senpi share one JSONL record layout under distinct roots: `<home>/.pi/agent/sessions`, `<home>/.omp/agent/sessions`, and the Senpi roots `<home>/.omo/agent/sessions`, `<home>/.senpi/agent/sessions`, `<home>/.omo/senpi-task/children`, `<home>/.omo/memory` and `<home>/.omo/agent/projects`. `SENPI_CODING_AGENT_DIR` replaces the `.senpi/agent` root and `SENPI_CODING_AGENT_SESSION_DIR` adds one sessions directory.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Leading `title` record(s), then a `session` header with `id` | supported | The header supplies the canonical session; pre-header title records are ignored. | A non-`session` header or a header without a valid ID rejects the candidate. |
| Assistant `message` with `message.usage` and a non-blank `message.model` | supported | One observation. `input`, `output`, `cacheRead`, `cacheWrite` and the `cacheWrite1h` subset are checked non-negative. `reasoning` is read as an informational subset of `output` and never added to the canonical total. | Session header ID. `responseId` gives a cross-session identity; otherwise the entry ID plus immutable event fields keeps copied fork records identical. |
| `model_change` entry | ignored non-usage | Updates the current model/provider used by a following model-less `usage` entry. | No row. |
| Root `usage` entry with usage | supported | Counted as a billed operation. A root model/provider wins; without one, the current selection is used, else `unknown`. | Same identity hierarchy. |
| `compaction` or `branch_summary` entry with root usage | supported | Counted as billed operation work. Without a recorded model it uses the `tools/summaries` bucket rather than charging the conversation model. | Same identity hierarchy. |
| `toolResult` message with usage | supported | Counted as operation work. A recorded model wins; otherwise `tools/summaries`. | Same identity hierarchy. |
| `session_info`, `title_change`, user messages, and other control records | ignored non-usage | No row. `session_info.name` is not treated as an agent identity. | No ownership change. |
| Missing header, missing assistant usage/model, invalid counter, `cacheWrite1h > cacheWrite`, malformed timestamp/JSON, or conflicting duplicate response identity | visible unsupported | Discard the candidate. | Prior committed generation remains active. |
| RLM parent/child accounting, OMP subagent parent links, session titles, workspace attribution, turn-duration timing, reported cost, provider inference, and lossy damaged-record recovery | visible unsupported / ignored | Each transcript is parsed independently; no cross-file allocation, title, workspace or price is invented. | No additional source slot. |

## Catalog JSONL matrix (Antigravity, Qwen, Reasonix, Muse, MiniMax Code)

| Client | Supported source and records | Normalization and identity | Unsupported |
|---|---|---|---|
| Antigravity | `<TOKSCALE_CONFIG_DIR>/antigravity-cache/**/*.jsonl` plus native `.gemini/antigravity*/conversations/*.db` `gen_metadata` blobs | Cache rows use direct counters; native rows decode the pinned `#1`/`#4` protobuf usage fields (`#1+#2` input, `#5` cache read, `#9` output, `#10` reasoning, `#11` response id). Both use `responseId` identity, else source-local ordinal. | Step/trajectory timestamps, workspace/project metadata, pricing-model aliases, cost. |
| Qwen | `<home>/.qwen/projects/<project>/chats/*.jsonl` assistant rows with `usageMetadata` | Prompt is cache-inclusive; cached content splits once. Thoughts split from candidates only when an explicit total proves they are inclusive; otherwise they stay additive. Session ID falls back to the file stem; identity is the session and emitted ordinal. | Workspace attribution, provider inference, cache writes, cost. |
| Reasonix | `<REASONIX_HOME or state home>/stats/YYYY-MM-DD.jsonl` request stats | A non-zero `cache_miss` is the uncached input; otherwise `prompt - cache_hit`. Reasoning is clamped to completion and split once. `provider/model` refs split at the first slash. | Session transcripts, turn markers, cost, workspace. |
| Muse | `<XDG data>/muse/sessions/**/session.jsonl` `runtime.session` `model_completed` events | Cached input and reasoning split once; `stream.id` supplies the session, `sequence` the stable ordinal. | Metadata provider overrides, workflow child aggregates, titles, duration, workspace, cost. |
| MiniMax Code | Captured `mcode exec --output-format stream-json` JSONL under `.minimax`/`.minimax-*`/`.mavis`/`.mavis-*`, `MINIMAX_DATA_DIR`/`MAVIS_DATA_DIR`, and the tokscale headless `mcode` roots | Assistant usage is buffered by turn and emitted only with a model-bearing `exec.result`; `exec.completed.result` supplies its own usage. Envelope identity is `session:turn:index:tokens`. | A capture with no model-bearing final result, local configuration guessing, desktop store inference, cost. |

All five readers reject negative or non-integral counters, malformed required fields, missing explicit timestamps where the pinned reader requires one, and zero-token rows are ignored. File-mtime fallback applies only to Qwen, Muse and MiniMax Code, matching the pinned readers.

## Zed threads matrix

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `threads.db` row with `data_type=json` or `zstd` and hosted `zed.dev` model | supported | One observation per thread. `request_token_usage` sums its map/array entries; otherwise `cumulative_token_usage` is the fallback. Input/output/cache read/cache write are checked non-negative. | Thread `id`; identity `zed:<id>`. `zstd` payloads decode through the bounded in-process frame reader. |
| Imported thread, external ACP provider, zero usage | ignored non-usage | No row. | No ownership change. |
| Missing required schema, unsupported `data_type`, oversized/invalid JSON, missing usage/timestamp, negative counter | visible unsupported | Discard the candidate. | Prior committed generation remains active. |
| Workspace folders, titles, reasoning, message count, cost | ignored | No projection. | No additional source slot. |

## Input and source matrix

| Source/input shape | Status | Exact contract |
|---|---|---|
| Ordinary Claude or Codex `.jsonl` at an admitted discovery role | supported | Bounded complete-record framing. Incomplete tail defers publication. Replacement, truncation, earlier-byte mutation, or ambiguous metadata forces full reconciliation. Metadata before/after a candidate is a change fence, not cryptographic proof. |
| Codex rollout `.jsonl.zst` (outer `.zst` case-insensitive) at live or archived Codex rollout roots | supported | Require standard zstd frame magic `28 B5 2F FD`, optionally preceded/interleaved by standard skippable-frame magic `50..5F 2A 4D 18`. Concatenated standard frames are one logical stream and share one ceiling. Decode directly into the shared framer; no `read_to_end`, temporary plaintext, process, or CLI. Decoded ceiling 1 GiB, zstd window-log ceiling 26, and ordinary record/batch limits still apply. |
| `.zst` outside an admitted Codex rollout role, non-JSONL archive name, gzip/zip, reserved/future/unsupported magic, required dictionary, corrupt/truncated frame or checksum, trailing undecodable data, or decoded/physical cap breach | visible unsupported | No format guessing or decompressor fallback. A stream of only skippable frames is not a rollout. Retain last-good rows. |
| Schema-v1 `session-usage-archive.sqlite` main/WAL/SHM logical source | supported | Exact schema below; read-only revision-fenced snapshot; many sessions per source. Sidecars consume no separate source slot. |
| Exact unversioned Token Monitor `0.88.0` `codex-local-usage.sqlite` main/WAL/SHM logical source | supported | Exact structural schema below; joined request/thread rows only; many sessions per source. |
| OpenCode `opencode.db` or `opencode-<channel>.db` main/WAL/SHM logical source under the XDG data root | supported | Structural `session_v2`/`session_message` (current) or `session`/`message`/`part` (legacy) detection; read-only fenced snapshot; many sessions per source. Exact filename roles below. |
| Gemini session JSON (`session-*.json`, `tmp/<id>/chats/*.json`) and JSONL under `~/.gemini/tmp` | supported | Whole-document framing for JSON and bounded JSONL framing for `.jsonl`; many sessions per file. Exact role filter below. |
| Cursor `usage.json`/`usage.<account>.json` in the tokscale cache | supported | Whole-document framing, non-recursive exact roles; many conversations per file. The cache must already exist. |
| Cline CLI `<session>.messages.json` in the shared data tree | supported | Whole-document framing, recursive `.messages.json` role; many sessions per file. |
| Amp thread JSON in the data tree or sync cache | supported | Whole-document framing, recursive `.json` role across both roots; many threads per source. |
| Kilo `kilo.db` at the XDG data role | supported | Read-only fenced SQLite snapshot, deterministic `message` pages; many sessions per source. |
| Goose `sessions.db` at the first existing or authoritative override | supported | Read-only fenced SQLite snapshot, deterministic `sessions` pages; one cumulative observation per session. |
| Copilot OTel JSONL under `~/.copilot/otel` | supported | Bounded JSONL framing; flat records only, many records per file. |
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

### OpenCode SQLite structural contract

Discovery admits only regular `opencode.db` or `opencode-<channel>.db` files directly under the OpenCode data root: `$XDG_DATA_HOME/opencode` when that variable is a non-empty absolute path, otherwise `<home>/.local/share/opencode` on Windows, macOS, and Linux. The channel suffix uses the `[A-Za-z0-9._-]` character class that OpenCode's channel path sanitizer produces. `-wal`, `-shm`, and `-journal` sidecars are never separate sources. The reference application uses the same XDG root on every host, so no Application Support/AppData fallback is guessed. The `OPENCODE_DB` override is not implemented.

The reader detects shapes structurally, not by a version table:

- **current**: `session_message(id, session_id, data, type, ...)` exists. `time_created` is optional because `$.time.created` normally supplies the timestamp.
- **legacy**: `message(id, session_id, data, ...)` exists. `time_created` is optional for the same reason.
- `part` is optional. When it has `message_id`, `session_id`, and `data`, it is used only to detect a restorable migrated subtask/summary; it is never a usage source.
- `session_v2`/`session` metadata tables are not required and are not persisted.

A table with the expected name but missing the required columns is visible unsupported, not silently skipped. If a session has current rows, only current usage is read; legacy assistant rows for that session are frozen history and are not added again. Sessions with no current rows use legacy assistant rows. A non-current legacy assistant that would need the v1 migration's summary/subtask restoration makes the whole candidate visible unsupported. Invalid legacy JSON rows are ignored as non-usage, mirroring the reference query's `json_valid` filter.

### Gemini input roles

Discovery walks `<home>/.gemini/tmp` recursively. `.jsonl` files are admitted anywhere below that root; `.json` files are admitted only when the filename starts with `session-` or the path is `tmp/<id>/chats/<file>.json`. A single JSON document is one complete record and is capped at 16 MiB; `.jsonl` uses the shared complete-line framer. `GEMINI_CLI_HOME` is not read, so relocated homes remain undiscovered rather than guessed.

### Cursor cache input roles

Discovery reads only `usage.json` and `usage.<account>.json` directly under `<home>/.config/tokscale/cursor-cache`. It is non-recursive, so `archive/` is not re-added. `usage.csv` is not discovered. The generated cache is never watched; a manual/periodic scan reads it read-only. `TOKSCALE_CONFIG_DIR` relocation is not implemented.

### Cline CLI input roles

The Cline root follows the reference precedence exactly, each winning outright: `CLINE_SESSION_DATA_DIR`, else `CLINE_DATA_DIR` + `sessions`, else `CLINE_DIR` + `data/sessions`, else `<home>/.cline/data/sessions`. Blank environment values are unset. Only recursive `.messages.json` files are admitted; VS Code `tasks/` logs are not.

### Amp input roles

Both Amp roots are scanned recursively for `.json` thread files: the XDG data root's `amp/threads` and the tokscale config root's `amp-cache/threads`. `TOKSCALE_CONFIG_DIR` is honored through the environment-path map. The same thread in both roots produces identical stable identities and merges only when the rows are identical.

### Kilo SQLite input role

Discovery admits exactly `<XDG_DATA_HOME or home/.local/share>/kilo/kilo.db`. The `message` table requires `id`, `session_id`, and `data`; the query selects only valid assistant rows with non-null `tokens`. Other SQLite files and IDE task roots are not admitted.

### Goose database roles

A valid `GOOSE_PATH_ROOT` (raw absolute, not whitespace-prefixed) makes only `<root>/data/sessions/sessions.db` eligible, even if absent. Otherwise the scanner opens the first existing candidate: Windows APPDATA or native roaming `Block/goose/data/sessions/sessions.db`, then the XDG data root's `goose/sessions/sessions.db`, then the macOS `Application Support` Goose and Block paths, then `<home>/.local/share/Block/goose/sessions/sessions.db`. An earlier existing but unreadable database shadows later files.

### Copilot input role

Discovery walks `<home>/.copilot/otel` recursively for `.jsonl` files. The configured exporter file is not read beyond that directory, and VS Code session-state stores are not admitted.

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

SQLite is read in deterministic pages no larger than the framing row limit. Release acceptance is exactly 1,000 canonical sessions across 100 mixed logical sources; passing that fixture does not relax any per-source bound. Cap exhaustion is visible unsupported and never publishes a truncated candidate, with one deliberate exception: an append-only log read through a declared tail window reports `truncated_head` and publishes the retained window rather than failing. The window is a declared read scope, not a silent truncation — the source health row says the oldest records are outside it.

## Current implementation delta at freeze

The existing native readers implement bounded ordinary JSONL, bounded streaming Codex `.jsonl.zst`, read-only Codex local-usage normalization/discovery, ordinary Claude assistant assembly, Claude advisor iterations, explicit Claude tool-result input accounting, bounded headless/result streams, sidechain parent attribution with agent provenance, cc-mirror client ownership, anthropic-prefix model normalization, file-mtime timestamp fallback, Gemini session-document/JSONL usage, read-only Cursor usage-cache events, Cline CLI session-message usage, Amp thread ledger/message usage, Kilo SQLite messages, Goose cumulative session rows, and flat Copilot OTel records. The OpenCode SQLite reader implements current `session_message` assistant/compaction usage, legacy-only `message` assistant usage, stable row-message identity, strict cache/model/time/counter validation, and active detection of restorable migrated summary/subtask usage. Schema-v1 archive retained periods are discovered, fenced, persisted separately in schema v12, and exposed through canonical retained-period rollups without being folded into request rows. Codex now settles pending `token_usage_record` rows against token_count/compaction/turn boundaries, suppresses replayed fork/subagent history until the child's own turn, buffers model-less usage until a later model, attributes subagent/guardian/headless agent provenance, and ignores supplemental local-usage rows for threads already represented by a native rollout. The readers still intentionally reject arbitrary model-alias tables, Claude Desktop IndexedDB, OpenClaw ownership, non-rollout headless documents, unverifiable fork/record ownership, OpenCode legacy JSON stores, OpenCode content-fingerprint fork merging, migrated OpenCode summary/subtask restoration, `GEMINI_CLI_HOME` relocation, cross-file Gemini merges, Cursor cloud sync/CSV/desktop state, `TOKSCALE_CONFIG_DIR` relocation for Cursor, Cline VS Code task logs, Amp ledger/message reconciliation, Kilo IDE task roots, Goose inferred reasoning, and Copilot cross-line trace enrichment.

## Runtime and persistence boundary (Gate 2A Tasks 9–11)

- One monitor worker parses, canonicalizes, and submits a candidate; only the committed SQLite response activates it and publishes the returned runtime generation. A blocked writer publishes a fixed `PersistenceDelayed` health state while the prior canonical rows stay visible; queue-busy/runtime-conflict responses retry, closed/invalid responses do not spin.
- Settings/day/timezone replacements use a settings snapshot commit so the application never accepts a snapshot under the wrong generation. A commit prepared under a replaced generation is never published.
- Fixed health states are `Pending`, `Healthy`, `Retained`, `ReadFailed`, `DecodingFailed`, `SourceBusy`, `Unsupported`, `CapacityExceeded`, `CanonicalizationFailed`, and `PersistenceDelayed`. Paths, raw errors, SQL, rows, and source identities never cross the UI boundary.
- Restart rehydrates committed source/canonical state before source I/O. Unavailable sources stay retained/stale rather than zero; a later successful read returns to healthy without duplicate observations. Unchanged parsed contents reuse the committed source generation, so ordinary restarts and single-source updates avoid full re-commits.
- Schema v12 persists runtime sources, request observations, SQLite retained periods/models/provenance, settings generations, imports, and credentials. Before migration it creates a verified backup. Missing/corrupt active databases restore the newest verified compatible backup; without one, startup reports recovery required and does not create an empty database.
- Source files are read-only throughout: ordinary/compressed JSONL is streamed (16 MiB record, 256 KiB/1,024-record steps, 256 MiB physical, 1 GiB decoded, 64 MiB zstd window); SQLite main/WAL/SHM is copied into a private `0700` snapshot without opening the original. Archive retained data and request observations remain separate.
- The mixed release fixture carries exactly 100 discovered sources and 1,000 canonical sessions (33 Claude JSONL including one 19-observation session, 25 Codex JSONL, 32 zstd Codex, one OpenCode SQLite, one Gemini session JSON, one Cursor usage cache, one Cline messages file, one Amp thread, one Kilo SQLite, one Goose session database, one Copilot OTel JSONL, one archive SQLite/WAL, one Codex local-usage SQLite/WAL), 2,786 parsed / 2,784 canonical observations, and 120 retained periods. Production ingestion, reopen without sources, restoration without duplicates, mutation/failure scenarios, backup/corruption recovery, and native-file sentinel scans pass remotely.

The pinned differentials cover the complete supported Codex prefix, including `token_usage_record` settlement and compaction, plus transport equivalence between ordinary JSONL and streaming zstd. No fixture result is a full-reader, live-account, platform, pricing, GUI, or packaging claim.
