//! Hosting the embedded control HTTP+WS server **inside the native GUI** — so the MCP /
//! agent-orchestration plane drives this process exactly like Electron (and the headless
//! `core::app::run`) do, but fed the **live GUI state**.
//!
//! The single-process win: a `/command` (open_pane / focus / set_meta / …) is applied
//! synchronously to `core::control`'s read-model + the shared `SessionManager`, and reflected
//! back into the live GUI on the UI thread — no renderer round-trip, no 504/echo race.
//!
//! ## Wiring (mirrors `core::app::run`, but live)
//!  * The control server runs on a **tokio task** over the app's one shared `SessionManager`
//!    (so every PTY a control command spawns is the *same* engine the GUI renders).
//!  * [`ControlHost::sync`] runs each UI-thread tick: it **publishes** the live windows→tabs→
//!    panes tree into `core::control`'s read-model (so `/state` / `list_panes` reflect the GUI)
//!    and **reconciles** any control-originated structural change (a `/command newPane`,
//!    `closePane`, `focusPane`, `renamePane`, `recolorPane`, `setMeta`) back into the GUI's
//!    [`State`] — always on the UI thread, never mutating Slint state off-thread.
//!  * Session events are **teed** to the server ([`ControlHost::tee_event`]) so `/events` WS
//!    output frames + the model's cwd/exit tracking stay live.
//!
//! Gated by `core::persistence::control_settings` (default OFF). Toggling Enabled starts/stops
//! the server live; toggling Allow-Input flips `allow_input` on the running server.
//!
//! Env overrides (parity with the headless bin / Electron, for the MCP acceptance gate):
//!   * `HYPERPANES_CONTROL_FILE` — discovery file path (also injected into spawned panes).
//!   * `HYPERPANES_ALLOW_INPUT`  — `1`/`true`/`yes` forces `allowInput` on (else from settings).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use hyperpanes_core::app::VERSION;
use hyperpanes_core::control::readmodel::{PaneInfo, PaneStatus, ReadModel, TabInfo, WindowInfo};
use hyperpanes_core::control::server::{self, notify_state, Shared};
use hyperpanes_core::persistence::{control_settings, paths};
use hyperpanes_core::session_manager::{SessionEvent, SessionManager};

use slint::Color;

use crate::app::Window;
use crate::state::{parse_hex, DetachedPane};

/// The pane fields the control plane (not the GUI) owns: a pane's launch spec (`command` /
/// `args` / `shell`) and its orchestration `meta`. The GUI never edits these, so we carry them
/// in a side store keyed by session uid and re-stamp them onto the read-model each publish
/// (otherwise the wholesale rebuild from GUI state would drop them).
#[derive(Default, Clone)]
struct CtlFields {
    command: Option<String>,
    args: Option<Vec<String>>,
    shell: Option<String>,
    meta: Option<BTreeMap<String, String>>,
}

/// A baseline snapshot of one pane's chrome as last written to the read-model, so the next
/// tick can diff the model (which a `/command` may have mutated off-thread) against what the
/// GUI published — the delta is exactly what the control plane changed.
#[derive(Clone)]
struct PaneSnap {
    label: String,
    color: String,
    subtitle: Option<String>,
}

