# Wave-2 INTEGRATION report — cross-OS verify + backlog (Linux/macOS ports)

Branch `fanout/integration` (base: merged main `e29d1dc`, all 7 Wave-1 tracks).
Run 2026-06-11. Windows = this PC (Win 11), Linux = WSL2/WSLg (Ubuntu), macOS = Mac mini
(Apple Silicon, ssh `admin@192.168.0.11`).

## Part A — test suites (at branch head)

| Suite | Windows | Linux (WSL) | macOS (mini) |
|---|---|---|---|
| core (`rs/`, lib) | **417 ok** (3 ignored) | **428 ok** | **425 ok** |
| core integration tests | all ok (control_parity 5, workspace_format 6) | all ok | all ok |
| app (`crates/app`) | **99 ok** | **104 ok** | **104 ok** |
| terminal-widget | **all ok** (80+19+8+1+8, 1 ignored) | all ok | all ok |

Counts differ across OSes only by `#[cfg]`-gated platform tests. The merged theme.rs
font fix is confirmed live: the app launches on Linux **and** macOS with no patch.

## Part B — backlog fixes

| # | Item | Status |
|---|---|---|
| 1 | zsh ZDOTDIR spawn wiring | **FIXED + live-verified** (Linux & macOS: spawned zsh reports OSC-7 cwd). `integration_for` Zsh branch spawns `zsh -i` with `ZDOTDIR=<dir>/zdotdir` (+`HYPERPANES_ZDOTDIR_ORIG`); all 3 packagers + build.rs ship `zdotdir/`. **Bonus root-cause**: `dispatch::spawn_pane` passed `integration: None`, so *every control-API pane* (MCP `open_pane` etc.) spawned without integration on all OSes — now wired like the GUI path. |
| 2 | core `classify()` zsh branch | **FIXED** — `ShellKind::Zsh` (basename match), tests updated + zsh coverage added. |
| 3 | `shell_integration_dir()` macOS bundle candidate | **FIXED** — `exe_dir/../Resources/shell-integration` added after the existing candidates (bundle.sh already shipped that copy). |
| 4 | macOS palette chord dead | **ROOT-CAUSED, labels fixed, HITL for final confirm.** Slint swaps modifiers on Apple (`event_loop.rs`: Cmd→`control`, Ctrl→`meta` — "Match Qt's behavior"); KeyMsg doesn't carry `meta`, so physical Ctrl+Shift+P falls to the pty (the leaked P). The chord table's ctrl slot therefore already matches **Cmd+Shift+P**; new `CTRL_LABEL` renders chips/menus as `Cmd+…` on macOS (verified live in the context menu: `Cmd+F`). ⚠ Synthetic verification is impossible: peekaboo CGEvents bypass winit's `flagsChanged` modifier tracking — even Shift arrives `false` while the char is uppercase (traced live). T3's original "leak" finding was observed through the same channel. **HITL**: press Cmd+Shift+P on a real keyboard. |
| 5 | macOS pane-header right-click dead | **WORKS at head, verified live** — peekaboo right-click on the header dispatched `OpenPaneContext` and the full menu rendered (screenshot-verified). Either fixed by the merged wave3z2 FocusScope first-press fix or a synthetic-click artifact in T3's run. |
| 6 | Session restore 0-pane tab | **FIXED, 3 layers + tests**: (a) `to_session_file` never persists a 0-pane tab and remaps `active` (the emptied-last-tab-mid-close snapshot); (b) `load_workspace` remaps the saved active index around skipped empty groups; (c) **the live producer**: `State::new`'s placeholder tab survived every workspace load as a 0-pane ghost "term 1" tab — now purged when content lands. Regression tests for all three. |
| 7 | `defaults_are_sane` .ttc relax | **FIXED** — asserts `.ttf|.ttc`. |

### Extra fixes found live by this track

* **GUI single-instance gate was never wired** (only core's headless daemon used it; the
  smoke-doc claim "works on unix" was wrong on every OS). Now: `main()` acquires the lock
  salted by the **userData dir** (Electron parity — isolated/dev instances independent);
  a secondary forwards `{argv,cwd}` and exits 0; the primary drains hand-offs in `tick`
  and routes them (attach-as-tab / attach-as-panes via new `State::attach_panes_from_specs`
  / new windows). Live-verified on **all three OSes** (`.hyperpanes` argv lands in the
  primary; secondary exits 0).
* **Session uids were per-window** (`pane-0` in every window) keying the *shared*
  SessionManager — a freshly spawned pane in any second window clobbered the first
  window's session and both died on its exit (tear-off survived only because adopt
  re-hosts the old uid). Found by the macOS hand-off smoke; uids are now a process-global
  atomic. This also affected plain "New Window" on Windows.

