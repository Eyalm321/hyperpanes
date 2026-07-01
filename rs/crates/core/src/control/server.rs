//! The control server: bind `127.0.0.1` on an EPHEMERAL port, build the axum router (routes +
//! the `/events` WS upgrade with token auth via header or `?token=`), and write the `control.json`
//! discovery file under `persistence::paths` — exact fields + 2-space pretty JSON:
//! `{ port, token, pid, version, events: "ws://127.0.0.1:<port>/events?token=<token>" }` —
//! removing it on shutdown. Owns/holds the `SessionManager` + `readmodel` + `tokens` + inbox +
//! locks. Toggled by control-settings (enabled / allowInput), default OFF.
//!
//! This is the single place the cores meet at runtime. It also runs the central activity ticker
//! (busy⇄idle flips → `activity` frames, computed from `session_manager.last_output_at` at the
//! `idleAlertSeconds` threshold) and forwards live `SessionEvent`s to the read-model + WS clients.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::runtime::Handle;

use serde::Serialize;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::control::events::{ControlEvent, EventHub};
use crate::control::inbox::MessageInbox;
use crate::control::lock::PaneLocks;
use crate::control::readmodel::{Activity, PaneInfo, PaneRef, PaneStatus, ReadModel};
use crate::control::routes;
use crate::control::supervisor::{Decision, Supervisor};
use crate::control::tokens::{random_token, TokenStore};
use crate::control::work::WorkQueue;
use crate::session_manager::{SessionEvent, SessionManager};

/// The renderer idle threshold (`useSettings.idleAlertSeconds`, default 10s): a running pane with
/// no pty output for at least this long reads as `idle`, else `busy`.
pub const IDLE_THRESHOLD_MS: i64 = 10_000;

/// How often the activity ticker re-evaluates liveness and fans out busy⇄idle flips.
const ACTIVITY_TICK_MS: u64 = 500;

/// Coalescing window for structure-only `state` pings (TS `notifyState`).
const STATE_COALESCE_MS: u64 = 100;

/// Everything the running control server owns, behind cheap component locks. Cloned-shared via
/// `Arc` into every axum handler + the background tasks.
pub struct Shared {
    pub model: Mutex<ReadModel>,
    pub tokens: Mutex<TokenStore>,
    pub inbox: Mutex<MessageInbox>,
    pub locks: Mutex<PaneLocks>,
    /// Durable, claimable work queue backing the `/queues` + `/tasks` routes
    /// (worker-pool phase-2). `rusqlite::Connection` is `Send` but not `Sync`, so —
    /// exactly like every other component — it lives behind a `Mutex` that serializes
    /// queue ops. Opened in-memory here (test-safe, zero-config); a durable embedder
    /// swaps in `WorkQueue::open(paths::work_db())` + boot recovery (work.rs §3.3).
    pub work: Mutex<WorkQueue>,
    pub events: EventHub,
    /// Phase-5 auto-restart supervisor: per-pane policy + retry ledger. Default-empty ⇒
    /// every `Exit` runs the legacy path until a pane opts in via `hp.supervise` meta.
    pub supervisor: Mutex<Supervisor>,
    pub sessions: Arc<SessionManager>,
    pub allow_input: AtomicBool,
    pub pid: u32,
    pub version: String,
    pub port: AtomicU16,
    pub idle_threshold_ms: i64,
    pub control_file: PathBuf,
    state_scheduled: AtomicBool,
    /// Set by the `/projects` routes (and a project-opening `newPane`) when they mutate the
    /// `projects.json` registry off the UI thread. The GUI host clears it each tick (see
    /// `ControlHost::sync`) to reload the sidebar rail live; in the headless bin it is simply
    /// never read. A plain flag, not coalesced — the UI tick polls it cheaply.
    projects_dirty: AtomicBool,
    /// Requested bind `(address, port)` for the listener. Defaults to `("127.0.0.1", 0)` —
    /// loopback, ephemeral — the frozen legacy behaviour. An embedder sets this from
    /// `control_settings` BEFORE `run_server` to allow remote clients (mobile app over
    /// Tailscale/LAN). Port `0` = OS-assigned.
    bind: Mutex<(String, u16)>,
    /// The runtime to spawn background tasks on (the coalescer in [`notify_state`]). Set once by
    /// an embedder that wants spawns bound to an explicit runtime rather than the ambient
    /// thread-local — the GUI host sets this so a `notify_state` from the UI thread can never
    /// panic if the runtime guard is ever absent. Unset ⇒ fall back to the ambient `tokio::spawn`.
    runtime: OnceLock<Handle>,
}

