# Export specification

Scope: the **data export** lane — the four files, their exact serialization, the
trigger rules, and the extended Trends history. Nothing here changes token
accounting (`docs/migration/reader-contracts.md`) or quota limits
(`docs/migration/limits-spec.md`).

Read from the pinned reference:
`token-monitor/docs/export.md` and `token-monitor/src/shared/exporter.js`.

Labels:

- **VERIFIED** — read from the reference, or produced by a command in this
  repository.
- **ASSUMED** — designed but not observed. No OS folder picker has been driven
  on any platform here.

## 1. The four files

`EXPORT_FILENAMES` is the single source of truth, in this order. The writer uses
it to delete orphans, so a file set can never be half-stale. **VERIFIED**.

| File | Written |
| --- | --- |
| `token-monitor-export.json` | always |
| `token-monitor-snapshot.csv` | always |
| `token-monitor-daily.csv` | only when there is ≥ 1 daily row |
| `token-monitor-daily-models.csv` | only when there is ≥ 1 daily-model row |

The two conditional files are the reference's own rule (`exportFileSet`), not an
optimisation of ours: a stale `daily.csv` after history empties is deleted, which
is why the name list must exist independently of what is written. **VERIFIED**.

## 2. CSV serialization

**UTF-8 BOM, CRLF, RFC 4180.** From `toCsv`:

- A leading `\u{FEFF}` BOM, so Excel opens non-ASCII correctly. **VERIFIED**.
- Header row, then one line per row, joined by `\r\n`, with a **trailing** `\r\n`
  after the last line. **VERIFIED**.
- A field is quoted only when it contains `"`, `,`, `\n` or `\r`; an embedded
  `"` doubles. `null`/`undefined` render as empty, not as `"null"`. **VERIFIED**.
- Numeric fields are coerced with `num()`: a non-finite or unparseable value
  becomes `0`, never `NaN`. **VERIFIED**.

Column sets, exactly as declared (**VERIFIED**):

```
snapshot:      period,dimension,name,tokens,cost_usd
daily:         date,tool,tokens,cost_usd
daily-models:  date,model,input_tokens,output_tokens,cache_read_tokens,
               cache_write_tokens,unclassified_tokens,total_tokens,cost_usd
```

Dates are ISO `YYYY-MM-DD` — the day, sliced to 10 characters. Cost columns are
named `cost_usd` and are always USD. **VERIFIED**.

## 3. Row construction

### 3.1 Snapshot

For each period in `today, month, allTime`, in that order: one row per client
with `dimension=tool`, then one row per model with `dimension=model`. Period
order, then tool-before-model, is the reference's iteration. **VERIFIED**.

A period with no usage still emits its header-bearing file but no rows; the
reference skips a period object that is missing. **VERIFIED**.

### 3.2 Daily

One row per day × client, from `history.daily[].perClient`. A day where three
tools were used yields three rows; a day with only `codex` yields one. There is
**no per-device column** — on a hub the counts are combined. **VERIFIED**.

### 3.3 Daily models — the component split

This is the subtle part, and the reference is explicit about why. From
`dailyModelComponents(day, value)`:

```
total        = max(0, tokens)
output       = max(0, outputTokens)
cache_read   = max(0, cacheReadTokens)
cache_write  = max(0, cacheWriteTokens)
known        = output + cache_read + cache_write
```

1. If `known > total`, the row is malformed or mixed-version: **the whole total
   becomes `unclassified_tokens`** and every component is 0. The trusted total is
   kept; no split is invented. **VERIFIED**.
2. Otherwise `remaining = total - known`, and `unclassified` is:
   - the explicit `unclassifiedTokens`, clamped to `[0, remaining]`, when the
     field is present;
   - else `0` when `day.tokenComponentsAvailable === true`;
   - else `remaining` — the unknown remainder is **not** guessed as input.
   **VERIFIED**.
3. `input_tokens = remaining - unclassified`, floored at 0. **VERIFIED**.

`cost_usd` is `value.cost`. **VERIFIED**.

### 3.4 JSON

`renderExportJson` writes `JSON.stringify(payload, null, 2)` plus a trailing
newline — 2-space indent, `\n` line endings. Shape: **VERIFIED**.

```json
{
  "generatedAt": "…ISO…",
  "app": { "name": "token-monitor" },
  "snapshot": { "today": {…}, "month": {…}, "allTime": {…} },
  "daily": [ … ],
  "monthly": [ … ]
}
```

`snapshot.*` is the period object passed through untouched — lossless — with one
exception: session text is stripped (`stripSessionTextFromPeriod`), because
`export.md` promises the export carries "only your usage numbers" and is "safe to
drop into a synced vault", while a local period keeps `session.title` for the
renderer. Classifying fields (session kinds, ids, counters) stay. **VERIFIED**.

