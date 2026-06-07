//! [`TerminalPane`] — the reusable Rust controller for one terminal pane.
//!
//! It owns the renderer-agnostic grid model ([`TermGrid`]) and a chosen [`PaneRenderer`]
//! (software or GPU), and exposes a small, Wave-2-friendly API. It is deliberately
//! **decoupled from the session transport**: the caller pumps it.
//!
//! ## Lifecycle (how the app-shell drives N of these)
//! 1. Spawn/attach a session in `hyperpanes_core::session_manager` sized to your initial
//!    `cols`×`rows`, and construct a `TerminalPane` of the same size.
//! 2. On each `SessionEvent::Data { data, .. }` for this pane → [`feed`](Self::feed), then
//!    drain [`take_replies`](Self::take_replies) and `SessionManager::write` them back
//!    (DSR/DA answers — the shell hangs without them).
//! 3. On a Slint key event → [`crate::keys::encode_key`] → `SessionManager::write`.
//! 4. On a geometry change → compute new `cols`×`rows` from the pixel size and the font
//!    cell metrics, [`resize`](Self::resize) the pane, and `SessionManager::resize` the
//!    session if it returns `true`.
//! 5. Each frame (a Slint `Timer`): if [`take_dirty`](Self::take_dirty) (or the cursor
//!    blink flipped) → [`render`](Self::render) and hand the `slint::Image` to your model.
//!
//! The `Font` is passed in at render time so a whole fleet of panes can share one glyph
//! cache (it is `&mut` because rasterization is lazy/cached).

use crate::font::Font;
use crate::grid::TermGrid;
use crate::render::{PaneRenderer, RenderOpts};
use slint::Image;

/// Controller for a single terminal pane: grid model + a pluggable renderer.
pub struct TerminalPane {
    grid: TermGrid,
    renderer: Box<dyn PaneRenderer>,
}

impl TerminalPane {
    /// Create a pane of `cols`×`rows` cells driving the given renderer. Use
    /// [`crate::render::SoftwareRenderer`] (always available) or
    /// [`crate::render::GpuRenderer`] (when a wgpu device is in hand).
    pub fn new(cols: usize, rows: usize, renderer: Box<dyn PaneRenderer>) -> Self {
        Self {
            grid: TermGrid::new(cols, rows),
            renderer,
        }
    }

    /// Feed a chunk of session output (the `data` of a `SessionEvent::Data`) into the grid.
    pub fn feed(&mut self, data: &str) {
        self.grid.feed(data.as_bytes());
    }

    /// Feed raw output bytes (when you have bytes rather than a decoded `String`).
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.grid.feed(bytes);
    }

    /// Drain terminal-originated replies (DSR/DA/etc.) that must be written back to the
    /// session's pty. Empty when there is nothing to forward. **Must** be forwarded or a
    /// real conpty blocks at startup (see [`TermGrid::take_replies`]).
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.grid.take_replies()
    }

    /// Resize the pane's grid. Returns `true` if the cell dimensions changed — in which
    /// case the caller should also `SessionManager::resize` the bound session.
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        self.grid.resize(cols, rows)
    }

    /// Take-and-clear the repaint flag. `true` means the grid changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        self.grid.take_dirty()
    }

    /// Render the current grid to a `slint::Image` at the pane's *physical* pixel
    /// resolution (`cols*cell_w × rows*cell_h`). Cheap to call repeatedly — the renderer
    /// caches its buffers/atlas — but gate it on [`take_dirty`](Self::take_dirty) plus the
    /// cursor blink for minimal CPU.
    pub fn render(&mut self, font: &mut Font, opts: &RenderOpts) -> Image {
        let snap = self.grid.snapshot();
        self.renderer.render(&snap, font, opts)
    }

    /// Current grid size in `(cols, rows)`.
    pub fn grid_size(&self) -> (usize, usize) {
        let s = self.grid.size();
        (s.cols, s.rows)
    }

    /// A human-readable name for the active renderer (e.g. for a HUD/debug overlay).
    pub fn renderer_name(&self) -> &'static str {
        self.renderer.name()
    }

    /// Swap the renderer at runtime (e.g. GPU↔software on a device-lost / RDP transition).
    /// The next [`render`](Self::render) rebuilds from the live grid, so the swap is seamless.
    pub fn set_renderer(&mut self, renderer: Box<dyn PaneRenderer>) {
        self.renderer = renderer;
    }
}

/// Compute the cell grid that fits a pane of `width_px`×`height_px` *physical* pixels for
/// a font with `cell_w`×`cell_h` cells. Clamped to a sane minimum so a collapsed pane
/// never produces a 0-sized grid. A small free helper the app-shell can reuse for the
/// geometry→resize step.
pub fn cells_for_px(width_px: f32, height_px: f32, cell_w: u32, cell_h: u32) -> (usize, usize) {
    let cols = ((width_px as u32) / cell_w.max(1)).max(2) as usize;
    let rows = ((height_px as u32) / cell_h.max(1)).max(1) as usize;
    (cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::SoftwareRenderer;

    #[test]
    fn feed_then_dirty_then_render_roundtrips() {
        let mut p = TerminalPane::new(20, 4, Box::new(SoftwareRenderer::new()));
        assert!(p.take_dirty()); // starts dirty
        assert!(!p.take_dirty());
        p.feed("hi");
        assert!(p.take_dirty());
        assert_eq!(p.grid_size(), (20, 4));
    }

    #[test]
    fn resize_signals_session_resize_need() {
        let mut p = TerminalPane::new(20, 4, Box::new(SoftwareRenderer::new()));
        assert!(!p.resize(20, 4));
        assert!(p.resize(30, 8));
        assert_eq!(p.grid_size(), (30, 8));
    }

    #[test]
    fn cells_for_px_clamps_and_divides() {
        assert_eq!(cells_for_px(800.0, 400.0, 8, 16), (100, 25));
        // Collapsed pane never yields a zero grid.
        assert_eq!(cells_for_px(0.0, 0.0, 8, 16), (2, 1));
    }
}