impl Shared {
    /// Build the shared state. `allow_input` mirrors control-settings; `control_file` is both the
    /// path written on start and the `HYPERPANES_CONTROL_FILE` injected into spawned panes.
    pub fn new(
        sessions: Arc<SessionManager>,
        allow_input: bool,
        version: impl Into<String>,
        control_file: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Shared {
            model: Mutex::new(ReadModel::new()),
            tokens: Mutex::new(TokenStore::new()),
            inbox: Mutex::new(MessageInbox::new()),
            locks: Mutex::new(PaneLocks::new()),
            work: Mutex::new(WorkQueue::open_in_memory().expect("open in-memory work queue")),
            events: EventHub::new(),
            supervisor: Mutex::new(Supervisor::new()),
            sessions,
            allow_input: AtomicBool::new(allow_input),
            pid: std::process::id(),
            version: version.into(),
            port: AtomicU16::new(0),
            idle_threshold_ms: IDLE_THRESHOLD_MS,
            control_file,
            state_scheduled: AtomicBool::new(false),
            projects_dirty: AtomicBool::new(false),
            bind: Mutex::new(("127.0.0.1".to_string(), 0)),
            runtime: OnceLock::new(),
        })
    }

    /// Set the requested bind address/port (from `control_settings`) BEFORE `run_server`.
    /// `address` must be a bare IP (the settings loader already validated it); `port` 0 =
    /// ephemeral.
    pub fn set_bind(&self, address: impl Into<String>, port: u16) {
        *self.bind.lock().unwrap() = (address.into(), port);
    }

    /// The requested `(address, port)` to bind.
    pub fn bind_config(&self) -> (String, u16) {
        self.bind.lock().unwrap().clone()
    }

    /// The host to advertise in `control.json` / `POST /tokens` events URLs: the bind
    /// address itself, except unspecified (`0.0.0.0` / `::`) — which listens on loopback
    /// too — where the legacy `127.0.0.1` stays correct for local readers.
    pub fn advertised_host(&self) -> String {
        let (addr, _) = self.bind_config();
        match addr.parse::<std::net::IpAddr>() {
            Ok(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
            Ok(_) => addr,
            Err(_) => "127.0.0.1".to_string(),
        }
    }

    /// Flag that the project registry changed off-thread (a `/projects` write or a
    /// project-opening `newPane`), so the GUI host reloads the sidebar rail next tick.
    pub fn mark_projects_dirty(&self) {
        self.projects_dirty.store(true, Ordering::SeqCst);
    }

    /// Atomically read-and-clear the project-registry dirty flag. The GUI host calls this
    /// once per UI tick; returns `true` exactly once per batch of changes.
    pub fn take_projects_dirty(&self) -> bool {
        self.projects_dirty.swap(false, Ordering::SeqCst)
    }

    /// Bind background-task spawns (the `notify_state` coalescer) to an explicit runtime handle.
    /// Idempotent — only the first set takes effect.
    pub fn set_runtime(&self, handle: Handle) {
        let _ = self.runtime.set(handle);
    }

    /// Spawn a background task on the stored runtime handle if set, else the ambient runtime.
    fn spawn_task<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match self.runtime.get() {
            Some(h) => {
                h.spawn(fut);
            }
            None => {
                tokio::spawn(fut);
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    pub fn allow_input(&self) -> bool {
        self.allow_input.load(Ordering::SeqCst)
    }

    /// Reconcile each live pane's supervisor policy from its `meta` map, and forget panes
    /// that no longer exist. Called after every structural `/command` so a `setMeta` that
    /// flips `hp.supervise`, or a `newPane` with a `meta.hp.supervise`, takes effect. Cheap
    /// and idempotent — a disabled policy is recorded but yields `Decision::None`.
    pub fn reconcile_policies(&self) {
        use crate::control::supervisor::Policy;
        let panes: Vec<(String, std::collections::BTreeMap<String, String>)> = {
            let m = self.model.lock().unwrap();
            m.panes()
                .into_iter()
                .map(|pr| {
                    let meta = m
                        .pane(&pr.pane_id)
                        .and_then(|p| p.meta.clone())
                        .unwrap_or_default();
                    (pr.pane_id, meta)
                })
                .collect()
        };
        let live: std::collections::HashSet<&str> =
            panes.iter().map(|(id, _)| id.as_str()).collect();
        let mut sup = self.supervisor.lock().unwrap();
        for (pane_id, meta) in &panes {
            sup.set_policy(pane_id, Policy::from_meta(meta));
        }
        sup.retain_panes(&live);
    }

    /// Resolve a pane's liveness from the session manager + the idle threshold. Mirrors the TS
    /// `status==='exited' ? 'exited' : idle ? 'idle' : 'busy'` with idle = "no output for the
    /// threshold"; a never-output pane reads `busy` (the renderer's `markActivity` only fires on
    /// output).
    pub fn compute_activity(&self, pane: &PaneInfo) -> Activity {
        self.activity_for(&pane.session_uid, pane.status)
    }

    fn activity_for(&self, uid: &str, status: PaneStatus) -> Activity {
        if status == PaneStatus::Exited {
            return Activity::Exited;
        }
        // Phase 4: prefer the precise, marker-derived state. The gate is `marker_seen` —
        // until a pane has EVER emitted a prompt marker, the legacy silence heuristic owns
        // its activity, so an un-instrumented pane is byte-for-byte unchanged.
        if let Some(l) = self.sessions.liveness(uid) {
            if l.marker_seen {
                // A command known to be running stays Busy THROUGH output silence — the
                // whole fix. A returned prompt is a positive AwaitingInput edge.
                return if l.command_running {
                    Activity::Busy
                } else if l.prompt_ready {
                    Activity::AwaitingInput
                } else {
                    // Markers seen but neither flag set (e.g. right after a write cleared
                    // prompt_ready): treat as Busy — the pane is mid-turn.
                    Activity::Busy
                };
            }
        }
        // Fallback: the legacy 10s output-silence heuristic (labeled, unchanged).
        match self.sessions.last_output_at(uid) {
            Some(t) if now_ms() - (t as i64) >= self.idle_threshold_ms => Activity::Idle,
            _ => Activity::Busy,
        }
    }
}

/// Map a computed [`Activity`] + liveness snapshot into a `liveness` frame. The `state`
/// string is `working | awaiting-input | done | exited`; `done` is used when the pane is
/// awaiting input AND its last command exited cleanly (code 0), else `awaiting-input`.
fn liveness_frame(
    pane_id: &str,
    act: Activity,
    liveness: Option<crate::session_manager::Liveness>,
) -> ControlEvent {
    let exit_code = liveness.and_then(|l| l.last_exit_code);
    let state = match act {
        Activity::Busy => "working",
        Activity::AwaitingInput => match exit_code {
            Some(0) => "done",
            _ => "awaiting-input",
        },
        Activity::Idle => "awaiting-input",
        Activity::Exited => "exited",
    };
    ControlEvent::Liveness {
        pane_id: pane_id.to_string(),
        state: state.to_string(),
        exit_code,
    }
}

/// Current epoch-ms (the TS `Date.now()`).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Coalesce structure changes into one "re-fetch /state" ping per ~100ms tick (TS `notifyState`).
pub fn notify_state(shared: &Arc<Shared>) {
    if !shared.events.has_clients() {
        return;
    }
    // First caller in the window arms the timer; the rest are no-ops.
    if shared
        .state_scheduled
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let shared = Arc::clone(shared);
    shared.clone().spawn_task(async move {
        tokio::time::sleep(Duration::from_millis(STATE_COALESCE_MS)).await;
        shared.state_scheduled.store(false, Ordering::SeqCst);
        shared.events.broadcast(&ControlEvent::State);
    });
}

/// The control.json discovery shape — field order + 2-space pretty JSON match TS `writeDiscovery`.
/// `bindAddress` is additive (mobile-client remote access) and OMITTED for the default
/// loopback bind, keeping the legacy shape byte-identical.
#[derive(Serialize)]
struct Discovery<'a> {
    port: u16,
    token: &'a str,
    pid: u32,
    version: &'a str,
    events: String,
    #[serde(rename = "bindAddress", skip_serializing_if = "Option::is_none")]
    bind_address: Option<&'a str>,
}

/// Build the WS events URL for a host + port + token (also used by `POST /tokens`).
pub fn events_url(host: &str, port: u16, token: &str) -> String {
    format!("ws://{host}:{port}/events?token={token}")
}

fn write_discovery(shared: &Arc<Shared>) -> io::Result<()> {
    let port = shared.port();
    let token = shared.tokens.lock().unwrap().master().map(str::to_string);
    let Some(token) = token else {
        return Ok(());
    };
    let (bind_addr, _) = shared.bind_config();
    let host = shared.advertised_host();
    let discovery = Discovery {
        port,
        token: &token,
        pid: shared.pid,
        version: &shared.version,
        events: events_url(&host, port, &token),
        bind_address: (bind_addr != "127.0.0.1").then_some(bind_addr.as_str()),
    };
    let json = serde_json::to_string_pretty(&discovery)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    crate::persistence::paths::write_atomic(&shared.control_file, json.as_bytes())
}

/// Remove the discovery file (best-effort), so a stale `control.json` never points at a dead port.
pub fn remove_discovery(shared: &Arc<Shared>) {
    let _ = std::fs::remove_file(&shared.control_file);
}

/// Bind loopback on an ephemeral port, mint the master token, write `control.json`, and serve
/// until the process exits. Never returns under normal operation. The activity ticker is a
/// SEPARATE task ([`run_activity_ticker`]) the embedder spawns alongside this one, so its
/// lifetime can be torn down independently (the GUI host aborts it on stop — see `control_host`).
pub async fn run_server(shared: Arc<Shared>) -> io::Result<()> {
    let (addr, req_port) = shared.bind_config();
    let listener = match tokio::net::TcpListener::bind((addr.as_str(), req_port)).await {
        Ok(l) => l,
        Err(e) if addr != "127.0.0.1" || req_port != 0 => {
            // A configured remote bind that fails (port taken, iface gone) must not brick
            // the LOCAL control API — fall back to the frozen loopback/ephemeral default.
            eprintln!(
                "[control] bind {addr}:{req_port} failed ({e}); falling back to 127.0.0.1:0"
            );
            shared.set_bind("127.0.0.1", 0);
            tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?
        }
        Err(e) => return Err(e),
    };
    let port = listener.local_addr()?.port();
    shared.port.store(port, Ordering::SeqCst);

    shared.tokens.lock().unwrap().set_master(random_token());
    write_discovery(&shared)?;

    let app = routes::router(Arc::clone(&shared));
    axum::serve(listener, app).await
}

/// Recompute each pane's activity every tick and broadcast a scope-filtered `activity` frame on
/// each flip of a KNOWN pane (a freshly-seen pane seeds its baseline silently — it rides the
/// `state` ping). A pure busy⇄idle flip does NOT trigger a `state` ping (TS #13). Runs forever
/// until its task is aborted (the GUI host holds its handle and aborts it on stop).
pub async fn run_activity_ticker(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(Duration::from_millis(ACTIVITY_TICK_MS));
    let mut last: HashMap<String, Activity> = HashMap::new();
    loop {
        interval.tick().await;
        let panes: Vec<PaneRef> = shared.model.lock().unwrap().panes();
        let mut seen = std::collections::HashSet::new();
        for pr in &panes {
            seen.insert(pr.pane_id.clone());
            let act = shared.activity_for(&pr.session_uid, pr.status);
            let prev = last.get(&pr.pane_id).copied();
            if prev != Some(act) {
                // Only emit on a flip of an already-tracked pane while someone is streaming.
                if prev.is_some() && shared.events.has_clients() {
                    // Frozen legacy frame: `busy|idle|exited` (AwaitingInput → idle).
                    shared.events.broadcast_for_pane(
                        Some(&pr.coords),
                        &ControlEvent::Activity {
                            pane_id: pr.pane_id.clone(),
                            activity: act.legacy_str().to_string(),
                        },
                    );
                    // Phase-4 precise frame, ignorable by legacy clients.
                    shared.events.broadcast_for_pane(
                        Some(&pr.coords),
                        &liveness_frame(
                            &pr.pane_id,
                            act,
                            shared.sessions.liveness(&pr.session_uid),
                        ),
                    );
                }
                last.insert(pr.pane_id.clone(), act);
            }
        }
        last.retain(|id, _| seen.contains(id));
    }
}

/// Apply one live `SessionEvent` to the read-model and fan out the matching `/events` frame.
/// `note_output` (byte cursor + last_output_at) is already done inside `SessionManager` BEFORE
/// any subscriber guard, so `since`/`waitForIdle` work with zero clients (the ordering invariant).
pub fn process_session_event(shared: &Arc<Shared>, ev: SessionEvent) {
    match ev {
        SessionEvent::Data { uid, data, cursor } => {
            if !shared.events.has_clients() {
                return;
            }
            let (pane_id, coords) = {
                let m = shared.model.lock().unwrap();
                let pid = m.uid_to_pane(&uid);
                let coords = pid.as_deref().and_then(|p| m.coords_of(p));
                (pid, coords)
            };
            shared.events.broadcast_for_pane(
                coords.as_ref(),
                &ControlEvent::Output {
                    session_uid: uid,
                    pane_id,
                    data,
                    cursor,
                },
            );
        }
        SessionEvent::Cwd { uid, cwd } => {
            shared.model.lock().unwrap().set_cwd(&uid, &cwd);
        }
        SessionEvent::CommandStart { uid } => {
            emit_command_frame(shared, &uid, "start", None);
            emit_marker_liveness(shared, &uid);
        }
        SessionEvent::CommandEnd { uid, code } => {
            emit_command_frame(shared, &uid, "end", code);
            emit_marker_liveness(shared, &uid);
        }
        SessionEvent::PromptReady { uid } => {
            emit_marker_liveness(shared, &uid);
        }
        SessionEvent::AgentState { uid, .. } => {
            emit_marker_liveness(shared, &uid);
        }
        SessionEvent::Exit { uid, code } => {
            // Always record the exit truthfully FIRST (mark_exited), then consult the
            // supervisor — a supervised crash is restarted from here; everything else is
            // byte-for-byte the legacy path (Decision::None for unsupervised panes).
            let marked = shared.model.lock().unwrap().mark_exited(&uid, code);
            match marked {
                Some((pane_id, coords)) => {
                    shared.events.broadcast_for_pane(
                        Some(&coords),
                        &ControlEvent::Exit {
                            session_uid: uid,
                            pane_id: Some(pane_id.clone()),
                            code,
                        },
                    );
                    shared.events.broadcast_for_pane(
                        Some(&coords),
                        &ControlEvent::Activity {
                            pane_id: pane_id.clone(),
                            activity: "exited".to_string(),
                        },
                    );
                    notify_state(shared);
                    // Phase-5 supervisor hook (no-op unless the pane opted in).
                    supervise_exit(shared, &pane_id, code);
                }
                None => {
                    if shared.events.has_clients() {
                        shared.events.broadcast_for_pane(
                            None,
                            &ControlEvent::Exit {
                                session_uid: uid,
                                pane_id: None,
                                code,
                            },
                        );
                    }
                }
            }
        }
    }
}

/// Phase-5: apply the supervisor's decision for one exit. Emits a `supervisor` frame for
/// every actionable outcome and, for a [`Decision::Restart`], schedules a delayed respawn
/// via [`Shared::spawn_task`] (no lock held across the delay — schedule and return).
fn supervise_exit(shared: &Arc<Shared>, pane_id: &str, code: i32) {
    let decision = shared.supervisor.lock().unwrap().on_exit(pane_id, code);
    match decision {
        Decision::None => {}
        Decision::Completed { code } => {
            broadcast_supervisor(shared, pane_id, "completed", None, None, None, Some(code));
        }
        Decision::Exhausted { attempt, max, code } => {
            broadcast_supervisor(
                shared,
                pane_id,
                "exhausted",
                Some(attempt),
                Some(max),
                None,
                Some(code),
            );
        }
        Decision::Restart {
            attempt,
            max,
            delay_ms,
            code,
        } => {
            broadcast_supervisor(
                shared,
                pane_id,
                "restarting",
                Some(attempt),
                Some(max),
                Some(delay_ms),
                Some(code),
            );
            let shared = Arc::clone(shared);
            let pane_id = pane_id.to_string();
            shared.clone().spawn_task(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                do_restart(&shared, &pane_id, attempt, max, code);
            });
        }
    }
}

