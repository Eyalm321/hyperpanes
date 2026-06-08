//! Win32 glue for the frameless window: pull the native HWND, strip `WS_CAPTION`,
//! the `HTCAPTION` drag trick, min/max/close, and borderless OS fullscreen
//! (save/restore the window placement). Lifted + extended from `spike-tearoff`.

/// Opaque saved window placement, restored when leaving fullscreen.
#[cfg(windows)]
pub use imp::SavedPlacement;
#[cfg(not(windows))]
pub type SavedPlacement = ();

pub use imp::*;

#[cfg(windows)]
mod imp {
    use core::ffi::c_void;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DWM_WINDOW_CORNER_PREFERENCE,
    };
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows::Win32::UI::WindowsAndMessaging::*;

    #[derive(Clone, Copy)]
    pub struct SavedPlacement(WINDOWPLACEMENT);

    /// The window proc we replaced (winit's), chained to for every message we
    /// don't handle ourselves.
    static OLD_WNDPROC: AtomicIsize = AtomicIsize::new(0);

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut c_void)
    }

    /// Our subclass proc: eat the non-client frame (`WM_NCCALCSIZE`) so the client
    /// area — our Slint top bar — fills the whole window with no top gap. When
    /// maximized, clamp the client to the monitor work area so it doesn't cover the
    /// taskbar or clip off-screen. Everything else chains to winit's proc.
    unsafe extern "system" fn subclass_proc(
        h: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_NCCALCSIZE && wparam.0 != 0 {
            if IsZoomed(h).as_bool() {
                let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
                let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO {
                    cbSize: core::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(mon, &mut mi).as_bool() {
                    (*params).rgrc[0] = mi.rcWork;
                }
            }
            // Returning 0 with the rect left at the proposed window bounds makes
            // the client area == the window (no borders).
            return LRESULT(0);
        }
        let old: WNDPROC = core::mem::transmute(OLD_WNDPROC.load(Ordering::Relaxed));
        CallWindowProcW(old, h, msg, wparam, lparam)
    }

    /// Pull the native HWND (as isize) out of a Slint window. 0 until the native
    /// window is realized by the event loop (callers retry).
    pub fn hwnd_of(win: &slint::Window) -> isize {
        let sh = win.window_handle();
        match HasWindowHandle::window_handle(&sh) {
            Ok(h) => match h.as_raw() {
                RawWindowHandle::Win32(h) => h.hwnd.get(),
                _ => 0,
            },
            Err(_) => 0,
        }
    }

    /// Strip the OS title bar (`WS_CAPTION`) while keeping the resize border +
    /// min/max/sysmenu, so our Slint top bar is the only chrome.
    pub fn make_frameless(raw: isize) {
        unsafe {
            let h = hwnd(raw);
            let style = GetWindowLongPtrW(h, GWL_STYLE);
            let new = style & !(WS_CAPTION.0 as isize);
            SetWindowLongPtrW(h, GWL_STYLE, new);
            // Subclass to remove the non-client frame (kills the top gap). Install
            // before the FRAMECHANGED so our handler catches the recalc.
            if OLD_WNDPROC.load(Ordering::Relaxed) == 0 {
                let prev = SetWindowLongPtrW(h, GWLP_WNDPROC, subclass_proc as usize as isize);
                OLD_WNDPROC.store(prev, Ordering::Relaxed);
            }
            // Windows 11 rounded corners (ignored on Win10).
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                h,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
                core::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
            let _ = SetWindowPos(
                h,
                HWND::default(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
    }

    /// Begin a system move-drag (the standard frameless "drag the bar" trick).
    pub fn start_drag(raw: isize) {
        unsafe {
            let h = hwnd(raw);
            let _ = ReleaseCapture();
            SendMessageW(h, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
        }
    }

    pub fn minimize(raw: isize) {
        unsafe {
            let _ = ShowWindow(hwnd(raw), SW_MINIMIZE);
        }
    }

    pub fn toggle_max(raw: isize) {
        unsafe {
            let h = hwnd(raw);
            if IsZoomed(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            } else {
                let _ = ShowWindow(h, SW_MAXIMIZE);
            }
        }
    }

    /// Post `WM_CLOSE` to a window. Unused by the managed multi-window close path
    /// (which flags the window for reaping), kept for completeness of the Win32 glue.
    #[allow(dead_code)]
    pub fn close(raw: isize) {
        unsafe {
            let _ = PostMessageW(hwnd(raw), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    /// Cover the current monitor borderlessly, returning the prior placement.
    pub fn enter_fullscreen(raw: isize) -> Option<SavedPlacement> {
        unsafe {
            let h = hwnd(raw);
            let mut wp = WINDOWPLACEMENT {
                length: core::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            if GetWindowPlacement(h, &mut wp).is_err() {
                return None;
            }
            let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: core::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(mon, &mut mi).as_bool() {
                return Some(SavedPlacement(wp));
            }
            let RECT {
                left,
                top,
                right,
                bottom,
            } = mi.rcMonitor;
            let _ = SetWindowPos(
                h,
                HWND_TOP,
                left,
                top,
                right - left,
                bottom - top,
                SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
            );
            Some(SavedPlacement(wp))
        }
    }

    /// Restore the placement captured by [`enter_fullscreen`].
    pub fn exit_fullscreen(raw: isize, saved: SavedPlacement) {
        unsafe {
            let h = hwnd(raw);
            let _ = SetWindowPlacement(h, &saved.0);
            let _ = SetWindowPos(
                h,
                HWND::default(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn hwnd_of(_win: &slint::Window) -> isize {
        0
    }
    pub fn make_frameless(_raw: isize) {}
    pub fn start_drag(_raw: isize) {}
    pub fn minimize(_raw: isize) {}
    pub fn toggle_max(_raw: isize) {}
    pub fn close(_raw: isize) {}
    pub fn enter_fullscreen(_raw: isize) -> Option<super::SavedPlacement> {
        Some(())
    }
    pub fn exit_fullscreen(_raw: isize, _saved: super::SavedPlacement) {}
}
