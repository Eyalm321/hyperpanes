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
    /// The running server's shared state (`None` when stopped).
    shared: RefCell<Option<Arc<Shared>>>,
    /// The serve task handle (aborted on stop).
    task: RefCell<Option<tokio::task::JoinHandle<std::io::Result<()>>>>,
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
            shared: RefCell::new(None),
            task: RefCell::new(None),
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
    /// ephemeral loopback port, master token, `control.json` discovery file, activity ticker).
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
        let task = tokio::spawn(server::run_server(Arc::clone(&shared)));
        *self.shared.borrow_mut() = Some(shared);
        *self.task.borrow_mut() = Some(task);
    }

    /// Stop the server: abort the serve task and remove the stale discovery file. (The activity
    /// ticker `run_server` spawned is detached; it idles harmlessly until the process exits.)
    fn stop(&self) {
        if let Some(t) = self.task.borrow_mut().take() {
            t.abort();
        }
        if let Some(s) = self.shared.borrow_mut().take() {
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
        let (cur, cur_active) = self.snapshot_model(&shared, windows);

        // 2. Reconcile the model→GUI deltas (what the control plane changed) on the UI thread.
        let reconciled = self.reconcile(windows, mgr, &cur, &cur_active);

        // 3. Republish the (now-updated) live GUI tree into the read-model.
        let republished = self.publish(&shared, windows);

        // 4. Nudge WS clients if the published structure changed (GUI- or control-driven).
        if reconciled || republished {
            notify_state(&shared);
        }
    }

    /// Read every pane the read-model currently holds (keyed by session uid) plus each GUI
    /// window's active tab id.
    fn snapshot_model(
        &self,
        shared: &Arc<Shared>,
        windows: &[Rc<Window>],
    ) -> (HashMap<String, ModelPane>, HashMap<i64, Option<String>>) {
        let model = shared.model.lock().unwrap();
        let mut cur = HashMap::new();
        for pr in model.panes() {
            if let Some(p) = model.pane(&pr.pane_id) {
                cur.insert(
                    p.session_uid.clone(),
                    ModelPane {
                        pane_id: p.id.clone(),
                        window_id: pr.coords.window_id,
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
        for w in windows {
            active.insert(w.id as i64, model.active_tab_id(w.id as i64));
        }
        (cur, active)
    }

    /// Apply control-originated deltas (diffing the model snapshot against the last published
    /// baseline) to the live GUI state. Returns whether anything structural changed (add/remove).
    fn reconcile(
        &self,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
        cur: &HashMap<String, ModelPane>,
        cur_active: &HashMap<i64, Option<String>>,
    ) -> bool {
        let prev = self.prev.borrow();
        let mut ctl = self.ctl.borrow_mut();
        let state_uids = gui_uids(windows);
        let mut structural = false;

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
                // A `/command newPane` (or `attach`) spawned it: adopt the already-live session
                // into the target window's active tab (replay-primed, no PTY restart).
                self.adopt_control_pane(windows, mgr, uid, c);
                self.pane_ids.borrow_mut().insert(uid.clone(), c.pane_id.clone());
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

        // Control `focusPane` flipped a window's active tab: mirror the tab switch.
        let prev_active = self.prev_active.borrow();
        for (wid, act) in cur_active {
            if prev_active.get(wid).map(|a| a.as_deref()) != Some(act.as_deref()) {
                if let Some(tab_id) = act {
                    if let Some(idx) = parse_tab_index(tab_id) {
                        if let Some(w) = windows.iter().find(|w| w.id as i64 == *wid) {
                            w.state.borrow_mut().switch_tab(idx);
                        }
                    }
                }
            }
        }
        drop(prev_active);

        // Prune side-store entries for panes that no longer exist in the GUI.
        let live = gui_uids(windows);
        ctl.retain(|uid, _| live.contains(uid));
        self.pane_ids.borrow_mut().retain(|uid, _| live.contains(uid));
        structural
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

    /// Adopt a control-spawned session into the GUI: build a [`DetachedPane`] for the live uid
    /// and re-host it into the target window's active tab (the PTY already exists).
    fn adopt_control_pane(
        &self,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
        uid: &str,
        c: &ModelPane,
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
        w.state.borrow_mut().adopt_pane(mgr, det);
        if let Some(cwd) = &c.cwd {
            let mut st = w.state.borrow_mut();
            if let Some((ti, pi)) = st.find_pane(uid) {
                st.tabs[ti].panes[pi].cwd = Some(cwd.clone());
            }
        }
    }
}

/// A pane as read from the control read-model.
struct ModelPane {
    pane_id: String,
    window_id: i64,
    label: String,
    color: String,
    subtitle: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    shell: Option<String>,
    cwd: Option<String>,
    meta: Option<BTreeMap<String, String>>,
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
