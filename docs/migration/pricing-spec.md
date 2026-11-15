# Pricing and cost specification

Status: implements the "Cost and pricing" and "Automatic price catalog" rows in
[inventory.md](inventory.md). Sources read before writing this:
`token-monitor/src/shared/pricingCatalog.js`, `usage.js`,
`tokscaleCustomPricing.js`, `docs/API.md` (§ customPricing, unpricedTokens),
`docs/architecture.md` ("Pricing authority"), `docs/configuration.md`
("Usage costs follow Edge Dock"), and the pinned Tokscale
`TokenBreakdown`/`CostProvenance` definitions.

## Rates

Rates are **USD per one million tokens**, declared as exact decimal strings:

```
input_per_m, output_per_m, cache_read_per_m,
cache_write_per_m, cache_write_1h_per_m
```

`None` means *unset* (unknown, never zero). An explicit `"0"` is a known free
rate. At least one of input/output must be set; any provided rate must be a
finite non-negative decimal. Custom entries are keyed by
`(provider, model)`; values are per-million and follow the upstream field
semantics (`input_cost_per_million_tokens`,
`cache_creation_input_token_cost_per_million_tokens_above_1hr`, etc.). The
provider is the parsed billing-provider hint when one exists, otherwise the
client id (`claude`, `codex`). Lookup precedence: custom `(provider, model)` →
custom `(any, model)` → bundled catalog by normalized model.

Internally every rate is an `i64` count of **nano-USD per million tokens**
(`$1` per million = `1_000_000_000`). No `f64` is used for money.

Model ids are normalized deterministically: lowercase, drop a `provider/`
prefix, drop a trailing `(reasoning-tier)` suffix, drop a trailing `-YYYYMMDD`
date, then exact match. A dated variant may also resolve through the longest
catalog key that is a `-`-delimited prefix of the id. Unmatched ids stay
unpriced; nothing is guessed.

## Billing rules

- `input` tokens bill at the input rate.
- `output` tokens bill at the output rate (including `reasoning`, which the
  Codex reader stores as a disjoint subset of upstream output; the Claude
  reader emits zero reasoning). **ASSUMED** for providers that include
  reasoning inside output; no supported reader double-counts today.
- `cache_read` bills at the cache-read rate.
- `cache_write` is a total that already contains `cache_write_1h` (verified
  from Tokscale's `TokenBreakdown` doc comment). The 1-hour subset bills at
  the 1-hour write rate; the remainder bills at the standard write rate. If
  the 1-hour rate is unset the standard write rate applies to both.
- Rounding is half-up per component at nano-USD precision:
  `cost = round(rate_nano * tokens / 1_000_000)`, accumulated with checked
  `i128` arithmetic.

## Provenance

`CostProvenance` is `Actual`, `Estimated`, `Free`, or `Unpriced`:

| Value | Meaning |
| --- | --- |
| `Actual` | The source supplied a cost; it is preserved verbatim. Rate coverage still reports unpriced tokens. |
| `Estimated` | Every billable token had a usable rate and the total is non-zero. |
| `Free` | Every billable token had an explicit zero rate (or there was no billable usage). |
| `Unpriced` | At least one billable token had no usable rate. The known subtotal is retained and `unpriced_tokens` counts the missing coverage. |

Incomplete coverage never replaces a source cost, and zero cost alone is never
treated as "free" — a zero from an unset rate is unpriced. This matches the
upstream "known subtotal + unpricedTokens" contract.

## Custom rates

Input shape mirrors `customPricing` (`{modelId, inputPerM?, outputPerM?,
cacheReadPerM?, cacheWritePerM?, cacheWrite1hPerM?}`). Normalization is
differential-tested against the upstream `tokscaleCustomPricing.test.js`
cases: trim ids, drop blank ids/rows with neither input nor output, reject a
row when any provided value is invalid (`false`, blank, array, negative,
non-finite), treat `""`/null as unset, preserve explicit `0`, deduplicate by
id with last-wins. No writes to any external config; overrides live in the
pricing catalog in process.

## Catalog and refresh

- **Bundled catalog:** `assets/pricing-catalog.json`, embedded with
  `include_str!`. It contains Claude/Codex models with rates taken from the
  public LiteLLM `model_prices_and_context_window.json` snapshot
  (`sha256 7147793e586ca9668140be9a655e25e60af91839ab53a09f556d8d6b226f976f`,
  fetched 2026-11-13) and records that provenance in the file.
- **Refresh:** a background worker fetches the same URL over HTTPS (host
  allowlist `raw.githubusercontent.com`, no redirects, bounded read, 15 s
  timeout) and filters canonical `claude-*`/`gpt-*` entries into the same
  rate shape. Network JSON numbers are converted to text and parsed by the
  exact decimal parser; the parser accepts exponent notation.
- **Cache:** `<native-data>/pricing/catalog.json`, written atomically
  (temporary + rename) and loaded at startup. The bundled catalog is used
  when the cache is missing or invalid.
- **Freshness:** one hour (mirrors `FRESH_MS`). Fresh caches skip downloads;
  the worker re-checks every 60 s.
- **Backoff:** failures retry after 5, 10, 20, then 30 minutes (cap), with up
  to +20 % jitter, mirroring `pricingCatalog.js`. A partial success still
  publishes the new revision.
- **Revision and reconciliation:** a catalog revision is `(source hash,
  parsed entry count, logical generation)`. When the revision changes the
  worker publishes the new catalog through a callback; the UI re-projects the
  current snapshot so Home, Models and Sessions costs change without waiting
  for a source event. Stop is idempotent and suppresses late publication.

Resident scans never perform network requests: costs are computed from the
bundled/cached catalog and the existing snapshot.

## Presentation

- Home: `Cost  $x.xxxxxx`, or `Cost  —` when there is unpriced usage and no
  positive known cost, or `Cost  $x.xxxxxx + ?` for a partial subtotal.
- Models: a cost column per `(client, model)` row. Retained/unclassified
  tokens with no model are counted as unpriced, never attributed.
- Sessions: a cost column per session row, priced from the canonical session
  rollups and the current catalog.
- Fully priced zero usage displays `$0.00`; unknown data is never shown as
  zero.

## Evidence

- VERIFIED: rate keys and per-million units (`docs/API.md`, §customPricing;
  `tokscaleCustomPricing.js`).
- VERIFIED: `costUsd` is a known subtotal and `unpricedTokens` is explicit;
  zero cost is a valid rate (`usage.js`; `docs/API.md` §unpricedTokens).
- VERIFIED: catalog freshness 1 h and 5/10/20/30-minute backoff
  (`pricingCatalog.js`, `pricingCatalog.test.js`).
- VERIFIED: `cache_write_1h` is a subset of `cache_write` and is not added
  again (pinned Tokscale `TokenBreakdown` doc comment).
- VERIFIED: pricing content changes invalidate anchors and require
  repricing (upstream `docs/architecture.md` "Pricing authority").
- ASSUMED: reasoning bills as output for every supported reader; a future
  reader that includes reasoning inside output must not add it again.
- ASSUMED: LiteLLM JSON numbers round-trip through shortest-decimal text
  without losing the catalog's meaningful precision; all arithmetic after
  parsing is integer-only.
