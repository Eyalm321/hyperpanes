//! Port of `src/main/session.ts` + `src/main/session-manager.ts` — the CENTRAL owner
//! of live sessions (`Map<uid, Session>`): create / get / write / resize / kill /
//! killAll. In one Rust process all PTYs live here and windows/panes just reference
//! `uid`s (no Electron broadcast-to-all-windows model — this is what simplifies
//! multi-window re-attach).
//!
//! A `Session` ties pty → cwd-sniff (`session::cwd`, on the RAW chunk pre-batch) →
//! `batcher` → `replay` (+ a live `screen` for `mode:"screen"` reads), emitting
//! Data / Cwd / Exit, and tracks `last_output_at` + a monotonic `output_bytes` cursor
//! (UTF-16 units) for the control read-path.
//!
//! # Wave-2 contract (the control server consumes this)
//! * [`SessionManager::create`] spawns a pty and starts its driver task; events arrive
//!   on the [`SessionEvent`] channel passed to [`SessionManager::new`].
//! * Read-path accessors are synchronous and cheap: [`SessionManager::replay`],
//!   [`SessionManager::output_bytes`] (UTF-16 monotonic cursor — pair with
//!   `control::output::sliceSince`), [`SessionManager::last_output_at`] (epoch ms, for
//!   `control::output::waitDecision`), and [`SessionManager::render_screen`].
//! * Mutators: [`write`](SessionManager::write) / [`resize`](SessionManager::resize) /
//!   [`kill`](SessionManager::kill) / [`kill_all`](SessionManager::kill_all).
//!
//! Must be called inside a Tokio runtime — `create` spawns the per-session driver task.
//!
//! ## Design note: clocks
//! The batcher's 16 ms timer runs on a **monotonic** clock (driver-local), while
//! `last_output_at` is an **epoch-ms** stamp so a control server can compare it against
//! its own wall clock exactly as the TS used `Date.now()`. The two never mix.
//!
//! ## Design note: shell integration
//! The injection side of shell integration lives in another track (`shell_integration`,
//! still a stub). To stay decoupled, `create` takes the resolved [`Integration`]
//! (extra args + env) as an *input* rather than calling that module — the wiring layer
//! supplies it. When absent, a plain interactive shell is spawned (additive no-op).

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::session::batcher::DataBatcher;
use crate::session::cwd::parse_osc_cwd;
use crate::session::pty::{spawn_pty, Pty, PtyEvent, PtySpec};
use crate::session::replay::Replay;
use crate::session::screen::Screen;
use crate::session::spawn::{build_env, default_shell, resolve_spawn, resolve_windows_command, EnvInputs, EnvMap};

/// Process-global counter for the in-process backend's `pane-N` uids (see
/// [`SessionManager::fresh_uid`]). Must be process-global (not per-manager) so two windows
/// sharing the one in-process `SessionManager` never mint the same `pane-0` — the historical
/// collision the GUI's own `state.rs` counter was hardened against; minting here keeps that
/// invariant for the daemon scheme too.
static NEXT_INPROC_UID: AtomicU64 = AtomicU64::new(0);

/// `pane-0`, `pane-1`, … — the in-process uid scheme (PTYs die with the GUI, so per-run
/// uniqueness suffices). The daemon scheme is a UUID for cross-run uniqueness; see
/// [`SessionManager::fresh_uid`].
fn next_inproc_uid() -> String {
    format!("pane-{}", NEXT_INPROC_UID.fetch_add(1, Ordering::Relaxed))
}

/// An event emitted by a live session, delivered on the manager's event channel.
/// Mirrors the TS `SessionHandlers` callbacks (`onData` / `onCwd` / `onExit`).
///
/// `Serialize`/`Deserialize` so the session daemon (`session::proto`) can carry the
/// event verbatim to attached clients (`DaemonMsg::Event`) — the enum is a flat,
/// owned-data shape with no GUI/runtime types, so the wire form is the in-process form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// A flushed (batched) output chunk. The renderer/control writes this to its
    /// terminal; it is also what the replay buffer and `output_bytes` accumulate.
    Data { uid: String, data: String },
    /// The pane's working directory changed (from an OSC 7 / OSC 9;9 sniff). De-duped:
    /// fires only on an actual change.
    Cwd { uid: String, cwd: String },
    /// The child exited with this code. Emitted on a *natural* exit only — a manual
    /// `kill` / `kill_all` is silent (mirrors TS `destroy()` gating `onExit`).
    Exit { uid: String, code: i32 },
}

/// Resolved shell-integration inputs for an interactive spawn: extra leading args and
/// env. Supplied by the wiring layer (the `shell_integration` track owns *producing*
/// these). Empty/`None` → a plain interactive shell.
#[derive(Debug, Clone, Default)]
pub struct Integration {
    pub args: Vec<String>,
    pub env: EnvMap,
}

/// Options to spawn a session — the port of TS `SpawnOptions`.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub uid: String,
    /// Shell to launch; defaults to `session::spawn::default_shell()`.
    pub shell: Option<String>,
    /// Program argv. With `command` → the verbatim argv for a DIRECT (no-shell) spawn
    /// (P4a). Without `command` → args handed to the interactive shell.
    pub args: Option<Vec<String>>,
    /// A command to run (shell-wrapped unless `args` is also given). `None` → an
    /// interactive shell.
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// Per-pane env override.
    pub env: Option<EnvMap>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// The owning pane's stable id → injected as `HYPERPANES_PANE_ID`.
    pub pane_id: Option<String>,
    /// Shell integration (interactive branch only). See [`Integration`].
    pub integration: Option<Integration>,
    /// Path to `control.json` (→ `HYPERPANES_CONTROL_FILE`, unless a scoped token is
    /// present). Supplied by the persistence/control wiring. `None` → not injected.
    pub control_file: Option<String>,
}