## 4. Privacy boundary

`export.md` §Privacy: the export never includes device identifiers, hostnames,
account emails, plan labels, or quota/limit account data. The reference enforces
this by **signature** — the exporter receives `periods`/`history` and devices and
limits are siblings that simply never reach it. **VERIFIED**.

We enforce it twice, because a signature is a convention and a test is not:

1. The exporter's input types carry no device, account, or quota field. Adding
   one is a compile error there.
2. `an_export_contains_no_identity_field` scans every byte of all four files for
   account/device/plan strings and fails if one appears.

## 5. Triggers and the no-rewrite rule

- **Manual export**: pick a folder, write the set once, now.
- **Auto-export**: on a cadence, rewrite only when the *meaningful* payload
  changed.

`exportSignature` is the reference's mechanism: a stable (sorted-key) stringify of
`snapshot` + `daily` + `monthly`, **excluding** `generatedAt` and the app version.
Comparing that against the last-written signature is what stops an idle machine
from re-uploading unchanged files through iCloud or Obsidian Sync. **VERIFIED**.

Two consequences we implement deliberately:

- The signature is computed from the payload, never from the rendered text, so a
  clock tick alone cannot cause a write.
- Because the signature excludes `generatedAt`, an auto-export that is skipped
  leaves the previous `generatedAt` in the file. That is the reference's
  behaviour and is what "no rewrite" means — we do not sneak a timestamp update
  past the rule.

## 6. Extended Trends

`export.md` caps daily history at "roughly the last 370 days"; `analytics.rs`
already has `MAX_TREND_DAYS = 370`. What is missing is the UI, and one
correctness fix.

### 6.1 Missing days are unknown, not zero

`daily_series` currently fills every absent day with `TokenCounts::default()`,
which renders as a real `0`. That is wrong in the one case that matters: a day
before this install started tracking is **unknown**, and drawing it as zero
claims you used nothing. The inventory already states the rule — "missing is
unknown, not zero".

So a day is one of three things:

| State | Meaning |
| --- | --- |
| **Observed** | the source saw usage that day (possibly zero) |
| **Empty** | inside the tracked span, and the source had the day available |
| **Unknown** | no evidence either way — outside/at the edge of tracked history |

Unknown days render as a distinct cell and are **excluded from averages**, never
counted as 0.

### 6.2 Monthly rollups

A month is the sum of its observed days. A month containing any unknown day is
marked **partial**, so a bar is never drawn as if the month were complete.

### 6.3 Heatmap

A 53-week grid (7 rows × 53 columns), one cell per day, intensity from the
day's observed total. An unknown day gets a distinct non-zero colour — never the
empty colour, which would read as "zero usage".

## 7. What we implement

| Module | Contents |
| --- | --- |
| `src/export/csv.rs` | BOM, CRLF, RFC 4180 escaping, `num()` coercion |
| `src/export/rows.rs` | Snapshot/daily/daily-model row construction + components |
| `src/export/json.rs` | The JSON document, 2-space, trailing newline |
| `src/export/signature.rs` | Stable stringify, signature over the meaningful payload |
| `src/export/mod.rs` | `FileSet`, filenames, conditional files, orphan sweep |
| `src/export/writer.rs` | Write-if-changed to a folder, atomic per file |
| `src/analytics.rs` | 370-day series with `Observed`/`Empty`/`Unknown`, monthly rollups |

## 8. Evidence

| Claim | Label |
| --- | --- |
| Four filenames and their order | VERIFIED (read) |
| BOM, CRLF, trailing newline, RFC 4180 quoting | VERIFIED (read) |
| Column sets and order | VERIFIED (read) |
| Tidy day × tool / day × model row shape | VERIFIED (read) |
| Component split and unclassified-remainder rules | VERIFIED (read) |
| JSON 2-space shape, trailing newline, stripped session text | VERIFIED (read) |
| Signature excludes `generatedAt`; skip-when-unchanged | VERIFIED (read) |
| Our serialization against golden files | **VERIFIED** once the golden tests run |
| Signature stability across key order | **VERIFIED** — unit test |
| Missing-day unknown semantics | **VERIFIED** — unit test |
| **Native folder pickers on Windows/macOS/Linux** | **ASSUMED** — no picker driven here |
| **Auto-export cadence against a real synced folder** | **ASSUMED** — no synced folder used |
| **Upstream byte-identical output** | **ASSUMED** — golden files are reproduced from the documented shape, not captured from a running upstream build |
