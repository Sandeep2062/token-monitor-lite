# Limits specification — framework, Claude, Codex

Scope: the **limits** lane only — provider quota windows, their status, and the
scheduling rules around them. Token accounting is a separate subsystem
(`docs/migration/reader-contracts.md`); nothing here changes it.

Read from the pinned reference at `token-monitor/`:
`src/shared/limits/{core,collector,providers,accounts,retryPolicy,resetBoundary,runtime}.js`,
`src/shared/limits/providerHelpers.js`, `src/shared/probeDeadline.js`,
`src/shared/providers/claude/{account,limits}.js`, and
`src/shared/providers/codex/{account,limits}.js`.

Every claim carries an evidence label:

- **VERIFIED** — read from the pinned reference revision above, or produced by a
  command in this repository.
- **ASSUMED** — designed from the reference but not observed. Network behaviour
  is **ASSUMED** by default per the task rule: no live call is made until a
  human approves one run with a real account.

## 1. What the reference actually models

### 1.1 Status vocabulary

`core.js` declares exactly these statuses, and nothing else:

```
ok | disabled | notConfigured | unauthorized | rateLimited | sourceRateLimited | unavailable | error
```

`runtime.js` additionally recognises **`timeout`** internally: `ProbeTimeoutError`
sets `status = 'timeout'` (`probeDeadline.js`), and `TRANSIENT_STATUSES` includes
it. So `timeout` is a real wire value even though `core.js`'s `VALID_STATUSES`
omits it — the two lists disagree in the reference. **VERIFIED**.

Two deliberate differences from our task wording:

- The task names a `throttled` state. The reference has no `throttled`; it splits
  throttling into `rateLimited` (a provider-wide limit) and `sourceRateLimited`
  (one lane of a provider). We keep the reference split — collapsing them would
  lose which limit applies.
- The task names a `stale` state. The reference has no row status `stale`; it
  derives staleness from age (`isProviderStale`, `aggregateLimits`) and marks a
  retained row `unavailable` while keeping its old `updatedAt` and windows
  (`runtime.js` `applyAttempt`, lines 539–550). We therefore keep `stale` as a
  *derived* flag on our own row, not as a wire status.

### 1.2 Sources and window kinds

`normalizeSource` accepts `oauth | cli | web | rpc | local | api`; window
`source` is narrower: `web | local` only. Window kinds are ordered
`session, daily, weekly, billing` (`WINDOW_ORDER`). Boundary kind is
`reset | expiry | mixed`. **VERIFIED**.

### 1.3 Provider row shape

`normalizeLimitProvider` produces: `provider`, `accountKey`, `accountLabel`,
`planLabel`, `accountName`, `accountEmail`, `workspaceKind`, `status`, `source`,
`sourceDetail`, `updatedAt`, `windows[]`, `balanceUsd`, `balance`, `resetCredits`,
`region`, plus conditional `adapterId`, `relatedAccountKey`, `webAccountKey`,
`accountKeyAliases`, `actionRequired`, `usageSummary`. **VERIFIED**.

Each window: `kind`, `metric` (`credits | spend`), `source`, `limitId`,
`boundaryKind`, `additional`, `label`, `used`, `limit`, `remaining`,
`usedPercent`, `remainingPercent`, `resetsAt`, `windowMinutes`,
`resetDescription`, `detail`, `currency`, `showMeter`. **VERIFIED**.

`usedPercent` is derived by `percentFromWindow` from `used`/`limit` when not
given explicitly; `remainingPercent = 100 - usedPercent`. A missing quota reads
as `null`, never as `0` (`numberOrNull`). **VERIFIED**.

### 1.4 Claude lanes

`claude/account.js` declares two credential-bearing surfaces and one setting:
`claudeManagedAccounts` (managed, `persist: 'never'`), `claudeWebCookie`
(credential, `storePath: ['providers','claude','webCookie']`, env fallback
`CLAUDE_WEB_COOKIE`), and `claudeWebOrganizationId`. **VERIFIED**.

