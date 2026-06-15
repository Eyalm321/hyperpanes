//! Spike B — cross-window live tear-off (Phase 0, go/no-go).
//! Throwaway harness owned entirely by track `spike-tearoff`.
//!
//! Architecture (the thing being proven):
//!   * Two Slint top-level windows (`AppWindow` instantiated twice). Each shows a
//!     vertical stack of "pane cards"; a `TouchArea` on each card reports the press.
//!   * On press we DO NOT lean on Slint's pointer delivery (which is per-window and
//!     loses the grab the moment the cursor crosses into the *other* window — exactly
//!     the risk this spike exists to test). Instead we start an 8 ms Slint timer that
//!     drives the whole drag from **global Win32 state**:
//!         - `GetCursorPos`      -> screen-global cursor (Slint has no global cursor API)
//!         - `GetAsyncKeyState`  -> left-button-still-down / released (drag end)
//!         - `GetWindowRect`     -> hit-test the cursor against each window's screen rect
//!   * Once the cursor leaves the source window we show a transparent / click-through /
//!     always-on-top **ghost** window (pure Win32 layered window) and move it to the
//!     cursor every tick. The target window highlights ("stitch" indicator) when the
//!     cursor is over it.
//!   * On release over the other window we reparent: remove the card from the source
//!     model and push it into the target model.
//!
//! Win32 pieces required are all centralised in the `win32` mod below + `hwnd_of`.
//! Full go/no-go criteria in FANOUT-HANDOFF.md / RESULTS.md.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, Model, PhysicalPosition, SharedString, VecModel, Weak};

slint::slint! {
    // Self-contained component, instantiated twice (source + target window).
    export component AppWindow inherits Window {
        title: "Tear-off spike";
        default-font-size: 14px;
        preferred-width: 340px;
        preferred-height: 460px;

        in property <string> win-name: "window";
        in property <[string]> tabs;
        in property <bool> drop-active;           // cursor is hovering this window mid-drag
        callback grab(int);                        // a card was pressed (index)

        background: drop-active ? #15301f : #1d1d22;

        VerticalLayout {
            padding: 14px;
            spacing: 10px;

            Text {
                text: root.win-name + (root.drop-active ? "   ⟶ drop to stitch" : "");
                color: root.drop-active ? #3ad07a : #cfcfd6;
                font-size: 18px;
            }

            Rectangle {
                border-width: root.drop-active ? 3px : 1px;
                border-color: root.drop-active ? #3ad07a : #34343c;
                border-radius: 8px;

                VerticalLayout {
                    padding: 8px;
                    spacing: 8px;
                    alignment: start;

                    for tab[idx] in root.tabs: Rectangle {
                        height: 46px;
                        background: ta.pressed ? #5a5a72 : #2b2b36;
                        border-radius: 6px;

                        HorizontalLayout {
                            padding-left: 12px;
                            alignment: start;
                            Text {
                                text: tab;
                                color: white;
                                vertical-alignment: center;
                            }
                        }

                        ta := TouchArea {
                            pointer-event(ev) => {
                                if (ev.kind == PointerEventKind.down) {
                                    root.grab(idx);
                                }
                            }
                        }
                    }
                }
            }

            Text {
                text: "drag a card out of this window and across to the other one";
                color: #6a6a76;
                font-size: 11px;
                wrap: word-wrap;
            }
        }
    }
}

/// All the Win32 glue, kept in one place so RESULTS can point at it.
mod win32 {
    use core::ffi::c_void;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::core::w;
    use windows::Win32::Foundation::{
        COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut c_void)
    }

    /// The ghost never handles input (it's WS_EX_TRANSPARENT); just forward everything.
    /// `DefWindowProcW` is generic in windows-0.58 so it can't be used as a raw fn
    /// pointer directly — this thin shim gives us a concrete `extern "system"` proc.
    unsafe extern "system" fn ghost_wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        DefWindowProcW(h, msg, wp, lp)
    }

    /// Pull the native HWND (as isize) out of a Slint window via raw-window-handle 0.6.
    /// `slint::Window::window_handle()` returns a `slint::WindowHandle`, which in turn
    /// implements the rwh `HasWindowHandle` trait we use to reach the raw HWND.
    /// Returns 0 until the native window is realized (the handle is `NotSupported`
    /// before the event loop creates the winit window), so callers retry.
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

    /// Screen-global cursor — Slint exposes no equivalent.
    pub fn cursor_pos() -> POINT {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        p
    }

    /// Is the primary (left) mouse button currently held?
    pub fn left_button_down() -> bool {
        unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
    }

    pub fn window_rect(raw: isize) -> RECT {
        let mut r = RECT::default();
        if raw != 0 {
            unsafe {
                let _ = GetWindowRect(hwnd(raw), &mut r);
            }
        }
        r
    }

    pub fn point_in(p: &POINT, r: &RECT) -> bool {
        p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
    }

    /// Transparent, click-through, always-on-top window that chases the cursor.
    pub struct Ghost {
        hwnd: HWND,
        w: i32,
        h: i32,
    }

    impl Ghost {
        pub fn new() -> Ghost {
            let w = 180;
            let h = 48;
            unsafe {
                let hmod = GetModuleHandleW(None).unwrap();
                let hinst = HINSTANCE(hmod.0);
                let class = w!("SpikeTearoffGhost");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(ghost_wndproc),
                    hInstance: hinst,
                    lpszClassName: class,
                    // bright teal so it's obvious; 0x00BBGGRR
                    hbrBackground: CreateSolidBrush(COLORREF(0x00d0_a04a)),
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
                    hinst,
                    None,
                )
                .unwrap();

                // ~74% opaque so you can see it's a translucent overlay.
                let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 190, LWA_ALPHA);

                Ghost { hwnd, w, h }
            }
        }

        /// Move + show, centring the ghost a little below/right of the cursor hotspot.
        pub fn follow(&self, p: &POINT) {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    p.x + 14,
                    p.y + 16,
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
}

