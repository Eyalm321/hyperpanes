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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::session::activity::{ActivitySignal, OutputGate};
use crate::session::batcher::DataBatcher;
use crate::session::cwd::parse_osc_cwd;
use crate::session::pty::{spawn_pty, Pty, PtyEvent, PtySpec};
use crate::session::replay::Replay;
use crate::session::respawn::{DeferredPty, Dying, DyingSessions, RespawnPolicy};
use crate::session::screen::Screen;
use crate::session::spawn::{
    build_env, default_shell, resolve_control_file, resolve_spawn, resolve_windows_command,
    EnvInputs, EnvMap,
};

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
    /// `cursor` is the monotonic `output_bytes` value AFTER this chunk (UTF-16 code
    /// units), computed atomically at flush time — remote clients use it to splice a
    /// live stream onto a `GET /output` snapshot without gaps or duplicates.
    /// `#[serde(default)]` keeps the daemon proto readable from pre-cursor peers.
    Data {
        uid: String,
        data: String,
        #[serde(default)]
        cursor: u64,
    },
    /// The pane's working directory changed (from an OSC 7 / OSC 9;9 sniff). De-duped:
    /// fires only on an actual change.
    Cwd { uid: String, cwd: String },
    /// The child exited with this code. Emitted on a *natural* exit only — a manual
    /// `kill` / `kill_all` is silent (mirrors TS `destroy()` gating `onExit`).
    Exit { uid: String, code: i32 },
    /// Phase-4 semantic markers (sniffed off the raw stream like cwd, then stripped).
    /// Additive — they ride the daemon proto verbatim via serde, and consumers that only
    /// care about output/cwd/exit ignore them.
    ///
    /// `133;C` — a command's output begins (a command is now running).
    CommandStart { uid: String },
    /// `133;D` / `133;D;<code>` — a command finished, optionally with its exit code.
    CommandEnd { uid: String, code: Option<i32> },
    /// `133;A` / `133;B` — the shell is at / drawing a prompt → ready for input.
    PromptReady { uid: String },
    /// `9;hp;state=…` — the program self-reports its liveness.
    AgentState {
        uid: String,
        state: AgentLiveness,
        code: Option<i32>,
    },
}

/// Serializable mirror of [`crate::session::osc133::AgentLiveness`] so the event can ride
/// the daemon proto. Kept in lockstep with the parser's enum via [`From`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentLiveness {
    Busy,
    AwaitingInput,
    Done,
    Error,
}

impl From<crate::session::osc133::AgentLiveness> for AgentLiveness {
    fn from(a: crate::session::osc133::AgentLiveness) -> Self {
        use crate::session::osc133::AgentLiveness as P;
        match a {
            P::Busy => AgentLiveness::Busy,
            P::AwaitingInput => AgentLiveness::AwaitingInput,
            P::Done => AgentLiveness::Done,
            P::Error => AgentLiveness::Error,
        }
    }
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
    /// Genuine-output stamp + cursor and the cached awaiting-input classification (see
    /// [`crate::session::activity`]). Written by the pipeline on genuine flushes and by the
    /// control layer's classifier; read by `control::server::activity_for`.
    activity: Mutex<ActivitySignal>,
    /// Set by a manual kill so the natural-exit `Exit` event is suppressed.
    killed: AtomicBool,
    /// Phase-4 liveness mirror, fed by the OSC-133 / OSC-9;hp sniff so the *pull-based*
    /// activity ticker can read a pane's precise state cheaply (no per-tick scan). Gated
    /// on `marker_seen`: until a marker is ever seen, the legacy silence heuristic owns
    /// the activity, so an un-instrumented pane is byte-for-byte unchanged.
    ///
    /// `prompt_ready` — true after `133;A`/`133;D` or agent awaiting-input/done.
    prompt_ready: AtomicBool,
    /// `command_running` — true after `133;C` or agent busy; cleared by `133;D`/prompt.
    command_running: AtomicBool,
    /// Last `133;D` exit code (`i32::MIN` = none yet).
    last_exit_code: AtomicI32,
    /// Have we EVER seen a marker? Gates the fallback to the silence heuristic.
    marker_seen: AtomicBool,
}

/// A cheap snapshot of a session's phase-4 liveness mirror (the pull side of the marker
/// channel). `marker_seen == false` ⇒ the caller must fall back to the silence heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    pub prompt_ready: bool,
    pub command_running: bool,
    pub last_exit_code: Option<i32>,
    pub marker_seen: bool,
}

impl Shared {
    /// Drain any buffered output into the screen mirror so a subsequent `screen.render()`
    /// reflects all flushed bytes. Cheap no-op when nothing is pending. Called on the
    /// read path (lazy) instead of on every flush (eager) — see `screen_pending`.
    fn sync_screen(&self) {
        // Take the pending buffer under its own lock first to minimize contention with
        // the driver thread's appends, then parse it into the screen.
        //
        // The screen lock is taken FIRST and held while the pending bytes are taken and parsed:
        // two readers that each took a batch and then raced for the screen could otherwise
        // apply the later batch before the earlier one. Lock order everywhere:
        // replay -> screen -> screen_pending (`screen_seed`), replay -> screen_pending
        // (`flush_into`), screen -> screen_pending (here).
        let mut screen = self.screen.lock().unwrap();
        let pending = {
            let mut p = self.screen_pending.lock().unwrap();
            if p.is_empty() {
                return;
            }
            std::mem::take(&mut *p)
        };
        screen.advance(&pending);
    }

    /// A [`ScreenSeed`](crate::session::screen::ScreenSeed) for a re-attaching view: the screen
    /// serialized at exactly the output cursor it reflects. The replay lock (which
    /// `flush_into` holds while it appends a whole chunk, bumps the cursor and buffers the
    /// chunk for the screen) is held only to read the cursor and take the pending bytes; the
    /// parse and the serialization run under the screen lock alone, so the output path never
    /// waits on them.
    fn screen_seed(&self) -> crate::session::screen::ScreenSeed {
        let replay = self.replay.lock().unwrap();
        let mut screen = self.screen.lock().unwrap();
        let pending = std::mem::take(&mut *self.screen_pending.lock().unwrap());
        let cursor = self.output_bytes.load(Ordering::Relaxed);
        drop(replay);
        screen.advance(&pending);
        let (cols, rows) = screen.dims();
        crate::session::screen::ScreenSeed {
            vt: screen.snapshot_vt(),
            cols,
            rows,
            cursor,
        }
    }

    /// Fold one parsed [`Marker`](crate::session::osc133::Marker) into the liveness mirror.
    /// Every marker flips `marker_seen`, which is what hands authority from the silence
    /// heuristic to the precise state for this pane (gate in `control::server::activity_for`).
    fn apply_marker(&self, m: &crate::session::osc133::Marker) {
        use crate::session::osc133::{AgentLiveness as A, Marker};
        self.marker_seen.store(true, Ordering::Relaxed);
        match m {
            Marker::CommandStart => {
                self.command_running.store(true, Ordering::Relaxed);
                self.prompt_ready.store(false, Ordering::Relaxed);
            }
            Marker::CommandEnd { code } => {
                self.command_running.store(false, Ordering::Relaxed);
                self.prompt_ready.store(true, Ordering::Relaxed);
                if let Some(c) = code {
                    self.last_exit_code.store(*c, Ordering::Relaxed);
                }
            }
            Marker::PromptReady => {
                self.command_running.store(false, Ordering::Relaxed);
                self.prompt_ready.store(true, Ordering::Relaxed);
            }
            Marker::Agent { state, code } => match state {
                A::Busy => {
                    self.command_running.store(true, Ordering::Relaxed);
                    self.prompt_ready.store(false, Ordering::Relaxed);
                }
                A::AwaitingInput | A::Done => {
                    self.command_running.store(false, Ordering::Relaxed);
                    self.prompt_ready.store(true, Ordering::Relaxed);
                }
                A::Error => {
                    self.command_running.store(false, Ordering::Relaxed);
                    self.prompt_ready.store(true, Ordering::Relaxed);
                    if let Some(c) = code {
                        self.last_exit_code.store(*c, Ordering::Relaxed);
                    }
                }
            },
        }
    }

