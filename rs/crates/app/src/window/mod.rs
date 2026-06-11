//! Native window chrome — the per-platform glue for the frameless window: strip the OS
//! title bar, install the subclass/hooks, min/max/close, borderless OS fullscreen
//! (save/restore placement), and the drag/hover cursor overrides.
//!
//! The dispatch is a cfg-selected module re-export (the lightest seam that keeps every
//! call site unchanged): each platform file exports the SAME function surface, frozen in
//! `docs/ports-seams.md`:
//!
//! ```text
//! pub type/struct SavedPlacement;                  // opaque pre-fullscreen placement
//! pub fn hwnd_of(win: &slint::Window) -> isize;    // native handle (0 until realized)
//! pub fn make_frameless(raw: isize);               // frameless chrome + hooks install
//! pub fn start_drag(raw: isize);                   // system move-drag (drag-the-bar)
//! pub fn begin_drag_cursor(raw: isize);            // force the tear-off drag cursor
//! pub fn end_drag_cursor(raw: isize);              // release drag cursor + capture
//! pub fn set_hover_cursor(on: bool);               // open-hand hover cursor on/off
//! pub fn minimize(raw: isize);
//! pub fn toggle_max(raw: isize);
//! pub fn is_maximized(raw: isize) -> bool;
//! pub fn close(raw: isize);
//! pub fn enter_fullscreen(raw: isize) -> Option<SavedPlacement>;
//! pub fn exit_fullscreen(raw: isize, saved: SavedPlacement);
//! ```
//!
//! `windows.rs` is the original Win32 implementation (moved verbatim from the old
//! `window.rs`); `linux.rs` / `macos.rs` are compiling no-op stubs owned by the Wave-1
//! platform tracks.

#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;

// Linux is also the fallback for other unixes (BSDs etc.).
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "linux.rs"]
mod platform;

pub use platform::*;
