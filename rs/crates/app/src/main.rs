//! `hyperpanes` — the native Slint GUI (Phase 2: single-window MVP).
//!
//! Assembles the finished pieces into a launchable, usable tiled terminal:
//!   * a frameless window + custom icon-only top bar (Win32: strip `WS_CAPTION`, drag via the
//!     `HTCAPTION` trick, min/max/close) — lifted from `rs/spikes/tearoff`;
//!   * a small central workspace state (the MVP subset of `useWorkspace.ts`): the pane list
//!     (each → a `session_manager` uid + label + accent), the active `Layout`, split `sizes`,
//!     `main_fraction`, and the `focused` index;
//!   * `core::layout::compute_tiles` → absolute logical-px rects → one
//!     `terminal_widget::TerminalPane` per visible tile, each bound to a real shell via
//!     `core::session_manager`;
//!   * key routing to the focused pane + a minimal set of Ctrl-Shift app shortcuts.
//!
//! Renderer: software (`SoftwareRenderer`) for every pane — always available, no wgpu-device
//! capture timing to get right. The widget keeps the GPU path one `set_renderer` away.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use hyperpanes_core::layout::navigate::{neighbor_index, Direction};
use hyperpanes_core::layout::presets::{compute_tiles, effective_layout, Layout};
use hyperpanes_core::layout::sizes::{insert_size, remove_size};
use hyperpanes_core::session_manager::{SessionEvent, SessionManager, SpawnOptions};
use hyperpanes_terminal_widget::{
    cells_for_px, encode_key, Font, RenderOpts, SoftwareRenderer, TerminalPane,
};

use slint::platform::Key;
use slint::{Color, ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc::unbounded_channel;

slint::include_modules!();

/// Accent palette assigned to panes in creation order (Tokyo-Night-ish).
const PALETTE: [(u8, u8, u8); 6] = [
    (0x7a, 0xa2, 0xf7), // blue
    (0x9e, 0xce, 0x6a), // green
    (0xbb, 0x9a, 0xf7), // purple
    (0xe0, 0xaf, 0x68), // amber
    (0xf7, 0x76, 0x8e), // red
    (0x7d, 0xcf, 0xff), // cyan
];

/// The cycle the layout button / Ctrl-Shift-L walks through.
const LAYOUT_CYCLE: [Layout; 5] = [
    Layout::Auto,
    Layout::Columns,
    Layout::Rows,
    Layout::Grid,
    Layout::MainStack,
];

fn layout_name(l: Layout) -> &'static str {
    match l {
        Layout::Auto => "auto",
        Layout::Single => "single",
        Layout::Columns => "columns",
        Layout::Rows => "rows",
        Layout::Grid => "grid",
        Layout::MainStack => "main-stack",
    }
}

/// One pane's controller-side state.
struct PaneState {
    uid: String,
    title: SharedString,
    accent: Color,
    pane: TerminalPane,
    /// Cell dims currently applied to the bound session (to detect a real reflow).
    applied: (usize, usize),
    /// The latest rendered terminal image (kept so the model always has a surface).
    surface: Image,
    /// Placement in logical px, recomputed on relayout.
    rect: (f32, f32, f32, f32),
    visible: bool,
    /// Whether the shell has produced its first output yet (gate the startup write).
    started: bool,
    startup: Option<String>,
}

/// The whole window's workspace state — the MVP subset of `useWorkspace.ts`.
struct State {
    font: Font,
    panes: Vec<PaneState>,
    layout: Layout,
    sizes: Vec<f64>,
    main_fraction: f64,
    focused: usize,
    next_uid: usize,
    last_blink: Instant,
    cursor_on: bool,
    /// Signature of the last applied layout — when it changes we recompute all rects.
    last_sig: String,
    frames: u32,
    last_hud: Instant,
}

impl State {
    fn accent_for(i: usize) -> Color {
        let (r, g, b) = PALETTE[i % PALETTE.len()];
        Color::from_rgb_u8(r, g, b)
    }

