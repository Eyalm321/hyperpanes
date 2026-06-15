//! **`DaemonSessionManager`** (`docs/session-daemon-plan.md` M1) — a daemon-backed
//! implementation of the exact [`SessionManager`](crate::session_manager::SessionManager)
//! surface. Where the in-process manager owns the PTYs directly, this owns a socket to the
//! [session daemon](crate::session::daemon) (which owns the PTYs) and presents the same
//! create/write/resize/kill/replay/render_screen/… API so the GUI's `Arc<SessionManager>`
//! and every call site are untouched (the backend is chosen behind
//! `HYPERPANES_SESSION_DAEMON=1` — see [`SessionManager::new_daemon`]).
//!
//! ## Keeping the synchronous API non-blocking
//! The plan's "Keeping `SessionManager`'s synchronous API non-blocking" table is the spec
//! for *how* each method avoids a blocking socket round-trip on the hot path:
//!
//! | Method | Strategy here |
//! | --- | --- |
//! | `has` / `uids` | **Client shadow** — a `HashMap<uid, Shadow>` seeded by `ListSessions` on connect, then maintained from the `Exit` event stream (+ the local `create`). |
//! | `output_bytes` / `last_output_at` / cwd | **Client shadow** — every `Data`/`Cwd` event the reader sees updates the shadow; reads are a plain map lookup. |
//! | `replay(uid)` | **Client mirror buffer** — a per-uid rolling [`Replay`] grown by `Data` events → a local return, no round-trip. Seeded ONCE from the `Attach` reply on (re)connect so a survivor's history is restored. |
//! | `render_screen(uid)` | **Bounded request/response** (`RenderScreen` → `Screen`). Off the hot path (control-API screen reads only), so a short blocking round-trip is fine. |
//! | `create` / `write` / `resize` / `kill` / `kill_all` | **Fire-and-forget** request (no reply awaited). |
//!
//! Net: the GUI tick/render loop and every shadow read are pure in-memory map lookups; the
//! only blocking I/O is the rare `render_screen` and the one-time reconnect `Attach`.
//!
//! ## uid ownership
//! The GUI passes its own `uid` in [`SpawnOptions`], so `create` PINS it in the wire
//! [`SpawnSpec`] (the daemon honors a pinned uid). That keeps the shadow + mirror keyed
//! immediately and the uid stable across this manager's lifetime — the GUI never has to
//! wait for the daemon's `Created` reply to know its uid. (The daemon's mint-a-uid path is
//! for clients that leave `uid: None`; the GUI doesn't.)
//!
//! ## Reader thread
//! One background thread owns the read half of the socket. It demultiplexes inbound
//! [`DaemonMsg`]s: streamed `Event`s update the shadow + mirror and are forwarded verbatim
//! to the GUI's existing `UnboundedSender<SessionEvent>` (so the renderer is fed exactly as
//! the in-process path feeds it); request/response replies (`Sessions` / `Replay` /
//! `Screen` / `Hello` / `Pong` / `Created`) go to a reply channel a waiting caller drains.
//!
//! Unix-only in M1 (the daemon transport is a UDS; Windows named pipes are M3). The
//! non-unix build provides a stub `new`/`new_connected` that errors, so the enum compiles.

#[cfg(unix)]
use std::collections::HashMap;
use std::io;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::mpsc::{Receiver, Sender};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use tokio::sync::mpsc::UnboundedSender;

#[cfg(unix)]
use crate::session::proto::{
    read_frame, write_frame, ClientMsg, DaemonMsg, SpawnSpec, PROTO_VER,
};
#[cfg(unix)]
use crate::session::replay::Replay;
#[cfg(unix)]
use crate::session_manager::{SessionEvent, SpawnOptions};

/// How long [`render_screen`](DaemonSessionManager::render_screen) waits for the daemon's
/// `Screen` reply before giving up (returning `None`). Generous — a screen serialize is
/// cheap daemon-side — but bounded so a wedged daemon can't hang a control read forever.
#[cfg(unix)]
const SCREEN_TIMEOUT: Duration = Duration::from_secs(2);

/// Total time [`connect_or_spawn`] will keep retrying the connect after spawning the
/// daemon, before giving up. The daemon binds its socket within a few ms of launch, but a
/// cold `current_exe` start (plus the Tokio runtime build) can take longer, so we allow a
/// comfortable margin with exponential-ish backoff.
#[cfg(unix)]
const SPAWN_CONNECT_BUDGET: Duration = Duration::from_secs(5);

/// Per-uid client-side shadow of a session's read-path state (the plan's "client shadow"):
/// what `has`/`uids`/`output_bytes`/`last_output_at`/cwd are answered from, plus the
/// `replay` mirror buffer. All maintained from the event stream so reads never touch I/O.
#[cfg(unix)]
struct Shadow {
    /// Rolling mirror of recent output, grown by `Data` events and seeded once from the
    /// `Attach` reply — the local source for `replay()` (no round-trip).
    replay: Replay,
    /// Monotonic UTF-16 output cursor, mirroring `SessionRegistry::output_bytes`.
    output_bytes: u64,
    /// Epoch-ms of the last `Data` flush, or `None` if nothing seen yet.
    last_output_at: Option<u64>,
    /// Last sniffed cwd (from `Cwd` events), if any.
    cwd: Option<String>,
}

#[cfg(unix)]
impl Shadow {
    fn new() -> Self {
        Self { replay: Replay::new(), output_bytes: 0, last_output_at: None, cwd: None }
    }
}

/// A daemon-backed [`SessionManager`](crate::session_manager::SessionManager): same API,
/// but the PTYs live in the [session daemon](crate::session::daemon) so they survive a GUI
/// crash. Owns one socket (a `Mutex`'d write half for requests) plus a reader thread that
/// maintains the shadow/mirror and forwards events to the GUI channel.
#[cfg(unix)]
pub struct DaemonSessionManager {
    /// The write half of the socket, serialized so concurrent `write`/`resize`/… frames
    /// from different threads never interleave on the wire.
    write_half: Mutex<std::os::unix::net::UnixStream>,
    /// The per-uid shadow + replay mirror — read by every hot-path accessor, written by the
    /// reader thread (events) and `create` (immediate insert).
    shadows: Arc<Mutex<HashMap<String, Shadow>>>,
    /// Reply channel for request/response messages (`Sessions`/`Replay`/`Screen`/`Hello`/
    /// `Pong`). Held behind a `Mutex` so the whole manager stays `Sync` (a bare
    /// `mpsc::Receiver` is `Send` but `!Sync`, and the axum control server shares the
    /// manager as `Arc<…>: Sync`). The lock doubles as the round-trip serializer — only one
    /// request/response is in flight at a time — which is fine: replies are all rare and off
    /// the hot path (only `render_screen` and the connect-time `ListSessions`/handshake).
    replies: Mutex<Receiver<DaemonMsg>>,
    _reader: std::thread::JoinHandle<()>,
}

