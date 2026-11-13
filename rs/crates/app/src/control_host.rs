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
//! Control `renameTab` / `recolorTab` land in the read model and reach the GUI through the tab
//! reconcile (`diff_tabs` / `apply_tab_change`), alongside `setLayout`.
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
    /// Spawn env (in-memory only; never serialized) so a restart can reuse it.
    env: Option<hyperpanes_core::session::spawn::EnvMap>,
}

impl CtlFields {
    /// The control-owned fields of a model pane, carried across the GUI republish. Includes the
    /// spawn `env` so a later `restartPane` can reuse it (it is never serialized to clients).
    fn from_model(c: &ModelPane) -> Self {
        CtlFields {
            command: c.command.clone(),
            args: c.args.clone(),
            shell: c.shell.clone(),
            meta: c.meta.clone(),
            env: c.env.clone(),
        }
    }
}

/// The launch spec (command/args/shell/env) to publish for a pane. Control-owned fields win; a
/// pane the GUI itself spawned (launchers, New Pane dialog, restored sessions) has no control
/// entry, so its own recorded spawn spec is published instead. Without it a resume has no
/// program or argv to relaunch and falls back to typing a bare `claude --resume`. The env is
/// carried into the read model for the resume path only; it is never serialized into `/state`.
fn effective_launch_spec(
    c: CtlFields,
    gui_command: Option<&String>,
    gui_args: Option<&Vec<String>>,
    gui_shell: Option<&String>,
    gui_env: Option<&hyperpanes_core::session::spawn::EnvMap>,
) -> CtlFields {
    let (command, args, shell) = if c.command.is_some() {
        (c.command, c.args, c.shell)
    } else {
        (
            gui_command.cloned(),
            gui_args.cloned(),
            c.shell.or_else(|| gui_shell.cloned()),
        )
    };
    CtlFields {
        command,
        args,
        shell,
        meta: c.meta,
        env: c.env.or_else(|| gui_env.cloned()),
    }
}

/// Meta queued for a pane that may not be in the read model yet (see `ControlHost::queue_meta`).
struct PendingMeta {
    /// Session uid of the target pane.
    uid: String,
    /// Keys to set on it.
    meta: BTreeMap<String, String>,
    /// Ticks spent waiting for the pane to appear.
    waited: u32,
}

/// A baseline snapshot of one pane's chrome as last written to the read-model, so the next
/// tick can diff the model (which a `/command` may have mutated off-thread) against what the
/// GUI published — the delta is exactly what the control plane changed.
#[derive(Clone)]
struct PaneSnap {
    label: String,
    color: String,
    subtitle: Option<String>,
    talk: bool,
}

/// A tab's model-visible chrome (title + layout name), the unit the tab reconcile diffs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TabSnap {
    title: String,
    layout: String,
    /// Manual tab colour, `#rrggbb`.
    color: Option<String>,
}

/// Model tab id → (window id, title/layout snapshot).
type TabSnaps = HashMap<String, (i64, TabSnap)>;

/// One tab-level change a control command made to the model since the last publish.
#[derive(Debug, PartialEq, Eq)]
struct TabChange {
    tab_id: String,
    title: Option<String>,
    layout: Option<String>,
    /// `Some(new)` when the colour changed; the inner `None` means cleared.
    color: Option<Option<String>>,
}

/// Pure diff: tabs present in both baselines whose title/layout differ. A tab absent from
/// `prev` is new (materialized by the adopt path, not here); absent from `cur` is gone.
fn diff_tabs(prev: &HashMap<String, TabSnap>, cur: &HashMap<String, TabSnap>) -> Vec<TabChange> {
    let mut out: Vec<TabChange> = cur
        .iter()
        .filter_map(|(id, c)| {
            let p = prev.get(id)?;
            let title = (c.title != p.title).then(|| c.title.clone());
            let layout = (c.layout != p.layout).then(|| c.layout.clone());
            let color = (c.color != p.color).then(|| c.color.clone());
            (title.is_some() || layout.is_some() || color.is_some()).then(|| TabChange {
                tab_id: id.clone(),
                title,
                layout,
                color,
            })
        })
        .collect();
    out.sort_by(|a, b| a.tab_id.cmp(&b.tab_id));
    out
}

/// Apply one [`TabChange`] to a window's state without moving its active tab. Returns whether
/// anything was applied (false for an out-of-range tab or an unknown layout name).
fn apply_tab_change(st: &mut crate::state::State, idx: usize, ch: &TabChange) -> bool {
    if idx >= st.tabs.len() {
        return false;
    }
    let mut applied = false;
    if let Some(l) = ch.layout.as_deref().and_then(crate::state::parse_layout) {
        st.set_tab_layout(idx, l);
        applied = true;
    }
    if let Some(t) = &ch.title {
        st.set_tab_title(idx, t);
        applied = true;
    }
    if let Some(c) = &ch.color {
        st.set_tab_color(idx, c.as_deref().and_then(crate::state::parse_hex_strict));
        applied = true;
    }
    applied
}

/// Hosts the embedded control server beside the GUI. UI-thread-owned (all interior mutability
/// is single-threaded `Cell`/`RefCell`); only the `Arc<Shared>` it hands to the tokio task is
/// shared across threads.
pub struct ControlHost {
    enabled: Cell<bool>,
    allow_input: Cell<bool>,
    /// Requested bind `(address, port)` from `control-settings.json` (`None` = loopback
    /// ephemeral). Read once at construction; a settings-file edit needs a restart (or an
    /// Enabled toggle) to re-bind.
    control_file: PathBuf,
    /// The tokio runtime the server tasks run on. Captured once (the app enters the runtime
    /// guard before building the host) and used for every `spawn`, so a spawn from the UI thread
    /// never depends on the ambient thread-local guard being present.
    runtime: Handle,
    /// The running server's shared state (`None` when stopped).
    shared: RefCell<Option<Arc<Shared>>>,
    /// When the work queue was last polled for the sidebar badge (#6).
    queue_polled: std::cell::Cell<Option<std::time::Instant>>,
    /// The serve task handle (aborted on stop).
    task: RefCell<Option<JoinHandle<std::io::Result<()>>>>,
    /// The activity-ticker task handle (aborted on stop — it would otherwise loop forever holding
    /// an `Arc<Shared>`, leaking one ticker per disable→enable toggle).
    ticker: RefCell<Option<JoinHandle<()>>>,
    /// The work-queue reaper-ticker handle (aborted on stop, same leak reasoning as `ticker`).
    reaper: RefCell<Option<JoinHandle<()>>>,
    // ---- sync baselines (UI thread only) ----
    /// Stable control pane-id per GUI session uid (GUI panes use the uid itself; a control-
    /// created pane keeps the uuid `dispatch` minted).
    pane_ids: RefCell<HashMap<String, String>>,
    /// Control-owned launch/meta fields per session uid.
    ctl: RefCell<HashMap<String, CtlFields>>,
    /// Meta the GUI wants stamped on a pane once it is in the read model — `(session uid, meta,
    /// ticks waited)`. Fed by [`Self::queue_meta`], applied after each publish. Needed because a
    /// pane restored this tick is not in the model until the publish that follows.
    pending_meta: RefCell<Vec<PendingMeta>>,
    /// `(old pane id, new pane id)` restore aliases waiting for the control server to exist.
    pending_aliases: RefCell<Vec<(String, String)>>,
    /// `movePane` requests whose pane is not in the GUI yet (a `projectTab` newPane adopted on a
    /// later tick), with the ticks waited. Retried by [`Self::apply_pane_group_requests`].
    pending_moves: RefCell<Vec<(server::PaneGroupRequest, u32)>>,
    /// The read-model panes as last published (baseline for the next reconcile diff).
    prev: RefCell<HashMap<String, PaneSnap>>,
    /// Tab title/layout as last published, keyed by model tab id (baseline for tab reconcile).
    prev_tabs: RefCell<HashMap<String, TabSnap>>,
    /// The active tab id per window as last published (baseline for focus reconcile).
    prev_active: RefCell<HashMap<i64, Option<String>>>,
    /// The window ids present in the read-model (so a rebuild can drop them all first).
    prev_windows: RefCell<Vec<i64>>,
    /// Session uids already shown the "talk needs the control server" toast while the server
    /// is stopped, so it fires once per pane (not every sync tick) — cleared once the server
    /// starts, so a later stop→talk-on cycle notices again.
    talk_notice_shown: RefCell<HashSet<String>>,
    /// Self-heal debounce: control-spawned uid → when it was first seen alive-but-untracked
    /// (live in the session manager, missing from BOTH the read-model and the GUI). Healed
    /// only after [`HEAL_DEBOUNCE`] so a transient mid-flight state (e.g. daemon re-attach
    /// during startup) is never misread as a lost pane.
    heal_pending: RefCell<HashMap<String, std::time::Instant>>,
    /// Last-known `(label, meta)` per control-spawned session uid, refreshed every sync while the
    /// read-model holds the pane. The self-heal restores it instead of a generic placeholder.
    last_known: RefCell<HashMap<String, KnownIdentity>>,
}

/// A pane's last-known `(label, meta)`, kept for the self-heal.
type KnownIdentity = (String, Option<BTreeMap<String, String>>);

/// How long a live control-spawned session must stay untracked before the self-heal
/// re-inserts its pane into the read-model.
const HEAL_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

impl ControlHost {
    /// Build the host from persisted `control-settings.json` (+ env overrides) and start the
    /// server immediately if it is enabled.
    pub fn new(mgr: &Arc<SessionManager>) -> Self {
        let settings = control_settings::load();
        // Panes may inherit `HYPERPANES_CONTROL_FILE` set-but-empty from the app; the resolver
        // treats empty as unset (see `hyperpanes pair`'s identical workaround).
        let control_file = paths::resolve_control_file();
        let allow_input = settings.allow_input || env_truthy("HYPERPANES_ALLOW_INPUT");
        let host = ControlHost {
            enabled: Cell::new(settings.enabled),
            allow_input: Cell::new(allow_input),
            control_file,
            // The app enters the tokio runtime guard before constructing the host, so the current
            // handle is always available here.
            runtime: Handle::current(),
            shared: RefCell::new(None),
            queue_polled: std::cell::Cell::new(None),
            task: RefCell::new(None),
            ticker: RefCell::new(None),
            reaper: RefCell::new(None),
            pane_ids: RefCell::new(HashMap::new()),
            ctl: RefCell::new(HashMap::new()),
            pending_meta: RefCell::new(Vec::new()),
            pending_aliases: RefCell::new(Vec::new()),
            pending_moves: RefCell::new(Vec::new()),
            prev: RefCell::new(HashMap::new()),
            prev_tabs: RefCell::new(HashMap::new()),
            prev_active: RefCell::new(HashMap::new()),
            prev_windows: RefCell::new(Vec::new()),
            talk_notice_shown: RefCell::new(HashSet::new()),
            heal_pending: RefCell::new(HashMap::new()),
            last_known: RefCell::new(HashMap::new()),
        };
        if host.enabled.get() {
            host.start(mgr);
        }
        host
    }

    /// The external pane id a control-spawned pane advertises (its `HYPERPANES_PANE_ID` and the
    /// key the Claude session hook writes markers under). `None` for GUI-native panes, whose
    /// pane id IS their session uid.
    pub fn pane_id_for_uid(&self, uid: &str) -> Option<String> {
        self.pane_ids.borrow().get(uid).cloned()
    }