    /// Spawn a new pane + its shell session, append it, and focus it.
    fn add_pane(&mut self, mgr: &SessionManager) {
        let uid = format!("pane-{}", self.next_uid);
        self.next_uid += 1;

        // Spawn at a placeholder grid; the next relayout reflows it to its real tile.
        let (cols, rows) = (80u16, 24u16);
        if let Err(e) = mgr.create(SpawnOptions {
            uid: uid.clone(),
            cols: Some(cols),
            rows: Some(rows),
            pane_id: Some(uid.clone()),
            ..Default::default()
        }) {
            eprintln!("[hyperpanes] failed to spawn {uid}: {e}");
            return;
        }

        let idx = self.panes.len();
        self.sizes = insert_size(&self.sizes, idx);
        self.panes.push(PaneState {
            uid,
            title: format!("{}", idx + 1).into(),
            accent: Self::accent_for(idx),
            pane: TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new())),
            applied: (cols as usize, rows as usize),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: false,
            startup: None,
        });
        self.focused = idx;
        self.last_sig.clear(); // force relayout
    }

    /// Close pane `idx`. Returns `false` if it was the last pane (caller decides to quit).
    fn close_pane(&mut self, idx: usize, mgr: &SessionManager) -> bool {
        if idx >= self.panes.len() {
            return true;
        }
        if self.panes.len() <= 1 {
            return false;
        }
        let ps = self.panes.remove(idx);
        mgr.kill(&ps.uid);
        self.sizes = remove_size(&self.sizes, idx);

        if self.focused >= self.panes.len() {
            self.focused = self.panes.len() - 1;
        } else if idx < self.focused {
            self.focused -= 1;
        }
        // Re-label + recolor so titles/accents stay 1..N in order.
        for (i, p) in self.panes.iter_mut().enumerate() {
            p.title = format!("{}", i + 1).into();
            p.accent = Self::accent_for(i);
        }
        self.last_sig.clear();
        true
    }

    /// Move focus to the neighbour in `dir` (no-op if there is none).
    fn focus_dir(&mut self, dir: Direction) {
        let n = self.panes.len();
        let eff = effective_layout(self.layout, n);
        let tiles = compute_tiles(eff, n, &self.sizes, self.main_fraction, self.focused as i32);
        if let Some(next) = neighbor_index(&tiles, self.focused, dir) {
            self.focused = next;
            self.last_sig.clear();
        }
    }

    fn cycle_layout(&mut self) {
        let cur = LAYOUT_CYCLE.iter().position(|l| *l == self.layout).unwrap_or(0);
        self.layout = LAYOUT_CYCLE[(cur + 1) % LAYOUT_CYCLE.len()];
        self.last_sig.clear();
    }
}