struct Drag {
    from: usize, // window index the card was grabbed from
    label: SharedString,
}

struct AppState {
    win: [Weak<AppWindow>; 2],
    hwnd: [isize; 2],
    model: [Rc<VecModel<SharedString>>; 2],
    ghost: win32::Ghost,
    drag: Option<Drag>,
    armed: bool, // saw the button held during this drag (debounce a stale release)
}

impl AppState {
    fn begin_drag(&mut self, from: usize, idx: usize) {
        if let Some(label) = self.model[from].row_data(idx) {
            self.drag = Some(Drag { from, label });
            self.armed = false;
        }
    }

    /// HWNDs aren't valid until the native windows are realized by the event loop,
    /// so we fill them in lazily on the first ticks (0 == not yet known).
    fn ensure_hwnds(&mut self) {
        for i in 0..2 {
            if self.hwnd[i] == 0 {
                if let Some(w) = self.win[i].upgrade() {
                    self.hwnd[i] = win32::hwnd_of(w.window());
                }
            }
        }
    }

    fn set_drop_active(&self, win: usize, active: bool) {
        if let Some(w) = self.win[win].upgrade() {
            w.set_drop_active(active);
        }
    }

    /// Driven every 8 ms by a Slint timer. No-ops unless a drag is in flight.
    fn tick(st: &Rc<RefCell<AppState>>) {
        let mut s = st.borrow_mut();
        s.ensure_hwnds();
        let Some(drag) = s.drag.as_ref().map(|d| Drag {
            from: d.from,
            label: d.label.clone(),
        }) else {
            return;
        };

        let from = drag.from;
        let other = 1 - from;

        let p = win32::cursor_pos();
        let down = win32::left_button_down();

        // Debounce: only treat "button up" as a release once we've actually observed
        // it held (guards against a single stale poll right after the grab).
        if down {
            s.armed = true;
        }

        let from_rect = win32::window_rect(s.hwnd[from]);
        let other_rect = win32::window_rect(s.hwnd[other]);
        let out_of_source = !win32::point_in(&p, &from_rect);
        let over_other = win32::point_in(&p, &other_rect);

        // Ghost appears only once the drag has left the source window.
        if out_of_source {
            s.ghost.follow(&p);
        } else {
            s.ghost.hide();
        }

        s.set_drop_active(other, over_other && out_of_source);
        s.set_drop_active(from, false);

        if s.armed && !down {
            // ---- release: finalise ----
            s.ghost.hide();
            s.set_drop_active(other, false);

            if over_other && out_of_source {
                // Reparent: pull the card out of the source model, push into target.
                let from_model = s.model[from].clone();
                let other_model = s.model[other].clone();
                let mut removed = None;
                for r in 0..from_model.row_count() {
                    if from_model.row_data(r).as_deref() == Some(drag.label.as_str()) {
                        removed = Some(from_model.remove(r));
                        break;
                    }
                }
                if let Some(lbl) = removed {
                    other_model.push(lbl);
                }
            }
            s.drag = None;
            s.armed = false;
        }
    }
}

fn main() {
    let win0 = AppWindow::new().unwrap();
    let win1 = AppWindow::new().unwrap();
    win0.set_win_name("Window A  ·  source".into());
    win1.set_win_name("Window B  ·  target".into());

    let m0: Rc<VecModel<SharedString>> = Rc::new(VecModel::from(vec![
        SharedString::from("Pane 1 · top"),
        SharedString::from("Pane 2 · logs"),
        SharedString::from("Pane 3 · shell"),
    ]));
    let m1: Rc<VecModel<SharedString>> =
        Rc::new(VecModel::from(vec![SharedString::from("Pane 4 · editor")]));
    win0.set_tabs(m0.clone().into());
    win1.set_tabs(m1.clone().into());

    win0.show().unwrap();
    win1.show().unwrap();
    // Place side-by-side with a gap so the ghost has open screen to traverse.
    win0.window().set_position(PhysicalPosition::new(160, 180));
    win1.window().set_position(PhysicalPosition::new(680, 180));

    let state = Rc::new(RefCell::new(AppState {
        win: [win0.as_weak(), win1.as_weak()],
        hwnd: [0, 0], // filled in lazily once the native windows exist (see ensure_hwnds)
        model: [m0, m1],
        ghost: win32::Ghost::new(),
        drag: None,
        armed: false,
    }));

    // Wire the grab callbacks (one per window, capturing its index).
    {
        let st = state.clone();
        win0.on_grab(move |idx| st.borrow_mut().begin_drag(0, idx as usize));
    }
    {
        let st = state.clone();
        win1.on_grab(move |idx| st.borrow_mut().begin_drag(1, idx as usize));
    }

    // Global-cursor drag pump. 8 ms ≈ 125 Hz so the ghost keeps up with the cursor.
    let timer = slint::Timer::default();
    {
        let st = state.clone();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(8),
            move || AppState::tick(&st),
        );
    }

    slint::run_event_loop().unwrap();
    // keep the timer alive for the whole loop
    drop(timer);
}