    /// Inverse of [`Self::pane_id_for_uid`]: the session uid hosting the pane that advertises
    /// `pane_id` externally. `None` when no alias exists — for GUI-native panes the caller
    /// falls back to identity (uid == pane id).
    pub fn uid_for_pane_id(&self, pane_id: &str) -> Option<String> {
        self.pane_ids
            .borrow()
            .iter()
            .find(|(_, v)| v.as_str() == pane_id)
            .map(|(k, _)| k.clone())
    }

    /// The control meta of the pane hosted by session `uid` (carried in the control side store,
    /// which mirrors model meta). `None` when it has none.
    pub fn meta_for_uid(&self, uid: &str) -> Option<BTreeMap<String, String>> {
        self.ctl.borrow().get(uid).and_then(|c| c.meta.clone())
    }

    /// Take (and clear) a pending `restartApp` request: 0 = none, 1 = gui, 2 = full.
    /// Set by the control route off the UI thread; the App tick executes it.
    pub fn take_restart_request(&self) -> u8 {
        self.shared.borrow().as_ref().map_or(0, |s| {
            s.restart_app.swap(0, std::sync::atomic::Ordering::SeqCst)
        })
    }

    /// Mirror `pane_ids` to disk. A pane's `HYPERPANES_PANE_ID` is baked into its environment
    /// at spawn, but this uid→pane-id map lived only in the GUI's memory — so after a GUI
    /// relaunch re-attached a control-spawned pane, nothing could resolve its external id
    /// (Claude session markers are keyed by it). Best-effort; reloaded by [`Self::start`].
    fn persist_pane_ids(&self) {
        let sorted: std::collections::BTreeMap<_, _> = self
            .pane_ids
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Ok(json) = serde_json::to_vec_pretty(&sorted) {
            let _ = paths::write_atomic(&paths::control_pane_ids_json(), &json);
        }
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
        // Recover the uid→pane-id map from the previous GUI generation, so re-attached
        // control-spawned panes keep resolving their external id (env `HYPERPANES_PANE_ID`,
        // the Claude session-marker key). Live in-memory entries win; stale uids are pruned
        // by the next reconcile pass.
        if self.pane_ids.borrow().is_empty() {
            if let Ok(text) = std::fs::read_to_string(paths::control_pane_ids_json()) {
                if let Ok(saved) = serde_json::from_str::<HashMap<String, String>>(&text) {
                    self.pane_ids.borrow_mut().extend(saved);
                }
            }
        }
        let shared = Shared::new(
            Arc::clone(mgr),
            self.allow_input.get(),
            // The shipped app's real version (this crate's Cargo version), so `control.json` +
            // `/health` report it accurately instead of a stale hardcoded string.
            env!("CARGO_PKG_VERSION"),
            self.control_file.clone(),
            paths::speech_json(),
        );
        // Bind the server's own background spawns (the `notify_state` coalescer) to this runtime.
        shared.set_runtime(self.runtime.clone());
        // Back the work queue with the durable on-disk DB and recover in-flight tasks left by
        // workers that died with the previous session.
        let work_attach = shared.attach_durable_work_queue();
        shared.attach_durable_inbox(&work_attach);
        // Listen config (#131): loopback always + the configured remote endpoints. Re-read the
        // settings file so an edit takes effect on the next Enabled toggle without an app restart.
        {
            let fresh = control_settings::load();
            shared.set_listen(fresh.remote_specs(), fresh.port.unwrap_or(0));
        }
        let task = self.runtime.spawn(server::run_server(Arc::clone(&shared)));
        let ticker = self
            .runtime
            .spawn(server::run_activity_ticker(Arc::clone(&shared)));
        // The reaper sweep and the coalesced queue-changed flusher share one task handle.
        let reaper = self.runtime.spawn({
            let shared = Arc::clone(&shared);
            async move {
                tokio::join!(
                    server::run_reaper_ticker(Arc::clone(&shared)),
                    server::run_queue_flusher(shared)
                );
            }
        });
        *self.shared.borrow_mut() = Some(shared);
        *self.task.borrow_mut() = Some(task);
        *self.ticker.borrow_mut() = Some(ticker);
        *self.reaper.borrow_mut() = Some(reaper);
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
        if let Some(t) = self.reaper.borrow_mut().take() {
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
        self.prev_tabs.borrow_mut().clear();
        self.prev_active.borrow_mut().clear();
        self.prev_windows.borrow_mut().clear();
        self.heal_pending.borrow_mut().clear();
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
        // Preserve fields this host doesn't own (remoteEndpoints/bindAddress/port live only in
        // the file):
        // load-modify-save so toggling the booleans can't erase a remote-access config.
        let mut settings = control_settings::load();
        settings.enabled = self.enabled.get();
        settings.allow_input = self.allow_input.get();
        let _ = control_settings::save(&settings);
    }

    /// `(enabled, allow_input, port-if-running)` for the Preferences status line.
    pub fn status(&self) -> (bool, bool, Option<u16>) {
        let port = self
            .shared
            .borrow()
            .as_ref()
            .map(|s| s.port())
            .filter(|p| *p != 0);
        (self.enabled.get(), self.allow_input.get(), port)
    }

    /// Per-remote-endpoint bind status of the running server (#131); empty when stopped or
    /// when no remote endpoint is configured.
    pub fn remote_status(&self) -> Vec<hyperpanes_core::control::endpoints::EndpointStatus> {
        self.shared
            .borrow()
            .as_ref()
            .map(|s| s.endpoint_status())
            .unwrap_or_default()
    }

    // ---- speech (SpeechService, owned by the running `Shared`) ----

    /// Kill any in-flight/queued speech immediately. No-op (returns `false`) when the control
    /// server isn't running — the `SpeechService` lives on `Shared`.
    pub fn speech_stop_all(&self) -> bool {
        match self.shared.borrow().as_ref() {
            Some(s) => {
                s.speech.stop_all();
                true
            }
            None => false,
        }
    }

    /// Flip the global speech mute flag, returning the new value. `None` when the control
    /// server isn't running.
    pub fn speech_toggle_muted(&self) -> Option<bool> {
        let shared = self.shared.borrow();
        let s = shared.as_ref()?;
        let on = !s.speech.status().muted;
        s.speech.set_muted(on);
        Some(on)
    }

    /// Flip "only speak the focused pane", returning the new value. `None` when the control
    /// server isn't running.
    pub fn speech_toggle_focused_only(&self) -> Option<bool> {
        let shared = self.shared.borrow();
        let s = shared.as_ref()?;
        let on = !s.speech.status().focused_only;
        s.speech.set_focused_only(on);
        Some(on)
    }

    /// Toast every pane with `talk` on that hasn't been notified yet that speech needs the
    /// control server running (it hosts the `SpeechService`). No-op once already shown per pane.
    fn notice_talk_needs_backend(&self, windows: &[Rc<Window>]) {
        let mut shown = self.talk_notice_shown.borrow_mut();
        for w in windows {
            let mut st = w.state.borrow_mut();
            for t in st.tabs.iter_mut() {
                for p in t.panes.iter_mut() {
                    if p.talk && shown.insert(p.uid.clone()) {
                        p.pane
                            .set_toast("Talk needs the control server (Preferences)");
                    }
                }
            }
        }
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

    /// Drain `connectRemote` requests queued by the control API (UI thread, once per tick).
    pub fn take_remote_connects(&self) -> Vec<hyperpanes_core::remote::ConnectRequest> {
        match self.shared.borrow().as_ref() {
            Some(s) => s.take_remote_connects(),
            None => Vec::new(),
        }
    }

    /// Publish the attached-remote-pane snapshot served by the `remoteScreen` command.
    pub fn publish_remote_screens(&self, v: serde_json::Value) {
        if let Some(s) = self.shared.borrow().as_ref() {
            s.set_remote_screens(v);
        }
    }

    /// Queue control-plane meta for pane `uid`, applied once the pane is in the read model.
    ///
    /// The GUI's restore path uses this to give a restored goal orchestrator back its
    /// `role=goals-orch` / `project`. That meta lives only in the read model, which a GUI restart
    /// wipes, and the persona that normally stamps it does so "on start" — which a `--resume`d
    /// orchestrator never repeats.
    pub fn queue_meta(&self, uid: String, meta: BTreeMap<String, String>) {
        self.pending_meta.borrow_mut().push(PendingMeta {
            uid,
            meta,
            waited: 0,
        });
    }

    /// Queue a restore alias (`old` pane id is now `new`), registered with the control server
    /// as soon as it is up (immediately on the next sync if it already is).
    pub fn queue_alias(&self, old: String, new: String) {
        self.pending_aliases.borrow_mut().push((old, new));
    }

    /// Register queued aliases and re-point restored panes' `parent` / `advisor` meta at the
    /// live ids. Runs under the model lock's caller; the alias table is cloned first so the
    /// inbox and model locks are never nested.
    fn apply_aliases(&self, shared: &Shared) {
        let queued: Vec<(String, String)> = std::mem::take(&mut *self.pending_aliases.borrow_mut());
        for (old, new) in queued {
            shared.register_pane_alias(&old, &new);
        }
        let aliases = shared.inbox.lock().unwrap().aliases().clone();
        if aliases.is_empty() {
            return;
        }
        let mut model = shared.model.lock().unwrap();
        let ids: Vec<String> = model.panes().into_iter().map(|p| p.pane_id).collect();
        for id in ids {
            let patch = model
                .pane(&id)
                .and_then(|p| p.meta.as_ref())
                .and_then(|m| alias_meta_patch(m, &aliases));
            if let Some(patch) = patch {
                model.set_meta(&id, &patch);
            }
        }
    }

    /// Apply queued meta to every pane that has reached the read model; keep the rest.
    ///
    /// A pane id is the control alias when one exists, else the session uid itself. Entries are
    /// dropped after `MAX_WAIT_TICKS` so a pane that never materializes (closed mid-restore) cannot
    /// pin its entry forever. The applied meta then survives on its own: the next reconcile copies
    /// model meta into `ctl`, and publish re-stamps `ctl` onto the model every tick.
    fn apply_pending_meta(&self, model: &mut ReadModel) {
        const MAX_WAIT_TICKS: u32 = 600;
        let mut pending = self.pending_meta.borrow_mut();
        if pending.is_empty() {
            return;
        }
        pending.retain_mut(|p| {
            let pane_id = self
                .pane_id_for_uid(&p.uid)
                .unwrap_or_else(|| p.uid.clone());
            let patch: BTreeMap<String, Option<String>> = p
                .meta
                .iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect();
            if model.set_meta(&pane_id, &patch).is_some() {
                return false; // applied
            }
            p.waited += 1;
            p.waited < MAX_WAIT_TICKS
        });
    }

    /// The read-model bridge: reconcile any control-originated structural change back into the
    /// live GUI (on the UI thread), then republish the GUI tree into the read-model. No-op when
    /// the server is stopped.
    pub fn sync(&self, windows: &[Rc<Window>], mgr: &Arc<SessionManager>) {
        let shared = match self.shared.borrow().as_ref() {
            Some(s) => Arc::clone(s),
            None => {
                // The server isn't running, so nothing polls `talk` — a pane the user just
                // switched on would otherwise silently never speak. One-shot, non-blocking
                // toast per pane (see `talk_notice_shown`).
                self.notice_talk_needs_backend(windows);
                return;
            }
        };
        // The server IS running: clear the notice baseline so a later stop→talk-on cycle
        // notices again.
        self.talk_notice_shown.borrow_mut().clear();

        // ONE model lock across snapshot → reconcile → republish. `/command` dispatch holds
        // this lock for its whole execution (routes.rs), so a mutation lands either before
        // the snapshot (reconciled this tick) or after the republish (seen next tick) —
        // never in between, where the wholesale rebuild would destroy it (the g4 pane-vanish
        // race). Lock order model→sessions matches dispatch, so the mgr calls inside
        // reconcile can't deadlock.
        let (reconciled, republished) = {
            let mut model = shared.model.lock().unwrap();

            // 0. Self-heal: re-insert any live control-spawned session that lost its pane.
            let healed = self.heal_lost_panes(&mut model, windows, mgr);
            self.remember_panes(&model);

            // 1. Snapshot the read-model (it may have been mutated off-thread by a `/command`).
            let (cur, cur_active, focus_uid) = self.snapshot_model(&model, windows);

            let cur_tabs = snapshot_tabs(&model);

            // 2. Reconcile the model→GUI deltas (what the control plane changed) on the UI thread.
            let reconciled = self.reconcile(windows, mgr, &cur, &cur_tabs, &cur_active, &focus_uid);

            // 2b. Execute queued `groupPanes` / `ungroupPanes` requests against the GUI (only
            //     the GUI can move panes across tabs), before the republish so this tick's
            //     read-model already shows the new/changed tab.
            let grouped = self.apply_pane_group_requests(&shared, windows, mgr);

            // 3. Republish the (now-updated) live GUI tree into the read-model.
            let republished = self.publish(&mut model, windows);

            // 3b. Stamp any meta the GUI queued for panes that now exist in the model.
            self.apply_pending_meta(&mut model);
            (reconciled || healed || grouped, republished)
        };

        // 3c. Restore aliases: register them, then rewrite stale parent/advisor ids. After the
        //     model guard drops (the inbox lock is taken inside).
        self.apply_aliases(&shared);

        // 4. Nudge WS clients if the published structure changed (GUI- or control-driven).
        //    (After the guard drops — notify serializes `/state` under the same lock.)
        if reconciled || republished {
            notify_state(&shared);
        }

        // 5. The control plane changed the project registry off-thread (an MCP add_project /
        //    rename / recolor / remove, or a project-opening newPane bumping recency). Reload
        //    every window's cached sidebar rail so it updates live, not just on the next
        //    flyout-open. Cheap: the flag is only set on an actual registry write.
        if shared.take_projects_dirty() {
            for w in windows {
                w.state.borrow_mut().refresh_projects();
            }
        }

        // 5b. Per-project task-count badge (#6/#9): refresh from the coalesced queue-changed
        //     flag (push), with a slow fallback sweep so a missed signal self-heals. Reads the
        //     in-process work queue (cheap SQLite count query; no HTTP hop).
        {
            let dirty = shared.take_queue_dirty();
            let since = self.queue_polled.get().map(|t| t.elapsed());
            // An open task list (#8) with a queued action / page request refreshes right away.
            let panel_wants = windows.iter().any(|w| {
                w.state
                    .borrow()
                    .task_panel
                    .as_ref()
                    .is_some_and(|p| p.needs_refresh())
            });
            if crate::project_queue::queue_refresh_due(dirty, since) || panel_wants {
                self.queue_polled.set(Some(std::time::Instant::now()));
                let queues = shared.work.lock().map(|w| w.queues()).unwrap_or_default();
                for w in windows {
                    let mut st = w.state.borrow_mut();
                    // Task list (#8): run any pending purge/cancel and re-fetch the rows,
                    // in-process (short SQLite ops; no HTTP hop).
                    if st.task_panel.is_some() {
                        let projects = st.projects.clone();
                        let mut panel = st.task_panel.take().unwrap();
                        if let Ok(mut work) = shared.work.lock() {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            // Re-read the overview so a purge/cancel is reflected immediately.
                            let live = work.queues();
                            panel.refresh(&mut work, &live, &projects, now);
                        }
                        st.task_panel = Some(panel);
                        st.dirty = true;
                    }
                    let counts = crate::project_queue::project_open_counts(&queues, &st.projects);
                    let other = crate::project_queue::other_open_count(&queues, &st.projects);
                    st.set_project_queue_counts(counts, other);
                    let lanes = crate::project_queue::project_lane_counts(&queues, &st.projects);
                    let other_lanes =
                        crate::project_queue::other_lane_counts(&queues, &st.projects);
                    st.set_lane_counts(lanes, other_lanes);
                }
            }
        }

        // 6. Publish the focused pane for the speech service's `focusedOnly` filter. The app
        //    has no per-window OS-focus tracking yet, so this attributes focus to the primary
        //    window's active tab (correct for the common single-window case; multi-window just
        //    doesn't disambiguate which OS window is frontmost).
        let focused_uid = windows.first().and_then(|w| {
            let st = w.state.borrow();
            let t = st.active_tab();
            t.panes
                .get(t.focused)
                .filter(|p| p.remote.is_none())
                .map(|p| p.uid.clone())
        });
        let focused_pane_id = focused_uid.map(|uid| self.pane_id_for_uid(&uid).unwrap_or(uid));
        shared
            .model
            .lock()
            .unwrap()
            .set_focused_pane(focused_pane_id);
    }

    /// Drain the queued pane-group requests and run each on the owning window's state.
    /// Returns whether any request changed the GUI.
    fn apply_pane_group_requests(
        &self,
        shared: &Shared,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
    ) -> bool {
        let reqs = shared.take_pane_group_requests();
        let mut changed = false;
        // `movePane` requests still waiting for their pane go first, in arrival order.
        let mut moves: Vec<(server::PaneGroupRequest, u32)> =
            self.pending_moves.borrow_mut().drain(..).collect();
        // A control pane id resolves to its GUI session uid through the alias map; a
        // GUI-native pane's id IS its uid.
        let resolve = |id: &str| self.uid_for_pane_id(id).unwrap_or_else(|| id.to_string());
        for req in &reqs {
            if matches!(req, server::PaneGroupRequest::Move { .. }) {
                moves.push((req.clone(), 0));
                continue;
            }
            for w in windows {
                let mut st = w.state.borrow_mut();
                if apply_pane_group_request(&mut st, mgr, w.id as i64, &resolve, req) {
                    changed = true;
                    break;
                }
            }
        }
        changed |= self.apply_move_requests(moves, windows, mgr, &resolve);
        changed
    }

    /// Run queued `movePane` requests; one whose pane the GUI does not host yet is retried on
    /// later ticks (bounded), any other miss is dropped. Returns whether a pane moved.
    fn apply_move_requests(
        &self,
        moves: Vec<(server::PaneGroupRequest, u32)>,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
        resolve: &dyn Fn(&str) -> String,
    ) -> bool {
        const MAX_MOVE_WAIT_TICKS: u32 = 120;
        let states: Vec<(i64, &RefCell<crate::state::State>)> =
            windows.iter().map(|w| (w.id as i64, &w.state)).collect();
        let mut changed = false;
        for (req, waited) in moves {
            match apply_move_request(&states, mgr, resolve, &req) {
                MoveOutcome::Moved => changed = true,
                MoveOutcome::NotFound if waited < MAX_MOVE_WAIT_TICKS => {
                    self.pending_moves.borrow_mut().push((req, waited + 1));
                }
                MoveOutcome::NotFound | MoveOutcome::Refused => {}
            }
        }
        changed
    }

    /// Self-heal (recovery, independent of the publish-race fix): a control-spawned pane whose
    /// SESSION is alive but which is missing from BOTH the read-model and the GUI is invisible
    /// to every documented mechanism (`list_panes` / `read_pane` / `restart_pane` all 404) even
    /// though its process is working. Whatever dropped it, re-insert a pane for it into the
    /// model under its persisted pane-id; the normal adopt path re-hosts it in the GUI on the
    /// next reconcile. Keyed on the uid→pane-id alias map (control-minted id ≠ uid), so
    /// GUI-native panes — including the closed-tab undo buffer and parked reminder panes,
    /// whose sessions stay alive on purpose — are never resurrected. Debounced by
    /// [`HEAL_DEBOUNCE`] so a transient mid-flight state (e.g. daemon re-attach at startup)
    /// is never misread as a lost pane. The original label/spec/meta died with the model
    /// entry, so the restored pane carries `label:"recovered"` + `meta.hp.recovered:"1"`.
    fn heal_lost_panes(
        &self,
        model: &mut ReadModel,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
    ) -> bool {
        let gui = gui_uids_with_parked(windows);
        let lost = {
            let ids = self.pane_ids.borrow();
            lost_control_panes(&ids, model, &gui, &|uid| mgr.has(uid))
        };
        let mut pending = self.heal_pending.borrow_mut();
        // Anything no longer lost (adopted, exited, healed) leaves the debounce map.
        pending.retain(|uid, _| lost.iter().any(|(u, _)| u == uid));
        let mut healed = false;
        for (uid, pane_id) in lost {
            let since = *pending
                .entry(uid.clone())
                .or_insert_with(std::time::Instant::now);
            if since.elapsed() < HEAL_DEBOUNCE {
                continue;
            }
            pending.remove(&uid);
            let Some(wid) = model.first_window_id() else {
                continue;
            };
            let (label, meta) = healed_identity(self.last_known.borrow().get(&uid));
            let inserted = model.insert_pane(
                wid,
                PaneInfo {
                    id: pane_id.clone(),
                    session_uid: uid.clone(),
                    label,
                    subtitle: None,
                    // A healed pane's prior talk state died with its read-model entry, and
                    // talk is off by default — a pane that started speaking on its own after
                    // a self-heal would be worse than one that stayed quiet.
                    talk: false,
                    color: "#888888".to_string(),
                    command: None,
                    args: None,
                    cwd: None,
                    shell: None,
                    status: PaneStatus::Running,
                    exit_code: None,
                    meta: Some(meta),
                    env: None,
                },
            );
            if inserted {
                eprintln!(
                    "[hyperpanes] self-heal: restored control pane {pane_id} (session {uid}) to the read-model"
                );
                healed = true;
            }
        }
        healed
    }

    /// Record each control-spawned pane's label + meta as the model currently has them (see
    /// `last_known`). Entries for sessions that no longer have a control alias are dropped.
    fn remember_panes(&self, model: &ReadModel) {
        let ids = self.pane_ids.borrow();
        let mut known = self.last_known.borrow_mut();
        known.retain(|uid, _| ids.contains_key(uid));
        for (uid, pane_id) in ids.iter() {
            if let Some(p) = model.pane(pane_id) {
                let mut meta = p.meta.clone();
                if let Some(m) = meta.as_mut() {
                    m.remove("hp.recovered");
                }
                known.insert(uid.clone(), (p.label.clone(), meta));
            }
        }
    }

    /// Read every pane the read-model currently holds (keyed by session uid), each GUI window's
    /// active tab id, and a representative session uid living in each window's active tab. The
    /// representative uid lets the focus reconcile resolve the focused tab by a pane that's
    /// actually in it (stable across GUI tab reorder/close) rather than parsing the positional id.
    fn snapshot_model(
        &self,
        model: &ReadModel,
        windows: &[Rc<Window>],
    ) -> (
        HashMap<String, ModelPane>,
        HashMap<i64, Option<String>>,
        HashMap<i64, Option<String>>,
    ) {
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
                        talk: p.talk,
                        command: p.command.clone(),
                        args: p.args.clone(),
                        shell: p.shell.clone(),
                        cwd: p.cwd.clone(),
                        meta: p.meta.clone(),
                        env: p.env.clone(),
                    },
                );
            }
        }
        let mut active = HashMap::new();
        let mut focus_uid = HashMap::new();
        for w in windows {
            let wid = w.id as i64;
            let at = model.active_tab_id(wid);
            let rep = at.as_ref().and_then(|tid| {
                cur.iter()
                    .find(|(_, m)| &m.tab_id == tid)
                    .map(|(u, _)| u.clone())
            });
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
        cur_tabs: &TabSnaps,
        cur_active: &HashMap<i64, Option<String>>,
        focus_uid: &HashMap<i64, Option<String>>,
    ) -> bool {
        let prev = self.prev.borrow();
        let mut ctl = self.ctl.borrow_mut();
        let state_uids = gui_uids(windows);
        let mut structural = false;
        let ids_before = self.pane_ids.borrow().len();
        // Record every control-minted pane-id alias the model knows, not just the panes this
        // host has adopted: the persisted uid→pane-id map is what the self-heal keys on, so a
        // pane must stay re-identifiable even if it is lost before adoption.
        {
            let mut ids = self.pane_ids.borrow_mut();
            for (uid, c) in cur {
                if c.pane_id != *uid && !ids.contains_key(uid) {
                    ids.insert(uid.clone(), c.pane_id.clone());
                }
            }
        }
        // Model tab id → the GUI tab a control-spawned tab was materialized into THIS tick, so the
        // 2nd…nth pane of an `attach as:tab` group joins the same new tab instead of each making one.
        let mut created_tabs: HashMap<String, (i64, usize)> = HashMap::new();

        // Refresh control-owned fields (command/args/shell/meta) for every model pane.
        for (uid, c) in cur {
            ctl.insert(uid.clone(), CtlFields::from_model(c));
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
                        self.adopt_control_pane(
                            windows,
                            mgr,
                            uid,
                            c,
                            cur,
                            cur_tabs,
                            &mut created_tabs,
                        );
                        self.pane_ids
                            .borrow_mut()
                            .insert(uid.clone(), c.pane_id.clone());
                    }
                }
                structural = true;
            } else if let Some(p) = prev.get(uid) {
                // Present on both sides: apply a control rename / recolor / subtitle / talk change.
                if c.label != p.label
                    || c.color != p.color
                    || c.subtitle != p.subtitle
                    || c.talk != p.talk
                {
                    apply_pane_chrome(windows, uid, c);
                }
            }
        }

        // Control `closePane` removed it from the model: drop it from the GUI too (the PTY was
        // already killed by `dispatch`, so detach without re-killing). Only a DEAD session's
        // pane goes: a pane whose session is still alive leaving the model without a kill would
        // orphan a running program with no pane, so it stays and the next publish puts
        // it back in the model.
        let (dropped, kept) = model_dropped_panes(prev.keys(), cur, &state_uids, |u| mgr.has(u));
        for uid in &dropped {
            remove_from_gui(windows, uid);
            structural = true;
        }
        for uid in &kept {
            eprintln!("[control] kept pane {uid}: the model dropped it but its session is alive");
            hyperpanes_core::persistence::session_log::record(
                "keep-live",
                uid,
                "model dropped a live pane without closePane",
                None,
            );
        }

        // Control tab-level changes (`setLayout`, tab rename) the model took since the last
        // publish. Applied per tab by index without touching the window's active tab.
        {
            let cur_snaps: HashMap<String, TabSnap> = cur_tabs
                .iter()
                .map(|(id, (_, s))| (id.clone(), s.clone()))
                .collect();
            for ch in diff_tabs(&self.prev_tabs.borrow(), &cur_snaps) {
                let (wid, _) = &cur_tabs[&ch.tab_id];
                let Some(w) = windows.iter().find(|w| w.id as i64 == *wid) else {
                    continue;
                };
                let idx = resolve_tab_index(&w.state.borrow().tabs, &ch.tab_id);
                if let Some(idx) = idx {
                    if apply_tab_change(&mut w.state.borrow_mut(), idx, &ch) {
                        structural = true;
                    }
                }
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
            let Some(w) = windows.iter().find(|w| w.id as i64 == *wid) else {
                continue;
            };
            if act.is_none() {
                continue;
            }
            let by_uid = focus_uid
                .get(wid)
                .and_then(|u| u.as_ref())
                .and_then(|u| w.state.borrow_mut().find_pane(u).map(|(ti, _)| ti));
            let idx = by_uid.or_else(|| {
                act.as_deref()
                    .and_then(|a| resolve_tab_index(&w.state.borrow().tabs, a))
            });
            if let Some(idx) = idx {
                w.state.borrow_mut().switch_tab(idx);
            }
        }
        drop(prev_active);

        // Prune side-store entries for panes that no longer exist in the GUI.
        let live = gui_uids(windows);
        ctl.retain(|uid, _| live.contains(uid));
        // The pane-id aliases also honor DAEMON liveness: right after a GUI relaunch this
        // pass can run before the surviving sessions are re-adopted (zero GUI panes), and
        // a GUI-presence-only prune would wipe the just-reloaded persisted map — orphaning
        // every control-spawned pane's external id (its env HYPERPANES_PANE_ID is baked at
        // spawn and keys the Claude session markers).
        self.pane_ids
            .borrow_mut()
            .retain(|uid, _| live.contains(uid) || mgr.has(uid));
        // One choke point covers every map mutation this pass (insert, respawn re-pin, prune).
        if structural || self.pane_ids.borrow().len() != ids_before {
            self.persist_pane_ids();
        }
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
                // The respawned pty starts at the backend's default size (the control path
                // knows no geometry), while this pane's grid keeps the size it was laid out
                // at. `place()` only resizes the pty when the grid size changes, which it
                // doesn't here, so push the pane's current size to the new session now —
                // else the program inside renders at 80x24 in a full-size pane.
                if p.remote.is_none() {
                    let (cols, rows) = respawn_pty_size(p.applied, p.pane.grid_size());
                    mgr.resize(new_uid, cols, rows);
                }
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
    fn publish(&self, model: &mut ReadModel, windows: &[Rc<Window>]) -> bool {
        let pane_ids = self.pane_ids.borrow();
        let ctl = self.ctl.borrow();

        let mut new_windows = Vec::new();
        let mut new_prev = HashMap::new();
        let mut new_prev_tabs = HashMap::new();
        let mut new_active = HashMap::new();
        let mut tree = Vec::new();
        for w in windows {
            let wid = w.id as i64;
            new_windows.push(wid);
            let st = w.state.borrow();
            let active_tab_id = st
                .tabs
                .get(st.active)
                .map(|t| tab_model_id(wid, st.active, t));
            new_active.insert(wid, active_tab_id.clone());
            let mut tabs = Vec::new();
            for (ti, tab) in st.tabs.iter().enumerate() {
                // Remote tabs (another host's panes) never enter the local read model: they
                // must not appear in this instance's `/state`, nor be healed/adopted. The
                // positional tab ids of the local tabs are unchanged (`ti` keeps counting).
                if tab.remote.is_some() {
                    continue;
                }
                let tab_id = tab_model_id(wid, ti, tab);
                let mut panes = Vec::new();
                for p in &tab.panes {
                    let uid = p.uid.clone();
                    let pane_id = pane_ids.get(&uid).cloned().unwrap_or_else(|| uid.clone());
                    let label = p.title.to_string();
                    let color = color_hex(p.accent);
                    let subtitle = p.subtitle.as_ref().map(|s| s.to_string());
                    let talk = p.talk;
                    let c = effective_launch_spec(
                        ctl.get(&uid).cloned().unwrap_or_default(),
                        p.spawn_command.as_ref(),
                        p.spawn_args.as_ref(),
                        p.spawn_shell.as_ref(),
                        p.env.as_ref(),
                    );
                    new_prev.insert(
                        uid.clone(),
                        PaneSnap {
                            label: label.clone(),
                            color: color.clone(),
                            subtitle: subtitle.clone(),
                            talk,
                        },
                    );
                    panes.push(PaneInfo {
                        id: pane_id,
                        session_uid: uid,
                        label,
                        subtitle,
                        color,
                        talk,
                        command: c.command,
                        args: c.args,
                        cwd: p.cwd.clone(),
                        shell: c.shell,
                        status: PaneStatus::Running,
                        exit_code: None,
                        meta: c.meta,
                        env: c.env,
                    });
                }
                let title = tab.title.to_string();
                let layout = crate::theme::layout_name(tab.layout).to_string();
                let color = tab.color.map(color_hex);
                new_prev_tabs.insert(
                    tab_id.clone(),
                    TabSnap {
                        title: title.clone(),
                        layout: layout.clone(),
                        color: color.clone(),
                    },
                );
                tabs.push(TabInfo {
                    id: tab_id,
                    title,
                    color,
                    layout,
                    panes,
                    pane_group: tab.pane_group.as_ref().map(|g| {
                        hyperpanes_core::control::readmodel::PaneGroupInfo {
                            project: g.project.clone(),
                            color: g.color.map(color_hex),
                        }
                    }),
                });
            }
            tree.push(WindowInfo {
                window_id: wid,
                active_tab_id,
                tabs,
            });
        }

        // The uids the LAST publish put in the model = the panes the GUI actually hosts.
        let last_published: HashSet<String> = self.prev.borrow().keys().cloned().collect();
        let drop_windows: Vec<i64> = self.prev_windows.borrow_mut().drain(..).collect();
        let carried = model.publish_replace(&drop_windows, tree, &last_published);
        for id in &carried {
            eprintln!(
                "[hyperpanes] control pane {id} carried over the GUI republish (not yet adopted)"
            );
        }

        // Did the structure (which panes exist, or which tab is active) change since last publish?
        let structural = {
            let old = self.prev.borrow();
            let old_active = self.prev_active.borrow();
            let panes_changed =
                old.len() != new_prev.len() || new_prev.keys().any(|uid| !old.contains_key(uid));
            panes_changed || *old_active != new_active
        };

        *self.prev_windows.borrow_mut() = new_windows;
        *self.prev.borrow_mut() = new_prev;
        *self.prev_tabs.borrow_mut() = new_prev_tabs;
        *self.prev_active.borrow_mut() = new_active;
        structural
    }

    /// Adopt a control-spawned session into the GUI: build a [`DetachedPane`] for the live uid and
    /// re-host it into the tab the MODEL placed it in (the PTY already exists). The target tab is
    /// resolved (in order): a sibling pane already in the GUI for the same model tab → a tab made
    /// this tick for that model tab → the positional model tab id → otherwise a brand-new GUI tab
    /// (an `attach as:tab` group). Adopting into a background tab does NOT steal the user's focus.
    #[allow(clippy::too_many_arguments)]
    fn adopt_control_pane(
        &self,
        windows: &[Rc<Window>],
        mgr: &Arc<SessionManager>,
        uid: &str,
        c: &ModelPane,
        cur: &HashMap<String, ModelPane>,
        cur_tabs: &TabSnaps,
        created_tabs: &mut HashMap<String, (i64, usize)>,
    ) {
        let target = windows
            .iter()
            .find(|w| w.id as i64 == c.window_id)
            .or_else(|| windows.first());
        let Some(w) = target else { return };
        let font_px = w.state.borrow().settings.font_px;
        let det = detached_pane_from_model(uid, c, font_px);

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
                let snap = cur_tabs.get(&c.tab_id).map(|(_, s)| s);
                let title = snap.map(|s| s.title.as_str());
                let layout = snap.and_then(|s| crate::state::parse_layout(&s.layout));
                st.adopt_pane_as_tab(mgr, det, title, layout);
                let new_ti = st.tabs.len().saturating_sub(1);
                // A control-created tab keeps the id `attach` returned for its whole life.
                if parse_tab_index(&c.tab_id).is_none() {
                    if let Some(t) = st.tabs.get_mut(new_ti) {
                        t.ctl_id = Some(c.tab_id.clone());
                    }
                }
                // ...and the manual colour `attach` gave it (`groups[].color`).
                let color = snap
                    .and_then(|s| s.color.as_deref())
                    .and_then(crate::state::parse_hex_strict);
                st.set_tab_color(new_ti, color);
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
        // 3. A control tab id or positional "{window_id}:{index}" id that maps to a live GUI tab.
        if let Some(idx) = resolve_tab_index(&w.state.borrow().tabs, &c.tab_id) {
            return Some(idx);
        }
        // 4. Otherwise the model put it in a brand-new tab → signal "make one".
        None
    }
}

