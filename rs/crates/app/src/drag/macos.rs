//! macOS implementation of the global-pointer pump + drag ghost (see `mod.rs`).
//!
//! AppKit can read global pointer state from anywhere (`NSEvent.mouseLocation` +
//! `NSEvent.pressedMouseButtons`), so full cross-window tear-off is supported.
//!
//! Coordinates: AppKit globals are **bottom-left-origin points** on the primary screen
//! (`NSScreen.screens[0]`, whose cocoa origin is (0,0)). The drag pump's contract
//! (see `compute_hover`) is **top-left-origin physical px**, divided by the window's
//! Slint `scale_factor` (= the backing scale) to get window-logical px. So both
//! `poll()` and `window_rect` flip the y-axis against the primary screen height and
//! multiply by the backing scale. The scale used is the **primary screen's** — uniform
//! and consistent between the two fns; a multi-monitor setup with *mixed* DPI would
//! mis-scale on the secondary screen (acceptable Wave-1 limitation, same class of
//! issue as global px coordinates being ill-defined across mixed-DPI displays).

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSScreen, NSScreenSaverWindowLevel, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Primary-screen metrics: `(height in points, backing scale)`. The flip axis + the
/// points→physical-px factor for every coordinate this module reports.
fn screen_metrics(mtm: MainThreadMarker) -> Option<(f64, f64)> {
    let screens = NSScreen::screens(mtm);
    let primary = screens.firstObject()?;
    Some((primary.frame().size.height, primary.backingScaleFactor()))
}

/// [`GlobalPointer`](super::GlobalPointer) over AppKit globals: cursor + button state
/// are always readable, across every window and the desktop (tear-off fully supported).
pub struct PlatformPointer;

impl super::GlobalPointer for PlatformPointer {
    fn poll(&self) -> Option<(slint::PhysicalPosition, bool)> {
        let mtm = MainThreadMarker::new()?;
        let (height, scale) = screen_metrics(mtm)?;
        let p = unsafe { NSEvent::mouseLocation() };
        let x = (p.x * scale).round() as i32;
        let y = ((height - p.y) * scale).round() as i32;
        // Bit 0 of the global pressed-buttons mask = the primary button.
        let down = unsafe { NSEvent::pressedMouseButtons() } & 1 != 0;
        // winit/Slint re-apply their own cursor on every pointer move; keep the
        // closed-hand drag cursor winning at the pump cadence (the AppKit analogue of
        // the Win32 subclass's WM_MOUSEMOVE re-assert).
        crate::window::reassert_drag_cursor();
        Some((slint::PhysicalPosition::new(x, y), down))
    }
    fn supports_cross_window(&self) -> bool {
        true
    }
}

/// A window's screen rect (physical px, top-left origin), `(left, top, right, bottom)`.
/// `0`-rect when the native window isn't realized yet. With `fullSizeContentView` the
/// content fills the whole frame, so the frame rect IS the Slint client area.
pub fn window_rect(raw: isize) -> (i32, i32, i32, i32) {
    if raw == 0 {
        return (0, 0, 0, 0);
    }
    let Some(mtm) = MainThreadMarker::new() else { return (0, 0, 0, 0) };
    let Some((height, scale)) = screen_metrics(mtm) else { return (0, 0, 0, 0) };
    let f = unsafe { &*(raw as *const NSWindow) }.frame();
    (
        (f.origin.x * scale).round() as i32,
        ((height - (f.origin.y + f.size.height)) * scale).round() as i32,
        ((f.origin.x + f.size.width) * scale).round() as i32,
        ((height - f.origin.y) * scale).round() as i32,
    )
}

/// Ghost chip size in points (the Windows ghost is 200×44 px; points keep it
/// crisp-but-equivalent on retina).
const GHOST_W: f64 = 200.0;
const GHOST_H: f64 = 44.0;

/// Borderless, click-through, above-everything window that chases the cursor — the
/// drag "ghost". Kept entirely out of Slint's render path (a bare NSWindow).
pub struct Ghost {
    /// `None` only if constructed off the main thread (never happens: the pump owns it).
    win: Option<Retained<NSWindow>>,
}

impl Ghost {
    pub fn new() -> Ghost {
        let Some(mtm) = MainThreadMarker::new() else { return Ghost { win: None } };
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(GHOST_W, GHOST_H));
        let win = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            // We own the lifetime via the Retained; AppKit must not autorelease on close.
            win.setReleasedWhenClosed(false);
            win.setIgnoresMouseEvents(true);
            win.setHasShadow(false);
            win.setLevel(NSScreenSaverWindowLevel);
            // Follow the drag onto any Space / over fullscreen apps.
            win.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary,
            );
            // Brand green (#5ee08f), ~78% opaque so it reads as a translucent overlay
            // (matches the Win32 ghost's LWA_ALPHA 200).
            win.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                0x5e as f64 / 255.0,
                0xe0 as f64 / 255.0,
                0x8f as f64 / 255.0,
                1.0,
            )));
            win.setOpaque(false);
            win.setAlphaValue(0.78);
        }
        Ghost { win: Some(win) }
    }

    /// Move + show, offset a little below/right of the cursor hotspot. `p` is in the
    /// pump's physical-px top-left coords; convert back to cocoa points.
    pub fn follow(&self, p: (i32, i32)) {
        let Some(win) = &self.win else { return };
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Some((height, scale)) = screen_metrics(mtm) else { return };
        let x = p.0 as f64 / scale + 14.0;
        let y_top = p.1 as f64 / scale + 16.0;
        unsafe {
            win.setFrameOrigin(NSPoint::new(x, height - y_top - GHOST_H));
            win.orderFrontRegardless();
        }
    }

    pub fn hide(&self) {
        if let Some(win) = &self.win {
            unsafe {
                win.orderOut(None);
            }
        }
    }
}
