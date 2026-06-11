//! macOS stub of the global-pointer pump + drag ghost (see `mod.rs`). Owned by the
//! Wave-1 `macos-window` track. With `poll() == None` the drag pump never engages, so
//! pane/tab drags are inert until the real implementation lands (CGEvent can poll the
//! global cursor, so cross-window tear-off is expected to be supported).

/// [`GlobalPointer`](super::GlobalPointer) stub: no global pointer state is readable yet.
pub struct PlatformPointer;

impl super::GlobalPointer for PlatformPointer {
    fn poll(&self) -> Option<(slint::PhysicalPosition, bool)> {
        None
    }
    fn supports_cross_window(&self) -> bool {
        false
    }
}

/// A window's screen rect (physical px), `(left, top, right, bottom)`. `0`-rect when
/// the native window isn't realized yet.
pub fn window_rect(_raw: isize) -> (i32, i32, i32, i32) {
    (0, 0, 0, 0)
}

/// No-op drag ghost (the Windows version is a transparent layered Win32 window).
pub struct Ghost;
impl Ghost {
    pub fn new() -> Ghost {
        Ghost
    }
    pub fn follow(&self, _p: (i32, i32)) {}
    pub fn hide(&self) {}
}