/// Execute a scheduled restart: re-read the pane's spawn recipe, spawn a fresh session,
/// and swap the read-model's uid. Aborts if the pane vanished during the backoff window
/// (closed / manually restarted). Emits `restarted` on success or `crashed` on a respawn
/// error. STUB: this respawns from the read-model's recorded recipe (shell/args/command/
/// cwd) rather than a full spawn ledger, so it does not yet re-attach `env`/`integration`
/// (the same lossy set `restartPane` had pre-supervisor). A later pass adds the ledger.
fn do_restart(shared: &Arc<Shared>, pane_id: &str, attempt: u32, max: u32, code: i32) {
    // Snapshot the spawn recipe under the model lock; abort if the pane is gone.
    let recipe = {
        let m = shared.model.lock().unwrap();
        m.pane(pane_id).map(|p| {
            (
                p.shell.clone(),
                p.args.clone(),
                p.command.clone(),
                p.cwd.clone(),
            )
        })
    };
    let Some((shell, args, command, cwd)) = recipe else {
        return; // pane closed during backoff — drop the restart
    };
    let new_uid = uuid::Uuid::new_v4().to_string();
    let opts = crate::session_manager::SpawnOptions {
        uid: new_uid.clone(),
        shell,
        args,
        command,
        cwd,
        env: None,
        cols: None,
        rows: None,
        pane_id: Some(pane_id.to_string()),
        integration: None,
        control_file: Some(shared.control_file.to_string_lossy().to_string()),
    };
    match shared.sessions.create(opts) {
        Ok(()) => {
            shared.model.lock().unwrap().respawn_pane(pane_id, &new_uid);
            let used = shared.supervisor.lock().unwrap().record_restart(pane_id);
            broadcast_supervisor(
                shared,
                pane_id,
                "restarted",
                Some(used),
                Some(max),
                None,
                Some(code),
            );
            notify_state(shared);
        }
        Err(_) => {
            broadcast_supervisor(
                shared,
                pane_id,
                "crashed",
                Some(attempt),
                Some(max),
                None,
                Some(code),
            );
        }
    }
}