/// Hosts the embedded control server beside the GUI. UI-thread-owned (all interior mutability
/// is single-threaded `Cell`/`RefCell`); only the `Arc<Shared>` it hands to the tokio task is
/// shared across threads.
pub struct ControlHost {
    enabled: Cell<bool>,
    allow_input: Cell<bool>,
    control_file: PathBuf,
    /// The tokio runtime the server tasks run on. Captured once (the app enters the runtime
    /// guard before building the host) and used for every `spawn`, so a spawn from the UI thread
    /// never depends on the ambient thread-local guard being present.
    runtime: Handle,
    /// The running server's shared state (`None` when stopped).
    shared: RefCell<Option<Arc<Shared>>>,
    /// The serve task handle (aborted on stop).
    task: RefCell<Option<JoinHandle<std::io::Result<()>>>>,
    /// The activity-ticker task handle (aborted on stop — it would otherwise loop forever holding
    /// an `Arc<Shared>`, leaking one ticker per disable→enable toggle).
    ticker: RefCell<Option<JoinHandle<()>>>,
    // ---- sync baselines (UI thread only) ----
    /// Stable control pane-id per GUI session uid (GUI panes use the uid itself; a control-
    /// created pane keeps the uuid `dispatch` minted).
    pane_ids: RefCell<HashMap<String, String>>,
    /// Control-owned launch/meta fields per session uid.
    ctl: RefCell<HashMap<String, CtlFields>>,
    /// The read-model panes as last published (baseline for the next reconcile diff).
    prev: RefCell<HashMap<String, PaneSnap>>,
    /// The active tab id per window as last published (baseline for focus reconcile).
    prev_active: RefCell<HashMap<i64, Option<String>>>,
    /// The window ids present in the read-model (so a rebuild can drop them all first).
    prev_windows: RefCell<Vec<i64>>,
}

