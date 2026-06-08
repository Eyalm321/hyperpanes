//! The multi-window application layer (Phase 4, Wave 1).
//!
//! The Wave-1/3 controller managed exactly one window: one [`State`] == one window's
//! tabs. This module lifts that into an app that owns a **set of windows**, each its own
//! Slint [`AppWindow`] + [`Ui`] model set + [`State`], **all sharing the one central
//! [`SessionManager`]**. Because the manager owns every PTY, a window only references
//! pane `uid`s — so a pane can be re-hosted in any window (replay-primed, no restart).
//!
//! Two things change versus the single-window controller, and only these:
//!   1. **The session event channel is drained centrally** ([`App::tick`]) and each
//!      event is routed to whichever window currently hosts that `uid` — one engine, N
//!      windows. The per-window [`paneview::pump`] no longer touches the channel.
//!   2. **Window-level [`Effect`]s** (new window / re-host / close) are applied against
//!      the window registry here, instead of against a single `AppWindow` in `main`.
//!
//! Everything else — the central [`State`] mutate→resync contract (Seam #1), [`dispatch`]
//! (Seam #2), the overlay slot (Seam #3) — is reused unchanged, per window.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use hyperpanes_core::layout::presets::DividerKind;
use hyperpanes_core::session_manager::{SessionEvent, SessionManager};
use hyperpanes_terminal_widget::encode_key;

use slint::platform::Key;
use slint::{ComponentHandle, LogicalPosition};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::command::{dispatch, set_layout_from_id, Command, Effect};
use crate::paneview::{self, Ui};
use crate::state::{DetachedPane, EscOutcome, State};
use crate::{theme, window, AppWindow, KeyMsg};

/// What a freshly-spawned window is seeded with once its area is known (the first pane
/// is sized against the real area, exactly as the single-window path did).
pub enum PendingSeed {
    /// A brand-new window → spawn one fresh interactive shell pane.
    EmptyTab,
    /// Re-host a session detached from another window (replay-primed, no PTY restart).
    Adopt(DetachedPane),
    /// Already seeded — nothing to do.
    Done,
}

/// One OS window: its realized Slint component, model set, workspace state, and the
/// per-window Win32 / geometry scratch the controller used to keep beside a single state.
pub struct Window {
    pub id: usize,
    pub app: AppWindow,
    pub ui: Rc<Ui>,
    pub state: RefCell<State>,
    /// Latest pane-area size in logical px (set by `area-resized`).
    pub area: Cell<(f32, f32)>,
    /// Native HWND (0 until the event loop realizes the window — filled in lazily).
    pub hwnd: Cell<isize>,
    /// Saved placement while in borderless OS fullscreen.
    pub saved: RefCell<Option<window::SavedPlacement>>,
    /// Deferred first-pane seeding (applied once the area is known).
    pub seed: RefCell<PendingSeed>,
    /// Set when this window should be reaped (last pane closed, or detached away).
    pub closing: Cell<bool>,
}

/// The app: the window registry + the shared session engine + the shared event stream.
pub struct App {
    pub mgr: Rc<SessionManager>,
    windows: RefCell<Vec<Rc<Window>>>,
    erx: RefCell<UnboundedReceiver<SessionEvent>>,
    next_id: Cell<usize>,
    /// True until the very first window is seeded (so the demo/screenshot env seeding
    /// applies only once, to window 0).
    first_seed: Cell<bool>,
    /// Guards the one-shot `HYPERPANES_MULTIWIN` screenshot scaffold.
    scaffold_done: Cell<bool>,
    /// Monotonic tick counter (only used to delay the screenshot scaffold).
    ticks: Cell<u64>,
}

impl App {
    pub fn new(mgr: Rc<SessionManager>, erx: UnboundedReceiver<SessionEvent>) -> Rc<Self> {
        Rc::new(App {
            mgr,
            windows: RefCell::new(Vec::new()),
            erx: RefCell::new(erx),
            next_id: Cell::new(0),
            first_seed: Cell::new(true),
            scaffold_done: Cell::new(false),
            ticks: Cell::new(0),
        })
    }