## Part C-1 — control-API parity

Probe: `rs/tools/control-parity-probe.py` (stdlib-only; run against isolated instances —
temp APPDATA / XDG dirs / HOME). Captures JSON **shapes** (key paths + types) of:
control.json discovery, `GET /health`, `GET /state`, `POST /command` (renamePane)
round-trip, `POST /panes/{id}/input`, WS `/events` hello + first output event.

**Result: Windows ≡ Linux ≡ macOS — identical on all 8 captured shapes.** (Values
differ as expected: port/token/pid; optional omit-when-unset pane fields depend on
runtime state, not OS.) Raw shape files: `shapes-{windows,linux,macos}.json` (probe
output, not committed).

## Part C-2 — GUI smoke matrix

Driving: Windows = local; Linux = WSLg (X11/XWayland); macOS = peekaboo daemon
(gui/501 LaunchAgent) for screenshot/click/hotkey. ✔ = verified this run,
✔(W1/W3) = verified live by a prior wave at the same code, HITL = needs a human hand.

| Check | Windows | Linux (WSLg/X11) | macOS |
|---|---|---|---|
| launch | ✔ (parity probe) | ✔ (no font patch) | ✔ (no font patch) |
| frameless chrome / bar-drag / min-max-close | ✔(waves 1-3z) | ✔(T2 live, X11) | ✔(T3 live; traffic lights shown this run) |
| fullscreen round-trip | ✔(waves) | ✔(T2 F11) | ✔(T3 native toggleFullScreen) |
| new tab / pane | ✔ (probe newPane) | ✔ (smoke newPane) | ✔ (smoke newPane) |
| pty echo + vim (no DSR interception) | ✔ (input→output event) | ✔ vim renders+quits | ✔ vim renders+quits |
| clickable path | ✔(wave3y live) | ✔(T5 probe-verified hit-test) | HITL (pointer-synthesis limits) |
| pane tear-off | ✔(waves, live) | ✔(T2 X11; Wayland fallback documented) | ✔(T3 live) |
| `.hyperpanes` file open | ✔ (handoff smoke) | ✔ (smoke) | ✔ (smoke) |
| second-instance handoff | ✔ **PASS** | ✔ **PASS** | ✔ **PASS** |
| prefs shell picker + font render | ✔(wave2 G) | ✔(T4 GUI-verified on WSL) | ✔(T4/T6; Monaco.ttf) |
| zsh pane OSC-7 cwd | n/a (no zsh) | ✔ **cwd reported** | ✔ **cwd reported** |
| palette chord | ✔ Ctrl+Shift+P (waves) | ✔(T2/T4 era) | HITL Cmd+Shift+P (see B4) |
| pane-header right-click menu | ✔(waves) | ✔(T2 era) | ✔ **verified this run** |

Wayland-native leg: not exercised (WSLg defaults to XWayland; per Wave-1 the Wayland
fallbacks are documented, interactive pass remains with the user).

## Part C-3 — packaging artifacts @ HEAD (0.0.6-rc1)

* **AppImage**: `rs/packaging/out/hyperpanes-0.0.6-rc1-x86_64.AppImage` (built in WSL at
  final head) — smoke-launched isolated under WSLg: window + control.json, clean. Ships
  the zdotdir pair.
* **dmg**: `rs/packaging/out/hyperpanes-0.0.6-rc1.dmg` (built on the mini, release
  profile) — staged .app smoke-launched isolated: clean, control.json written. Ships
  shell-integration (incl. zdotdir) in both MacOS/resources and Contents/Resources;
  the new `../Resources` lookup makes the idiomatic copy live.
* (Binary-internal `/health` version remains the crate VERSION `0.1.8`; artifact
  version is the filename/installer version, as in prior releases.)

## Open items / hand-offs

1. **HITL**: Cmd+Shift+P on a real macOS keyboard (expected to open the palette; labels
   now say Cmd). If it fails, capture `$TMPDIR/hyperpanes-debug.log` with
   `HYPERPANES_DEBUG=1` — the `key raw …` line shows the delivered modifiers.
2. **HITL**: Wayland-native interactive pass (Linux box), clickable-path click on macOS.
3. `routing::resolve_second_instance_windows` treats a bare relaunch as focus-only; the
   primary does not yet raise/focus its window on that (Slint has no raise API surfaced
   here) — cosmetic, noted in `apply_handoff`.
4. Debug tracing added this wave (`key raw`, spawn/exit/handoff traces) is env-gated
   behind `HYPERPANES_DEBUG` and left in deliberately.
