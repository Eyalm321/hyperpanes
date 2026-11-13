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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::runtime::Handle;

use serde::Serialize;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::control::endpoints::{plan, EndpointStatus, Planned};
use crate::control::events::{ControlEvent, EventHub};
use crate::control::inbox::MessageInbox;
use crate::control::lock::PaneLocks;
use crate::control::nudge::NudgeLedger;
use crate::control::output::{blocks_typed_input, detect_awaiting_input};
use crate::control::readmodel::{Activity, PaneInfo, PaneRef, PaneStatus, ReadModel};
use crate::control::routes;
use crate::control::speech_service::SpeechService;
use crate::control::supervisor::{Decision, Supervisor};
use crate::control::tokens::{random_token, TokenStore};
use crate::control::work::WorkQueue;
use crate::session::activity::ActivitySignal;
use crate::session_manager::{SessionEvent, SessionManager};

/// The renderer idle threshold (`useSettings.idleAlertSeconds`, default 10s): a running pane with
/// no pty output for at least this long reads as `idle`, else `busy`.
pub const IDLE_THRESHOLD_MS: i64 = 10_000;

/// How often the activity ticker re-evaluates liveness and fans out busy⇄idle flips.
const ACTIVITY_TICK_MS: u64 = 500;

/// How often the work-queue reaper sweeps for expired leases (recovering dead/wedged workers).
/// Coarser than the activity tick — leases are seconds-to-minutes, so a 5s sweep is ample.
const REAPER_TICK_MS: u64 = 5_000;

/// Coalescing window for structure-only `state` pings (TS `notifyState`).
const STATE_COALESCE_MS: u64 = 100;

/// Everything the running control server owns, behind cheap component locks. Cloned-shared via
/// `Arc` into every axum handler + the background tasks.
pub struct Shared {
    pub model: Mutex<ReadModel>,
    pub tokens: Mutex<TokenStore>,
    pub inbox: Mutex<MessageInbox>,
    /// Inbox-nudge bookkeeping (`control::nudge`): which agent panes have unread mail and when
    /// they were last woken. The inbox itself stays pull-only; this is the opt-in wake-up.
    pub nudges: Mutex<NudgeLedger>,
    pub locks: Mutex<PaneLocks>,
    /// Durable, claimable work queue backing the `/queues` + `/tasks` routes
    /// (worker-pool phase-2). `rusqlite::Connection` is `Send` but not `Sync`, so —
    /// exactly like every other component — it lives behind a `Mutex` that serializes
    /// queue ops. Opened in-memory here (test-safe, zero-config); a durable embedder
    /// swaps in `WorkQueue::open(paths::work_db())` + boot recovery (work.rs §3.3).
    pub work: Mutex<WorkQueue>,
    /// Which queue `work` is: a [`WorkQueueMode`] as `u8`, set by
    /// [`attach_durable_work_queue`](Self::attach_durable_work_queue), reported on `/health`.
    work_mode: AtomicU8,
    /// Which inbox this is: an [`InboxMode`] as `u8`, set by
    /// [`attach_durable_inbox_at`](Self::attach_durable_inbox_at), reported on `/health`.
    inbox_mode: AtomicU8,
    pub events: EventHub,
    /// Phase-5 auto-restart supervisor: per-pane policy + retry ledger. Default-empty ⇒
    /// every `Exit` runs the legacy path until a pane opts in via `hp.supervise` meta.
    pub supervisor: Mutex<Supervisor>,
    pub sessions: Arc<SessionManager>,
    pub allow_input: AtomicBool,
    pub pid: u32,
    pub version: String,
    /// Optional free-form build label (already sanitized). `None` in the public app.
    build: Mutex<Option<String>>,
    pub port: AtomicU16,
    pub idle_threshold_ms: i64,
    pub control_file: PathBuf,
    state_scheduled: AtomicBool,
    /// Set by the `/projects` routes (and a project-opening `newPane`) when they mutate the
    /// `projects.json` registry off the UI thread. The GUI host clears it each tick (see
    /// `ControlHost::sync`) to reload the sidebar rail live; in the headless bin it is simply
    /// never read. A plain flag, not coalesced — the UI tick polls it cheaply.
    projects_dirty: AtomicBool,
    /// Set by [`flush_queue_changes`](Self::flush_queue_changes) when task states changed;
    /// the GUI host clears it each tick to refresh the sidebar queue badge live.
    queue_dirty: AtomicBool,
    /// App-restart request from the `restartApp` command: 0 = none, 1 = gui (relaunch the GUI,
    /// panes survive via daemon re-attach), 2 = full (also shut the session daemon down —
    /// every pane dies and the restore path resurrects the workspace + Claude conversations
    /// from the snapshot). Set by dispatch off the UI thread; the GUI host polls + clears it
    /// each tick and performs the teardown (snapshot flush, detached relauncher, quit) on the
    /// UI thread. In the headless bin it is simply never read.
    pub restart_app: AtomicU8,
    /// Listen config: the configured remote endpoint specs (interface names / IPs, #131) and
    /// the requested port. Loopback is ALWAYS bound in addition; no specs = loopback only (the
    /// legacy default). An embedder sets this from `control_settings` BEFORE `run_server`.
    /// Port `0` = OS-assigned (remote sockets then reuse loopback's port).
    listen: Mutex<(Vec<String>, u16)>,
    /// Per-remote-endpoint bind status (see [`crate::control::endpoints`]), maintained by the
    /// server's rebind watcher. Empty when no remote endpoint is configured.
    endpoint_status: Mutex<Vec<EndpointStatus>>,
    /// The runtime to spawn background tasks on (the coalescer in [`notify_state`]). Set once by
    /// an embedder that wants spawns bound to an explicit runtime rather than the ambient
    /// thread-local — the GUI host sets this so a `notify_state` from the UI thread can never
    /// panic if the runtime guard is ever absent. Unset ⇒ fall back to the ambient `tokio::spawn`.
    runtime: OnceLock<Handle>,
    /// Per-pane "talk" (local TTS): persisted settings + lazily-spawned engine + transcript
    /// tails. Default-off; costs nothing until a pane's talk is switched on.
    pub speech: SpeechService,
    /// `connectRemote` requests queued off the UI thread; the GUI host drains them each tick
    /// ([`Self::take_remote_connects`]). Unread in the headless bin.
    remote_connects: Mutex<Vec<crate::remote::ConnectRequest>>,
    /// `groupPanes`/`ungroupPanes` requests queued for the GUI host ([`Self::take_pane_group_requests`]).
    pane_group_requests: Mutex<Vec<PaneGroupRequest>>,
    /// The GUI's latest `remoteScreen` snapshot (array of attached remote panes), republished
    /// by the GUI host; served verbatim by the `remoteScreen` command.
    remote_screens: Mutex<serde_json::Value>,
}

