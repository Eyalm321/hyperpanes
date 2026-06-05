# Tab / pane drag-and-drop — case matrix

Living checklist of every drag interaction. Tick cases off as they're GUI-verified
(`npm run dev`). The mechanics live in: `src/renderer/components/TabStrip.tsx`
(tab drag), `src/renderer/paneDrag.ts` (pane drag), `src/renderer/liveTearOff.ts`
(capture hand-off), `src/main/ipc.ts` (`drag:detach`/`drag:drop`/`drag:cancel` +
the float/dock state machine), `src/renderer/store/useWorkspace.ts` (moves).

**Status:** ✅ implemented (static-verified; GUI pending) · ⚠️ works w/ quirk or
open decision · ❌ gap · ❔ undefined / needs a call · 🔲 GUI-confirmed (tick here)

**Test coverage.** The pure move logic is unit-tested in
`src/renderer/store/useWorkspace.dnd.test.ts` (`moveGroupToIndex`, `movePaneToGroup`
cross-group + same-group reorder, `injectGroup` slot/append/pristine/idempotent,
`adoptPaneInto` slot/clamp/idempotent) and `src/main/window-geometry.test.ts`
(`originForDrop` + grab-band invariant). The
pointer orchestration (`paneDrag.ts`, `TabStrip`/`PaneFrame` handlers, `liveTearOff`)
and the main-process float/dock IPC need a DOM/Electron harness — those are the
🔲 GUI-only rows.

**Terms:** *Dock* = drop a tab into a strip · *Redock* = mid-drag dock-preview /
undock crossing strips · *Tear off* = pull into a new window · *Stitch in* = insert
a pane into a layout at a position · *Move-window* = dragging a window's entire
content relocates the whole window.

## T — Dragging a TAB
| # | Precondition → gesture → target | Expected | Status | GUI |
|---|---|---|---|---|
| T1 | press, move <6px, release | activate tab (click) | ✅ | ✅ GUI |
| T2 | drag within own strip, release on strip | stays docked | ✅ | 🔲 |
| T3 | reorder tab among its siblings in own strip | live reorder | ✅ (new) | 🔲 |
| T4 | multi-tab window → pull tab off strip | tears off → float follows cursor | ✅ | 🔲 |
| T5 | float over another window's tab bar | float hides, dock-preview ghost at slot | ✅ | 🔲 |
| T6 | float back over source window's bar | dock-preview in source (redock home) | ✅ | 🔲 |
| T7 | float moves off any bar | float reappears (undock) | ✅ | 🔲 |
| T8 | release over another window's bar | docks at that slot | ✅ | 🔲 |
| T9 | release over empty desktop | stays as its own new window | ✅ | 🔲 |
| T10 | release over another window's body (not bar) | new window (Chrome-like) — **decided** | ✅ | 🔲 |
| T11 | sole tab of a window → drag it | whole window moves; no duplicate | ✅ | 🔲 |
| T12 | sole-tab move → release on another bar | window closes, tab docks into target | ✅ | 🔲 |
| T13 | sole-tab move → release on empty desktop | window just stays where dropped | ✅ | 🔲 |
| T14 | drop over a minimized window's bar | skipped (treated as empty) | ⚠️ | 🔲 |
| T15 | drop over menu/window-controls of another window | 40px top-bar band counts as its strip → docks | ⚠️ lenient | 🔲 |
| T16 | pointercancel mid-drag | settles float in place | ✅ | 🔲 |
| T17 | across two monitors (diff scale) | dock/tear-off land correctly (DIP) | ✅ | 🔲 |

## P — Dragging a PANE
| # | Precondition → gesture → target | Expected | Status | GUI |
|---|---|---|---|---|
| P1 | press, move <6px, release | focus pane (click) | ✅ | 🔲 |
| P2 | drag over sibling panes in its own tab, release | reorder within the same layout | ✅ (new) | 🔲 |
| P3 | drag over another tab, quick drop | pane moves to that tab (appended) | ✅ | 🔲 |
| P4 | drag over another tab, hold ≥450ms | spring-loads (switches to that tab) | ✅ | 🔲 |
| P5 | after spring, over a target pane → release | stitch in at that edge's slot | ✅ | 🔲 |
| P6 | after spring, release over target's empty area | append to the sprung tab | ✅ (new) | 🔲 |
| P7 | drag to +/empty strip | new tab from the pane | ✅ | 🔲 |
| P8 | drag out of window | tear off → float with the pane | ✅ | 🔲 |
| P9 | sole pane of sole tab → drag out | whole window moves; no duplicate | ✅ | 🔲 |
| P10 | after spring, then drag out of window | tear off (extract) → float | ✅ | 🔲 |
| P11a | torn-off pane-float over another window's **bar** | docks there as a tab (strip) | ✅ | 🔲 |
| P11b | single-pane float over another window's **body** | stitches into that window's layout at the slot | ✅ | ✅ GUI |
| P11c | **sole-pane** window's pane → another window's **body** | stitches in; source window closes (was: move-only, no stitch — fixed 2026-06-05) | ✅ (fixed) | ✅ GUI |
| P12 | drop on own group **tab** (flashing strip target) | no-op (move to same group) | ✅ | 🔲 |
| P12b | drag into any **dead zone** — the pane's own tile (incl. onto itself / down its own body), another pane's centre, a gap, or outside | **live** tear-off → float follows the cursor (like a tab off its strip); release settles it as a new window | ✅ (new) | ✅ GUI |
| P12d | single-pane float over a pane's dead **centre** (not near a border) | stays a detached float (not stitched); only near a pane EDGE does it hide + show the dropline | ✅ (new) | ✅ GUI |
| P13 | source tab emptied by the move | source tab dropped/reseeded; pty survives | ✅ | 🔲 |
| P14 | pointercancel mid-drag | cleanup, no move | ✅ | 🔲 |

