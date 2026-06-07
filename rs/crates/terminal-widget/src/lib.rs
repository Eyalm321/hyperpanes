//! `hyperpanes-terminal-widget` — the reusable terminal-pane UI component for the Slint app.
//!
//! Adapt the Spike A renderer into a `TerminalPane` (a Slint component + a Rust controller)
//! bound to a `hyperpanes-core` session:
//!   - **Lift** `rs/spikes/terminal-render/src/{render,term_backend,font}.rs` + the
//!     wgpu-texture-in-Slint integration + `ui/app.slint` (all merged on this branch).
//!   - Bind it to `hyperpanes_core::session_manager` — spawn/attach a pty, feed the raw bytes
//!     into the alacritty grid, render to a Slint `Image` (software-first; GPU behind the
//!     `PaneRenderer` trait), and route Slint key events → pty write + resize.
//!   - Expose a clean component + controller API so Wave-2's `app-shell` can drop N panes into
//!     layout rects.
//!
//! Develop/verify standalone via `src/bin/demo.rs` (like the spike).
//! STUB — owned by track `terminal-widget`.