/// Fan out a `supervisor` frame (scope-filtered to the pane).
fn broadcast_supervisor(
    shared: &Arc<Shared>,
    pane_id: &str,
    state: &str,
    attempt: Option<u32>,
    max: Option<u32>,
    delay_ms: Option<u64>,
    code: Option<i32>,
) {
    if !shared.events.has_clients() {
        return;
    }
    let coords = shared.model.lock().unwrap().coords_of(pane_id);
    shared.events.broadcast_for_pane(
        coords.as_ref(),
        &ControlEvent::Supervisor {
            pane_id: pane_id.to_string(),
            state: state.to_string(),
            attempt,
            max,
            delay_ms,
            code,
        },
    );
}

/// Resolve a session uid to (pane_id, coords) under one model lock.
fn pane_and_coords(
    shared: &Arc<Shared>,
    uid: &str,
) -> Option<(String, crate::control::scope::PaneCoords)> {
    let m = shared.model.lock().unwrap();
    let pid = m.uid_to_pane(uid)?;
    let coords = m.coords_of(&pid)?;
    Some((pid, coords))
}

/// Emit a phase-4 `command` frame (scope-filtered) for a per-command edge.
fn emit_command_frame(shared: &Arc<Shared>, uid: &str, phase: &str, code: Option<i32>) {
    if !shared.events.has_clients() {
        return;
    }
    if let Some((pane_id, coords)) = pane_and_coords(shared, uid) {
        shared.events.broadcast_for_pane(
            Some(&coords),
            &ControlEvent::Command {
                pane_id,
                phase: phase.to_string(),
                code,
            },
        );
    }
}