/// A pane-group request queued by `groupPanes` / `ungroupPanes` for the GUI host to execute
/// (the read model cannot move panes across tabs; only the GUI can). Drained each tick via
/// [`Shared::take_pane_group_requests`]. A headless server never drains it, so the requests
/// stay queued and unapplied.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneGroupRequest {
    /// Move `pane_ids` (all local, all in `window_id`) into a new tab `tab_id` after the current
    /// one, marked as a pane group. `project` is a resolved project id.
    Group {
        tab_id: String,
        window_id: i64,
        pane_ids: Vec<String>,
        title: Option<String>,
        project: Option<String>,
    },
    /// Turn the group tab `tab_id` back into ordinary tab(s).
    Ungroup { tab_id: String },
    /// `movePane`: re-host live pane `pane_id` (from any tab of any window) into `to`, in window
    /// `window_id`. The PTY, session, meta and inbox are untouched; only the tab changes.
    Move {
        pane_id: String,
        window_id: i64,
        to: MoveTarget,
    },
}

/// Where a [`PaneGroupRequest::Move`] puts its pane.
#[derive(Debug, Clone, PartialEq)]
pub enum MoveTarget {
    /// An existing tab.
    Tab { tab_id: String },
    /// A brand-new tab whose control id is `tab_id` (the id the API already returned).
    NewTab {
        tab_id: String,
        title: Option<String>,
        color: Option<String>,
    },
    /// The window's pane-group tab for project `project_id`, reused when it exists, else
    /// created as `tab_id` with the project's `title` (name) and `color`.
    Project {
        tab_id: String,
        project_id: String,
        title: String,
        color: Option<String>,
    },
}

/// Which work queue an instance runs — `/health`'s stable `workQueue` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WorkQueueMode {
    /// The ephemeral in-memory queue: never attached (tests) or the DB would not open.
    Memory = 0,
    /// The on-disk `work.db`, owned by this instance.
    Durable = 1,
    /// Another live instance owns `work.db`; this one stays in-memory and never recovers
    /// or reaps the DB.
    Refused = 2,
}

impl WorkQueueMode {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkQueueMode::Memory => "memory",
            WorkQueueMode::Durable => "durable",
            WorkQueueMode::Refused => "refused",
        }
    }
}

/// Which message inbox an instance runs — `/health`'s additive `inbox` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InboxMode {
    /// The in-memory inbox (never attached, or the store would not open).
    Memory = 0,
    /// The on-disk `messages.db`, owned by this instance.
    Durable = 1,
    /// Another live instance owns the data dir; this one stays in-memory.
    Refused = 2,
}

impl InboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            InboxMode::Memory => "memory",
            InboxMode::Durable => "durable",
            InboxMode::Refused => "refused",
        }
    }
}

/// What [`Shared::attach_durable_work_queue`] did.
#[derive(Debug)]
pub enum WorkAttach {
    /// Attached the on-disk DB as its single owner; boot recovery requeued (or dead-lettered)
    /// `recovered` tasks whose holder was dead and kept `kept` claims whose holder process
    /// is still alive (a worker that survived a GUI relaunch in the session daemon).
    Durable { recovered: usize, kept: usize },
    /// Another live process owns the DB. Nothing was opened or recovered; the queue stays
    /// in-memory for this instance's lifetime.
    Refused(Box<crate::control::work_lock::Contended>),
    /// We own it but SQLite would not open it; the queue stays in-memory.
    OpenFailed(String),
}

/// Sanitize a build label for the unauthenticated surfaces: keep only `[A-Za-z0-9 ._,+()-]`,
/// cap at 64 chars, and reject (→ `None`) anything containing `/ \\ @ :` so no path, host or
/// user can leak. Empty after trimming → `None`.
pub fn sanitize_build_label(s: &str) -> Option<String> {
    if s.contains(['/', '\\', '@', ':']) {
        return None;
    }
    let out: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " ._,+()-".contains(*c))
        .take(64)
        .collect();
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

