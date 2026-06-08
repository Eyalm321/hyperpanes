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

// ---- the Win32 ghost + global-cursor helpers (lifted from spike-tearoff) ----

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;
    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut c_void)
    }

    /// The ghost never handles input (`WS_EX_TRANSPARENT`); forward everything.
    /// `DefWindowProcW` is generic in windows-0.58 so it can't be a raw fn pointer —
    /// this thin shim gives a concrete `extern "system"` proc.
    unsafe extern "system" fn ghost_wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        DefWindowProcW(h, msg, wp, lp)
    }

    /// Screen-global cursor position (physical px). Slint exposes no equivalent.
    pub fn cursor_pos() -> (i32, i32) {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }

    /// Is the primary (left) mouse button currently held?
    pub fn left_button_down() -> bool {
        unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
    }

    /// A window's screen rect (physical px), `(left, top, right, bottom)`. `0`-rect when
    /// the HWND isn't realized yet.
    pub fn window_rect(raw: isize) -> (i32, i32, i32, i32) {
        let mut r = RECT::default();
        if raw != 0 {
            unsafe {
                let _ = GetWindowRect(hwnd(raw), &mut r);
            }
        }
        (r.left, r.top, r.right, r.bottom)
    }

    /// Transparent, click-through, always-on-top window that chases the cursor — the
    /// drag "ghost". Kept entirely out of Slint's render path.
    pub struct Ghost {
        hwnd: HWND,
        w: i32,
        h: i32,
    }

    impl Ghost {
        pub fn new() -> Ghost {
            let w = 200;
            let h = 44;
            unsafe {
                let hmod = GetModuleHandleW(None).unwrap();
                let hinst = HINSTANCE(hmod.0);
                let class = w!("HyperpanesTearoffGhost");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(ghost_wndproc),
                    hInstance: hinst,
                    lpszClassName: class,
                    // brand green (#5ee08f) as 0x00BBGGRR.
                    hbrBackground: CreateSolidBrush(COLORREF(0x008f_e05e)),
                    ..Default::default()
                };
                RegisterClassW(&wc);

                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED
                        | WS_EX_TRANSPARENT
                        | WS_EX_TOPMOST
                        | WS_EX_TOOLWINDOW
                        | WS_EX_NOACTIVATE,
                    class,
                    w!("ghost"),
                    WS_POPUP,
                    0,
                    0,
                    w,
                    h,
                    None,
                    None,
                    Some(hinst),
                    None,
                )
                .unwrap();

                // ~78% opaque so it reads as a translucent overlay.
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 200, LWA_ALPHA);
                Ghost { hwnd, w, h }
            }
        }

        /// Move + show, offset a little below/right of the cursor hotspot.
        pub fn follow(&self, p: (i32, i32)) {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    p.0 + 14,
                    p.1 + 16,
                    self.w,
                    self.h,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }

        pub fn hide(&self) {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

// Non-Windows stub so the crate still type-checks off-Windows (it only ships on Windows).
#[cfg(not(windows))]
pub use stub::*;

#[cfg(not(windows))]
mod stub {
    pub fn cursor_pos() -> (i32, i32) {
        (0, 0)
    }
    pub fn left_button_down() -> bool {
        false
    }
    pub fn window_rect(_raw: isize) -> (i32, i32, i32, i32) {
        (0, 0, 0, 0)
    }
    pub struct Ghost;
    impl Ghost {
        pub fn new() -> Ghost {
            Ghost
        }
        pub fn follow(&self, _p: (i32, i32)) {}
        pub fn hide(&self) {}
    }
}
