//! The **session daemon** (`docs/session-daemon-plan.md` M0) — a long-lived,
//! PTY-owning process that survives a GUI crash. The GUI becomes a *client* that
//! attaches over a framed Unix-domain socket (Windows: named pipe — M3); the daemon
//! owns a [`SessionRegistry`] and multiplexes its event stream to every attached client.
//! Entirely in `core` (no Slint), so the whole thing is headless-testable.
//!
//! ## Transport & blocking I/O
//! The wire framing ([`session::proto`](crate::session::proto)) is plain blocking
//! `Read`/`Write`, so the daemon serves each connection on its own OS thread with blocking
//! `std::os::unix::net` sockets — no async-framing layer, and it composes with the
//! blocking [`DaemonClient`] reader thread the test (and future M1) use. The accept loop
//! and the event-broadcast pump each run on a thread too. The registry's per-session pty
//! *driver* tasks still need a Tokio runtime (`SessionRegistry::create` spawns them), so
//! [`run`] builds one and the registry is created inside it.
//!
//! ## Event multiplexing
//! Every session event from the registry arrives on one mpsc receiver; a pump thread
//! rebroadcasts each event over a [`tokio::sync::broadcast`] channel. Each connection
//! subscribes and forwards the events for the uids it has `Attach`ed to. Multiple clients
//! may attach to the same (or different) sessions concurrently.
//!
//! ## Discovery / single-daemon-per-salt
//! Reuses the `single_instance` machinery's shape: a flock'd `hyperpanesd-<salt>.lock`
//! plus a `hyperpanesd-<salt>.sock`, both under the per-user runtime dir, with the salt
//! hashed to a fixed-width token. The flock guarantees one daemon per salt; a second
//! `run` for a salt already served exits cleanly (`AddrInUse`).
//!
//! M0 scope: bind + accept + handle create/write/resize/kill/kill_all, stream events,
//! serve ListSessions / Attach(replay) / RenderScreen, plus a loopback [`DaemonClient`].
//!
//! ## M3 lifecycle hardening
//! Three new behaviors layer on top of the M0 serve loop, all coordinated through a small
//! shared [`Lifecycle`] (an atomic connection counter + a shutdown flag + the listening
//! socket path, so the exit path can unlink it):
//!
//! * **Idle-exit.** A monitor thread arms a [grace timer](idle_grace) once the daemon has
//!   **0 live sessions AND 0 connected clients**, and exits the process when the grace
//!   elapses while still idle. Any client connect (the connection counter) or session
//!   create (the registry's live count) resets it — so the daemon lingers across a GUI
//!   crash→relaunch gap but never forever. The grace is overridable via
//!   `HYPERPANES_DAEMON_IDLE_MS` so tests can use a short one.
//! * **`Shutdown`.** [`ClientMsg::Shutdown`] kills every session and exits cleanly
//!   (releasing the flock on process death, unlinking the socket on the way out). Drives
//!   the app's `--kill-daemon` and the quit-vs-keep-alive "OFF" branch.
//! * **Robust discovery/teardown.** Startup reaps a stale lock/socket (the flock gates, a
//!   leftover socket from a dead daemon is removed); the runtime dir is created `0700` and
//!   the socket tightened to `0600`.
//!
//! Windows named pipes are sketched in [`windows`](self::windows) (**WINDOWS-CI-PENDING** —
//! unbuildable on this Linux box; M4 flips the default on cross-platform).

#[cfg(unix)]
use std::collections::HashSet;
use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::session::proto::{
    read_frame, write_frame, ClientMsg, DaemonMsg, SessionMeta, PROTO_VER,
};
#[cfg(unix)]
use crate::session_manager::{SessionEvent, SessionRegistry};

/// How long the daemon stays alive after going fully idle (0 sessions AND 0 clients)
/// before exiting. Long enough to span a GUI crash→relaunch gap; the
/// `HYPERPANES_DAEMON_IDLE_MS` env override lets tests use a tiny grace.
#[cfg(unix)]
const DEFAULT_IDLE_GRACE_MS: u64 = 30_000;

/// How often the idle monitor re-checks the idle condition. Small relative to the grace so
/// the exit fires promptly once the grace elapses, large enough not to spin.
#[cfg(unix)]
const IDLE_POLL_MS: u64 = 100;

/// How long the accept loop sleeps between non-blocking `accept()` polls when no connection
/// is pending. Short so a GUI-startup connect is picked up promptly (the daemon is connected
/// to rarely, so this never busy-spins in practice).
#[cfg(unix)]
const ACCEPT_POLL_MS: u64 = 15;

/// The configured idle grace — [`DEFAULT_IDLE_GRACE_MS`] unless `HYPERPANES_DAEMON_IDLE_MS`
/// is set to a parseable millisecond count (the test hook for a short grace).
#[cfg(unix)]
fn idle_grace() -> Duration {
    idle_grace_from(std::env::var("HYPERPANES_DAEMON_IDLE_MS").ok().as_deref())
}

/// Pure parse of the idle-grace override (factored out so it's testable WITHOUT mutating the
/// process env — a global `set_var` would race the other env-sensitive tests). `None` or an
/// unparseable value → [`DEFAULT_IDLE_GRACE_MS`].
#[cfg(unix)]
fn idle_grace_from(raw: Option<&str>) -> Duration {
    let ms = raw.and_then(|v| v.parse::<u64>().ok()).unwrap_or(DEFAULT_IDLE_GRACE_MS);
    Duration::from_millis(ms)
}

/// Run the session daemon for `salt`, blocking until the process exits. Binds the salted
/// lock + socket under the runtime dir (one daemon per salt), then serves clients forever.
/// This is the body behind `hyperpanes --session-daemon <salt>` — `main` is a 3-line entry.
///
/// On unix: builds a Tokio runtime (the pty drivers need it), acquires the daemon flock,
/// binds the UDS, and serves. If another daemon already holds the salt, returns cleanly
/// (the lock is held → `AddrInUse`).
#[cfg(unix)]
pub fn run(salt: &str) -> io::Result<()> {
    let names = daemon_names(salt);
    if let Some(dir) = names.lock.parent() {
        ensure_runtime_dir(dir)?;
    }

    // The flock gates one daemon per salt (kernel-released on death, so a crashed daemon
    // never wedges the next launch — same contract as the single-instance detector). A
    // STALE lock FILE (dead pid inside, flock not held) is harmless: the flock itself is
    // the gate, not the file's existence (mirrors `single_instance::unix`).
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&names.lock)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            // Another daemon already serves this salt — nothing to do.
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "a daemon already holds this salt"));
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    // Record our pid for diagnostics now that we hold the gate (the flock is the real lock).
    {
        use std::io::Write;
        let _ = lock.set_len(0);
        let _ = (&lock).write_all(std::process::id().to_string().as_bytes());
    }

    // We hold the flock, so any leftover socket file is a DEAD daemon's — safe to reap and
    // rebind (the stale-socket-reclaim path; a previous daemon that crashed without unlinking
    // its socket must never block the new one's bind).
    let _ = std::fs::remove_file(&names.socket);
    let listener = std::os::unix::net::UnixListener::bind(&names.socket)?;
    restrict_socket_perms(&names.socket);

    // The pty drivers spawned by `SessionRegistry::create` need a Tokio runtime in scope.
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    // M3 lifecycle: shared shutdown flag + connection counter + the socket path (so the exit
    // path unlinks it). The idle monitor watches it; `serve`'s accept loop checks it.
    let lifecycle = Arc::new(Lifecycle::new(names.socket.clone()));
    let daemon = Daemon::new(Arc::clone(&lifecycle));
    daemon.start_idle_monitor(idle_grace());

    // Hold the lock for the daemon's whole lifetime (dropping it releases the salt). The
    // exit path (idle-monitor or `Shutdown`) calls `process::exit` AFTER unlinking the
    // socket, and the kernel releases the flock on process death.
    let _lock = lock;
    daemon.serve(listener)
}