/// Every model tab's title/layout keyed by tab id, with its window id.
fn snapshot_tabs(model: &ReadModel) -> TabSnaps {
    model
        .tabs()
        .map(|(wid, t)| {
            (
                t.id.clone(),
                (
                    wid,
                    TabSnap {
                        title: t.title.clone(),
                        layout: t.layout.clone(),
                        color: t.color.clone(),
                    },
                ),
            )
        })
        .collect()
}

/// The [`DetachedPane`] a control-spawned (or orphan-adopted) session is hosted as. The model
/// pane carries the original command/args/shell, forwarded here so the GUI pane's snapshot
/// (`to_session_file`) records them and a restore re-runs the original program instead of a
/// bare shell, and its spawn `env` so the snapshot can persist the (filtered) explicit env.
fn detached_pane_from_model(uid: &str, c: &ModelPane, font_px: f32) -> DetachedPane {
    DetachedPane {
        replay_answered: false,
        live: Default::default(),
        uid: uid.to_string(),
        title: c.label.clone().into(),
        subtitle: c.subtitle.clone().map(Into::into),
        pinned_accent: Some(parse_hex(&c.color)),
        show_frame: Some(true),
        show_dot: Some(true),
        font_px,
        spawn_command: c.command.clone(),
        spawn_args: c.args.clone(),
        spawn_shell: c.shell.clone(),
        spawn_env: c.env.clone(),
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
    talk: bool,
    command: Option<String>,
    args: Option<Vec<String>>,
    shell: Option<String>,
    cwd: Option<String>,
    meta: Option<BTreeMap<String, String>>,
    env: Option<hyperpanes_core::session::spawn::EnvMap>,
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
        let effective = pane_ids
            .get(&uid)
            .map(String::as_str)
            .unwrap_or(uid.as_str());
        if effective == pane_id {
            return Some(uid);
        }
    }
    None
}