    /// Snapshot the liveness mirror for the activity ticker.
    fn liveness(&self) -> Liveness {
        let raw = self.last_exit_code.load(Ordering::Relaxed);
        Liveness {
            prompt_ready: self.prompt_ready.load(Ordering::Relaxed),
            command_running: self.command_running.load(Ordering::Relaxed),
            last_exit_code: (raw != i32::MIN).then_some(raw),
            marker_seen: self.marker_seen.load(Ordering::Relaxed),
        }
    }

    /// Input was just sent → optimistically clear `prompt_ready` so the busy edge is
    /// reported without waiting for the next marker (tightens latency, never lies for
    /// long — a real prompt re-asserts `prompt_ready` on its next `133;A`).
    fn note_write(&self) {
        if self.marker_seen.load(Ordering::Relaxed) {
            self.prompt_ready.store(false, Ordering::Relaxed);
        }
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
    /// The pane id the session was spawned for (`SpawnOptions::pane_id`).
    pane_id: Option<String>,
    /// The killed predecessor a deferred spawn waits on ([`crate::session::respawn`]). A kill
    /// before the spawn hands it on to the pane's next respawn.
    awaiting: Option<Dying>,
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
    /// Killed sessions whose processes may still be exiting, by pane id: a respawn of the
    /// same pane waits on them ([`crate::session::respawn`]).
    dying: Arc<Mutex<DyingSessions>>,
    respawn: RespawnPolicy,
}

impl SessionRegistry {
    /// Create a registry that emits [`SessionEvent`]s on `events`.
    pub fn new(events: UnboundedSender<SessionEvent>) -> Self {
        Self::with_respawn_policy(events, RespawnPolicy::default())
    }

    /// [`new`](Self::new) with its own respawn wait (a test's probe and bound).
    pub(crate) fn with_respawn_policy(
        events: UnboundedSender<SessionEvent>,
        respawn: RespawnPolicy,
    ) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events,
            next_uid: Arc::new(AtomicU64::new(1)),
            dying: Arc::new(Mutex::new(DyingSessions::default())),
            respawn,
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
        let factory: SpawnFn = Box::new(|spec, sink| spawn_pty(spec, move |ev| sink(ev)));
        self.create_with(opts, factory)
    }

    /// Spawn a session using a custom pty `factory` (tests inject a mock). The resolved
    /// [`PtySpec`] is built from `opts` exactly as the production path does.
    ///
    /// A respawn of a pane whose previous session was just killed and still has live
    /// processes returns at once, but the pty spawns only once those processes are gone or
    /// the bound passes ([`crate::session::respawn`]). Until then the session buffers writes
    /// and resizes.
    pub fn create_with(&self, opts: SpawnOptions, factory: SpawnFn) -> io::Result<()> {
        let spec = build_spec(&opts);
        let (cols, rows) = (spec.cols, spec.rows);

        let (ptx, prx) = unbounded_channel::<PtyEvent>();
        let sink: EventSink = Arc::new(move |ev| {
            let _ = ptx.send(ev);
        });
        let awaiting = opts
            .pane_id
            .as_deref()
            .and_then(|pane| self.dying.lock().unwrap().take(pane, self.respawn.bound))
            .filter(|d| (self.respawn.probe)(d.leader));
        let pty: Box<dyn Pty> = match awaiting {
            None => factory(&spec, sink)?,
            Some(dying) => {
                crate::persistence::session_log::record(
                    "respawn-wait",
                    &opts.uid,
                    "previous session of the pane still running; spawn deferred",
                    opts.pane_id.as_deref(),
                );
                Box::new(DeferredPty::spawn_after(
                    dying,
                    self.respawn.clone(),
                    spec,
                    sink,
                    factory,
                    opts.uid.clone(),
                    opts.pane_id.clone(),
                )?)
            }
        };

        let shared = Arc::new(Shared {
            replay: Mutex::new(Replay::new()),
            screen: Mutex::new(Screen::new(cols, rows)),
            screen_pending: Mutex::new(Vec::new()),
            output_bytes: AtomicU64::new(0),
            last_output_at: AtomicU64::new(0),
            activity: Mutex::new(ActivitySignal::default()),
            killed: AtomicBool::new(false),
            prompt_ready: AtomicBool::new(false),
            command_running: AtomicBool::new(false),
            last_exit_code: AtomicI32::new(i32::MIN),
            marker_seen: AtomicBool::new(false),
        });

        let pipeline = SessionPipeline::new(opts.uid.clone(), Arc::clone(&shared));
        let sessions = Arc::clone(&self.sessions);
        let events = self.events.clone();
        let uid = opts.uid.clone();
        tokio::spawn(drive_session(pipeline, prx, events, sessions, uid));

        self.sessions.lock().unwrap().insert(
            opts.uid.clone(),
            Session {
                pty,
                shared,
                pane_id: opts.pane_id.clone(),
                awaiting,
            },
        );
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
        map.get(uid)
            .map(|s| s.shared.replay.lock().unwrap().get().to_string())
    }

    /// Monotonic count of all output UTF-16 code units ever emitted (the `since`
    /// cursor; pair with `control::output::sliceSince`).
    pub fn output_bytes(&self, uid: &str) -> Option<u64> {
        let map = self.sessions.lock().unwrap();
        map.get(uid)
            .map(|s| s.shared.output_bytes.load(Ordering::Relaxed))
    }

    /// Epoch-ms of the last output flush, or `None` if the pane has produced nothing
    /// yet (feeds `control::output::waitDecision`).
    pub fn last_output_at(&self, uid: &str) -> Option<u64> {
        let map = self.sessions.lock().unwrap();
        map.get(uid)
            .and_then(|s| match s.shared.last_output_at.load(Ordering::Relaxed) {
                0 => None,
                ms => Some(ms),
            })
    }

    /// Test-only hook: stamp the pane's last-output time (epoch ms) so liveness heuristics
    /// can be exercised deterministically.
    #[doc(hidden)]
    pub fn set_last_output_at_for_test(&self, uid: &str, ms: u64) {
        if let Some(s) = self.sessions.lock().unwrap().get(uid) {
            s.shared.last_output_at.store(ms, Ordering::Relaxed);
            let mut a = s.shared.activity.lock().unwrap();
            let cursor = s.shared.output_bytes.load(Ordering::Relaxed);
            a.note_genuine(ms, cursor);
        }
    }

    /// Test-only hook: stamp ONLY the raw last-output time, as a cosmetic redraw (a blink)
    /// does, leaving the genuine-output stamp alone.
    #[doc(hidden)]
    pub fn note_raw_output_for_test(&self, uid: &str, ms: u64) {
        if let Some(s) = self.sessions.lock().unwrap().get(uid) {
            s.shared.last_output_at.store(ms, Ordering::Relaxed);
        }
    }