// Read-side state shared between the driver task (writer) and control reads (readers).
// Replay + screen are mutex-guarded; the counters are atomics.
struct Shared {
    replay: Mutex<Replay>,
    screen: Mutex<Screen>,
    /// Output flushed since the screen mirror was last brought up to date, buffered
    /// for a LAZY `Screen::advance`. The screen is the headless VTE mirror that only
    /// feeds `mode:"screen"` control reads (and the `awaitingInput` heuristic) — which
    /// are infrequent and on-demand. Parsing the full pty stream into it on EVERY flush
    /// (as the eager design did) double-parses the same bytes the GUI grid already
    /// parses, pure wasted CPU when no control client is reading the screen. Instead we
    /// stash flushed bytes here and drain them into `screen` only when a screen read
    /// actually happens (`sync_screen`). Correctness is identical — the screen is brought
    /// fully current at read time — but the hot path does zero VTE work for the mirror.
    screen_pending: Mutex<Vec<u8>>,
    /// Monotonic count of ALL output UTF-16 code units ever flushed (the `since`
    /// cursor basis). Never decreases.
    output_bytes: AtomicU64,
    /// Epoch-ms of the last flush, or 0 if no output yet.
    last_output_at: AtomicU64,
    /// Set by a manual kill so the natural-exit `Exit` event is suppressed.
    killed: AtomicBool,
}

impl Shared {
    /// Drain any buffered output into the screen mirror so a subsequent `screen.render()`
    /// reflects all flushed bytes. Cheap no-op when nothing is pending. Called on the
    /// read path (lazy) instead of on every flush (eager) — see `screen_pending`.
    fn sync_screen(&self) {
        // Take the pending buffer under its own lock first to minimize contention with
        // the driver thread's appends, then parse it into the screen.
        let pending = {
            let mut p = self.screen_pending.lock().unwrap();
            if p.is_empty() {
                return;
            }
            std::mem::take(&mut *p)
        };
        self.screen.lock().unwrap().advance(&pending);
    }
}

/// A sink the pty reader thread calls with each [`PtyEvent`]. `Arc`-wrapped so the
/// real spawn and the test mock can both hold and invoke it.
pub type EventSink = Arc<dyn Fn(PtyEvent) + Send + Sync>;

/// A factory that turns a spec + sink into a live pty. The default uses `spawn_pty`;
/// tests inject a mock so the async pipeline is exercised without ConPTY. `pub(crate)` so
/// the daemon backend ([`session::daemon_client`](crate::session::daemon_client)) can name
/// it in its mirroring `create_with` signature (it ignores the factory — a closure can't
/// cross a socket; the daemon owns real PTYs).
pub(crate) type SpawnFn = Box<dyn FnOnce(&PtySpec, EventSink) -> io::Result<Box<dyn Pty>> + Send>;

/// One live session: the pty handle plus the shared read state. The driver task runs
/// detached; dropping the `Session` drops the pty (closing its handles).
struct Session {
    pty: Box<dyn Pty>,
    shared: Arc<Shared>,
}

/// The reusable, transport-agnostic per-uid session store + operations — the heart of
/// what was historically `SessionManager`, factored out so the **session daemon**
/// (`session::daemon`) can own the very same registry the in-process GUI owns through
/// [`SessionManager`]. Cheap to clone-share via `Arc`; `Clone` shares the same session
/// map + event sender + uid counter (a handle, not a copy), so spawn work can move onto
/// a worker thread (`Spawn` on Windows ConPTY can block ~1s for some shells — see
/// `docs/conpty-passthrough-investigation.md`).
///
/// Every method here is the literal body of the corresponding old `SessionManager`
/// method; `SessionManager` now delegates verbatim so its public API is unchanged.
///
/// ## Daemon-assignable uids
/// The daemon must be the source of truth for uids across GUI restarts (a re-attaching
/// pane references a session by uid — see the plan's "uid stability" note). [`mint_uid`]
/// hands out a process-unique `s{n}` token from a per-registry counter so the daemon can
/// allocate the uid itself when a client's [`proto::SpawnSpec`] left it blank. The GUI
/// path keeps minting uids exactly as before (it passes its own `uid` in `SpawnOptions`),
/// so this is purely additive.
#[derive(Clone)]
pub struct SessionRegistry {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    events: UnboundedSender<SessionEvent>,
    /// Monotonic uid source for daemon-assigned sessions (see [`mint_uid`]).
    next_uid: Arc<AtomicU64>,
}