/// The control-spawned panes that are LOST: session alive, but neither the read-model nor the
/// GUI (including its kept-alive off-layout buffers) knows the uid. GUI-native panes are
/// excluded structurally — their alias entry is `uid == pane_id`. Pure, so it is unit-testable
/// without a Slint window. Returns `(uid, pane_id)` pairs.
/// The label + meta a self-healed pane comes back with: the last-known pair when there is one,
/// else the generic placeholder; always marked `hp.recovered` (a runtime key, never persisted).
fn healed_identity(known: Option<&KnownIdentity>) -> (String, BTreeMap<String, String>) {
    let (label, mut meta) = match known {
        Some((l, m)) => (l.clone(), m.clone().unwrap_or_default()),
        None => ("recovered".to_string(), BTreeMap::new()),
    };
    meta.insert("hp.recovered".to_string(), "1".to_string());
    (label, meta)
}

fn lost_control_panes(
    pane_ids: &HashMap<String, String>,
    model: &ReadModel,
    gui_uids: &HashSet<String>,
    alive: &dyn Fn(&str) -> bool,
) -> Vec<(String, String)> {
    pane_ids
        .iter()
        .filter(|(uid, pane_id)| {
            uid != pane_id
                && !gui_uids.contains(*uid)
                && model.uid_to_pane(uid).is_none()
                && model.coords_of(pane_id).is_none()
                && alive(uid)
        })
        .map(|(u, p)| (u.clone(), p.clone()))
        .collect()
}