impl Shared {
    /// Build the shared state. `allow_input` mirrors control-settings; `control_file` is both the
    /// path written on start and the `HYPERPANES_CONTROL_FILE` injected into spawned panes.
    /// `speech_settings_path` is `speech.json`'s path — a real embedder passes
    /// [`paths::speech_json`](crate::persistence::paths::speech_json); tests pass a scratch
    /// path so `cargo test` never touches the developer's real settings file.
    pub fn new(
        sessions: Arc<SessionManager>,
        allow_input: bool,
        version: impl Into<String>,
        control_file: PathBuf,
        speech_settings_path: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Shared {
            model: Mutex::new(ReadModel::new()),
            tokens: Mutex::new(TokenStore::new()),
            inbox: Mutex::new(MessageInbox::new()),
            nudges: Mutex::new(NudgeLedger::new()),
            locks: Mutex::new(PaneLocks::new()),
            work: Mutex::new(WorkQueue::open_in_memory().expect("open in-memory work queue")),
            work_mode: AtomicU8::new(WorkQueueMode::Memory as u8),
            inbox_mode: AtomicU8::new(InboxMode::Memory as u8),
            events: EventHub::new(),
            supervisor: Mutex::new(Supervisor::new()),
            sessions,
            allow_input: AtomicBool::new(allow_input),
            pid: std::process::id(),
            version: version.into(),
            build: Mutex::new(None),
            port: AtomicU16::new(0),
            idle_threshold_ms: IDLE_THRESHOLD_MS,
            control_file,
            state_scheduled: AtomicBool::new(false),
            projects_dirty: AtomicBool::new(false),
            queue_dirty: AtomicBool::new(false),
            restart_app: AtomicU8::new(0),
            listen: Mutex::new((Vec::new(), 0)),
            endpoint_status: Mutex::new(Vec::new()),
            runtime: OnceLock::new(),
            speech: SpeechService::new(speech_settings_path),
            remote_connects: Mutex::new(Vec::new()),
            pane_group_requests: Mutex::new(Vec::new()),
            remote_screens: Mutex::new(serde_json::Value::Array(Vec::new())),
        })
    }

    /// Set the build label reported by `/health` and `/state`. Sanitized via
    /// [`sanitize_build_label`]; an empty/unsafe label clears it.
    pub fn set_build(&self, label: Option<&str>) {
        *self.build.lock().unwrap() = label.and_then(sanitize_build_label);
    }

    /// The current (sanitized) build label, if any.
    pub fn build(&self) -> Option<String> {
        self.build.lock().unwrap().clone()
    }

    /// Queue a `connectRemote` request for the GUI host.
    pub fn queue_remote_connect(&self, req: crate::remote::ConnectRequest) {
        self.remote_connects.lock().unwrap().push(req);
    }

    /// Drain queued `connectRemote` requests (GUI host, once per tick).
    pub fn take_remote_connects(&self) -> Vec<crate::remote::ConnectRequest> {
        std::mem::take(&mut *self.remote_connects.lock().unwrap())
    }

    /// Queue a pane-group request for the GUI host.
    pub fn queue_pane_group_request(&self, req: PaneGroupRequest) {
        self.pane_group_requests.lock().unwrap().push(req);
    }

    /// Drain queued pane-group requests (GUI host, once per tick).
    pub fn take_pane_group_requests(&self) -> Vec<PaneGroupRequest> {
        std::mem::take(&mut *self.pane_group_requests.lock().unwrap())
    }

    /// Publish the GUI's current attached-remote-pane snapshot (see `remoteScreen`).
    pub fn set_remote_screens(&self, v: serde_json::Value) {
        *self.remote_screens.lock().unwrap() = v;
    }

    /// The last published `remoteScreen` snapshot.
    pub fn remote_screens(&self) -> serde_json::Value {
        self.remote_screens.lock().unwrap().clone()
    }

    /// Set the listen config (from `control_settings`) BEFORE `run_server`: the remote endpoint
    /// specs (interface names or IPs; empty = loopback only) and the port (0 = ephemeral).
    pub fn set_listen(&self, remote_endpoints: Vec<String>, port: u16) {
        *self.listen.lock().unwrap() = (remote_endpoints, port);
    }

    /// The requested `(remote endpoint specs, port)`.
    pub fn listen_config(&self) -> (Vec<String>, u16) {
        self.listen.lock().unwrap().clone()
    }

    /// Whether any remote endpoint is configured (bound or not).
    pub fn has_remote_endpoints(&self) -> bool {
        !self.listen.lock().unwrap().0.is_empty()
    }

    /// The current per-remote-endpoint bind status (empty when none is configured).
    pub fn endpoint_status(&self) -> Vec<EndpointStatus> {
        self.endpoint_status.lock().unwrap().clone()
    }

    /// The host to advertise in `control.json` / `POST /tokens` events URLs. Loopback is always
    /// served, and both are LOCAL discovery surfaces, so this is always `127.0.0.1` (remote
    /// clients build their URLs from the pairing info).
    pub fn advertised_host(&self) -> String {
        "127.0.0.1".to_string()
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

    /// Atomically read-and-clear the queue-changed flag (GUI host, once per UI tick).
    pub fn take_queue_dirty(&self) -> bool {
        self.queue_dirty.swap(false, Ordering::SeqCst)
    }

    /// Drain the work queue's changed-queue set; if non-empty, raise the GUI dirty flag and
    /// broadcast ONE scope-filtered `queue` event naming every changed queue. Called on a
    /// [`QUEUE_COALESCE_MS`] tick, so any burst of mutations yields at most one event per tick.
    /// Returns the queues that were flushed.
    pub fn flush_queue_changes(&self) -> Vec<String> {
        let changed = self.work.lock().unwrap().take_changed();
        if !changed.is_empty() {
            self.queue_dirty.store(true, Ordering::SeqCst);
            self.events.broadcast_queues(&changed);
        }
        changed
    }

    /// Bind background-task spawns (the `notify_state` coalescer) to an explicit runtime handle.
    /// Idempotent — only the first set takes effect.
    pub fn set_runtime(&self, handle: Handle) {
        let _ = self.runtime.set(handle);
    }

    /// Swap the (default in-memory) work queue for the durable on-disk DB at
    /// [`paths::work_db`](crate::persistence::paths::work_db) and run boot recovery — but only
    /// as that DB's single owner (see [`work_lock`](crate::control::work_lock)). A real
    /// embedder (GUI control host / headless daemon) calls this once after [`Shared::new`];
    /// tests never do, so they keep the ephemeral in-memory queue. Never fatal: a refused or
    /// failed attach logs loudly and keeps the in-memory queue rather than take down the
    /// control plane over a queue that many sessions never touch.
    pub fn attach_durable_work_queue(&self) -> WorkAttach {
        self.attach_durable_work_queue_at(
            &crate::persistence::paths::work_db(),
            crate::control::work_lock::ACQUIRE_WAIT,
        )
    }

    /// [`attach_durable_work_queue`](Self::attach_durable_work_queue) against an explicit DB
    /// path, waiting up to `wait` for a live owner to let go (restart overlap).
    ///
    /// The single-owner lock comes FIRST: boot recovery requeues every claim it cannot prove
    /// is held by a live process, and claims made through another live instance (in-process
    /// or holder-less) look dead to it. A second opener (a half-isolated dev instance, a
    /// headless server beside the GUI) running it would requeue a live instance's in-flight
    /// tasks. So a non-owner never opens the DB at all: no recovery, no writes, and the reaper
    /// ticker only ever sweeps its in-memory queue.
    pub fn attach_durable_work_queue_at(
        &self,
        path: &std::path::Path,
        wait: Duration,
    ) -> WorkAttach {
        use crate::control::work_lock::{self, Acquired, OwnerRecord};
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let me = OwnerRecord::current(&self.version, &self.control_file, now_ms());
        match work_lock::acquire(path, &me, wait) {
            Err(contended) => {
                eprintln!("[work] {}", contended.message());
                self.work_mode
                    .store(WorkQueueMode::Refused as u8, Ordering::SeqCst);
                return WorkAttach::Refused(contended);
            }
            Ok(Acquired::Unavailable(e)) => {
                // Fail open: losing the primary's durable queue on a lock-less filesystem
                // would be worse; the startup isolation check still guards dev instances.
                eprintln!(
                    "[work] could not take the single-owner lock {} ({e}); attaching {} \
                     WITHOUT single-owner protection",
                    work_lock::lock_path_for(path).display(),
                    path.display()
                );
            }
            Ok(Acquired::Owner) | Ok(Acquired::AlreadyOwner) => {}
        }
        match crate::control::work::WorkQueue::open(path) {
            Ok(mut wq) => {
                // We own the DB. Claims whose holder died with the previous owner (or never
                // named one) are requeued (exhausted ones dead-letter); claims whose holder
                // process is still alive, i.e. workers that survived a GUI relaunch in the
                // session daemon, stay claimed with their fence and get a re-armed lease.
                let rec = wq.recover_in_flight_with(now_ms(), &crate::control::holder::is_alive);
                if !rec.reaped.is_empty() || !rec.kept.is_empty() {
                    eprintln!(
                        "[work] durable queue opened ({}); recovered {} in-flight task(s), \
                         kept {} claim(s) held by live workers",
                        path.display(),
                        rec.reaped.len(),
                        rec.kept.len()
                    );
                }
                *self.work.lock().unwrap() = wq;
                self.work_mode
                    .store(WorkQueueMode::Durable as u8, Ordering::SeqCst);
                WorkAttach::Durable {
                    recovered: rec.reaped.len(),
                    kept: rec.kept.len(),
                }
            }
            Err(e) => {
                eprintln!(
                    "[work] could not open durable queue at {} ({e}); staying in-memory",
                    path.display()
                );
                WorkAttach::OpenFailed(e.to_string())
            }
        }
    }

    /// Back the pane inbox with the durable `messages.db` at
    /// [`paths::messages_db`](crate::persistence::paths::messages_db). Call right after
    /// [`attach_durable_work_queue`](Self::attach_durable_work_queue) with its result: the
    /// inbox is only attached by the instance that owns `work.db`.
    pub fn attach_durable_inbox(&self, work: &WorkAttach) -> InboxMode {
        self.attach_durable_inbox_at(&crate::persistence::paths::messages_db(), work)
    }

    /// [`attach_durable_inbox`](Self::attach_durable_inbox) against an explicit path. Only a
    /// [`WorkAttach::Durable`] owner attaches; a refused instance reports `refused` and an
    /// owner whose store fails to open stays in-memory. Never fatal.
    pub fn attach_durable_inbox_at(&self, path: &std::path::Path, work: &WorkAttach) -> InboxMode {
        let mode = match work {
            WorkAttach::Refused(_) => InboxMode::Refused,
            WorkAttach::OpenFailed(_) => InboxMode::Memory,
            WorkAttach::Durable { .. } => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match crate::control::inbox::MessageInbox::open(path, now_ms()) {
                    Ok(inbox) => {
                        let aliases = inbox.aliases().clone();
                        *self.inbox.lock().unwrap() = inbox;
                        self.model.lock().unwrap().set_aliases(aliases);
                        InboxMode::Durable
                    }
                    Err(e) => {
                        eprintln!(
                            "[inbox] could not open durable inbox at {} ({e}); staying in-memory",
                            path.display()
                        );
                        InboxMode::Memory
                    }
                }
            }
        };
        self.inbox_mode.store(mode as u8, Ordering::SeqCst);
        mode
    }

    /// Which inbox this instance runs (reported on `/health` as `inbox`).
    pub fn inbox_mode(&self) -> InboxMode {
        match self.inbox_mode.load(Ordering::SeqCst) {
            x if x == InboxMode::Durable as u8 => InboxMode::Durable,
            x if x == InboxMode::Refused as u8 => InboxMode::Refused,
            _ => InboxMode::Memory,
        }
    }

    /// Record that pane `old` is now `new` (a restored pane re-spawned under a new id): the
    /// stored mail is re-keyed, and every pane route accepts `old` and lands on the live pane.
    pub fn register_pane_alias(&self, old: &str, new: &str) -> bool {
        let aliases = {
            let mut inbox = self.inbox.lock().unwrap();
            if !inbox.register_alias(old, new, now_ms()) {
                return false;
            }
            inbox.aliases().clone()
        };
        self.model.lock().unwrap().set_aliases(aliases);
        true
    }

    /// Which queue this instance runs (reported on `/health` as `workQueue`).
    pub fn work_queue_mode(&self) -> WorkQueueMode {
        match self.work_mode.load(Ordering::SeqCst) {
            x if x == WorkQueueMode::Durable as u8 => WorkQueueMode::Durable,
            x if x == WorkQueueMode::Refused as u8 => WorkQueueMode::Refused,
            _ => WorkQueueMode::Memory,
        }
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
    /// `status==='exited' ? 'exited' : idle ? 'idle' : 'busy'` with idle = "no genuine output
    /// for the threshold" (a blink is not output, see `session::activity`); a never-output pane
    /// reads `busy` (the renderer's `markActivity` only fires on output).
    pub fn compute_activity(&self, pane: &PaneInfo) -> Activity {
        activity_for(
            &self.sessions,
            self.idle_threshold_ms,
            &pane.session_uid,
            pane.status,
        )
    }
}