impl SessionRegistry {
    /// Create a registry that emits [`SessionEvent`]s on `events`.
    pub fn new(events: UnboundedSender<SessionEvent>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_uid: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Allocate a fresh process-unique uid (`s1`, `s2`, …). The daemon calls this when a
    /// client did not pin a uid, making the daemon the authoritative uid source.
    pub fn mint_uid(&self) -> String {
        format!("s{}", self.next_uid.fetch_add(1, Ordering::Relaxed))
    }

    /// Spawn a real pty session for `opts`. Returns once the pty is live and its driver
    /// task is running. Errors if the pty fails to spawn.
    pub fn create(&self, opts: SpawnOptions) -> io::Result<()> {
        let factory: SpawnFn = Box::new(|spec, sink| {
            spawn_pty(spec, move |ev| sink(ev))
        });
        self.create_with(opts, factory)
    }

    /// Spawn a session using a custom pty `factory` (tests inject a mock). The resolved
    /// [`PtySpec`] is built from `opts` exactly as the production path does.
    pub fn create_with(&self, opts: SpawnOptions, factory: SpawnFn) -> io::Result<()> {
        let spec = build_spec(&opts);

        let (ptx, prx) = unbounded_channel::<PtyEvent>();
        let sink: EventSink = Arc::new(move |ev| {
            let _ = ptx.send(ev);
        });
        let pty = factory(&spec, sink)?;

        let shared = Arc::new(Shared {
            replay: Mutex::new(Replay::new()),
            screen: Mutex::new(Screen::new(spec.cols, spec.rows)),
            screen_pending: Mutex::new(Vec::new()),
            output_bytes: AtomicU64::new(0),
            last_output_at: AtomicU64::new(0),
            killed: AtomicBool::new(false),
        });

        let pipeline = SessionPipeline::new(opts.uid.clone(), Arc::clone(&shared));
        let sessions = Arc::clone(&self.sessions);
        let events = self.events.clone();
        let uid = opts.uid.clone();
        tokio::spawn(drive_session(pipeline, prx, events, sessions, uid));

        self.sessions
            .lock()
            .unwrap()
            .insert(opts.uid.clone(), Session { pty, shared });
        Ok(())
    }

    /// Whether a session with `uid` is currently live.
    pub fn has(&self, uid: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(uid)
    }

    /// The uids of all live sessions.
    pub fn uids(&self) -> Vec<String> {
        self.sessions.lock().unwrap().keys().cloned().collect()
    }

    /// Recent output for a re-attaching view (the rolling replay buffer).
    pub fn replay(&self, uid: &str) -> Option<String> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| s.shared.replay.lock().unwrap().get().to_string())
    }

    /// Monotonic count of all output UTF-16 code units ever emitted (the `since`
    /// cursor; pair with `control::output::sliceSince`).
    pub fn output_bytes(&self, uid: &str) -> Option<u64> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| s.shared.output_bytes.load(Ordering::Relaxed))
    }

    /// Epoch-ms of the last output flush, or `None` if the pane has produced nothing
    /// yet (feeds `control::output::waitDecision`).
    pub fn last_output_at(&self, uid: &str) -> Option<u64> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).and_then(|s| match s.shared.last_output_at.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        })
    }

    /// Serialize the pane's current screen to clean text (for `mode:"screen"` reads).
    /// Brings the lazily-fed screen mirror fully up to date first (see `screen_pending`).
    pub fn render_screen(&self, uid: &str) -> Option<String> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| {
            s.shared.sync_screen();
            s.shared.screen.lock().unwrap().render()
        })
    }

    /// Write input to the pane's pty.
    pub fn write(&self, uid: &str, data: &str) {
        let map = self.sessions.lock().unwrap();
        if let Some(s) = map.get(uid) {
            let _ = s.pty.write(data.as_bytes());
        }
    }

    /// Resize the pane (≥1×1) — both the pty grid and the live screen model.
    pub fn resize(&self, uid: &str, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let map = self.sessions.lock().unwrap();
        if let Some(s) = map.get(uid) {
            let _ = s.pty.resize(cols, rows);
            // Apply any buffered output to the screen BEFORE reflowing, so the resize
            // reflows the real content rather than reflowing an empty grid and then
            // advancing post-resize (which would wrap at the new width inconsistently).
            s.shared.sync_screen();
            s.shared.screen.lock().unwrap().resize(cols, rows);
        }
    }

    /// Kill the pane's pty and forget it. The natural-exit `Exit` event is suppressed
    /// (mirrors TS `destroy()`), so a deliberate kill is silent.
    pub fn kill(&self, uid: &str) {
        let removed = self.sessions.lock().unwrap().remove(uid);
        if let Some(s) = removed {
            s.shared.killed.store(true, Ordering::SeqCst);
            let _ = s.pty.kill();
        }
    }

    /// Kill every live pane and clear the map.
    pub fn kill_all(&self) {
        let drained: Vec<Session> = {
            let mut map = self.sessions.lock().unwrap();
            map.drain().map(|(_, s)| s).collect()
        };
        for s in drained {
            s.shared.killed.store(true, Ordering::SeqCst);
            let _ = s.pty.kill();
        }
    }
}

/// Owns every live pty session, keyed by uid — the GUI's single handle to sessions. The
/// GUI holds an `Arc<SessionManager>` and calls this exact API; the **backend** behind it
/// is chosen once at construction (`docs/session-daemon-plan.md` M1):
///
/// * [`Backend::InProcess`] — the historical path: the PTYs are children of the GUI process
///   and live in a [`SessionRegistry`] right here. This is the default
///   ([`SessionManager::new`]) and what CI / `--no-daemon` use.
/// * [`Backend::Daemon`] — a [`DaemonSessionManager`] talking to the PTY-owning
///   [session daemon](crate::session::daemon) over a UDS, so the PTYs survive a GUI crash
///   (selected by [`SessionManager::new_daemon`], wired to `HYPERPANES_SESSION_DAEMON=1`).
///
/// Every public method dispatches to the active backend with an **identical signature**, so
/// the GUI's call sites are untouched — the whole point of M1: the backend swap is invisible
/// above this type. The daemon backend honors the plan's non-blocking-API contract (shadow
/// state + a mirror buffer; only `render_screen` does a bounded round-trip).
///
/// `SessionManager` stays **`Clone`** exactly as the historical in-process one was — the
/// in-process variant clones the cheap [`SessionRegistry`] handle (a shared map + sender),
/// and the daemon variant is an `Arc<DaemonSessionManager>` so a clone shares the one
/// socket + reader thread (the daemon backend is single-connection; a clone is another
/// handle, not another connection). Preserving `Clone` keeps GUI code that moves an owned
/// `mgr.clone()` onto a worker thread (`state.rs::spawn_session_async`) untouched.
#[derive(Clone)]
pub enum SessionManager {
    /// The PTYs live in this process (the default, pre-daemon path).
    InProcess(SessionRegistry),
    /// The PTYs live in the session daemon; this talks to it over a socket. `Arc` so the
    /// non-`Clone` socket/reader inside is shared across manager clones.
    Daemon(Arc<crate::session::daemon_client::DaemonSessionManager>),
}

impl SessionManager {
    /// Create an **in-process** manager that emits [`SessionEvent`]s on `events` (the
    /// default backend — PTYs are children of this process, as before the daemon existed).
    pub fn new(events: UnboundedSender<SessionEvent>) -> Self {
        SessionManager::InProcess(SessionRegistry::new(events))
    }