`fetchClaudeLimits` precedence, in order (**VERIFIED**, `claude/limits.js`
1481–1558):

1. A configured **Web cookie wins outright** — `fetchClaudeWebLimits`.
2. Otherwise local **OAuth credentials** (`readClaudeCredentials`): proactive
   refresh only when *not* macOS and within `CLAUDE_REFRESH_LEEWAY_MS` (5 min),
   then `GET /api/oauth/usage` (`CLAUDE_USAGE_URL`); on `unauthorized` it
   refreshes once and retries — the only reactive retry.
3. On failure it may fall back to the **Claude CLI** (`claude — usage text`)
   when `claudeCliFallbackEnabled !== false` and the error is one
   `shouldTryClaudeCliFallback` allows, and only if the CLI is authenticated.

Window mapping from the OAuth usage payload (`mapClaudeUsageToProvider`):
`five_hour` → `session`, `seven_day` → `weekly`, a `weekly_scoped` `limits[]`
entry named "Fable" → one extra `weekly` (a promo that self-removes),
and usage credits → one `billing` window with `metric: 'spend'` **when and only
when** the enable flags say credits are on (`spend.enabled` or
`extra_usage.is_enabled`) — never merely because a number exists. `spend` wins
over `extra_usage` because its units are self-describing
(`{amount_minor, currency, exponent}`). `spend.percent` is deliberately **not**
forwarded: it reports 0 rather than null when unlimited, which would paint a 0 %
meter over real spending. **VERIFIED**.

The CLI lane reports *percent left*, so `cliWindow` converts with
`100 - percentLeft`, clamped to 0..100, and carries `resetDescription` text plus
`windowMinutes` (session 300, weekly 10080). **VERIFIED**.

Reset credits come from the `cedar_ember` block; spent grants
(`resets_left <= 0`) and expired ones (`ends_at` past) are dropped — counting
them would promise a reset the account cannot use. **VERIFIED**.

### 1.5 Codex lanes

`codex/account.js` declares only `codexManagedAccounts`. **VERIFIED**.

`readCodexUsageOrRpc` prefers the **OAuth** usage endpoint
(`chatgpt.com/backend-api`, base URL overridable by `chatgpt_base_url` in
`config.toml`) and falls back to the **app-server RPC** only when the OAuth error
is `notConfigured`/`unauthorized` or a transient fallback is allowed. A missing
`auth.json` is `notConfigured`; an `auth.json` without `access_token` is
`unauthorized`. 401/403-with-`forbiddenIsUnauthorized` → `unauthorized`;
429 → `sourceRateLimited`; anything else → `unavailable`
(`providerHelpers.fetchJson`). **VERIFIED**.

Window kinds are derived from duration, not from names
(`codexWindowKind`): 30 days → `billing` (labelled "Monthly"), ≥ 7 days →
`weekly`, ≥ 1 day → `daily`, exactly 5 h → `session`, else `session`; a
`secondary` window with no duration → `weekly`. Named buckets in
`rateLimitsByLimitId` other than `codex` become **`additional: true`** windows
carrying their `limitId` and `limitName` — and only when a canonical quota
exists, so the same numbers are never published twice. **VERIFIED**.

### 1.6 Scheduler rules (`runtime.js`)

**Concurrency**: `DEFAULT_LIMITS_MAX_CONCURRENCY = 3`; `pump()` runs at most
`maxConcurrency` dispatches and one lane never runs two probes at once
(`lane.active`). Queue position is by intent `sequence` (oldest first).
**VERIFIED**.

**Deadline**: every probe runs inside `runWithProbeDeadline`, twice — once in
`collector.probeLimitProvider`'s caller and once around `dispatchIntent`. The
bound is `providerPhysicalBoundMs + PROVIDER_CLEANUP_GRACE_MS` (5 s), where the
physical bound is 120 s scaled by the number of accounts a provider must probe
(`providerPhysicalBoundMs`). A timeout is **not** a provider failure: it aborts
the attempt with `status = 'timeout'`. **VERIFIED**.