/// Emit a phase-4 `liveness` frame (scope-filtered) computed from the live mirror — the
/// instant a marker arrives, before the ticker's next 500ms tick.
fn emit_marker_liveness(shared: &Arc<Shared>, uid: &str) {
    if !shared.events.has_clients() {
        return;
    }
    let (pane_id, coords, status) = {
        let m = shared.model.lock().unwrap();
        let Some(pid) = m.uid_to_pane(uid) else {
            return;
        };
        let Some(coords) = m.coords_of(&pid) else {
            return;
        };
        let status = m
            .pane(&pid)
            .map(|p| p.status)
            .unwrap_or(PaneStatus::Running);
        (pid, coords, status)
    };
    let act = shared.activity_for(uid, status);
    // Health signal for the supervisor: a prompt-ready / agent-done edge means the worker
    // is healthy, so reset its backoff budget (no-op if the pane isn't supervised).
    if matches!(act, Activity::AwaitingInput) {
        shared.supervisor.lock().unwrap().note_healthy(&pane_id);
    }
    shared.events.broadcast_for_pane(
        Some(&coords),
        &liveness_frame(&pane_id, act, shared.sessions.liveness(uid)),
    );
}

/// Drain the session-event channel forever, applying each event. (app.rs uses this when it has no
/// extra taps; otherwise it runs its own loop calling [`process_session_event`].)
pub async fn forward_session_events(shared: Arc<Shared>, mut rx: UnboundedReceiver<SessionEvent>) {
    while let Some(ev) = rx.recv().await {
        process_session_event(&shared, ev);
    }
}

