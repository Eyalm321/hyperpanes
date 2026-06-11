//! Drag / tear-off — the app's signature interaction (Phase 4, Wave 2).
//!
//! This module owns the **global-cursor drag pump** (lifted from `spike-tearoff`) and
//! the pure geometry used to resolve a drop. The pump does *not* lean on Slint pointer
//! delivery (which is per-window and loses the grab the instant the cursor crosses into
//! another window — exactly what tear-off needs). Instead a drag is *started* by a Slint
//! pointer-down (on a pane header or a tab), and from then on the whole gesture is driven
//! from **global Win32 state** read every 8 ms by [`crate::app::App::tick`]:
//!   * `GetCursorPos`     → screen-global cursor (Slint has no global-cursor API);
//!   * `GetAsyncKeyState` → left-button-still-down / released (drag end);
//!   * `GetWindowRect`    → hit-test the cursor against each window's screen rect.
//!
//! Once the cursor leaves the source window a transparent / click-through / topmost
//! **ghost** (a pure Win32 layered window, kept out of Slint's render path) chases the
//! cursor. On release the drop is resolved against the window under the cursor:
//!   * over another window's **pane area** → *stitch* the pane in at the hovered slot;
//!   * over another window's **tab strip**  → *dock* the pane as a new tab;
//!   * over **empty space**                → a *new window* hosting the pane.
//! A drop back inside the source window **reorders** (pane → slot, tab → strip position).
//!
//! `State` is never mutated mid-drag; the source pane/tab stays put and the ghost+preview
//! provide the live feedback. The detach→adopt (replay-primed, no PTY restart) happens
//! only on release, so a cancelled drag costs nothing.

/// Movement past this many **physical** px (from the press point) promotes a pending
/// press into a real drag — below it, the gesture is just a click (focus / select).
pub const DRAG_THRESHOLD_PX: i32 = 6;

/// Fraction of a tile (along the layout axis) at each end that counts as the "insert
/// before / after" edge band for a stitch; capped so the band stays edge-like on a big
/// tile. Mirrors `src/renderer/stitch.ts` (`EDGE_BAND_FRAC` / `EDGE_BAND_MAX_PX`).
const EDGE_BAND_FRAC: f32 = 0.3;
const EDGE_BAND_MAX_PX: f32 = 140.0;

/// What is being dragged. Just the identity of the dragged element — the chrome (title /
/// accent) is re-read fresh from the live pane at drop time (via `detach_uid`), so a drag
/// never carries a stale snapshot.
#[derive(Debug, Clone)]
pub enum DragKind {
    /// A pane pulled by its header (by session `uid`).
    Pane { uid: String },
    /// A tab pulled along the strip (in-window reorder); `index` is its live position,
    /// updated as it slides between siblings.
    Tab { index: usize },
}

/// One in-flight drag, owned by the app while a gesture is live.
pub struct DragState {
    /// Registry index of the window the gesture started in.
    pub source_win: usize,
    pub kind: DragKind,
    /// Press point in **physical** screen px (to measure the drag threshold).
    pub origin: (i32, i32),
    /// Seen the button actually held (debounces a stale "up" right after the grab).
    pub armed: bool,
    /// Crossed [`DRAG_THRESHOLD_PX`] → a real drag (ghost + previews are now live).
    pub active: bool,
}

impl DragState {
    pub fn new(source_win: usize, kind: DragKind, origin: (i32, i32)) -> Self {
        DragState { source_win, kind, origin, armed: false, active: false }
    }
    pub fn is_pane(&self) -> bool {
        matches!(self.kind, DragKind::Pane { .. })
    }
}

/// Where the cursor currently is, resolved into a drop target. Built each tick by the
/// app from the live window geometry; consumed both to paint previews and to apply the
/// drop on release.
#[derive(Debug, Clone, Default)]
pub struct Hover {
    /// Registry index of the window under the cursor (`None` = empty space).
    pub win: Option<usize>,
    /// Cursor is over that window's tab strip (the top bar).
    pub over_strip: bool,
    /// Insertion index in the strip (for a tab reorder / dock caret).
    pub tab_slot: usize,
    /// The existing tab chip directly under the cursor (vs the empty strip / `+`), if any.
    /// Drives spring-load (hover-to-switch) and dock-into-that-tab on drop.
    pub tab_over: Option<usize>,
    /// Pane tile under the cursor (active-tab pane index), if any.
    pub pane_idx: Option<usize>,
    /// Cursor is within the hovered pane's **header** band (the drag handle) — drives the
    /// idle open-hand cursor.
    pub over_header: bool,
    /// Insertion index among that tab's panes for a stitch (edge-band aware).
    pub slot_index: usize,
    /// The hovered pane's rect (area-relative logical px) — for the slot highlight.
    pub pane_rect: (f32, f32, f32, f32),
    /// The edge marker within the hovered tile: 0 left · 1 right · 2 top · 3 bottom.
    pub edge: u8,
}

/// Edge bands of a tile of size `size` along its layout axis. Returns the slot offset
/// (`0` insert-before, `1` insert-after) and which edge the marker sits on. The central
/// band resolves to insert-after (so an in-window reorder always lands), matching the
/// forgiving "drop anywhere on the tile" reorder while still biasing to the near edge.
pub fn edge_band(pos: f32, size: f32, vertical: bool) -> (usize, u8) {
    let band = (size * EDGE_BAND_FRAC).min(EDGE_BAND_MAX_PX);
    if pos <= band {
        (0, if vertical { 2 } else { 0 }) // before → top/left
    } else if pos >= size - band {
        (1, if vertical { 3 } else { 1 }) // after → bottom/right
    } else {
        (1, if vertical { 3 } else { 1 }) // centre → after (still a valid reorder)
    }
}

// ---- the per-platform pointer pump + ghost (the GlobalPointer seam) ----

/// The global-pointer seam the drag pump runs on. The whole tear-off gesture is driven
/// from OS-global pointer state polled every tick (Slint pointer delivery is per-window
/// and loses the grab the instant the cursor crosses into another window).
///
/// Implementations: Windows = `GetCursorPos`/`GetAsyncKeyState` (`windows.rs`). The
/// Wave-1 platform tracks own `linux.rs`/`macos.rs`; Wayland cannot poll a global
/// cursor, so its implementation returns `supports_cross_window() == false` and the
/// app falls back to in-window drags only.
pub trait GlobalPointer {
    /// Screen-global cursor position (physical px) + whether the primary (left) button
    /// is currently held. `None` when the platform cannot read global pointer state —
    /// the drag pump then never engages.
    fn poll(&self) -> Option<(slint::PhysicalPosition, bool)>;
    /// Whether the pointer can be tracked across/outside this app's own windows
    /// (drives tear-off-to-new-window and cross-window stitch/dock).
    fn supports_cross_window(&self) -> bool;
}

/// The platform's global pointer (a static zero-sized provider).
pub fn global_pointer() -> &'static dyn GlobalPointer {
    &platform::PlatformPointer
}

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "linux.rs"]
mod platform;

pub use platform::{window_rect, Ghost};