/// Free-function core of [`Shared::compute_activity`], factored out so `dispatch::recoverPane`
/// (which has a `&SessionManager` but no `Shared`) reads liveness through the exact same
/// source of truth as `/state`'s `activity` field, rather than a second, driftable copy.
pub fn activity_for(
    sessions: &SessionManager,
    idle_threshold_ms: i64,
    uid: &str,
    status: PaneStatus,
) -> Activity {
    {
        if status == PaneStatus::Exited {
            return Activity::Exited;
        }
        // Phase 4: prefer the precise, marker-derived state. The gate is `marker_seen` —
        // until a pane has EVER emitted a prompt marker, the legacy silence heuristic owns
        // its activity, so an un-instrumented pane is byte-for-byte unchanged.
        if let Some(l) = sessions.liveness(uid) {
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
        // Fallback: the 10s output-silence heuristic over GENUINE output only. Cosmetic redraws
        // (a blinking indicator, an app-drawn cursor toggling the same cells) never stamp
        // `content_at` (see `session::activity`), so a pane that only blinks goes idle.
        let Some(sig) = sessions.activity_signal(uid) else {
            // Unknown session: nothing has been seen from it, which reads busy (as before).
            return Activity::Busy;
        };
        // A screen classified as blocked on a decision (a permission dialog, a y/n prompt) is
        // awaiting input even inside the busy window. The verdict is cached per genuine-output
        // cursor, so it holds until the screen really changes.
        if sig.awaiting_now() {
            return Activity::AwaitingInput;
        }
        match sig.content_at {
            Some(t) if now_ms() - (t as i64) >= idle_threshold_ms => Activity::Idle,
            _ => Activity::Busy,
        }
    }
}

/// How long a pane's genuine output must have been quiet (while still inside the busy window)
/// before the activity ticker classifies its screen for an awaiting-input prompt.
pub const CLASSIFY_SETTLE_MS: i64 = 750;

/// Whether the activity ticker should classify this pane's screen now: its genuine output went
/// quiet at least [`CLASSIFY_SETTLE_MS`] ago, it would still read busy, and the cached verdict
/// is missing or older than the current content.
pub fn needs_classification(sig: &ActivitySignal, now: i64, idle_threshold_ms: i64) -> bool {
    let Some(t) = sig.content_at else {
        return false;
    };
    let quiet = now - t as i64;
    (CLASSIFY_SETTLE_MS..idle_threshold_ms).contains(&quiet) && sig.classification_stale()
}

/// Classify the pane's current screen with [`detect_awaiting_input`] (the same detector a
/// `mode:"screen"` read reports as `awaitingInput`) and cache the verdict under the content
/// cursor read BEFORE rendering: output that lands mid-render leaves the cache stale (so the
/// next tick re-classifies) instead of mislabelling newer content. Blocking (the daemon backend
/// round-trips), so callers run it off the async runtime. `None` for an unknown or gone pane.
pub fn classify_screen(sessions: &SessionManager, uid: &str) -> Option<bool> {
    let cursor = sessions.activity_signal(uid)?.content_cursor;
    let text = sessions.render_screen(uid)?;
    let awaiting = detect_awaiting_input(&text);
    sessions.set_awaiting(uid, cursor, awaiting);
    Some(awaiting)
}

/// Whether text + Enter typed by the app into this pane would answer a decision prompt instead
/// of reaching the agent's input box ([`blocks_typed_input`] over a fresh render). Blocking (the
/// daemon backend round-trips). The inbox nudge checks it right before typing. A live pane whose
/// screen cannot be read counts as blocked (never type blind); a gone pane does not (nothing
/// typed reaches it). The render also refreshes the cached awaiting-input verdict.
pub fn screen_blocks_typing(sessions: &SessionManager, uid: &str) -> bool {
    let cursor = sessions.activity_signal(uid).map(|s| s.content_cursor);
    let Some(text) = sessions.render_screen(uid) else {
        return sessions.has(uid);
    };
    if let Some(c) = cursor {
        sessions.set_awaiting(uid, c, detect_awaiting_input(&text));
    }
    blocks_typed_input(&text)
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
/// `remoteEndpoints` is additive (#131: the per-endpoint bind status, never a token) and OMITTED
/// when no remote endpoint is configured, keeping the legacy shape byte-identical. This server
/// never writes the legacy `bindAddress` field: its presence marks an OLDER server that listens
/// only there (local dialers then dial it instead of loopback).
#[derive(Serialize)]
struct Discovery<'a> {
    port: u16,
    token: &'a str,
    pid: u32,
    version: &'a str,
    events: String,
    #[serde(rename = "remoteEndpoints", skip_serializing_if = "Vec::is_empty")]
    remote_endpoints: Vec<EndpointStatus>,
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
    let host = shared.advertised_host();
    let discovery = Discovery {
        port,
        token: &token,
        pid: shared.pid,
        version: &shared.version,
        events: events_url(&host, port, &token),
        remote_endpoints: shared.endpoint_status(),
    };
    let json = serde_json::to_string_pretty(&discovery)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    // Owner-only: this file carries the MASTER token. Created 0600 before the bytes
    // land, so no other local user ever gets a window to read it.
    crate::persistence::paths::write_atomic_private(&shared.control_file, json.as_bytes())
}

/// Remove the discovery file (best-effort), so a stale `control.json` never points at a dead port.
/// Owner-checked: only the instance whose pid the file records may delete it — a refused or
/// hijacked instance stopping must not take the live owner's file down with it.
pub fn remove_discovery(shared: &Arc<Shared>) {
    if crate::control::discovery_guard::recorded_pid(&shared.control_file) == Some(shared.pid) {
        let _ = std::fs::remove_file(&shared.control_file);
    }
}

/// Bind loopback (always), mint the master token, bind the configured remote endpoints (#131),
/// write `control.json`, and serve until the process exits. Never returns under normal
/// operation. Remote sockets live in a [`JoinSet`](tokio::task::JoinSet) owned by this future,
/// so aborting the server task (the GUI host does on stop) tears every listener down. The
/// activity ticker is a SEPARATE task ([`run_activity_ticker`]) the embedder spawns alongside.
pub async fn run_server(shared: Arc<Shared>) -> io::Result<()> {
    // A control file owned by a live, different-pid instance is not ours to claim — fail
    // loudly before binding anything (see control::discovery_guard).
    crate::control::discovery_guard::ensure_claimable(&shared.control_file, shared.pid).await?;
    let (specs, req_port) = shared.listen_config();
    // Loopback is ALWAYS served. A taken fixed port is an error for loopback too (no silent
    // move to another port: local clients and the pairing URL would disagree).
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", req_port)).await?;
    let port = listener.local_addr()?.port();
    shared.port.store(port, Ordering::SeqCst);

    shared
        .tokens
        .lock()
        .unwrap()
        .set_master(master_token(&shared));
    // Reload persisted device tokens (paired phones) so pairing survives a restart, just like the
    // master token does when remote endpoints are configured. Kept beside the active control.json.
    {
        let path = shared.control_file.with_file_name("device-tokens.json");
        let mut tokens = shared.tokens.lock().unwrap();
        for rec in crate::persistence::device_tokens::load_from(&path) {
            tokens.add_device(rec.token, rec.label, rec.expires_at);
        }
    }

    let app = routes::router(Arc::clone(&shared));
    let mut remotes = RemoteListeners::new(app.clone(), port);
    if !specs.is_empty() {
        let table = crate::control::endpoints::system_ifaces();
        remotes.reconcile(&shared, plan(&specs, &table)).await;
    }
    write_discovery(&shared)?;

    let watcher = async {
        if specs.is_empty() {
            // Nothing remote configured: no watcher at all.
            std::future::pending::<()>().await;
        }
        let mut tick = tokio::time::interval(REBIND_EVERY);
        tick.tick().await; // the first tick is immediate; the initial bind already ran
        loop {
            tick.tick().await;
            let table = crate::control::endpoints::system_ifaces();
            if remotes.reconcile(&shared, plan(&specs, &table)).await {
                let _ = write_discovery(&shared);
            }
        }
    };
    tokio::select! {
        r = axum::serve(listener, app) => r,
        _ = watcher => Ok(()),
    }
}

/// How often the rebind watcher re-resolves the configured remote endpoints.
const REBIND_EVERY: Duration = Duration::from_secs(10);

/// The bound remote sockets, keyed by address, each served by its own task in `tasks`.
struct RemoteListeners {
    app: axum::Router,
    port: u16,
    tasks: tokio::task::JoinSet<()>,
    bound: HashMap<std::net::IpAddr, tokio::task::AbortHandle>,
}

impl RemoteListeners {
    fn new(app: axum::Router, port: u16) -> Self {
        Self {
            app,
            port,
            tasks: tokio::task::JoinSet::new(),
            bound: HashMap::new(),
        }
    }

    /// Bring the bound set in line with `planned` (from [`plan`]): drop listeners whose address
    /// is gone, bind newly-resolvable addresses, record a status per endpoint. Returns whether
    /// the published status changed (and logs each change loudly — failures included, once per
    /// change, never on every tick).
    async fn reconcile(&mut self, shared: &Arc<Shared>, planned: Vec<Planned>) -> bool {
        // Reap listener tasks that ended on their own (accept error); they rebind below.
        while self.tasks.try_join_next().is_some() {}
        self.bound.retain(|_, h| !h.is_finished());

        let wanted: Vec<std::net::IpAddr> = planned
            .iter()
            .filter_map(|p| match p {
                Planned::Addr { ip, .. } => Some(*ip),
                Planned::Failed { .. } => None,
            })
            .collect();
        let stale: Vec<std::net::IpAddr> = self
            .bound
            .keys()
            .filter(|ip| !wanted.contains(ip))
            .copied()
            .collect();
        for ip in stale {
            if let Some(h) = self.bound.remove(&ip) {
                h.abort();
            }
        }

        let mut status = Vec::with_capacity(planned.len());
        for p in planned {
            match p {
                Planned::Failed { spec, error } => status.push(EndpointStatus {
                    spec,
                    address: None,
                    bound: false,
                    error: Some(error),
                }),
                Planned::Addr { spec, ip } => {
                    let mut error = None;
                    if !self.bound.contains_key(&ip) {
                        match tokio::net::TcpListener::bind((ip, self.port)).await {
                            Ok(l) => {
                                let app = self.app.clone();
                                let h = self.tasks.spawn(async move {
                                    if let Err(e) = axum::serve(l, app).await {
                                        eprintln!("[control] remote listener {ip} ended: {e}");
                                    }
                                });
                                self.bound.insert(ip, h);
                            }
                            Err(e) => error = Some(e.to_string()),
                        }
                    }
                    status.push(EndpointStatus {
                        spec,
                        address: Some(ip.to_string()),
                        bound: error.is_none(),
                        error,
                    });
                }
            }
        }

        let mut cur = shared.endpoint_status.lock().unwrap();
        if *cur == status {
            return false;
        }
        for st in &status {
            if !cur.contains(st) {
                if st.bound {
                    eprintln!("[control] remote endpoint up: {}", st.summary(self.port));
                } else {
                    eprintln!(
                        "[control] WARNING remote endpoint NOT served (loopback only): {}",
                        st.summary(self.port)
                    );
                }
            }
        }
        *cur = status;
        true
    }
}

/// The master token for this server run. Loopback (the frozen default) mints a fresh
/// one per start, exactly like the TS source. A REMOTE bind persists it in a 0600
/// `control-token` file next to control.json instead — a paired phone must survive
/// host restarts, or every reboot strands it until the user is back at the desk to
/// re-scan a QR.
fn master_token(shared: &Arc<Shared>) -> String {
    if !shared.has_remote_endpoints() {
        return random_token();
    }
    let path = shared.control_file.with_file_name("control-token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        // Same sanity shape as minted tokens (64 hex chars) — anything else re-mints.
        if existing.len() == 64 && existing.chars().all(|c| c.is_ascii_hexdigit()) {
            return existing.to_string();
        }
    }
    let token = random_token();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::persistence::paths::write_atomic_private(&path, token.as_bytes());
    token
}

/// Recompute each pane's activity every tick and broadcast a scope-filtered `activity` frame on
/// each flip of a KNOWN pane (a freshly-seen pane seeds its baseline silently — it rides the
/// `state` ping). A pure busy⇄idle flip does NOT trigger a `state` ping (TS #13). Runs forever
/// until its task is aborted (the GUI host holds its handle and aborts it on stop).
pub async fn run_activity_ticker(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(Duration::from_millis(ACTIVITY_TICK_MS));
    let mut last: HashMap<String, Activity> = HashMap::new();
    // Session uids with a screen classification in flight (one at a time per pane).
    let classifying: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
    loop {
        interval.tick().await;
        let panes: Vec<PaneRef> = shared.model.lock().unwrap().panes();
        let mut seen = std::collections::HashSet::new();
        // Panes that just went quiet inside the busy window: classify their screens off the
        // runtime so a permission dialog reads awaiting-input on a following tick, not after
        // the whole idle threshold. Marker-instrumented panes are owned by their markers.
        let now = now_ms();
        let to_classify: Vec<String> = {
            let mut inflight = classifying.lock().unwrap();
            panes
                .iter()
                .filter(|pr| pr.status != PaneStatus::Exited)
                .filter(|pr| {
                    !shared
                        .sessions
                        .liveness(&pr.session_uid)
                        .is_some_and(|l| l.marker_seen)
                })
                .filter(|pr| {
                    shared
                        .sessions
                        .activity_signal(&pr.session_uid)
                        .is_some_and(|sig| {
                            needs_classification(&sig, now, shared.idle_threshold_ms)
                        })
                })
                .filter(|pr| inflight.insert(pr.session_uid.clone()))
                .map(|pr| pr.session_uid.clone())
                .collect()
        };
        if !to_classify.is_empty() {
            let sessions = Arc::clone(&shared.sessions);
            let classifying = Arc::clone(&classifying);
            tokio::task::spawn_blocking(move || {
                for uid in to_classify {
                    // Leave the in-flight set even if the classification panics, or the pane
                    // would never be classified again.
                    struct Release<'a>(&'a Mutex<std::collections::HashSet<String>>, &'a str);
                    impl Drop for Release<'_> {
                        fn drop(&mut self) {
                            if let Ok(mut set) = self.0.lock() {
                                set.remove(self.1);
                            }
                        }
                    }
                    let _release = Release(&classifying, &uid);
                    classify_screen(&sessions, &uid);
                }
            });
        }
        for pr in &panes {
            seen.insert(pr.pane_id.clone());
            let act = activity_for(
                &shared.sessions,
                shared.idle_threshold_ms,
                &pr.session_uid,
                pr.status,
            );
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

/// Coalescing window for `queue` events / the GUI queue-dirty flag.
pub const QUEUE_COALESCE_MS: u64 = 250;

/// Flushes coalesced queue-changed signals every [`QUEUE_COALESCE_MS`]. Spawned by the
/// embedder next to [`run_reaper_ticker`]; cheap when idle (a mutex + empty-set check).
pub async fn run_queue_flusher(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(Duration::from_millis(QUEUE_COALESCE_MS));
    loop {
        interval.tick().await;
        shared.flush_queue_changes();
    }
}

/// The work-queue visibility-timeout sweep. On its own interval (a separate task the embedder
/// spawns alongside [`run_activity_ticker`] and aborts on stop), it requeues every `claimed`
/// task whose lease has expired — recovering a worker that died without a clean `Exit`, or one
/// wedged past its lease. Exhausted tasks dead-letter. No-op (cheap indexed scan) when idle.
pub async fn run_reaper_ticker(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(Duration::from_millis(REAPER_TICK_MS));
    let grace = crate::control::work::stranded_grace_ms();
    loop {
        interval.tick().await;
        let (reaped, stranded) = {
            let mut wq = shared.work.lock().unwrap();
            let now = now_ms();
            (wq.reap_expired(now), wq.check_stranded(now, grace))
        };
        if !reaped.is_empty() {
            eprintln!(
                "[work] reaper requeued/dead-lettered {} task(s)",
                reaped.len()
            );
        }
        for (queue, n) in stranded {
            eprintln!(
                "{}",
                crate::control::work::stranded_log_line(&queue, n, grace)
            );
        }
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
                p.meta.clone(),
            )
        })
    };
    let Some((shell, args, command, cwd, meta)) = recipe else {
        return; // pane closed during backoff — drop the restart
    };
    let new_uid = uuid::Uuid::new_v4().to_string();
    let opts = crate::session_manager::SpawnOptions {
        uid: new_uid.clone(),
        shell,
        args,
        command,
        cwd,
        env: crate::session_manager::stamp_unattended(None, meta.as_ref()),
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
    let act = activity_for(&shared.sessions, shared.idle_threshold_ms, uid, status);
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
    let speech_settings_path = control_file.with_file_name("speech.json");
    let shared = Shared::new(
        sessions,
        allow_input,
        env!("CARGO_PKG_VERSION"),
        control_file,
        speech_settings_path,
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

    #[test]
    fn build_label_sanitizer() {
        assert_eq!(
            sanitize_build_label("private build, a1b2c3d").as_deref(),
            Some("private build, a1b2c3d")
        );
        assert_eq!(sanitize_build_label("/srv/build/src"), None);
        assert_eq!(sanitize_build_label("C:\\Users\\x"), None);
        assert_eq!(sanitize_build_label("user@host"), None);
        assert_eq!(sanitize_build_label("host:8080"), None);
        assert_eq!(sanitize_build_label("   "), None);
        assert_eq!(sanitize_build_label("é€!!"), None);
        assert_eq!(sanitize_build_label(&"a".repeat(200)).unwrap().len(), 64);
    }

    fn shared_with_pane() -> (Arc<Shared>, String) {
        let (tx, _rx) = unbounded_channel();
        let sm = Arc::new(SessionManager::new(tx));
        let shared = Shared::new(
            sm,
            true,
            "0.0.0-test",
            std::env::temp_dir().join("control-test.json"),
            std::env::temp_dir().join("control-test-speech.json"),
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
            talk: false,
            env: None,
        };
        shared.model.lock().unwrap().add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo {
                id: "t1".into(),
                title: "Tab".into(),
                color: None,
                layout: "auto".into(),
                panes: vec![pane],
                pane_group: None,
            }],
        });
        (shared, "u1".to_string())
    }

    #[test]
    fn classification_runs_once_output_settles_inside_the_busy_window() {
        let sig = |at: Option<u64>, awaiting: Option<(u64, bool)>| ActivitySignal {
            content_at: at,
            content_cursor: 7,
            awaiting,
        };
        let now = 100_000;
        let t = 10_000;
        // Never produced genuine output: nothing to classify.
        assert!(!needs_classification(&sig(None, None), now, t));
        // Still streaming (quiet for less than the settle window).
        assert!(!needs_classification(&sig(Some(99_500), None), now, t));
        // Settled, still inside the busy window, no verdict yet: classify.
        assert!(needs_classification(&sig(Some(99_000), None), now, t));
        // A verdict for older content is stale: classify again.
        assert!(needs_classification(
            &sig(Some(99_000), Some((6, true))),
            now,
            t
        ));
        // A verdict for the current content: nothing to do.
        assert!(!needs_classification(
            &sig(Some(99_000), Some((7, false))),
            now,
            t
        ));
        // Past the idle threshold the pane reads idle anyway: no screen read.
        assert!(!needs_classification(&sig(Some(80_000), None), now, t));
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
            events_url("100.64.0.1", 51888, "abc"),
            "ws://100.64.0.1:51888/events?token=abc"
        );
    }

    #[test]
    fn master_token_persists_only_for_remote_binds() {
        let dir = std::env::temp_dir().join(format!("hp-token-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let control = dir.join("control.json");
        let speech_settings = dir.join("speech.json");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let sessions = Arc::new(SessionManager::new(tx));
        let shared = Shared::new(sessions, false, "0.0.0", control, speech_settings);

        // Loopback default: fresh token every start (frozen legacy behaviour).
        let a = master_token(&shared);
        let b = master_token(&shared);
        assert_ne!(a, b);
        assert!(!dir.join("control-token").exists());

        // A configured remote endpoint: minted once, then stable across restarts.
        shared.set_listen(vec!["wg0".into()], 51888);
        let c = master_token(&shared);
        let d = master_token(&shared);
        assert_eq!(c, d);
        assert_eq!(
            std::fs::read_to_string(dir.join("control-token"))
                .unwrap()
                .trim(),
            c
        );

        // Corrupt file re-mints instead of serving garbage.
        std::fs::write(dir.join("control-token"), "not-a-token").unwrap();
        let e = master_token(&shared);
        assert_ne!(e, "not-a-token");
        assert_eq!(e.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn advertised_host_is_always_loopback() {
        let (shared, _uid) = shared_with_pane();
        assert_eq!(shared.advertised_host(), "127.0.0.1");
        // Loopback is always served, so local discovery never advertises a remote address.
        shared.set_listen(vec!["100.64.0.1".into(), "0.0.0.0".into()], 51888);
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

    // --- queue-changed signal (#9) ---------------------------------------------------

    fn bare_shared() -> Arc<Shared> {
        let (tx, _rx) = unbounded_channel();
        Shared::new(
            Arc::new(SessionManager::new(tx)),
            true,
            "0.0.0-test",
            std::env::temp_dir().join("control-test.json"),
            std::env::temp_dir().join("control-test-speech.json"),
        )
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut v = Vec::new();
        while let Ok(m) = rx.try_recv() {
            v.push(m);
        }
        v
    }

    #[test]
    fn queue_burst_coalesces_to_one_event_and_one_dirty_flag() {
        use crate::control::work::{EnqueueOpts, NackOpts};
        let shared = bare_shared();
        let (_id, mut rx) = shared.events.add_client(None);
        {
            let mut wq = shared.work.lock().unwrap();
            for _ in 0..20 {
                wq.enqueue("a", "{}", EnqueueOpts::default(), 0);
            }
            wq.enqueue("b", "{}", EnqueueOpts::default(), 0);
            let c = wq.claim("a", "w", 1000, 0).unwrap();
            wq.nack(
                &c.task.id,
                c.fencing_token,
                NackOpts {
                    requeue: false,
                    ..Default::default()
                },
                1,
            );
        }
        assert_eq!(shared.flush_queue_changes(), vec!["a", "b"]);
        let frames = drain(&mut rx);
        assert_eq!(frames, vec![r#"{"type":"queue","queues":["a","b"]}"#]);
        assert!(shared.take_queue_dirty());
        assert!(!shared.take_queue_dirty());
        // Nothing changed since: no further event, flag stays clear.
        assert!(shared.flush_queue_changes().is_empty());
        assert!(drain(&mut rx).is_empty());
        assert!(!shared.take_queue_dirty());
    }

    #[test]
    fn every_mutation_kind_marks_its_queue_changed() {
        use crate::control::work::{EnqueueOpts, NackOpts};
        let shared = bare_shared();
        let mut wq = shared.work.lock().unwrap();
        wq.enqueue("q", "{}", EnqueueOpts::default(), 0);
        assert_eq!(wq.take_changed(), vec!["q"]); // enqueue
        let c = wq.claim("q", "w", 10, 0).unwrap();
        assert_eq!(wq.take_changed(), vec!["q"]); // claim
        wq.ack(&c.task.id, c.fencing_token, None, 1);
        assert_eq!(wq.take_changed(), vec!["q"]); // ack
        assert_eq!(wq.purge("q", 100), 1);
        assert_eq!(wq.take_changed(), vec!["q"]); // purge
        wq.enqueue("q", "{}", EnqueueOpts::default(), 0);
        let c = wq.claim("q", "w", 10, 0).unwrap();
        wq.take_changed();
        wq.nack(
            &c.task.id,
            c.fencing_token,
            NackOpts {
                requeue: true,
                ..Default::default()
            },
            1,
        );
        assert_eq!(wq.take_changed(), vec!["q"]); // nack
        let _ = wq.claim("q", "w", 10, 1_000_000_000);
        wq.take_changed();
        assert_eq!(wq.reap_expired(2_000_000_000).len(), 1);
        assert_eq!(wq.take_changed(), vec!["q"]); // reap
        assert!(wq.reap_expired(3_000_000_000).is_empty());
        assert!(wq.take_changed().is_empty()); // no-op sweep stays quiet
    }

    #[test]
    fn queue_event_is_scope_filtered_per_client() {
        use crate::control::scope::Scope;
        use crate::control::work::EnqueueOpts;
        let shared = bare_shared();
        let scoped = |ids: &[&str]| Scope {
            queue_ids: Some(ids.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        let (_m, mut master) = shared.events.add_client(None);
        let (_a, mut only_a) = shared.events.add_client(Some(scoped(&["a"])));
        let (_c, mut only_c) = shared.events.add_client(Some(scoped(&["c"])));
        let (_p, mut pane_only) = shared.events.add_client(Some(Scope {
            pane_ids: Some(vec!["p1".into()]),
            ..Default::default()
        }));
        {
            let mut wq = shared.work.lock().unwrap();
            wq.enqueue("a", "{}", EnqueueOpts::default(), 0);
            wq.enqueue("b", "{}", EnqueueOpts::default(), 0);
        }
        shared.flush_queue_changes();
        assert_eq!(
            drain(&mut master),
            vec![r#"{"type":"queue","queues":["a","b"]}"#]
        );
        assert_eq!(
            drain(&mut only_a),
            vec![r#"{"type":"queue","queues":["a"]}"#]
        );
        assert!(drain(&mut only_c).is_empty());
        assert!(drain(&mut pane_only).is_empty());
    }

    // ---- #131: loopback always + remote endpoints -------------------------------------------

    /// Boot `run_server` on a fresh temp control file with the given remote specs; returns the
    /// shared state, the loopback port and the temp dir (caller removes it).
    async fn boot_listen(tag: &str, specs: Vec<String>) -> (Arc<Shared>, u16, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hp-listen-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, _rx) = unbounded_channel();
        let sm = Arc::new(SessionManager::new(tx));
        let shared = Shared::new(
            sm,
            false,
            "0.0.0-test",
            dir.join("control.json"),
            dir.join("speech.json"),
        );
        shared.set_listen(specs, 0);
        tokio::spawn(run_server(Arc::clone(&shared)));
        let mut port = 0;
        for _ in 0..200 {
            port = shared.port();
            if port != 0 && dir.join("control.json").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_ne!(port, 0, "server never bound");
        (shared, port, dir)
    }

    async fn health_on(host: &str, port: u16) -> Option<String> {
        let r = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap()
            .get(format!("http://{host}:{port}/health"))
            .send()
            .await
            .ok()?;
        r.text().await.ok()
    }

    #[tokio::test]
    async fn loopback_only_by_default_with_legacy_discovery_shape() {
        let (shared, port, dir) = boot_listen("default", Vec::new()).await;
        assert!(health_on("127.0.0.1", port).await.is_some());
        assert!(shared.endpoint_status().is_empty());
        let disco = std::fs::read_to_string(dir.join("control.json")).unwrap();
        assert!(!disco.contains("remoteEndpoints"), "{disco}");
        assert!(!disco.contains("bindAddress"), "{disco}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unbindable_and_wildcard_endpoints_fail_loudly_loopback_stays_up() {
        // 203.0.113.7 is TEST-NET-3: never a local address. No silent fallback: the endpoint
        // is reported failed and loopback still serves.
        let (shared, port, dir) = boot_listen(
            "unbindable",
            vec![
                "203.0.113.7".into(),
                "0.0.0.0".into(),
                "hp-no-such-if0".into(),
            ],
        )
        .await;
        assert!(health_on("127.0.0.1", port).await.is_some());
        let st = shared.endpoint_status();
        assert_eq!(st.len(), 3, "{st:?}");
        assert!(st.iter().all(|e| !e.bound && e.error.is_some()), "{st:?}");
        assert_eq!(st[0].address.as_deref(), Some("203.0.113.7"));
        assert!(st[1]
            .error
            .as_deref()
            .unwrap()
            .starts_with("wildcard bind refused"));
        assert_eq!(st[2].error.as_deref(), Some("interface not found"));
        // Status reaches control.json, never carrying the token.
        let disco: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("control.json")).unwrap())
                .unwrap();
        let token = disco["token"].as_str().unwrap().to_string();
        let eps = disco["remoteEndpoints"].as_array().unwrap();
        assert_eq!(eps.len(), 3);
        assert!(!disco["remoteEndpoints"].to_string().contains(&token));
        assert!(disco.get("bindAddress").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 127.0.0.2 is a loopback ALIAS on Linux (any 127/8 address is local there), so it stands
    /// in for a VPN interface address: the planner would never pick a 127/8 address, so the
    /// plan is injected directly. Proves a second socket serves the same router, and that an
    /// address change rebinds (stale listener dropped, new one up).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn remote_socket_serves_beside_loopback_and_rebinds_on_change() {
        let (shared, port, dir) = boot_listen("both", Vec::new()).await;
        let app = routes::router(Arc::clone(&shared));
        let mut remotes = RemoteListeners::new(app, port);
        let at = |ip: &str| Planned::Addr {
            spec: "testif0".into(),
            ip: ip.parse().unwrap(),
        };
        assert!(remotes.reconcile(&shared, vec![at("127.0.0.2")]).await);
        assert!(
            health_on("127.0.0.1", port).await.is_some(),
            "loopback down"
        );
        let body = health_on("127.0.0.2", port)
            .await
            .expect("remote socket not serving");
        assert!(body.contains(r#""app":"hyperpanes""#));
        // Unauthenticated over the remote socket: no pid (#131).
        assert!(!body.contains("\"pid\""), "{body}");
        let st = shared.endpoint_status();
        assert!(st[0].bound && st[0].address.as_deref() == Some("127.0.0.2"));
        // Same plan again: no status change.
        assert!(!remotes.reconcile(&shared, vec![at("127.0.0.2")]).await);
        // The interface's address moves: new socket up, stale one dropped.
        assert!(remotes.reconcile(&shared, vec![at("127.0.0.3")]).await);
        assert!(health_on("127.0.0.3", port).await.is_some());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            health_on("127.0.0.2", port).await.is_none(),
            "stale listener kept"
        );
        assert!(health_on("127.0.0.1", port).await.is_some());
        drop(remotes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