    /// Realize a new OS window, wire its callbacks to act on its own state, show it, and
    /// register it. `seed` decides its first pane (empty shell, or a re-hosted session).
    pub fn spawn_window(self: &Rc<Self>, seed: PendingSeed) {
        let aw = match AppWindow::new() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[hyperpanes] failed to create window: {e}");
                return;
            }
        };
        let ui = Ui::new();
        ui.attach(&aw);

        let id = self.next_id.get();
        self.next_id.set(id + 1);

        // The real DPI-scaled font is (re)loaded on the first pump (which owns the
        // scale); `State::new` flags `font_reload`, so a scale-1 placeholder is fine.
        let state = State::new(theme::load_font(1.0));

        let win = Rc::new(Window {
            id,
            app: aw,
            ui,
            state: RefCell::new(state),
            area: Cell::new((0.0, 0.0)),
            hwnd: Cell::new(0),
            saved: RefCell::new(None),
            seed: RefCell::new(seed),
            closing: Cell::new(false),
        });

        self.wire(&win);
        // Cascade additional windows so they don't land exactly on top of each other.
        if id > 0 {
            let off = 36.0 * id as f32;
            win.app
                .window()
                .set_position(LogicalPosition::new(140.0 + off, 90.0 + off));
        }
        let _ = win.app.show();
        self.windows.borrow_mut().push(win);
    }

    /// Look a window up by id (clones the `Rc`, releasing the registry borrow so the
    /// caller can freely borrow the window's state / spawn / reap).
    fn window_by_id(&self, id: usize) -> Option<Rc<Window>> {
        self.windows.borrow().iter().find(|w| w.id == id).cloned()
    }

    /// Run `cmd` against `win`'s state and apply any window-level [`Effect`].
    fn run_command(self: &Rc<Self>, win: &Rc<Window>, cmd: Command) {
        crate::dbg_log(&format!("cmd[{}] {cmd:?}", win.id));
        let eff = {
            let mut st = win.state.borrow_mut();
            dispatch(&mut st, cmd, &self.mgr)
        };
        crate::dbg_log(&format!("  -> effect {eff:?}"));
        match eff {
            Effect::None => {}
            Effect::Quit => win.closing.set(true),
            Effect::SetFullscreen(on) => self.set_fullscreen(win, on),
            Effect::NewWindow => self.spawn_window(PendingSeed::EmptyTab),
            Effect::MoveToNewWindow { det, source_alive } => {
                self.spawn_window(PendingSeed::Adopt(det));
                if !source_alive {
                    win.closing.set(true);
                }
            }
        }
    }

    /// Apply OS fullscreen to `win` (mirrors the single-window controller's handling).
    fn set_fullscreen(&self, win: &Rc<Window>, on: bool) {
        let raw = win.hwnd.get();
        if on {
            *win.saved.borrow_mut() = window::enter_fullscreen(raw);
        } else if let Some(s) = win.saved.borrow_mut().take() {
            window::exit_fullscreen(raw, s);
        }
        win.app.set_fullscreen_hint(on);
        if on {
            let weak = win.app.as_weak();
            slint::Timer::single_shot(Duration::from_millis(2500), move || {
                if let Some(a) = weak.upgrade() {
                    a.set_fullscreen_hint(false);
                }
            });
        }
    }

    /// Flag `win` for reaping (its sessions are killed + the window dropped on the next
    /// tick — never mid-callback, so we don't drop a component while it's dispatching).
    fn close_window(&self, win: &Rc<Window>) {
        win.closing.set(true);
    }

    // ---- the central pump (one shared 8 ms timer drives every window) ----

    /// One UI-thread tick across all windows: realize HWNDs, drain the shared session
    /// stream into the owning windows, render each window, then reap any that closed.
    pub fn tick(self: &Rc<Self>) {
        // Snapshot the registry so per-window work can borrow each window freely.
        let windows: Vec<Rc<Window>> = self.windows.borrow().clone();
        if windows.is_empty() {
            return;
        }

        // 1. Lazily strip each window's frame once its HWND exists.
        for w in &windows {
            if w.hwnd.get() == 0 {
                let raw = window::hwnd_of(w.app.window());
                if raw != 0 {
                    window::make_frameless(raw);
                    w.hwnd.set(raw);
                }
            }
        }

        // 2. Drain the ONE shared event channel, routing each event to its window.
        {
            let mut rx = self.erx.borrow_mut();
            while let Ok(ev) = rx.try_recv() {
                self.route_event(&windows, ev);
            }
        }

        // 3. Render each window from its own state.
        for w in &windows {
            self.pump_window(w);
        }

        // 3b. One-shot screenshot scaffold: after a short settle (so the shells have
        // printed their banner into the replay buffer), re-host window 0's focused pane
        // into a fresh window — proving re-host shows real replayed scrollback.
        self.ticks.set(self.ticks.get() + 1);
        if !self.scaffold_done.get()
            && self.ticks.get() > 350 // ≈2.8 s at 8 ms/tick
            && std::env::var_os("HYPERPANES_MULTIWIN").is_some()
        {
            if let Some(w0) = windows.first() {
                let ready = {
                    let st = w0.state.borrow();
                    !st.tabs.is_empty() && st.active_tab().panes.len() >= 2
                };
                if ready {
                    self.scaffold_done.set(true);
                    self.run_command(w0, Command::MovePaneToNewWindow);
                }
            }
        }

        // 4. Reap windows that asked to close; quit when the last one goes.
        if windows.iter().any(|w| w.closing.get()) {
            let mut survivors = Vec::new();
            for w in self.windows.borrow().iter() {
                if w.closing.get() {
                    for uid in w.state.borrow().session_uids() {
                        self.mgr.kill(&uid);
                    }
                    let _ = w.app.window().hide();
                } else {
                    survivors.push(w.clone());
                }
            }
            *self.windows.borrow_mut() = survivors;
            if self.windows.borrow().is_empty() {
                let _ = slint::quit_event_loop();
            }
        }
    }

    /// Route one session event to the window hosting its `uid`.
    fn route_event(&self, windows: &[Rc<Window>], ev: SessionEvent) {
        match ev {
            SessionEvent::Data { uid, data } => {
                let Some(w) = find_window(windows, &uid) else { return };
                let mut st = w.state.borrow_mut();
                if let Some((ti, pi)) = st.find_pane(&uid) {
                    let pc = &mut st.tabs[ti].panes[pi];
                    pc.pane.feed(&data);
                    let replies = pc.pane.take_replies();
                    if !replies.is_empty() {
                        self.mgr.write(&uid, &String::from_utf8_lossy(&replies));
                    }
                    if !pc.started {
                        pc.started = true;
                        if let Some(cmd) = pc.startup.take() {
                            self.mgr.write(&uid, &cmd);
                        }
                    }
                }
            }
            SessionEvent::Exit { uid, .. } => {
                if let Some(w) = find_window(windows, &uid) {
                    let alive = w.state.borrow_mut().pane_exited(&uid, &self.mgr);
                    if !alive {
                        w.closing.set(true);
                    }
                }
            }
            SessionEvent::Cwd { uid, cwd } => {
                if let Some(w) = find_window(windows, &uid) {
                    w.state.borrow_mut().note_cwd(&cwd);
                }
            }
        }
    }

    /// Seed (if pending + area known) and render one window.
    fn pump_window(self: &Rc<Self>, win: &Rc<Window>) {
        let scale = win.app.window().scale_factor().max(1.0);
        let (aw, ah) = win.area.get();

        // Apply the deferred first-pane seed once the area is known.
        if aw > 1.0 && ah > 1.0 {
            let pending = std::mem::replace(&mut *win.seed.borrow_mut(), PendingSeed::Done);
            match pending {
                PendingSeed::EmptyTab => {
                    let mut st = win.state.borrow_mut();
                    st.add_pane(&self.mgr);
                    if self.first_seed.replace(false) {
                        if std::env::var_os("HYPERPANES_DEMO").is_some() {
                            crate::demo_seed(&mut st, &self.mgr);
                        }
                        if let Some(which) = std::env::var_os("HYPERPANES_OPEN") {
                            let cmd = match which.to_string_lossy().as_ref() {
                                "palette" => Some(Command::PaletteOpen),
                                "prefs" => Some(Command::PrefsOpen),
                                "sidebar" => Some(Command::ToggleSidebar),
                                _ => None,
                            };
                            if let Some(cmd) = cmd {
                                dispatch(&mut st, cmd, &self.mgr);
                            }
                        }
                    }
                }
                PendingSeed::Adopt(det) => win.state.borrow_mut().adopt_pane(&self.mgr, det),
                PendingSeed::Done => {}
            }
        }

        if aw <= 1.0 || ah <= 1.0 {
            return;
        }
        let mut st = win.state.borrow_mut();
        paneview::pump(&win.app, &mut st, &win.ui, (aw, ah), scale, &self.mgr);
    }

    // ---- key routing for one window (app chords first, else encode to the pane) ----

    fn on_key(self: &Rc<Self>, win: &Rc<Window>, idx: usize, msg: KeyMsg) {
        // Ctrl+Shift is fully app-reserved: run the mapped command and ALWAYS swallow.
        if msg.control && msg.shift {
            if let Some(cmd) = crate::route_chord(&msg) {
                self.run_command(win, cmd);
            }
            return;
        }
        // Other modifier chords (Alt+… focus, bare F11) — run + swallow.
        if let Some(cmd) = crate::route_chord(&msg) {
            self.run_command(win, cmd);
            return;
        }
        // Escape: a tap reaches the shell; HOLDING it in fullscreen exits fullscreen.
        if crate::is_key(&msg.text, Key::Escape) {
            let outcome = win.state.borrow_mut().note_esc();
            match outcome {
                EscOutcome::Exit => {
                    self.run_command(win, Command::ToggleFullscreen);
                    return;
                }
                EscOutcome::Ignore => return,
                EscOutcome::Forward => {}
            }
        }
        // Drop bare modifiers / F-keys / special private-use keys.
        if !crate::forwardable(&msg.text) {
            return;
        }
        if let Some(bytes) = encode_key(&msg.text, msg.control, msg.alt, msg.shift) {
            let st = win.state.borrow();
            if let Some(ps) = st.active_tab().panes.get(idx) {
                self.mgr.write(&ps.uid, &String::from_utf8_lossy(&bytes));
            }
        }
    }

    /// Wire every Slint callback of `win` to operate on *that* window's state. Each
    /// closure captures the app + the window id and resolves the window per-call, so a
    /// reaped window's stale closures simply no-op.
    fn wire(self: &Rc<Self>, win: &Rc<Window>) {
        let app = self;

        // Callbacks that just run a fixed command on the window.
        macro_rules! cb0 {
            ($setter:ident, $cmd:expr) => {{
                let app = app.clone();
                let id = win.id;
                win.app.$setter(move || {
                    if let Some(w) = app.window_by_id(id) {
                        app.run_command(&w, $cmd);
                    }
                });
            }};
        }
        // Callbacks taking an int arg passed to a `fn(usize) -> Command` constructor.
        macro_rules! cb_usize {
            ($setter:ident, $ctor:expr) => {{
                let app = app.clone();
                let id = win.id;
                win.app.$setter(move |i| {
                    if let Some(w) = app.window_by_id(id) {
                        app.run_command(&w, $ctor(i as usize));
                    }
                });
            }};
        }
        // Callbacks taking an int arg passed straight to a `fn(i32) -> Command`.
        macro_rules! cb_i32 {
            ($setter:ident, $ctor:expr) => {{
                let app = app.clone();
                let id = win.id;
                win.app.$setter(move |i| {
                    if let Some(w) = app.window_by_id(id) {
                        app.run_command(&w, $ctor(i));
                    }
                });
            }};
        }

        // panes
        cb_usize!(on_focus_pane, Command::FocusPane);
        cb0!(on_new_pane, Command::NewPane);
        cb0!(on_close_focused, Command::CloseFocused);
        cb0!(on_toggle_zoom, Command::ToggleZoom);
        cb0!(on_toggle_fullscreen, Command::ToggleFullscreen);
        cb_i32!(on_set_layout, set_layout_from_id);
        cb_usize!(on_pane_close, Command::ClosePane);
        // Pane-header zoom/fullscreen act on that pane: focus it first, then the action.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_zoom(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::FocusPane(i as usize));
                    app.run_command(&w, Command::ToggleZoom);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_fullscreen(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::FocusPane(i as usize));
                    app.run_command(&w, Command::ToggleFullscreen);
                }
            });
        }

        // tabs
        cb0!(on_new_tab, Command::NewTab);
        cb_usize!(on_select_tab, Command::SwitchTab);
        cb_usize!(on_close_tab, Command::CloseTab);
        cb_i32!(on_begin_rename, Command::BeginRename);
        {
            let app = app.clone();
            let id = win.id;
            win.app
                .on_rename_tab(move |i, t| {
                    if let Some(w) = app.window_by_id(id) {
                        app.run_command(&w, Command::RenameTab(i, t.to_string()));
                    }
                });
        }

        // multi-window
        cb0!(on_new_window, Command::NewWindow);
        cb0!(on_move_pane_new_window, Command::MovePaneToNewWindow);

        // Wave-2 overlays
        cb0!(on_open_palette, Command::PaletteOpen);
        cb0!(on_open_prefs, Command::PrefsOpen);
        cb0!(on_toggle_sidebar, Command::ToggleSidebar);
        cb0!(on_palette_activate, Command::PaletteActivate);
        cb0!(on_overlay_dismiss, Command::CloseOverlay);
        cb_i32!(on_palette_nav, Command::PaletteNav);
        cb_usize!(on_palette_pick, Command::PaletteSelect);
        cb_usize!(on_open_project, Command::OpenProject);
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_palette_query(move |q| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::PaletteQuery(q.to_string()));
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pref_action(move |kind, arg| {
                let Some(w) = app.window_by_id(id) else { return };
                let setting = match kind {
                    0 => crate::state::Setting::FontFamily(arg as usize),
                    1 => crate::state::Setting::FontDelta(arg),
                    2 => crate::state::Setting::ShowFrame(arg != 0),
                    3 => crate::state::Setting::ShowDot(arg != 0),
                    _ => return,
                };
                app.run_command(&w, Command::ApplySetting(setting));
            });
        }

        // area geometry (resize → relayout)
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_area_resized(move |w_, h_| {
                if let Some(w) = app.window_by_id(id) {
                    w.area.set((w_, h_));
                    w.state.borrow_mut().dirty = true;
                }
            });
        }
        // divider drag: cursor offset from seam centre → size-fraction delta
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_divider_drag(move |index, main, vertical, dx, dy| {
                let Some(w) = app.window_by_id(id) else { return };
                let (aw, ah) = w.area.get();
                let delta = if vertical {
                    if aw > 0.0 { (dx / aw) as f64 } else { 0.0 }
                } else if ah > 0.0 {
                    (dy / ah) as f64
                } else {
                    0.0
                };
                if delta == 0.0 {
                    return;
                }
                let kind = if main { DividerKind::Main } else { DividerKind::Size };
                app.run_command(&w, Command::ResizeDivider { kind, index, delta });
            });
        }
        // key routing
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_key(move |idx, msg: KeyMsg| {
                if let Some(w) = app.window_by_id(id) {
                    app.on_key(&w, idx as usize, msg);
                }
            });
        }

        // window controls (Win32) — act on this window's HWND.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_start_drag(move || {
                if let Some(w) = app.window_by_id(id) {
                    window::start_drag(w.hwnd.get());
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_min_window(move || {
                if let Some(w) = app.window_by_id(id) {
                    window::minimize(w.hwnd.get());
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_max_window(move || {
                if let Some(w) = app.window_by_id(id) {
                    window::toggle_max(w.hwnd.get());
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_close_window(move || {
                if let Some(w) = app.window_by_id(id) {
                    app.close_window(&w);
                }
            });
        }
    }
}

/// Find the window currently hosting session `uid` (each `uid` lives in one window).
fn find_window<'a>(windows: &'a [Rc<Window>], uid: &str) -> Option<&'a Rc<Window>> {
    windows
        .iter()
        .find(|w| w.state.borrow_mut().find_pane(uid).is_some())
}