#[cfg(unix)]
impl DaemonSessionManager {
    /// Connect to the daemon serving `salt`, spawning it (detached) if none is listening,
    /// then start the reader thread and seed the shadow from `ListSessions`. Streamed
    /// events are forwarded to `events` (the GUI's existing channel). The salt is the
    /// user-data dir, exactly as the GUI's single-instance gate and the daemon's own
    /// discovery use it.
    ///
    /// **Proto-version handshake (M3):** before building the manager, this does a bare
    /// `Hello` round-trip on the raw socket and compares the daemon's `proto_ver` against the
    /// client's [`PROTO_VER`]. On a MISMATCH the running daemon is a stale build of OUR binary
    /// (lock-step upgrades — no third-party compat burden), so we tear it down
    /// ([`tear_down_stale_daemon`]) and respawn a fresh one, then retry. Bounded retries
    /// guard against a wedged respawn loop.
    pub fn new(events: UnboundedSender<SessionEvent>, salt: &str) -> io::Result<Self> {
        let socket = crate::session::daemon::socket_path_for(salt);
        // Up to a couple of respawn rounds: a single mismatch should resolve in one
        // tear-down + respawn; more than that means something is wrong (e.g. two GUIs of
        // different versions fighting), and we just proceed with whatever answers last.
        for attempt in 0..3 {
            let stream = connect_or_spawn(&socket, salt)?;
            match probe_proto_version(&stream)? {
                ProtoCheck::Match => return Self::from_stream(stream, events),
                ProtoCheck::Mismatch { daemon_ver } => {
                    // The daemon is a stale version of our own binary. Tear it down (Shutdown
                    // → wait for the socket to drop) and respawn a fresh one on the next loop.
                    dbg(&format!(
                        "daemon proto-version mismatch (client {PROTO_VER}, daemon {daemon_ver}); \
                         tearing down stale daemon (attempt {attempt})"
                    ));
                    drop(stream);
                    tear_down_stale_daemon(&socket, salt);
                    // Loop: connect_or_spawn will start a fresh daemon if none is now up.
                }
            }
        }
        // Last resort after exhausting respawns: connect and proceed regardless of version, so
        // a transient mismatch never hard-blocks launch (the GUI still falls back to in-process
        // upstream if even this errors).
        let stream = connect_or_spawn(&socket, salt)?;
        Self::from_stream(stream, events)
    }

    /// Build a manager over an already-connected socket — the seam tests use with an
    /// in-process daemon on a temp socket (no spawn/discovery). Sends the `Hello`
    /// handshake, starts the reader, and seeds the shadow from a `ListSessions`.
    pub fn from_stream(
        stream: std::os::unix::net::UnixStream,
        events: UnboundedSender<SessionEvent>,
    ) -> io::Result<Self> {
        let read_half = stream.try_clone()?;
        let write_half = stream;

        let shadows: Arc<Mutex<HashMap<String, Shadow>>> = Arc::default();
        let (reply_tx, replies) = std::sync::mpsc::channel::<DaemonMsg>();

        // Reader thread: demux inbound frames. Events maintain the shadow + mirror and are
        // forwarded to the GUI channel; replies go to the reply channel.
        let shadows_r = Arc::clone(&shadows);
        let reader = std::thread::Builder::new()
            .name("hp-daemon-sm-reader".into())
            .spawn(move || reader_loop(read_half, shadows_r, events, reply_tx))?;

        let mgr = DaemonSessionManager {
            write_half: Mutex::new(write_half),
            shadows,
            replies: Mutex::new(replies),
            _reader: reader,
        };

        // Handshake (M1 transports the version; M3 enforces it) — drains the `Hello` reply
        // so it doesn't sit in front of a later request/response.
        mgr.send(&ClientMsg::Hello { proto_ver: PROTO_VER })?;
        let _ = mgr.request(ClientMsg::Hello { proto_ver: PROTO_VER }, |m| {
            matches!(m, DaemonMsg::Hello { .. })
        });

        // Seed the shadow from the daemon's live session set (the "+ one `ListSessions` on
        // connect" half of the has/uids strategy) AND re-attach each survivor so its replay
        // mirror is re-seeded from the daemon's retained buffer (a fresh manager on the same
        // salt — e.g. after a GUI restart — picks the survivors back up). M2 drives the
        // visual re-host on top of this; here we just make the shadow + mirror correct.
        mgr.seed_from_daemon();
        Ok(mgr)
    }

    /// Send one request frame (fire-and-forget at this layer). Used directly for the
    /// no-reply mutators; [`request`](Self::request) wraps it for round-trips.
    fn send(&self, msg: &ClientMsg) -> io::Result<()> {
        let mut w = self.write_half.lock().unwrap();
        write_frame(&mut *w, msg)
    }