/// Create the per-user runtime dir if absent and tighten it to owner-only (`0700`) so the
/// salted lock + socket live in a private dir — the daemon analog of the single-instance
/// trust boundary (filesystem-scoped to the user, no network surface). Best-effort on the
/// `chmod` (a pre-existing `$XDG_RUNTIME_DIR` is already `0700` and owned by us).
#[cfg(unix)]
fn ensure_runtime_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// How a daemon tears itself down. The real `--session-daemon` process exits (releasing the
/// flock); the in-process test daemon only *flags* shutdown (so it never kills the
/// `cargo test` process) and the harness observes the flag + the dropped socket.
#[cfg(unix)]
enum TeardownMode {
    /// Production: unlink the socket then `process::exit(0)` (the flock releases on death).
    Exit,
    /// Tests: just set the shutdown latch + unlink the socket; the in-process accept thread
    /// and the test both poll [`Lifecycle::is_shutting_down`]. Never touches the process.
    #[cfg(test)]
    FlagOnly,
}

/// Shared M3 lifecycle state: the connection counter, the shutdown latch, and the socket
/// path to unlink on exit. Both the idle monitor and the `Shutdown` dispatch funnel through
/// [`Lifecycle::shutdown`] so there is exactly one teardown path (unlink socket → exit 0,
/// or flag-only under test).
#[cfg(unix)]
struct Lifecycle {
    /// Currently-connected client count. Incremented when a connection thread starts,
    /// decremented when it ends. `0` is one half of the idle condition.
    active_conns: AtomicU64,
    /// Set once a shutdown has been initiated, so the teardown runs exactly once even if the
    /// idle monitor and a `Shutdown` message race. Also the in-process test observable.
    shutting_down: AtomicBool,
    /// The bound socket path, unlinked on the way out so a future daemon binds cleanly (the
    /// flock is kernel-released; the socket file is ours to remove).
    socket: PathBuf,
    /// Whether `shutdown` exits the process (production) or only flags (tests).
    mode: TeardownMode,
}

#[cfg(unix)]
impl Lifecycle {
    fn new(socket: PathBuf) -> Self {
        Lifecycle {
            active_conns: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
            socket,
            mode: TeardownMode::Exit,
        }
    }

    /// A connection started — bump the counter. Returns the new count.
    fn conn_opened(&self) -> u64 {
        self.active_conns.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// A connection ended — drop the counter.
    fn conn_closed(&self) {
        self.active_conns.fetch_sub(1, Ordering::SeqCst);
    }

    fn conn_count(&self) -> u64 {
        self.active_conns.load(Ordering::SeqCst)
    }

    /// Whether a shutdown has been initiated (the in-process test observable).
    #[cfg(test)]
    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Tear the daemon down exactly once: kill its PTYs, unlink the socket, then exit the
    /// process (production) or just leave the latch set (tests). Idempotent via the
    /// `shutting_down` latch — a `Shutdown` message that arrives just as the idle monitor
    /// fires won't double-tear-down. `kill` reaps the PTYs (a no-op for the idle path, which
    /// is already at 0 sessions).
    fn shutdown(&self, kill_sessions: impl FnOnce()) {
        // First-wins: only one caller runs the teardown body. In `Exit` mode a loser parks
        // until the winner's `exit` takes the process down; in `FlagOnly` mode it just returns.
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            match self.mode {
                TeardownMode::Exit => loop {
                    std::thread::sleep(Duration::from_millis(50));
                },
                #[cfg(test)]
                TeardownMode::FlagOnly => return,
            }
        }
        kill_sessions();
        let _ = std::fs::remove_file(&self.socket);
        match self.mode {
            TeardownMode::Exit => std::process::exit(0),
            #[cfg(test)]
            TeardownMode::FlagOnly => {
                // The latch is set + socket unlinked; the in-process accept thread sees the
                // flag and stops, and the test observes `is_shutting_down()`. No process exit.
            }
        }
    }
}

/// Lock + socket endpoints for a daemon salt — the daemon analog of the single-instance
/// names, kept here so the daemon's discovery is independent of the GUI gate's.
#[cfg(unix)]
struct DaemonNames {
    lock: PathBuf,
    socket: PathBuf,
}

/// The per-user runtime dir: `$XDG_RUNTIME_DIR` then `$TMPDIR` (absolute only), `/tmp`
/// last — the same resolution `single_instance::unix` uses.
#[cfg(unix)]
fn runtime_dir() -> PathBuf {
    for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Some(v) = std::env::var_os(var) {
            if !v.is_empty() {
                let p = PathBuf::from(v);
                if p.is_absolute() {
                    return p;
                }
            }
        }
    }
    PathBuf::from("/tmp")
}

/// FNV-1a (64-bit) — the same hash the single-instance gate uses to turn an arbitrary
/// salt (a userData path, with spaces/colons/slashes) into a fixed-width, namespace-safe
/// token. Tiny and dependency-free.
#[cfg(unix)]
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(unix)]
fn daemon_names(salt: &str) -> DaemonNames {
    let h = format!("{:016x}", fnv1a64(salt));
    let dir = runtime_dir();
    DaemonNames {
        lock: dir.join(format!("hyperpanesd.{h}.lock")),
        socket: dir.join(format!("hyperpanesd.{h}.sock")),
    }
}

/// The salted socket path a daemon for `salt` binds — the one a client connects to. Kept
/// `pub(crate)` so the M1 [`DaemonSessionManager`](crate::session::daemon_client) resolves
/// the SAME path the daemon's own [`run`]/[`connect`](DaemonClient::connect) use, without
/// re-deriving the hash (one source of truth for the name scheme).
#[cfg(unix)]
pub(crate) fn socket_path_for(salt: &str) -> PathBuf {
    daemon_names(salt).socket
}

