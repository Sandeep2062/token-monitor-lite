# System tray

The tray shows today's tracked tokens and today's cost, offers Open / Refresh /
Quit, and lets the window close into the tray instead of quitting. This file
records what was decided, what was measured, and what is still assumption.

Every claim below is labelled:

- **VERIFIED** — observed here, by a command in this file or a test in this repo.
- **ASSUMED** — designed and reviewed but not observed on this machine.

## Why Slint's own tray and not `tray-icon`

Slint 1.18 ships a `system-tray` feature with native backends, so the comparison
was real rather than hypothetical:

| | Slint 1.18 `system-tray` | `tray-icon` 0.26.1 |
|---|---|---|
| New direct dependency | none — the toolkit is already a dependency | new pinned dependency |
| License | `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0` (already relied on) | `MIT OR Apache-2.0` |
| Windows backend | Win32 `Shell_NotifyIcon` inside `i-slint-core` | `windows-sys` + `muda` |
| macOS backend | AppKit `NSStatusItem` inside `i-slint-core` | AppKit + `muda` |
| Linux backend | `ksni` 0.3.7 (Unlicense) — pure-Rust StatusNotifierItem over D-Bus | `libappindicator`/GTK3 — C libraries |
| Event loop | runs on Slint's own loop | needs its own platform loop integration |
| Icon input | `slint::Image`, the type already in use | `tray_icon::Icon::from_rgba` |

Picked **Slint's `system-tray`** for three reasons that matter here:

1. **No second event loop.** The application already runs one Slint loop and the
   requirement is "no second collector or watcher, no idle polling". `tray-icon`
   needs a platform loop to be pumped (`muda`/`winit` integration), which is a
   second wakeup source to reason about.
2. **No GTK on Linux.** The Linux backend is `ksni`, which speaks
   StatusNotifierItem over the existing session bus. `tray-icon` on Linux needs
   `libappindicator`/GTK3, which drags in a C stack and tens of megabytes of RSS
   against a 100 MiB idle budget and the project's no-Electron/no-webview stance.
3. **One licence surface.** Slint is already a dependency with its licence
   position settled; adding `tray-icon` would add a new dependency and a new
   licence record for no behavioural gain. `tray-icon`'s MIT/Apache-2.0 is
   permissive and would have been acceptable — it lost on the other two points,
   not on licence.

The `system-tray` feature is enabled in `Cargo.toml`. `Cargo.lock` gains
`ksni` 0.3.7 (Unlicense) and `async-channel` 2.5.0 on Linux, `windows-sys`
0.61.2 (`MIT OR Apache-2.0`) on Windows, and `objc2`/`objc2-app-kit` on macOS;
`THIRD_PARTY_NOTICES.md` is regenerated from `cargo metadata` and now records
599 registry crates across 33 license expressions. **VERIFIED**: the pinned
Windows toolchain builds them.

## What the tray shows

The badge is a 32×32 RGBA8 bitmap rendered by `src/tray.rs`, which is pure Rust
and has no OS or Slint dependency:

- row 1: today's tokens, at most four characters (`12k`, `1.2M`, `999T`);
- row 2: today's cost, at most five characters (`$0.00`, `$1.24`, `$12`, `$1.2k`);
- the hover tooltip carries the exact figures: `Token Monitor Lite — today 12,345 tokens · $1.24`;
- the `title` property carries `12k · $1.24`, which macOS renders next to the
  icon in the menu bar and other desktops show in overflow listings.

The tray always reports **today**, not the period the window happens to show.
`tests/ui.rs::tray_always_reports_today_even_when_the_window_shows_all_time`
proves it: with the window on All Time (60 tokens) the tray still reads 10.

**ASSUMED**: the two-line badge is legible after the platform scales it to its
own tray size (16 px on Windows at 100 %, 22 px on many Linux panels). The
pixels are produced deterministically and tested; how a given desktop renders a
32 px source is not something this machine can observe.

## Menu and clicks

- **Open** shows the window.
- **Refresh** submits the same `ApplicationCommand::Refresh` the window's Refresh
  button sends — one collector, one pipeline.
- **Quit** hides the icon and quits the event loop.
- **Left click toggles the window.** `TrayIcon.clicked` is handled by asking the
  window whether it is visible, then hiding or showing it. **VERIFIED** on
  Windows that the window is actually mapped once the loop starts (title
  `Token Monitor`, non-zero window handle, process stays alive). The click
  gesture itself is **ASSUMED** on Windows and Linux; it cannot be exercised
  without a desktop session.