    /// Send a request and block (holding the reply-channel lock, which serializes
    /// round-trips) for the first reply matching `want`, up to [`SCREEN_TIMEOUT`]. Streamed
    /// events never reach this channel (the reader routes them elsewhere), so the only
    /// traffic here is replies; `want` still guards against an out-of-order reply from a
    /// prior timed-out round-trip whose answer arrived late.
    fn request(&self, msg: ClientMsg, want: impl Fn(&DaemonMsg) -> bool) -> Option<DaemonMsg> {
        // Holding the receiver lock for the whole round-trip both makes the channel a
        // single consumer at a time and serializes overlapping requests onto one wire turn.
        let replies = self.replies.lock().unwrap();
        // Drain any stale reply left from a prior, timed-out round-trip so it can't be
        // mistaken for this one's answer.
        while replies.try_recv().is_ok() {}
        if self.send(&msg).is_err() {
            return None;
        }
        let deadline = Instant::now() + SCREEN_TIMEOUT;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match replies.recv_timeout(remaining) {
                Ok(m) if want(&m) => return Some(m),
                Ok(_) => continue, // not our reply kind — keep waiting
                Err(_) => return None, // timeout or disconnect
            }
        }
    }

    /// `ListSessions` → insert a shadow for every live uid (preserving any existing mirror),
    /// then `Attach` each so its replay mirror is (re)seeded from the daemon's buffer. Run
    /// once at connect; safe to call again (idempotent per uid).
    fn seed_from_daemon(&self) {
        let Some(DaemonMsg::Sessions(metas)) =
            self.request(ClientMsg::ListSessions, |m| matches!(m, DaemonMsg::Sessions(_)))
        else {
            return;
        };
        {
            let mut shadows = self.shadows.lock().unwrap();
            for meta in &metas {
                let shadow = shadows.entry(meta.uid.clone()).or_insert_with(Shadow::new);
                shadow.output_bytes = meta.output_bytes;
                shadow.last_output_at = meta.last_output_at;
                if meta.cwd.is_some() {
                    shadow.cwd = meta.cwd.clone();
                }
            }
        }
        // Attach each survivor to (a) subscribe this connection to its live events and (b)
        // seed its replay mirror from the `Attach` reply ONCE (the reader applies it).
        for meta in &metas {
            let _ = self.send(&ClientMsg::Attach { uid: meta.uid.clone() });
        }
    }

    // ---- the SessionManager surface (delegated to over the wire) ----

    /// Spawn a session for `opts`. PINS the GUI-chosen uid in the wire spec, inserts an
    /// empty shadow so `has`/`replay` answer immediately, and fires a `Create` (the daemon
    /// auto-attaches the creator, so every event from the session's birth streams back).
    pub fn create(&self, opts: SpawnOptions) -> io::Result<()> {
        let uid = opts.uid.clone();
        // Insert the shadow up front so a `has(uid)`/`replay(uid)` immediately after create
        // (before any event arrives) is consistent with the in-process path.
        self.shadows.lock().unwrap().entry(uid.clone()).or_insert_with(Shadow::new);
        let spec = spawn_spec_from(opts);
        self.send(&ClientMsg::Create(spec))?;
        Ok(())
    }

    /// The custom-pty-`factory` variant exists only for in-process tests (a closure can't
    /// cross the socket). The daemon owns real PTYs, so the daemon backend ignores the
    /// factory and spawns a normal session — preserving the public signature without a
    /// meaningless wire form. (No production caller uses `create_with`.)
    pub fn create_with(
        &self,
        opts: SpawnOptions,
        _factory: crate::session_manager::SpawnFn,
    ) -> io::Result<()> {
        self.create(opts)
    }

    /// Whether a session with `uid` is live — answered from the shadow (no I/O).
    pub fn has(&self, uid: &str) -> bool {
        self.shadows.lock().unwrap().contains_key(uid)
    }

    /// The uids of all live sessions — from the shadow (no I/O).
    pub fn uids(&self) -> Vec<String> {
        self.shadows.lock().unwrap().keys().cloned().collect()
    }

    /// Recent output for a re-attaching view — the client mirror buffer (no round-trip).
    /// `None` for an unknown uid, matching the in-process `replay`.
    pub fn replay(&self, uid: &str) -> Option<String> {
        self.shadows.lock().unwrap().get(uid).map(|s| s.replay.get().to_string())
    }

    /// Monotonic UTF-16 output cursor — from the shadow (no I/O).
    pub fn output_bytes(&self, uid: &str) -> Option<u64> {
        self.shadows.lock().unwrap().get(uid).map(|s| s.output_bytes)
    }

    /// Epoch-ms of the last output flush — from the shadow (no I/O); `None` if nothing
    /// has flushed yet, mirroring the in-process accessor.
    pub fn last_output_at(&self, uid: &str) -> Option<u64> {
        self.shadows.lock().unwrap().get(uid).and_then(|s| s.last_output_at)
    }

    /// Serialize the pane's current screen — a bounded `RenderScreen`/`Screen` round-trip
    /// (off the hot path). `None` on an unknown uid, a gone session, or a timeout.
    pub fn render_screen(&self, uid: &str) -> Option<String> {
        let want_uid = uid.to_string();
        let reply = self.request(ClientMsg::RenderScreen { uid: uid.to_string() }, move |m| {
            matches!(m, DaemonMsg::Screen { uid: u, .. } if *u == want_uid)
        })?;
        match reply {
            DaemonMsg::Screen { text, .. } => text,
            _ => None,
        }
    }

    /// Write input to the pane's pty — fire-and-forget.
    pub fn write(&self, uid: &str, data: &str) {
        let _ = self.send(&ClientMsg::Write { uid: uid.to_string(), data: data.to_string() });
    }

    /// Resize the pane — fire-and-forget.
    pub fn resize(&self, uid: &str, cols: u16, rows: u16) {
        let _ = self.send(&ClientMsg::Resize { uid: uid.to_string(), cols, rows });
    }

    /// Kill the pane — fire-and-forget — and forget its shadow locally (the daemon
    /// suppresses the natural-exit event for a deliberate kill, so no `Exit` will arrive to
    /// drop it; we drop it here to keep `has`/`uids` correct immediately, mirroring the
    /// in-process `kill` which removes the session synchronously).
    pub fn kill(&self, uid: &str) {
        self.shadows.lock().unwrap().remove(uid);
        let _ = self.send(&ClientMsg::Kill { uid: uid.to_string() });
    }

    /// Kill every pane — fire-and-forget — and clear the local shadow.
    pub fn kill_all(&self) {
        self.shadows.lock().unwrap().clear();
        let _ = self.send(&ClientMsg::KillAll);
    }

    /// Ask the daemon to **shut down** (kill its sessions + exit): the quit-vs-keep-alive
    /// "OFF" branch and the `--kill-daemon` path. Fire-and-forget — the daemon exits without
    /// a reply frame, so the connection just drops (the EOF is the acknowledgement). Clears
    /// the local shadow so a subsequent accessor sees no sessions. No-op-safe: if the daemon
    /// is already gone the send simply fails and is ignored.
    pub fn shutdown_daemon(&self) {
        self.shadows.lock().unwrap().clear();
        let _ = self.send(&ClientMsg::Shutdown);
    }
}

/// The reader thread body: decode inbound frames forever, demuxing events (which update the
/// shadow/mirror and forward to the GUI channel) from replies (which go to the reply
/// channel). Exits on EOF, a socket error, or a dropped GUI channel.
#[cfg(unix)]
fn reader_loop(
    read_half: std::os::unix::net::UnixStream,
    shadows: Arc<Mutex<HashMap<String, Shadow>>>,
    events: UnboundedSender<SessionEvent>,
    replies: Sender<DaemonMsg>,
) {
    let mut r = read_half;
    loop {
        match read_frame::<_, DaemonMsg>(&mut r) {
            Ok(Some(DaemonMsg::Event(ev))) => {
                apply_event_to_shadow(&shadows, &ev);
                // Forward verbatim to the renderer. A send error means the GUI dropped its
                // receiver (shutting down) — stop reading.
                if events.send(ev).is_err() {
                    break;
                }
            }
            Ok(Some(DaemonMsg::Replay { uid, data })) => {
                // The one-shot replay seed from an `Attach`: prime the mirror from the
                // daemon's retained buffer so a re-attaching view restores history. Only
                // seed when the local mirror is still empty (a fresh/just-reconnected
                // shadow) — never clobber output already mirrored live.
                if !data.is_empty() {
                    let mut shadows = shadows.lock().unwrap();
                    let shadow = shadows.entry(uid).or_insert_with(Shadow::new);
                    if shadow.replay.get().is_empty() {
                        shadow.replay.append(&data);
                    }
                }
            }
            // Other replies (Sessions/Screen/Hello/Pong/Created) → the request channel.
            Ok(Some(reply)) => {
                if replies.send(reply).is_err() {
                    break; // the manager was dropped
                }
            }
            // Clean EOF (daemon closed) or a malformed-frame/socket error → done.
            Ok(None) | Err(_) => break,
        }
    }
}

/// Fold one streamed [`SessionEvent`] into the shadow: `Data` grows the mirror + counters,
/// `Cwd` updates the cached cwd, `Exit` drops the session (mirrors the in-process driver
/// removing a session from the map on terminal exit, so `has`/`uids` go false).
#[cfg(unix)]
fn apply_event_to_shadow(shadows: &Mutex<HashMap<String, Shadow>>, ev: &SessionEvent) {
    let mut shadows = shadows.lock().unwrap();
    match ev {
        SessionEvent::Data { uid, data } => {
            let shadow = shadows.entry(uid.clone()).or_insert_with(Shadow::new);
            shadow.replay.append(data);
            shadow.output_bytes += data.encode_utf16().count() as u64;
            shadow.last_output_at = Some(epoch_ms());
        }
        SessionEvent::Cwd { uid, cwd } => {
            let shadow = shadows.entry(uid.clone()).or_insert_with(Shadow::new);
            shadow.cwd = Some(cwd.clone());
        }
        SessionEvent::Exit { uid, .. } => {
            shadows.remove(uid);
        }
        // Phase-4 markers ride the proto but the client keeps no marker shadow yet
        // (the in-process backend owns the live liveness mirror). Ignored here; the
        // control server still receives the event stream verbatim for fan-out.
        SessionEvent::CommandStart { .. }
        | SessionEvent::CommandEnd { .. }
        | SessionEvent::PromptReady { .. }
        | SessionEvent::AgentState { .. } => {}
    }
}

