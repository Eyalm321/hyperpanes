//! Linux implementation of the window-chrome surface (see `mod.rs` for the frozen
//! signatures). Unlike Win32 there is no OS-global window handle that can drive chrome
//! operations on its own — on Wayland *everything* (move-drag, minimize, maximize,
//! fullscreen, decorations) must go through the compositor via the window's own
//! `xdg_toplevel`, which winit owns. So this file routes every op through the
//! `winit::window::Window` behind the Slint window, obtained once via the
//! `slint::winit_030` accessor and kept in a registry keyed by the value we hand out as
//! `raw`:
//!
//! * `raw` encodes the winit window's allocation address (`Weak::as_ptr as isize`) —
//!   unique, stable for the window's lifetime, and nonzero, which is all callers rely on.
//! * The registry stores `Weak` references so a closed window's native handle is not kept
//!   alive by us; a dead entry degrades every op to the contractual "0 = no-op".
//!
//! The same registry also feeds the drag seam (`drag/linux.rs`): the winit event hook
//! installed here tracks the last in-window pointer position + primary-button state,
//! which is the *only* pointer source available on Wayland (no global pointer exists).
//! Works on both backends; X11-only extras (true global pointer, the tear-off ghost)
//! live in `drag/linux.rs`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::sync::{Arc, OnceLock, Weak};
use std::task::{Context, Poll, Waker};

use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent};
use slint::winit_030::winit::window::{CursorIcon, Fullscreen, Window as WinitWindow};
use slint::winit_030::{EventResult, WinitWindowAccessor};

/// One registered window: `raw` → the winit window it encodes, plus the desired
/// frameless state (shared with that window's event hook, which re-asserts it — see
/// [`make_frameless`]).
struct Entry {
    raw: isize,
    win: Weak<WinitWindow>,
    frameless: std::rc::Rc<Cell<bool>>,
}

thread_local! {
    /// UI-thread only (every caller is). Weak so we never extend a closed window's
    /// native lifetime; dead entries are purged on the next `hwnd_of` (the only growth
    /// point).
    static REGISTRY: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
    /// Last pointer state reported by any of our windows (see [`PointerTrack`]).
    static POINTER: Cell<PointerTrack> = const { Cell::new(PointerTrack::new()) };
    /// Whether the open-hand hover cursor is currently forced (mirrors the Win32
    /// `HOVER_CURSOR` static; only transitions touch the windows).
    static HOVER_ON: Cell<bool> = const { Cell::new(false) };
}

/// In-window pointer state fed by the winit event hook — the Wayland fallback pointer
/// source for `drag/linux.rs` (Wayland has no global pointer; during a button-held drag
/// the implicit grab keeps `CursorMoved` flowing to the source window even past its
/// bounds, so `pos` can legitimately go negative / exceed the window size — exactly what
/// lets "drag past the edge" resolve to a detach).
#[derive(Clone, Copy)]
pub(crate) struct PointerTrack {
    /// `raw` of the window the pointer last reported from (`0` = none seen yet).
    pub raw: isize,
    /// Last position, physical px, relative to that window's client origin.
    pub pos: (i32, i32),
    /// `false` after `CursorLeft` (a no-button hover-out): the position is stale and
    /// must not be treated as in-window.
    pub inside: bool,
    /// Primary (left) button currently held.
    pub left_down: bool,
}

impl PointerTrack {
    const fn new() -> Self {
        PointerTrack { raw: 0, pos: (0, 0), inside: false, left_down: false }
    }
}

/// Snapshot of the tracked pointer state (for `drag/linux.rs`).
pub(crate) fn pointer_track() -> PointerTrack {
    POINTER.with(|p| p.get())
}

static WAYLAND: OnceLock<bool> = OnceLock::new();

/// Whether the app realized on Wayland (vs X11/XWayland). Pinned from the first real
/// window's handle in [`hwnd_of`] (authoritative even when winit's backend is forced,
/// e.g. `SLINT_BACKEND=winit-x11`); before any window exists, fall back to winit's own
/// selection rule — Wayland whenever `WAYLAND_DISPLAY` is set (WSLg sets both
/// `WAYLAND_DISPLAY` and `DISPLAY`; force X11 by clearing the former).
pub(crate) fn is_wayland() -> bool {
    *WAYLAND.get_or_init(|| {
        std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
    })
}