**Backoff** (`retryPolicy.js`): failures in
`{timeout, rateLimited, sourceRateLimited, unavailable, error}` are retryable.
`computeRetryDelayMs` uses `Retry-After` when present (bounded to 1 h, plus up to
10 % jitter capped at 5 s), else exponential from 5 s doubling to 5 min, jittered
over the upper half of each cap. **VERIFIED**.

**Reset boundary** (`resetBoundary.js`): after a window resets, a refresh is
scheduled at `resetsAt + 30 s`, with a minimum timer of 5 s and the 32-bit timer
ceiling; attempted keys are pruned when they leave the snapshot. **VERIFIED**.

**Last-good retention** (`applyAttempt`): a successful attempt replaces
`lastGood`; a **transient** failure keeps `lastGood` untouched; a
**non-transient** status (`notConfigured`, `unauthorized`, `disabled`) clears it;
a success that only re-reports some windows keeps the rest **visibly stale** by
downgrading the row to `unavailable`, restoring the previous `updatedAt`, and
carrying the previous balance when a credits window was retained. **VERIFIED**.

**Trigger discipline**: the runtime has no hover, visibility or focus listener.
`refresh()` is driven by an interval, a reset boundary, or an explicit reason;
`COOLDOWN_BYPASS_REASONS` is an allowlist of *state changes* (login, credential
save, account add/remove, settings change), and `burn-rate` is deliberately
absent so it cannot bypass a cooldown. Local writes therefore do not probe
implicitly — a caller must name the reason. **VERIFIED**.

## 2. What we implement

`src/limits/` — one module tree, no HTTP client of its own, no second watcher.

| Module | Contents |
| --- | --- |
| `status.rs` | The status enum, ordering, `is_transient`, parse/format |
| `window.rs` | Window kinds/metrics, `Window`, `used_percent` derivation |
| `provider.rs` | `LimitsProvider` row, normalization, ordering |
| `store.rs` | Last-good retention: the `apply_attempt` rules |
| `retry.rs` | Backoff, `Retry-After` parsing, jitter — deterministic RNG injected |
| `reset_boundary.rs` | Next refresh after a reset |
| `runtime.rs` | Bounded concurrency, deadlines, queue, reasons |
| `providers.rs` | Parse+map only, for **Claude** and **Codex**; transport is injected |
| `transport.rs` | The one authenticated GET, `HttpsTransport` and `RecordedTransport` |
| `collector.rs` | Credential → transport → mapped rows, generation tickets |
| `bridge.rs` | `ProbeSource` adapter and the UI hand-off |
| `scheduler.rs` | Deterministic `Scheduler` plus the blocking `LimitsWorker` |

### 2.1 Decisions that differ from the reference, with reasons

1. **`stale` is derived, not sent.** A row carries `stale: bool` computed from
   `updated_at` age against the configured interval. We do not invent a wire
   status the reference never emits.
2. **`throttled` maps to two statuses.** `rate_limited` and
   `source_rate_limited` are kept apart, as the reference does.
3. **No credential file reading.** The reference reads `~/.claude/.credentials.json`,
   the macOS Keychain, Windows Credential Manager, `$CODEX_HOME/auth.json` and
   `config.toml`. We **do not**: every credential enters through the existing
   `CredentialService` (native `keyring`), and the provider parsers receive an
   already-resolved access token. This is a deliberate narrowing — one credential
   path, no plaintext on disk, nothing to leak in a log.
4. **`claudeWebCookie` becomes a keyring entry**, not a field inside
   `settings.json`. The environment fallback is retained only for tests.
5. **No CLI subprocess probes.** `claude — usage` and the codex app-server RPC are
   out of scope: both spawn processes, and the RPC needs a second live process.
   The OAuth lanes are the ones implemented; a CLI-sourced row is never produced,
   so `source` is `oauth` only for now.
6. **Node-specific normalizations are omitted** — `opencode` aliases, minimax
   API keys, antigravity grouping, `thirdparty` adapters. Ids are validated
   against our catalog so the surface cannot drift from it (see
   `src/catalog.rs`).