    /// Create a **daemon-backed** manager: connect to (spawning if needed) the session
    /// daemon for `salt` and forward its events to `events`. Errors only if the daemon
    /// can't be reached/spawned — `main` falls back to [`new`](Self::new) on `Err` so a
    /// daemon failure never blocks launch. `salt` is the user-data dir (same key the GUI's
    /// single-instance gate and the daemon's discovery use). Unix-only in M1.
    pub fn new_daemon(events: UnboundedSender<SessionEvent>, salt: &str) -> io::Result<Self> {
        Ok(SessionManager::Daemon(Arc::new(
            crate::session::daemon_client::DaemonSessionManager::new(events, salt)?,
        )))
    }

    /// The underlying [`SessionRegistry`] for the in-process backend, or `None` when this
    /// manager is daemon-backed (the registry then lives in the daemon, not here). Unused by
    /// the GUI today; kept for in-process tooling that wants the registry directly.
    pub fn registry(&self) -> Option<&SessionRegistry> {
        match self {
            SessionManager::InProcess(r) => Some(r),
            SessionManager::Daemon(_) => None,
        }
    }

    /// Whether this manager is backed by the (crash-surviving) session daemon. The GUI uses
    /// this to decide whether re-attach (M2) is even possible — only a daemon retains a
    /// session across a GUI restart, so the in-process backend always re-spawns from the
    /// recorded spawn command instead.
    pub fn is_daemon(&self) -> bool {
        matches!(self, SessionManager::Daemon(_))
    }

    /// Mint a fresh, **unique** session uid for a NEWLY created pane, choosing a scheme that
    /// fits the backend's uid-stability needs (`docs/session-daemon-plan.md` "uid stability"):
    ///
    /// * **In-process** — the historical `pane-N` token from a process-global counter. The
    ///   PTYs die with the GUI, so a uid only ever has to be unique *within* this run; the
    ///   short readable form is kept (and existing call sites/tests see no change).
    /// * **Daemon** — a `pane-<uuid>` token. Daemon sessions OUTLIVE the GUI, so a re-attaching
    ///   pane references its session by a uid recorded in a *previous* run's snapshot; were a
    ///   new run to re-use a per-run counter (`pane-0`, `pane-1`, …) its fresh panes would
    ///   collide with the daemon's still-live sessions from the prior run (silently adopting a
    ///   stranger's pty). A v4 UUID is globally unique across runs, so a new pane's uid can
    ///   never alias a survivor — and that same uid is exactly what [`to_session_file`] records
    ///   and a later launch re-attaches by.
    ///
    /// (The wire side already PINS whatever uid the GUI passes — see
    /// [`daemon_client`](crate::session::daemon_client) — so making the GUI's *minting* stable
    /// is the whole fix; the daemon honors it verbatim.)
    pub fn fresh_uid(&self) -> String {
        match self {
            SessionManager::InProcess(_) => next_inproc_uid(),
            SessionManager::Daemon(_) => format!("pane-{}", uuid::Uuid::new_v4()),
        }
    }

    /// Spawn a real pty session for `opts`. Returns once the pty is live and its driver
    /// task is running (in-process), or the create request is sent (daemon). Errors if the
    /// pty fails to spawn (in-process) / the request can't be sent (daemon).
    pub fn create(&self, opts: SpawnOptions) -> io::Result<()> {
        match self {
            SessionManager::InProcess(r) => r.create(opts),
            SessionManager::Daemon(d) => d.create(opts),
        }
    }

    /// Spawn a session using a custom pty `factory` (tests inject a mock). The daemon
    /// backend ignores the factory (a closure can't cross a socket; the daemon owns real
    /// PTYs) and spawns a normal session — no production caller uses `create_with`.
    pub fn create_with(&self, opts: SpawnOptions, factory: SpawnFn) -> io::Result<()> {
        match self {
            SessionManager::InProcess(r) => r.create_with(opts, factory),
            SessionManager::Daemon(d) => d.create_with(opts, factory),
        }
    }

    /// Whether a session with `uid` is currently live.
    pub fn has(&self, uid: &str) -> bool {
        match self {
            SessionManager::InProcess(r) => r.has(uid),
            SessionManager::Daemon(d) => d.has(uid),
        }
    }

    /// The uids of all live sessions.
    pub fn uids(&self) -> Vec<String> {
        match self {
            SessionManager::InProcess(r) => r.uids(),
            SessionManager::Daemon(d) => d.uids(),
        }
    }

    /// Recent output for a re-attaching view (the rolling replay buffer).
    pub fn replay(&self, uid: &str) -> Option<String> {
        match self {
            SessionManager::InProcess(r) => r.replay(uid),
            SessionManager::Daemon(d) => d.replay(uid),
        }
    }

    /// Monotonic count of all output UTF-16 code units ever emitted (the `since`
    /// cursor; pair with `control::output::sliceSince`).
    pub fn output_bytes(&self, uid: &str) -> Option<u64> {
        match self {
            SessionManager::InProcess(r) => r.output_bytes(uid),
            SessionManager::Daemon(d) => d.output_bytes(uid),
        }
    }

    /// Epoch-ms of the last output flush, or `None` if the pane has produced nothing
    /// yet (feeds `control::output::waitDecision`).
    pub fn last_output_at(&self, uid: &str) -> Option<u64> {
        match self {
            SessionManager::InProcess(r) => r.last_output_at(uid),
            SessionManager::Daemon(d) => d.last_output_at(uid),
        }
    }

    /// Serialize the pane's current screen to clean text (for `mode:"screen"` reads).
    /// Brings the lazily-fed screen mirror fully up to date first (see `screen_pending`);
    /// the daemon backend does a bounded `RenderScreen` round-trip.
    pub fn render_screen(&self, uid: &str) -> Option<String> {
        match self {
            SessionManager::InProcess(r) => r.render_screen(uid),
            SessionManager::Daemon(d) => d.render_screen(uid),
        }
    }

