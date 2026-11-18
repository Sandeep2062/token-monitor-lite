# Settings specification

Scope: the native settings sections — **General, Appearance, Currency, Language** —
plus the two properties they have to hold: unknown fields survive a save, and the
existing revisioned Save/Cancel keeps working.

Read from the pinned reference:

- `token-monitor/src/electron/main.js` — the settings defaults block
- `token-monitor/src/shared/currency.js` — the currency table and formatting
- `token-monitor/src/electron/motionPreference.js` — `system` / `on` / `off`
- `token-monitor/src/electron/renderer/i18n.js` — the six-locale catalogue
- `scripts/run-linux-acceptance.py` — the AT-SPI acceptance

Labels: **VERIFIED** = read from the reference, or produced by a command here.
**ASSUMED** = designed but not observed.

## 1. Sections and their fields

Field names are the reference's own, so a settings file stays interchangeable with
the tool that writes it. **VERIFIED** (read from `main.js`).

| Section | Field | Values | Default |
| --- | --- | --- | --- |
| Appearance | `themeMode` | `system`, `light`, `dark` | `system` |
| Appearance | `density` | `comfortable`, `compact` | `comfortable` |
| Appearance | `reduceMotion` | `system`, `on`, `off` | `system` |
| Currency | `currency` | `USD`, `TWD`, `HKD`, `CNY` | `USD` |
| Language | `language` | `auto`, `en`, `zh-TW`, `zh-CN`, `ko`, `ja`, `pt-BR` | `en` |

**Divergence, stated rather than hidden.** The reference does not store a
light/dark mode: it stores a custom colour set in `themeColors` and reports
`theme: 'custom' | 'system'`. This task asks for light/dark/system, so
`themeMode` is a new native field. `themeColors` is **left untouched** — choosing
a mode must not delete the reference's colours, and a test pins that.

`language` defaults to `en` here, not `auto`. The reference defaults to `auto`,
but the acceptance run locates controls by their English labels, so a machine
whose locale is Japanese would otherwise fail to find "Settings". `auto` remains
selectable and resolves against the OS. This is a deliberate, test-visible choice.

## 2. Currency is display-only

`currency.js` ships a **fixed** table — USD 1, TWD 31.5, HKD 7.8, CNY 6.8 — plus
`convertUsd` (×rate, rounded to 6 decimals) and `fractionDigitsFor` (2 decimals at
or above a boundary, 4 below; the boundary is 10 for USD and 1 otherwise).
**VERIFIED** (read).

Accounting stays in USD. That is enforced by *shape*, not comment:
`Currency::format_from_usd` and `format_nano` return `String`. There is no API
that hands back a converted number a caller could add to a total, so a second
currency cannot leak into the figures. **VERIFIED** by test.

The fixed rates are deliberate: a live rate would make two exports of the same
data disagree. Fetching rates is out of scope.

## 3. Appearance and contrast

The palette lives in `src/theme.rs` as roles (`background`, `surface`, `text`,
`text_muted`, `text_faint`, `border`, `accent`, `warning`, `danger`, `success`),
not as literals scattered through Slint. The reason is testability: contrast is
**computed** against WCAG 2.1 §1.4.3 rather than asserted in prose.

`PAIRS` lists every foreground/background combination the UI draws, and each is
checked at 4.5:1 (body text) or 3:1 (focus ring, which is non-text). **VERIFIED**
by test.

**A real defect this found:** the shipped dark `text_faint` (`#6d7b89`) measured
**4.12:1** on the surface — below AA. It has been lightened to `#768492` (4.66:1).
That colour was already in the window, so this is a fix to existing behaviour, not
just to the new palette.

Density changes row height and gap (26/4 comfortable, 20/2 compact). Reduced
motion resolves `system` against the desktop's own preference.

**ASSUMED**: reading the desktop's actual appearance preference. There is no
portable API, so `src/system_palette.rs` reads the environment conventions
(`TOKEN_MONITOR_DARK`, `GTK_THEME`, `COLORSCHEME`) and otherwise keeps the dark
palette the window has always shipped. Being wrong here preserves existing
behaviour, which is the safe direction. A native probe would need per-target
acceptance evidence.

## 4. Language