impl ControlHost {
    /// Build the host from persisted `control-settings.json` (+ env overrides) and start the
    /// server immediately if it is enabled.
    pub fn new(mgr: &Arc<SessionManager>) -> Self {
        let settings = control_settings::load();
        let control_file = std::env::var_os("HYPERPANES_CONTROL_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(paths::control_json);
        let allow_input = settings.allow_input || env_truthy("HYPERPANES_ALLOW_INPUT");
        let host = ControlHost {
            enabled: Cell::new(settings.enabled),
            allow_input: Cell::new(allow_input),
            control_file,
            // The app enters the tokio runtime guard before constructing the host, so the current
            // handle is always available here.
            runtime: Handle::current(),
            shared: RefCell::new(None),
            task: RefCell::new(None),
            ticker: RefCell::new(None),
            pane_ids: RefCell::new(HashMap::new()),
            ctl: RefCell::new(HashMap::new()),
            prev: RefCell::new(HashMap::new()),
            prev_active: RefCell::new(HashMap::new()),
            prev_windows: RefCell::new(Vec::new()),
        };
        if host.enabled.get() {
            host.start(mgr);
        }
        host
    }

    // ---- lifecycle ----

    /// Bind + serve on a fresh `Shared` over the shared engine (mirrors `server::run_server`:
    /// ephemeral loopback port, master token, `control.json` discovery file). The activity ticker
    /// is spawned as a SEPARATE task so [`Self::stop`] can abort it (it loops forever otherwise).
    /// Every spawn goes through the stored runtime `Handle` (never the ambient guard).
    fn start(&self, mgr: &Arc<SessionManager>) {
        if self.shared.borrow().is_some() {
            return;
        }
        let shared = Shared::new(
            Arc::clone(mgr),
            self.allow_input.get(),
            VERSION,
            self.control_file.clone(),
        );
        // Bind the server's own background spawns (the `notify_state` coalescer) to this runtime.
        shared.set_runtime(self.runtime.clone());
        let task = self.runtime.spawn(server::run_server(Arc::clone(&shared)));
        let ticker = self.runtime.spawn(server::run_activity_ticker(Arc::clone(&shared)));
        *self.shared.borrow_mut() = Some(shared);
        *self.task.borrow_mut() = Some(task);
        *self.ticker.borrow_mut() = Some(ticker);
    }

    /// Stop the server: abort the serve task AND the activity ticker, drop every WS client (so
    /// their `handle_ws` tasks see the channel close and exit, releasing their `Arc<Shared>`), and
    /// remove the stale discovery file. Nothing is left looping or retaining `Shared` after this.
    fn stop(&self) {
        if let Some(t) = self.task.borrow_mut().take() {
            t.abort();
        }
        if let Some(t) = self.ticker.borrow_mut().take() {
            t.abort();
        }
        if let Some(s) = self.shared.borrow_mut().take() {
            s.events.clear_clients();
            server::remove_discovery(&s);
        }
        // Drop the sync baselines so a later re-enable republishes from scratch.
        self.pane_ids.borrow_mut().clear();
        self.ctl.borrow_mut().clear();
        self.prev.borrow_mut().clear();
        self.prev_active.borrow_mut().clear();
        self.prev_windows.borrow_mut().clear();
    }

    /// Toggle the server on/off live, persisting the setting.
    pub fn set_enabled(&self, on: bool, mgr: &Arc<SessionManager>) {
        if self.enabled.get() == on {
            return;
        }
        self.enabled.set(on);
        self.persist();
        if on {
            self.start(mgr);
        } else {
            self.stop();
        }
    }

    /// Flip `allow_input` live (gates `/panes/{id}/input`), persisting the setting.
    pub fn set_allow_input(&self, on: bool) {
        if self.allow_input.get() == on {
            return;
        }
        self.allow_input.set(on);
        self.persist();
        if let Some(s) = self.shared.borrow().as_ref() {
            s.allow_input.store(on, Ordering::SeqCst);
        }
    }

    fn persist(&self) {
        let _ = control_settings::save(&control_settings::ControlSettings {
            enabled: self.enabled.get(),
            allow_input: self.allow_input.get(),
        });
    }

    /// `(enabled, allow_input, port-if-running)` for the Preferences status line.
    pub fn status(&self) -> (bool, bool, Option<u16>) {
        let port = self.shared.borrow().as_ref().map(|s| s.port()).filter(|p| *p != 0);
        (self.enabled.get(), self.allow_input.get(), port)
    }

    // ---- live event tee ----

    /// Forward one session event to the running server (model cwd/exit + `/events` WS frames).
    /// Cheap no-op when stopped; the Data path inside short-circuits when no WS clients.
    pub fn tee_event(&self, ev: &SessionEvent) {
        if let Some(s) = self.shared.borrow().as_ref() {
            server::process_session_event(s, ev.clone());
        }
    }

    // ---- per-tick reconcile + publish ----

    /// The read-model bridge: reconcile any control-originated structural change back into the
    /// live GUI (on the UI thread), then republish the GUI tree into the read-model. No-op when
    /// the server is stopped.
    pub fn sync(&self, windows: &[Rc<Window>], mgr: &Arc<SessionManager>) {
        let shared = match self.shared.borrow().as_ref() {
            Some(s) => Arc::clone(s),
            None => return,
        };

        // 1. Snapshot the read-model (it may have been mutated off-thread by a `/command`).
        let (cur, cur_active, focus_uid) = self.snapshot_model(&shared, windows);

        // 2. Reconcile the model→GUI deltas (what the control plane changed) on the UI thread.
        let reconciled = self.reconcile(windows, mgr, &cur, &cur_active, &focus_uid);

        // 3. Republish the (now-updated) live GUI tree into the read-model.
        let republished = self.publish(&shared, windows);

        // 4. Nudge WS clients if the published structure changed (GUI- or control-driven).
        if reconciled || republished {
            notify_state(&shared);
        }
    }

    /// Read every pane the read-model currently holds (keyed by session uid), each GUI window's
    /// active tab id, and a representative session uid living in each window's active tab. The
    /// representative uid lets the focus reconcile resolve the focused tab by a pane that's
    /// actually in it (stable across GUI tab reorder/close) rather than parsing the positional id.
    fn snapshot_model(
        &self,
        shared: &Arc<Shared>,
        windows: &[Rc<Window>],
    ) -> (
        HashMap<String, ModelPane>,
        HashMap<i64, Option<String>>,
        HashMap<i64, Option<String>>,
    ) {
        let model = shared.model.lock().unwrap();
        let mut cur = HashMap::new();
        for pr in model.panes() {
            if let Some(p) = model.pane(&pr.pane_id) {
                cur.insert(
                    p.session_uid.clone(),
                    ModelPane {
                        pane_id: p.id.clone(),
                        window_id: pr.coords.window_id,
                        tab_id: pr.coords.tab_id.clone(),
                        label: p.label.clone(),
                        color: p.color.clone(),
                        subtitle: p.subtitle.clone(),
                        command: p.command.clone(),
                        args: p.args.clone(),
                        shell: p.shell.clone(),
                        cwd: p.cwd.clone(),
                        meta: p.meta.clone(),
                    },
                );
            }
        }
        let mut active = HashMap::new();
        let mut focus_uid = HashMap::new();
        for w in windows {
            let wid = w.id as i64;
            let at = model.active_tab_id(wid);
            let rep = at
                .as_ref()
                .and_then(|tid| cur.iter().find(|(_, m)| &m.tab_id == tid).map(|(u, _)| u.clone()));
            active.insert(wid, at);
            focus_uid.insert(wid, rep);
        }
        (cur, active, focus_uid)
    }

    /// Apply control-originated deltas (diffing the model snapshot against the last published
    /// baseline) to the live GUI state. Returns whether anything structural changed (add/remove).
    fn reconcile(
        &self,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
        cur: &HashMap<String, ModelPane>,
        cur_active: &HashMap<i64, Option<String>>,
        focus_uid: &HashMap<i64, Option<String>>,
    ) -> bool {
        let prev = self.prev.borrow();
        let mut ctl = self.ctl.borrow_mut();
        let state_uids = gui_uids(windows);
        let mut structural = false;
        // Model tab id → the GUI tab a control-spawned tab was materialized into THIS tick, so the
        // 2nd…nth pane of an `attach as:tab` group joins the same new tab instead of each making one.
        let mut created_tabs: HashMap<String, (i64, usize)> = HashMap::new();

        // Refresh control-owned fields (command/args/shell/meta) for every model pane.
        for (uid, c) in cur {
            ctl.insert(
                uid.clone(),
                CtlFields {
                    command: c.command.clone(),
                    args: c.args.clone(),
                    shell: c.shell.clone(),
                    meta: c.meta.clone(),
                },
            );
        }

        for (uid, c) in cur {
            if !state_uids.contains(uid) {
                if prev.contains_key(uid) {
                    // The GUI removed it this tick; the republish will drop it from the model.
                    continue;
                }
                // A uid new to the GUI. Distinguish a RESPAWN (restartPane swaps a pane's
                // session_uid while keeping its stable pane_id — the GUI still hosts the OLD uid
                // under that pane_id) from a genuinely new control-spawned pane.
                let respawn_of = {
                    let ids = self.pane_ids.borrow();
                    gui_uid_for_pane_id(windows, &ids, &c.pane_id)
                };
                match respawn_of {
                    Some(old_uid) if old_uid != *uid => {
                        // Rebind the existing GUI pane to the new session in place — no duplicate
                        // adoption, no dropped terminal.
                        self.rebind_respawn(windows, mgr, &old_uid, uid, c);
                    }
                    _ => {
                        // Adopt the already-live session into the tab the MODEL placed it in
                        // (replay-primed, no PTY restart).
                        self.adopt_control_pane(windows, mgr, uid, c, cur, &mut created_tabs);
                        self.pane_ids.borrow_mut().insert(uid.clone(), c.pane_id.clone());
                    }
                }
                structural = true;
            } else if let Some(p) = prev.get(uid) {
                // Present on both sides: apply a control rename / recolor / subtitle change.
                if c.label != p.label || c.color != p.color || c.subtitle != p.subtitle {
                    apply_pane_chrome(windows, uid, c);
                }
            }
        }

        // Control `closePane` removed it from the model: drop it from the GUI too (the PTY was
        // already killed by `dispatch`, so detach without re-killing).
        for (uid, _) in prev.iter() {
            if state_uids.contains(uid) && !cur.contains_key(uid) {
                remove_from_gui(windows, uid);
                structural = true;
            }
        }

        // Control `focusPane` flipped a window's active tab: mirror the tab switch. Resolve the
        // focused tab by a pane that actually lives in it (stable across tab reorder/close),
        // falling back to the positional id only when the active tab is empty.
        let prev_active = self.prev_active.borrow();
        for (wid, act) in cur_active {
            if prev_active.get(wid).map(|a| a.as_deref()) == Some(act.as_deref()) {
                continue;
            }
            let Some(w) = windows.iter().find(|w| w.id as i64 == *wid) else { continue };
            if act.is_none() {
                continue;
            }
            let by_uid = focus_uid
                .get(wid)
                .and_then(|u| u.as_ref())
                .and_then(|u| w.state.borrow_mut().find_pane(u).map(|(ti, _)| ti));
            let idx = by_uid.or_else(|| act.as_deref().and_then(parse_tab_index));
            if let Some(idx) = idx {
                w.state.borrow_mut().switch_tab(idx);
            }
        }
        drop(prev_active);

        // Prune side-store entries for panes that no longer exist in the GUI.
        let live = gui_uids(windows);
        ctl.retain(|uid, _| live.contains(uid));
        self.pane_ids.borrow_mut().retain(|uid, _| live.contains(uid));
        structural
    }

    /// Rebind an existing GUI pane (currently bound to `old_uid`) to a control-respawned session
    /// `new_uid` in place: clear the dead session's stale grid, re-prime from the new session's
    /// replay buffer, re-arm startup gating, and re-pin the stable control pane-id onto the new
    /// uid (so the republish keeps the pane's id steady for the MCP client).
    fn rebind_respawn(
        &self,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
        old_uid: &str,
        new_uid: &str,
        c: &ModelPane,
    ) {
        for w in windows {
            let mut st = w.state.borrow_mut();
            if let Some((ti, pi)) = st.find_pane(old_uid) {
                let p = &mut st.tabs[ti].panes[pi];
                p.uid = new_uid.to_string();
                p.pane.clear();
                if let Some(replay) = mgr.replay(new_uid) {
                    p.pane.feed(&replay);
                }
                p.started = true;
                p.cwd = c.cwd.clone();
                st.dirty = true;
                drop(st);
                let mut ids = self.pane_ids.borrow_mut();
                ids.remove(old_uid);
                ids.insert(new_uid.to_string(), c.pane_id.clone());
                return;
            }
        }
    }

    /// Wholesale-rebuild the read-model from the live GUI tree, re-stamping the control-owned
    /// fields, and refresh the baselines for the next reconcile. Returns whether the published
    /// structure (pane set or active tabs) changed versus the previous publish.
    fn publish(&self, shared: &Arc<Shared>, windows: &[Rc<Window>]) -> bool {
        let pane_ids = self.pane_ids.borrow();
        let ctl = self.ctl.borrow();
        let mut model = shared.model.lock().unwrap();

        // Drop every window we previously published, then rebuild from GUI state.
        for wid in self.prev_windows.borrow_mut().drain(..) {
            model.drop_window(wid);
        }

        let mut new_windows = Vec::new();
        let mut new_prev = HashMap::new();
        let mut new_active = HashMap::new();
        for w in windows {
            let wid = w.id as i64;
            new_windows.push(wid);
            let st = w.state.borrow();
            let active_tab_id = Some(format!("{wid}:{}", st.active));
            new_active.insert(wid, active_tab_id.clone());
            let mut tabs = Vec::new();
            for (ti, tab) in st.tabs.iter().enumerate() {
                let tab_id = format!("{wid}:{ti}");
                let mut panes = Vec::new();
                for p in &tab.panes {
                    let uid = p.uid.clone();
                    let pane_id = pane_ids.get(&uid).cloned().unwrap_or_else(|| uid.clone());
                    let label = p.title.to_string();
                    let color = color_hex(p.accent);
                    let subtitle = p.subtitle.as_ref().map(|s| s.to_string());
                    let c = ctl.get(&uid).cloned().unwrap_or_default();
                    new_prev.insert(
                        uid.clone(),
                        PaneSnap {
                            label: label.clone(),
                            color: color.clone(),
                            subtitle: subtitle.clone(),
                        },
                    );
                    panes.push(PaneInfo {
                        id: pane_id,
                        session_uid: uid,
                        label,
                        subtitle,
                        color,
                        command: c.command,
                        args: c.args,
                        cwd: p.cwd.clone(),
                        shell: c.shell,
                        status: PaneStatus::Running,
                        exit_code: None,
                        meta: c.meta,
                    });
                }
                tabs.push(TabInfo {
                    id: tab_id,
                    title: tab.title.to_string(),
                    layout: crate::theme::layout_name(tab.layout).to_string(),
                    panes,
                });
            }
            model.add_window(WindowInfo { window_id: wid, active_tab_id, tabs });
        }
        drop(model);

        // Did the structure (which panes exist, or which tab is active) change since last publish?
        let structural = {
            let old = self.prev.borrow();
            let old_active = self.prev_active.borrow();
            let panes_changed = old.len() != new_prev.len()
                || new_prev.keys().any(|uid| !old.contains_key(uid));
            panes_changed || *old_active != new_active
        };

        *self.prev_windows.borrow_mut() = new_windows;
        *self.prev.borrow_mut() = new_prev;
        *self.prev_active.borrow_mut() = new_active;
        structural
    }

    /// Adopt a control-spawned session into the GUI: build a [`DetachedPane`] for the live uid and
    /// re-host it into the tab the MODEL placed it in (the PTY already exists). The target tab is
    /// resolved (in order): a sibling pane already in the GUI for the same model tab → a tab made
    /// this tick for that model tab → the positional model tab id → otherwise a brand-new GUI tab
    /// (an `attach as:tab` group). Adopting into a background tab does NOT steal the user's focus.
    fn adopt_control_pane(
        &self,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
        uid: &str,
        c: &ModelPane,
        cur: &HashMap<String, ModelPane>,
        created_tabs: &mut HashMap<String, (i64, usize)>,
    ) {
        let target = windows
            .iter()
            .find(|w| w.id as i64 == c.window_id)
            .or_else(|| windows.first());
        let Some(w) = target else { return };
        let accent = parse_hex(&c.color);
        let font_px = w.state.borrow().settings.font_px;
        let det = DetachedPane {
            uid: uid.to_string(),
            title: c.label.clone().into(),
            subtitle: c.subtitle.clone().map(Into::into),
            pinned_accent: Some(accent),
            show_frame: Some(true),
            show_dot: Some(true),
            font_px,
        };

        // Resolve the GUI tab index this pane belongs in (None ⇒ it needs a brand-new tab).
        let target_ti = self.resolve_adopt_tab(w, c, uid, cur, created_tabs);

        match target_ti {
            Some(ti) => {
                let mut st = w.state.borrow_mut();
                let saved = st.active;
                // adopt_pane targets the ACTIVE tab; switch to the target, adopt, switch back so
                // a background adopt never moves the user's current tab.
                st.switch_tab(ti);
                st.adopt_pane(mgr, det);
                st.switch_tab(saved);
            }
            None => {
                let mut st = w.state.borrow_mut();
                let saved = st.active;
                st.adopt_pane_as_tab(mgr, det);
                let new_ti = st.tabs.len().saturating_sub(1);
                st.switch_tab(saved);
                created_tabs.insert(c.tab_id.clone(), (c.window_id, new_ti));
            }
        }

        if let Some(cwd) = &c.cwd {
            let mut st = w.state.borrow_mut();
            if let Some((ti, pi)) = st.find_pane(uid) {
                st.tabs[ti].panes[pi].cwd = Some(cwd.clone());
            }
        }
    }

    /// Resolve which existing GUI tab index a control-spawned pane should be adopted into, or
    /// `None` if the model placed it in a tab the GUI doesn't host yet (→ make a new tab).
    fn resolve_adopt_tab(
        &self,
        w: &Rc<Window>,
        c: &ModelPane,
        uid: &str,
        cur: &HashMap<String, ModelPane>,
        created_tabs: &HashMap<String, (i64, usize)>,
    ) -> Option<usize> {
        // 1. A sibling pane in the same model tab that the GUI already hosts → its live tab.
        for (sib_uid, m) in cur {
            if sib_uid != uid && m.tab_id == c.tab_id {
                if let Some((ti, _)) = w.state.borrow_mut().find_pane(sib_uid) {
                    return Some(ti);
                }
            }
        }
        // 2. A tab we materialized for this model tab earlier this tick (same window).
        if let Some((wid, ti)) = created_tabs.get(&c.tab_id) {
            if *wid == c.window_id {
                return Some(*ti);
            }
        }
        // 3. A positional "{window_id}:{index}" model tab id that maps to a live GUI tab.
        if let Some(idx) = parse_tab_index(&c.tab_id) {
            if idx < w.state.borrow().tabs.len() {
                return Some(idx);
            }
        }
        // 4. Otherwise the model put it in a brand-new tab → signal "make one".
        None
    }
}

/// A pane as read from the control read-model.
struct ModelPane {
    pane_id: String,
    window_id: i64,
    tab_id: String,
    label: String,
    color: String,
    subtitle: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    shell: Option<String>,
    cwd: Option<String>,
    meta: Option<BTreeMap<String, String>>,
}

/// The GUI session uid currently mapped to control `pane_id`, if any. A GUI pane's effective
/// pane-id is its own uid unless `pane_ids` pins a control-minted id onto it. Used to detect a
/// respawn: the model carries a new `session_uid` under a still-live stable `pane_id`.
fn gui_uid_for_pane_id(
    windows: &[Rc<Window>],
    pane_ids: &HashMap<String, String>,
    pane_id: &str,
) -> Option<String> {
    for uid in gui_uids(windows) {
        let effective = pane_ids.get(&uid).map(String::as_str).unwrap_or(uid.as_str());
        if effective == pane_id {
            return Some(uid);
        }
    }
    None
}

/// Every session uid the GUI currently hosts across all windows + tabs.
fn gui_uids(windows: &[Rc<Window>]) -> HashSet<String> {
    let mut set = HashSet::new();
    for w in windows {
        for t in &w.state.borrow().tabs {
            for p in &t.panes {
                set.insert(p.uid.clone());
            }
        }
    }
    set
}

/// Apply a control-originated label / color / subtitle change to the GUI pane with `uid`.
fn apply_pane_chrome(windows: &[Rc<Window>], uid: &str, c: &ModelPane) {
    for w in windows {
        let mut st = w.state.borrow_mut();
        if let Some((ti, pi)) = st.find_pane(uid) {
            let accent = parse_hex(&c.color);
            let p = &mut st.tabs[ti].panes[pi];
            p.title = c.label.clone().into();
            p.accent = accent;
            p.pinned_accent = Some(accent);
            p.subtitle = c.subtitle.clone().map(Into::into);
            st.dirty = true;
            return;
        }
    }
}

/// Remove the GUI pane with `uid` without killing its (already-dead) session.
fn remove_from_gui(windows: &[Rc<Window>], uid: &str) {
    for w in windows {
        let has = w.state.borrow_mut().find_pane(uid).is_some();
        if has {
            let _ = w.state.borrow_mut().detach_uid(uid);
            return;
        }
    }
}

/// Parse the tab index out of a `"{window_id}:{tab_index}"` id.
fn parse_tab_index(tab_id: &str) -> Option<usize> {
    tab_id.rsplit(':').next()?.parse().ok()
}

/// Format a Slint color as `#rrggbb` (the read-model's `color` shape).
fn color_hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

fn env_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}