    /// Write input to the pane's pty.
    pub fn write(&self, uid: &str, data: &str) {
        match self {
            SessionManager::InProcess(r) => r.write(uid, data),
            SessionManager::Daemon(d) => d.write(uid, data),
        }
    }

    /// Resize the pane (≥1×1) — both the pty grid and the live screen model.
    pub fn resize(&self, uid: &str, cols: u16, rows: u16) {
        match self {
            SessionManager::InProcess(r) => r.resize(uid, cols, rows),
            SessionManager::Daemon(d) => d.resize(uid, cols, rows),
        }
    }

    /// Kill the pane's pty and forget it. The natural-exit `Exit` event is suppressed
    /// (mirrors TS `destroy()`), so a deliberate kill is silent.
    pub fn kill(&self, uid: &str) {
        match self {
            SessionManager::InProcess(r) => r.kill(uid),
            SessionManager::Daemon(d) => d.kill(uid),
        }
    }

    /// Kill every live pane and clear the map.
    pub fn kill_all(&self) {
        match self {
            SessionManager::InProcess(r) => r.kill_all(),
            SessionManager::Daemon(d) => d.kill_all(),
        }
    }

    /// Ask the session **daemon** to shut down (kill its sessions + exit), the
    /// quit-vs-keep-alive "OFF" branch and `--kill-daemon` (`docs/session-daemon-plan.md` M3).
    /// **Inert for the in-process backend** — there is no out-of-process daemon to stop; the
    /// PTYs die with the GUI on exit anyway (the GUI's `main` already calls `kill_all` on the
    /// way out). Returns whether a daemon shutdown was actually requested, so a caller can
    /// distinguish "told the daemon to stop" from "nothing to do".
    pub fn shutdown_daemon(&self) -> bool {
        match self {
            SessionManager::InProcess(_) => false,
            SessionManager::Daemon(d) => {
                d.shutdown_daemon();
                true
            }
        }
    }
}

// Build the resolved pty spec from spawn options — the port of the TS `Session`
// constructor's resolution block (resolveSpawn → win-resolve → integration → env).
fn build_spec(opts: &SpawnOptions) -> PtySpec {
    let shell = opts.shell.clone().unwrap_or_else(default_shell);
    let args = opts.args.as_deref();
    let resolved = resolve_spawn(&shell, opts.command.as_deref(), args, opts.cwd.as_deref(), opts.env.as_ref());

    // node-pty/conpty launches `file` directly and won't find a bare shell NAME like
    // 'cmd' — resolve to a full path on Windows (idempotent for an already-resolved
    // file or absolute path).
    let spawn_file = if cfg!(windows) {
        resolve_windows_command(&resolved.file, opts.cwd.as_deref(), opts.env.as_ref())
    } else {
        resolved.file
    };

    // Shell integration applies ONLY on the interactive branch (no `command`).
    let mut final_args = resolved.args;
    let mut integration_env = EnvMap::new();
    if opts.command.is_none() {
        if let Some(integration) = &opts.integration {
            let mut merged = integration.args.clone();
            merged.extend(final_args);
            final_args = merged;
            integration_env = integration.env.clone();
        }
    }

    // FRESH base env per spawn (#28): registry-resolved on Windows so PATH/user-var
    // changes made after app launch reach new panes — not the process env frozen at
    // startup. See `session::env`.
    let process_env: EnvMap = crate::session::env::fresh_env();
    let env = build_env(&EnvInputs {
        process_env: &process_env,
        opts_env: opts.env.as_ref(),
        integration_env: &integration_env,
        pane_id: opts.pane_id.as_deref(),
        control_file: opts.control_file.as_deref().unwrap_or(""),
    });

    PtySpec {
        file: spawn_file,
        args: final_args,
        // A spawnable working directory MUST exist or the underlying `posix_spawn`/
        // `CreateProcessW` fails with ENOENT *before the child ever runs* — sinking the
        // whole session silently (no pty, so no Data/Exit ever reaches an attached
        // client). `opts.cwd` is honored when it is a real directory; otherwise we fall
        // back to one that exists rather than inheriting a stale/missing cwd (e.g. a
        // pane's saved cwd that was since deleted, or a `$HOME` on an unmounted drive —
        // portable-pty defaults a None cwd to `$HOME`, which need not exist). `None`
        // means "let the pty layer pick its default" only when nothing valid is found.
        cwd: resolve_spawn_cwd(opts.cwd.as_deref(), &env),
        env,
        cols: opts.cols.unwrap_or(80),
        rows: opts.rows.unwrap_or(24),
    }
}