Slint 1.18 bundles gettext catalogs at compile time via
`with_bundled_translations`, in the layout
`ui/i18n/<locale>/LC_MESSAGES/token-monitor-lite.po`. `select_bundled_translation`
chooses one at runtime. **VERIFIED** (read from `slint-build` 1.18 and
`i-slint-core` 1.18).

The catalogs are **generated**, not hand-written:
`scripts/generate-settings-i18n.py` reads the upstream 2,428-key catalogue and
emits the `.po` files. A source string the upstream does not have is a hard error,
so a typo in a Slint `@tr()` call cannot silently ship English text in a Japanese
window. **VERIFIED** by test — parity across all six locales, identity for English,
and at least 15 actually-translated messages per locale.

Language names are their own source strings ("日本語", not a translation), because
a language menu you cannot read is useless.

**ASSUMED**: the native UI is not fully translated. Only the settings section
headings and the appearance/currency/language controls use `@tr()`; Home, Trends
and Models still carry hard-coded English. Widening that is mechanical but needs
the same upstream-verified source strings, which is why it is called out rather
than half-done.

## 5. Unknown fields survive a save

This is the property with the most to lose. `Preferences` wraps a
`BTreeMap<String, Value>`, so every key round-trips by construction, and
`with_native_edits` clones the map before replacing only the eleven fields this
app owns. **VERIFIED** by tests covering:

- unknown nested objects and arrays
- explicit `null` (retained distinctly from absent)
- every JSON scalar type
- the reference's `themeColors` and `vendorColors`
- 200 unknown keys in one document
- idempotence: saving twice over its own output is stable

**The honest limit of "byte-exact".** The document is re-encoded, so key order
within an object follows the map and insignificant whitespace differs. No JSON
consumer can observe either. What is preserved exactly is the **key set and every
value**, which is what another tool actually reads. A test pins this distinction
rather than letting "byte-exact" over-claim.

## 6. Save, cancel, and the revision CAS

Appearance choices write the **draft** only. Nothing repaints until Save succeeds,
through the existing revision CAS — so an abandoned draft cannot leave the window
in a theme that was never stored. **VERIFIED** by tests: a stale revision is
refused with `Revision`/`Stale`, a current revision persists all five fields, and
an unsaved draft leaves the committed document untouched.

Unreadable stored values are defaulted **and reported** (`SettingsValues::unreadable`)
so the UI can say it is using a default rather than implying the file agreed.

## 7. Accessibility: what is verified and what is not

`scripts/run-linux-acceptance.py` runs on the remote Linux VM and cannot execute
here. What it enforces, **VERIFIED** by reading it:

- `keyboard-focus` — one Tab must expose a focused control; it does not require a
  specific widget, so added controls cannot break it.
- labels are matched by **exact normalized text** (`_find_node`, equality not
  substring), which is why section headings reuse single words and why the
  language default stays English.
- the accessibility tree must not contain the home path, work path, `$USER` or
  `$LOGNAME`.

**ASSUMED**: the acceptance has not been re-run against these changes. It needs
the Linux VM. Every new control has an `accessible-role`, an `accessible-id` and
an `accessible-label` that names its section and its selected state, and the
contrast ratios are computed in-test — but "the run passes" is not claimed.

## 8. Evidence summary

| Claim | Label |
| --- | --- |
| Field names and defaults from `main.js` | VERIFIED (read) |
| Currency rates, rounding, fraction digits | VERIFIED (read + test) |
| Accounting stays USD; conversion returns text only | VERIFIED (test) |
| Motion `system`/`on`/`off` semantics | VERIFIED (read + test) |
| Six locales, 2,428 keys each, parity | VERIFIED (node + test) |
| Catalog generation from upstream | VERIFIED (generator + test) |
| Every palette pair meets WCAG AA | VERIFIED (computed) |
| Dark `text_faint` was below AA and is fixed | VERIFIED (computed) |
| Unknown fields, nulls, types, 200-key stress | VERIFIED (test) |
| Revision CAS on appearance | VERIFIED (test) |
| **Desktop appearance preference detection** | **ASSUMED** (env conventions) |
| **AT-SPI acceptance re-run** | **ASSUMED** (needs the Linux VM) |
| **Full native-UI translation** | **ASSUMED** (settings sections only) |