    /// The pane's activity inputs (genuine-output stamp, content cursor, cached
    /// classification), or `None` for an unknown uid.
    pub fn activity_signal(&self, uid: &str) -> Option<ActivitySignal> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| *s.shared.activity.lock().unwrap())
    }

    /// Cache an awaiting-input classification of the screen as it was at `content_cursor`.
    pub fn set_awaiting(&self, uid: &str, content_cursor: u64, awaiting: bool) {
        if let Some(s) = self.sessions.lock().unwrap().get(uid) {
            s.shared.activity.lock().unwrap().awaiting = Some((content_cursor, awaiting));
        }
    }

    /// Serialize the pane's current screen to clean text (for `mode:"screen"` reads).
    /// Brings the lazily-fed screen mirror fully up to date first (see `screen_pending`).
    pub fn render_screen(&self, uid: &str) -> Option<String> {
        self.render_screen_read(uid).map(|r| r.text)
    }

    /// [`render_screen`](Self::render_screen) plus the prompt-box classification
    /// ([`crate::session::screen::InputLine`]), from the same synced grid.
    pub fn render_screen_read(&self, uid: &str) -> Option<crate::session::screen::ScreenRead> {
        // Clone the session handle out and release the sessions lock before parsing: the parse
        // can chew through a large pending-output buffer, and every other pane op (the GUI's
        // status poll included) takes that lock.
        let shared = {
            let map = self.sessions.lock().unwrap();
            Arc::clone(&map.get(uid)?.shared)
        };
        shared.sync_screen();
        let read = shared.screen.lock().unwrap().render_read();
        Some(read)
    }

    /// The pane's screen as a [`ScreenSeed`](crate::session::screen::ScreenSeed) for a
    /// re-attaching view (issue #140), or `None` for an unknown uid.
    pub fn screen_seed(&self, uid: &str) -> Option<crate::session::screen::ScreenSeed> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| s.shared.screen_seed())
    }

    /// Current pty grid `(cols, rows)` — the width a remote client must emulate at.
    pub fn dims(&self, uid: &str) -> Option<(u16, u16)> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| s.shared.screen.lock().unwrap().dims())
    }

    /// The replay buffer + the byte cursor as an ATOMIC pair (both read under the
    /// replay lock, which `flush_into` also holds while bumping the cursor). Remote
    /// clients splice a live `output` frame stream onto this snapshot; a torn pair
    /// would drop or duplicate bytes at the seam.
    pub fn replay_with_cursor(&self, uid: &str) -> Option<(String, u64)> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| {
            let replay = s.shared.replay.lock().unwrap();
            let cursor = s.shared.output_bytes.load(Ordering::Relaxed);
            (replay.get().to_string(), cursor)
        })
    }

    /// Write input to the pane's pty.
    pub fn write(&self, uid: &str, data: &str) {
        let map = self.sessions.lock().unwrap();
        if let Some(s) = map.get(uid) {
            // Phase 4: input was just sent → optimistically clear `prompt_ready` so the
            // busy edge is reported immediately (a real prompt re-asserts it on its `133;A`).
            s.shared.note_write();
            let _ = s.pty.write(data.as_bytes());
        }
    }

    /// Snapshot a session's phase-4 liveness mirror, or `None` if the uid is unknown.
    pub fn liveness(&self, uid: &str) -> Option<Liveness> {
        let map = self.sessions.lock().unwrap();
        map.get(uid).map(|s| s.shared.liveness())
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
            crate::persistence::session_log::record("kill", uid, "registry kill", None);
            self.note_dying(&s);
            let _ = s.pty.kill();
        }
    }

    /// Remember a killed session's leader by pane id, so a respawn of that pane waits for
    /// its processes. A session killed before its own deferred spawn hands on the
    /// predecessor it waited on. Read before the kill: a killed deferred pty has no pid.
    fn note_dying(&self, s: &Session) {
        let Some(pane) = s.pane_id.as_deref() else {
            return;
        };
        let dying = match s.pty.pid() {
            Some(leader) => Dying {
                leader,
                killed_at: Instant::now(),
            },
            None => match s.awaiting {
                Some(d) => d,
                None => return,
            },
        };
        self.dying
            .lock()
            .unwrap()
            .record(pane, dying, self.respawn.bound);
    }

    /// Kill every live pane and clear the map.
    pub fn kill_all(&self) {
        let drained: Vec<(String, Session)> = {
            let mut map = self.sessions.lock().unwrap();
            map.drain().collect()
        };
        for (uid, s) in drained {
            s.shared.killed.store(true, Ordering::SeqCst);
            crate::persistence::session_log::record("kill-all", &uid, "registry kill_all", None);
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
/// Env var carrying the running app's version into every pane spawned through a
/// [`SessionManager`] in this process.
pub const RUNNING_VERSION_ENV: &str = "HYPERPANES_VERSION";

static RUNNING_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the running app's version (the GUI calls this once at startup). Every later
/// [`SessionManager::create`] then stamps it into the spawn env as `HYPERPANES_VERSION`, on
/// every path (new pane, restore, restartPane, recoverPane, supervisor restart). Stamped on the
/// GUI side, not in the daemon's `build_env`: the session daemon can be an older binary than the
/// GUI, and it passes `opts.env` through. A process that never calls this (the daemon itself,
/// tests) stamps nothing. Frozen at pane spawn; `/health` and `/state` stay authoritative.
pub fn set_running_version(version: &str) {
    let _ = RUNNING_VERSION.set(version.to_string());
}

fn stamp_running_version(opts: SpawnOptions) -> SpawnOptions {
    stamp_version(opts, RUNNING_VERSION.get().map(String::as_str))
}

/// Overwrites any recorded value (a restart re-uses the pane's stored env), keeps other keys.
fn stamp_version(mut opts: SpawnOptions, version: Option<&str>) -> SpawnOptions {
    if let Some(v) = version {
        opts.env
            .get_or_insert_with(Default::default)
            .insert(RUNNING_VERSION_ENV.to_string(), v.to_string());
    }
    opts
}

/// Env var marking a pane the app launched to run unattended (no human at the keyboard), so
/// tools inside it (hooks, guards) can refuse instead of raising an interactive prompt nobody
/// answers. Value is always `"1"`; absent for every other pane.
pub const UNATTENDED_ENV: &str = "HYPERPANES_UNATTENDED";

/// Pane meta `role`s that run unattended: impl workers, the goals orchestrator and spec agents
/// (the latter two are also [`crate::control::nudge::NUDGED_ROLES`]).
pub const UNATTENDED_ROLES: &[&str] = &["worker", "goals-orch", "spec"];

/// `(HYPERPANES_UNATTENDED, "1")` when the pane's meta `role` is in [`UNATTENDED_ROLES`].
///
/// Derived from the role on every spawn (new pane, restart, restore, resume) and never
/// persisted (see `env_persist`). A role set by `setMeta` AFTER the pane spawned only takes
/// effect at the pane's next restart.
pub fn unattended_env(
    meta: Option<&std::collections::BTreeMap<String, String>>,
) -> Option<(&'static str, &'static str)> {
    meta?
        .get("role")
        .is_some_and(|r| UNATTENDED_ROLES.contains(&r.as_str()))
        .then_some((UNATTENDED_ENV, "1"))
}

/// Layer [`unattended_env`] onto a spawn env. A non-unattended pane has the key REMOVED, so a
/// stale value carried in a recorded env never survives a role change.
pub fn stamp_unattended(
    env: Option<EnvMap>,
    meta: Option<&std::collections::BTreeMap<String, String>>,
) -> Option<EnvMap> {
    match unattended_env(meta) {
        Some((k, v)) => {
            let mut e = env.unwrap_or_default();
            e.insert(k.to_string(), v.to_string());
            Some(e)
        }
        None => env.map(|mut e| {
            e.remove(UNATTENDED_ENV);
            e
        }),
    }
}

#[cfg(test)]
mod unattended_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn role(r: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("role".to_string(), r.to_string())])
    }

    #[test]
    fn org_roles_are_unattended_others_are_not() {
        for r in ["worker", "goals-orch", "spec"] {
            assert_eq!(
                unattended_env(Some(&role(r))),
                Some((UNATTENDED_ENV, "1")),
                "{r}"
            );
        }
        assert_eq!(unattended_env(Some(&role("lead"))), None);
        assert_eq!(unattended_env(Some(&BTreeMap::new())), None);
        assert_eq!(unattended_env(None), None);
    }

    #[test]
    fn stamp_adds_for_unattended_and_strips_stale_for_others() {
        let e = stamp_unattended(None, Some(&role("worker"))).unwrap();
        assert_eq!(e.get(UNATTENDED_ENV).map(String::as_str), Some("1"));
        let mut base = EnvMap::new();
        base.insert("FOO".into(), "bar".into());
        base.insert(UNATTENDED_ENV.into(), "1".into());
        let e = stamp_unattended(Some(base), Some(&role("lead"))).unwrap();
        assert_eq!(e.get("FOO").map(String::as_str), Some("bar"));
        assert!(!e.contains_key(UNATTENDED_ENV));
        assert!(stamp_unattended(None, None).is_none());
    }
}