## X — Cross-cutting / lifecycle
| # | Case | Status | GUI |
|---|---|---|---|
| X1 | live ptys survive every dock/tear-off/stitch/window-move (ownership transfer + `movingSessions`) | ✅ — re-home is the key unverified risk | 🔲 |
| X2 | `pointerup` delivered to source after capture moved to `<html>`, cursor outside window | ❔ underlies all tear-off/dock | 🔲 |
| X3 | sole-tab window-move uses `setOpacity(0)` to keep capture during a dock-preview | ✅ | 🔲 |
| X4 | primary (session-of-record) window torn down on a dock → promotion to a survivor | ✅ | 🔲 |
| X5 | overlapping windows: target = first-by-creation-order, not visual z-order | ⚠️ known limit | — |
| X6 | re-entrancy: starting a new drag mid-drag pre-empts (`finalizeDrag`) | ✅ | 🔲 |

## Decided
- **T10** — tab on another window's *body* (not bar) → new window (Chrome-like, strip-only docking). Whole-window docking was rejected: it makes the float vanish whenever it passes over any window mid-drag.
- **Dock band fixed**: the tab strip now lives inside the 40px top bar (not a separate 34px strip), so `DOCK_BAND_HEIGHT` 74→40 and `GRAB_OFFSET_Y` 57→25. (Regression from the strip-into-top-bar refactor.)

## GUI pass — found & fixed (2026-06-05, first real `npm run dev` pass)
- **Drag started a text selection** (esp. across the top bar). Fixed with a central drag
  guard: `src/renderer/dragGuard.ts` adds `hp-dragging` to **`<html>`** on drag start (both
  the tab drag in `TabStrip` and `beginPaneDrag`), self-clearing on the next `pointerup`
  (survives capture hand-off to a torn-off window). CSS `.hp-dragging, .hp-dragging *` forces
  `user-select: none` (overriding the terminal's `user-select: text`). `.hp-pane-header` is
  also `user-select: none` (it's a drag handle). GUI-confirmed.
- **Grab cursor during drag.** Same `.hp-dragging` rule sets `cursor: grabbing !important`.
  Key gotcha: the class must be on **`<html>`** (the pointer-capture target) — while capture
  is active the cursor is resolved from the capturing element, so a `body`-only rule left the
  cursor reverting to the unstyled `<html>` default mid-drag. GUI-confirmed.
- **Sole-pane window couldn't stitch cross-window** (P11c). The stitch mode was gated
  `&& !ds.moveWindow`, so a lone pane only ever moved its window — no drop-line. Lifted the
  gate (`ipc.ts`); `setOpacity(0)`-based hide already keeps capture for a `moveWindow` float,
  and the stitch branch closes the source window after re-home. GUI-confirmed.

### Environment notes (not app bugs, but they bite a dev run)
- `node-pty` ConPTY **`AttachConsole failed`** crashes recur on this Windows box and can take
  the whole dev instance down during heavy session-restore (many panes spawning at once).
- Vite's **file watcher silently died** mid-session (HMR stopped); and `Menu.setApplicationMenu(null)`
  means **Ctrl+R / F5 don't reload** — a main-process change (or a dead watcher) needs a full
  `npm run dev` restart, not an in-app reload.
- `ipc.ts` is under **concurrent edit** by another agent (added a `ControlServer` / launch-time
  `WindowSpec`); the P11c gate-lift coexists with it but watch for clobbering.

## Cross-window pane stitch (P11b) — how it works
The float/dock timer (`ipc.ts`) is 3-way: cursor over a window's **tab bar** → dock-preview
(`tab:preview`, dock as a tab); over its **body** with a **single-pane** float → stitch-preview
(`pane:preview`, the target shows the `.hp-pane-dropline` via `useUI.layoutDrop`); else float.
On release, `finalizeDrag` routes to dock / `pane:stitch` (→ `adoptPaneInto` at the slot) / settle.
Gated to single-pane floats. A **`moveWindow` sole-pane float stitches too** (fixed 2026-06-05 —
the gate used to exclude it): `hideFloat` dims it via `setOpacity(0)` to keep the capture for the
preview, and `finalizeDrag`'s stitch branch re-homes the pane then destroys the float (= the source
window), so the source window closes — the pane equivalent of T12 (docking a window's last tab).
Hit-test + slot math shared with the in-window drag via `stitch.ts`.

## Open decisions / further gaps
1. **Stitch granularity / split tree.** Insertion is before/after a pane along the layout's main axis (preset-based). No "split this specific pane into a new row/column" — the layout model is presets (columns/rows/grid/main-stack), not a BSP split tree. True per-pane split direction needs a layout-model rearchitect.
2. **Stitch a whole tab into another tab** (merge two tabs' panes into one layout) — today a tab dock makes a *separate* tab, never merges panes.
3. **T14 minimized** — can't hover a minimized window's strip anyway; effectively won't-fix.
