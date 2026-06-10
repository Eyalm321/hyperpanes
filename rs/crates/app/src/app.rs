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
use std::sync::Arc;
use std::time::Duration;

use hyperpanes_core::layout::presets::DividerKind;
use hyperpanes_core::session_manager::{SessionEvent, SessionManager};
use hyperpanes_terminal_widget::{encode_key, keys};

use slint::platform::Key;
use slint::{ComponentHandle, LogicalPosition};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::command::{dispatch, set_layout_from_id, Command, Effect};
use crate::drag::{self, DragKind, DragState, Hover};
use crate::paneview::{self, Ui};
use crate::state::{DetachedPane, DetachedTab, EscOutcome, NewPaneOpts, State};
use crate::{theme, window, AppWindow, KeyMsg};

/// Fast (active) pump cadence in ms — the responsive default whenever there's work to do.
pub const TICK_FAST_MS: u64 = 8;
/// Idle pump cadence in ms — the pump drops to this after a stretch with no work, so the
/// 125 Hz wakeups stop burning idle CPU. Kept conservative (≈31 Hz) so streamed output and
/// the OS maximize-state poll still refresh within a frame; input wakes back to fast
/// instantly. ⚠ INPUT-LATENCY-SENSITIVE — see `App::wake` and the Task-17 #3 notes.
pub const TICK_IDLE_MS: u64 = 32;
/// Consecutive idle fast-cadence ticks before dropping to the idle cadence (≈0.6 s at 8 ms).
const IDLE_TICKS_BEFORE_SLOW: u32 = 75;

/// Logical height of the top bar (where a tab-strip drop lands). Hidden in fullscreen,
/// so the pane area then starts at the window's top edge.
const TOPBAR_H: f32 = 32.0;

/// Logical height of a pane's header (its drag handle) — matches the 26px header in
/// `paneview.slint`. Used to show the open-hand cursor only over the handle, not the body.
const HEADER_BAND: f32 = 26.0;

/// Sentinel "row" the context-menu click-away catcher hands to `ctx-pick` to request a
/// right-click *chain* (Task 7): close the open menu and reopen the one under the new cursor
/// in a single action. `app.slint` is owned by another track and can't gain a dedicated
/// callback, so the catcher rides the existing `pick(int)` surface with this out-of-range
/// row — the same encode-on-a-frozen-callback trick as `open-project`'s worktree-delete
/// codes. Kept in sync with `reopen-chain-row` in `contextmenu.slint`. Far out of any real
/// row range, so it can never collide with a genuine menu row.
const CTX_REOPEN_CHAIN_ROW: i32 = -987654;

/// What a freshly-spawned window is seeded with once its area is known (the first pane
/// is sized against the real area, exactly as the single-window path did).
pub enum PendingSeed {
    /// A brand-new window → spawn one fresh interactive shell pane.
    EmptyTab,
    /// Seed window 0 from a CLI-resolved workspace (`hyperpanes -c …` or a positional `.json`),
    /// materialised through [`State::load_workspace`] (tabs/panes/layout from the spec).
    Workspace(Box<hyperpanes_core::workspace::model::WorkspaceFile>),
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
    /// Last value pushed to `set_win_maximized` (`None` = never written). Slint property
    /// setters mark the tree dirty without an equality check, so writing the same value
    /// every 8 ms pump forces a wgpu re-render at ~125 Hz even when idle — these caches
    /// gate the write so an unchanged value costs nothing.
    pub last_maximized: Cell<Option<bool>>,
    /// Last `(enabled, allow_input, status)` pushed to the control Preferences props, same reason.
    pub last_control: RefCell<Option<(bool, bool, slint::SharedString)>>,
    /// Last `(phase, status, progress)` pushed to the self-update Preferences props (same
    /// idle-render guard — the updater snapshot is polled every tick but only written on change).
    pub last_update: RefCell<Option<(i32, slint::SharedString, f32)>>,
    /// Signature of the last reminder rows/open/fired set pushed into the `RemindersAdapter`
    /// global (Track F) — the same idle-render guard: an unchanged list writes nothing.
    pub last_reminders: Cell<Option<u64>>,
}

/// The app: the window registry + the shared session engine + the shared event stream.
pub struct App {
    pub mgr: Arc<SessionManager>,
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
    /// The ambient-AI engine bridge (runs `core::ai` on its own thread; default-OFF). Fed
    /// the live session taps + per-window pane context; produces `ai.subtitle` lines.
    ai: crate::ai::AiBridge,
    /// Per-window signature of the last-published AI pane context, so we only re-publish
    /// (and the engine only reconciles) when a watched pane's label / mute / set changed.
    ai_ctx_sig: RefCell<std::collections::HashMap<usize, u64>>,
    /// Per-pane (session uid) debounce state for the rendered-screen feed: the last screen
    /// hash fed to the engine and when, so we feed only on change and at most every
    /// [`AI_FEED_INTERVAL`].
    ai_feed: RefCell<std::collections::HashMap<String, (u64, std::time::Instant)>>,
    /// The embedded control HTTP+WS server host (default-OFF): publishes the live windows→
    /// tabs→panes tree into `core::control`'s read-model + applies inbound `/command`s to the
    /// GUI in-process, so the MCP / agent orchestration drives this process like Electron.
    pub control: crate::control_host::ControlHost,
    /// The offline-safe self-updater (Task 8): GitHub-releases check + installer download on
    /// background threads; its live status is mirrored into every window's General panel.
    pub update: crate::update::Updater,
    /// The shared pump timer, owned here so [`App::tick`] can re-interval it for the adaptive
    /// idle cadence (#3). Set once from `main` via [`App::set_timer`]; `None` until then.
    timer: RefCell<Option<slint::Timer>>,
    /// Consecutive pump ticks with no work — drives the drop to the idle cadence.
    idle_ticks: Cell<u32>,
    /// Whether the pump is currently at the slow (idle) cadence (so we re-interval only on a
    /// transition, never every tick).
    cadence_slow: Cell<bool>,
}

/// How often (at most) a pane's rendered screen is re-fed to the ambient-AI engine. The
/// scheduler's settle window is shorter, so a quiet gap between feeds lets it summarise.
/// Kept brisk so the subtitle tracks a live agent/chat pane; the real refresh floor is the
/// model's per-call latency, not this.
const AI_FEED_INTERVAL: Duration = Duration::from_millis(400);

/// How long a pane drag must rest over a tab before it springs open (Chrome/Finder).
const SPRING_DELAY: std::time::Duration = std::time::Duration::from_millis(450);

