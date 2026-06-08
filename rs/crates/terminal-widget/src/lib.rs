//! `hyperpanes-terminal-widget` — the reusable live-terminal pane component.
//!
//! A clean, Wave-2-consumable terminal pane: a [`pane::TerminalPane`] Rust **controller**
//! + a `TerminalPane` **Slint component** (in [`ui`]), driven by the proven Spike A
//! renderer (`alacritty_terminal` grid → `swash` atlas → software `SharedPixelBuffer` *or*
//! GPU `wgpu::Texture` → `slint::Image`, behind the [`render::PaneRenderer`] trait).
//!
//! Unlike the spike, the pane owns **no PTY**: the live shell lives in
//! `hyperpanes_core::session_manager`, and the controller is pumped with that session's
//! output (and forwards key input / resize / DSR replies back to it). See
//! [`pane::TerminalPane`] for the lifecycle, and `src/bin/demo.rs` for a full wiring.
//!
//! ## Pieces
//! * [`grid::TermGrid`] — the renderer-agnostic terminal model (fed bytes → snapshot).
//! * [`render`] — the `PaneRenderer` trait + `SoftwareRenderer` / `GpuRenderer`.
//! * [`font::Font`] — the shared `swash` glyph cache + cell metrics.
//! * [`pane::TerminalPane`] — the controller that ties grid + renderer together.
//! * [`keys::encode_key`] — Slint key event → PTY bytes.
//! * [`ui`] — the compiled Slint components (`TerminalPane`, `DemoWindow`).

pub mod font;
pub mod grid;
pub mod keys;
pub mod links;
pub mod pane;
pub mod render;

/// The compiled Slint components. Wave-2's `app-shell` imports the **`TerminalPane`**
/// component directly from `ui/widget.slint`; this module re-exposes the generated Rust
/// types (`DemoWindow`, `PaneVisual`, `KeyMsg`, …) for in-crate use such as the demo.
///
/// It lives in its own module so the Slint `TerminalPane` component and the Rust
/// [`pane::TerminalPane`] controller can share a name without colliding.
pub mod ui {
    slint::include_modules!();
}

// ---- flat re-exports for ergonomic downstream use ----
pub use font::Font;
pub use grid::{GridSnapshot, RenderCell, TermGrid, TermSize};
pub use keys::encode_key;
pub use links::{extract_path_candidates, PathCandidate};
pub use pane::{cells_for_px, LinkAction, LinkHit, TerminalPane};
pub use render::{GpuRenderer, PaneRenderer, RenderOpts, SoftwareRenderer};