#[cfg(test)]
mod running_version_tests {
    use super::*;

    #[test]
    fn stamps_version_over_recorded_env_and_keeps_other_keys() {
        let o = stamp_version(SpawnOptions::default(), Some("1.2.3"));
        assert_eq!(
            o.env.unwrap().get(RUNNING_VERSION_ENV).map(String::as_str),
            Some("1.2.3")
        );
        let mut env = EnvMap::new();
        env.insert("FOO".into(), "bar".into());
        env.insert(RUNNING_VERSION_ENV.into(), "0.0.1".into());
        let o = stamp_version(
            SpawnOptions {
                env: Some(env),
                ..Default::default()
            },
            Some("1.2.3"),
        );
        let e = o.env.unwrap();
        assert_eq!(e.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(
            e.get(RUNNING_VERSION_ENV).map(String::as_str),
            Some("1.2.3")
        );
    }

    #[test]
    fn unset_version_stamps_nothing() {
        let o = stamp_version(SpawnOptions::default(), None);
        assert!(o.env.is_none());
    }
}

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
        let opts = stamp_running_version(opts);
        match self {
            SessionManager::InProcess(r) => r.create(opts),
            SessionManager::Daemon(d) => d.create(opts),
        }
    }

    /// Spawn a session using a custom pty `factory` (tests inject a mock). The daemon
    /// backend ignores the factory (a closure can't cross a socket; the daemon owns real
    /// PTYs) and spawns a normal session — no production caller uses `create_with`.
    pub fn create_with(&self, opts: SpawnOptions, factory: SpawnFn) -> io::Result<()> {
        let opts = stamp_running_version(opts);
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

    /// Test-only hook (in-process backend only): stamp a pane's last-output time (both the raw
    /// stamp and the genuine-output stamp, as real output does).
    #[doc(hidden)]
    pub fn set_last_output_at_for_test(&self, uid: &str, ms: u64) {
        if let SessionManager::InProcess(r) = self {
            r.set_last_output_at_for_test(uid, ms);
        }
    }

    /// Test-only hook (in-process backend only): stamp ONLY the raw last-output time, as a
    /// cosmetic redraw (a blinking indicator) does.
    #[doc(hidden)]
    pub fn note_raw_output_for_test(&self, uid: &str, ms: u64) {
        if let SessionManager::InProcess(r) = self {
            r.note_raw_output_for_test(uid, ms);
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

    /// The pane's screen serialized for a re-attaching view (issue #140): `None` for an unknown
    /// uid, or a daemon that doesn't offer it (an older daemon surviving a GUI upgrade: the
    /// caller falls back to the replay buffer). Against a daemon this is a bounded round-trip.
    pub fn screen_seed(&self, uid: &str) -> Option<crate::session::screen::ScreenSeed> {
        match self {
            SessionManager::InProcess(r) => r.screen_seed(uid),
            SessionManager::Daemon(d) => d.screen_seed(uid),
        }
    }

    /// The screen text plus the prompt box ([`crate::session::screen::InputLine`]). Against a
    /// daemon that predates the field, `input_line` is `None` (the text still comes back).
    pub fn render_screen_read(&self, uid: &str) -> Option<crate::session::screen::ScreenRead> {
        match self {
            SessionManager::InProcess(r) => r.render_screen_read(uid),
            SessionManager::Daemon(d) => d.render_screen_read(uid),
        }
    }

    /// Write input to the pane's pty.
    pub fn write(&self, uid: &str, data: &str) {
        match self {
            SessionManager::InProcess(r) => r.write(uid, data),
            SessionManager::Daemon(d) => d.write(uid, data),
        }
    }

    /// Snapshot a session's phase-4 liveness mirror (OSC-133 / OSC-9;hp), or `None`.
    ///
    /// The in-process backend reads the live mirror. The daemon backend has no shadow
    /// of the marker mirror yet (the marker `SessionEvent`s do flow over the proto, but
    /// the client doesn't fold them into a shadow), so it returns `None` → the activity
    /// ticker keeps using the silence heuristic for daemon-backed panes. STUB: a later
    /// pass can mirror markers into `daemon_client::Shadow` like `last_output_at`.
    pub fn liveness(&self, uid: &str) -> Option<Liveness> {
        match self {
            SessionManager::InProcess(r) => r.liveness(uid),
            SessionManager::Daemon(_) => None,
        }
    }

    /// The pane's activity inputs: when it last produced GENUINE output (cosmetic redraws such
    /// as a blinking indicator excluded, see [`crate::session::activity`]), the output cursor at
    /// that point, and the cached awaiting-input classification. `None` for an unknown uid.
    /// Both backends compute it locally from the `Data` stream (no daemon round-trip).
    pub fn activity_signal(&self, uid: &str) -> Option<ActivitySignal> {
        match self {
            SessionManager::InProcess(r) => r.activity_signal(uid),
            SessionManager::Daemon(d) => d.activity_signal(uid),
        }
    }

    /// Cache an awaiting-input classification of the pane's screen as it was when the content
    /// cursor read `content_cursor`. It stays valid until the next genuine output.
    pub fn set_awaiting(&self, uid: &str, content_cursor: u64, awaiting: bool) {
        match self {
            SessionManager::InProcess(r) => r.set_awaiting(uid, content_cursor, awaiting),
            SessionManager::Daemon(d) => d.set_awaiting(uid, content_cursor, awaiting),
        }
    }

    /// Resize the pane (≥1×1) — both the pty grid and the live screen model.
    pub fn resize(&self, uid: &str, cols: u16, rows: u16) {
        match self {
            SessionManager::InProcess(r) => r.resize(uid, cols, rows),
            SessionManager::Daemon(d) => d.resize(uid, cols, rows),
        }
    }

    /// Current pty grid `(cols, rows)`, or `None` when unknown. Daemon-backed panes
    /// return `None` for now (dims live daemon-side; mirroring them into the shadow is
    /// a follow-up, like `liveness`) — `/state` simply omits cols/rows for them.
    pub fn dims(&self, uid: &str) -> Option<(u16, u16)> {
        match self {
            SessionManager::InProcess(r) => r.dims(uid),
            SessionManager::Daemon(_) => None,
        }
    }

    /// The replay buffer + byte cursor as an ATOMIC pair (see the in-process impl) —
    /// what remote clients seed from before splicing the live `output` frame stream.
    pub fn replay_with_cursor(&self, uid: &str) -> Option<(String, u64)> {
        match self {
            SessionManager::InProcess(r) => r.replay_with_cursor(uid),
            SessionManager::Daemon(d) => d.replay_with_cursor(uid),
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
    let resolved = resolve_spawn(
        &shell,
        opts.command.as_deref(),
        args,
        opts.cwd.as_deref(),
        opts.env.as_ref(),
    );

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
    let resolved_control_file = resolve_control_file(opts.control_file.as_deref());
    let env = build_env(&EnvInputs {
        process_env: &process_env,
        opts_env: opts.env.as_ref(),
        integration_env: &integration_env,
        pane_id: opts.pane_id.as_deref(),
        control_file: resolved_control_file.as_deref(),
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    /// Carry for a phase-4 semantic marker (OSC 133 / OSC 9;hp) split across chunks —
    /// independent of `osc_carry` so the two scanners never clobber each other's tail.
    marker_carry: String,
    /// De-dupe: emit `Cwd` only when the directory actually changes.
    last_cwd: Option<String>,
    /// Carry for an incomplete trailing UTF-8 sequence split across pty reads.
    utf8_carry: Vec<u8>,
    /// Tells genuine flushes from cosmetic redraws (blinks) for the activity stamp.
    gate: OutputGate,
    ended: bool,
    shared: Arc<Shared>,
}

impl SessionPipeline {
    fn new(uid: String, shared: Arc<Shared>) -> Self {
        Self {
            uid,
            batcher: DataBatcher::new(),
            osc_carry: String::new(),
            marker_carry: String::new(),
            last_cwd: None,
            utf8_carry: Vec::new(),
            gate: OutputGate::new(),
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
                out.push(SessionEvent::Cwd {
                    uid: self.uid.clone(),
                    cwd,
                });
            }
        }

        // Phase 4: tap the same RAW chunk for semantic prompt markers (OSC 133 /
        // OSC 9;hp), updating the liveness mirror and emitting per-marker events. The
        // marker bytes are NOT stripped from the batched stream here — like the cwd OSC
        // they are inert escape sequences the terminal grid ignores, so they never render
        // visibly; this keeps the byte cursor / replay buffer faithful to what was sent.
        let (markers, mcarry) = crate::session::osc133::parse_osc_markers(&self.marker_carry, raw);
        self.marker_carry = mcarry;
        for m in &markers {
            self.shared.apply_marker(m);
            out.push(marker_to_event(&self.uid, m));
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
        let killed = self.shared.killed.load(Ordering::SeqCst);
        crate::persistence::session_log::record(
            if killed { "exit-after-kill" } else { "exit" },
            &self.uid,
            &format!("code={code}"),
            None,
        );
        if killed {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(flushed) = self.batcher.flush() {
            self.flush_into(flushed, now_epoch_ms, &mut out);
        }
        out.push(SessionEvent::Exit {
            uid: self.uid.clone(),
            code,
        });
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
        // Bump the cursor while HOLDING the replay lock: `replay_with_cursor` reads the
        // pair under the same lock, so a snapshot can never see one without the other
        // (a torn pair would drop or duplicate bytes on a remote attach splice).
        // The chunk is buffered for the screen under the same lock too, so a screen seed
        // (`Shared::screen_seed`) that reads the cursor always has every byte up to it.
        let cursor = {
            let mut replay = self.shared.replay.lock().unwrap();
            replay.append(&data);
            self.shared
                .screen_pending
                .lock()
                .unwrap()
                .extend_from_slice(data.as_bytes());
            self.shared.output_bytes.fetch_add(n, Ordering::Relaxed) + n
        };
        self.shared
            .last_output_at
            .store(now_epoch_ms, Ordering::Relaxed);
        if self.gate.observe(data.as_bytes(), now_epoch_ms) {
            self.shared
                .activity
                .lock()
                .unwrap()
                .note_genuine(now_epoch_ms, cursor);
        }
        out.push(SessionEvent::Data {
            uid: self.uid.clone(),
            data,
            cursor,
        });
    }
}

/// Map a parsed phase-4 [`Marker`](crate::session::osc133::Marker) to its `SessionEvent`.
fn marker_to_event(uid: &str, m: &crate::session::osc133::Marker) -> SessionEvent {
    use crate::session::osc133::Marker;
    match m {
        Marker::CommandStart => SessionEvent::CommandStart {
            uid: uid.to_string(),
        },
        Marker::CommandEnd { code } => SessionEvent::CommandEnd {
            uid: uid.to_string(),
            code: *code,
        },
        Marker::PromptReady => SessionEvent::PromptReady {
            uid: uid.to_string(),
        },
        Marker::Agent { state, code } => SessionEvent::AgentState {
            uid: uid.to_string(),
            state: (*state).into(),
            code: *code,
        },
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
            activity: Mutex::new(ActivitySignal::default()),
            killed: AtomicBool::new(false),
            prompt_ready: AtomicBool::new(false),
            command_running: AtomicBool::new(false),
            last_exit_code: AtomicI32::new(i32::MIN),
            marker_seen: AtomicBool::new(false),
        })
    }

    // ---- #140: screen seeds ----

    /// A seed taken while output streams in is consistent: its cursor is the end of a whole
    /// chunk (chunks never straddle it, so a re-attaching view can skip `Data` whose cursor is
    /// `<=` it and apply the rest whole), and its screen shows exactly the output up to that
    /// cursor. Regression for buffering the chunk for the screen OUTSIDE the replay lock: a seed
    /// could then count a chunk in its cursor that its screen did not show yet.
    #[test]
    fn a_seed_taken_mid_stream_matches_its_cursor() {
        let sh = shared();
        let writer_sh = Arc::clone(&sh);
        let writer = std::thread::spawn(move || {
            let mut p = SessionPipeline::new("u1".into(), writer_sh);
            let mut ends = Vec::new();
            for i in 0..4000 {
                let mut out = Vec::new();
                p.flush_into(format!("\r\x1b[Kn={i}"), 0, &mut out);
                if let Some(SessionEvent::Data { cursor, .. }) = out.first() {
                    ends.push(*cursor);
                }
            }
            ends
        });
        let mut seeds = Vec::new();
        while !writer.is_finished() {
            seeds.push(sh.screen_seed());
        }
        let ends = writer.join().unwrap();
        seeds.push(sh.screen_seed());
        assert!(seeds.len() > 1);
        for seed in &seeds {
            let mut rebuilt = Screen::new(seed.cols, seed.rows);
            rebuilt.advance(seed.vt.as_bytes());
            let text = rebuilt.render();
            if seed.cursor == 0 {
                assert_eq!(text, "");
                continue;
            }
            let chunk = ends
                .iter()
                .position(|&end| end == seed.cursor)
                .unwrap_or_else(|| panic!("seed cursor {} is not a chunk end", seed.cursor));
            assert_eq!(text, format!("n={chunk}"), "seed at cursor {}", seed.cursor);
        }
        assert_eq!(seeds.last().unwrap().cursor, *ends.last().unwrap());
    }

    /// Deterministic regression for the ordering above: while a chunk is not yet buffered for
    /// the screen, its cursor must not be visible (the cursor is bumped only after the chunk is
    /// in `screen_pending`, both under the replay lock). With the old order the cursor was
    /// bumped first, so a seed could read it and serialize a screen without that chunk.
    #[test]
    fn a_chunk_is_buffered_for_the_screen_before_its_cursor_shows() {
        let sh = shared();
        let held = sh.screen_pending.lock().unwrap();
        let writer_sh = Arc::clone(&sh);
        let writer = std::thread::spawn(move || {
            let mut p = SessionPipeline::new("u1".into(), writer_sh);
            p.flush_into("chunk".into(), 0, &mut Vec::new());
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            sh.output_bytes.load(Ordering::Relaxed),
            0,
            "the cursor moved before the chunk reached the screen buffer"
        );
        drop(held);
        writer.join().unwrap();
        assert_eq!(sh.output_bytes.load(Ordering::Relaxed), 5);
        assert_eq!(sh.screen_seed().cursor, 5);
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
        assert_eq!(
            evs[0],
            SessionEvent::Cwd {
                uid: "u1".into(),
                cwd: "C:\\proj".into()
            }
        );
        // Same cwd again → no Cwd event (the prompt re-emits its OSC each keystroke).
        let evs2 = p.on_data(seq, 1, 1001);
        assert!(!evs2.iter().any(|e| matches!(e, SessionEvent::Cwd { .. })));
    }

    // ---- pipeline: phase-4 marker sniff + liveness mirror ----

    #[test]
    fn pipeline_sniffs_osc133_markers_and_updates_the_liveness_mirror() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        // Before any marker the mirror reports marker_seen=false (silence heuristic owns it).
        assert!(!sh.liveness().marker_seen);

        // A command starts running: 133;C → CommandStart event + command_running mirror.
        let evs = p.on_data("\u{1b}]133;C\u{07}", 0, 1000);
        assert!(evs
            .iter()
            .any(|e| matches!(e, SessionEvent::CommandStart { .. })));
        let l = sh.liveness();
        assert!(l.marker_seen && l.command_running && !l.prompt_ready);

        // The command finishes with code 0, then the prompt returns: 133;D;0 then 133;A.
        let evs2 = p.on_data("\u{1b}]133;D;0\u{07}\u{1b}]133;A\u{07}", 1, 1001);
        assert!(evs2
            .iter()
            .any(|e| matches!(e, SessionEvent::CommandEnd { code: Some(0), .. })));
        assert!(evs2
            .iter()
            .any(|e| matches!(e, SessionEvent::PromptReady { .. })));
        let l2 = sh.liveness();
        assert!(!l2.command_running && l2.prompt_ready);
        assert_eq!(l2.last_exit_code, Some(0));
    }

    #[test]
    fn pipeline_sniffs_agent_liveness_marker() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let evs = p.on_data("\u{1b}]9;hp;state=busy\u{07}", 0, 1000);
        assert!(evs.iter().any(|e| matches!(
            e,
            SessionEvent::AgentState {
                state: AgentLiveness::Busy,
                ..
            }
        )));
        assert!(sh.liveness().command_running);
        // awaiting-input flips the mirror to prompt_ready.
        p.on_data("\u{1b}]9;hp;state=awaiting-input\u{07}", 1, 1001);
        let l = sh.liveness();
        assert!(l.prompt_ready && !l.command_running);
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
        assert_eq!(
            evs,
            vec![SessionEvent::Data {
                uid: "u1".into(),
                data: "abcde".into(),
                cursor: 5,
            }]
        );
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
        assert_eq!(
            evs,
            vec![SessionEvent::Data {
                uid: "u1".into(),
                data: big.clone(),
                cursor: (BATCH_MAX_SIZE - 1) as u64,
            }]
        );
        // The new chunk remains buffered until its own flush.
        let evs2 = p.on_timer(120);
        assert_eq!(
            evs2,
            vec![SessionEvent::Data {
                uid: "u1".into(),
                data: "yy".into(),
                cursor: (BATCH_MAX_SIZE + 1) as u64,
            }]
        );
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
                SessionEvent::Data {
                    uid: "u1".into(),
                    data: "tail".into(),
                    cursor: 4,
                },
                SessionEvent::Exit {
                    uid: "u1".into(),
                    code: 0
                },
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

    // ---- activity gate wiring: cosmetic redraws never stamp the genuine-output time ----

    // A tool bullet blinking under a permission dialog (recorded byte shape).
    const BLINK: [&str; 2] = [
        "\x1b[H\r\x1b[53B\x1b[38;2;153;153;153m\u{25cf}\x1b[39m\x1b[81;1H\x1b[77;2H",
        "\x1b[H\r\x1b[53B\x1b[38;2;153;153;153m \x1b[39m\x1b[81;1H\x1b[77;2H",
    ];

    fn spinner_frame(i: usize) -> String {
        let glyphs = [
            "\u{b7}", "\u{2722}", "\u{2733}", "\u{2736}", "\u{273b}", "\u{273d}",
        ];
        format!(
            "\x1b[H\r\x1b[20B\x1b[38;2;215;119;87m{}\x1b[39m Working\u{2026} ({}s)\x1b[24;3H",
            glyphs[i % glyphs.len()],
            i / 10
        )
    }

    #[test]
    fn pipeline_blink_frames_do_not_advance_the_genuine_stamp() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let mut t = 1_000u64;
        for i in 0..20 {
            p.on_data(BLINK[i % 2], t, t);
            p.on_timer(t + 20);
            t += 500;
        }
        let a = *sh.activity.lock().unwrap();
        // Only the first two frames were new; the stamp stopped at the second one's flush.
        assert_eq!(a.content_at, Some(1_520));
        assert!(a.content_cursor < sh.output_bytes.load(Ordering::Relaxed));
        // The raw stamp kept moving with every frame.
        assert_eq!(sh.last_output_at.load(Ordering::Relaxed), t - 500 + 20);
    }

    #[test]
    fn pipeline_spinner_frames_keep_stamping() {
        let sh = shared();
        let mut p = SessionPipeline::new("u1".into(), Arc::clone(&sh));
        let mut t = 1_000u64;
        for i in 0..30 {
            p.on_data(&spinner_frame(i), t, t);
            p.on_timer(t + 20);
            t += 100;
        }
        let a = *sh.activity.lock().unwrap();
        assert_eq!(a.content_at, Some(t - 100 + 20));
        assert_eq!(a.content_cursor, sh.output_bytes.load(Ordering::Relaxed));
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
        mgr.create_with(
            SpawnOptions {
                uid: uid.into(),
                ..Default::default()
            },
            factory,
        )
        .expect("create");
        let sink = slot.lock().unwrap().clone().expect("sink captured");
        (mgr, erx, sink)
    }

    async fn recv(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    ) -> Option<SessionEvent> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn manager_streams_data_then_removes_on_natural_exit() {
        let (mgr, mut rx, sink) = make_session("u1");
        assert!(mgr.has("u1"));

        sink(PtyEvent::Data(b"hello".to_vec()));
        // Flushed by the 16 ms batch timer.
        assert_eq!(
            recv(&mut rx).await,
            Some(SessionEvent::Data {
                uid: "u1".into(),
                data: "hello".into(),
                cursor: 5,
            })
        );
        assert_eq!(mgr.output_bytes("u1"), Some(5));
        assert_eq!(mgr.replay("u1").as_deref(), Some("hello"));
        assert!(mgr.last_output_at("u1").is_some());

        sink(PtyEvent::Exit(0));
        assert_eq!(
            recv(&mut rx).await,
            Some(SessionEvent::Exit {
                uid: "u1".into(),
                code: 0
            })
        );

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
        assert_eq!(
            recv(&mut rx).await,
            Some(SessionEvent::Cwd {
                uid: "u1".into(),
                cwd: "C:\\work".into()
            })
        );
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
        assert_eq!(
            sh.screen.lock().unwrap().render(),
            "",
            "screen is lazy: empty before sync"
        );
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
        assert!(matches!(
            recv(&mut rx).await,
            Some(SessionEvent::Data { .. })
        ));
        // render_screen must lazily sync the buffered output before serializing.
        assert_eq!(mgr.render_screen("u1").as_deref(), Some("abc\ndef"));
    }

    // Push one chunk through the mock pty and wait for its flush, so every chunk is its own
    // `Data` batch (as distinct writes 50+ ms apart are in a real pane).
    async fn feed(
        sink: &EventSink,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
        bytes: &str,
    ) {
        sink(PtyEvent::Data(bytes.as_bytes().to_vec()));
        assert!(matches!(recv(rx).await, Some(SessionEvent::Data { .. })));
    }

    // Idle threshold for the timing tests below (ms): short, so the tests stay fast.
    const QUICK_IDLE_MS: i64 = 300;

    #[tokio::test]
    async fn blink_only_pane_goes_idle_despite_continuous_output() {
        use crate::control::readmodel::{Activity, PaneStatus};
        use crate::control::server::{activity_for, now_ms};
        let (mgr, mut rx, sink) = make_session("u1");
        feed(&sink, &mut rx, "Running the build script\r\n").await;
        // ~1 s of nothing but the blink: far past the threshold, with fresh raw output.
        for i in 0..20 {
            feed(&sink, &mut rx, BLINK[i % 2]).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let raw_age = now_ms() - mgr.last_output_at("u1").unwrap() as i64;
        assert!(
            raw_age < QUICK_IDLE_MS,
            "the blink keeps raw output fresh ({raw_age} ms)"
        );
        assert_eq!(
            activity_for(&mgr, QUICK_IDLE_MS, "u1", PaneStatus::Running),
            Activity::Idle
        );
    }

    #[tokio::test]
    async fn spinner_with_a_changing_timer_keeps_the_pane_busy() {
        use crate::control::readmodel::{Activity, PaneStatus};
        use crate::control::server::activity_for;
        let (mgr, mut rx, sink) = make_session("u1");
        for i in 0..20 {
            feed(&sink, &mut rx, &spinner_frame(i)).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            activity_for(&mgr, QUICK_IDLE_MS, "u1", PaneStatus::Running),
            Activity::Busy
        );
    }

    // A permission dialog on an 80x24 screen (synthetic content), and the tool bullet on its
    // row 2 blinking while the dialog waits.
    const DIALOG: &str = concat!(
        "\x1b[2J\x1b[H",
        "> run the build\r\n",
        "\u{25cf} Running the build script\r\n",
        "\r\n",
        " Bash command\r\n",
        "   ./build.sh\r\n",
        "\r\n",
        " Do you want to proceed?\r\n",
        " \u{276f} 1. Yes\r\n",
        "   2. No\r\n",
        "\r\n",
        " Esc to cancel \u{b7} Tab to amend\r\n",
        "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} status \u{2500}\u{2500}",
    );
    const BULLET: [&str; 2] = [
        "\x1b[2;1H\x1b[38;2;153;153;153m\u{25cf}\x1b[39m\x1b[12;1H",
        "\x1b[2;1H\x1b[38;2;153;153;153m \x1b[39m\x1b[12;1H",
    ];

    #[tokio::test]
    async fn permission_dialog_reads_awaiting_input_until_the_screen_changes() {
        use crate::control::readmodel::{Activity, PaneStatus};
        use crate::control::server::{activity_for, classify_screen};
        let act = |m: &SessionManager| activity_for(m, 60_000, "u1", PaneStatus::Running);
        let (mgr, mut rx, sink) = make_session("u1");
        feed(&sink, &mut rx, DIALOG).await;
        // The bullet starts blinking: two new frames, then repeats.
        for i in 0..4 {
            feed(&sink, &mut rx, BULLET[i % 2]).await;
        }
        // Fresh output, not classified yet: busy (inside the 60 s window).
        assert_eq!(act(&mgr), Activity::Busy);
        assert_eq!(classify_screen(&mgr, "u1"), Some(true));
        assert_eq!(act(&mgr), Activity::AwaitingInput);
        // More blinking is cosmetic: the verdict holds.
        for i in 0..6 {
            feed(&sink, &mut rx, BULLET[i % 2]).await;
        }
        assert_eq!(act(&mgr), Activity::AwaitingInput);
        // The dialog is answered and real output follows: busy again, verdict stale.
        feed(&sink, &mut rx, "\x1b[2J\x1b[H\u{25cf} Build finished\r\n").await;
        assert_eq!(act(&mgr), Activity::Busy);
        assert!(mgr.activity_signal("u1").unwrap().classification_stale());
        // Re-classifying the new screen finds no prompt.
        assert_eq!(classify_screen(&mgr, "u1"), Some(false));
        assert_eq!(act(&mgr), Activity::Busy);
    }

    // The inbox nudge's typing guard: a dialog on screen blocks it however old the dialog is
    // (the pane itself may read idle); a shell prompt does not.
    #[tokio::test]
    async fn nudge_guard_blocks_on_a_dialog_screen_only() {
        use crate::control::readmodel::{Activity, PaneStatus};
        use crate::control::server::{activity_for, screen_blocks_typing};
        let (mgr, mut rx, sink) = make_session("u1");
        feed(&sink, &mut rx, DIALOG).await;
        mgr.set_last_output_at_for_test("u1", 1);
        assert_eq!(
            activity_for(&mgr, 10_000, "u1", PaneStatus::Running),
            Activity::Idle
        );
        assert!(screen_blocks_typing(&mgr, "u1"));
        // The render refreshed the cached verdict too: the pane now reads awaiting input.
        assert_eq!(
            activity_for(&mgr, 10_000, "u1", PaneStatus::Running),
            Activity::AwaitingInput
        );
        feed(&sink, &mut rx, "\x1b[2J\x1b[H$ \r\n").await;
        assert!(!screen_blocks_typing(&mgr, "u1"));
        // A gone pane is not blocked (nothing typed reaches it).
        assert!(!screen_blocks_typing(&mgr, "nope"));
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
        assert!(
            !mgr.is_daemon(),
            "the in-process backend reports is_daemon() == false"
        );
    }

    // shutdown_daemon is INERT for the in-process backend (session-daemon M3): there is no
    // out-of-process daemon to stop, so it returns false and does nothing — the quit path
    // distinguishes this from a daemon shutdown by the bool.
    #[test]
    fn in_process_shutdown_daemon_is_inert() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        assert!(
            !mgr.shutdown_daemon(),
            "in-process shutdown_daemon is a no-op returning false"
        );
    }

    #[test]
    fn in_process_fresh_uid_is_pane_n_and_unique() {
        let (etx, _erx) = unbounded_channel::<SessionEvent>();
        let mgr = SessionManager::new(etx);
        // The in-process scheme is the readable `pane-N` (PTYs die with the GUI, so per-run
        // uniqueness suffices); the counter is process-global so two managers never alias.
        let a = mgr.fresh_uid();
        let b = mgr.fresh_uid();
        assert!(
            a.starts_with("pane-"),
            "in-process fresh_uid is pane-N, got {a}"
        );
        assert_ne!(a, b, "successive fresh_uids are unique");
        let (etx2, _erx2) = unbounded_channel::<SessionEvent>();
        let mgr2 = SessionManager::new(etx2);
        // A SECOND manager shares the process-global counter — no cross-manager `pane-0`
        // collision (the historical multi-window clobber this counter was hardened against).
        assert_ne!(
            mgr2.fresh_uid(),
            a,
            "the counter is process-global across managers"
        );
    }

    // ---- a respawn waits for the pane's previous session (session::respawn) ----

    /// A mock pty with an OS pid that records what it is sent.
    struct PidPty {
        pid: u32,
        input: Arc<Mutex<Vec<u8>>>,
        size: Arc<Mutex<Option<(u16, u16)>>>,
    }
    impl Pty for PidPty {
        fn write(&self, data: &[u8]) -> io::Result<()> {
            self.input.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            *self.size.lock().unwrap() = Some((cols, rows));
            Ok(())
        }
        fn kill(&self) -> io::Result<()> {
            Ok(())
        }
        fn pid(&self) -> Option<u32> {
            Some(self.pid)
        }
    }

    /// Counts the spawns of one session and keeps what its pty is sent. It holds the event
    /// sink as a real pty's reader thread does: once every sender is gone, the driver task
    /// ends and drops the session.
    #[derive(Clone, Default)]
    struct Spawns {
        count: Arc<AtomicU64>,
        input: Arc<Mutex<Vec<u8>>>,
        size: Arc<Mutex<Option<(u16, u16)>>>,
        sinks: Arc<Mutex<Vec<EventSink>>>,
    }
    impl Spawns {
        fn factory(&self, pid: u32) -> SpawnFn {
            let me = self.clone();
            Box::new(move |_spec, sink| {
                me.sinks.lock().unwrap().push(sink);
                me.count.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(PidPty {
                    pid,
                    input: Arc::clone(&me.input),
                    size: Arc::clone(&me.size),
                }) as Box<dyn Pty>)
            })
        }
        fn count(&self) -> u64 {
            self.count.load(Ordering::SeqCst)
        }
    }

    fn pane_opts(uid: &str, pane: Option<&str>) -> SpawnOptions {
        SpawnOptions {
            uid: uid.into(),
            pane_id: pane.map(str::to_string),
            ..Default::default()
        }
    }

    /// The pid of every old session these tests kill.
    const OLD: u32 = 4100;

    /// A registry whose respawn probe reports [`OLD`]'s session live while the flag holds,
    /// and its event receiver (held so the driver tasks' channel stays open).
    fn respawn_registry(
        bound_ms: u64,
    ) -> (
        SessionRegistry,
        Arc<AtomicBool>,
        UnboundedReceiver<SessionEvent>,
    ) {
        let (etx, erx) = unbounded_channel::<SessionEvent>();
        let alive = Arc::new(AtomicBool::new(true));
        let probe_alive = Arc::clone(&alive);
        let policy = RespawnPolicy {
            probe: Arc::new(move |pid| pid == OLD && probe_alive.load(Ordering::SeqCst)),
            bound: Duration::from_millis(bound_ms),
            poll: Duration::from_millis(5),
        };
        (
            SessionRegistry::with_respawn_policy(etx, policy),
            alive,
            erx,
        )
    }

    async fn until(what: &str, check: impl Fn() -> bool) {
        for _ in 0..400 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn respawn_waits_for_the_old_session_then_spawns_with_what_the_pane_got_meanwhile() {
        let (reg, alive, _events) = respawn_registry(5_000);
        reg.create_with(pane_opts("s1", Some("p1")), Spawns::default().factory(OLD))
            .unwrap();
        reg.kill("s1");
        let new = Spawns::default();
        reg.create_with(pane_opts("s2", Some("p1")), new.factory(4200))
            .unwrap();
        assert!(reg.has("s2"), "the session exists while its spawn waits");
        reg.write("s2", "typed early\r");
        reg.resize("s2", 120, 40);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(new.count(), 0, "no spawn while the old session runs");

        alive.store(false, Ordering::SeqCst);
        until("the deferred spawn", || new.count() == 1).await;
        assert_eq!(new.input.lock().unwrap().as_slice(), b"typed early\r");
        assert_eq!(*new.size.lock().unwrap(), Some((120, 40)));
        reg.write("s2", "later");
        assert_eq!(new.input.lock().unwrap().as_slice(), b"typed early\rlater");
    }

    #[tokio::test]
    async fn respawn_spawns_at_the_bound_when_the_old_session_lingers() {
        let (reg, _alive, _events) = respawn_registry(80);
        reg.create_with(pane_opts("s1", Some("p1")), Spawns::default().factory(OLD))
            .unwrap();
        let killed = Instant::now();
        reg.kill("s1");
        let new = Spawns::default();
        reg.create_with(pane_opts("s2", Some("p1")), new.factory(4200))
            .unwrap();
        assert_eq!(new.count(), 0);
        until("the spawn at the bound", || new.count() == 1).await;
        assert!(killed.elapsed() >= Duration::from_millis(80));
    }

    #[tokio::test]
    async fn a_kill_during_the_respawn_wait_cancels_that_spawn_and_hands_the_wait_on() {
        let (reg, alive, _events) = respawn_registry(5_000);
        reg.create_with(pane_opts("s1", Some("p1")), Spawns::default().factory(OLD))
            .unwrap();
        reg.kill("s1");
        let cancelled = Spawns::default();
        reg.create_with(pane_opts("s2", Some("p1")), cancelled.factory(4200))
            .unwrap();
        reg.kill("s2");
        assert!(!reg.has("s2"));
        let third = Spawns::default();
        reg.create_with(pane_opts("s3", Some("p1")), third.factory(4300))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            third.count(),
            0,
            "the next respawn still waits on the old session"
        );

        alive.store(false, Ordering::SeqCst);
        until("the third spawn", || third.count() == 1).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            cancelled.count(),
            0,
            "a killed deferred session never spawns"
        );
    }

    #[tokio::test]
    async fn respawn_is_immediate_for_a_gone_old_session_another_pane_or_no_pane() {
        let (reg, alive, _events) = respawn_registry(5_000);
        // Another pane, or no pane id, never waits on p1's old session.
        reg.create_with(pane_opts("s1", Some("p1")), Spawns::default().factory(OLD))
            .unwrap();
        reg.kill("s1");
        for (uid, pane) in [("s2", Some("p2")), ("s3", None)] {
            let other = Spawns::default();
            reg.create_with(pane_opts(uid, pane), other.factory(4200))
                .unwrap();
            assert_eq!(other.count(), 1, "{uid} spawned at once");
        }
        // kill_all records nothing to wait on.
        reg.create_with(pane_opts("s4", Some("p3")), Spawns::default().factory(OLD))
            .unwrap();
        reg.kill_all();
        let after_all = Spawns::default();
        reg.create_with(pane_opts("s5", Some("p3")), after_all.factory(4200))
            .unwrap();
        assert_eq!(after_all.count(), 1);
        // Once the old session is gone, its pane respawns at once.
        alive.store(false, Ordering::SeqCst);
        reg.kill("s5");
        reg.create_with(pane_opts("s6", Some("p1")), Spawns::default().factory(OLD))
            .unwrap();
        let gone = Spawns::default();
        reg.kill("s6");
        reg.create_with(pane_opts("s7", Some("p1")), gone.factory(4200))
            .unwrap();
        assert_eq!(gone.count(), 1);
    }
}