/// Epoch-ms now — the client's own `last_output_at` stamp (the daemon's
/// `SessionEvent::Data` doesn't carry a timestamp, and the GUI compares against its own
/// wall clock anyway, exactly as the in-process `last_output_at` is a local stamp).
#[cfg(unix)]
fn epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Build the wire [`SpawnSpec`] from [`SpawnOptions`]: PIN the uid (the GUI owns it),
/// flatten the integration into the spec's resolved `integration_args`/`integration_env`
/// (the daemon folds them back via [`SpawnSpec::into_options`]), and carry the rest.
#[cfg(unix)]
fn spawn_spec_from(opts: SpawnOptions) -> SpawnSpec {
    let (integration_args, integration_env) = match opts.integration {
        Some(i) => (i.args, i.env),
        None => (Vec::new(), Default::default()),
    };
    SpawnSpec {
        uid: Some(opts.uid),
        shell: opts.shell,
        command: opts.command,
        args: opts.args,
        cwd: opts.cwd,
        env: opts.env,
        cols: opts.cols,
        rows: opts.rows,
        pane_id: opts.pane_id,
        integration_args,
        integration_env,
        control_file: opts.control_file,
    }
}

/// Result of the proto-version handshake probe ([`probe_proto_version`]).
#[cfg(unix)]
enum ProtoCheck {
    /// The daemon's `proto_ver` equals the client's [`PROTO_VER`] — proceed.
    Match,
    /// The daemon speaks a different version — tear it down + respawn.
    Mismatch { daemon_ver: u32 },
}

/// Do a bare `Hello` round-trip on a freshly-connected (NOT yet manager-owned) socket and
/// compare the daemon's reported `proto_ver` to the client's [`PROTO_VER`]. Used by `new`
/// BEFORE the manager + reader thread are built, so a mismatch can be resolved by tearing
/// the stale daemon down and respawning (the lock-step-upgrade contract — the daemon is our
/// own binary). A handshake that doesn't answer in time is treated as a `Match` (proceed):
/// the version gate must never hard-block launch over a slow/odd handshake — the worst case
/// is talking to a same-version daemon we couldn't confirm, which is harmless.
#[cfg(unix)]
fn probe_proto_version(stream: &std::os::unix::net::UnixStream) -> io::Result<ProtoCheck> {
    let mut w = stream.try_clone()?;
    let mut r = stream.try_clone()?;
    // A bounded read so a daemon that never answers doesn't wedge the probe. NB: the read
    // timeout is a SOCKET-level option (`SO_RCVTIMEO`), shared across `try_clone`'d fds AND
    // the original `stream` the manager later owns — so it MUST be cleared before we return,
    // or the manager's reader thread would spuriously time out every 2s. We clear it on every
    // path below (`r.set_read_timeout(None)` on the shared socket).
    r.set_read_timeout(Some(Duration::from_secs(2)))?;
    let send = write_frame(&mut w, &ClientMsg::Hello { proto_ver: PROTO_VER });
    let check = match send.and_then(|()| read_frame::<_, DaemonMsg>(&mut r)) {
        Ok(Some(DaemonMsg::Hello { proto_ver, .. })) => {
            if proto_ver == PROTO_VER {
                ProtoCheck::Match
            } else {
                ProtoCheck::Mismatch { daemon_ver: proto_ver }
            }
        }
        // Any non-Hello reply, EOF, or a (timed-out) read error: don't block launch over an
        // unconfirmed handshake — proceed as a match. NB: `from_stream` re-runs its own Hello
        // round-trip, draining the daemon's (second) reply, so this probe's `Hello` reply does
        // not desync the stream the manager later owns.
        _ => ProtoCheck::Match,
    };
    // Restore blocking reads on the shared socket before the manager takes it over.
    let _ = r.set_read_timeout(None);
    Ok(check)
}