/// Pick a working directory that is guaranteed to exist (or `None` to defer to the pty
/// layer's own default). A non-existent cwd makes the child spawn fail with ENOENT, so
/// we never hand one through: the requested `cwd` if it is a real directory, else the
/// resolved env's `$HOME` if that exists, else the daemon/process cwd, else `/` (which
/// always exists on unix). `None` only if even the process cwd is unreadable AND there
/// is no usable `$HOME` — leaving the pty layer to apply its own fallback.
fn resolve_spawn_cwd(requested: Option<&str>, env: &EnvMap) -> Option<String> {
    let is_dir = |p: &str| std::path::Path::new(p).is_dir();
    if let Some(c) = requested {
        if is_dir(c) {
            return Some(c.to_string());
        }
    }
    if let Some(home) = env.get("HOME") {
        if is_dir(home) {
            return Some(home.clone());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(s) = cwd.to_str() {
            return Some(s.to_string());
        }
    }
    if cfg!(unix) && is_dir("/") {
        return Some("/".to_string());
    }
    None
}

fn epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// The async driver: pull pty events, run them through the pipeline, forward emitted
// session events to the manager channel, and on terminal exit remove the session.
async fn drive_session(
    mut pipeline: SessionPipeline,
    mut prx: UnboundedReceiver<PtyEvent>,
    events: UnboundedSender<SessionEvent>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    uid: String,
) {
    let start = Instant::now();
    let now_mono = || start.elapsed().as_millis() as u64;

    loop {
        let sleep_for = pipeline
            .batcher
            .deadline()
            .map(|d| Duration::from_millis(d.saturating_sub(now_mono())));

        tokio::select! {
            maybe = prx.recv() => {
                match maybe {
                    Some(PtyEvent::Data(bytes)) => {
                        let decoded = pipeline.decode(&bytes);
                        for ev in pipeline.on_data(&decoded, now_mono(), epoch_ms()) {
                            let _ = events.send(ev);
                        }
                    }
                    Some(PtyEvent::Exit(code)) => {
                        for ev in pipeline.on_exit(code, epoch_ms()) {
                            let _ = events.send(ev);
                        }
                        break;
                    }
                    None => break, // pty sink dropped
                }
            }
            _ = async { tokio::time::sleep(sleep_for.unwrap()).await }, if sleep_for.is_some() => {
                for ev in pipeline.on_timer(epoch_ms()) {
                    let _ = events.send(ev);
                }
            }
        }
    }

    sessions.lock().unwrap().remove(&uid);
}

// The session output pipeline: cwd-sniff (raw) → batcher → flush → replay + screen +
// counters + Data/Cwd/Exit. Driver-task-local except for the shared read state.
// Unit-testable in isolation by driving its methods with controlled clocks.
struct SessionPipeline {
    uid: String,
    batcher: DataBatcher,
    /// Carry for an OSC cwd sequence split across pty chunks.
    osc_carry: String,
    /// De-dupe: emit `Cwd` only when the directory actually changes.
    last_cwd: Option<String>,
    /// Carry for an incomplete trailing UTF-8 sequence split across pty reads.
    utf8_carry: Vec<u8>,
    ended: bool,
    shared: Arc<Shared>,
}

impl SessionPipeline {
    fn new(uid: String, shared: Arc<Shared>) -> Self {
        Self {
            uid,
            batcher: DataBatcher::new(),
            osc_carry: String::new(),
            last_cwd: None,
            utf8_carry: Vec::new(),
            ended: false,
            shared,
        }
    }

    /// Streaming UTF-8 decode of a raw pty chunk, buffering an incomplete trailing
    /// sequence so a multibyte glyph split across reads isn't mangled (node-pty's
    /// `StringDecoder` does the same). Genuinely invalid bytes become U+FFFD.
    fn decode(&mut self, chunk: &[u8]) -> String {
        decode_utf8_streaming(&mut self.utf8_carry, chunk)
    }

    /// Handle a decoded raw chunk: sniff cwd (pre-batch), then feed the batcher. A
    /// size-triggered flush is processed inline.
    fn on_data(&mut self, raw: &str, now_mono_ms: u64, now_epoch_ms: u64) -> Vec<SessionEvent> {
        if self.ended {
            return Vec::new();
        }
        let mut out = Vec::new();

        // Tap the RAW chunk for a cwd OSC before batching (xterm consumes these OSCs
        // silently; we only sniff the cwd out).
        let (cwd, carry) = parse_osc_cwd(&self.osc_carry, raw);
        self.osc_carry = carry;
        if let Some(cwd) = cwd {
            if Some(&cwd) != self.last_cwd.as_ref() {
                self.last_cwd = Some(cwd.clone());
                out.push(SessionEvent::Cwd { uid: self.uid.clone(), cwd });
            }
        }

        if let Some(flushed) = self.batcher.write(raw, now_mono_ms) {
            self.flush_into(flushed, now_epoch_ms, &mut out);
        }
        out
    }

    /// A time-triggered flush from the driver's 16 ms timer.
    fn on_timer(&mut self, now_epoch_ms: u64) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        if let Some(flushed) = self.batcher.flush() {
            self.flush_into(flushed, now_epoch_ms, &mut out);
        }
        out
    }

    /// Terminal pty exit. On a *natural* exit, flush remaining output then emit `Exit`.
    /// On a manual kill (`shared.killed`), stay silent — mirrors TS `destroy()` gating.
    fn on_exit(&mut self, code: i32, now_epoch_ms: u64) -> Vec<SessionEvent> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        if self.shared.killed.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(flushed) = self.batcher.flush() {
            self.flush_into(flushed, now_epoch_ms, &mut out);
        }
        out.push(SessionEvent::Exit { uid: self.uid.clone(), code });
        out
    }

    // Apply a flushed batch: grow replay, BUFFER for the lazy screen mirror, bump the
    // cursor/stamp, emit Data. The screen is NOT parsed here — its bytes are stashed in
    // `screen_pending` and parsed on demand by `Shared::sync_screen` at read time. This
    // removes the per-flush second VTE parse (the GUI grid already parses the same bytes
    // via `SessionEvent::Data`), which was pure wasted CPU when no control client reads
    // the screen. See `Shared::screen_pending`.
    fn flush_into(&mut self, data: String, now_epoch_ms: u64, out: &mut Vec<SessionEvent>) {
        let n = data.encode_utf16().count() as u64;
        self.shared.replay.lock().unwrap().append(&data);
        self.shared.screen_pending.lock().unwrap().extend_from_slice(data.as_bytes());
        self.shared.output_bytes.fetch_add(n, Ordering::Relaxed);
        self.shared.last_output_at.store(now_epoch_ms, Ordering::Relaxed);
        out.push(SessionEvent::Data { uid: self.uid.clone(), data });
    }
}

