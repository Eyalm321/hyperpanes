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
use crate::drag::{self, DragKind, DragState, Hover};
use crate::paneview::{self, Ui};
use crate::state::{DetachedPane, DetachedTab, EscOutcome, State};
use crate::{theme, window, AppWindow, KeyMsg};

/// Logical height of the top bar (where a tab-strip drop lands). Hidden in fullscreen,
/// so the pane area then starts at the window's top edge.
const TOPBAR_H: f32 = 32.0;

/// Logical height of a pane's header (its drag handle) — matches the 26px header in
/// `paneview.slint`. Used to show the open-hand cursor only over the handle, not the body.
const HEADER_BAND: f32 = 26.0;

/// What a freshly-spawned window is seeded with once its area is known (the first pane
/// is sized against the real area, exactly as the single-window path did).
pub enum PendingSeed {
    /// A brand-new window → spawn one fresh interactive shell pane.
    EmptyTab,
    /// Re-host a session detached from another window (replay-primed, no PTY restart).
    Adopt(DetachedPane),
    /// Re-host a whole tab (its panes + title/layout) detached from another window.
    AdoptTab(DetachedTab),
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
    /// Per-tab strip geometry (window-logical `x`, `width`), reported by the UI and used
    /// to hit-test a tab-strip drop / reorder caret. Index = tab order.
    pub tab_geom: RefCell<Vec<(f32, f32)>>,
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
    /// The in-flight drag/tear-off gesture, if any (driven by [`App::pump_drag`]).
    drag: RefCell<Option<DragState>>,
    /// The Win32 ghost window that chases the cursor mid tear-off (lazily created on the
    /// first drag, then reused — it's a heavyweight HWND).
    ghost: RefCell<Option<drag::Ghost>>,
    /// Set while previews are painted, so they're cleared exactly once when a drag ends.
    preview_on: Cell<bool>,
    /// True while the global tear-off drag cursor + mouse capture are engaged (so we
    /// begin/end them exactly once per drag).
    drag_capture: Cell<bool>,
    /// Spring-load tracker: `(window id, hovered tab index, since)`. When a pane drag rests
    /// over the same tab past [`SPRING_DELAY`], that window switches to the tab so the pane
    /// can be dropped into its layout.
    spring: RefCell<Option<(usize, usize, std::time::Instant)>>,
}