/// [`gui_uids`] plus the sessions the GUI keeps alive OFF-layout on purpose: the closed-tab
/// undo buffer and parked reminder panes. The self-heal must not resurrect either.
fn gui_uids_with_parked(windows: &[Rc<Window>]) -> HashSet<String> {
    let mut set = gui_uids(windows);
    for w in windows {
        let st = w.state.borrow();
        for t in &st.closed_tabs {
            for p in &t.panes {
                set.insert(p.uid.clone());
            }
        }
        for r in &st.reminders {
            set.insert(r.pane.uid.clone());
        }
    }
    set
}

/// Every session uid the GUI currently hosts across all windows + tabs.
fn gui_uids(windows: &[Rc<Window>]) -> HashSet<String> {
    let mut set = HashSet::new();
    for w in windows {
        for t in w.state.borrow().tabs.iter().filter(|t| t.remote.is_none()) {
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
            p.talk = c.talk;
            st.dirty = true;
            return;
        }
    }
}

/// Remove the GUI pane with `uid` without killing its (already-dead) session.
/// The panes the model dropped since the last publish that the GUI still hosts, split into
/// `(remove, keep)`: a pane goes only when its session is dead (`closePane` kills before it
/// removes); one whose session is alive is kept (removing it orphans the program).
fn model_dropped_panes<'a>(
    prev: impl Iterator<Item = &'a String>,
    cur: &HashMap<String, ModelPane>,
    state_uids: &HashSet<String>,
    alive: impl Fn(&str) -> bool,
) -> (Vec<String>, Vec<String>) {
    let (mut remove, mut keep) = (Vec::new(), Vec::new());
    for uid in prev {
        if state_uids.contains(uid) && !cur.contains_key(uid) {
            if alive(uid) {
                keep.push(uid.clone());
            } else {
                remove.push(uid.clone());
            }
        }
    }
    (remove, keep)
}

fn remove_from_gui(windows: &[Rc<Window>], uid: &str) {
    for w in windows {
        let has = w.state.borrow_mut().find_pane(uid).is_some();
        if has {
            let _ = w.state.borrow_mut().detach_uid(uid);
            return;
        }
    }
}

/// The id the read model publishes for GUI tab `ti` of window `wid`: the control id the tab was
/// created with (`open_tab`), else the positional `"{wid}:{ti}"`.
fn tab_model_id(wid: i64, ti: usize, tab: &crate::state::Tab) -> String {
    tab.ctl_id.clone().unwrap_or_else(|| format!("{wid}:{ti}"))
}

/// Execute one queued pane-group request on window `window_id`'s state `st`. `resolve` maps a
/// control pane id to its GUI session uid. Returns whether it changed this window (so the caller
/// stops looking). A `Group` for another window, or with <2 resolvable local panes, is a no-op;
/// the new tab's `ctl_id` is the `tab_id` the API already returned, so that id resolves.
pub(crate) fn apply_pane_group_request(
    st: &mut crate::state::State,
    mgr: &SessionManager,
    window_id: i64,
    resolve: &dyn Fn(&str) -> String,
    req: &server::PaneGroupRequest,
) -> bool {
    use server::PaneGroupRequest as R;
    match req {
        R::Group {
            tab_id,
            window_id: wid,
            pane_ids,
            title,
            project,
        } => {
            if *wid != window_id {
                return false;
            }
            let uids: Vec<String> = pane_ids.iter().map(|id| resolve(id)).collect();
            let Some(anchor) = uids.first().cloned() else {
                return false;
            };
            st.group_panes_uids(
                mgr,
                &uids,
                &anchor,
                title.as_deref(),
                project.as_deref(),
                Some(tab_id.clone()),
            )
            .is_some()
        }
        R::Ungroup { tab_id } => {
            let Some(ti) = resolve_tab_index(&st.tabs, tab_id) else {
                return false;
            };
            if st.tabs[ti].pane_group.is_none() {
                return false;
            }
            st.ungroup(ti, mgr);
            true
        }
        // Spans windows, so it runs through `apply_move_request` instead.
        R::Move { .. } => false,
    }
}

/// What [`apply_move_request`] did.
#[derive(Debug, PartialEq)]
pub(crate) enum MoveOutcome {
    /// The pane is now in the target tab (or already was).
    Moved,
    /// No window hosts the pane (yet).
    NotFound,
    /// The move can't happen (unknown/remote target, or the window's only pane).
    Refused,
}

/// Execute one queued `movePane` across the GUI windows `states` (`(window id, state)`): lift the
/// live pane out of whichever tab hosts it and re-host it in the target tab of the target window,
/// creating that tab (in the background, never stealing focus) for `newTab` / a project without
/// one. The session, label, subtitle and control meta (keyed by session uid) are untouched.
pub(crate) fn apply_move_request(
    states: &[(i64, &RefCell<crate::state::State>)],
    mgr: &SessionManager,
    resolve: &dyn Fn(&str) -> String,
    req: &server::PaneGroupRequest,
) -> MoveOutcome {
    use crate::state::PaneGroup;
    use server::MoveTarget as T;
    let server::PaneGroupRequest::Move {
        pane_id,
        window_id,
        to,
    } = req
    else {
        return MoveOutcome::Refused;
    };
    let uid = resolve(pane_id);
    let Some(src) = states
        .iter()
        .find(|(_, s)| s.borrow_mut().find_pane(&uid).is_some())
        .map(|(_, s)| *s)
    else {
        return MoveOutcome::NotFound;
    };
    let Some(dst) = states.iter().find(|(w, _)| w == window_id).map(|(_, s)| *s) else {
        return MoveOutcome::Refused;
    };
    // The existing target tab's stable uid (None ⇒ make a new tab after lifting the pane).
    let existing: Option<u64> = {
        let mut st = dst.borrow_mut();
        let idx = match to {
            T::Tab { tab_id } => match resolve_tab_index(&st.tabs, tab_id) {
                Some(i) => Some(i),
                None => return MoveOutcome::Refused,
            },
            T::NewTab { .. } => None,
            T::Project {
                tab_id, project_id, ..
            } => {
                let i = st.project_tab(project_id);
                // Reusing a project tab the API didn't know about: give it the id it returned.
                if let Some(t) = i.and_then(|i| st.tabs.get_mut(i)) {
                    t.ctl_id.get_or_insert_with(|| tab_id.clone());
                }
                i
            }
        };
        match idx {
            Some(i) if st.tabs[i].remote.is_some() => return MoveOutcome::Refused,
            Some(i) if st.tabs[i].panes.iter().any(|p| p.uid == uid) => return MoveOutcome::Moved,
            Some(i) => Some(st.tabs[i].uid),
            None => None,
        }
    };
    let Some(mp) = src.borrow_mut().lift_pane(&uid) else {
        return MoveOutcome::Refused;
    };
    let mut st = dst.borrow_mut();
    let tab_uid = existing.unwrap_or_else(|| match to {
        T::NewTab {
            tab_id,
            title,
            color,
        } => st.push_background_tab(
            title.as_deref().unwrap_or(""),
            color.as_deref().and_then(crate::state::parse_hex_strict),
            None,
            Some(tab_id.clone()),
        ),
        T::Project {
            tab_id,
            project_id,
            title,
            color,
        } => {
            let color = color.as_deref().and_then(crate::state::parse_hex_strict);
            st.push_background_tab(
                title,
                None,
                Some(PaneGroup {
                    project: Some(project_id.clone()),
                    color,
                }),
                Some(tab_id.clone()),
            )
        }
        // An existing tab always resolved above.
        T::Tab { .. } => st.push_background_tab("", None, None, None),
    });
    // A target tab that vanished between resolving and placing puts the pane back where it
    // was (`place_moved_pane_in` → rescue) instead of dropping a live session.
    st.place_moved_pane_in(mgr, mp, tab_uid);
    MoveOutcome::Moved
}

