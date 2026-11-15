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
| Assistant payload where `tokens` exists but `input`, `output`, `cache.read`, or `cache.write` is missing, non-integral, negative, or unsafe | visible unsupported | Discard the candidate. The pinned reader clamps negatives and truncates fractional timestamps; the native contract deliberately does not. | Prior committed generation remains active. |
| `$.time.created` missing but a non-negative `time_created` column exists | supported | Use the column value as milliseconds. Fractional millisecond values, negative values, and overflow are visible unsupported. Epoch seconds are not guessed or scaled. | Same row session. |
| Same message ID in two admitted channel databases | supported | Identical rows collapse through the stable message identity. Conflicting content for the same identity is a candidate conflict, not a merge. | Source-qualified provenance is retained. |
| Forked copies with different message IDs but identical content | visible unsupported | No content-fingerprint merge is implemented, so the native reader does not silently choose one copy. | Prior source generation retained. |
| Agent/mode, duration, workspace, title, cost/currency, and provider canonicalization | ignored non-selection | These fields are not persisted or projected. Only the explicit provider ID is retained as a bounded non-secret hint. | No usage mutation. |
| Missing required table columns, a malformed `session_message`/`message` table, invalid UTF-8/JSON, an oversized row, or a cap breach | visible unsupported | Discard the candidate. | Prior committed generation remains active. |
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
| Negative, fractional, unsafe, or overflowing counters | visible unsupported | Discard the candidate. The pinned reader clamps negatives; the native reader does not. | Prior committed generation remains active. |
| Direct event or stats usage with no resolvable model | visible unsupported | No fabricated `unknown` model. | Prior committed generation remains active. |
| Malformed JSON line, non-array `messages`, invalid session/message ID, or unknown usage-like shape | visible unsupported | Discard the candidate. The pinned reader skips malformed JSONL lines; the native reader does not. | Prior source generation retained. |
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
| Cache read + cache write greater than `inputTokens` | visible unsupported | Discard the candidate. The pinned reader saturating-subtracts and can under-count; the native reader refuses that shape. | Prior committed generation remains active. |
| Assistant with metrics but no resolvable model | visible unsupported | No fabricated `unknown` model. | Prior committed generation remains active. |
| Invalid JSON, missing `messages`, non-object message/metrics, negative/fractional counter, or invalid timestamp | visible unsupported | Discard the candidate. | Prior source generation retained. |
| VS Code `tasks/**/ui_messages.json`, `api_conversation_history.json`, manifest-only model fallback, cost/currency, and the Roo/Kilo text-payload format | visible unsupported / ignored | Not discovered or not parsed; no format guessing. | No source slot. |

## Amp thread matrix

Amp threads live under `<XDG_DATA_HOME or home/.local/share>/amp/threads` and the tokscale sync cache `<TOKSCALE_CONFIG_DIR or home/.config/tokscale>/amp-cache/threads`. The native reader treats the usage ledger as authoritative when it yields token rows and otherwise falls back to assistant per-message usage. It does not merge the two sources.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| `usageLedger.events[]` with `model` and token counters | supported | One observation per event with token usage. `input`, `output`, `cacheReadInputTokens`, `cacheCreationInputTokens` are checked non-negative; reasoning is zero and cost is unavailable. Timestamp is RFC3339, else thread `created`, else file mtime. | Thread `id` or file stem. Ledger event ordinal is the stable identity (`amp:<thread>:ledger:<n>`). |
| Assistant message `usage` with `model` and token counters, when the ledger yields no token rows | supported | Same component checks; `inputTokens`/`outputTokens` aliases are used. Timestamp from usage, else thread created + message ID, else file mtime. | Thread `id`. `messageId` or message index is the identity. |
| Ledger event or assistant message with zero tokens | ignored non-usage | No row. | No ownership change. |
| Event/message without a model, non-assistant message, or messages absent | ignored non-usage | No row. | No ownership change. |
| Negative/fractional counters, invalid JSON, non-object ledger/message, or invalid timestamp shape | visible unsupported | Discard the candidate. | Prior committed generation remains active. |
| Ledger and message reconciliation, credits/cost, reasoning, provider inference, and overlapping non-identical thread copies | visible unsupported / ignored | No fabricated merge or provider. Cost is Gate 3. | Prior source generation retained. |

## Kilo SQLite matrix

Kilo CLI stores assistant turns in OpenCode's message schema at `<XDG_DATA_HOME or home/.local/share>/kilo/kilo.db`. The supported source is the `message(id, session_id, data)` table. Kilo's VS Code task roots, migration-stub reconciliation, cost, and provider inference are not implemented.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Assistant payload with top-level `modelID`, strict `tokens.cache.read`/`write`, `input`, `output`, optional `reasoning`, and `time.created` | supported | One observation. Reason is split out of inclusive output once; input/cache stay as stored; provider uses explicit `providerID` only; cost is unavailable. Payload `session_id` overrides the row column. | Row ID is the stable identity (`kilo:<session>:<row>`). Missing `time.created` uses the database file mtime. |
| Zero-token assistant row (including legacy migration stubs) | ignored non-usage | No row. | No ownership change. |
| Non-assistant row or row without `tokens` | ignored non-usage | Not selected by the query. | No ownership change. |
| Missing/partial cache, missing model, negative/fractional counter, reasoning greater than output, or invalid ID/timestamp | visible unsupported | Discard the candidate. The pinned reader clamps counters; the native reader does not. | Prior committed generation remains active. |
| Kilo IDE task roots, provider inference/fallback, cost/currency, and migration-stub reconstruction | visible unsupported / ignored | No guessed provider or task-log format. | No source slot. |

## Goose session matrix

Goose stores one cumulative row per session in `sessions.db`. The reader opens the first existing database from the documented ordered candidates, or exactly the `GOOSE_PATH_ROOT` override when it is a raw absolute non-whitespace value. Databases are never unioned.

| Input shape | Status | Normalized result / parser state | Session ownership and failure rule |
|---|---|---|---|
| Session row with valid `model_config_json.model_name` and non-zero counters | supported | One cumulative observation. Each `accumulated_*` counter wins independently when non-null; an accumulated zero is authoritative. Input/output are checked non-negative; cache is zero and cost is unavailable. | Session row `id`; identity `goose:<id>`. `created_at` accepts RFC3339, SQLite datetime, or date-only; database mtime is the fallback. |
| `total_tokens` greater than input + output | visible unsupported | Discard the candidate. Goose has no reasoning column, and the native reader does not infer a remainder as reasoning. | Prior committed generation remains active. |
| Missing/empty/invalid model config, or an all-zero counter row | ignored non-usage | No row. | No ownership change. |
| Negative counter, invalid timestamp/ID, or malformed required column | visible unsupported | Discard the candidate. | Prior source generation retained. |
| Multiple candidate databases, WAL sidecars, inferred reasoning, cost/currency, conversation content | ignored / visible unsupported | Only the first existing database is read; sidecars are one logical source. No reasoning, pricing or content is invented. | No additional source slot. |

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

SQLite is read in deterministic pages no larger than the framing row limit. Release acceptance is exactly 1,000 canonical sessions across 100 mixed logical sources; passing that fixture does not relax any per-source bound. Cap exhaustion is visible unsupported and never publishes a truncated candidate.

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