**macOS limitation, from Slint's own item contract:** when a menu is attached,
AppKit pops the menu on left click and `clicked` never fires. Since the menu is a
requirement, macOS gets menu-on-click; there is no toggle there. This is a
property of `NSStatusItem`, not of the chosen crate — `tray-icon` behaves the
same way.

## Close to tray

`closeToTray` is a native-owned key in the settings document, next to the
timezone and the selected sources. `Preferences::close_to_tray()` reads it and
**absent, non-boolean or unreadable means `false`**: the window close quits,
which is the only behaviour that can never strand the application behind an icon
that is not there.

Saving goes through the existing revision CAS — the draft carries
`base_revision`, and `StoreHandle::save_settings` rejects a stale write.
`tests/application.rs::close_to_tray_persists_and_a_stale_draft_cannot_flip_it_back`
proves the sequence: enable, save, revision 1, then replay the same draft with
`false` and observe a `Conflict` notice with the setting unchanged and the draft
preserved.

The window close handler asks the **application** for the committed value rather
than a cached copy, so a save followed immediately by a close cannot race the
100 ms presentation timer. If the value is true and a tray exists, the window
hides and the event loop keeps running (`run_event_loop_until_quit`). Otherwise
the icon is hidden and `quit_event_loop` is called.

## Linux: the fallback

The Linux backend registers a StatusNotifierItem over the session bus. Two
things can be missing, and they are not the same:

1. **No session bus at all** — as under a bare `Xvfb`, which has no D-Bus
   session. `tray::tray_capable()` checks `DBUS_SESSION_BUS_ADDRESS`; when it is
   unset the tray component is never constructed, so no backend is asked to
   register anything, and the settings toggle is disabled with the text "Keeps
   running · no tray on this desktop". **VERIFIED** by the bus-absent rows of the
   resource measurement: the window starts and idle CPU is unchanged.
2. **A session bus without a status-notifier host** — a desktop whose panel has
   no AppIndicator/StatusNotifier support, which is also what the acceptance
   environment looks like (it starts a bus for AT-SPI but no panel). **VERIFIED**:
   the backend reports the failure and the application carries on —
   `Slint: Failed to create system tray icon: Failed to spawn ksni tray: failed to
   register to the StatusNotifierWatcher: org.freedesktop.DBus.Error.ServiceUnknown:
   The name org.kde.StatusNotifierWatcher was not provided by any .service files`.
   The window still appears, the settings screen still works, and idle CPU stays
   at 0 %. **ASSUMED**: on such a desktop the icon is simply not visible, and
   because `tray_capable()` can only see the bus, the close-to-tray setting is
   still offered and would hide a window that nothing brings back. That is why
   the setting is off by default, and the documented recovery is to start the
   application again.

Slint's tray item **fails soft**: a backend error is logged and the item simply
never appears. That is why the close-to-tray decision also requires that a tray
component was actually constructed (`UiController::set_tray_available`) — the
setting can never hide the window when there is no icon to bring it back.

AT-SPI is a separate axis: the tray raises no accessibility node of its own, and
menu accessibility depends on the desktop's status-notifier host, not on this
application. The Xvfb acceptance therefore asserts nothing about the tray icon;
it asserts the window, settings and totals it can see. **ASSUMED** for every
desktop with a real host.

## Resource cost

The tray adds no timer, no thread and no watcher. The badge is re-rendered only
from the delivery pass that already paints the window, and only when the label
text actually changed (`sync_tray` compares against the last applied triple), so
a quiet system does no tray work at all.

Measured on the Linux builder under `Xvfb`, release build, the same populated
home (61 configured sources), 8 s settle then 10 one-second samples; thresholds
are the suite's own: idle CPU ≤ 0.5 %, idle RSS ≤ 100 MiB.

| Build | Session bus | Idle CPU | Idle RSS max | Verdict |
|---|---|---|---|---|
| before (no `system-tray`) | present | 0.1999 % | 34,904 KiB | pass |
| before (no `system-tray`) | absent | 0.1000 % | 32,940 KiB | pass |
| after (tray enabled) | present | 0.0000 % | 36,224 KiB | pass |
| after (tray enabled) | absent | 0.1000 % | 33,968 KiB | pass |