/// All the Win32 glue for the frameless window. Lifted from `spike-tearoff`.
#[cfg(windows)]
mod win32 {
    use core::ffi::c_void;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut c_void)
    }

    /// Pull the native HWND (as isize) out of a Slint window. 0 until the native window is
    /// realized by the event loop (callers retry).
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

    /// Strip the OS title bar (`WS_CAPTION`) while keeping the resize border + min/max/sysmenu,
    /// so our Slint top bar is the only chrome. Native maximize/restore still behave correctly.
    pub fn make_frameless(raw: isize) {
        unsafe {
            let h = hwnd(raw);
            let style = GetWindowLongPtrW(h, GWL_STYLE);
            let new = style & !(WS_CAPTION.0 as isize);
            SetWindowLongPtrW(h, GWL_STYLE, new);
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

    /// Begin a system move-drag (the standard frameless "drag the custom bar" trick).
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

    pub fn close(raw: isize) {
        unsafe {
            let _ = PostMessageW(hwnd(raw), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

#[cfg(not(windows))]
mod win32 {
    pub fn hwnd_of(_win: &slint::Window) -> isize {
        0
    }
    pub fn make_frameless(_raw: isize) {}
    pub fn start_drag(_raw: isize) {}
    pub fn minimize(_raw: isize) {}
    pub fn toggle_max(_raw: isize) {}
    pub fn close(_raw: isize) {}
}

fn load_font(scale: f32) -> Font {
    let px = (14.0 * scale).round().max(8.0);
    let candidates = [
        "C:/Windows/Fonts/CascadiaMono.ttf",
        "C:/Windows/Fonts/CascadiaCode.ttf",
        "C:/Windows/Fonts/consola.ttf",
    ];
    let path = candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .copied()
        .unwrap_or("C:/Windows/Fonts/consola.ttf");
    Font::from_path(path, px).expect("load monospace font")
}

/// Build a model row for pane `i`.
fn pane_item(ps: &PaneState, focused: bool) -> PaneItem {
    let (x, y, w, h) = ps.rect;
    PaneItem {
        surface: ps.surface.clone(),
        title: ps.title.clone(),
        accent: ps.accent,
        x,
        y,
        w,
        h,
        visible: ps.visible,
        focused,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tokio runtime that drives the SessionManager's per-session driver tasks.
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    // session manager + its event stream.
    let (etx, erx) = unbounded_channel::<SessionEvent>();
    let mgr = Rc::new(SessionManager::new(etx));
    let erx = Rc::new(RefCell::new(erx));

    let app = AppWindow::new()?;
    let model: Rc<VecModel<PaneItem>> = Rc::new(VecModel::default());
    app.set_panes(ModelRc::from(model.clone()));

    let state: Rc<RefCell<Option<State>>> = Rc::new(RefCell::new(None));
    let area: Rc<RefCell<(f32, f32)>> = Rc::new(RefCell::new((0.0, 0.0)));
    let hwnd: Rc<RefCell<isize>> = Rc::new(RefCell::new(0));

    // ---- area geometry ----
    {
        let area = area.clone();
        app.on_area_resized(move |w, h| *area.borrow_mut() = (w, h));
    }

    // ---- click-to-focus ----
    {
        let state = state.clone();
        app.on_focus_pane(move |idx| {
            if let Some(st) = state.borrow_mut().as_mut() {
                let idx = idx as usize;
                if idx < st.panes.len() && st.focused != idx {
                    st.focused = idx;
                    st.last_sig.clear();
                }
            }
        });
    }

    // ---- key routing: app shortcuts first, else encode to the focused pane's pty ----
    {
        let state = state.clone();
        let mgr = mgr.clone();
        let app_weak = app.as_weak();
        app.on_key(move |idx, msg: KeyMsg| {
            let idx = idx as usize;
            // Ctrl+Shift app shortcuts (intercepted; never forwarded to the shell).
            if msg.control && msg.shift {
                let is = |k: Key| -> bool {
                    let s: SharedString = k.into();
                    msg.text == s
                };
                let mut handled = true;
                if let Some(st) = state.borrow_mut().as_mut() {
                    if is(Key::LeftArrow) {
                        st.focus_dir(Direction::Left);
                    } else if is(Key::RightArrow) {
                        st.focus_dir(Direction::Right);
                    } else if is(Key::UpArrow) {
                        st.focus_dir(Direction::Up);
                    } else if is(Key::DownArrow) {
                        st.focus_dir(Direction::Down);
                    } else {
                        let c = msg.text.chars().next().map(|c| c.to_ascii_lowercase());
                        match c {
                            Some('t') => st.add_pane(&mgr),
                            Some('l') => st.cycle_layout(),
                            Some('w') => {
                                let f = st.focused;
                                if !st.close_pane(f, &mgr) {
                                    if let Some(a) = app_weak.upgrade() {
                                        let _ = a.window().hide();
                                    }
                                }
                            }
                            _ => handled = false,
                        }
                    }
                }
                if handled {
                    return;
                }
            }

            // Otherwise: forward to that pane's shell.
            if let Some(bytes) = encode_key(&msg.text, msg.control, msg.alt, msg.shift) {
                if let Some(st) = state.borrow().as_ref() {
                    if let Some(ps) = st.panes.get(idx) {
                        mgr.write(&ps.uid, &String::from_utf8_lossy(&bytes));
                    }
                }
            }
        });
    }

    // ---- top-bar tools ----
    {
        let state = state.clone();
        let mgr = mgr.clone();
        app.on_new_pane(move || {
            if let Some(st) = state.borrow_mut().as_mut() {
                st.add_pane(&mgr);
            }
        });
    }
    {
        let state = state.clone();
        let app_weak = app.as_weak();
        app.on_cycle_layout(move || {
            if let Some(st) = state.borrow_mut().as_mut() {
                st.cycle_layout();
                if let Some(a) = app_weak.upgrade() {
                    a.set_layout_name(layout_name(st.layout).into());
                }
            }
        });
    }
    {
        let state = state.clone();
        let mgr = mgr.clone();
        let app_weak = app.as_weak();
        app.on_close_focused(move || {
            if let Some(st) = state.borrow_mut().as_mut() {
                let f = st.focused;
                if !st.close_pane(f, &mgr) {
                    if let Some(a) = app_weak.upgrade() {
                        let _ = a.window().hide();
                    }
                }
            }
        });
    }

    // ---- window controls (Win32) ----
    {
        let hwnd = hwnd.clone();
        app.on_start_drag(move || win32::start_drag(*hwnd.borrow()));
    }
    {
        let hwnd = hwnd.clone();
        app.on_min_window(move || win32::minimize(*hwnd.borrow()));
    }
    {
        let hwnd = hwnd.clone();
        app.on_max_window(move || win32::toggle_max(*hwnd.borrow()));
    }
    {
        let hwnd = hwnd.clone();
        let app_weak = app.as_weak();
        app.on_close_window(move || {
            win32::close(*hwnd.borrow());
            if let Some(a) = app_weak.upgrade() {
                let _ = a.window().hide();
            }
        });
    }

    // ---- the render / pump loop (8 ms Slint timer on the UI thread) ----
    let timer = slint::Timer::default();
    let app_weak = app.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(8), {
        let state = state.clone();
        let area = area.clone();
        let hwnd = hwnd.clone();
        let model = model.clone();
        let mgr = mgr.clone();
        let erx = erx.clone();
        move || {
            let app = match app_weak.upgrade() {
                Some(a) => a,
                None => return,
            };
            let scale = app.window().scale_factor().max(1.0);

            // Lazily realize the native HWND + strip the frame, once.
            {
                let mut h = hwnd.borrow_mut();
                if *h == 0 {
                    let raw = win32::hwnd_of(app.window());
                    if raw != 0 {
                        win32::make_frameless(raw);
                        *h = raw;
                    }
                }
            }

            // Lazy init: wait for the first real area layout, then spawn pane 0.
            if state.borrow().is_none() {
                let (aw, ah) = *area.borrow();
                if aw <= 1.0 || ah <= 1.0 {
                    return;
                }
                let mut st = State {
                    font: load_font(scale),
                    panes: Vec::new(),
                    layout: Layout::Auto,
                    sizes: Vec::new(),
                    main_fraction: 0.6,
                    focused: 0,
                    next_uid: 0,
                    last_blink: Instant::now(),
                    cursor_on: true,
                    last_sig: String::new(),
                    frames: 0,
                    last_hud: Instant::now(),
                };
                st.add_pane(&mgr);
                *state.borrow_mut() = Some(st);
                app.set_layout_name(layout_name(Layout::Auto).into());
            }

            let mut guard = state.borrow_mut();
            let st = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };

            // ---- drain session events into the panes ----
            {
                let mut rx = erx.borrow_mut();
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        SessionEvent::Data { uid, data } => {
                            if let Some(i) = st.panes.iter().position(|p| p.uid == uid) {
                                let pc = &mut st.panes[i];
                                pc.pane.feed(&data);
                                let replies = pc.pane.take_replies();
                                if !replies.is_empty() {
                                    mgr.write(&uid, &String::from_utf8_lossy(&replies));
                                }
                                if !pc.started {
                                    pc.started = true;
                                    if let Some(cmd) = pc.startup.take() {
                                        mgr.write(&uid, &cmd);
                                    }
                                }
                            }
                        }
                        SessionEvent::Exit { uid, .. } => {
                            // The driver removes the session; drop the pane from the workspace.
                            if let Some(i) = st.panes.iter().position(|p| p.uid == uid) {
                                if st.panes.len() > 1 {
                                    st.close_pane(i, &mgr);
                                }
                            }
                        }
                        SessionEvent::Cwd { .. } => {}
                    }
                }
            }

            let (aw, ah) = *area.borrow();
            let n = st.panes.len();

            // ---- relayout (only when the layout signature changes) ----
            let eff = effective_layout(st.layout, n);
            let sizes_sig: String = st.sizes.iter().map(|s| format!("{:.3},", s)).collect();
            let sig = format!(
                "{}|{}|{}|{}|{:.0}x{:.0}|{}",
                st.layout as u8, eff as u8, n, st.focused, aw, ah, sizes_sig
            );
            if sig != st.last_sig {
                st.last_sig = sig;
                let tiles =
                    compute_tiles(eff, n, &st.sizes, st.main_fraction, st.focused as i32);
                let cw = st.font.cell_w;
                let ch = st.font.cell_h;
                // Reset placement.
                for p in st.panes.iter_mut() {
                    p.visible = false;
                }
                for t in &tiles {
                    let i = t.index;
                    let x = (t.rect.x * aw as f64) as f32;
                    let y = (t.rect.y * ah as f64) as f32;
                    let w = (t.rect.w * aw as f64) as f32;
                    let h = (t.rect.h * ah as f64) as f32;
                    st.panes[i].rect = (x, y, w, h);
                    st.panes[i].visible = t.visible;
                    if t.visible {
                        let (cols, rows) =
                            cells_for_px(w * scale, h * scale, cw, ch);
                        if (cols, rows) != st.panes[i].applied {
                            if st.panes[i].pane.resize(cols, rows) {
                                mgr.resize(&st.panes[i].uid, cols as u16, rows as u16);
                            }
                            st.panes[i].applied = (cols, rows);
                        }
                    }
                }

                // Sync the model length, then write every row with its fresh rect.
                while model.row_count() > n {
                    model.remove(model.row_count() - 1);
                }
                while model.row_count() < n {
                    model.push(pane_item(&st.panes[model.row_count()], false));
                }
                let focused = st.focused;
                for i in 0..n {
                    model.set_row_data(i, pane_item(&st.panes[i], i == focused));
                }
            }

            // ---- cursor blink (~530 ms) ----
            let blink_changed = if st.last_blink.elapsed() >= Duration::from_millis(530) {
                st.cursor_on = !st.cursor_on;
                st.last_blink = Instant::now();
                true
            } else {
                false
            };
            let opts = RenderOpts {
                cursor_on: st.cursor_on,
            };

            // ---- render dirty (visible) panes → model ----
            let focused = st.focused;
            let State { font, panes, .. } = &mut *st;
            let mut rendered = false;
            for (i, ps) in panes.iter_mut().enumerate() {
                if !ps.visible {
                    let _ = ps.pane.take_dirty(); // keep the flag from piling up
                    continue;
                }
                let focus_blink = i == focused && blink_changed;
                if !ps.pane.take_dirty() && !focus_blink {
                    continue;
                }
                ps.surface = ps.pane.render(font, &opts);
                if i < model.row_count() {
                    model.set_row_data(i, pane_item(ps, i == focused));
                }
                rendered = true;
            }
            if rendered {
                st.frames += 1;
            }

            // ---- HUD ----
            if st.last_hud.elapsed() >= Duration::from_millis(500) {
                let fps = st.frames as f32 / st.last_hud.elapsed().as_secs_f32();
                app.set_hud(
                    format!("{} · {} panes · {:.0} fps", layout_name(st.layout), n, fps).into(),
                );
                app.set_tab_title(format!("shell · {n}").into());
                st.frames = 0;
                st.last_hud = Instant::now();
            }
        }
    });

    app.run()?;
    mgr.kill_all();
    Ok(())
}