/// How long a pane drag must rest over a tab before it springs open (Chrome/Finder).
const SPRING_DELAY: std::time::Duration = std::time::Duration::from_millis(450);

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
            drag: RefCell::new(None),
            ghost: RefCell::new(None),
            preview_on: Cell::new(false),
            drag_capture: Cell::new(false),
            spring: RefCell::new(None),
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
            tab_geom: RefCell::new(Vec::new()),
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
            Effect::MoveTabToNewWindow { tab, source_alive } => {
                self.spawn_window(PendingSeed::AdoptTab(tab));
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

        // 1. Lazily strip each window's frame once its HWND exists; keep the maximize/
        //    restore icon in sync with the OS maximized state.
        for w in &windows {
            if w.hwnd.get() == 0 {
                let raw = window::hwnd_of(w.app.window());
                if raw != 0 {
                    window::make_frameless(raw);
                    w.hwnd.set(raw);
                }
            }
            w.app.set_win_maximized(window::is_maximized(w.hwnd.get()));
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

        // 3a. Drive any in-flight drag/tear-off from the global cursor (ghost + previews;
        // resolves the drop on release). No-ops when nothing is being dragged.
        self.pump_drag(&windows);

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
                    // Sniff the shell's OSC window title so the idle glow can tell an agent
                    // pane (claude, etc.) from a plain shell (app-side; no core change).
                    if let Some(title) = crate::glow::sniff_osc_title(&data) {
                        pc.shell_title = title;
                    }
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
                    let mut st = w.state.borrow_mut();
                    // Resolve clickable paths relative to this pane's live directory.
                    if let Some((ti, pi)) = st.find_pane(&uid) {
                        st.tabs[ti].panes[pi].pane.set_cwd(Some(cwd.clone()));
                    }
                    // Refresh the remembered-projects list AND tint THIS pane if its cwd is
                    // inside a git repo (project color + frame/dot on + project-name label).
                    st.note_pane_cwd(&uid, &cwd);
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
                                // The rail is persistent; open its projects flyout so a
                                // screenshot exercises the full surface.
                                "sidebar" => Some(Command::ToggleProjects),
                                _ => None,
                            };
                            if let Some(cmd) = cmd {
                                dispatch(&mut st, cmd, &self.mgr);
                            }
                        }
                    }
                }
                PendingSeed::Adopt(det) => win.state.borrow_mut().adopt_pane(&self.mgr, det),
                PendingSeed::AdoptTab(det) => win.state.borrow_mut().adopt_tab(&self.mgr, det),
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
            let cmd = crate::route_chord(&win.state.borrow().keymap, &msg);
            if let Some(cmd) = cmd {
                self.run_command(win, cmd);
            }
            return;
        }
        // Other modifier chords (Alt+… focus, bare F11) — run + swallow.
        let cmd = crate::route_chord(&win.state.borrow().keymap, &msg);
        if let Some(cmd) = cmd {
            self.run_command(win, cmd);
            return;
        }
        // Escape while an overlay is open closes it; Preferences routes through the
        // appearance save/discard guard (so unsaved edits prompt) rather than reaching the shell.
        if crate::is_key(&msg.text, Key::Escape) && win.state.borrow().overlay_open() {
            self.run_command(win, Command::CloseOverlay);
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

    // ---- drag / tear-off pump ----

    /// A pane header was pressed: snapshot the pane (uid + chrome) and arm a drag from the
    /// current global cursor. Until the cursor moves past the threshold this is just a
    /// pending click (the header's own `clicked` still focuses the pane).
    fn begin_pane_drag(self: &Rc<Self>, win: &Rc<Window>, pane_idx: usize) {
        let uid = win
            .state
            .borrow()
            .active_tab()
            .panes
            .get(pane_idx)
            .map(|p| p.uid.clone());
        if let Some(uid) = uid {
            crate::dbg_log(&format!("pane-grab win={} idx={} uid={}", win.id, pane_idx, uid));
            let mut ds = DragState::new(
                win.id,
                DragKind::Pane { uid },
                drag::cursor_pos(),
            );
            // The button is down right now (this fires on pointer-down); arm immediately so
            // a click faster than one tick still resolves its release.
            ds.armed = drag::left_button_down();
            *self.drag.borrow_mut() = Some(ds);
        }
    }

    /// A tab was pressed: arm an in-window tab-reorder drag from the global cursor.
    fn begin_tab_drag(self: &Rc<Self>, win: &Rc<Window>, tab_idx: usize) {
        if tab_idx >= win.state.borrow().tabs.len() {
            return;
        }
        crate::dbg_log(&format!("tab-grab win={} idx={}", win.id, tab_idx));
        // Select the grabbed tab so it's visually distinct (active chip) while dragging.
        win.state.borrow_mut().switch_tab(tab_idx);
        let mut ds = DragState::new(
            win.id,
            DragKind::Tab { index: tab_idx },
            drag::cursor_pos(),
        );
        ds.armed = drag::left_button_down();
        *self.drag.borrow_mut() = Some(ds);
    }

    /// Drive an in-flight drag from the global cursor: promote past the threshold, follow
    /// with the ghost once the cursor leaves the source window, paint drop previews, and
    /// resolve the drop on button release. No-op when nothing is being dragged.
    fn pump_drag(self: &Rc<Self>, windows: &[Rc<Window>]) {
        if self.drag.borrow().is_none() {
            if self.preview_on.replace(false) {
                self.clear_previews(windows);
            }
            if self.drag_capture.replace(false) {
                window::end_drag_cursor(0); // defensive: release a stray capture/cursor
            }
            // Idle: normal cursor. The hand only appears once you actually press a handle
            // (no open hand on mere hover).
            window::set_hover_cursor(false);
            return;
        }

        let cursor = drag::cursor_pos();
        let down = drag::left_button_down();

        // Update arm/threshold state; read out what the rest of the tick needs.
        let (source_id, is_pane, active, released) = {
            let mut guard = self.drag.borrow_mut();
            let d = guard.as_mut().unwrap();
            if down {
                d.armed = true;
            }
            if !d.active {
                let dx = (cursor.0 - d.origin.0).abs();
                let dy = (cursor.1 - d.origin.1).abs();
                if dx.max(dy) >= drag::DRAG_THRESHOLD_PX {
                    d.active = true;
                }
            }
            (d.source_win, d.is_pane(), d.active, d.armed && !down)
        };

        if released {
            // Finalise: resolve the drop (only if it ever became a real drag), then reset.
            let d = self.drag.borrow_mut().take().unwrap();
            if d.active {
                let hover = self.compute_hover(windows, cursor);
                crate::dbg_log(&format!(
                    "drag release: source_win={} pane={} hover{{win={:?} strip={} pane_idx={:?} slot={} tab_slot={}}}",
                    d.source_win, d.is_pane(), hover.win, hover.over_strip, hover.pane_idx,
                    hover.slot_index, hover.tab_slot
                ));
                self.apply_drop(windows, &d, &hover, cursor);
            } else {
                crate::dbg_log("drag release: was a plain click (never crossed threshold)");
            }
            if let Some(g) = self.ghost.borrow().as_ref() {
                g.hide();
            }
            if self.drag_capture.replace(false) {
                let raw = self.window_by_id(d.source_win).map(|w| w.hwnd.get()).unwrap_or(0);
                window::end_drag_cursor(raw);
            }
            window::set_hover_cursor(false); // back to the normal cursor on release
            *self.spring.borrow_mut() = None;
            if self.preview_on.replace(false) {
                self.clear_previews(windows);
            }
            return;
        }

        if !active {
            // Pressed a handle but not yet moved past the threshold (drag start): show the
            // open hand. It becomes the closed grabbing hand the moment the drag engages.
            window::set_hover_cursor(true);
            return;
        }

        let hover = self.compute_hover(windows, cursor);

        // Live tab reorder: a tab drag rearranges its siblings in real time (Chrome-style),
        // so the dragged tab visibly slides between its neighbours instead of only showing a
        // caret. The drop on release then needs no further work.
        if !is_pane && hover.win == Some(source_id) && hover.over_strip {
            if let Some(src) = self.window_by_id(source_id) {
                let mut guard = self.drag.borrow_mut();
                if let Some(DragState { kind: DragKind::Tab { index, .. }, .. }) = guard.as_mut() {
                    let cur = *index;
                    let slot = hover.tab_slot;
                    let dest = if slot > cur { slot - 1 } else { slot };
                    if dest != cur {
                        src.state.borrow_mut().reorder_tab(cur, slot);
                        *index = dest;
                    }
                }
            }
        }

        // Engage the grabbing cursor + mouse capture once, for the whole drag.
        if !self.drag_capture.get() {
            if let Some(src) = self.window_by_id(source_id) {
                window::begin_drag_cursor(src.hwnd.get());
                window::set_hover_cursor(false); // drop the open hand → grabbing takes over
                self.drag_capture.set(true);
            }
        }

        // Spring-load: a pane resting over a tab switches that window to the tab after a
        // short hold, so the pane can be dropped into that tab's layout at a precise slot.
        if is_pane {
            let mut sp = self.spring.borrow_mut();
            match (hover.win, hover.over_strip, hover.tab_over) {
                (Some(win), true, Some(ti)) => match *sp {
                    Some((w, t, since)) if w == win && t == ti => {
                        if since.elapsed() >= SPRING_DELAY {
                            if let Some(tw) = self.window_by_id(win) {
                                if tw.state.borrow().active != ti {
                                    tw.state.borrow_mut().switch_tab(ti);
                                }
                            }
                            *sp = Some((win, ti, std::time::Instant::now())); // re-arm
                        }
                    }
                    _ => *sp = Some((win, ti, std::time::Instant::now())),
                },
                _ => *sp = None,
            }
        }

        // Ghost chases the cursor once a *pane* drag has left its source window. A tab drag
        // is in-window only, so it never spawns a ghost.
        {
            let mut gb = self.ghost.borrow_mut();
            if gb.is_none() {
                *gb = Some(drag::Ghost::new());
            }
            let g = gb.as_ref().unwrap();
            if is_pane && hover.win != Some(source_id) {
                g.follow(cursor);
            } else {
                g.hide();
            }
        }

        self.set_previews(windows, is_pane, &hover, source_id);
        self.preview_on.set(true);
    }

    /// Hit-test the global `cursor` against every window's live geometry, resolving it into
    /// a [`Hover`] (which window / tab strip / pane slot it is over). `win == None` means
    /// empty desktop space (→ a tear-off drop makes a new window).
    fn compute_hover(&self, windows: &[Rc<Window>], cursor: (i32, i32)) -> Hover {
        for w in windows {
            let raw = w.hwnd.get();
            if raw == 0 {
                continue;
            }
            let (l, t, r, b) = drag::window_rect(raw);
            if !(cursor.0 >= l && cursor.0 < r && cursor.1 >= t && cursor.1 < b) {
                continue;
            }
            let scale = w.app.window().scale_factor().max(1.0);
            let st = w.state.borrow();
            let fullscreen = st.fullscreen;
            let lx = (cursor.0 - l) as f32 / scale; // window-logical x
            let ly = (cursor.1 - t) as f32 / scale; // window-logical y (from window top)

            let mut h = Hover { win: Some(w.id), ..Default::default() };

            // Over the top bar → a tab-strip target.
            if !fullscreen && ly < TOPBAR_H {
                h.over_strip = true;
                let (slot, over) = self.tab_hit(w, lx);
                h.tab_slot = slot;
                h.tab_over = over;
                return h;
            }

            // Otherwise the pane area (area-relative: subtract the top bar).
            let ax = lx;
            let ay = ly - if fullscreen { 0.0 } else { TOPBAR_H };
            let vertical = st.active_is_rows();
            for (j, p) in st.active_tab().panes.iter().enumerate() {
                if !p.visible {
                    continue;
                }
                let (px, py, pw, ph) = p.rect;
                if ax >= px && ax < px + pw && ay >= py && ay < py + ph {
                    let pos = if vertical { ay - py } else { ax - px };
                    let size = if vertical { ph } else { pw };
                    let (off, edge) = drag::edge_band(pos, size, vertical);
                    h.pane_idx = Some(j);
                    h.slot_index = j + off;
                    h.pane_rect = p.rect;
                    h.edge = edge;
                    // The header band (top of the tile) is the drag handle → open-hand.
                    h.over_header = (ay - py) < HEADER_BAND;
                    return h;
                }
            }
            return h; // inside the window but over a gap
        }
        Hover::default() // empty space
    }

    /// Hit-test a window-logical x against the tab strip: returns `(insertion slot, the tab
    /// chip directly under the cursor)`. The slot counts tabs whose centre the cursor has
    /// passed (for the reorder/dock caret); the chip is the tab whose extent contains the
    /// cursor (for spring-load / dock-into-tab). Uses the UI-reported [`Window::tab_geom`].
    fn tab_hit(&self, w: &Rc<Window>, lx: f32) -> (usize, Option<usize>) {
        let g = w.tab_geom.borrow();
        let n = w.state.borrow().tabs.len().min(g.len());
        let mut slot = 0;
        let mut over = None;
        for i in 0..n {
            let (x, wd) = g[i];
            if lx >= x + wd / 2.0 {
                slot = i + 1;
            }
            if lx >= x && lx < x + wd {
                over = Some(i);
            }
        }
        (slot, over)
    }

    /// Resolve a completed drop: reorder in-window, stitch / dock cross-window, or spawn a
    /// new window for an empty-space drop. Re-host uses detach→adopt (replay-primed, no PTY
    /// restart); `State` was untouched until this moment.
    fn apply_drop(self: &Rc<Self>, _windows: &[Rc<Window>], d: &DragState, hover: &Hover, cursor: (i32, i32)) {
        match &d.kind {
            DragKind::Tab { index, .. } => {
                if hover.win == Some(d.source_win) && hover.over_strip {
                    if let Some(src) = self.window_by_id(d.source_win) {
                        src.state.borrow_mut().reorder_tab(*index, hover.tab_slot);
                    }
                }
            }
            DragKind::Pane { uid, .. } => {
                let Some(src) = self.window_by_id(d.source_win) else {
                    return;
                };
                match hover.win {
                    // ---- in-window ----
                    Some(target_id) if target_id == d.source_win => {
                        if hover.over_strip {
                            match hover.tab_over {
                                // dropped on an existing tab chip → dock into that tab.
                                Some(ti) => {
                                    src.state.borrow_mut().switch_tab(ti);
                                    let det = src.state.borrow_mut().detach_uid(uid);
                                    if let Some((det, _alive)) = det {
                                        src.state.borrow_mut().adopt_pane(&self.mgr, det);
                                    }
                                }
                                // dropped on the empty strip / `+` → a fresh tab.
                                None => {
                                    let det = src.state.borrow_mut().detach_uid(uid);
                                    if let Some((det, _alive)) = det {
                                        src.state.borrow_mut().adopt_pane_as_tab(&self.mgr, det);
                                    }
                                }
                            }
                        } else if hover.pane_idx.is_some() {
                            // Pane area: reorder within the active tab, or — if a spring-load
                            // moved us to a different tab — move the pane across tabs to the
                            // hovered slot.
                            if src.state.borrow().active_has_uid(uid) {
                                let from = src
                                    .state
                                    .borrow()
                                    .active_tab()
                                    .panes
                                    .iter()
                                    .position(|p| p.uid == *uid);
                                if let Some(from) = from {
                                    src.state.borrow_mut().reorder_pane(from, hover.slot_index);
                                }
                            } else {
                                let det = src.state.borrow_mut().detach_uid(uid);
                                if let Some((det, _alive)) = det {
                                    src.state
                                        .borrow_mut()
                                        .adopt_pane_at(&self.mgr, det, hover.slot_index);
                                }
                            }
                        }
                    }
                    // ---- cross-window: stitch (pane area) or dock (strip) ----
                    Some(target_id) => {
                        let det = src.state.borrow_mut().detach_uid(uid);
                        if let Some((det, alive)) = det {
                            if let Some(tgt) = self.window_by_id(target_id) {
                                if hover.over_strip {
                                    match hover.tab_over {
                                        // onto an existing tab → switch to it + dock in.
                                        Some(ti) => {
                                            tgt.state.borrow_mut().switch_tab(ti);
                                            tgt.state.borrow_mut().adopt_pane(&self.mgr, det);
                                        }
                                        None => {
                                            tgt.state.borrow_mut().adopt_pane_as_tab(&self.mgr, det)
                                        }
                                    }
                                } else {
                                    tgt.state
                                        .borrow_mut()
                                        .adopt_pane_at(&self.mgr, det, hover.slot_index);
                                }
                            }
                            if !alive {
                                src.closing.set(true);
                            }
                        }
                    }
                    // ---- empty space → a new window hosting the pane ----
                    None => {
                        let det = src.state.borrow_mut().detach_uid(uid);
                        if let Some((det, alive)) = det {
                            self.spawn_window(crate::app::PendingSeed::Adopt(det));
                            if let Some(nw) = self.windows.borrow().last() {
                                let scale = src.app.window().scale_factor().max(1.0);
                                let lx = cursor.0 as f32 / scale - 80.0;
                                let ly = cursor.1 as f32 / scale - 16.0;
                                nw.app
                                    .window()
                                    .set_position(LogicalPosition::new(lx, ly));
                            }
                            if !alive {
                                src.closing.set(true);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Paint the drop previews for the current `hover` onto every window: a window-level
    /// glow on the target of a cross-window pane tear, a tab-strip highlight + insertion
    /// caret, and a slot highlight on the hovered pane tile.
    fn set_previews(&self, windows: &[Rc<Window>], is_pane: bool, hover: &Hover, source_id: usize) {
        for w in windows {
            let is_target = hover.win == Some(w.id);

            // Window glow: only when tearing a pane *into a different* window.
            w.app
                .set_drop_win_active(is_pane && is_target && w.id != source_id);

            // Tab strip: dock-as-tab (pane drag) into any window, or a tab reorder in the
            // source window.
            let strip_active =
                is_target && hover.over_strip && (is_pane || w.id == source_id);
            w.app.set_drop_strip_active(strip_active);

            // Spring-target highlight: the existing tab chip a dragged pane is hovering.
            let spring_tab = if is_pane && is_target && hover.over_strip {
                hover.tab_over.map(|i| i as i32).unwrap_or(-1)
            } else {
                -1
            };
            w.app.set_drop_tab_idx(spring_tab);

            // Insertion caret — shown ONLY over the empty strip / gap (or a tab reorder),
            // never together with a spring-highlighted tab (either/or, not both).
            if strip_active && spring_tab < 0 {
                let g = w.tab_geom.borrow();
                let n = w.state.borrow().tabs.len().min(g.len());
                let caret = if hover.tab_slot < n {
                    g[hover.tab_slot].0
                } else if n > 0 {
                    let (x, wd) = g[n - 1];
                    x + wd
                } else {
                    0.0
                };
                w.app.set_drop_tab_x(caret);
                w.app.set_drop_tab_active(true);
            } else {
                w.app.set_drop_tab_active(false);
            }

            // Pane tile slot highlight (stitch / reorder target).
            let pane_active =
                is_pane && is_target && !hover.over_strip && hover.pane_idx.is_some();
            if pane_active {
                let (x, y, wd, ht) = hover.pane_rect;
                w.app.set_drop_rect_x(x);
                w.app.set_drop_rect_y(y);
                w.app.set_drop_rect_w(wd);
                w.app.set_drop_rect_h(ht);
                w.app.set_drop_rect_edge(hover.edge as i32);
                w.app.set_drop_rect_active(true);
            } else {
                w.app.set_drop_rect_active(false);
            }
        }
    }

    /// Clear all drop-preview props on every window (drag ended / cancelled).
    fn clear_previews(&self, windows: &[Rc<Window>]) {
        for w in windows {
            w.app.set_drop_win_active(false);
            w.app.set_drop_strip_active(false);
            w.app.set_drop_tab_active(false);
            w.app.set_drop_tab_idx(-1);
            w.app.set_drop_rect_active(false);
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

        // pane-header inline rename (double-click the header label).
        cb_i32!(on_begin_rename_pane, Command::BeginRenamePane);
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_rename_pane(move |i, t| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::RenamePane(i, t.to_string()));
                }
            });
        }

        // clickable paths: track each pane's surface size + drive hover/click hit-testing.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_geometry(move |i, w, h| {
                if let Some(win) = app.window_by_id(id) {
                    win.state.borrow_mut().set_pane_surf(i as usize, w, h);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_link_moved(move |i, x, y| {
                if let Some(win) = app.window_by_id(id) {
                    win.state.borrow_mut().pane_link_moved(i as usize, x, y);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_link_exited(move |i| {
                if let Some(win) = app.window_by_id(id) {
                    win.state.borrow_mut().pane_link_exited(i as usize);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_link_activated(move |i, x, y, ctrl| {
                if let Some(win) = app.window_by_id(id) {
                    let action = win
                        .state
                        .borrow_mut()
                        .pane_link_activate(i as usize, x, y, ctrl);
                    if let Some(hyperpanes_terminal_widget::LinkAction::Copy(path)) = action {
                        copy_to_clipboard(&path);
                    }
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

        // ---- drag / tear-off (Wave 2) ----
        // A pane header / tab was pressed: arm a drag from the *global* cursor (the pump
        // promotes it past the threshold, else it stays a plain click).
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_grab(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    app.begin_pane_drag(&w, i as usize);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_tab_grab(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    app.begin_tab_drag(&w, i as usize);
                }
            });
        }
        // The UI reports each tab's window-logical geometry so the strip can be hit-tested.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_tab_geom(move |i, x, wd| {
                if let Some(w) = app.window_by_id(id) {
                    let i = i as usize;
                    let mut g = w.tab_geom.borrow_mut();
                    if g.len() <= i {
                        g.resize(i + 1, (0.0, 0.0));
                    }
                    g[i] = (x, wd);
                }
            });
        }

        // Wave-2 overlays
        cb0!(on_open_palette, Command::PaletteOpen);
        cb0!(on_open_prefs, Command::PrefsOpen);
        cb0!(on_toggle_sidebar, Command::ToggleSidebar);
        cb0!(on_toggle_projects, Command::ToggleProjects);
        cb0!(on_palette_activate, Command::PaletteActivate);
        cb0!(on_overlay_dismiss, Command::CloseOverlay);
        cb0!(on_pref_done, Command::PrefsDone);
        cb_i32!(on_pref_confirm, Command::PrefsConfirm);

        // ---- keybindings editor: rebind capture + reset (act on state directly) ----
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pref_rebind(move |bid| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().begin_rebind(&bid);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pref_capture(move |ctrl, alt, shift, text| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().capture_chord(ctrl, alt, shift, &text);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pref_reset_binding(move |bid| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().reset_binding(&bid);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pref_reset_all_bindings(move || {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().reset_all_bindings();
                }
            });
        }
        cb_i32!(on_palette_nav, Command::PaletteNav);
        cb_usize!(on_palette_pick, Command::PaletteSelect);
        cb_usize!(on_open_project, Command::OpenProject);
        cb_usize!(on_remove_project, Command::RemoveProject);
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_recolor_project(move |row, swatch| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::SetProjectColor(row as usize, swatch as usize));
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_rename_project(move |row, name| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::RenameProject(row as usize, name.to_string()));
                }
            });
        }
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
                // Font selection (kind 0) is its own command (handles presets + Custom… mode).
                if kind == 0 {
                    app.run_command(&w, Command::FontSelect(arg.max(0) as usize));
                    return;
                }
                let setting = match kind {
                    1 => crate::state::Setting::FontDelta(arg),
                    2 => crate::state::Setting::ShowFrame(arg != 0),
                    3 => crate::state::Setting::ShowDot(arg != 0),
                    4 => crate::state::Setting::FramePalette(arg as usize),
                    9 => crate::state::Setting::TerminalTheme(arg as usize),
                    5 => crate::state::Setting::DefaultShell(
                        crate::prefs::SHELL_OPTIONS
                            .get(arg as usize)
                            .map(|(_, v)| v.to_string())
                            .unwrap_or_default(),
                    ),
                    6 => crate::state::Setting::ClickablePaths(arg != 0),
                    // idle-glow settings — apply immediately (not drafted).
                    10 => crate::state::Setting::IdleAlert(arg != 0),
                    11 => crate::state::Setting::IdleEffect(arg.max(0) as usize),
                    12 => crate::state::Setting::IdleSeconds(arg),
                    _ => return,
                };
                // Appearance settings (0–4, 9 = theme) edit the draft (commit on Done);
                // General/Terminal/idle settings apply immediately, matching the renderer.
                let cmd = if kind <= 4 || kind == 9 {
                    Command::DraftSetting(setting)
                } else {
                    Command::ApplySetting(setting)
                };
                app.run_command(&w, cmd);
            });
        }
        // String-valued settings (editor command).
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pref_text(move |kind, value| {
                let Some(w) = app.window_by_id(id) else { return };
                // Custom font path (kind 8) is its own command; editor command (7) is a setting.
                if kind == 8 {
                    app.run_command(&w, Command::FontCustomValue(value.to_string()));
                    return;
                }
                let setting = match kind {
                    7 => crate::state::Setting::EditorCommand(value.to_string()),
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

/// Copy `text` to the Windows clipboard via the built-in `clip` utility (no extra crate /
/// Win32 feature needed). Used by the Ctrl+click branch of clickable paths. Best-effort.
pub(crate) fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("clip").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}
