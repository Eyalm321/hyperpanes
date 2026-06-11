//! macOS implementation of the window-chrome surface (see `mod.rs` for the frozen
//! signatures). Design (user-approved): **hidden titlebar + traffic lights** — the
//! window keeps its native style mask (so the red/yellow/green buttons stay overlaid
//! top-left, like iTerm2/Warp) but the titlebar is made transparent + title hidden and
//! the content view expands under it (`fullSizeContentView`), so the app's custom Slint
//! top bar renders edge-to-edge.
//!
//! Fullscreen choice: **native `toggleFullScreen:`** (own Space, menu bar auto-hides) —
//! the platform-idiomatic behavior, vs the Windows impl's borderless monitor-cover.
//! `SavedPlacement` is therefore a unit: AppKit itself restores the pre-fullscreen frame
//! when toggling back, so there is nothing to save; `exit_fullscreen` just toggles again
//! if the window is still in fullscreen (a user may have already left via the green
//! button / Esc — the style-mask check makes that a no-op).
//!
//! `raw` is the NSWindow pointer as `isize` (`0` = not realized). Every AppKit call is
//! main-thread-only; the seam is only ever invoked from the Slint UI thread (which IS
//! the process main thread on macOS), and each fn double-checks with
//! [`MainThreadMarker`] and no-ops off-thread rather than UB.

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSCursor, NSEvent, NSEventModifierFlags, NSEventType, NSView, NSWindow, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_foundation::{NSPoint, NSProcessInfo};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::{AtomicBool, Ordering};

/// Nothing to save: native `toggleFullScreen:` restores the prior frame itself.
pub type SavedPlacement = ();

/// A tear-off drag is in flight → force the closed-hand cursor. AppKit has no
/// `WM_SETCURSOR` equivalent we can subclass, and winit/Slint re-apply their own cursor
/// on pointer moves — so the drag pump re-asserts it every poll via
/// [`reassert_drag_cursor`] (called from `drag/macos.rs`).
static DRAG_CURSOR_ON: AtomicBool = AtomicBool::new(false);
/// The open-hand hover cursor is currently set (transition-gated: `set_hover_cursor` is
/// called every idle tick and must not fight Slint's cursor management with redundant sets).
static HOVER_CURSOR_ON: AtomicBool = AtomicBool::new(false);

/// Borrow the NSWindow back out of the `isize` handle. The pointer stays valid for the
/// life of the Slint window (winit retains it); `0` = not realized.
fn ns_window<'a>(raw: isize) -> Option<&'a NSWindow> {
    if raw == 0 {
        return None;
    }
    MainThreadMarker::new()?;
    Some(unsafe { &*(raw as *const NSWindow) })
}

/// Pull the native NSWindow (as isize) out of a Slint window. The AppKit raw handle
/// only carries the NSView; hop to its window. 0 until the native window is realized
/// by the event loop (callers retry).
pub fn hwnd_of(win: &slint::Window) -> isize {
    if MainThreadMarker::new().is_none() {
        return 0;
    }
    let sh = win.window_handle();
    match HasWindowHandle::window_handle(&sh) {
        Ok(h) => match h.as_raw() {
            RawWindowHandle::AppKit(h) => {
                let view = h.ns_view.as_ptr() as *const NSView;
                match unsafe { (*view).window() } {
                    Some(w) => Retained::as_ptr(&w) as isize,
                    None => 0,
                }
            }
            _ => 0,
        },
        Err(_) => 0,
    }
}

/// Hidden-titlebar treatment: transparent titlebar + hidden title + content under the
/// titlebar. Keeps the native traffic lights overlaid top-left.
pub fn make_frameless(raw: isize) {
    let Some(w) = ns_window(raw) else { return };
    unsafe {
        let mask = w.styleMask() | NSWindowStyleMask::FullSizeContentView;
        w.setStyleMask(mask);
        w.setTitlebarAppearsTransparent(true);
        w.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    }
}

