# hyperpanes app — architecture (Phase 3, Wave 1)

The GUI crate (`rs/crates/app`) is the native Slint shell. It **consumes**
`hyperpanes-core` (layout math, session manager) and `hyperpanes-terminal-widget`
(the reusable `TerminalPane`) unchanged, and assembles them into a multi-tab
tiled-terminal workspace.

## Module layout

| Module / file        | Responsibility |
|----------------------|----------------|
| `src/main.rs`        | Thin **controller**: runtime + `SessionManager`, realizes the frameless window, wires every Slint callback to a `Command`, runs the 8 ms pump timer. |
| `src/state.rs`       | The central **`State`** — every `Tab` (workspace group) with its panes, layout, sizes, main-fraction, focus and zoom. All mutation lives here. |
| `src/command.rs`     | The **`Command`** enum + **`dispatch`** — the single entry point for every action. |
| `src/paneview.rs`    | **resync** (State → Slint models) + the per-frame **pump** (drain PTY output, render dirty panes, HUD). |
| `src/theme.rs`       | Palette, layout metadata (name/glyph/id round-trip), font loading. |
| `src/window.rs`      | Win32 glue: frameless window, drag, min/max/close, borderless OS fullscreen. |
| `ui/app.slint`       | `AppWindow` — composes the views, declares the models + callbacks. |
| `ui/topbar.slint`    | `TopBar` — brand · tab strip (incl. inline rename) · HUD · tools · window controls · layout picker popup. |
| `ui/paneview.slint`  | `PaneView` — the tiled panes, resize dividers, and the overlay slot. |
| `ui/theme.slint`     | `Theme` global + `IconButton`. |
| `ui/types.slint`     | The model row structs (`PaneItem`, `TabItem`, `DividerItem`, `LayoutOption`). |

## The three Wave-2 seams

Wave-2 features (command palette, preferences dialog, sidebar/projects, clickable
paths, keybindings) plug into these three seams **without touching the workspace
core or the view layout**.

### Seam #1 — central `State` with a mutate-then-resync API (`src/state.rs`)

All workspace data hangs off one `State` (a `Vec<Tab>` + the active index + a few
session/UI scalars). The contract is:

```
mutate State  →  set State.dirty  →  paneview::resync() rebuilds the Slint models
```

Every mutator (`add_pane`, `close_pane`, `set_layout`, `toggle_zoom`,
`resize_divider`, `new_tab`, `switch_tab`, `rename_tab`, …) leaves the data
consistent and flips `dirty`. The pump calls `resync` whenever `dirty` is set,
which is the **only** place that writes the UI models. A Wave-2 feature adds a
mutator here (or reuses one) and never reaches into Slint directly — it just
mutates and lets the next resync reflect it.

`resync` itself is pure projection (`State` → `panes` / `tabs` / `dividers` /
`layouts` models + scalar props), so new derived UI is added by extending
`resync`, not by scattering `app.set_*` calls across the codebase.

### Seam #2 — command dispatch (`src/command.rs`)

Every action is a `Command` value run through `dispatch(&mut State, Command,
&SessionManager) -> Effect`. Today the sources are top-bar callbacks and the
Ctrl+Shift keyboard chords (`main.rs::shortcut`). Tomorrow:

* the **command palette** builds `Command`s from a searchable list and dispatches
  them;
* **keybindings** map chords → `Command`s through the same `shortcut`-style table.

`dispatch` returns an `Effect` for the handful of concerns outside the state
(quit, OS fullscreen) so the controller stays the only place that talks to the
window. Adding an action = adding a `Command` variant + a match arm; no new
plumbing.

### Seam #3 — overlay / panel slot (`ui/paneview.slint`)

`PaneView` carries an `overlay-visible` flag and a full-area scrim + centred
container (`overlay-slot`) that is empty and hidden in Wave 1. The command
palette, preferences dialog and sidebar mount into this slot. Because the slot
already exists in the tree (above the panes, below nothing), Wave-2 panel work
never has to restructure the pane-area layout — it toggles `overlay-visible`
(via a `Command` + a scalar in `resync`) and fills the container.

## Workspace core (Wave 1, built)

* **Tabs / groups** — `State.tabs: Vec<Tab>`, ported from `useWorkspace.ts`'s
  `Group`. Each tab owns its panes + `layout` + `sizes` + `main_fraction` +
  `focused` + `zoomed`. Background tabs keep their `PaneState`s — and therefore
  their live sessions — alive; only the active tab is mounted in the models.
  The tab strip supports new / switch / close / double-click-rename.
* **Full layout switching** — all five presets (`auto`, `single`, `columns`,
  `rows`, `grid`, `main-stack`) are selectable per tab from the layout picker
  (and `Ctrl+Shift+L` cycles).
* **Resizable dividers** — `core::layout::compute_dividers` produces the seams;
  dragging a `DividerHandle` reports the cursor's offset from the seam centre,
  which the controller turns into a fraction delta and feeds to
  `sizes::resize_at` (or `main_fraction`). Re-tiling re-centres the handle under
  the cursor, so the drag is self-correcting.
* **Zoom + fullscreen** — per-tab `zoomed` solos one pane to fill the tab
  (others hidden); OS fullscreen (`window::enter/exit_fullscreen`) covers the
  monitor borderlessly and hides the bars. Each toggles independently.

## Keyboard (Wave 1)

`Ctrl+Shift` + … `←↑→↓` focus by direction · `T` new tab · `N` new pane ·
`W` close pane · `L` cycle layout · `Z` zoom · `F` fullscreen.
`F11` toggles fullscreen; **holding `Esc`** exits fullscreen (a tap still reaches
the shell). Ctrl+Shift is fully app-reserved (never forwarded); bare modifiers,
F-keys and other non-text keys are filtered out so they never leak to the shell.