### 2.2 Credential rules

- Every secret is a `CredentialMaterial`/`SecretBytes` from `src/credentials.rs`.
- `Debug` on anything holding a secret is redacted; the reference's redacted forms
  are matched.
- No token, cookie, bearer, or email is ever written to a log, an error string, or
  the ledger. Errors carry a status and a stable code only.
- A missing credential is `notConfigured`, never `unauthorized`; the distinction
  is what tells a user to sign in rather than to re-authenticate.

#### 2.2.1 The generation is a ticket, and it is consumed

Discovered while wiring the collector, and it changes the design:
`check_generation` does **not** compare against a fixed current generation. It
**raises** `latest_generation` on every request and refuses anything lower as
`StaleGeneration`. So a read is not free — it consumes the ticket — and the
collector cannot hard-code one.

Consequences, all enforced in code:

- `CredentialTickets` allocates monotonically, shared by the whole lane.
- A probe must never use a constant generation; doing so works once and then
  makes the application's own next write look stale.
- `CredentialService` is now `Clone` over one `Arc` inner, so the application and
  the limits lane share **one** keyring worker thread rather than starting two.
- Secrets leave the service only through `authorize`, a scoped closure; the bytes
  are zeroed afterwards. There is no accessor that returns a secret by value.

### 2.2.2 Retention publishes the last good row

`apply` used to return the bare failure for a transient status, so a throttle
blanked a quota bar that was still valid — precisely what retention exists to
prevent. The retained entry keeps the reading it came from (so a later success
knows what it replaced), and the **published** row carries the failure status and
`stale: true` beside the retained windows. `a_transient_failure_keeps_the_last_good_quota`
pins both halves.

### 2.3 Trigger rules

- Nothing probes on hover, on window focus, or on a local write.
- The only reasons are: `interval`, `reset-boundary`, `manual`, and an explicit
  allowlist of state changes (`login`, `credential-save`, `settings-change`,
  `account-add`, `account-remove`).
- `burn-rate` is explicitly absent from that allowlist, as in the reference.

## 3. Evidence

| Claim | Label |
| --- | --- |
| Status vocabulary, window kinds, row/window fields | VERIFIED (read) |
| Claude OAuth precedence and window mapping | VERIFIED (read) |
| Codex OAuth/RPC precedence and duration→kind rules | VERIFIED (read) |
| Concurrency 3, deadline composition, backoff formula, reset-boundary delay | VERIFIED (read) |
| Last-good retention rules | VERIFIED (read) |
| Reference implementation compiles / runs | not required — reference only |
| Our mapping against recorded payloads | **VERIFIED** — `src/limits/providers/tests.rs` |
| Retention, backoff, reset boundary, scheduling, deadlines, concurrency | **VERIFIED** — `src/limits/{store,retry,reset_boundary,scheduler,runtime}.rs` tests |
| No probe without a credential, and no request when the keyring is unreachable | **VERIFIED** — `src/limits/collector/tests.rs` counts transport calls |
| No probe triggered by hover, focus or a local write | **VERIFIED** — `Trigger` has no hover/focus variant and an allowlist gates cooldown bypass |
| No second collector: one worker, blocked until due | **VERIFIED** — `the_worker_starts_idle_and_releases_its_thread` |
| Quota bars, reset time, retained-row marking, sign-in states | **VERIFIED** — `src/limits/e2e_tests.rs` against the real `UiController` |
| Slint surface compiles and lays out | **VERIFIED** — `cargo check --all-targets` builds `ui/app.slint` |
| **Any live Claude endpoint** | **ASSUMED** — requires an approved run with a real account |
| **Any live Codex endpoint** | **ASSUMED** — requires an approved run with a real account |
| **The exact wire paths** (`/api/oauth/usage`, `/backend-api/wham/usage`) | **ASSUMED** — read from the reference, never contacted |
| **The real keyring on each OS** | **ASSUMED** — tests use the in-memory backend |