/// Tear down a stale-version daemon at `socket`: connect, send `Shutdown`, then wait (briefly)
/// for the socket to disappear (the daemon unlinks it on exit). Best-effort — if the connect
/// fails the daemon is already gone, and if it lingers, `connect_or_spawn`'s `AddrInUse`
/// handling + retry on the next `new` loop iteration still converges. `salt` is unused today
/// (the socket path is enough) but kept for symmetry with the spawn side.
#[cfg(unix)]
fn tear_down_stale_daemon(socket: &Path, _salt: &str) {
    if let Ok(stream) = std::os::unix::net::UnixStream::connect(socket) {
        let mut w = stream;
        let _ = write_frame(&mut w, &ClientMsg::Shutdown);
        // Wait for the daemon to exit (it unlinks the socket on the way out). Bounded so a
        // wedged daemon doesn't hang launch; the respawn loop tolerates a slow teardown.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            // Once the socket file is gone (or refuses connections), the stale daemon is down.
            if !socket.exists() || std::os::unix::net::UnixStream::connect(socket).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Append a line to the daemon debug log when `HYPERPANES_DEBUG` is set (mirrors the app's
/// `dbg_log`; core has no logger of its own). Inert otherwise.
#[cfg(unix)]
fn dbg(msg: &str) {
    use std::io::Write;
    if std::env::var_os("HYPERPANES_DEBUG").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("hyperpanes-debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[daemon-client] {msg}");
    }
}

/// Connect to the daemon socket; if none is listening, spawn the daemon detached and
/// retry-connect with backoff until [`SPAWN_CONNECT_BUDGET`]. A spawn race — another client
/// just launched the daemon, so OUR spawn's bind hits `AddrInUse` — is NOT an error: we
/// simply keep retrying the connect, since whoever won the lock is now (or soon) listening.
#[cfg(unix)]
fn connect_or_spawn(socket: &Path, salt: &str) -> io::Result<std::os::unix::net::UnixStream> {
    // Fast path: a daemon is already up.
    if let Ok(s) = std::os::unix::net::UnixStream::connect(socket) {
        return Ok(s);
    }

    // None listening → spawn it detached. The daemon is a mode of THIS binary
    // (`current_exe --session-daemon <salt>`); `setsid` + null stdio so it outlives us and
    // never touches our console (the survival contract — see the plan's "Spawn" note).
    spawn_daemon_detached(salt)?;

    // Retry-connect with a short, growing backoff until the daemon binds (cold start +
    // Tokio runtime build can take a beat). `AddrInUse` cannot surface here — that's a
    // BIND-side error in the daemon we just (maybe redundantly) launched; on the CONNECT
    // side we only ever see ConnectionRefused/NotFound until the socket is live, which the
    // retry rides out. Treating a spawn race as "already running → connect" is exactly this
    // loop: it doesn't matter whose daemon won, only that one is listening.
    let deadline = Instant::now() + SPAWN_CONNECT_BUDGET;
    let mut backoff = Duration::from_millis(10);
    loop {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(s) => return Ok(s),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(200));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Launch `current_exe --session-daemon <salt>` fully detached: a new session (`setsid`,
/// so a GUI crash/SIGHUP never reaches it) with null stdio. Best-effort reap-avoidance:
/// the child re-parents to init once we don't wait on it.
#[cfg(unix)]
fn spawn_daemon_detached(salt: &str) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--session-daemon")
        .arg(salt)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // `setsid(2)` after fork detaches the child into its own session/process group so it
    // outlives the GUI (no controlling terminal, no SIGHUP on our exit). Declared inline as
    // a raw libc extern to avoid adding a `libc` direct dependency for one call — it's an
    // async-signal-safe syscall wrapper with no allocation, which is the bar for `pre_exec`.
    extern "C" {
        fn setsid() -> i32;
    }
    // SAFETY: `setsid` is async-signal-safe (no allocation, no locks); the closure runs in
    // the forked child between `fork` and `exec`, which is exactly where `pre_exec` allows
    // only such calls. We ignore its result (a failure just leaves us in the parent's
    // session, which is harmless — the daemon still runs, only slightly less isolated).
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    // Spawn and immediately drop the handle — we never wait on the daemon (it's long-lived
    // and detached). On unix the dropped child re-parents to init, so it isn't zombied.
    cmd.spawn().map(|_child| ())
}

// ---- non-unix stub: the daemon transport is unix-only in M1 (Windows pipes are M3) ----

/// On non-unix the daemon transport (UDS) doesn't exist yet, so the daemon backend can't be
/// constructed; the enum dispatch in [`SessionManager`](crate::session_manager) falls back
/// to in-process. This stub exists only so the type name resolves in the enum on all
/// platforms.
#[cfg(not(unix))]
pub struct DaemonSessionManager {
    _never: std::convert::Infallible,
}

#[cfg(not(unix))]
impl DaemonSessionManager {
    pub fn new(
        _events: tokio::sync::mpsc::UnboundedSender<crate::session_manager::SessionEvent>,
        _salt: &str,
    ) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the session daemon transport is unix-only in M1 (Windows named pipes are M3)",
        ))
    }
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> crate::session::spawn::EnvMap {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn spawn_spec_from_pins_uid_and_flattens_integration() {
        let opts = SpawnOptions {
            uid: "pane-9".into(),
            shell: Some("/bin/zsh".into()),
            command: Some("ls".into()),
            cwd: Some("/tmp".into()),
            integration: Some(crate::session_manager::Integration {
                args: vec!["-i".into()],
                env: env(&[("HP", "1")]),
            }),
            control_file: Some("/c.json".into()),
            ..Default::default()
        };
        let spec = spawn_spec_from(opts);
        assert_eq!(spec.uid.as_deref(), Some("pane-9"), "the GUI's uid is pinned on the wire");
        assert_eq!(spec.command.as_deref(), Some("ls"));
        assert_eq!(spec.integration_args, vec!["-i".to_string()]);
        assert_eq!(spec.integration_env.get("HP").map(String::as_str), Some("1"));
        assert_eq!(spec.control_file.as_deref(), Some("/c.json"));
        // Round-trips back through into_options to the same uid (the daemon honors it).
        assert_eq!(spec.into_options("pane-9".into()).uid, "pane-9");
    }

    #[test]
    fn spawn_spec_from_no_integration_is_a_plain_shell() {
        let spec = spawn_spec_from(SpawnOptions { uid: "p1".into(), ..Default::default() });
        assert!(spec.integration_args.is_empty());
        assert!(spec.integration_env.is_empty());
        assert!(spec.into_options("p1".into()).integration.is_none());
    }

    // ---- shadow folding (no socket needed) ----

    fn shadows() -> Arc<Mutex<HashMap<String, Shadow>>> {
        Arc::default()
    }

    #[test]
    fn data_event_grows_mirror_and_counters() {
        let s = shadows();
        apply_event_to_shadow(&s, &SessionEvent::Data { uid: "u1".into(), data: "ab".into() });
        apply_event_to_shadow(&s, &SessionEvent::Data { uid: "u1".into(), data: "😀".into() });
        let g = s.lock().unwrap();
        let sh = g.get("u1").unwrap();
        assert_eq!(sh.replay.get(), "ab😀");
        assert_eq!(sh.output_bytes, 4, "ab=2 + emoji=2 UTF-16 units");
        assert!(sh.last_output_at.is_some());
    }

    #[test]
    fn cwd_event_updates_shadow_cwd() {
        let s = shadows();
        apply_event_to_shadow(&s, &SessionEvent::Cwd { uid: "u1".into(), cwd: "/tmp".into() });
        assert_eq!(s.lock().unwrap().get("u1").unwrap().cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn exit_event_drops_the_shadow() {
        let s = shadows();
        apply_event_to_shadow(&s, &SessionEvent::Data { uid: "u1".into(), data: "x".into() });
        assert!(s.lock().unwrap().contains_key("u1"));
        apply_event_to_shadow(&s, &SessionEvent::Exit { uid: "u1".into(), code: 0 });
        assert!(!s.lock().unwrap().contains_key("u1"), "Exit drops the session shadow");
    }

    // ---- end-to-end: DaemonSessionManager against a REAL in-process daemon ----
    //
    // These reuse M0's loopback harness (`session::daemon::spawn_in_process`, on a temp
    // socket with the daemon's own runtime) and drive the M1 client over it: create →
    // observe Data/Exit on the GUI channel, replay() returns the mirror, render_screen()
    // round-trips, kill works, and a fresh manager on the same socket re-seeds from Attach.

    use crate::session::daemon::spawn_in_process;
    use std::time::Duration as Dur;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    // A unique temp socket path per test AND per run (pid + thread id) — never collides.
    fn temp_socket(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hp-m1-{tag}-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    // Block (on a helper thread / spin) until an event channel yields one matching `pred`,
    // or the deadline passes. Drains intervening events. The channel is the GUI's
    // `UnboundedReceiver<SessionEvent>` that the manager's reader thread feeds.
    fn recv_event_until(
        rx: &mut UnboundedReceiver<SessionEvent>,
        timeout: Dur,
        mut pred: impl FnMut(&SessionEvent) -> bool,
    ) -> Option<SessionEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => continue,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Dur::from_millis(5));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
        None
    }

    // Spin until `cond` is true or the deadline passes (for shadow propagation, which lands
    // a beat after the event since the reader thread applies it asynchronously).
    fn wait_until(timeout: Dur, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Dur::from_millis(5));
        }
        cond()
    }

    fn connect_manager(
        socket: &Path,
    ) -> (DaemonSessionManager, UnboundedReceiver<SessionEvent>) {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
        let (etx, erx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager");
        (mgr, erx)
    }

    // create → write → observe Data on the GUI channel; replay() mirrors; the shadow
    // accumulates output_bytes/last_output_at; kill() drops it synchronously. Uses a
    // long-lived interactive shell so the session stays alive while we assert the (live)
    // mirror — `create_short_command_streams_data_then_exit` covers the exit path.
    #[test]
    fn create_write_streams_data_replay_mirrors_and_kill() {
        let socket = temp_socket("create");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "p1".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");

        // has()/uids() reflect the session immediately (shadow inserted on create).
        assert!(mgr.has("p1"), "has() true right after create");
        assert!(mgr.uids().contains(&"p1".to_string()));

        // Drive a marker; its echo streams back as Data on the GUI channel.
        mgr.write("p1", "echo HELLO_MARKER\n");
        let data = recv_event_until(&mut rx, Dur::from_secs(10), |e| {
            matches!(e, SessionEvent::Data { uid, data } if uid == "p1" && data.contains("HELLO_MARKER"))
        });
        assert!(data.is_some(), "expected Data{{HELLO_MARKER}} on the GUI channel");

        // replay() returns the client mirror (no round-trip) and includes the output.
        assert!(
            wait_until(Dur::from_secs(2), || {
                mgr.replay("p1").map_or(false, |r| r.contains("HELLO_MARKER"))
            }),
            "replay() mirror should hold the streamed output, got {:?}",
            mgr.replay("p1")
        );
        // output_bytes / last_output_at shadow advanced.
        assert!(mgr.output_bytes("p1").unwrap_or(0) > 0, "output_bytes shadow advanced");
        assert!(mgr.last_output_at("p1").is_some(), "last_output_at shadow set");

        // kill() drops the shadow synchronously (deliberate kill is silent — no Exit event).
        mgr.kill("p1");
        assert!(!mgr.has("p1"), "kill drops the shadow synchronously");
    }

    // A short-lived command streams its Data AND a natural Exit{0} to the GUI channel, and
    // the natural exit drops the shadow (has() → false) — mirroring the in-process driver
    // removing a session from the map on terminal exit.
    #[test]
    fn create_short_command_streams_data_then_exit() {
        let socket = temp_socket("shortcmd");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        // The 0.3s sleep holds output back until the daemon-side auto-attach (on Create)
        // has registered us, so the Data + Exit stream live and deterministically.
        mgr.create(SpawnOptions {
            uid: "q1".into(),
            command: Some("/bin/sh".into()),
            args: Some(vec!["-c".into(), "sleep 0.3; echo hi".into()]),
            ..Default::default()
        })
        .expect("create");

        let data = recv_event_until(&mut rx, Dur::from_secs(10), |e| {
            matches!(e, SessionEvent::Data { uid, data } if uid == "q1" && data.contains("hi"))
        });
        assert!(data.is_some(), "expected Data{{hi}} on the GUI channel");

        let exit = recv_event_until(&mut rx, Dur::from_secs(10), |e| {
            matches!(e, SessionEvent::Exit { uid, code } if uid == "q1" && *code == 0)
        });
        assert!(exit.is_some(), "expected Exit{{0}} on the GUI channel");
        assert!(wait_until(Dur::from_secs(2), || !mgr.has("q1")), "natural exit drops the shadow");
    }

    // render_screen() round-trips to the daemon (a bounded request/response).
    #[test]
    fn render_screen_round_trips() {
        let socket = temp_socket("screen");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "p1".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");

        // Drive a marker and wait for it to stream so the daemon's screen has content.
        mgr.write("p1", "echo SCREEN_MARKER\n");
        let saw = recv_event_until(&mut rx, Dur::from_secs(10), |e| {
            matches!(e, SessionEvent::Data { uid, data } if uid == "p1" && data.contains("SCREEN_MARKER"))
        });
        assert!(saw.is_some(), "marker should stream");

        // render_screen() returns the serialized screen (a real round-trip), containing it.
        let screen = wait_until(Dur::from_secs(3), || {
            mgr.render_screen("p1").map_or(false, |s| s.contains("SCREEN_MARKER"))
        });
        assert!(screen, "render_screen should round-trip the screen incl. the marker");

        // An unknown uid renders to None (gone session / never existed).
        assert_eq!(mgr.render_screen("nope"), None);

        mgr.kill("p1");
        assert!(!mgr.has("p1"), "kill drops the shadow synchronously");
    }

    // RECONNECT: drop the client, make a NEW manager on the same socket; uids() shows the
    // survivor and replay() re-seeds from the Attach reply (the M2 payoff, at the client).
    #[test]
    fn reconnect_shows_survivor_and_reseeds_replay_from_attach() {
        let socket = temp_socket("reconnect");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        // First manager: create a long-lived shell and drive a marker into it.
        let (mgr1, mut rx1) = connect_manager(&socket);
        mgr1.create(SpawnOptions {
            uid: "surv".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        mgr1.write("surv", "echo SURVIVOR_MARKER\n");
        assert!(
            recv_event_until(&mut rx1, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, data } if uid == "surv" && data.contains("SURVIVOR_MARKER"))
            })
            .is_some(),
            "marker should stream to the first manager"
        );
        // The marker is now in the daemon's retained replay buffer for this live session.

        // Drop the first manager (simulating a GUI crash) — the daemon + session survive.
        drop(mgr1);
        drop(rx1);

        // A FRESH manager on the same socket: ListSessions (on connect) shows the survivor,
        // and the Attach it issues re-seeds the replay mirror from the daemon's buffer.
        let (mgr2, _rx2) = connect_manager(&socket);
        assert!(
            wait_until(Dur::from_secs(2), || mgr2.uids().contains(&"surv".to_string())),
            "reconnect: uids() should show the survivor, got {:?}",
            mgr2.uids()
        );
        assert!(
            wait_until(Dur::from_secs(3), || {
                mgr2.replay("surv").map_or(false, |r| r.contains("SURVIVOR_MARKER"))
            }),
            "reconnect: replay() should re-seed from the Attach reply, got {:?}",
            mgr2.replay("surv")
        );

        mgr2.kill("surv");
    }

    // ---- M2 re-attach: the SessionManager-level decision the GUI restore branches on ----
    //
    // `state.rs::make_pane_from_spec` decides RE-ATTACH vs RE-SPAWN with exactly this
    // predicate: `mgr.is_daemon() && spec.uid.map(|u| mgr.has(u)).unwrap_or(false)`. These
    // tests stand up a SessionManager::Daemon over a REAL (in-process) daemon and prove that
    // predicate end-to-end — a SURVIVING uid re-attaches (the session was NOT re-spawned;
    // `replay` carries the prior output), an UNKNOWN/dead uid does not — plus the
    // uid-stability invariant the whole scheme rests on.

    use crate::session_manager::SessionManager;

    // Wrap a real connected daemon socket in a SessionManager::Daemon — the same backend the
    // GUI holds (`new_daemon` builds this; here we connect to a temp-socket in-process daemon
    // instead of spawning `current_exe`, which a test binary can't do).
    fn daemon_manager(socket: &Path) -> (SessionManager, UnboundedReceiver<SessionEvent>) {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
        let (etx, erx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager");
        (SessionManager::Daemon(Arc::new(mgr)), erx)
    }

    // The crux: a snapshot uid that is STILL LIVE in the daemon re-attaches (no re-spawn),
    // and a dead/unknown uid does not — so the GUI restore would re-spawn it instead.
    #[test]
    fn surviving_uid_reattaches_unknown_uid_does_not() {
        let socket = temp_socket("reattach-decide");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        // --- "previous run": create a long-lived session under a recorded uid + drive output.
        let recorded_uid = {
            let (mgr1, mut rx1) = daemon_manager(&socket);
            assert!(mgr1.is_daemon(), "the daemon backend reports is_daemon()");
            // A GUI pane would mint this via `mgr.fresh_uid()` (a UUID on the daemon backend);
            // a literal uid is fine for the test — the point is it's PINNED + recorded.
            let uid = mgr1.fresh_uid();
            assert!(uid.starts_with("pane-"), "daemon fresh_uid is a pane-<uuid>, got {uid}");
            mgr1.create(SpawnOptions {
                uid: uid.clone(),
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            })
            .expect("create");
            mgr1.write(&uid, "echo REATTACH_MARKER\n");
            assert!(
                recv_event_until(&mut rx1, Dur::from_secs(10), |e| {
                    matches!(e, SessionEvent::Data { uid: u, data } if *u == uid && data.contains("REATTACH_MARKER"))
                })
                .is_some(),
                "marker should stream into the live session"
            );
            // Snapshot would record `uid` (to_session_file does). Drop the manager = GUI crash.
            uid
        };

        // --- "next launch": a FRESH manager on the same daemon. The restore predicate:
        let (mgr2, _rx2) = daemon_manager(&socket);

        // (a) The surviving uid: is_daemon && has(uid) → RE-ATTACH. The session was NOT
        //     re-spawned (it's the very same one), and replay carries its prior output.
        assert!(
            wait_until(Dur::from_secs(2), || mgr2.has(&recorded_uid)),
            "the survivor's uid is live in the daemon after reconnect, got uids {:?}",
            mgr2.uids()
        );
        let reattach_survivor = mgr2.is_daemon() && mgr2.has(&recorded_uid);
        assert!(reattach_survivor, "restore would RE-ATTACH the surviving uid (no re-spawn)");
        assert!(
            wait_until(Dur::from_secs(3), || {
                mgr2.replay(&recorded_uid).map_or(false, |r| r.contains("REATTACH_MARKER"))
            }),
            "re-attach seeds the fresh grid from the survivor's replay, got {:?}",
            mgr2.replay(&recorded_uid)
        );

        // (b) An unknown/dead uid (the program had exited last run): has() is false → the GUI
        //     falls back to a fresh spawn from spec.command/args/shell.
        let dead_uid = "pane-00000000-dead-dead-dead-000000000000";
        let reattach_dead = mgr2.is_daemon() && mgr2.has(dead_uid);
        assert!(!reattach_dead, "an unknown/dead uid does NOT re-attach → restore re-spawns it");

        mgr2.kill(&recorded_uid);
    }

    // uid-stability invariant (the plan's "uid stability"): the daemon backend's fresh_uid is
    // UNIQUE across independent "runs" (manager instances) — a new run's freshly minted uid
    // can never collide with (and thus silently adopt) a survivor from a prior run. A literal
    // process-local counter would re-issue `pane-0` every run and alias the survivor.
    #[test]
    fn daemon_fresh_uid_is_unique_across_runs() {
        let socket = temp_socket("uid-stability");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        // Two independent managers (two GUI runs against the same daemon) each mint a batch
        // of fresh uids; the two batches must be disjoint (no cross-run collision).
        let (runa, _ra) = daemon_manager(&socket);
        let (runb, _rb) = daemon_manager(&socket);

        let batch1: Vec<String> = (0..8).map(|_| runa.fresh_uid()).collect();
        let batch2: Vec<String> = (0..8).map(|_| runb.fresh_uid()).collect();

        // Unique within each run AND across the two runs.
        let mut all = batch1.clone();
        all.extend(batch2.clone());
        all.sort();
        all.dedup();
        assert_eq!(all.len(), batch1.len() + batch2.len(), "fresh_uid never collides across runs");
        drop((runa, runb, _ra, _rb));

        // And the same uid a run minted is exactly the one a survivor would be re-attached by:
        // create under a minted uid, drop, reconnect, has(uid) true (the round-trip the GUI
        // snapshot→reattach relies on).
        let (run_a, mut rx_a) = daemon_manager(&socket);
        let surv = run_a.fresh_uid();
        run_a.create(SpawnOptions {
            uid: surv.clone(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        // Drive a marker and wait for its echo so the session is CONFIRMED registered + live
        // daemon-side before we drop (mirrors the reconnect test — without this the daemon may
        // not have finished spawning the pty when ListSessions runs on the next connect).
        run_a.write(&surv, "echo STABLE_SURVIVOR\n");
        assert!(
            recv_event_until(&mut rx_a, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid: u, data } if *u == surv && data.contains("STABLE_SURVIVOR"))
            })
            .is_some(),
            "the minted-uid session is live daemon-side before we drop"
        );
        drop(run_a);
        drop(rx_a);

        let (run3, _r3) = daemon_manager(&socket);
        assert!(
            wait_until(Dur::from_secs(2), || run3.has(&surv)),
            "the minted uid round-trips: a later run re-attaches the survivor by that exact uid"
        );
        run3.kill(&surv);
    }

    // ---- keystroke→echo micro-bench: daemon vs in-process ----
    //
    // The plan's latency risk: the daemon adds a local UDS hop per keystroke/output chunk.
    // This measures keystroke→echoed-Data round-trip latency on BOTH backends and prints
    // both numbers, to confirm the daemon overhead is negligible (the design's hypothesis).
    //
    // Ignored by default (it spawns real shells and takes a couple seconds); run with:
    //   cargo test -p hyperpanes-core keystroke_echo_latency_bench -- --ignored --nocapture
    #[test]
    #[ignore = "micro-bench: run with --ignored --nocapture"]
    fn keystroke_echo_latency_bench() {
        const ITERS: usize = 60;
        const WARMUP: usize = 5;

        // In-process backend: a real SessionManager (no daemon, no socket).
        let inproc = {
            let (etx, mut erx) = unbounded_channel::<SessionEvent>();
            let rt = tokio::runtime::Runtime::new().expect("rt");
            let _g = rt.enter();
            let mgr = crate::session_manager::SessionManager::new(etx);
            mgr.create(SpawnOptions {
                uid: "ip".into(),
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            })
            .expect("create inproc");
            // Drain the shell's startup banner before timing.
            std::thread::sleep(Dur::from_millis(300));
            while erx.try_recv().is_ok() {}
            let lat = bench_echo("ip", &mgr, &mut erx, ITERS, WARMUP);
            mgr.kill("ip");
            // Keep the runtime alive for the duration (drivers run on it); drop after.
            drop(rt);
            lat
        };

        // Daemon backend: a DaemonSessionManager over an in-process daemon (a real socket).
        let daemon = {
            let socket = temp_socket("bench");
            let _d = spawn_in_process(&socket).expect("daemon binds");
            let (mgr, mut rx) = connect_manager(&socket);
            mgr.create(SpawnOptions {
                uid: "dm".into(),
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            })
            .expect("create daemon");
            std::thread::sleep(Dur::from_millis(300));
            while rx.try_recv().is_ok() {}
            let lat = bench_echo("dm", &mgr, &mut rx, ITERS, WARMUP);
            mgr.kill("dm");
            lat
        };

        println!("\n=== keystroke->echo latency ({ITERS} iters, {WARMUP} warmup) ===");
        println!("  in-process : mean {:>7.1}us  p50 {:>7.1}us  max {:>7.1}us", inproc.0, inproc.1, inproc.2);
        println!("  daemon     : mean {:>7.1}us  p50 {:>7.1}us  max {:>7.1}us", daemon.0, daemon.1, daemon.2);
        println!("  daemon overhead (mean): {:+.1}us\n", daemon.0 - inproc.0);
    }

    // A backend-agnostic echo timer: write a unique marker line, time until its echoed Data
    // arrives on `rx`, repeated. Returns (mean_us, p50_us, max_us). Works for any type with
    // `write(&str)` and a paired `UnboundedReceiver<SessionEvent>` — i.e. both backends.
    trait WriteToBackend {
        fn write(&self, uid: &str, data: &str);
    }
    impl WriteToBackend for crate::session_manager::SessionManager {
        fn write(&self, uid: &str, data: &str) {
            self.write(uid, data)
        }
    }
    impl WriteToBackend for DaemonSessionManager {
        fn write(&self, uid: &str, data: &str) {
            self.write(uid, data)
        }
    }

    fn bench_echo(
        uid: &str,
        mgr: &impl WriteToBackend,
        rx: &mut UnboundedReceiver<SessionEvent>,
        iters: usize,
        warmup: usize,
    ) -> (f64, f64, f64) {
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let marker = format!("M{i}Z");
            let t0 = Instant::now();
            mgr.write(uid, &format!("echo {marker}\n"));
            // Wait for the echoed marker to come back as Data.
            let got = recv_event_until(rx, Dur::from_secs(5), |e| {
                matches!(e, SessionEvent::Data { uid: u, data } if u == uid && data.contains(&marker))
            });
            let dt = t0.elapsed();
            assert!(got.is_some(), "echo {marker} timed out");
            if i >= warmup {
                samples.push(dt.as_secs_f64() * 1e6); // microseconds
            }
            // Drain any trailing chunks (prompt redraw) before the next iteration.
            while rx.try_recv().is_ok() {}
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let p50 = samples[samples.len() / 2];
        let max = *samples.last().unwrap();
        (mean, p50, max)
    }

    // M0 follow-up: a create whose pty spawn FAILS surfaces as an Exit event (instead of a
    // silently-hung blank pane). Force a failure with a non-existent shell binary.
    #[test]
    fn create_spawn_failure_surfaces_as_exit() {
        let socket = temp_socket("spawnfail");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, mut rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "bad".into(),
            // A direct spawn of a binary that does not exist → the pty spawn errors.
            command: Some("/nonexistent/definitely-not-a-real-binary-xyz".into()),
            args: Some(vec!["/nonexistent/definitely-not-a-real-binary-xyz".into()]),
            ..Default::default()
        })
        .expect("create request sends");

        // The daemon injects an Exit for the uid on spawn failure; the client reflects it.
        let exit = recv_event_until(&mut rx, Dur::from_secs(5), |e| {
            matches!(e, SessionEvent::Exit { uid, .. } if uid == "bad")
        });
        assert!(exit.is_some(), "a spawn failure should surface as an Exit, not a hang");
        assert!(wait_until(Dur::from_secs(2), || !mgr.has("bad")), "the failed session is dropped");
    }

    // ====================== M3 proto-version handshake + shutdown ======================

    // probe_proto_version against a REAL daemon (same PROTO_VER) returns Match, AND the
    // manager built on that same socket afterwards still works — i.e. the probe's Hello
    // round-trip did NOT desync the stream nor leave a read timeout on the shared fd (a
    // regression guard for the SO_RCVTIMEO-is-shared subtlety).
    #[test]
    fn version_probe_matches_real_daemon_and_manager_works_after() {
        let socket = temp_socket("proto-match");
        let _daemon = spawn_in_process(&socket).expect("daemon binds");

        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        assert!(
            matches!(probe_proto_version(&stream).expect("probe"), ProtoCheck::Match),
            "a same-version daemon must match"
        );
        // Build the manager on the SAME stream (as `new` does after a Match) and drive it —
        // proves no desync + blocking reads restored (the reader thread must not time out).
        let (etx, mut rx) = unbounded_channel::<SessionEvent>();
        let mgr = DaemonSessionManager::from_stream(stream, etx).expect("manager after probe");
        mgr.create(SpawnOptions {
            uid: "pm".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        mgr.write("pm", "echo PROBE_OK\n");
        assert!(
            recv_event_until(&mut rx, Dur::from_secs(10), |e| {
                matches!(e, SessionEvent::Data { uid, data } if uid == "pm" && data.contains("PROBE_OK"))
            })
            .is_some(),
            "the manager must stream normally after the version probe"
        );
        mgr.kill("pm");
    }

    // A daemon that reports a DIFFERENT proto_ver makes the probe return Mismatch — the
    // signal `new` uses to tear down + respawn. Stand up a one-shot fake listener that
    // answers the client's Hello with a bumped version.
    #[test]
    fn version_probe_detects_a_mismatched_daemon() {
        let socket = temp_socket("proto-mismatch");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("fake bind");

        // Fake daemon: accept one connection, read the client's Hello, reply with a version
        // ONE GREATER than ours (a "newer daemon" a stale client would meet).
        let server = std::thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let _ = read_frame::<_, ClientMsg>(&mut conn); // the client's Hello
                let _ = write_frame(
                    &mut conn,
                    &DaemonMsg::Hello { proto_ver: PROTO_VER + 1, daemon_pid: 4242 },
                );
                // Keep the connection open briefly so the client reads the reply.
                std::thread::sleep(Dur::from_millis(200));
            }
        });

        let stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect fake");
        let check = probe_proto_version(&stream).expect("probe");
        assert!(
            matches!(check, ProtoCheck::Mismatch { daemon_ver } if daemon_ver == PROTO_VER + 1),
            "a different-version daemon must be a Mismatch carrying the daemon's version"
        );
        drop(stream);
        let _ = server.join();
    }

    // tear_down_stale_daemon actually brings a running daemon down: after it returns, the
    // socket is unlinked (the daemon exited) — the mechanism `new` relies on to clear the
    // stale daemon before respawning a fresh one.
    #[test]
    fn tear_down_stale_daemon_shuts_a_running_daemon_down() {
        let socket = temp_socket("teardown");
        let daemon = spawn_in_process(&socket).expect("daemon binds");
        assert!(socket.exists() && !daemon.is_shutting_down());

        tear_down_stale_daemon(&socket, "salt-unused");

        assert!(
            wait_until(Dur::from_secs(3), || daemon.is_shutting_down()),
            "tear_down_stale_daemon should shut the daemon down"
        );
        assert!(
            wait_until(Dur::from_secs(1), || !socket.exists()),
            "the torn-down daemon unlinks its socket"
        );
    }

    // The manager-level shutdown_daemon() sends Shutdown and clears the local shadow, and the
    // daemon tears down (the quit-vs-keep-alive "OFF" branch at the client surface).
    #[test]
    fn manager_shutdown_daemon_tears_the_daemon_down() {
        let socket = temp_socket("mgr-shutdown");
        let daemon = spawn_in_process(&socket).expect("daemon binds");
        let (mgr, _rx) = connect_manager(&socket);

        mgr.create(SpawnOptions {
            uid: "s".into(),
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        })
        .expect("create");
        assert!(mgr.has("s"), "session present before shutdown");

        mgr.shutdown_daemon();
        assert!(!mgr.has("s"), "shutdown_daemon clears the local shadow");
        assert!(
            wait_until(Dur::from_secs(3), || daemon.is_shutting_down()),
            "shutdown_daemon brings the daemon down"
        );
    }
}