/// **Kill the running daemon** for `salt` (the `--kill-daemon` entry, M3): connect to its
/// socket, send [`ClientMsg::Shutdown`], and wait (briefly) for it to exit (it unlinks the
/// socket on the way out). Returns `Ok(true)` if a daemon was running and asked to stop,
/// `Ok(false)` if none was listening (a clean no-op). Blocks only for the short teardown
/// confirmation, so the `--kill-daemon` CLI returns promptly.
#[cfg(unix)]
pub fn kill_daemon(salt: &str) -> io::Result<bool> {
    let socket = socket_path_for(salt);
    let mut stream = match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(s) => s,
        // No daemon listening (refused / no socket file) → nothing to kill.
        Err(e) if matches!(e.kind(), io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound) => {
            // A stale socket FILE with no listener: tidy it so a later launch binds cleanly.
            let _ = std::fs::remove_file(&socket);
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    write_frame(&mut stream, &ClientMsg::Shutdown)?;
    // Wait for the daemon to exit (it unlinks the socket). Bounded so the CLI never hangs.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !socket.exists() || std::os::unix::net::UnixStream::connect(&socket).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(true)
}

/// Tighten the socket to owner-only (`0600`), matching the single-instance trust boundary
/// (filesystem-scoped to the user, no network surface). Best-effort.
#[cfg(unix)]
fn restrict_socket_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// The running daemon: a [`SessionRegistry`] plus the broadcast channel its event pump
/// feeds. Cheaply cloneable (handles), so each connection thread holds its own copy.
#[cfg(unix)]
#[derive(Clone)]
struct Daemon {
    registry: SessionRegistry,
    /// Broadcasts every [`SessionEvent`] to all connection threads (each filters to the
    /// uids it has attached to). Bounded — a slow client that lags past the buffer just
    /// drops events (it can re-`Attach` to reseed), it never stalls the others.
    bus: tokio::sync::broadcast::Sender<SessionEvent>,
    /// Last sniffed cwd per uid, accumulated from `Cwd` events so `ListSessions` can
    /// report it (the registry tracks output counters but not the latest cwd).
    cwds: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// A handle to the daemon's Tokio runtime. Connection threads are plain OS threads
    /// (the framing I/O is blocking), so they are NOT inside the runtime's context — but
    /// `SessionRegistry::create` spawns the per-session pty driver via `tokio::spawn`,
    /// which requires it. The handler enters this handle before `create` so the driver
    /// task lands on the daemon's runtime.
    rt: tokio::runtime::Handle,
    /// M3 lifecycle: the shutdown latch + connection counter + socket-to-unlink. Shared with
    /// the idle monitor; the `Shutdown` dispatch and the monitor both call `shutdown` here.
    lifecycle: Arc<Lifecycle>,
}

#[cfg(unix)]
impl Daemon {
    /// Build the registry + event pump. The pump thread drains the registry's event
    /// receiver, records cwds, and rebroadcasts onto the bus.
    fn new(lifecycle: Arc<Lifecycle>) -> Self {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let registry = SessionRegistry::new(etx);
        let (bus, _) = tokio::sync::broadcast::channel::<SessionEvent>(4096);
        let cwds: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::default();

        // Event pump: registry mpsc → cwd cache + broadcast bus. Runs as a tokio task on
        // the ambient runtime (`run`/`spawn_in_process` enter one before constructing).
        let bus_tx = bus.clone();
        let cwds_pump = Arc::clone(&cwds);
        tokio::spawn(async move {
            while let Some(ev) = erx.recv().await {
                if let SessionEvent::Cwd { uid, cwd } = &ev {
                    cwds_pump.lock().unwrap().insert(uid.clone(), cwd.clone());
                }
                // `send` errors only when there are zero receivers — fine, just means no
                // client is currently attached; the event is simply not delivered live.
                let _ = bus_tx.send(ev);
            }
        });

        // Capture the ambient runtime's handle so connection threads (plain OS threads)
        // can enter it to spawn pty drivers — `new` is always called inside a runtime.
        let rt = tokio::runtime::Handle::current();
        Daemon { registry, bus, cwds, rt, lifecycle }
    }

    /// Start the **idle-exit monitor** on its own thread: once the daemon has 0 live sessions
    /// AND 0 connected clients, it arms a `grace` countdown and exits the process when the
    /// grace elapses while still idle. Any new connection or session resets the timer (the
    /// monitor re-observes a non-idle state and clears its armed instant). The grace spans a
    /// GUI crash→relaunch gap; a test-only short grace comes via `HYPERPANES_DAEMON_IDLE_MS`.
    ///
    /// The monitor exits the WHOLE process (through [`Lifecycle::shutdown`]) rather than
    /// unwinding `serve`, because the accept loop is parked in a blocking `accept()` — there
    /// is no live session or client to disturb, so a clean `exit(0)` is the right teardown.
    fn start_idle_monitor(&self, grace: Duration) {
        let lifecycle = Arc::clone(&self.lifecycle);
        let registry = self.registry.clone();
        std::thread::Builder::new()
            .name("hp-daemon-idle".into())
            .spawn(move || {
                // `armed` holds the instant the daemon first went idle; cleared whenever it
                // is busy again. Once `armed + grace` is in the past while still idle, exit.
                let mut armed: Option<Instant> = None;
                loop {
                    std::thread::sleep(Duration::from_millis(IDLE_POLL_MS));
                    let idle = registry.uids().is_empty() && lifecycle.conn_count() == 0;
                    if !idle {
                        armed = None; // busy → reset the countdown
                        continue;
                    }
                    match armed {
                        None => armed = Some(Instant::now()), // first idle tick — start counting
                        Some(since) if since.elapsed() >= grace => {
                            // Idle through the whole grace → exit. No sessions to kill (idle).
                            // In production `shutdown` never returns (process exit); under
                            // test it flags + returns, so we `break` to stop the monitor.
                            lifecycle.shutdown(|| {});
                            break;
                        }
                        Some(_) => {} // still within the grace window — keep waiting
                    }
                }
            })
            .expect("spawn idle monitor");
    }

    /// Accept connections forever, serving each on its own thread. In production this never
    /// returns — a `Shutdown` or the idle monitor exits the whole process out from under it.
    /// To let the in-process test daemon stop cleanly (it only *flags* shutdown rather than
    /// exiting the test process), the listener is non-blocking and the loop polls the
    /// shutdown latch between accepts, returning once it is set. Returns only on shutdown or
    /// an accept error severe enough to stop.
    fn serve(&self, listener: std::os::unix::net::UnixListener) -> io::Result<()> {
        // Non-blocking + a short poll interval, so the loop notices a flagged shutdown
        // promptly without a busy spin. (In production the process exits before this matters,
        // but the poll is cheap and keeps the test daemon tear-downable.)
        listener.set_nonblocking(true)?;
        loop {
            if self.lifecycle.shutting_down.load(Ordering::SeqCst) {
                return Ok(());
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    // Restore blocking mode for the per-connection framing I/O (the listener's
                    // non-blocking flag is inherited by the accepted stream on some platforms).
                    let _ = stream.set_nonblocking(false);
                    let daemon = self.clone();
                    std::thread::Builder::new()
                        .name("hp-daemon-conn".into())
                        .spawn(move || daemon.handle_connection(stream))?;
                }
                // No pending connection — sleep briefly then re-poll the shutdown latch. A
                // short poll keeps connection-accept latency low (GUI startup connects here)
                // while the idle monitor runs on its own coarser cadence.
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(ACCEPT_POLL_MS));
                }
                // One bad accept must not kill the daemon.
                Err(_) => continue,
            }
        }
    }

    /// Serve one client connection until it disconnects. Two halves share the socket: this
    /// thread reads + dispatches [`ClientMsg`]s, while a spawned writer thread forwards
    /// broadcast events (for attached uids) and the replies this thread hands it over a
    /// channel. The attached-uid set is shared between the halves.
    fn handle_connection(&self, stream: std::os::unix::net::UnixStream) {
        // Count this connection for the idle condition. The decrement on the way out is in a
        // guard so it runs on every exit path (clean close, malformed frame, panic).
        self.lifecycle.conn_opened();
        struct ConnGuard<'a>(&'a Lifecycle);
        impl Drop for ConnGuard<'_> {
            fn drop(&mut self) {
                self.0.conn_closed();
            }
        }
        let _conn_guard = ConnGuard(&self.lifecycle);

        let Ok(write_half) = stream.try_clone() else {
            return;
        };
        let mut read_half = stream;

        // Replies (Hello/Sessions/Replay/Screen/Pong/Created) and broadcast Events both
        // flow to the single writer thread over this channel, so all writes are serialized
        // on one thread (a socket write from two threads could interleave a frame).
        let (out_tx, out_rx) = std::sync::mpsc::channel::<DaemonMsg>();

        // The uids this connection has attached to → which broadcast events to forward.
        let attached: Arc<Mutex<HashSet<String>>> = Arc::default();

        // Writer thread: drain replies + forward subscribed broadcast events. It selects
        // across two sources by draining the reply channel first (non-blocking) then doing
        // a bounded poll on the bus, so neither source starves.
        let mut bus_rx = self.bus.subscribe();
        let attached_w = Arc::clone(&attached);
        let writer = std::thread::Builder::new()
            .name("hp-daemon-writer".into())
            .spawn(move || writer_loop(write_half, out_rx, &mut bus_rx, attached_w));

        // Reader/dispatch loop: handle each ClientMsg against the registry.
        loop {
            match read_frame::<_, ClientMsg>(&mut read_half) {
                Ok(Some(msg)) => {
                    if !self.dispatch(msg, &out_tx, &attached) {
                        break; // a control path asked to close the connection
                    }
                }
                Ok(None) => break,  // client closed cleanly
                Err(_) => break,    // malformed frame / socket error → drop the connection
            }
        }

        // Dropping `out_tx` ends the writer's reply drain; it then exits on its next bus
        // poll. Best-effort join so the thread is reaped.
        drop(out_tx);
        if let Ok(w) = writer {
            let _ = w.join();
        }
    }

    /// Handle one [`ClientMsg`]. Returns `false` to close the connection. Replies are sent
    /// to the writer thread via `out`; mutators act on the registry fire-and-forget.
    fn dispatch(
        &self,
        msg: ClientMsg,
        out: &std::sync::mpsc::Sender<DaemonMsg>,
        attached: &Arc<Mutex<HashSet<String>>>,
    ) -> bool {
        match msg {
            ClientMsg::Hello { proto_ver: _ } => {
                // The daemon always answers with ITS `PROTO_VER`; the CLIENT owns the mismatch
                // decision (M3): if the daemon's version differs from the client's, the client
                // sends `Shutdown` and respawns a fresh daemon (lock-step upgrades — see
                // `daemon_client`). The daemon need not reject the client here: a stale client
                // that ignores the reply simply talks to a daemon it can't fully understand,
                // which is exactly what the client-side mismatch check prevents.
                let _ = out.send(DaemonMsg::Hello {
                    proto_ver: PROTO_VER,
                    daemon_pid: std::process::id(),
                });
            }
            ClientMsg::ListSessions => {
                let _ = out.send(DaemonMsg::Sessions(self.list_sessions()));
            }
            ClientMsg::Attach { uid } => {
                // Subscribe this connection to the uid's live events, then return its
                // replay ONCE to seed a fresh grid (an empty string if it has none yet).
                attached.lock().unwrap().insert(uid.clone());
                let data = self.registry.replay(&uid).unwrap_or_default();
                let _ = out.send(DaemonMsg::Replay { uid, data });
            }
            ClientMsg::Create(spec) => {
                // The daemon is the uid source of truth: honor a pinned uid, else mint one.
                let uid = spec.uid.clone().unwrap_or_else(|| self.registry.mint_uid());
                let opts = spec.into_options(uid.clone());
                // AUTO-ATTACH the creating connection to the new uid BEFORE spawning. This
                // closes a lost-event race: a fast-exit command (e.g. `/bin/echo hi`) can
                // emit its entire Data + Exit stream in microseconds — potentially before
                // a separate `Attach` round-trip lands — and the writer only forwards
                // events for attached uids, so those events would be dropped. The writer's
                // broadcast subscription already exists (created at connection time), so
                // adding the uid to `attached` first guarantees every event from the
                // session's birth is forwarded to its creator. A separate `Attach` remains
                // for OTHER connections (and re-attach after a crash).
                attached.lock().unwrap().insert(uid.clone());
                // Enter the runtime so `create`'s `tokio::spawn` of the pty driver succeeds
                // from this plain OS thread, then SURFACE a spawn failure to the client
                // (M1 follow-up). Previously the result was swallowed (`let _created = …`),
                // so a pane whose pty failed to spawn (bad shell, ENOENT cwd that slipped
                // past the guard, fork/exec failure) produced NO events at all — the GUI
                // pane just hung blank with no Data and no Exit. Now we still reply
                // `Created` (the uid lets the client correlate its request and key its
                // shadow state), but on a spawn error we ALSO inject a synthetic
                // `Exit{code:-1}` onto the bus for the uid. The creator was just
                // auto-attached above, so the writer forwards it immediately — the pane
                // reflects a dead session instead of hanging. A successful create needs no
                // injection: the pty driver emits real Data/Exit. (`Exit` is the only
                // failure shape the wire carries today; a richer `DaemonMsg::Error` is a
                // possible M3 refinement, but `Exit` already drives the GUI's
                // pane-died path, so it reuses an existing, tested route.)
                let _guard = self.rt.enter();
                let created = self.registry.create(opts);
                drop(_guard);
                let _ = out.send(DaemonMsg::Created { uid: uid.clone() });
                if created.is_err() {
                    // Broadcast so EVERY connection attached to this uid (the creator, and
                    // any future re-attacher that races the failure) sees it; the pump's
                    // cwd cache has nothing to clean since no session was created.
                    let _ = self.bus.send(SessionEvent::Exit { uid, code: -1 });
                }
            }
            ClientMsg::Write { uid, data } => self.registry.write(&uid, &data),
            ClientMsg::Resize { uid, cols, rows } => self.registry.resize(&uid, cols, rows),
            ClientMsg::Kill { uid } => {
                self.registry.kill(&uid);
                self.cwds.lock().unwrap().remove(&uid);
            }
            ClientMsg::KillAll => {
                self.registry.kill_all();
                self.cwds.lock().unwrap().clear();
            }
            ClientMsg::RenderScreen { uid } => {
                let text = self.registry.render_screen(&uid);
                let _ = out.send(DaemonMsg::Screen { uid, text });
            }
            ClientMsg::Ping => {
                let _ = out.send(DaemonMsg::Pong);
            }
            ClientMsg::Shutdown => {
                // Kill every session, unlink the socket, and exit the process cleanly (the
                // flock releases on death). In production `shutdown` never returns (process
                // exit) — the connection simply drops and the client treats the resulting EOF
                // as the acknowledgement (no reply frame is sent). Under test it only flags;
                // returning `false` closes this connection promptly. Routed through the shared
                // `Lifecycle` so a `Shutdown` racing the idle monitor still tears down once.
                let registry = self.registry.clone();
                self.lifecycle.shutdown(move || registry.kill_all());
                return false;
            }
        }
        true
    }

    /// Snapshot every live session into [`SessionMeta`] (uid + counters + cached cwd).
    fn list_sessions(&self) -> Vec<SessionMeta> {
        let cwds = self.cwds.lock().unwrap();
        self.registry
            .uids()
            .into_iter()
            .map(|uid| SessionMeta {
                cwd: cwds.get(&uid).cloned(),
                output_bytes: self.registry.output_bytes(&uid).unwrap_or(0),
                last_output_at: self.registry.last_output_at(&uid),
                alive: true,
                uid,
            })
            .collect()
    }
}