/// Begin a system move-drag (drag-the-bar) via `performWindowDragWithEvent:`.
///
/// The anchor event is SYNTHESIZED at the live global mouse position rather than taken
/// from `NSApp.currentEvent`: by the time the Slint callback runs, the current event's
/// location is not reliably the press on the bar (observed live: a garbage anchor that
/// teleported the window on drag). AppKit only reads the event's `locationInWindow` +
/// `windowNumber` to compute the drag anchor, so a minimal synthetic left-mouse-down at
/// the cursor anchors the drag exactly under the pointer.
pub fn start_drag(raw: isize) {
    let Some(w) = ns_window(raw) else { return };
    if MainThreadMarker::new().is_none() {
        return;
    }
    unsafe {
        let cursor = NSEvent::mouseLocation(); // cocoa global (bottom-left) points
        let frame = w.frame();
        let loc_in_win = NSPoint::new(cursor.x - frame.origin.x, cursor.y - frame.origin.y);
        let ev = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::LeftMouseDown,
            loc_in_win,
            NSEventModifierFlags::empty(),
            NSProcessInfo::processInfo().systemUptime(),
            w.windowNumber(),
            None,
            0,
            1,
            1.0,
        );
        if let Some(ev) = ev {
            w.performWindowDragWithEvent(&ev);
        }
    }
}

/// Begin forcing the tear-off drag cursor (closed hand). No mouse capture exists on
/// macOS (none is needed: the pump reads global state), so this is cursor-only; the
/// pump re-asserts it each poll (see [`reassert_drag_cursor`]).
pub fn begin_drag_cursor(_raw: isize) {
    DRAG_CURSOR_ON.store(true, Ordering::Relaxed);
    if MainThreadMarker::new().is_some() {
        unsafe {
            NSCursor::closedHandCursor().set();
        }
    }
}

/// Stop forcing the drag cursor (drop / cancel).
pub fn end_drag_cursor(_raw: isize) {
    if DRAG_CURSOR_ON.swap(false, Ordering::Relaxed) && MainThreadMarker::new().is_some() {
        unsafe {
            NSCursor::arrowCursor().set();
        }
    }
}

/// Re-assert the closed-hand cursor mid-drag (winit/Slint re-apply their own cursor on
/// every pointer move, clobbering ours). Called from the drag pump's `poll()` — i.e. at
/// the pump cadence, same trick as the Win32 subclass's `WM_MOUSEMOVE` re-assert.
pub fn reassert_drag_cursor() {
    if DRAG_CURSOR_ON.load(Ordering::Relaxed) && MainThreadMarker::new().is_some() {
        unsafe {
            NSCursor::closedHandCursor().set();
        }
    }
}

/// Show / hide the open-hand "grab" cursor while pressing a drag handle (not yet
/// dragging). Transition-gated: the pump calls `set_hover_cursor(false)` every idle tick.
pub fn set_hover_cursor(on: bool) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    if on {
        if !HOVER_CURSOR_ON.swap(true, Ordering::Relaxed) {
            unsafe {
                NSCursor::openHandCursor().set();
            }
        }
    } else if HOVER_CURSOR_ON.swap(false, Ordering::Relaxed) {
        unsafe {
            NSCursor::arrowCursor().set();
        }
    }
}

pub fn minimize(raw: isize) {
    if let Some(w) = ns_window(raw) {
        w.miniaturize(None);
    }
}

/// Zoom (the macOS maximize): toggles between the user frame and the screen-filling
/// standard frame.
pub fn toggle_max(raw: isize) {
    if let Some(w) = ns_window(raw) {
        w.zoom(None);
    }
}

/// Whether the window is currently zoomed (drives the restore-vs-maximize icon).
pub fn is_maximized(raw: isize) -> bool {
    ns_window(raw).map(|w| w.isZoomed()).unwrap_or(false)
}

/// Close the window. Unused by the managed multi-window close path (which flags the
/// window for reaping), kept for completeness of the AppKit glue.
#[allow(dead_code)]
pub fn close(raw: isize) {
    if let Some(w) = ns_window(raw) {
        w.close();
    }
}

/// Enter native fullscreen (own Space). AppKit remembers the prior frame itself, so the
/// returned placement is a unit marker.
pub fn enter_fullscreen(raw: isize) -> Option<SavedPlacement> {
    let w = ns_window(raw)?;
    if !w.styleMask().contains(NSWindowStyleMask::FullScreen) {
        w.toggleFullScreen(None);
    }
    Some(())
}

/// Leave native fullscreen (no-op if the user already left via the green button / Esc).
pub fn exit_fullscreen(raw: isize, _saved: SavedPlacement) {
    let Some(w) = ns_window(raw) else { return };
    if w.styleMask().contains(NSWindowStyleMask::FullScreen) {
        w.toggleFullScreen(None);
    }
}