/// Map a model tab id (control id or positional) to a GUI tab index within one window.
fn resolve_tab_index(tabs: &[crate::state::Tab], id: &str) -> Option<usize> {
    if let Some(i) = tabs.iter().position(|t| t.ctl_id.as_deref() == Some(id)) {
        return Some(i);
    }
    parse_tab_index(id).filter(|&i| i < tabs.len())
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

/// Meta keys holding another pane's id that a restore must re-point at the live pane.
const ID_META_KEYS: [&str; 2] = ["parent", "advisor"];

/// The external pane id a restored pane advertised BEFORE the restore: the persisted
/// uid→external-id map entry for `spec_uid` when present, else the uid itself (a GUI-native
/// pane's id is its uid).
pub(crate) fn old_external_id(spec_uid: &str, old_ids: &HashMap<String, String>) -> String {
    old_ids
        .get(spec_uid)
        .cloned()
        .unwrap_or_else(|| spec_uid.to_string())
}

/// The `parent` / `advisor` meta values that point at an aliased (pre-restore) pane id,
/// as a set-patch to the live id. Either `pane-` spelling of the old id matches. `None` when
/// nothing needs rewriting.
pub(crate) fn alias_meta_patch(
    meta: &BTreeMap<String, String>,
    aliases: &HashMap<String, String>,
) -> Option<BTreeMap<String, Option<String>>> {
    let mut patch = BTreeMap::new();
    for key in ID_META_KEYS {
        let Some(v) = meta.get(key) else { continue };
        let alt = match v.strip_prefix("pane-") {
            Some(bare) => bare.to_string(),
            None => format!("pane-{v}"),
        };
        if let Some(new) = aliases.get(v).or_else(|| aliases.get(&alt)) {
            if new != v {
                patch.insert(key.to_string(), Some(new.clone()));
            }
        }
    }
    (!patch.is_empty()).then_some(patch)
}

/// The cols/rows a control-respawned pane's new pty gets: the size the pane was last laid out
/// at, else its grid's size (a pane respawned before its first layout), clamped to a usable
/// minimum. The respawn path spawns the pty at the backend default, so this is what makes the
/// new program see the pane's real width.
fn respawn_pty_size(applied: (usize, usize), grid: (usize, usize)) -> (u16, u16) {
    let (cols, rows) = if applied.0 > 0 && applied.1 > 0 {
        applied
    } else {
        grid
    };
    let clamp = |v: usize, min: u16| u16::try_from(v).unwrap_or(u16::MAX).max(min);
    (clamp(cols, 2), clamp(rows, 1))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_respawned_pty_takes_the_panes_laid_out_size_not_the_default() {
        assert_eq!(respawn_pty_size((291, 81), (291, 81)), (291, 81));
        // Never laid out yet: fall back to the grid's size.
        assert_eq!(respawn_pty_size((0, 0), (120, 40)), (120, 40));
        // Degenerate sizes clamp to a usable minimum; huge ones saturate.
        assert_eq!(respawn_pty_size((0, 0), (0, 0)), (2, 1));
        assert_eq!(respawn_pty_size((100_000, 3), (1, 1)), (u16::MAX, 3));
    }
    /// A control-spawned session the GUI never showed has UNANSWERED terminal queries in its
    /// replay (a just-started program may be waiting on them): its adoption must answer them,
    /// unlike a re-attach or a move, whose replay a previous grid already answered.
    #[test]
    fn a_never_shown_control_pane_adopts_with_an_unanswered_replay() {
        let c = model_pane(Some("claude"), None);
        assert!(!detached_pane_from_model("u-new", &c, 14.0).replay_answered);
    }

    #[test]
    fn old_external_id_prefers_the_persisted_map_then_the_uid() {
        let mut m = HashMap::new();
        m.insert("u1".to_string(), "pane-ctl".to_string());
        assert_eq!(old_external_id("u1", &m), "pane-ctl");
        assert_eq!(old_external_id("u2", &m), "u2");
    }

    #[test]
    fn alias_meta_patch_rewrites_parent_and_advisor_only() {
        let mut aliases = HashMap::new();
        aliases.insert("pane-old".to_string(), "new1".to_string());
        aliases.insert("adv".to_string(), "new2".to_string());
        let meta: BTreeMap<String, String> =
            [("parent", "old"), ("advisor", "pane-adv"), ("task", "old")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let p = alias_meta_patch(&meta, &aliases).unwrap();
        assert_eq!(p.get("parent"), Some(&Some("new1".to_string())));
        assert_eq!(p.get("advisor"), Some(&Some("new2".to_string())));
        assert!(!p.contains_key("task"));
        let live: BTreeMap<String, String> = [("parent".to_string(), "new1".to_string())].into();
        assert!(alias_meta_patch(&live, &aliases).is_none());
    }

    use super::*;
    use hyperpanes_core::layout::presets::Layout;

    fn model_pane(command: Option<&str>, args: Option<Vec<&str>>) -> ModelPane {
        ModelPane {
            pane_id: "p1".into(),
            window_id: 1,
            tab_id: "1:0".into(),
            label: "agent".into(),
            color: "#888888".into(),
            subtitle: None,
            talk: false,
            command: command.map(Into::into),
            args: args.map(|a| a.into_iter().map(Into::into).collect()),
            shell: None,
            cwd: None,
            meta: None,
            env: None,
        }
    }

    /// Adopt `c` into a fresh GUI state and return the snapshot's spec for it.
    fn adopted_snapshot_spec(c: &ModelPane) -> hyperpanes_core::workspace::model::PaneSpec {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = SessionManager::new(tx);
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        st.adopt_pane(&mgr, detached_pane_from_model("11111111-uuid", c, 14.0));
        let file = st.to_session_file();
        file.groups.unwrap().remove(0).panes.remove(0)
    }

    /// #65: a control-spawned claude pane keeps its argv across a daemon-replacing restore:
    /// the snapshot carries command/args, and the resume appends `--resume <id>` to that argv
    /// instead of typing a bare `claude --resume` into a shell.
    #[test]
    fn adopted_control_claude_pane_restores_with_original_argv() {
        let c = model_pane(
            Some("claude"),
            Some(vec!["--permission-mode", "plan", "--model", "opus"]),
        );
        let spec = adopted_snapshot_spec(&c);
        assert_eq!(spec.command.as_deref(), Some("claude"));
        let plan = crate::state::plan_claude_resume(
            spec.command,
            spec.args,
            "abc-123",
            None,
            None,
            &Default::default(),
        );
        assert_eq!(plan.startup, None, "must not type a bare resume line");
        assert_eq!(plan.command.as_deref(), Some("claude"));
        assert_eq!(
            plan.args.unwrap(),
            [
                "--permission-mode",
                "plan",
                "--model",
                "opus",
                "--resume",
                "abc-123"
            ]
        );
    }

    /// A pane that already went through a resume (restartPane / recoverPane add
    /// `--resume <old>`) restores with exactly ONE `--resume <new>`, not two.
    #[test]
    fn adopted_resumed_pane_restores_with_a_single_resume() {
        let c = model_pane(
            Some("claude"),
            Some(vec![
                "--model",
                "opus",
                "--resume",
                "old-id",
                "--session-id",
                "x",
            ]),
        );
        let spec = adopted_snapshot_spec(&c);
        let plan = crate::state::plan_claude_resume(
            spec.command,
            spec.args,
            "new-id",
            None,
            None,
            &Default::default(),
        );
        assert_eq!(plan.startup, None);
        assert_eq!(plan.command.as_deref(), Some("claude"));
        assert_eq!(
            plan.args.unwrap(),
            ["--model", "opus", "--resume", "new-id"]
        );
    }

    /// A command-string claude launch has its earlier session flags (`--continue`) stripped.
    #[test]
    fn adopted_string_command_with_continue_is_stripped() {
        let c = model_pane(Some("claude --continue --model m"), None);
        let spec = adopted_snapshot_spec(&c);
        let plan = crate::state::plan_claude_resume(
            spec.command,
            spec.args,
            "new-id",
            None,
            None,
            &Default::default(),
        );
        assert_eq!(plan.startup, None);
        assert_eq!(
            plan.command.as_deref(),
            Some("claude --model m --resume new-id")
        );
        assert_eq!(plan.args, None);
    }

    /// #66: a control pane opened with `env` has its explicit spawn env recorded by the relaunch
    /// snapshot (filtered), and a restore spawns with those keys as the base layer. Secrets,
    /// runtime-owned keys and PATH are withheld and only their NAMES are recorded.
    #[test]
    fn control_pane_env_survives_snapshot_and_restore() {
        let mut c = model_pane(Some("claude"), Some(vec!["--model", "opus"]));
        let mut env = hyperpanes_core::session::spawn::EnvMap::new();
        for (k, v) in [
            ("CLAUDE_CONFIG_DIR", "/home/user/.claude-a"),
            ("ENABLE_TOOL_SEARCH", "false"),
            ("HP_GOAL_ACCOUNTS", "a,b"),
            ("GH_TOKEN", "x"),
            ("HYPERPANES_PANE_ID", "p1"),
            ("PATH", "/usr/bin"),
        ] {
            env.insert(k.to_string(), v.to_string());
        }
        c.env = Some(env);
        let spec = adopted_snapshot_spec(&c);
        let kept = spec.env.as_ref().expect("env persisted");
        assert_eq!(
            kept.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "CLAUDE_CONFIG_DIR",
                "ENABLE_TOOL_SEARCH",
                "HP_GOAL_ACCOUNTS"
            ]
        );
        assert_eq!(
            spec.env_dropped.as_deref(),
            Some(&["GH_TOKEN", "HYPERPANES_PANE_ID", "PATH"].map(String::from)[..])
        );
        let spawn = hyperpanes_core::workspace::env_persist::merge_restore_env(
            spec.env.as_ref(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(spawn["ENABLE_TOOL_SEARCH"], "false");
        assert_eq!(spawn["HP_GOAL_ACCOUNTS"], "a,b");
        assert!(!spawn.contains_key("GH_TOKEN"));
    }

    /// A control pane running a non-claude program round-trips its command; no resume.
    #[test]
    fn adopted_control_non_claude_pane_round_trips_command() {
        let c = model_pane(Some("sh"), Some(vec!["-c", "echo hi"]));
        let spec = adopted_snapshot_spec(&c);
        assert_eq!(spec.command.as_deref(), Some("sh"));
        let plan = crate::state::plan_claude_resume(
            spec.command,
            spec.args,
            "abc-123",
            None,
            None,
            &Default::default(),
        );
        assert_eq!(plan.startup, None);
        assert_eq!(plan.command.as_deref(), Some("sh"));
        assert_eq!(plan.args.unwrap(), ["-c", "echo hi"]);
    }

    /// #67: a control-spawned worker-runner pane (not a claude program) restores with its exact
    /// argv and its filtered env, so it re-claims after a daemon-replacing restart. A claude
    /// session marker (the resume id) never appends `--resume` to a non-claude program.
    #[test]
    fn control_worker_pane_restores_with_argv_env_and_no_resume() {
        let mut c = model_pane(
            Some("hyperpanes"),
            Some(vec![
                "worker",
                "--queue",
                "myapp-g1",
                "--worktree",
                "--base",
                "main",
                "--",
                "sh",
                "-c",
                "echo hi",
            ]),
        );
        let mut env = hyperpanes_core::session::spawn::EnvMap::new();
        for (k, v) in [
            ("HP_GOAL_PERSONA_DIR", "/opt/personas"),
            ("CLAUDE_CONFIG_DIR", "/home/user/.claude-a"),
            ("GH_TOKEN", "x"),
        ] {
            env.insert(k.to_string(), v.to_string());
        }
        c.env = Some(env);
        let spec = adopted_snapshot_spec(&c);
        let want_args = spec.args.clone().unwrap();
        let plan = crate::state::plan_claude_resume(
            spec.command.clone(),
            spec.args.clone(),
            "abc-123",
            None,
            None,
            &hyperpanes_core::claude_panes::RecordedArgv::Absent,
        );
        assert_eq!(plan.startup, None);
        assert_eq!(plan.command.as_deref(), Some("hyperpanes"));
        let args = plan.args.unwrap();
        assert_eq!(args, want_args, "same args, no --resume appended");
        assert!(!args.iter().any(|a| a == "--resume"));
        assert_eq!(args[..3], ["worker", "--queue", "myapp-g1"]);
        let spawn = hyperpanes_core::workspace::env_persist::merge_restore_env(
            spec.env.as_ref(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(spawn["HP_GOAL_PERSONA_DIR"], "/opt/personas");
        assert_eq!(spawn["CLAUDE_CONFIG_DIR"], "/home/user/.claude-a");
        assert!(!spawn.contains_key("GH_TOKEN"));
    }

    /// #68 regression: `claude --dangerously-skip-permissions --model opus` typed into a shell
    /// pane restores with both flags (the base typed a bare `claude --resume`).
    #[test]
    fn shell_pane_restore_replays_typed_claude_flags() {
        use hyperpanes_core::claude_panes::RecordedArgv;
        let argv = RecordedArgv::Argv(
            ["--dangerously-skip-permissions", "--model", "opus"]
                .map(String::from)
                .to_vec(),
        );
        let plan = crate::state::plan_claude_resume(
            None,
            None,
            "abc-123",
            Some("/home/user/dev/myapp"),
            None,
            &argv,
        );
        assert_eq!(
            plan.startup.as_deref(),
            Some(
                "cd '/home/user/dev/myapp' && claude '--dangerously-skip-permissions' \
                 '--model' 'opus' --resume abc-123\r"
            )
        );
        // A recorded-but-unusable argv types a visible notice, then the bare line.
        let plan = crate::state::plan_claude_resume(
            None,
            None,
            "abc-123",
            None,
            None,
            &RecordedArgv::Unavailable("secret".into()),
        );
        assert_eq!(
            plan.startup.as_deref(),
            Some(
                "echo '[hyperpanes] resumed without the original claude flags (secret)'; \
                 claude --resume abc-123\r"
            )
        );
        // An old marker: the bare line, no notice.
        let plan = crate::state::plan_claude_resume(
            None,
            None,
            "abc-123",
            None,
            None,
            &RecordedArgv::Absent,
        );
        assert_eq!(plan.startup.as_deref(), Some("claude --resume abc-123\r"));
    }

    fn pane(id: &str, uid: &str) -> PaneInfo {
        PaneInfo {
            id: id.to_string(),
            session_uid: uid.to_string(),
            label: "shell".to_string(),
            subtitle: None,
            talk: false,
            env: None,
            color: "#888888".to_string(),
            command: None,
            args: None,
            cwd: None,
            shell: None,
            status: PaneStatus::Running,
            exit_code: None,
            meta: None,
        }
    }

    fn model_with(panes: Vec<PaneInfo>) -> ReadModel {
        let mut m = ReadModel::new();
        m.add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("1:0".to_string()),
            tabs: vec![TabInfo {
                id: "1:0".to_string(),
                title: "Tab 1".to_string(),
                color: None,
                layout: "auto".to_string(),
                panes,
                pane_group: None,
            }],
        });
        m
    }

    fn snap(title: &str, layout: &str) -> TabSnap {
        TabSnap {
            title: title.into(),
            layout: layout.into(),
            color: None,
        }
    }

    #[test]
    fn diff_tabs_reports_layout_and_title_changes_on_any_tab() {
        let prev: HashMap<_, _> = [
            ("0:0".to_string(), snap("term 1", "auto")),
            ("0:1".to_string(), snap("term 2", "single")),
        ]
        .into();
        let mut cur = prev.clone();
        cur.insert("0:0".into(), snap("term 1", "single"));
        cur.insert("0:1".into(), snap("renamed", "single"));
        cur.insert("0:2".into(), snap("brand new", "grid")); // new tab: not a delta
        let d = diff_tabs(&prev, &cur);
        assert_eq!(
            d,
            vec![
                TabChange {
                    tab_id: "0:0".into(),
                    title: None,
                    layout: Some("single".into()),
                    color: None
                },
                TabChange {
                    tab_id: "0:1".into(),
                    title: Some("renamed".into()),
                    layout: None,
                    color: None
                },
            ]
        );
        assert!(diff_tabs(&prev, &prev).is_empty());
    }

    #[test]
    fn diff_tabs_reports_colour_set_and_clear() {
        let prev: HashMap<_, _> = [
            ("0:0".to_string(), snap("a", "auto")),
            ("0:1".to_string(), snap("b", "auto")),
        ]
        .into();
        let mut p2 = prev.clone();
        p2.get_mut("0:1").unwrap().color = Some("#aabbcc".into());
        let mut cur = p2.clone();
        cur.get_mut("0:0").unwrap().color = Some("#112233".into());
        cur.get_mut("0:1").unwrap().color = None;
        let d = diff_tabs(&p2, &cur);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].color, Some(Some("#112233".to_string())));
        assert_eq!(d[1].color, Some(None));
        assert!(d.iter().all(|c| c.title.is_none() && c.layout.is_none()));
        assert!(diff_tabs(&prev, &prev).is_empty());
    }

    #[test]
    fn apply_tab_change_sets_and_clears_colour() {
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        let mk = |c: Option<&str>| TabChange {
            tab_id: "0:0".into(),
            title: None,
            layout: None,
            color: Some(c.map(str::to_string)),
        };
        assert!(apply_tab_change(&mut st, 0, &mk(Some("#aabbcc"))));
        assert_eq!(st.tabs[0].color, Some(Color::from_rgb_u8(0xaa, 0xbb, 0xcc)));
        assert!(apply_tab_change(&mut st, 0, &mk(None)));
        assert_eq!(st.tabs[0].color, None);
    }

    #[test]
    fn control_title_change_leaves_other_tabs_inline_edit_alone() {
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        st.tabs
            .push(crate::state::Tab::new_for_test("term 2".into()));
        st.editing_tab = 1;
        let ch = TabChange {
            tab_id: "0:0".into(),
            title: Some("renamed".into()),
            layout: None,
            color: None,
        };
        assert!(apply_tab_change(&mut st, 0, &ch));
        assert_eq!(st.editing_tab, 1);
        assert_eq!(st.tabs[0].title.as_str(), "renamed");
    }

    #[test]
    fn apply_tab_change_targets_background_tab_and_keeps_active() {
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        st.tabs
            .push(crate::state::Tab::new_for_test("term 2".into()));
        assert_eq!(st.tabs.len(), 2);
        st.switch_tab(1);
        let ch = TabChange {
            tab_id: "0:0".into(),
            title: Some("renamed".into()),
            layout: Some("grid".into()),
            color: None,
        };
        assert!(apply_tab_change(&mut st, 0, &ch));
        assert_eq!(st.active, 1);
        assert_eq!(st.tabs[0].layout, Layout::Grid);
        assert_eq!(st.tabs[0].title.as_str(), "renamed");
        assert_eq!(st.tabs[1].layout, Layout::Auto);
        assert!(!apply_tab_change(&mut st, 9, &ch));
    }

    #[test]
    fn control_tab_id_survives_republish_and_set_layout_applies() {
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        let mut t = crate::state::Tab::new_for_test("term 2".into());
        t.ctl_id = Some("3f2a9c1e-uuid".into());
        st.tabs.push(t);
        // Republish: control tab keeps its id, native tab stays positional.
        let ids = |st: &crate::state::State| -> Vec<String> {
            st.tabs
                .iter()
                .enumerate()
                .map(|(i, t)| tab_model_id(0, i, t))
                .collect()
        };
        assert_eq!(ids(&st), vec!["0:0", "3f2a9c1e-uuid"]);
        // Reordering the tabs doesn't change the control id.
        st.tabs.swap(0, 1);
        assert_eq!(ids(&st), vec!["3f2a9c1e-uuid", "0:1"]);
        // setLayout with the id attach returned still resolves and applies.
        let idx = resolve_tab_index(&st.tabs, "3f2a9c1e-uuid").unwrap();
        let ch = TabChange {
            tab_id: "3f2a9c1e-uuid".into(),
            title: None,
            layout: Some("grid".into()),
            color: None,
        };
        assert!(apply_tab_change(&mut st, idx, &ch));
        assert_eq!(st.tabs[0].layout, Layout::Grid);
    }

    #[test]
    fn move_requests_rehost_live_panes_across_tabs_and_windows() {
        use hyperpanes_core::control::server::{MoveTarget as T, PaneGroupRequest as R};
        use std::cell::RefCell;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let m = SessionManager::new(tx);
        let det = |n: &str| crate::state::DetachedPane {
            replay_answered: true,
            live: Default::default(),
            uid: n.into(),
            title: format!("{n} title").into(),
            subtitle: Some(format!("{n} sub").into()),
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            spawn_env: None,
        };
        let w1 = RefCell::new(crate::state::State::new(crate::theme::load_font(1.0)));
        let w2 = RefCell::new(crate::state::State::new(crate::theme::load_font(1.0)));
        for n in ["a", "b", "c"] {
            w1.borrow_mut().adopt_pane(&m, det(n));
        }
        w2.borrow_mut().adopt_pane(&m, det("z"));
        w1.borrow_mut().tabs[0].panes[0].cwd = Some("/work/a".into());
        w1.borrow_mut().tabs[0].panes[0].talk = true;
        let states = [(1i64, &w1), (2i64, &w2)];
        let resolve = |id: &str| id.trim_start_matches("ctl-").to_string();
        let mv = |pane: &str, window_id: i64, to: T| R::Move {
            pane_id: pane.into(),
            window_id,
            to,
        };

        // newTab in the same window: made in the background, carries label/subtitle/cwd/talk.
        let r = mv(
            "ctl-a",
            1,
            T::NewTab {
                tab_id: "new-1".into(),
                title: Some("proj-a".into()),
                color: Some("#a855f7".into()),
            },
        );
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Moved
        );
        {
            let st = w1.borrow();
            assert_eq!(st.tabs.len(), 2);
            assert_eq!(st.active, 0, "a control move never steals focus");
            let t = &st.tabs[1];
            assert_eq!(t.title.as_str(), "proj-a");
            assert_eq!(t.ctl_id.as_deref(), Some("new-1"));
            assert!(t.color.is_some());
            let p = &t.panes[0];
            assert_eq!(p.uid, "a");
            assert_eq!(p.title.as_str(), "a title");
            assert_eq!(p.subtitle.as_deref(), Some("a sub"));
            assert_eq!(p.cwd.as_deref(), Some("/work/a"));
            assert!(p.talk);
            assert_eq!(st.tabs[0].panes.len(), 2);
        }

        // To an existing tab (by the id the API returned) in the same window.
        let r = mv(
            "ctl-b",
            1,
            T::Tab {
                tab_id: "new-1".into(),
            },
        );
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Moved
        );
        assert_eq!(w1.borrow().tabs[1].panes.len(), 2);
        // Already there → Moved, no change.
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Moved
        );
        assert_eq!(w1.borrow().tabs[1].panes.len(), 2);

        // Project tab: created as a pane group the first time (in window 2), reused after.
        let proj = |tab: &str| T::Project {
            tab_id: tab.into(),
            project_id: "pid-1".into(),
            title: "proj-a".into(),
            color: Some("#22c55e".into()),
        };
        let r = mv("ctl-c", 2, proj("ptab-1"));
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Moved
        );
        {
            let st = w2.borrow();
            assert_eq!(st.tabs.len(), 2);
            let t = &st.tabs[1];
            assert_eq!(t.title.as_str(), "proj-a");
            assert_eq!(t.ctl_id.as_deref(), Some("ptab-1"));
            let g = t
                .pane_group
                .as_ref()
                .expect("a project tab is a pane group");
            assert_eq!(g.project.as_deref(), Some("pid-1"));
            assert_eq!(t.panes[0].uid, "c");
        }
        // Window 1's source tab emptied → dropped, active index still valid.
        assert_eq!(w1.borrow().tabs.len(), 1);
        assert_eq!(w1.borrow().active, 0);
        let r = mv("ctl-a", 2, proj("ptab-2"));
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Moved
        );
        {
            let st = w2.borrow();
            assert_eq!(st.tabs.len(), 2, "the project's tab is reused");
            assert_eq!(
                st.tabs[1].ctl_id.as_deref(),
                Some("ptab-1"),
                "a tab keeps its id"
            );
            assert_eq!(st.tabs[1].panes.len(), 2);
        }

        // The window's only pane is refused; an unknown pane is NotFound (retried later);
        // an unknown target tab or window is refused. Nothing moves.
        let r = mv(
            "ctl-b",
            2,
            T::Tab {
                tab_id: "2:0".into(),
            },
        );
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Refused
        );
        assert_eq!(w1.borrow().tabs[0].panes[0].uid, "b");
        let r = mv(
            "ctl-nope",
            1,
            T::Tab {
                tab_id: "1:0".into(),
            },
        );
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::NotFound
        );
        let r = mv(
            "ctl-z",
            1,
            T::Tab {
                tab_id: "zzz".into(),
            },
        );
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Refused
        );
        let r = mv(
            "ctl-z",
            9,
            T::Tab {
                tab_id: "1:0".into(),
            },
        );
        assert_eq!(
            apply_move_request(&states, &m, &resolve, &r),
            MoveOutcome::Refused
        );
        assert_eq!(w2.borrow().tabs[0].panes[0].uid, "z");
    }

    #[test]
    fn pane_group_requests_drain_into_state() {
        use hyperpanes_core::control::server::PaneGroupRequest as R;
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let m = SessionManager::new(tx);
        for n in ["a", "b", "c"] {
            st.adopt_pane(
                &m,
                crate::state::DetachedPane {
                    replay_answered: true,
                    live: Default::default(),
                    uid: n.into(),
                    title: n.into(),
                    subtitle: None,
                    pinned_accent: None,
                    show_frame: None,
                    show_dot: None,
                    font_px: 14.0,
                    spawn_command: None,
                    spawn_args: None,
                    spawn_shell: None,
                    spawn_env: None,
                },
            );
        }
        // Control pane ids are aliases for GUI uids.
        let resolve = |id: &str| id.trim_start_matches("ctl-").to_string();
        let group = R::Group {
            tab_id: "tab-uuid-1".into(),
            window_id: 7,
            pane_ids: vec!["ctl-a".into(), "ctl-b".into()],
            title: Some("My group".into()),
            project: None,
        };
        // wrong window → untouched
        assert!(!apply_pane_group_request(&mut st, &m, 8, &resolve, &group));
        assert_eq!(st.tabs.len(), 1);
        assert!(apply_pane_group_request(&mut st, &m, 7, &resolve, &group));
        assert_eq!(st.tabs.len(), 2);
        let g = &st.tabs[1];
        assert!(g.pane_group.is_some());
        assert_eq!(g.title.as_str(), "My group");
        assert_eq!(
            g.ctl_id.as_deref(),
            Some("tab-uuid-1"),
            "the id the API returned resolves"
        );
        assert_eq!(resolve_tab_index(&st.tabs, "tab-uuid-1"), Some(1));
        // ungroup by that id returns the panes; the tab is gone.
        let un = R::Ungroup {
            tab_id: "tab-uuid-1".into(),
        };
        assert!(apply_pane_group_request(&mut st, &m, 7, &resolve, &un));
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.tabs[0].panes.len(), 3);
        // ungroup of an unknown / non-group tab is a no-op
        assert!(!apply_pane_group_request(&mut st, &m, 7, &resolve, &un));
        // <2 resolvable panes → no group
        let one = R::Group {
            tab_id: "t2".into(),
            window_id: 7,
            pane_ids: vec!["ctl-a".into(), "ctl-nope".into()],
            title: None,
            project: None,
        };
        assert!(!apply_pane_group_request(&mut st, &m, 7, &resolve, &one));
        assert_eq!(st.tabs.len(), 1);
    }

    /// A pane the model dropped leaves the GUI only when its session is dead; a live
    /// one is kept (removing it would orphan the program with no pane).
    #[test]
    fn model_dropped_panes_keeps_live_sessions() {
        let prev: Vec<String> = ["closed", "orphan", "still", "gone-gui"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cur: HashMap<String, ModelPane> = HashMap::new();
        let mut cur = cur;
        let still = ModelPane {
            pane_id: "still".into(),
            window_id: 0,
            tab_id: "0:0".into(),
            label: "s".into(),
            color: "#888888".into(),
            subtitle: None,
            talk: false,
            command: None,
            args: None,
            shell: None,
            cwd: None,
            meta: None,
            env: None,
        };
        cur.insert("still".into(), still);
        let gui: HashSet<String> = ["closed", "orphan", "still"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (remove, keep) = model_dropped_panes(prev.iter(), &cur, &gui, |u| u == "orphan");
        assert_eq!(remove, vec!["closed".to_string()]);
        assert_eq!(keep, vec!["orphan".to_string()]);
    }

    #[test]
    fn resolve_tab_index_uuid_positional_and_unknown() {
        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        let mut t = crate::state::Tab::new_for_test("b".into());
        t.ctl_id = Some("abc-123".into());
        st.tabs.push(t);
        assert_eq!(resolve_tab_index(&st.tabs, "abc-123"), Some(1));
        assert_eq!(resolve_tab_index(&st.tabs, "0:0"), Some(0));
        assert_eq!(resolve_tab_index(&st.tabs, "0:1"), Some(1));
        assert_eq!(resolve_tab_index(&st.tabs, "0:7"), None);
        assert_eq!(resolve_tab_index(&st.tabs, "nope-uuid"), None);
    }

    #[test]
    fn ctl_fields_carry_spawn_env_across_republish() {
        let mut env = hyperpanes_core::session::spawn::EnvMap::new();
        env.insert("MY_VAR".to_string(), "x".to_string());
        let mp = ModelPane {
            pane_id: "ctl-1".to_string(),
            window_id: 1,
            tab_id: "1:0".to_string(),
            label: "shell".to_string(),
            color: "#888888".to_string(),
            subtitle: None,
            talk: false,
            command: Some("tool".to_string()),
            args: None,
            shell: None,
            cwd: None,
            meta: None,
            env: Some(env.clone()),
        };
        // reconcile stores it in the side store; publish re-stamps it onto the rebuilt pane.
        let c = CtlFields::from_model(&mp);
        assert_eq!(c.env.as_ref(), Some(&env));
    }

    #[test]
    fn gui_spawned_pane_launch_spec_reaches_the_resume_path() {
        let mut env = hyperpanes_core::session::spawn::EnvMap::new();
        env.insert("MY_VAR".to_string(), "x".to_string());
        let args = vec!["--model".to_string(), "sonnet".to_string()];
        // No control entry (default CtlFields): the GUI pane's own spec is published.
        let c = effective_launch_spec(
            CtlFields::default(),
            Some(&"claude".to_string()),
            Some(&args),
            None,
            Some(&env),
        );
        assert_eq!(c.command.as_deref(), Some("claude"));
        assert_eq!(c.args.as_ref(), Some(&args));
        assert_eq!(c.env.as_ref(), Some(&env));
        // A control-owned spec is left alone.
        let ctl = CtlFields {
            command: Some("other".to_string()),
            ..CtlFields::default()
        };
        let c = effective_launch_spec(ctl, Some(&"claude".to_string()), Some(&args), None, None);
        assert_eq!(c.command.as_deref(), Some("other"));
        assert!(c.args.is_none());
    }

    #[test]
    fn lost_control_pane_is_detected() {
        let mut ids = HashMap::new();
        ids.insert("u1".to_string(), "ctl-1".to_string());
        let model = model_with(vec![]);
        let gui = HashSet::new();
        let lost = lost_control_panes(&ids, &model, &gui, &|_| true);
        assert_eq!(lost, vec![("u1".to_string(), "ctl-1".to_string())]);
    }

    #[test]
    fn gui_native_alias_and_tracked_or_dead_panes_are_not_lost() {
        let mut ids = HashMap::new();
        // GUI-native pane (uid == pane id): structurally excluded — never healed, so the
        // closed-tab undo buffer / parked reminders can keep sessions alive off-layout.
        ids.insert("u-gui".to_string(), "u-gui".to_string());
        // Still present in the read-model → not lost.
        ids.insert("u-in-model".to_string(), "ctl-m".to_string());
        // Hosted by the GUI (mid-adoption) → not lost.
        ids.insert("u-in-gui".to_string(), "ctl-g".to_string());
        // Session dead → nothing to restore.
        ids.insert("u-dead".to_string(), "ctl-d".to_string());
        let model = model_with(vec![pane("ctl-m", "u-in-model")]);
        let mut gui = HashSet::new();
        gui.insert("u-in-gui".to_string());
        let lost = lost_control_panes(&ids, &model, &gui, &|uid| uid != "u-dead");
        assert!(lost.is_empty(), "unexpected heal targets: {lost:?}");
    }

    #[test]
    fn pane_id_already_in_model_is_not_healed_twice() {
        let mut ids = HashMap::new();
        ids.insert("u-new".to_string(), "ctl-1".to_string());
        // A restartPane swapped the session uid but kept the stable pane id: the pane EXISTS
        // under a different uid — healing would duplicate it.
        let model = model_with(vec![pane("ctl-1", "u-old")]);
        let lost = lost_control_panes(&ids, &model, &HashSet::new(), &|_| true);
        assert!(lost.is_empty(), "unexpected heal targets: {lost:?}");
    }

    #[test]
    fn healed_pane_keeps_last_known_label_and_meta() {
        let known = (
            "build-bot".to_string(),
            Some(BTreeMap::from([("role".to_string(), "worker".to_string())])),
        );
        let (label, meta) = healed_identity(Some(&known));
        assert_eq!(label, "build-bot");
        assert_eq!(meta["role"], "worker");
        assert_eq!(meta["hp.recovered"], "1");
        let (label, meta) = healed_identity(None);
        assert_eq!(label, "recovered");
        assert_eq!(meta.len(), 1);
    }
}