/// Run `f` against the winit window `raw` encodes; `None` for 0 / unknown / dead
/// handles (the contractual no-op cases).
pub(crate) fn with_window<T>(raw: isize, f: impl FnOnce(&WinitWindow) -> T) -> Option<T> {
    if raw == 0 {
        return None;
    }
    REGISTRY.with(|r| {
        r.borrow()
            .iter()
            .find(|e| e.raw == raw)
            .and_then(|e| e.win.upgrade())
            .map(|w| f(&w))
    })
}

/// Run `f` on every live registered window (cursor overrides are global on Win32; the
/// closest Linux equivalent is applying to all our windows).
fn for_each_window(f: impl Fn(&WinitWindow)) {
    REGISTRY.with(|r| {
        for e in r.borrow().iter() {
            if let Some(w) = e.win.upgrade() {
                f(&w);
            }
        }
    });
}

/// Pre-fullscreen placement. winit restores the floating size/position itself when
/// leaving fullscreen; the only bit it forgets is whether the window was maximized.
#[derive(Clone, Copy)]
pub struct SavedPlacement {
    maximized: bool,
}

/// Native handle of a Slint window, encoded as the winit window's allocation address.
/// `0` until winit realizes the window (callers retry each tick). First success
/// registers the window and installs the pointer-tracking event hook.
pub fn hwnd_of(win: &slint::Window) -> isize {
    // `winit_window()` resolves immediately once the window exists; a single poll with a
    // no-op waker turns the async accessor into the sync probe this call site needs
    // (Pending → not realized yet → 0, the caller's retry signal).
    let fut = WinitWindowAccessor::winit_window(win);
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    let w = match fut.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(w)) => w,
        Poll::Ready(Err(e)) => {
            // Wrong backend / window torn down — permanent for this window, worth a trace.
            crate::dbg_log(&format!("hwnd_of: winit accessor failed: {e}"));
            return 0;
        }
        Poll::Pending => return 0, // not realized yet; the caller retries next tick
    };
    let raw = Arc::as_ptr(&w) as isize;
    // Pin the backend from the realized handle (beats the env heuristic).
    if WAYLAND.get().is_none() {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = w.window_handle() {
            let _ = WAYLAND.set(matches!(h.as_raw(), RawWindowHandle::Wayland(_)));
        }
    }
    let fresh = REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        r.retain(|e| e.win.strong_count() > 0); // purge closed windows
        if r.iter().any(|e| e.raw == raw) {
            None
        } else {
            let frameless = std::rc::Rc::new(Cell::new(false));
            r.push(Entry { raw, win: Arc::downgrade(&w), frameless: frameless.clone() });
            Some(frameless)
        }
    });
    if let Some(frameless) = fresh {
        crate::dbg_log(&format!(
            "hwnd_of: realized raw={raw} wayland={}",
            WAYLAND.get().copied().unwrap_or(false)
        ));
        // The hook does double duty:
        //  * feed the pointer tracker (the Wayland drag fallback) from this window's
        //    event stream — positions are physical px, window-relative;
        //  * keep the window frameless: the Slint adapter re-asserts
        //    `set_decorations(!no_frame)` on every window-property sync (and the
        //    WindowItem's `no-frame` is compile-time constant, so it can't be flipped at
        //    runtime), which re-grows the title bar after `make_frameless`. Re-strip on
        //    the next event whenever that happens (`is_decorated` is a cached read).
        let hook_win = Arc::downgrade(&w);
        win.on_winit_window_event(move |_, ev| {
            if frameless.get() {
                if let Some(w) = hook_win.upgrade() {
                    if w.is_decorated() {
                        w.set_decorations(false);
                    }
                }
            }
            POINTER.with(|p| {
                let mut t = p.get();
                match ev {
                    WindowEvent::CursorMoved { position, .. } => {
                        t.raw = raw;
                        t.pos = (position.x as i32, position.y as i32);
                        t.inside = true;
                        if t.left_down {
                            crate::dbg_log(&format!("ptr-move-held pos={:?}", t.pos));
                        }
                    }
                    WindowEvent::CursorLeft { .. } => {
                        if t.raw == raw {
                            t.inside = false;
                        }
                    }
                    WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                        t.raw = raw;
                        t.left_down = *state == ElementState::Pressed;
                        if t.left_down {
                            crate::dbg_log(&format!("ptr-press raw={raw} pos={:?}", t.pos));
                        }
                    }
                    _ => {}
                }
                p.set(t);
            });
            EventResult::Propagate
        });
    }
    raw
}

