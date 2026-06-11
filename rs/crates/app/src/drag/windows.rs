//! Win32 implementation of the global-pointer pump + the drag ghost (see `mod.rs`).
//! Moved verbatim from the old single-file `drag.rs` (lifted from `spike-tearoff`).

use core::ffi::c_void;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// The ghost never handles input (`WS_EX_TRANSPARENT`); forward everything.
/// `DefWindowProcW` is generic in windows-0.58 so it can't be a raw fn pointer —
/// this thin shim gives a concrete `extern "system"` proc.
unsafe extern "system" fn ghost_wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(h, msg, wp, lp)
}

/// [`GlobalPointer`](super::GlobalPointer) over the Win32 pump: the global cursor is
/// always readable (`GetCursorPos` + `GetAsyncKeyState`), and the pointer is trackable
/// across every window and the desktop (tear-off fully supported).
pub struct PlatformPointer;

impl super::GlobalPointer for PlatformPointer {
    fn poll(&self) -> Option<(slint::PhysicalPosition, bool)> {
        let (x, y) = cursor_pos();
        Some((slint::PhysicalPosition::new(x, y), left_button_down()))
    }
    fn supports_cross_window(&self) -> bool {
        true
    }
}

/// Screen-global cursor position (physical px). Slint exposes no equivalent.
pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

/// Is the primary (left) mouse button currently held?
pub fn left_button_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
}

/// A window's screen rect (physical px), `(left, top, right, bottom)`. `0`-rect when
/// the HWND isn't realized yet.
pub fn window_rect(raw: isize) -> (i32, i32, i32, i32) {
    let mut r = RECT::default();
    if raw != 0 {
        unsafe {
            let _ = GetWindowRect(hwnd(raw), &mut r);
        }
    }
    (r.left, r.top, r.right, r.bottom)
}

/// Transparent, click-through, always-on-top window that chases the cursor — the
/// drag "ghost". Kept entirely out of Slint's render path.
pub struct Ghost {
    hwnd: HWND,
    w: i32,
    h: i32,
}

impl Ghost {
    pub fn new() -> Ghost {
        let w = 200;
        let h = 44;
        unsafe {
            let hmod = GetModuleHandleW(None).unwrap();
            let hinst = HINSTANCE(hmod.0);
            let class = w!("HyperpanesTearoffGhost");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(ghost_wndproc),
                hInstance: hinst,
                lpszClassName: class,
                // brand green (#5ee08f) as 0x00BBGGRR.
                hbrBackground: CreateSolidBrush(COLORREF(0x008f_e05e)),
                ..Default::default()
            };
            RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                class,
                w!("ghost"),
                WS_POPUP,
                0,
                0,
                w,
                h,
                None,
                None,
                Some(hinst),
                None,
            )
            .unwrap();

            // ~78% opaque so it reads as a translucent overlay.
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 200, LWA_ALPHA);
            Ghost { hwnd, w, h }
        }
    }

    /// Move + show, offset a little below/right of the cursor hotspot.
    pub fn follow(&self, p: (i32, i32)) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                p.0 + 14,
                p.1 + 16,
                self.w,
                self.h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}
