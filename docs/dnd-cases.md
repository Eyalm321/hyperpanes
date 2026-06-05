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
| T1 | press, move <6px, release | activate tab (click) | ✅ | 🔲 |
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
| P11b | single-pane float over another window's **body** | stitches into that window's layout at the slot | ✅ (new) | 🔲 |
| P12 | drag over the source tab / drop on own group tab | no-op (cancel) | ✅ | 🔲 |
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

## Cross-window pane stitch (P11b) — how it works
The float/dock timer (`ipc.ts`) is 3-way: cursor over a window's **tab bar** → dock-preview
(`tab:preview`, dock as a tab); over its **body** with a **single-pane** float → stitch-preview
(`pane:preview`, the target shows the `.hp-pane-dropline` via `useUI.layoutDrop`); else float.
On release, `finalizeDrag` routes to dock / `pane:stitch` (→ `adoptPaneInto` at the slot) / settle.
Gated to single-pane, non-`moveWindow` floats (the sole-tab-move float holds the capture and can't
be hidden for a preview). Hit-test + slot math shared with the in-window drag via `stitch.ts`.

## Open decisions / further gaps
1. **Stitch granularity / split tree.** Insertion is before/after a pane along the layout's main axis (preset-based). No "split this specific pane into a new row/column" — the layout model is presets (columns/rows/grid/main-stack), not a BSP split tree. True per-pane split direction needs a layout-model rearchitect.
2. **Stitch a whole tab into another tab** (merge two tabs' panes into one layout) — today a tab dock makes a *separate* tab, never merges panes.
3. **T14 minimized** — can't hover a minimized window's strip anyway; effectively won't-fix.
