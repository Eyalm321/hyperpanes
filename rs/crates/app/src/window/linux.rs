//! Linux stub of the window-chrome surface (see `mod.rs` for the frozen signatures).
//! Compiling no-ops today; the Wave-1 `linux-window` track owns this file and fills in
//! the real implementation (Wayland/X11 chrome, fullscreen, drag cursors).

/// Opaque saved window placement, restored when leaving fullscreen.
pub type SavedPlacement = ();

/// Native handle of a Slint window. No HWND equivalent is wired yet; `0` means
/// "not realized" to every caller, which keeps the chrome calls no-ops.
pub fn hwnd_of(_win: &slint::Window) -> isize {
    0
}
pub fn make_frameless(_raw: isize) {}
pub fn start_drag(_raw: isize) {}
pub fn begin_drag_cursor(_raw: isize) {}
pub fn end_drag_cursor(_raw: isize) {}
pub fn set_hover_cursor(_on: bool) {}
pub fn minimize(_raw: isize) {}
pub fn toggle_max(_raw: isize) {}
pub fn is_maximized(_raw: isize) -> bool {
    false
}
#[allow(dead_code)]
pub fn close(_raw: isize) {}
pub fn enter_fullscreen(_raw: isize) -> Option<SavedPlacement> {
    Some(())
}
pub fn exit_fullscreen(_raw: isize, _saved: SavedPlacement) {}