impl App {
    pub fn new(mgr: Arc<SessionManager>, erx: UnboundedReceiver<SessionEvent>) -> Rc<Self> {
        let control = crate::control_host::ControlHost::new(&mgr);
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
            ai: crate::ai::AiBridge::spawn(),
            ai_ctx_sig: RefCell::new(std::collections::HashMap::new()),
            ai_feed: RefCell::new(std::collections::HashMap::new()),
            control,
            update: crate::update::Updater::new(),
            timer: RefCell::new(None),
            idle_ticks: Cell::new(0),
            cadence_slow: Cell::new(false),
        })
    }

    /// Hand the shared pump timer to the app so the adaptive cadence (#3) can re-interval it.
    /// Called once from `main` after the timer is started at the fast cadence.
    pub fn set_timer(&self, timer: slint::Timer) {
        *self.timer.borrow_mut() = Some(timer);
    }

    /// Switch the pump cadence (no-op when already there, so we never churn the timer). Safe
    /// to call from inside the timer callback — Slint releases the timer-registry borrow
    /// before invoking the callback — and from input callbacks on the UI thread.
    fn set_cadence(&self, slow: bool) {
        if self.cadence_slow.get() == slow {
            return;
        }
        self.cadence_slow.set(slow);
        if let Some(t) = self.timer.borrow().as_ref() {
            let ms = if slow { TICK_IDLE_MS } else { TICK_FAST_MS };
            t.set_interval(Duration::from_millis(ms));
        }
    }

    /// Wake the pump to the fast cadence immediately and reset the idle counter. Called from
    /// the input paths (keystrokes, command dispatch) so a keystroke's echo renders without
    /// the idle-cadence delay. ⚠ This is what keeps adaptive cadence input-latency-safe.
    pub fn wake(&self) {
        self.idle_ticks.set(0);
        self.set_cadence(false);
    }

    /// Realize a new OS window, wire its callbacks to act on its own state, show it, and
    /// register it. `seed` decides its first pane (empty shell, or a re-hosted session).
    pub fn spawn_window(self: &Rc<Self>, seed: PendingSeed) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);

        // The real DPI-scaled font is (re)loaded on the first pump (which owns the
        // scale); `State::new` flags `font_reload`, so a scale-1 placeholder is fine.
        let mut state = State::new(theme::load_font(1.0));

        // #2 startup: seed the first pane BEFORE the heavy `AppWindow::new` (wgpu device
        // init). The pty spawn itself runs on a worker thread (`spawn_session_async` —
        // pwsh-into-ConPTY blocks ~1s, which used to serialize with GPU init here), so
        // seeding costs ~1ms and the shell starts up fully overlapped with the window
        // realize. Seeding needs no area; the first render pump relayouts the pane and the
        // spawn-done drain reconciles the pty size. Window 0 only — re-host/tear-off
        // windows are also seeded here.
        crate::perf::mark("spawn_window: seeding first pane");
        self.apply_seed(&mut state, seed);
        crate::perf::mark("spawn_window: first pane seeded (pty spawn queued)");

        let aw = match AppWindow::new() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[hyperpanes] failed to create window: {e}");
                return;
            }
        };
        crate::perf::mark("spawn_window: AppWindow::new done (wgpu device ready)");
        let ui = Ui::new();
        ui.attach(&aw);

        let win = Rc::new(Window {
            id,
            app: aw,
            ui,
            state: RefCell::new(state),
            area: Cell::new((0.0, 0.0)),
            hwnd: Cell::new(0),
            saved: RefCell::new(None),
            // Already seeded above (eagerly) — nothing pending for the pump to do.
            seed: RefCell::new(PendingSeed::Done),
            closing: Cell::new(false),
            tab_geom: RefCell::new(Vec::new()),
            last_maximized: Cell::new(None),
            last_control: RefCell::new(None),
            last_update: RefCell::new(None),
            last_reminders: Cell::new(None),
        });

        self.wire(&win);
        // The running build version is constant — push it once (the General panel's "About").
        win.app.set_pref_app_version(crate::update::CURRENT_VERSION.into());
        // Seed the auto-update toggle from the just-loaded settings (read-only borrow, no
        // command in flight, so the #18 borrow rule is moot here).
        win.app.set_pref_auto_update(win.state.borrow().settings.auto_update);
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
        // Any command is user activity — snap the pump back to the fast cadence (#3) so the
        // result renders immediately even if we'd dropped to the idle cadence.
        self.wake();
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
        // Perf instrumentation (#1) — inert unless `HYPERPANES_PERFLOG` is set.
        let perf_on = crate::perf::enabled();
        let t_tick = perf_on.then(std::time::Instant::now);

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
            // Only push to Slint when the maximized state actually flips — an unconditional
            // per-tick write would re-dirty the render tree and pin idle CPU at the pump rate.
            let maxd = window::is_maximized(w.hwnd.get());
            if w.last_maximized.get() != Some(maxd) {
                w.last_maximized.set(Some(maxd));
                w.app.set_win_maximized(maxd);
            }
        }

        // 1b. Async pane spawns that just completed (see `State::spawn_session_async`):
        //    force a geometry re-apply — the pump may have resized the pane while its
        //    session didn't exist yet (a resize on a missing uid is a silent no-op), so
        //    `applied = (0,0)` makes the next `place()` resend cols/rows to the now-live
        //    pty. A uid whose pane is GONE (closed mid-spawn) is killed instead, so the
        //    orphaned shell doesn't leak.
        let done: Vec<String> = {
            let mut q = crate::state::spawn_done().lock().unwrap();
            std::mem::take(&mut *q)
        };
        for uid in done {
            let mut found = false;
            for w in &windows {
                let mut st = w.state.borrow_mut();
                if let Some((ti, pi)) = st.find_pane(&uid) {
                    st.tabs[ti].panes[pi].applied = (0, 0);
                    st.dirty = true;
                    found = true;
                    break;
                }
            }
            if !found {
                self.mgr.kill(&uid);
            }
        }

        // 2. Drain the ONE shared event channel, routing each event to its window. Each event
        //    is teed to the control server (live `/events` WS frames + model cwd/exit) before
        //    the GUI consumes it — a cheap no-op when the server is stopped.
        let mut events: u64 = 0;
        let mut bytes: u64 = 0;
        let t_drain = perf_on.then(std::time::Instant::now);
        {
            let mut rx = self.erx.borrow_mut();
            while let Ok(ev) = rx.try_recv() {
                self.control.tee_event(&ev);
                bytes += self.route_event(&windows, ev) as u64;
                events += 1;
            }
        }
        let drain_ns = t_drain.map(|s| s.elapsed().as_nanos()).unwrap_or(0);

        // 2c. Control plane: reconcile any inbound `/command` structural change into the live
        //     GUI (on this UI thread), then republish the live tree into the read-model so
        //     `/state` / `list_panes` reflect the GUI. No-op when the server is stopped.
        self.control.sync(&windows, &self.mgr);
        // Mirror the control-server status into every window's Preferences props.
        {
            let (enabled, allow_input, port) = self.control.status();
            let status: slint::SharedString = control_status_line(enabled, allow_input, port).into();
            let cur = (enabled, allow_input, status);
            for w in &windows {
                // Same idle-render guard as the maximized flag: only write when it changed.
                if w.last_control.borrow().as_ref() == Some(&cur) {
                    continue;
                }
                *w.last_control.borrow_mut() = Some(cur.clone());
                w.app.set_pref_control_enabled(cur.0);
                w.app.set_pref_control_allow_input(cur.1);
                w.app.set_pref_control_status(cur.2.clone());
            }
        }

        // 2d. Self-update: mirror the off-thread updater's snapshot (phase/message/progress)
        //     into every window's General panel. Same idle-render guard as the control props —
        //     polled each tick (cheap) but only written when something actually changed.
        {
            let snap = self.update.snapshot();
            let cur = (snap.phase, slint::SharedString::from(snap.message), snap.progress);
            for w in &windows {
                if w.last_update.borrow().as_ref() == Some(&cur) {
                    continue;
                }
                *w.last_update.borrow_mut() = Some(cur.clone());
                w.app.set_pref_update_phase(cur.0);
                w.app.set_pref_update_status(cur.1.clone());
                w.app.set_pref_update_progress(cur.2);
            }
        }

        // 2b. Ambient-AI: apply produced subtitles, (re)publish each window's pane context,
        //     and mirror the engine status into the Preferences props.
        self.pump_ai(&windows);

        // 2e. Reminder panes (Track F): mark due reminders fired, then mirror each window's
        //     rows + bell state into the `RemindersAdapter` global (signature-gated, so an
        //     unchanged list costs nothing at either tick cadence).
        self.pump_reminders(&windows);

        // 3. Render each window from its own state. Aggregate per-window activity so the
        //    adaptive cadence (#3) can tell a busy frame (streaming output / animation) from a
        //    truly idle one (a bare cursor blink does NOT count as work — see `paneview::pump`).
        let t_render = perf_on.then(std::time::Instant::now);
        let mut renders: u64 = 0;
        let mut any_active = false;
        for w in &windows {
            let r = self.pump_window(w);
            renders += r.rendered as u64;
            any_active |= r.active;
        }
        let render_ns = t_render.map(|s| s.elapsed().as_nanos()).unwrap_or(0);

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
                    // Tell the AI engine to forget this window's panes, and drop its context sig.
                    self.ai.send(crate::ai::AiMsg::DropWindow { window_id: w.id as i64 });
                    self.ai_ctx_sig.borrow_mut().remove(&w.id);
                    for uid in w.state.borrow().session_uids() {
                        self.ai_feed.borrow_mut().remove(&uid);
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

        // ---- adaptive idle cadence (#3) ----
        // A tick "did work" if it drained any session output, animated/rendered real pane
        // content, has a drag in flight, or the control server is live (keep MCP/agent drivers
        // responsive). After a stretch of idle ticks the pump drops to the slow cadence; any
        // work — or an input via `App::wake` — snaps it back to fast.
        let did_work =
            events > 0 || any_active || self.drag.borrow().is_some() || self.control.status().0;
        if did_work {
            self.idle_ticks.set(0);
            self.set_cadence(false);
        } else {
            let n = self.idle_ticks.get().saturating_add(1);
            self.idle_ticks.set(n);
            if n >= IDLE_TICKS_BEFORE_SLOW {
                self.set_cadence(true);
            }
        }

        if let Some(t) = t_tick {
            crate::perf::tick(events, bytes, renders, drain_ns, render_ns, t.elapsed().as_nanos());
        }
    }

    /// Route one session event to the window hosting its `uid`. Returns the number of output
    /// bytes fed into a pane (0 for non-`Data` events / no owning pane) so the pump can report
    /// feed throughput in the perf log (#1).
    fn route_event(&self, windows: &[Rc<Window>], ev: SessionEvent) -> usize {
        match ev {
            SessionEvent::Data { uid, data } => {
                // NB: the ambient-AI engine is NOT fed the raw redraw byte stream here — a
                // repainting TUI (e.g. an agent CLI) would drown the quiescence scheduler in
                // noise. Instead `pump_ai` feeds each pane's *rendered screen text* on a
                // cadence (what the user actually sees). See `App::pump_ai`.
                let Some(w) = find_window(windows, &uid) else { return 0 };
                let mut st = w.state.borrow_mut();
                let mut fed = 0;
                if let Some((ti, pi)) = st.find_pane(&uid) {
                    let pc = &mut st.tabs[ti].panes[pi];
                    // Sniff the shell's OSC window title so the idle glow can tell an agent
                    // pane (claude, etc.) from a plain shell (app-side; no core change).
                    if let Some(title) = crate::glow::sniff_osc_title(&data) {
                        pc.shell_title = title;
                    }
                    pc.pane.feed(&data);
                    fed = data.len();
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
                fed
            }
            SessionEvent::Exit { uid, .. } => {
                self.ai.send(crate::ai::AiMsg::Exit { uid: uid.clone() });
                self.ai_feed.borrow_mut().remove(&uid);
                if let Some(w) = find_window(windows, &uid) {
                    let alive = w.state.borrow_mut().pane_exited(&uid, &self.mgr);
                    if !alive {
                        w.closing.set(true);
                    }
                }
                0
            }
            SessionEvent::Cwd { uid, cwd } => {
                if let Some(w) = find_window(windows, &uid) {
                    let project = {
                        let mut st = w.state.borrow_mut();
                        // Resolve clickable paths relative to this pane's live directory.
                        if let Some((ti, pi)) = st.find_pane(&uid) {
                            st.tabs[ti].panes[pi].pane.set_cwd(Some(cwd.clone()));
                            // Mirror it for the control read-model's `/state` cwd field.
                            st.tabs[ti].panes[pi].cwd = Some(cwd.clone());
                        }
                        // Refresh the remembered-projects list AND tint THIS pane if its cwd is
                        // inside a git repo (project color + frame/dot on + project-name label).
                        st.note_pane_cwd(&uid, &cwd)
                    };
                    // Feed the cwd + resolved project to the ambient-AI engine (for the prompt
                    // context + per-project memory).
                    self.ai.send(crate::ai::AiMsg::Cwd { uid, cwd, project });
                }
                0
            }
        }
    }

    /// Ambient-AI pump (UI thread): apply produced subtitles to pane state, (re)publish each
    /// window's pane context to the engine when it changed, and mirror the latest engine
    /// status into every window's Preferences props. Cheap when the engine is off/idle.
    fn pump_ai(self: &Rc<Self>, windows: &[Rc<Window>]) {
        // Apply each produced `ai.subtitle` to the owning pane (kicks its typewriter reveal).
        for u in self.ai.drain_meta() {
            if let Some(w) = windows.iter().find(|w| w.id as i64 == u.window_id) {
                let mut st = w.state.borrow_mut();
                for (k, v) in &u.pairs {
                    if k == "ai.subtitle" {
                        st.set_ai_subtitle(&u.pane_id, v);
                    }
                }
            }
        }
        // Re-publish a window's pane context only when its watch list changed.
        for w in windows {
            let sig = w.state.borrow().ai_context_sig();
            let changed = self.ai_ctx_sig.borrow().get(&w.id).copied() != Some(sig);
            if changed {
                self.ai_ctx_sig.borrow_mut().insert(w.id, sig);
                let panes = w.state.borrow().ai_pane_publish();
                self.ai.send(crate::ai::AiMsg::PaneContext {
                    window_id: w.id as i64,
                    panes,
                });
            }
        }
        // Mirror an engine status transition into the Preferences projection on every window.
        if self.ai.drain_status() {
            let st = self.ai.status();
            for w in windows {
                apply_ai_status_props(&w.app, &st);
            }
        }

        // Feed each pane's RENDERED screen text (what the user sees) to the engine — debounced
        // and only on change. This replaces the raw pty feed so a repainting agent TUI (Claude
        // Code, etc.) is summarised from a clean snapshot rather than redraw noise. Skipped
        // entirely when the engine is off, and per-pane when muted.
        if self.ai.enabled() {
            let now = std::time::Instant::now();
            let mut feed = self.ai_feed.borrow_mut();
            for w in windows {
                let st = w.state.borrow();
                for tab in &st.tabs {
                    for p in &tab.panes {
                        if p.ai_muted {
                            continue;
                        }
                        let prev = feed.get(&p.uid).copied();
                        if let Some((_, at)) = prev {
                            if now.duration_since(at) < AI_FEED_INTERVAL {
                                continue; // debounce: too soon since the last check
                            }
                        }
                        let text = p.pane.screen_text();
                        if text.trim().chars().count() < 3 {
                            continue;
                        }
                        let hash = fnv1a(&text);
                        // Unchanged screen → just bump the timestamp (re-check next interval),
                        // don't re-feed (the engine would only dedupe it anyway).
                        if prev.map(|(h, _)| h) == Some(hash) {
                            feed.insert(p.uid.clone(), (hash, now));
                            continue;
                        }
                        feed.insert(p.uid.clone(), (hash, now));
                        self.ai.feed_data(&p.uid, &text);
                    }
                }
            }
        }
    }

    /// Reminder-pane pump (Track F, UI thread): fire any due reminder (the tick half of the
    /// adaptive-tick pattern — marking an entry `fired` sets `dirty`, which counts as work
    /// and keeps the highlight responsive), then mirror each window's reminder list into its
    /// `RemindersAdapter` global. The push is gated by a signature over (open, uid, fired)
    /// — labels/tints are fixed at park time — because Slint property writes dirty the
    /// render tree unconditionally (the same idle-render guard as the maximized flag).
    fn pump_reminders(&self, windows: &[Rc<Window>]) {
        let now_ms = crate::glow::now_epoch_ms();
        for w in windows {
            let _ = w.state.borrow_mut().tick_reminders(now_ms);
            // Read the signature + projection out of a scoped borrow, dropped before any
            // Slint write (the #18 borrow rule, defensively — setters don't re-enter).
            let (sig, open) = {
                let st = w.state.borrow();
                let mut h: u64 = 0xcbf29ce484222325;
                let mut mix = |bytes: &[u8]| {
                    for b in bytes {
                        h ^= *b as u64;
                        h = h.wrapping_mul(0x100000001b3);
                    }
                };
                mix(if st.reminders_open { b"\x01" } else { b"\x00" });
                for r in &st.reminders {
                    mix(r.pane.uid.as_bytes());
                    mix(if r.fired { b"\x01" } else { b"\x00" });
                    mix(b"\x1e");
                }
                (h, st.reminders_open)
            };
            if w.last_reminders.get() == Some(sig) {
                continue;
            }
            w.last_reminders.set(Some(sig));
            let (rows, alert) = {
                let st = w.state.borrow();
                let palette = st.settings.frame_palette;
                let alert = st.reminders.iter().any(|r| r.fired);
                let rows: Vec<crate::ReminderItem> = st
                    .reminders
                    .iter()
                    .enumerate()
                    .map(|(i, r)| crate::ReminderItem {
                        uid: r.pane.uid.as_str().into(),
                        title: r.pane.title.clone(),
                        tint: r
                            .pane
                            .pinned_accent
                            .unwrap_or_else(|| theme::accent_for(i, palette)),
                        due: r.due_label.clone(),
                        overdue: r.fired,
                    })
                    .collect();
                (rows, alert)
            };
            let g = w.app.global::<crate::RemindersAdapter>();
            g.set_rows(slint::ModelRc::from(Rc::new(slint::VecModel::from(rows))));
            g.set_open(open);
            g.set_alert(alert);
        }
    }

    /// Seed a window's first pane(s) from its [`PendingSeed`]. Called EAGERLY from
    /// `spawn_window` (#2 startup) — the pty spawn no longer waits for the pane area to be
    /// known, because [`State::make_pane`] sizes the pty at a fixed 80×24 and the first render
    /// pump relayouts it to the real area. Seeding here (before the heavy `AppWindow::new`)
    /// overlaps the shell's process startup with wgpu/device init instead of running it after.
    fn apply_seed(self: &Rc<Self>, st: &mut State, seed: PendingSeed) {
        match seed {
            PendingSeed::EmptyTab => {
                st.add_pane(&self.mgr);
                if self.first_seed.replace(false) {
                    if std::env::var_os("HYPERPANES_DEMO").is_some() {
                        crate::demo_seed(st, &self.mgr);
                    }
                    if let Some(which) = std::env::var_os("HYPERPANES_OPEN") {
                        match which.to_string_lossy().as_ref() {
                            "palette" => { dispatch(st, Command::PaletteOpen, &self.mgr); }
                            "prefs" => { dispatch(st, Command::PrefsOpen, &self.mgr); }
                            // The rail is persistent; open its projects flyout so a
                            // screenshot exercises the full surface.
                            "sidebar" => { dispatch(st, Command::ToggleProjects, &self.mgr); }
                            // Open a context menu at a fixed anchor (screenshot scaffold).
                            "panemenu" => { dispatch(st, Command::OpenPaneContext(0, 380.0, 150.0), &self.mgr); }
                            "tabmenu" => { dispatch(st, Command::OpenTabContext(0, 90.0, 44.0), &self.mgr); }
                            // Phase-5 chrome-parity scaffolds:
                            // the hamburger (application) menu, anchored under the button.
                            "appmenu" => { dispatch(st, Command::OpenAppContext(10.0, 32.0), &self.mgr); }
                            // three panes in the single preset → the bottom pane taskbar shows.
                            "taskbar" => {
                                dispatch(st, Command::NewPane, &self.mgr);
                                dispatch(st, Command::NewPane, &self.mgr);
                                dispatch(st, Command::SetLayout(hyperpanes_core::layout::presets::Layout::Single), &self.mgr);
                            }
                            // taskbar + its right-click pane menu (inTaskbar variant: Show + no Maximize).
                            "taskbarmenu" => {
                                dispatch(st, Command::NewPane, &self.mgr);
                                dispatch(st, Command::NewPane, &self.mgr);
                                dispatch(st, Command::SetLayout(hyperpanes_core::layout::presets::Layout::Single), &self.mgr);
                                dispatch(st, Command::OpenTaskbarContext(0, 40.0, 805.0), &self.mgr);
                            }
                            _ => {}
                        }
                    }
                }
            }
            PendingSeed::Workspace(file) => {
                st.load_workspace(*file, &self.mgr);
                // A contentless spec (no spawnable panes) would leave the window blank —
                // fall back to a fresh shell so a launch never yields an empty window.
                if st.tabs.is_empty() {
                    st.add_pane(&self.mgr);
                }
            }
            PendingSeed::Adopt(det) => st.adopt_pane(&self.mgr, det),
            PendingSeed::AdoptTab(det) => st.adopt_tab(&self.mgr, det),
            PendingSeed::Done => {}
        }
    }

    /// Render one window. Returns its [`paneview::PumpResult`] (panes repainted + whether the
    /// pass was active) for the perf log + adaptive cadence. The first pane is seeded eagerly
    /// in `spawn_window`, so this only renders once the pane area is known.
    fn pump_window(self: &Rc<Self>, win: &Rc<Window>) -> paneview::PumpResult {
        let scale = win.app.window().scale_factor().max(1.0);
        let (aw, ah) = win.area.get();

        // Defensive: if a seed is still pending (it normally isn't — `spawn_window` seeds
        // eagerly), apply it now. Area-independent (the pty is fixed 80×24 until relayout).
        let pending = std::mem::replace(&mut *win.seed.borrow_mut(), PendingSeed::Done);
        if !matches!(pending, PendingSeed::Done) {
            let mut st = win.state.borrow_mut();
            self.apply_seed(&mut st, pending);
        }

        if aw <= 1.0 || ah <= 1.0 {
            return paneview::PumpResult { rendered: 0, active: false };
        }
        let mut st = win.state.borrow_mut();
        paneview::pump(&win.app, &mut st, &win.ui, (aw, ah), scale, &self.mgr)
    }

    // ---- key routing for one window (app chords first, else encode to the pane) ----

    fn on_key(self: &Rc<Self>, win: &Rc<Window>, idx: usize, msg: KeyMsg) {
        // A keystroke is the latency-critical input: wake the pump to the fast cadence now so
        // the echo renders without the idle-cadence delay (#3). Commands go through
        // `run_command` (which also wakes); this covers raw typing routed straight to the pty.
        self.wake();
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
        // Escape closes an open context menu first (it sits above everything else).
        if crate::is_key(&msg.text, Key::Escape) && win.state.borrow().ctx_open() {
            self.run_command(win, Command::CloseContext);
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
        // Shift+PageUp / Shift+PageDown scroll the pane's scrollback by one page instead of
        // reaching the shell (plain PageUp/Down still go to the shell). Handled before the
        // forwardable/encode path since PageUp/Down are otherwise pty-bound keys. The grid marks
        // itself dirty, so the 8 ms render pump repaints the new viewport.
        if let Some(up) = keys::scroll_page_key(&msg.text, msg.shift) {
            let mut st = win.state.borrow_mut();
            if let Some(p) = st.active_tab_mut().panes.get_mut(idx) {
                p.pane.scroll_page(up);
            }
            return;
        }
        // Drop bare modifiers / F-keys / special private-use keys.
        if !crate::forwardable(&msg.text) {
            return;
        }
        if let Some(bytes) = encode_key(&msg.text, msg.control, msg.alt, msg.shift) {
            // Type-over selection (prompt-line-only, safe scope): a printable character (ordinary
            // text, no Ctrl/Alt) typed over a drag-selection that sits entirely on the cursor's
            // own row — the live shell input line — drops the selection highlight first, since
            // you're replacing your own prompt input. A selection on any other row (scrollback /
            // command output) is left intact: that text isn't in the shell's editable buffer, so
            // we never emit speculative deletes for it (the brief's no-PTY-corruption fallback —
            // we clear only the on-screen highlight, never edit off-row text).
            let printable = !msg.control
                && !msg.alt
                && msg.text.chars().next().is_some_and(|c| {
                    let u = c as u32;
                    u >= 0x20 && u != 0x7f && !(0xe000..=0xf8ff).contains(&u)
                });
            let mut st = win.state.borrow_mut();
            if let Some(ps) = st.active_tab_mut().panes.get_mut(idx) {
                if printable && ps.pane.selection_on_cursor_row() {
                    ps.pane.selection_clear();
                }
                // Any key that reaches the shell snaps the viewport back to the live edge so the
                // user sees their input echoed at the prompt even after scrolling up to read
                // history (a no-op when already at the bottom).
                ps.pane.scroll_to_bottom();
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
            // Snap the pump to the fast cadence so the drag tracks immediately even when the
            // pane was idle (the adaptive idle pump otherwise lags the grab by up to a tick).
            self.wake();
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
        self.wake();
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

    /// Right-click chaining (Task 7): a right-click *while a context menu is open* should close
    /// it AND open the menu for whatever target sits under the new cursor — in one action,
    /// without a second click. The click-away catcher in `contextmenu.slint` can't carry the
    /// cursor up a dedicated callback (`app.slint` is frozen), so it signals via
    /// `ctx-pick(CTX_REOPEN_CHAIN_ROW)`; here we read the *global* cursor ourselves and hit-test
    /// it with the same window/strip/pane geometry the drag-hover uses, then route to the same
    /// `Open*Context` command a normal right-click would. Reopening replaces the open menu
    /// (`State::ctx` is a single slot), so the swap is atomic.
    ///
    /// Borrow discipline (the #18 fix): every geometry read is bound to a LOCAL and the shared
    /// `state` borrow is dropped *before* `run_command` takes `borrow_mut()`. Never hold a
    /// `state.borrow()` across the reopen.
    fn reopen_context_at_cursor(self: &Rc<Self>, win: &Rc<Window>) {
        let cursor = drag::cursor_pos();
        let raw = win.hwnd.get();

        // Resolve the chained menu to a single Command while the shared borrow is held ONLY to
        // read geometry; it is gone before we run anything below.
        let next: Option<Command> = (|| {
            if raw == 0 {
                return None;
            }
            let (l, t, r, b) = drag::window_rect(raw);
            if !(cursor.0 >= l && cursor.0 < r && cursor.1 >= t && cursor.1 < b) {
                return None; // cursor left the window → no chain target
            }
            let scale = win.app.window().scale_factor().max(1.0);
            let lx = (cursor.0 - l) as f32 / scale; // window-logical x
            let ly = (cursor.1 - t) as f32 / scale; // window-logical y (from window top)

            // `.fullscreen` is Copy, so this borrow ends with the statement.
            let fullscreen = win.state.borrow().fullscreen;

            // Over the top bar → a tab-strip target (reuses `tab_hit`, which re-borrows on its
            // own, so our borrow is already released here).
            if !fullscreen && ly < TOPBAR_H {
                let (_slot, over) = self.tab_hit(win, lx);
                return over.map(|i| Command::OpenTabContext(i, lx, ly));
            }

            // Otherwise the pane area (area-relative: subtract the top bar). Bind the pane hit
            // out of a scoped borrow, then drop it before we build the command.
            let ax = lx;
            let ay = ly - if fullscreen { 0.0 } else { TOPBAR_H };
            let pane_hit = {
                let st = win.state.borrow();
                let mut hit = None;
                for (j, p) in st.active_tab().panes.iter().enumerate() {
                    if !p.visible {
                        continue;
                    }
                    let (px, py, pw, ph) = p.rect;
                    if ax >= px && ax < px + pw && ay >= py && ay < py + ph {
                        // Only the header band opens the pane menu, matching a normal
                        // right-click (the body below is the terminal). A body / gap hit falls
                        // through to a plain close.
                        if (ay - py) < HEADER_BAND {
                            hit = Some(j);
                        }
                        break;
                    }
                }
                hit
            };
            pane_hit.map(|j| Command::OpenPaneContext(j, lx, ly))
        })();

        // Borrow released. Open the chained menu (replaces the open one) or, with no target
        // under the cursor, treat the right-click as a plain dismiss.
        match next {
            Some(cmd) => self.run_command(win, cmd),
            None => self.run_command(win, Command::CloseContext),
        }
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
        cb0!(on_open_new_pane, Command::OpenNewPane);
        // New Pane dialog submit: build the options payload (empty fields are normalized by
        // `make_pane`) and spawn the configured pane. The shell index maps back to its token.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_submit_new_pane(move |label, color, frame, dot, command, cwd, shell_idx| {
                if let Some(w) = app.window_by_id(id) {
                    let shell = crate::prefs::SHELL_OPTIONS
                        .get(shell_idx as usize)
                        .map(|(_, tok)| (*tok).to_string())
                        .unwrap_or_default();
                    let opts = NewPaneOpts {
                        label: Some(label.to_string()),
                        cwd: Some(cwd.to_string()),
                        command: Some(command.to_string()),
                        shell: Some(shell),
                        accent: Some(color),
                        show_frame: Some(frame),
                        show_dot: Some(dot),
                    };
                    app.run_command(&w, Command::SubmitNewPane(opts));
                }
            });
        }
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
            // Plain mouse-wheel over a pane body → scroll its scrollback viewport. The
            // TerminalPane emits `scroll-requested(±lines)`; paneview forwards it here as
            // `pane-scroll`. (Ctrl+wheel is handled separately as font-zoom.) `scroll_by`
            // marks the grid dirty, so the render pump repaints the new viewport.
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_scroll(move |i, d| {
                if let Some(win) = app.window_by_id(id) {
                    let mut st = win.state.borrow_mut();
                    if let Some(p) = st.active_tab_mut().panes.get_mut(i as usize) {
                        p.pane.scroll_by(d as i32);
                    }
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

        // Ctrl+wheel over a pane → zoom the terminal font (same command as Ctrl+= / Ctrl+-).
        cb_i32!(on_pane_font_zoom, Command::FontZoom);

        // text selection: drag to select, copy-on-release (the widget reports logical-px
        // points; the controller hit-tests against the pane's surface + font cell metrics).
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_selection_begin(move |i, x, y| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().pane_selection_begin(i as usize, x, y);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_selection_update(move |i, x, y| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().pane_selection_update(i as usize, x, y);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_selection_end(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().pane_selection_end(i as usize);
                }
            });
        }

        // right-click in a pane body → paste the clipboard into that pane's session (mirrors
        // Electron; routed through the command path so it uses the session manager).
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_paste(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::PastePane(i as usize));
                }
            });
        }

        // in-pane search box (opened from the pane context menu)
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_search_edited(move |i, q| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().pane_search_query(i as usize, &q);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_search_next(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().pane_search_step(i as usize, true);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_search_prev(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().pane_search_step(i as usize, false);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_search_closed(move |i| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().pane_search_close(i as usize);
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

        // ---- context menus (pane header / tab strip) ----
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pane_context(move |i, x, y| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::OpenPaneContext(i as usize, x, y));
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_tab_context(move |i, x, y| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::OpenTabContext(i as usize, x, y));
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_taskbar_context(move |i, x, y| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::OpenTaskbarContext(i as usize, x, y));
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_open_app_menu(move |x, y| {
                if let Some(w) = app.window_by_id(id) {
                    app.run_command(&w, Command::OpenAppContext(x, y));
                }
            });
        }
        // A top-level row: run its command, then dismiss the menu. The click-away catcher
        // overloads this surface with `CTX_REOPEN_CHAIN_ROW` to ask for a right-click *chain*
        // (close this menu + reopen the one under the new cursor) — see
        // `reopen_context_at_cursor`.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_ctx_pick(move |row| {
                if let Some(w) = app.window_by_id(id) {
                    if row == CTX_REOPEN_CHAIN_ROW {
                        app.reopen_context_at_cursor(&w);
                        return;
                    }
                    // Bind the command out of the shared borrow before mutating (the #18 rule).
                    let cmd = w.state.borrow().ctx_command(row as usize);
                    if let Some(cmd) = cmd {
                        app.run_command(&w, cmd);
                    }
                    app.run_command(&w, Command::CloseContext);
                }
            });
        }
        // Change-Color submenu — these keep the menu open (live preview), like the renderer.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_ctx_swatch(move |sw| {
                if let Some(w) = app.window_by_id(id) {
                    // Bind the target out of the borrow first: holding `state.borrow()` across
                    // `run_command` (which takes `state.borrow_mut()`) double-borrows the RefCell
                    // and panics — in edition 2021 the `if let Some(_) = …borrow()…` temporary
                    // lives for the whole arm (see the `on_ctx_pick` pattern above).
                    let target = w.state.borrow().ctx_target();
                    if let Some(t) = target {
                        app.run_command(&w, Command::RecolorPane(t, sw as usize));
                    }
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_ctx_swatch_none(move || {
                if let Some(w) = app.window_by_id(id) {
                    let target = w.state.borrow().ctx_target();
                    if let Some(t) = target {
                        app.run_command(&w, Command::SetPaneFrame(t, false));
                        app.run_command(&w, Command::SetPaneDot(t, false));
                    }
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_ctx_frame_set(move |on| {
                if let Some(w) = app.window_by_id(id) {
                    let target = w.state.borrow().ctx_target();
                    if let Some(t) = target {
                        app.run_command(&w, Command::SetPaneFrame(t, on));
                    }
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_ctx_dot_set(move |on| {
                if let Some(w) = app.window_by_id(id) {
                    let target = w.state.borrow().ctx_target();
                    if let Some(t) = target {
                        app.run_command(&w, Command::SetPaneDot(t, on));
                    }
                }
            });
        }
        // Move-to-Tab + Layout submenus — perform, then dismiss.
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_ctx_move_tab(move |tab| {
                if let Some(w) = app.window_by_id(id) {
                    let target = w.state.borrow().ctx_target();
                    if let Some(t) = target {
                        app.run_command(&w, Command::MovePaneToTab(t, tab as usize));
                    }
                    app.run_command(&w, Command::CloseContext);
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_ctx_layout(move |lid| {
                if let Some(w) = app.window_by_id(id) {
                    // The crash that was issue #18: the hamburger Layout submenu held this
                    // `state.borrow()` across `run_command`'s `borrow_mut()`. Drop it first.
                    let target = w.state.borrow().ctx_target();
                    if let Some(t) = target {
                        app.run_command(&w, Command::SetTabLayout(t, theme::layout_from_id(lid)));
                    }
                    app.run_command(&w, Command::CloseContext);
                }
            });
        }
        cb0!(on_ctx_dismiss, Command::CloseContext);

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

        // ---- reminder panes (Track F): the RemindersAdapter global's callbacks ----
        // The bell + list live in sidebar.slint and talk through the global, so wiring is
        // here rather than on AppWindow callbacks. Per-window like everything else.
        {
            let app = app.clone();
            let id = win.id;
            win.app
                .global::<crate::RemindersAdapter>()
                .on_toggle(move || {
                    if let Some(w) = app.window_by_id(id) {
                        app.run_command(&w, Command::ToggleReminders);
                    }
                });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app
                .global::<crate::RemindersAdapter>()
                .on_restore(move |uid| {
                    if let Some(w) = app.window_by_id(id) {
                        app.run_command(&w, Command::RestoreReminder(uid.to_string()));
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
            win.app.on_pref_cancel_rebind(move || {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().cancel_rebind();
                }
            });
        }
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_pref_unbind(move |bid| {
                if let Some(w) = app.window_by_id(id) {
                    w.state.borrow_mut().unbind_binding(&bid);
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
        // `open-project` is overloaded by the sidebar worktree tree: a normal index opens
        // the project in a pane, while an out-of-range encoded index means "remove worktree
        // (proj, wt)" — letting the worktree feature ride the existing frozen callback
        // surface (app.slint isn't touched). DELETE_BASE/STRIDE mirror sidebar.slint.
        {
            const DELETE_BASE: i64 = 1_000_000;
            const DELETE_STRIDE: i64 = 1_000;
            let app = app.clone();
            let id = win.id;
            win.app.on_open_project(move |code| {
                let Some(w) = app.window_by_id(id) else { return };
                let code = code as i64;
                if code < DELETE_BASE {
                    app.run_command(&w, Command::OpenProject(code as usize));
                    return;
                }
                let proj = ((code - DELETE_BASE) / DELETE_STRIDE) as usize;
                let wt = ((code - DELETE_BASE) % DELETE_STRIDE) as usize;
                // Resolve the repo + the WHOLE worktree row (path + is_main) under a read-only
                // borrow, dropped before we spawn git. The sidebar already verified, at confirm
                // time, that these indices still map to the path the user saw (the TOCTOU guard
                // lives in sidebar.slint); resolving by index here reads that same model
                // snapshot synchronously.
                let target = {
                    let st = w.state.borrow();
                    st.projects.get(proj).map(|p| {
                        let row = crate::sidebar::worktrees_for(&p.path).get(wt).cloned();
                        (p.path.clone(), row)
                    })
                };
                let Some((repo, Some(row))) = target else { return };
                // Defense-in-depth: NEVER delete the main checkout (or a locked worktree) from
                // code, regardless of git also refusing both without `--force`. The disabled
                // trash in the UI is not a guarantee — re-check here before touching the
                // filesystem.
                if row.is_main {
                    crate::dbg_log("worktree remove refused: target is the main checkout");
                    return;
                }
                if row.locked {
                    crate::dbg_log("worktree remove refused: target worktree is locked");
                    return;
                }
                let wt_path = row.path;
                match crate::sidebar::remove_worktree(&repo, &wt_path) {
                    Ok(()) => crate::sidebar::invalidate(&repo),
                    Err(e) => {
                        // Surface the failure (git refused: dirty / locked / etc.) instead of
                        // swallowing it to a debug log — the popup closed, so the user believes
                        // it worked. Toast it on the focused pane and invalidate so the next
                        // projection re-enumerates reality (the worktree is still there).
                        crate::dbg_log(&format!("worktree remove failed for {wt_path}: {e}"));
                        crate::sidebar::invalidate(&repo);
                        let mut st = w.state.borrow_mut();
                        let f = st.active_tab().focused;
                        if let Some(p) = st.active_tab_mut().panes.get_mut(f) {
                            p.pane.set_toast(format!("Worktree remove failed: {e}"));
                        }
                        st.dirty = true;
                    }
                }
            });
        }
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
        // Resume a Claude session in a new pane: spawn `claude --resume <id>` cd'd into the
        // project's repo, tinted with the project color. Resolve the repo path + color under a
        // read-only borrow that's dropped before dispatch (borrow rule #18: never hold a
        // `state.borrow()` across `run_command`).
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_resume_session(move |proj_idx, sid| {
                let Some(w) = app.window_by_id(id) else { return };
                let target = {
                    let st = w.state.borrow();
                    st.projects
                        .get(proj_idx as usize)
                        .map(|p| (p.path.clone(), p.color.clone()))
                };
                let Some((cwd, color)) = target else { return };
                let opts = NewPaneOpts {
                    label: Some("claude".to_string()),
                    cwd: Some(cwd),
                    command: Some(crate::sidebar::claude_resume_command(&sid)),
                    shell: None,
                    accent: Some(crate::state::parse_hex(&color)),
                    show_frame: None,
                    show_dot: None,
                };
                app.run_command(&w, Command::SubmitNewPane(opts));
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
                // Ambient-AI master toggle — drives the engine directly (its own ai-settings.json,
                // not a drafted appearance Setting).
                if kind == 13 {
                    app.ai.send(crate::ai::AiMsg::SetEnabled(arg != 0));
                    return;
                }
                // Control API (agents / MCP) toggles — start/stop the embedded server live,
                // and flip its input gate. Both persist via `persistence::control_settings`.
                if kind == 15 {
                    app.control.set_enabled(arg != 0, &app.mgr);
                    return;
                }
                if kind == 16 {
                    app.control.set_allow_input(arg != 0);
                    return;
                }
                // Auto-update toggle (persisted) — apply + mirror the prop back immediately.
                if kind == 17 {
                    app.run_command(
                        &w,
                        Command::ApplySetting(crate::state::Setting::AutoUpdate(arg != 0)),
                    );
                    w.app.set_pref_auto_update(arg != 0);
                    return;
                }
                // Self-update (General panel) — all off-thread + offline-safe.
                if kind == 18 {
                    app.update.check(false); // manual check: report every outcome
                    return;
                }
                if kind == 19 {
                    app.update.download();
                    return;
                }
                if kind == 20 {
                    // Launch the staged installer silently, then quit so it can replace our
                    // files. We never overwrite the running exe in place.
                    match app.update.installer_path() {
                        Some(path) => match crate::update::launch_installer(&path) {
                            Ok(()) => {
                                let _ = slint::quit_event_loop();
                            }
                            Err(e) => {
                                app.update.set_error(format!("Couldn't launch installer: {e}"))
                            }
                        },
                        None => app.update.set_error("No downloaded installer to run".to_string()),
                    }
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
                // Ambient-AI Ollama host/base-URL (13) + model (14) — live-configure the engine.
                if kind == 13 || kind == 14 {
                    use hyperpanes_core::ai::service::AiSettingsPatch;
                    let patch = if kind == 13 {
                        AiSettingsPatch { endpoint: Some(value.to_string()), ..Default::default() }
                    } else {
                        AiSettingsPatch { model: Some(value.to_string()), ..Default::default() }
                    };
                    app.ai.send(crate::ai::AiMsg::Configure(patch));
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
                    app.wake(); // live window/pane resize → keep the fast cadence (#3)
                }
            });
        }
        // divider drag: cursor offset from seam centre → size-fraction delta
        {
            let app = app.clone();
            let id = win.id;
            win.app.on_divider_drag(move |index, main, vertical, dx, dy| {
                let Some(w) = app.window_by_id(id) else { return };
                app.wake(); // dragging a divider is active input → keep the fast cadence (#3)
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
/// "Hosting" includes a pane PARKED as a reminder (Track F) — its session events must
/// still reach the owning window (notably `Exit`, which drops the reminder); a `Data`
/// event for a parked pane is then a harmless no-op (no laid-out pane to feed).
fn find_window<'a>(windows: &'a [Rc<Window>], uid: &str) -> Option<&'a Rc<Window>> {
    windows
        .iter()
        .find(|w| w.state.borrow_mut().hosts_session(uid))
}

/// Cheap FNV-1a hash of a string — used to detect when a pane's rendered screen text
/// changed (so the ambient-AI feed only sends on a real change).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Mirror the ambient-AI engine status into a window's Preferences props: the enabled
/// toggle, the host/model field seeds, and a human-readable status line.
fn apply_ai_status_props(app: &AppWindow, st: &hyperpanes_core::ai::service::AiStatus) {
    app.set_pref_ai_enabled(st.enabled);
    app.set_pref_ai_host(st.endpoint.clone().into());
    app.set_pref_ai_model(st.model.clone().into());
    let line = if !st.enabled {
        "Disabled".to_string()
    } else if st.online {
        format!("Running · connected to {}", st.endpoint)
    } else if let Some(err) = &st.last_error {
        format!("Enabled · error: {err}")
    } else {
        "Enabled · connecting…".to_string()
    };
    app.set_pref_ai_status(line.into());
}

/// The human-readable Control-API status line for the Preferences section: off, or running
/// with its loopback port + whether input is allowed.
fn control_status_line(enabled: bool, allow_input: bool, port: Option<u16>) -> String {
    if !enabled {
        return "Off".to_string();
    }
    let input = if allow_input { "input allowed" } else { "input blocked" };
    match port {
        Some(p) => format!("Running · http://127.0.0.1:{p} · {input}"),
        None => format!("Starting… · {input}"),
    }
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