/// The per-connection writer loop: forward replies (drained first, non-blocking) and the
/// broadcast events for attached uids. Exits when the reply channel is closed (the reader
/// half dropped its sender) and the bus yields nothing pending.
#[cfg(unix)]
fn writer_loop(
    mut write_half: std::os::unix::net::UnixStream,
    out_rx: std::sync::mpsc::Receiver<DaemonMsg>,
    bus_rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
    attached: Arc<Mutex<HashSet<String>>>,
) {
    use std::sync::mpsc::TryRecvError;
    use tokio::sync::broadcast::error::TryRecvError as BusErr;

    loop {
        // 1) Flush any pending replies first (they're the responses the client awaits).
        let mut reader_gone = false;
        loop {
            match out_rx.try_recv() {
                Ok(reply) => {
                    if write_frame(&mut write_half, &reply).is_err() {
                        return; // client went away
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    reader_gone = true;
                    break;
                }
            }
        }

        // 2) Forward any broadcast events for uids this connection attached to. `bus_idle`
        // is set when the bus yields nothing more this pass (Empty or Closed).
        let bus_idle;
        loop {
            match bus_rx.try_recv() {
                Ok(ev) => {
                    let is_attached = attached.lock().unwrap().contains(event_uid(&ev));
                    if is_attached {
                        if write_frame(&mut write_half, &DaemonMsg::Event(ev)).is_err() {
                            return;
                        }
                    }
                }
                // Lagged: the bounded bus dropped events for this slow connection. It can
                // re-Attach to reseed; just keep going.
                Err(BusErr::Lagged(_)) => continue,
                // Empty (nothing pending) or Closed (no senders) → done draining this pass.
                Err(BusErr::Empty) | Err(BusErr::Closed) => {
                    bus_idle = true;
                    break;
                }
            }
        }

        // If the reader is gone AND we've drained the bus, the connection is finished.
        if reader_gone && bus_idle {
            return;
        }
        // Nothing to do this pass → brief sleep so we don't spin (M0 is a simple poll; a
        // future version could block on a unified async select). Short enough that
        // event/echo latency stays well under a frame.
        if bus_idle {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// The uid a session event pertains to (for the attached-set filter).
#[cfg(unix)]
fn event_uid(ev: &SessionEvent) -> &str {
    match ev {
        SessionEvent::Data { uid, .. }
        | SessionEvent::Cwd { uid, .. }
        | SessionEvent::Exit { uid, .. }
        | SessionEvent::CommandStart { uid }
        | SessionEvent::CommandEnd { uid, .. }
        | SessionEvent::PromptReady { uid }
        | SessionEvent::AgentState { uid, .. } => uid,
    }
}

/// A minimal blocking client for the daemon: connect, send requests, and receive streamed
/// events on a background thread. Used by the M0 loopback test and (extended) by M1's
/// `DaemonSessionManager`. Owns one socket.
#[cfg(unix)]
pub struct DaemonClient {
    write_half: Mutex<std::os::unix::net::UnixStream>,
    /// The reader thread feeds inbound [`DaemonMsg`]s here. Replies and streamed events
    /// share the channel; M1 will split them (shadow-state vs. the GUI event channel).
    inbox: std::sync::mpsc::Receiver<DaemonMsg>,
    _reader: std::thread::JoinHandle<()>,
}

#[cfg(unix)]
impl DaemonClient {
    /// Connect to the daemon serving `salt`. Errors if no daemon is listening (M1 adds the
    /// spawn-then-retry; M0 connects to an already-running daemon).
    pub fn connect(salt: &str) -> io::Result<Self> {
        let names = daemon_names(salt);
        Self::connect_path(&names.socket)
    }

    /// Connect to a daemon at an explicit socket path (used by tests with a temp socket).
    pub fn connect_path(path: &Path) -> io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        let read_half = stream.try_clone()?;
        let write_half = stream;

        // Reader thread: decode inbound frames forever, pushing them onto the inbox.
        let (tx, rx) = std::sync::mpsc::channel::<DaemonMsg>();
        let reader = std::thread::Builder::new()
            .name("hp-daemon-client-reader".into())
            .spawn(move || {
                let mut r = read_half;
                loop {
                    match read_frame::<_, DaemonMsg>(&mut r) {
                        Ok(Some(msg)) => {
                            if tx.send(msg).is_err() {
                                break; // client dropped
                            }
                        }
                        // Clean EOF (peer closed) or a malformed-frame/socket error: the
                        // connection is finished, so stop reading.
                        Ok(None) | Err(_) => break,
                    }
                }
            })?;

        Ok(DaemonClient { write_half: Mutex::new(write_half), inbox: rx, _reader: reader })
    }

    /// Send one request to the daemon (length-framed). Fire-and-forget at this layer —
    /// callers that expect a reply read it off [`recv`](DaemonClient::recv).
    pub fn send(&self, msg: &ClientMsg) -> io::Result<()> {
        let mut w = self.write_half.lock().unwrap();
        write_frame(&mut *w, msg)
    }

    /// Block for the next inbound [`DaemonMsg`] (reply or streamed event), up to `timeout`.
    /// `None` on timeout or a closed connection.
    pub fn recv(&self, timeout: std::time::Duration) -> Option<DaemonMsg> {
        self.inbox.recv_timeout(timeout).ok()
    }
}

#[cfg(unix)]
impl Drop for DaemonClient {
    /// Explicitly shut down both directions of the socket on drop. The reader thread holds a
    /// CLONED read half of the same fd, so simply dropping the client would leave that fd
    /// open — the daemon would never see an EOF and would treat the connection as still alive
    /// (wedging the idle-exit's "0 clients" condition forever). A `shutdown(Both)` signals
    /// EOF to the daemon's connection reader AND unblocks our own reader thread's `read`, so
    /// the connection truly closes and the daemon's `conn_count` returns to zero.
    fn drop(&mut self) {
        if let Ok(w) = self.write_half.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// A running in-process daemon for tests: it owns a DEDICATED multi-thread Tokio runtime
/// (built on a background thread, exactly like [`run`] does) so the daemon's event pump +
/// pty drivers run on their own scheduler — not on the test harness's shared runtime,
/// where 400+ concurrent tests could starve the 16 ms batch timer and make the pty-output
/// assertions flaky. Dropping it lets the runtime + accept thread tear down.
///
/// Holds the shared [`Lifecycle`] (in [`TeardownMode::FlagOnly`], so a `Shutdown` or the
/// idle monitor flags + unlinks the socket WITHOUT exiting the `cargo test` process) so M3
/// tests can assert `is_shutting_down()` and the socket's removal.
#[cfg(all(unix, test))]
pub(crate) struct InProcessDaemon {
    _accept: std::thread::JoinHandle<()>,
    lifecycle: Arc<Lifecycle>,
    socket: PathBuf,
}

#[cfg(all(unix, test))]
impl InProcessDaemon {
    /// Whether the daemon has begun (test-mode) shutdown — set by a `Shutdown` message or the
    /// idle monitor firing. `pub(crate)` so the M3 `daemon_client` tests can assert teardown.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.lifecycle.is_shutting_down()
    }

    /// The bound socket path (gone once shutdown has unlinked it).
    #[allow(dead_code)]
    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }
}

/// Start a daemon **in-process** on an explicit socket path, for tests. The daemon gets
/// its own runtime (see [`InProcessDaemon`]); this returns once the listener is bound, so
/// a client can connect immediately. No flock (the temp path is unique per test/run).
/// `pub(crate)` so the M1 `daemon_client` tests can stand up a real daemon to exercise
/// [`DaemonSessionManager`](crate::session::daemon_client) against — the same harness M0's
/// own loopback tests use. No idle monitor (M0/M1 tests don't want an idle-exit racing
/// them); the M3 idle test uses [`spawn_in_process_with_idle`].
#[cfg(all(unix, test))]
pub(crate) fn spawn_in_process(socket: &Path) -> io::Result<InProcessDaemon> {
    spawn_in_process_inner(socket, None)
}

/// Like [`spawn_in_process`] but arms the idle-exit monitor with `grace` (the M3 idle test).
#[cfg(all(unix, test))]
pub(crate) fn spawn_in_process_with_idle(
    socket: &Path,
    grace: Duration,
) -> io::Result<InProcessDaemon> {
    spawn_in_process_inner(socket, Some(grace))
}

#[cfg(all(unix, test))]
fn spawn_in_process_inner(socket: &Path, idle_grace: Option<Duration>) -> io::Result<InProcessDaemon> {
    let _ = std::fs::remove_file(socket);
    let listener = std::os::unix::net::UnixListener::bind(socket)?;
    // FlagOnly teardown: a test-mode `Shutdown`/idle-exit must not kill the test process.
    let lifecycle = Arc::new(Lifecycle {
        active_conns: AtomicU64::new(0),
        shutting_down: AtomicBool::new(false),
        socket: socket.to_path_buf(),
        mode: TeardownMode::FlagOnly,
    });
    let lifecycle_thread = Arc::clone(&lifecycle);
    let accept = std::thread::Builder::new()
        .name("hp-daemon-accept".into())
        .spawn(move || {
            // The daemon's own runtime — its async work (pump + pty drivers) lives here,
            // isolated from the test harness's scheduling.
            let Ok(rt) = tokio::runtime::Runtime::new() else { return };
            let _guard = rt.enter();
            let daemon = Daemon::new(lifecycle_thread);
            if let Some(grace) = idle_grace {
                daemon.start_idle_monitor(grace);
            }
            let _ = daemon.serve(listener);
        })?;
    Ok(InProcessDaemon { _accept: accept, lifecycle, socket: socket.to_path_buf() })
}

/// Non-unix `run` stub: the daemon transport (UDS) is unix-only. Windows named pipes are
/// sketched in [`windows`](self::windows) (**WINDOWS-CI-PENDING**, M3); the full serve-loop
/// integration is M4, so `run` still returns `Unsupported` on Windows today and
/// `SessionManager::new_daemon` falls back to in-process.
#[cfg(not(unix))]
pub fn run(_salt: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the session daemon transport is unix-only; Windows named-pipe transport is WINDOWS-CI-PENDING (M3/M4)",
    ))
}

/// Non-unix `kill_daemon` stub — no out-of-process daemon exists on Windows yet (M4), so
/// there is nothing to kill: a clean no-op.
#[cfg(not(unix))]
pub fn kill_daemon(_salt: &str) -> io::Result<bool> {
    Ok(false)
}

// Windows named-pipe daemon transport — best-effort sketch, **WINDOWS-CI-PENDING** (can't be
// compiled/run on the Linux dev box). Mirrors `single_instance::windows`. The file lives at
// `session/windows.rs` (a sibling of this `session/daemon.rs`), so point `#[path]` at it
// rather than the default `session/daemon/windows.rs`.
#[cfg(windows)]
#[path = "windows.rs"]
pub mod windows;

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use crate::session::proto::SpawnSpec;
    use std::time::{Duration, Instant};

    // A unique temp socket path per test AND per run (pid + thread id), so parallel and
    // repeated runs never collide on the bind path.
    fn temp_socket(tag: &str) -> PathBuf {
        // Unix-socket paths (`sun_path`) are capped: 108 bytes on Linux but only **104 on
        // macOS/BSD** -- which is shorter than the macOS per-user temp dir
        // (`/var/folders/.../T/`) plus a descriptive name, so a long `tag` overflows and
        // `bind` fails with EINVAL. (In CI this showed up as `.expect("binds")` panicking
        // only on macOS, and only for the longest tags.) Build a compact name and fall back
        // to a short dir when the runtime-dir path would overflow.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let name = format!(
            "hp-{}-{}-{tag}.sock",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let candidate = runtime_dir().join(&name);
        // 104 = macOS/BSD sun_path cap (incl. trailing NUL); keep margin.
        if candidate.as_os_str().len() < 100 {
            candidate
        } else {
            std::path::Path::new("/tmp").join(&name)
        }
    }

    // Drain a client's inbox until `pred` matches one message or the deadline passes.
    // Returns the matching message if found.
    fn recv_until(
        client: &DaemonClient,
        timeout: Duration,
        mut pred: impl FnMut(&DaemonMsg) -> bool,
    ) -> Option<DaemonMsg> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match client.recv(Duration::from_millis(200)) {
                Some(m) if pred(&m) => return Some(m),
                Some(_) => continue,
                None => continue,
            }
        }
        None
    }

    // The headless integration test the plan calls for (M0 acceptance): start the daemon
    // in-process on a temp socket, connect a DaemonClient, Create a session running a
    // short command, assert a Data event containing "hi" and an Exit{code:0}, then attach
    // a SECOND client and assert Attach returns the replay.
    #[test]
    fn loopback_create_streams_data_and_exit_then_second_client_gets_replay() {
        let socket = temp_socket("loopback");
        let _accept = spawn_in_process(&socket).expect("daemon binds");

        let client = DaemonClient::connect_path(&socket).expect("client connects");
        client.send(&ClientMsg::Hello { proto_ver: PROTO_VER }).unwrap();
        let hello = recv_until(&client, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Hello { .. }));
        assert!(matches!(hello, Some(DaemonMsg::Hello { .. })), "handshake reply");

        // Create the session and learn its (daemon-minted) uid from the Created reply.
        client
            .send(&ClientMsg::Create(SpawnSpec {
                // `/bin/sh -c "sleep 0.3; echo hi"` — direct spawn (command + non-empty
                // args → no extra shell wrap, verbatim argv). The 0.3s `sleep` is the
                // whole point: it holds output back until AFTER this client's `Attach`
                // round-trip has registered it as a live subscriber, so the `Data{"hi"}`
                // and `Exit{0}` are observed LIVE and deterministically. A bare one-shot
                // (`/bin/echo hi`) would emit + exit in microseconds — before `Attach`
                // lands — making the live stream a race and the post-exit replay empty
                // (the registry drops a session on exit). `/bin/sh` is gated to unix
                // (the whole module is `#[cfg(unix)]`).
                command: Some("/bin/sh".into()),
                args: Some(vec!["-c".into(), "sleep 0.3; echo hi".into()]),
                ..Default::default()
            }))
            .unwrap();
        let created = recv_until(&client, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Created { .. }))
            .expect("Created reply");
        let DaemonMsg::Created { uid } = created else { unreachable!() };

        // Attach so we receive this session's streamed events (Data / Exit).
        client.send(&ClientMsg::Attach { uid: uid.clone() }).unwrap();
        // The Attach reply is the (initially empty) replay.
        let replay = recv_until(&client, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Replay { .. }))
            .expect("Replay reply");
        assert!(matches!(replay, DaemonMsg::Replay { .. }));

        // Assert a Data event containing "hi".
        let data = recv_until(&client, Duration::from_secs(10), |m| {
            matches!(m, DaemonMsg::Event(SessionEvent::Data { data, .. }) if data.contains("hi"))
        });
        assert!(data.is_some(), "expected a Data event containing 'hi', timed out");

        // Assert an Exit{code:0}.
        let exit = recv_until(&client, Duration::from_secs(10), |m| {
            matches!(m, DaemonMsg::Event(SessionEvent::Exit { code, .. }) if *code == 0)
        });
        assert!(exit.is_some(), "expected an Exit{{code:0}} event, timed out");

        // A SECOND client attaches and Attach returns the replay for the uid (the
        // protocol round-trip a re-attaching GUI uses to seed a fresh grid). This command
        // has now EXITED (we observed its Exit above), so the daemon has dropped it and
        // the replay is empty — exactly the in-process model (a dead uid is gone; M2's
        // reconnect re-spawns those). The non-empty "replay seeds a re-attach" guarantee
        // for a SURVIVING session is asserted in
        // `second_client_attach_replays_a_surviving_sessions_output`.
        let client2 = DaemonClient::connect_path(&socket).expect("second client connects");
        // ListSessions over the second client confirms multiple clients can query.
        client2.send(&ClientMsg::ListSessions).unwrap();
        let _ = recv_until(&client2, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Sessions(_)));

        client2.send(&ClientMsg::Attach { uid: uid.clone() }).unwrap();
        let replay2 = recv_until(&client2, Duration::from_secs(2), |m| {
            matches!(m, DaemonMsg::Replay { uid: u, .. } if *u == uid)
        });
        assert!(matches!(replay2, Some(DaemonMsg::Replay { .. })), "second client gets a Replay for the uid");
    }

    // The "replay seeds a re-attach" payoff on a SURVIVING session: a second client that
    // attaches after the session has produced output gets that output back in the Attach
    // reply (so it can prime a fresh grid without a restart — the whole point of M2).
    #[test]
    fn second_client_attach_replays_a_surviving_sessions_output() {
        let socket = temp_socket("replay-survive");
        let _accept = spawn_in_process(&socket).expect("daemon binds");

        // A long-lived interactive shell that stays in the registry after producing output.
        let a = DaemonClient::connect_path(&socket).expect("client A connects");
        a.send(&ClientMsg::Create(SpawnSpec {
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        }))
        .unwrap();
        let created = recv_until(&a, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Created { .. }))
            .expect("Created");
        let DaemonMsg::Created { uid } = created else { unreachable!() };

        // Attach A, drive a marker, and wait until A has seen it stream — guaranteeing the
        // daemon's replay buffer for this still-alive session now holds the marker.
        a.send(&ClientMsg::Attach { uid: uid.clone() }).unwrap();
        assert!(recv_until(&a, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Replay { .. })).is_some());
        a.send(&ClientMsg::Write { uid: uid.clone(), data: "echo REPLAY_MARKER\n".into() }).unwrap();
        assert!(
            recv_until(&a, Duration::from_secs(10), |m| {
                matches!(m, DaemonMsg::Event(SessionEvent::Data { data, .. }) if data.contains("REPLAY_MARKER"))
            })
            .is_some(),
            "client A should first see the marker stream live"
        );

        // Now a SECOND client attaches to the surviving session — its Replay must contain
        // the marker (the daemon seeds the re-attach from the retained replay buffer).
        let b = DaemonClient::connect_path(&socket).expect("client B connects");
        b.send(&ClientMsg::Attach { uid: uid.clone() }).unwrap();
        let replay = recv_until(&b, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Replay { .. }))
            .expect("B gets a Replay");
        if let DaemonMsg::Replay { data, .. } = replay {
            assert!(
                data.contains("REPLAY_MARKER"),
                "a re-attaching client's replay should carry the surviving session's output, got {data:?}"
            );
        }

        a.send(&ClientMsg::Kill { uid }).unwrap();
    }

    // Multiple clients attach to the SAME session and both receive its streamed events —
    // the multiplexing the daemon exists to provide. Drive a long-lived `sh -i` and feed
    // it a line; both attached clients must see the echoed Data.
    #[test]
    fn two_clients_attached_to_one_session_both_receive_data() {
        let socket = temp_socket("multiplex");
        let _accept = spawn_in_process(&socket).expect("daemon binds");

        let a = DaemonClient::connect_path(&socket).expect("client A connects");
        a.send(&ClientMsg::Create(SpawnSpec {
            // An interactive shell so the session stays alive while we write to it.
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        }))
        .unwrap();
        let created = recv_until(&a, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Created { .. }))
            .expect("Created");
        let DaemonMsg::Created { uid } = created else { unreachable!() };

        let b = DaemonClient::connect_path(&socket).expect("client B connects");

        // Both attach to the same uid.
        a.send(&ClientMsg::Attach { uid: uid.clone() }).unwrap();
        b.send(&ClientMsg::Attach { uid: uid.clone() }).unwrap();
        assert!(recv_until(&a, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Replay { .. })).is_some());
        assert!(recv_until(&b, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Replay { .. })).is_some());

        // Write a marker through one client; the shell echoes it. Both attached clients
        // should see it stream as a Data event.
        a.send(&ClientMsg::Write { uid: uid.clone(), data: "echo MUX_MARKER\n".into() }).unwrap();

        let saw = |c: &DaemonClient| {
            recv_until(c, Duration::from_secs(10), |m| {
                matches!(m, DaemonMsg::Event(SessionEvent::Data { data, .. }) if data.contains("MUX_MARKER"))
            })
            .is_some()
        };
        assert!(saw(&a), "client A should see the echoed marker");
        assert!(saw(&b), "client B should see the echoed marker");

        // Clean up the long-lived session.
        a.send(&ClientMsg::Kill { uid }).unwrap();
    }

    // Ping/Pong + ListSessions on an empty daemon — the request/response paths with no
    // sessions in play (smoke that the writer thread serializes replies correctly).
    #[test]
    fn ping_and_empty_list_sessions() {
        let socket = temp_socket("ping");
        let _accept = spawn_in_process(&socket).expect("daemon binds");
        let c = DaemonClient::connect_path(&socket).expect("connect");

        c.send(&ClientMsg::Ping).unwrap();
        assert!(matches!(
            recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Pong)),
            Some(DaemonMsg::Pong)
        ));

        c.send(&ClientMsg::ListSessions).unwrap();
        let sessions = recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Sessions(_)));
        assert!(matches!(sessions, Some(DaemonMsg::Sessions(v)) if v.is_empty()), "no sessions yet");
    }

    #[test]
    fn daemon_names_are_deterministic_and_hashed() {
        let a = daemon_names("C:\\Users\\me\\AppData\\Roaming\\hyperpanes");
        let b = daemon_names("C:\\Users\\me\\AppData\\Roaming\\hyperpanes");
        assert_eq!(a.socket, b.socket);
        assert_eq!(a.lock, b.lock);
        // Distinct salts → distinct names.
        let c = daemon_names("someone-else");
        assert_ne!(a.socket, c.socket);
        // The salt is hashed to a 16-hex token, never embedded raw.
        let fname = a.socket.file_name().unwrap().to_string_lossy();
        assert!(fname.starts_with("hyperpanesd."), "got {fname}");
        assert!(fname.ends_with(".sock"));
    }

    // ====================== M3 lifecycle ======================

    // Spin until `cond` is true or the deadline passes (for the async shutdown-latch flip).
    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    // IDLE-EXIT (the happy path): a daemon with 0 sessions AND 0 clients exits within the
    // (short, test-only) grace. We assert the shutdown latch flips and the socket is unlinked.
    #[test]
    fn idle_daemon_exits_within_grace() {
        let socket = temp_socket("idle-exit");
        // A 150 ms grace; the monitor polls every IDLE_POLL_MS(=100). Connect briefly so the
        // counter goes 1→0 (exercises the reset-then-arm path), then disconnect and wait.
        let daemon = spawn_in_process_with_idle(&socket, Duration::from_millis(150)).expect("binds");
        {
            let c = DaemonClient::connect_path(&socket).expect("connect");
            c.send(&ClientMsg::Ping).unwrap();
            assert!(recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Pong)).is_some());
            // Drop the client → connection count returns to 0; idle countdown can arm.
        }
        assert!(
            wait_until(Duration::from_secs(3), || daemon.is_shutting_down()),
            "an idle daemon should exit within the grace"
        );
        assert!(
            wait_until(Duration::from_secs(1), || !daemon.socket().exists()),
            "shutdown unlinks the socket"
        );
    }

    // IDLE-EXIT does NOT fire while a CLIENT is attached: the connection counter holds the
    // daemon alive past several grace windows.
    #[test]
    fn idle_exit_is_held_off_by_a_connected_client() {
        let socket = temp_socket("idle-client-held");
        let daemon = spawn_in_process_with_idle(&socket, Duration::from_millis(100)).expect("binds");
        let c = DaemonClient::connect_path(&socket).expect("connect");
        c.send(&ClientMsg::Ping).unwrap();
        assert!(recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Pong)).is_some());
        // Hold the connection open well past many grace windows — must NOT shut down.
        std::thread::sleep(Duration::from_millis(600));
        assert!(!daemon.is_shutting_down(), "a connected client must keep the daemon alive");
        drop(c);
    }

    // IDLE-EXIT does NOT fire while a SESSION is live, even after the client disconnects: the
    // registry's live-session count is the other half of the idle condition.
    #[test]
    fn idle_exit_is_held_off_by_a_live_session() {
        let socket = temp_socket("idle-session-held");
        let daemon = spawn_in_process_with_idle(&socket, Duration::from_millis(100)).expect("binds");

        // Create a long-lived interactive shell, then DROP the client so 0 clients remain —
        // the live session alone must keep the daemon alive.
        let uid = {
            let c = DaemonClient::connect_path(&socket).expect("connect");
            c.send(&ClientMsg::Create(SpawnSpec {
                shell: Some("/bin/sh".into()),
                args: Some(vec!["-i".into()]),
                ..Default::default()
            }))
            .unwrap();
            let created = recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Created { .. }))
                .expect("Created");
            let DaemonMsg::Created { uid } = created else { unreachable!() };
            // Confirm it's registered before dropping the client (drive a marker echo).
            c.send(&ClientMsg::Attach { uid: uid.clone() }).unwrap();
            assert!(recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Replay { .. })).is_some());
            c.send(&ClientMsg::Write { uid: uid.clone(), data: "echo HELD\n".into() }).unwrap();
            assert!(
                recv_until(&c, Duration::from_secs(10), |m| {
                    matches!(m, DaemonMsg::Event(SessionEvent::Data { data, .. }) if data.contains("HELD"))
                })
                .is_some(),
                "the session should be live before we drop the client"
            );
            uid
        };
        // 0 clients now, but the session lives → must NOT shut down across grace windows.
        std::thread::sleep(Duration::from_millis(600));
        assert!(!daemon.is_shutting_down(), "a live session must keep the daemon alive with no clients");

        // Kill the session → now fully idle → it should exit within the grace.
        let c = DaemonClient::connect_path(&socket).expect("reconnect");
        c.send(&ClientMsg::Kill { uid }).unwrap();
        drop(c);
        assert!(
            wait_until(Duration::from_secs(3), || daemon.is_shutting_down()),
            "once the last session dies and clients leave, the idle grace exits the daemon"
        );
    }

    // SHUTDOWN message: a `ClientMsg::Shutdown` kills sessions, unlinks the socket, and (in
    // production) exits — here, in FlagOnly mode, it flips the latch and removes the socket.
    #[test]
    fn shutdown_message_tears_the_daemon_down() {
        let socket = temp_socket("shutdown");
        // No idle monitor — the shutdown must come purely from the message.
        let daemon = spawn_in_process(&socket).expect("binds");

        // A live session, to prove `Shutdown` kills it on the way out.
        let c = DaemonClient::connect_path(&socket).expect("connect");
        c.send(&ClientMsg::Create(SpawnSpec {
            shell: Some("/bin/sh".into()),
            args: Some(vec!["-i".into()]),
            ..Default::default()
        }))
        .unwrap();
        assert!(recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Created { .. })).is_some());

        assert!(!daemon.is_shutting_down(), "not shutting down before the message");
        c.send(&ClientMsg::Shutdown).unwrap();

        assert!(
            wait_until(Duration::from_secs(3), || daemon.is_shutting_down()),
            "Shutdown should tear the daemon down"
        );
        assert!(
            wait_until(Duration::from_secs(1), || !daemon.socket().exists()),
            "Shutdown unlinks the socket so a future daemon binds cleanly"
        );
        drop(c);
    }

    // STALE-SOCKET RECLAIM: a leftover socket FILE from a dead daemon (no listener behind it)
    // must not block a new daemon's bind — `run` removes it while holding the flock. We
    // simulate by binding-then-leaking a socket path, then proving a fresh in-process daemon
    // re-binds the same path (its `spawn_in_process` removes the leftover first, mirroring
    // `run`'s reclaim) and serves.
    #[test]
    fn stale_socket_is_reclaimed_on_startup() {
        let socket = temp_socket("stale-sock");
        // Leave a stale socket FILE at the path (bind then forget — the listener is dropped
        // but on Linux the path lingers; exactly the dead-daemon-leftover shape).
        {
            let stale = std::os::unix::net::UnixListener::bind(&socket).expect("stale bind");
            drop(stale);
        }
        assert!(socket.exists(), "a stale socket file is present before startup");

        // A fresh daemon on the SAME path must reclaim it and serve a connection.
        let _daemon = spawn_in_process(&socket).expect("a stale socket must be reclaimed, not block bind");
        let c = DaemonClient::connect_path(&socket).expect("connect to the reclaimed daemon");
        c.send(&ClientMsg::Ping).unwrap();
        assert!(
            recv_until(&c, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Pong)).is_some(),
            "the reclaimed daemon serves normally"
        );
    }

    // Socket perms are owner-only (0600) after bind — the trust boundary (no cross-user
    // access to the daemon's control channel).
    #[test]
    fn bound_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let socket = temp_socket("perms");
        let _daemon = spawn_in_process(&socket).expect("binds");
        // The in-process harness binds directly; assert run()'s restrict step on the same path.
        restrict_socket_perms(&socket);
        let mode = std::fs::metadata(&socket).expect("stat socket").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the daemon socket must be owner-only, got {mode:o}");
    }

    // The idle-grace override parses (the test hook for a short grace). Tested through the
    // pure `idle_grace_from` so it never mutates the process env (which would race the other
    // env-sensitive tests under the parallel runner).
    #[test]
    fn idle_grace_honors_the_override() {
        assert_eq!(idle_grace_from(Some("250")), Duration::from_millis(250));
        assert_eq!(idle_grace_from(None), Duration::from_millis(DEFAULT_IDLE_GRACE_MS));
        // An unparseable value falls back to the default rather than erroring.
        assert_eq!(idle_grace_from(Some("not-a-number")), Duration::from_millis(DEFAULT_IDLE_GRACE_MS));
    }
}