/// Test/embedding helper: build a `Shared` with a fresh engine, install `master_token`, and serve
/// the router on an ephemeral loopback port in a background thread (with its own runtime). Returns
/// the shared state (so the caller can seed the read-model) + the bound port. Session events are
/// drained. Used by the std-only `tests/control_parity.rs` integration test and embedders that
/// want a control server without driving the full `app::run` loop.
pub fn serve_for_test(
    control_file: PathBuf,
    allow_input: bool,
    master_token: &str,
) -> io::Result<(Arc<Shared>, u16)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let sessions = Arc::new(SessionManager::new(tx));
    let shared = Shared::new(
        sessions,
        allow_input,
        env!("CARGO_PKG_VERSION"),
        control_file,
    );
    shared
        .tokens
        .lock()
        .unwrap()
        .set_master(master_token.to_string());

    let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = std_listener.local_addr()?.port();
    shared.port.store(port, Ordering::SeqCst);

    let serve_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("control-test-server".to_string())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                tokio::spawn(async move { while rx.recv().await.is_some() {} });
                if std_listener.set_nonblocking(true).is_err() {
                    return;
                }
                let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else {
                    return;
                };
                let _ = axum::serve(listener, routes::router(serve_shared)).await;
            });
        })?;

    Ok((shared, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::readmodel::{TabInfo, WindowInfo};
    use tokio::sync::mpsc::unbounded_channel;

    fn shared_with_pane() -> (Arc<Shared>, String) {
        let (tx, _rx) = unbounded_channel();
        let sm = Arc::new(SessionManager::new(tx));
        let shared = Shared::new(
            sm,
            true,
            "0.0.0-test",
            std::env::temp_dir().join("control-test.json"),
        );
        let pane = PaneInfo {
            id: "p1".into(),
            session_uid: "u1".into(),
            label: "shell".into(),
            subtitle: None,
            color: "#3b82f6".into(),
            command: None,
            args: None,
            cwd: None,
            shell: None,
            status: PaneStatus::Running,
            exit_code: None,
            meta: None,
        };
        shared.model.lock().unwrap().add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo {
                id: "t1".into(),
                title: "Tab".into(),
                layout: "auto".into(),
                panes: vec![pane],
            }],
        });
        (shared, "u1".to_string())
    }

    #[test]
    fn activity_is_busy_for_a_never_output_running_pane() {
        let (shared, _uid) = shared_with_pane();
        let pane = shared.model.lock().unwrap().pane("p1").unwrap().clone();
        // No session/output ⇒ busy (matches useIdle: markActivity only fires on output).
        assert_eq!(shared.compute_activity(&pane), Activity::Busy);
    }

    #[test]
    fn discovery_url_shape() {
        assert_eq!(
            events_url("127.0.0.1", 54321, "abc"),
            "ws://127.0.0.1:54321/events?token=abc"
        );
        assert_eq!(
            events_url("100.71.2.9", 51888, "abc"),
            "ws://100.71.2.9:51888/events?token=abc"
        );
    }

    #[test]
    fn advertised_host_rules() {
        let (shared, _uid) = shared_with_pane();
        // Default loopback bind advertises loopback.
        assert_eq!(shared.advertised_host(), "127.0.0.1");
        // A specific address advertises itself (local readers can reach the machine's
        // own LAN/Tailscale IP).
        shared.set_bind("100.71.2.9", 51888);
        assert_eq!(shared.advertised_host(), "100.71.2.9");
        // Unspecified listens on loopback too — keep the legacy local URL.
        shared.set_bind("0.0.0.0", 51888);
        assert_eq!(shared.advertised_host(), "127.0.0.1");
        shared.set_bind("::", 51888);
        assert_eq!(shared.advertised_host(), "127.0.0.1");
    }

    #[tokio::test]
    async fn exit_event_marks_pane_and_fans_out() {
        let (shared, uid) = shared_with_pane();
        let (_id, mut rx) = shared.events.add_client(None);
        process_session_event(
            &shared,
            SessionEvent::Exit {
                uid: uid.clone(),
                code: 7,
            },
        );
        // The pane is now exited in the model.
        assert_eq!(
            shared.model.lock().unwrap().pane("p1").unwrap().status,
            PaneStatus::Exited
        );
        // Two pane-addressed frames went out: exit then activity:exited.
        let f1 = rx.try_recv().unwrap();
        assert!(f1.contains(r#""type":"exit""#) && f1.contains(r#""code":7"#));
        let f2 = rx.try_recv().unwrap();
        assert!(f2.contains(r#""type":"activity""#) && f2.contains(r#""activity":"exited""#));
    }

    // Set a pane's meta then reconcile so the supervisor picks up the policy.
    fn supervise_p1(shared: &Arc<Shared>, pairs: &[(&str, &str)]) {
        let mut patch = std::collections::BTreeMap::new();
        for (k, v) in pairs {
            patch.insert((*k).to_string(), Some((*v).to_string()));
        }
        shared.model.lock().unwrap().set_meta("p1", &patch);
        shared.reconcile_policies();
    }

    #[test]
    fn reconcile_policies_picks_up_supervise_meta() {
        let (shared, _uid) = shared_with_pane();
        assert!(!shared.supervisor.lock().unwrap().is_supervised("p1"));
        supervise_p1(&shared, &[("hp.supervise", "on")]);
        assert!(shared.supervisor.lock().unwrap().is_supervised("p1"));
    }

    #[tokio::test]
    async fn unsupervised_exit_emits_no_supervisor_frame() {
        let (shared, uid) = shared_with_pane();
        let (_id, mut rx) = shared.events.add_client(None);
        process_session_event(&shared, SessionEvent::Exit { uid, code: 1 });
        // exit + activity:exited, but NO supervisor frame.
        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        assert!(frames.iter().any(|f| f.contains(r#""type":"exit""#)));
        assert!(!frames.iter().any(|f| f.contains(r#""type":"supervisor""#)));
    }

    #[tokio::test]
    async fn supervised_clean_exit_emits_completed_and_does_not_restart() {
        let (shared, uid) = shared_with_pane();
        supervise_p1(&shared, &[("hp.supervise", "on")]); // restartOn=failure default
        let (_id, mut rx) = shared.events.add_client(None);
        process_session_event(&shared, SessionEvent::Exit { uid, code: 0 });
        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        assert!(
            frames
                .iter()
                .any(|f| f.contains(r#""type":"supervisor""#)
                    && f.contains(r#""state":"completed""#)),
            "frames: {frames:?}"
        );
        // No restart scheduled → retry count stays 0 and the pane stays exited.
        assert_eq!(shared.supervisor.lock().unwrap().retries_used("p1"), 0);
        assert_eq!(
            shared.model.lock().unwrap().pane("p1").unwrap().status,
            PaneStatus::Exited
        );
    }

    #[tokio::test]
    async fn supervised_crash_emits_a_restarting_frame_and_schedules_a_respawn() {
        let (shared, uid) = shared_with_pane();
        // A long backoff (above the default 30s cap): the synchronous "restarting" frame
        // fires now; the actual respawn is deferred ~30s (capped), so this test asserts the
        // decision/frame without spawning a real pty.
        supervise_p1(
            &shared,
            &[("hp.supervise", "on"), ("hp.backoffMs", "60000")],
        );
        let (_id, mut rx) = shared.events.add_client(None);
        process_session_event(&shared, SessionEvent::Exit { uid, code: 1 });
        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        // A non-success exit on a supervised pane → a "restarting" frame with attempt/max/delay
        // (delay is capped to the default backoffCapMs = 30000).
        assert!(
            frames.iter().any(|f| {
                f.contains(r#""type":"supervisor""#)
                    && f.contains(r#""state":"restarting""#)
                    && f.contains(r#""attempt":1"#)
                    && f.contains(r#""delayMs":30000"#)
            }),
            "expected a restarting frame, got: {frames:?}"
        );
        // The respawn is still pending (long backoff) → retry count not yet bumped, pane
        // still recorded as exited until the delayed task fires.
        assert_eq!(shared.supervisor.lock().unwrap().retries_used("p1"), 0);
        assert_eq!(
            shared.model.lock().unwrap().pane("p1").unwrap().status,
            PaneStatus::Exited
        );
    }
}