/// Streaming UTF-8 decoder: append `chunk` to `carry`, emit all decodable text, and
/// keep only an incomplete trailing sequence in `carry` for the next call. Invalid
/// bytes are replaced with U+FFFD (matching `from_utf8_lossy`). Free function so it can
/// be unit-tested directly.
pub fn decode_utf8_streaming(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    let mut decoded = String::new();
    loop {
        match std::str::from_utf8(carry) {
            Ok(s) => {
                decoded.push_str(s);
                carry.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: bytes [..valid] are valid UTF-8 by `valid_up_to`'s contract.
                decoded.push_str(unsafe { std::str::from_utf8_unchecked(&carry[..valid]) });
                match e.error_len() {
                    Some(len) => {
                        // A genuinely invalid sequence mid-buffer: replace and continue.
                        decoded.push('\u{FFFD}');
                        carry.drain(..valid + len);
                    }
                    None => {
                        // An incomplete sequence at the tail: keep it for next time.
                        carry.drain(..valid);
                        break;
                    }
                }
            }
        }
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::batcher::BATCH_MAX_SIZE;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            replay: Mutex::new(Replay::new()),
            screen: Mutex::new(Screen::new(80, 24)),
            screen_pending: Mutex::new(Vec::new()),
            output_bytes: AtomicU64::new(0),
            last_output_at: AtomicU64::new(0),
            killed: AtomicBool::new(false),
        })
    }

    // ---- streaming UTF-8 decoder ----

    #[test]
    fn decoder_passes_through_ascii() {
        let mut carry = Vec::new();
        assert_eq!(decode_utf8_streaming(&mut carry, b"hello"), "hello");
        assert!(carry.is_empty());
    }

    #[test]
    fn decoder_buffers_a_split_multibyte_char() {
        let mut carry = Vec::new();
        let emoji = "😀".as_bytes(); // 4 bytes: F0 9F 98 80
        // First read ends mid-emoji.
        let a = decode_utf8_streaming(&mut carry, &emoji[..2]);
        assert_eq!(a, "");
        assert_eq!(carry.len(), 2);
        // Second read completes it.
        let b = decode_utf8_streaming(&mut carry, &emoji[2..]);
        assert_eq!(b, "😀");
        assert!(carry.is_empty());
    }

    #[test]
    fn decoder_replaces_truly_invalid_bytes() {
        let mut carry = Vec::new();
        // 0xFF is never valid UTF-8.
        let out = decode_utf8_streaming(&mut carry, &[b'a', 0xFF, b'b']);
        assert_eq!(out, "a\u{FFFD}b");
        assert!(carry.is_empty());
    }

    // ---- pipeline: cwd sniffing + de-dupe ----

    #[test]
    fn pipeline_emits_cwd_on_change_only() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let seq = "\u{1b}]7;file:///C:/proj\u{07}";
        let evs = p.on_data(seq, 0, 1000);
        assert_eq!(evs[0], SessionEvent::Cwd { uid: "u1".into(), cwd: "C:\\proj".into() });
        // Same cwd again → no Cwd event (the prompt re-emits its OSC each keystroke).
        let evs2 = p.on_data(seq, 1, 1001);
        assert!(!evs2.iter().any(|e| matches!(e, SessionEvent::Cwd { .. })));
    }

    // ---- pipeline: flush → replay + counters + Data ----

    #[test]
    fn pipeline_time_flush_emits_data_and_updates_state() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        // Small writes don't flush synchronously.
        assert!(p.on_data("abc", 0, 500).is_empty());
        assert!(p.on_data("de", 5, 505).is_empty());
        // Timer fires.
        let evs = p.on_timer(520);
        assert_eq!(evs, vec![SessionEvent::Data { uid: "u1".into(), data: "abcde".into() }]);
        assert_eq!(sh.replay.lock().unwrap().get(), "abcde");
        assert_eq!(sh.output_bytes.load(Ordering::Relaxed), 5);
        assert_eq!(sh.last_output_at.load(Ordering::Relaxed), 520);
    }

    #[test]
    fn pipeline_output_bytes_counts_utf16_units() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("😀a", 0, 100); // emoji=2 u16, a=1 → 3
        p.on_timer(110);
        assert_eq!(sh.output_bytes.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn pipeline_size_overflow_flushes_inline() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let big = "x".repeat(BATCH_MAX_SIZE - 1);
        assert!(p.on_data(&big, 0, 100).is_empty());
        // This pushes past the threshold → the buffered `big` flushes out as Data.
        let evs = p.on_data("yy", 1, 101);
        assert_eq!(evs, vec![SessionEvent::Data { uid: "u1".into(), data: big.clone() }]);
        // The new chunk remains buffered until its own flush.
        let evs2 = p.on_timer(120);
        assert_eq!(evs2, vec![SessionEvent::Data { uid: "u1".into(), data: "yy".into() }]);
    }

    // ---- pipeline: exit gating ----

    #[test]
    fn pipeline_natural_exit_flushes_then_emits_exit() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("tail", 0, 200);
        let evs = p.on_exit(0, 210);
        assert_eq!(
            evs,
            vec![
                SessionEvent::Data { uid: "u1".into(), data: "tail".into() },
                SessionEvent::Exit { uid: "u1".into(), code: 0 },
            ]
        );
    }

    #[test]
    fn pipeline_manual_kill_suppresses_exit() {
        let sh = shared();
        sh.killed.store(true, Ordering::SeqCst);
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("tail", 0, 200);
        let evs = p.on_exit(0, 210);
        assert!(evs.is_empty(), "manual kill must be silent");
    }

    #[test]
    fn pipeline_ignores_data_after_exit() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_exit(0, 100);
        assert!(p.on_data("late", 1, 101).is_empty());
    }

    // ---- end-to-end manager wiring via a mock pty (no ConPTY) ----

    #[derive(Default)]
    struct MockPty {
        last_resize: Mutex<Option<(u16, u16)>>,
        killed: AtomicBool,
    }
    impl Pty for MockPty {
        fn write(&self, _data: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            *self.last_resize.lock().unwrap() = Some((cols, rows));
            Ok(())
        }
        fn kill(&self) -> io::Result<()> {
            self.killed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    // Create a session whose pty is a mock; returns the manager event receiver and the
    // captured event sink the test uses to drive PtyEvents into the driver task.
    fn make_session(
        uid: &str,
    ) -> (
        SessionManager,
        tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
        EventSink,
    ) {
        let (etx, erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        let slot: Arc<Mutex<Option<EventSink>>> = Arc::new(Mutex::new(None));
        let slot2 = Arc::clone(&slot);
        let factory: SpawnFn = Box::new(move |_spec, sink| {
            *slot2.lock().unwrap() = Some(sink);
            Ok(Box::new(MockPty::default()) as Box<dyn Pty>)
        });
        mgr.create_with(SpawnOptions { uid: uid.into(), ..Default::default() }, factory)
            .expect("create");
        let sink = slot.lock().unwrap().clone().expect("sink captured");
        (mgr, erx, sink)
    }

    async fn recv(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    ) -> Option<SessionEvent> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.ok().flatten()
    }

    #[tokio::test]
    async fn manager_streams_data_then_removes_on_natural_exit() {
        let (mgr, mut rx, sink) = make_session("u1");
        assert!(mgr.has("u1"));

        sink(PtyEvent::Data(b"hello".to_vec()));
        // Flushed by the 16 ms batch timer.
        assert_eq!(recv(&mut rx).await, Some(SessionEvent::Data { uid: "u1".into(), data: "hello".into() }));
        assert_eq!(mgr.output_bytes("u1"), Some(5));
        assert_eq!(mgr.replay("u1").as_deref(), Some("hello"));
        assert!(mgr.last_output_at("u1").is_some());

        sink(PtyEvent::Exit(0));
        assert_eq!(recv(&mut rx).await, Some(SessionEvent::Exit { uid: "u1".into(), code: 0 }));

        // The driver removes the session from the map after the terminal exit.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!mgr.has("u1"));
    }

    #[tokio::test]
    async fn manager_kill_is_silent_and_forgets_the_session() {
        let (mgr, mut rx, sink) = make_session("u1");
        mgr.kill("u1");
        assert!(!mgr.has("u1"));

        // A pty Exit arriving after a manual kill must NOT surface as a SessionEvent.
        sink(PtyEvent::Exit(0));
        let got = recv(&mut rx).await;
        assert!(
            !matches!(got, Some(SessionEvent::Exit { .. })),
            "manual kill must suppress the Exit event, got {got:?}"
        );
    }

    #[tokio::test]
    async fn manager_emits_cwd_event_from_an_osc_sniff() {
        let (mgr, mut rx, sink) = make_session("u1");
        sink(PtyEvent::Data(b"\x1b]7;file:///C:/work\x07".to_vec()));
        // Cwd fires immediately (pre-batch), before any Data flush.
        assert_eq!(recv(&mut rx).await, Some(SessionEvent::Cwd { uid: "u1".into(), cwd: "C:\\work".into() }));
        let _ = mgr;
    }

    #[test]
    fn lazy_screen_reflects_buffered_output_on_sync() {
        // flush_into buffers for the screen instead of parsing eagerly; sync_screen must
        // bring the mirror fully current so a read sees everything flushed so far.
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        p.on_data("hello world", 0, 100);
        p.on_timer(120); // flush → buffered, screen NOT yet advanced
        assert_eq!(sh.screen.lock().unwrap().render(), "", "screen is lazy: empty before sync");
        sh.sync_screen();
        assert_eq!(sh.screen.lock().unwrap().render(), "hello world");
        // A second sync with nothing pending is a no-op and leaves the screen intact.
        sh.sync_screen();
        assert_eq!(sh.screen.lock().unwrap().render(), "hello world");
    }

    #[tokio::test]
    async fn manager_render_screen_syncs_pending_output() {
        let (mgr, mut rx, sink) = make_session("u1");
        sink(PtyEvent::Data(b"abc\r\ndef".to_vec()));
        // Wait for the Data flush so the bytes are buffered for the screen.
        assert!(matches!(recv(&mut rx).await, Some(SessionEvent::Data { .. })));
        // render_screen must lazily sync the buffered output before serializing.
        assert_eq!(mgr.render_screen("u1").as_deref(), Some("abc\ndef"));
    }

    #[tokio::test]
    async fn manager_resize_updates_screen_and_pty() {
        let (mgr, _rx, _sink) = make_session("u1");
        mgr.resize("u1", 120, 40);
        // Screen render works post-resize (smoke that the screen lock path is sound).
        assert!(mgr.render_screen("u1").is_some());
    }

    // ---- uid minting policy (session-daemon-plan "uid stability") ----

    #[test]
    fn in_process_backend_is_not_daemon() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        assert!(!mgr.is_daemon(), "the in-process backend reports is_daemon() == false");
    }

    // shutdown_daemon is INERT for the in-process backend (session-daemon M3): there is no
    // out-of-process daemon to stop, so it returns false and does nothing — the quit path
    // distinguishes this from a daemon shutdown by the bool.
    #[test]
    fn in_process_shutdown_daemon_is_inert() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        assert!(!mgr.shutdown_daemon(), "in-process shutdown_daemon is a no-op returning false");
    }

    #[test]
    fn in_process_fresh_uid_is_pane_n_and_unique() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        // The in-process scheme is the readable `pane-N` (PTYs die with the GUI, so per-run
        // uniqueness suffices); the counter is process-global so two managers never alias.
        let a = mgr.fresh_uid();
        let b = mgr.fresh_uid();
        assert!(a.starts_with("pane-"), "in-process fresh_uid is pane-N, got {a}");
        assert_ne!(a, b, "successive fresh_uids are unique");
        let (etx2, _erx2) = unbounded_channel::<SessionEvent>();
        let mgr2 = SessionManager::new(etx2);
        // A SECOND manager shares the process-global counter — no cross-manager `pane-0`
        // collision (the historical multi-window clobber this counter was hardened against).
        assert_ne!(mgr2.fresh_uid(), a, "the counter is process-global across managers");
    }
}