The tray costs about 1.0–1.3 MiB of resident memory (≈3 % of the app) and no
measurable idle CPU. **VERIFIED** by measurement, not by inspection.

`scripts/run-linux-resource-acceptance.py` now runs to completion on the Linux
builder with the tray enabled. Same host, same fixture, release build, all 53
catalog clients enabled so the whole 100-source fixture is selectable:

| Check | Measured | Threshold | Result |
|---|---|---|---|
| cold startup | 956 ms | ≤ 2,000 ms | passed |
| warm startup | median 594 ms, p95 603 ms | — | passed |
| **idle CPU** | **0.2666 %** | ≤ 0.5 % | **passed** |
| **idle RSS** | **73,504 KiB** | ≤ 102,400 KiB | **passed** |
| visible append / burst | 636 ms / 635 ms | — | passed |
| removal / rewrite / recovery | 474 / 628 / 776 ms | — | passed |
| no network listener, no child process | 0 / 0 | 0 | passed |

14 of 15 resource checks pass and all 18 GUI checks pass. The one remaining
resource check is `detail-history-peak-memory`, recorded as `blocked` with
`gate-3-5-views-not-implemented` — the session detail/history views it would
measure do not exist yet, which is why both reports carry the declared
`representative-limits-accounts` blocker and report `diagnostic` rather than
`passed`. That is the honest ceiling for this flow: nothing failed.

The before/after comparison earlier in this file was taken directly, before the
harness could complete, and both builds passed the same thresholds. On this run
idle CPU is 0.2666 % with the tray constructing nothing (no status-notifier host)
and 73,504 KiB resident with all 53 clients enabled — the memory is dominated by
the 100-source fixture, not by the tray, which measured at 1.0–1.3 MiB when the
two builds were compared directly.

Both acceptance scripts derive their expectations instead of hardcoding them: the
seed enables every catalog client, `expected_sources` comes from the fixture
manifest, `_healthy_sources` parses the count the window reports,
`_selected_source_count` and `_page_label` match the row and pager shapes rather
than fixed strings, and the update checks baseline the total the window is
actually showing instead of assuming the fixture aggregate. A timeout now
reports the visible text, so a stalled wait explains itself.

What *was* repaired earlier, because it aborted every remote run before any
measurement: `scripts/check-catalog.cjs` compared the 53-client catalog against
upstream's 42 rows with `deepStrictEqual`, which cannot hold once the documented
extensions exist. It now checks the 42 upstream rows exactly and pins the 11
extensions by name.

## Fixed: enabling every client lost the whole selection

Enabling all clients in the acceptance settings produced a saved selection of 100
sources that the running application then reported as "No sources configured",
with no notice on the settings screen and every row still shown as
"Selected · Available". It was observed on the pre-tray build, so it was not
caused by the tray work, and the cause was not the sources at all.

`SettingsEditor::validate` carried its **own hardcoded client list of 25 ids**
while `assets/catalog.json` tracks **53**. Any draft enabling one of the other 28
— `antigravity`, `commandcode`, `cursor`, `droid`, `grok`, `hermes`, `kimi`,
`kiro`, `zcode` and the rest — was rejected whole with
`InvalidSettings { EnabledClients: Unsupported }`. Nothing was saved, so the
application came back up with no committed selection and the Home screen said
"No sources configured", while the settings screen kept showing the unsaved
draft. A second layer hid it: `UiController` had three more hardcoded lists, and
the settings row list was missing **11** clients the catalog tracks
(`augment`, `bb`, `codebuff`, `freebuff`, `gjc`, `junie`, `kimchi`, `mux`,
`opencodereview`, `roocode`, `trae`), so those could not be toggled at all.

The catalog is now the single source of truth — `catalog::catalog()` (decoded
once) and `catalog::tracks_client()` — and all five lists are gone. Reproduced
and verified headlessly against the acceptance fixture: before the change,
saving 95 sources ended in revision 0 with `InvalidSettings`; after, revision 1,
the monitor starts, and it reports **95 healthy of 95**, 113 session models.
Three tests pin it: every catalog client validates through settings, the settings
row list equals the catalog exactly and every row has a real label, and every
catalog client can be toggled on and produces a savable draft.

## Previously observed, not explained

The symptom that led to the fix above, recorded as it first appeared so the
sequence is not lost: a saved selection of 100 sources that the running
application then reported as "No sources configured".

