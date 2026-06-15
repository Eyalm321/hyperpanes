//! Linux implementation of the global-pointer pump + drag ghost (see `mod.rs`). Two
//! very different backends hide behind the one seam:
//!
//! * **X11** — a real global pointer exists: `QueryPointer` on the root window gives
//!   root-space coordinates + the live button mask every tick, exactly like the Win32
//!   `GetCursorPos`/`GetAsyncKeyState` pump, so the full cross-window tear-off works
//!   (`supports_cross_window() == true`). The ghost is an override-redirect window
//!   chasing the root-space cursor.
//!
//! * **Wayland** — no global pointer exists, by design. `poll()` falls back to the
//!   in-window pointer state tracked by the winit event hook in `window/linux.rs`
//!   (window-relative physical px). Crucially, the implicit grab while the primary
//!   button is held keeps `CursorMoved` flowing to the source window even past its
//!   bounds — so coordinates go out-of-rect when a pane is dragged to the edge, the
//!   hover resolves to "empty space", and the existing drop path detaches into a new
//!   window: the in-window fallback the contract asks for. Cross-window paths stay
//!   unreachable because [`window_rect`] reports a real rect ONLY for the window the
//!   pointer is currently in (coordinates from different windows share no space). The
//!   ghost is a no-op (a Wayland client cannot position a window at the cursor anyway).
//!
//! Backend pick: `WAYLAND_DISPLAY` set ⇒ Wayland (matches winit's own selection; on
//! WSLg both vars are set and Wayland wins — clear `WAYLAND_DISPLAY` to exercise X11).

use std::cell::Cell;
use std::sync::OnceLock;

use crate::window::{is_wayland, pointer_track, with_window};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, KeyButMask, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;

/// Position reported while the pointer is outside every window of ours and untracked
/// (Wayland, after a no-button hover-out): far outside any plausible window rect, so
/// every hit-test misses and a release resolves to empty space.
const OUTSIDE: (i32, i32) = (-100_000, -100_000);

/// The lazily-opened X11 connection (None on Wayland / headless / connect failure —
/// every X11 path then degrades to the contractual no-op).
struct X11 {
    conn: RustConnection,
    root: u32,
}

fn x11() -> Option<&'static X11> {
    static X: OnceLock<Option<X11>> = OnceLock::new();
    X.get_or_init(|| {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots[screen_num].root;
        Some(X11 { conn, root })
    })
    .as_ref()
}

/// [`GlobalPointer`](super::GlobalPointer) over the per-backend pointer source.
pub struct PlatformPointer;

impl super::GlobalPointer for PlatformPointer {
    fn poll(&self) -> Option<(slint::PhysicalPosition, bool)> {
        if is_wayland() {
            // Nothing tracked yet (no pointer event ever hit our windows) → the pump
            // must not engage; afterwards the last in-window state is authoritative.
            let t = pointer_track();
            if t.raw == 0 {
                return None;
            }
            let pos = if t.inside { t.pos } else { OUTSIDE };
            Some((slint::PhysicalPosition::new(pos.0, pos.1), t.left_down))
        } else {
            let x = x11()?;
            let r = x.conn.query_pointer(x.root).ok()?.reply().ok()?;
            let down = u16::from(r.mask) & u16::from(KeyButMask::BUTTON1) != 0;
            Some((
                slint::PhysicalPosition::new(r.root_x as i32, r.root_y as i32),
                down,
            ))
        }
    }

    fn supports_cross_window(&self) -> bool {
        !is_wayland()
    }
}

/// A window's screen rect (physical px), `(left, top, right, bottom)`; `0`-rect when
/// not realized. X11: real root-space geometry (matches the root-space `poll()`).
/// Wayland: window-relative space — `(0, 0, w, h)` for the single window currently
/// hosting the pointer, `0`-rect for every other (their coordinate spaces are disjoint,
/// so pretending they share one would mis-resolve hovers).
pub fn window_rect(raw: isize) -> (i32, i32, i32, i32) {
    if is_wayland() {
        let t = pointer_track();
        if t.raw != raw {
            return (0, 0, 0, 0);
        }
        with_window(raw, |w| {
            let s = w.inner_size();
            (0, 0, s.width as i32, s.height as i32)
        })
        .unwrap_or((0, 0, 0, 0))
    } else {
        with_window(raw, |w| {
            let p = w.outer_position().unwrap_or_default();
            let s = w.outer_size();
            (p.x, p.y, p.x + s.width as i32, p.y + s.height as i32)
        })
        .unwrap_or((0, 0, 0, 0))
    }
}

/// Tear-off ghost. X11: an override-redirect brand-green window (created lazily on the
/// first `follow`) riding the root-space cursor, like the Win32 layered window. Wayland:
/// inert — the in-window fallback never shows a ghost (and a client can't place a
/// surface at the cursor).
pub struct Ghost {
    win: Cell<Option<u32>>,
    mapped: Cell<bool>,
}

const GHOST_W: u16 = 200;
const GHOST_H: u16 = 44;

impl Ghost {
    pub fn new() -> Ghost {
        Ghost {
            win: Cell::new(None),
            mapped: Cell::new(false),
        }
    }

    /// Move + show, offset a little below/right of the cursor hotspot (root coords).
    pub fn follow(&self, p: (i32, i32)) {
        let Some(x) = x11() else { return };
        let id = match self.win.get() {
            Some(id) => id,
            None => {
                let Ok(id) = x.conn.generate_id() else { return };
                // Brand green (#5ee08f); override-redirect keeps the WM (and input)
                // entirely out of it.
                let aux = CreateWindowAux::new()
                    .override_redirect(1)
                    .background_pixel(0x005e_e08f);
                let _ = x.conn.create_window(
                    x11rb::COPY_DEPTH_FROM_PARENT,
                    id,
                    x.root,
                    0,
                    0,
                    GHOST_W,
                    GHOST_H,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &aux,
                );
                self.win.set(Some(id));
                id
            }
        };
        let aux = ConfigureWindowAux::new()
            .x(p.0 + 14)
            .y(p.1 + 16)
            .stack_mode(StackMode::ABOVE);
        let _ = x.conn.configure_window(id, &aux);
        if !self.mapped.replace(true) {
            let _ = x.conn.map_window(id);
        }
        let _ = x.conn.flush();
    }

    pub fn hide(&self) {
        let Some(x) = x11() else { return };
        if self.mapped.replace(false) {
            if let Some(id) = self.win.get() {
                let _ = x.conn.unmap_window(id);
                let _ = x.conn.flush();
            }
        }
    }
}