/// Drop the server-side decorations so the Slint top bar is the only chrome. (X11:
/// Motif hints; Wayland: zxdg-decoration / no CSD fallback is compiled in, so the
/// window simply turns borderless.) Also arms the per-window event hook's re-strip:
/// Slint's property sync re-enables decorations behind our back (see `hwnd_of`), so the
/// strip must be standing, not one-shot.
pub fn make_frameless(raw: isize) {
    REGISTRY.with(|r| {
        if let Some(e) = r.borrow().iter().find(|e| e.raw == raw) {
            e.frameless.set(true);
        }
    });
    with_window(raw, |w| w.set_decorations(false));
}

/// Begin a compositor-driven interactive move (the frameless "drag the bar" trick).
/// Must be called from a pointer-down gesture (both backends key the move off the
/// active button grab) — which is exactly how the top bar wires it.
pub fn start_drag(raw: isize) {
    crate::dbg_log(&format!("start_drag raw={raw}"));
    with_window(raw, |w| {
        if let Err(e) = w.drag_window() {
            crate::dbg_log(&format!("start_drag: drag_window failed: {e}"));
        }
    });
}

/// Force the closed-hand "grabbing" cursor on the drag's source window. The implicit
/// pointer grab during a held button keeps events (and the cursor) on that window for
/// the whole gesture, so setting it there is enough — no global capture exists or is
/// needed on Linux.
pub fn begin_drag_cursor(raw: isize) {
    with_window(raw, |w| w.set_cursor(CursorIcon::Grabbing));
}

/// Stop forcing the drag cursor. `0` (the defensive caller) resets every window.
pub fn end_drag_cursor(raw: isize) {
    if raw == 0 {
        for_each_window(|w| w.set_cursor(CursorIcon::Default));
    } else {
        with_window(raw, |w| w.set_cursor(CursorIcon::Default));
    }
}

/// Show / hide the open-hand "grab" cursor while a drag handle is pressed (pre-
/// threshold). Transition-edged: the pump calls this every tick, but only state flips
/// touch the windows (Slint re-asserts its own cursor on the next pointer move, which
/// also naturally undoes our `Default` reset).
pub fn set_hover_cursor(on: bool) {
    let was = HOVER_ON.with(|h| h.replace(on));
    if was == on {
        return;
    }
    let icon = if on { CursorIcon::Grab } else { CursorIcon::Default };
    for_each_window(|w| w.set_cursor(icon));
}

pub fn minimize(raw: isize) {
    with_window(raw, |w| w.set_minimized(true));
}

pub fn toggle_max(raw: isize) {
    with_window(raw, |w| w.set_maximized(!w.is_maximized()));
}

/// Whether the window is currently maximized (drives the restore-vs-maximize icon).
pub fn is_maximized(raw: isize) -> bool {
    with_window(raw, |w| w.is_maximized()).unwrap_or(false)
}

/// Unused by the managed multi-window close path (which flags the window for reaping
/// and drops the component); winit exposes no way to synthesize a close request on
/// another window, so this stays a no-op on Linux.
#[allow(dead_code)]
pub fn close(_raw: isize) {}

/// Cover the current monitor borderlessly. winit remembers the floating geometry and
/// restores it on exit; we only need to carry the maximized flag across.
pub fn enter_fullscreen(raw: isize) -> Option<SavedPlacement> {
    with_window(raw, |w| {
        let maximized = w.is_maximized();
        w.set_fullscreen(Some(Fullscreen::Borderless(None)));
        SavedPlacement { maximized }
    })
}

/// Restore the placement captured by [`enter_fullscreen`].
pub fn exit_fullscreen(raw: isize, saved: SavedPlacement) {
    with_window(raw, |w| {
        w.set_fullscreen(None);
        if saved.maximized {
            w.set_maximized(true);
        }
    });
}
