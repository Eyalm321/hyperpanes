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
//! Idle-exit, proto-version enforcement, Windows pipes, and `--kill-daemon` are M3.

#[cfg(unix)]
use std::collections::HashSet;
use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use crate::session::proto::{
    read_frame, write_frame, ClientMsg, DaemonMsg, SessionMeta, PROTO_VER,
};
#[cfg(unix)]
use crate::session_manager::{SessionEvent, SessionRegistry};

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
        std::fs::create_dir_all(dir)?;
    }

    // The flock gates one daemon per salt (kernel-released on death, so a crashed daemon
    // never wedges the next launch — same contract as the single-instance detector).
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

    // We hold the flock, so any leftover socket file is a dead daemon's — safe to remove.
    let _ = std::fs::remove_file(&names.socket);
    let listener = std::os::unix::net::UnixListener::bind(&names.socket)?;
    restrict_socket_perms(&names.socket);

    // The pty drivers spawned by `SessionRegistry::create` need a Tokio runtime in scope.
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let daemon = Daemon::new();
    // Hold the lock for the daemon's whole lifetime (dropping it releases the salt).
    let _lock = lock;
    daemon.serve(listener)
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
}

#[cfg(unix)]
impl Daemon {
    /// Build the registry + event pump. The pump thread drains the registry's event
    /// receiver, records cwds, and rebroadcasts onto the bus.
    fn new() -> Self {
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
        Daemon { registry, bus, cwds, rt }
    }

    /// Accept connections forever, serving each on its own thread. Returns only on an
    /// accept error severe enough to stop (the normal path never returns).
    fn serve(&self, listener: std::os::unix::net::UnixListener) -> io::Result<()> {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let daemon = self.clone();
                    std::thread::Builder::new()
                        .name("hp-daemon-conn".into())
                        .spawn(move || daemon.handle_connection(stream))?;
                }
                // One bad accept must not kill the daemon.
                Err(_) => continue,
            }
        }
        Ok(())
    }

    /// Serve one client connection until it disconnects. Two halves share the socket: this
    /// thread reads + dispatches [`ClientMsg`]s, while a spawned writer thread forwards
    /// broadcast events (for attached uids) and the replies this thread hands it over a
    /// channel. The attached-uid set is shared between the halves.
    fn handle_connection(&self, stream: std::os::unix::net::UnixStream) {
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
                // M0 transports the version but does not enforce it (M3 kills-on-mismatch).
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
                // A spawn failure is swallowed here (M0 keeps create fire-and-forget on the
                // engine side; the client learns of a dead session via the absence of
                // events / a later ListSessions). The uid still lets it correlate the
                // request. Enter the runtime so `create`'s `tokio::spawn` of the pty driver
                // succeeds from this plain OS thread.
                let _guard = self.rt.enter();
                let _ = self.registry.create(opts);
                drop(_guard);
                let _ = out.send(DaemonMsg::Created { uid });
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
                    if attached.lock().unwrap().contains(event_uid(&ev)) {
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
        | SessionEvent::Exit { uid, .. } => uid,
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
                while let Ok(Some(msg)) = read_frame::<_, DaemonMsg>(&mut r) {
                    if tx.send(msg).is_err() {
                        break; // client dropped
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

/// Start a daemon **in-process** on an explicit socket path, for tests. Spawns the accept
/// loop on a background thread against the ambient Tokio runtime (the caller must already
/// be inside one — the pty drivers and the event pump need it). Returns once the listener
/// is bound, so a client can connect immediately. No flock (the temp path is unique).
#[cfg(all(unix, test))]
fn spawn_in_process(socket: &Path) -> io::Result<std::thread::JoinHandle<()>> {
    let _ = std::fs::remove_file(socket);
    let listener = std::os::unix::net::UnixListener::bind(socket)?;
    let daemon = Daemon::new();
    let handle = std::thread::Builder::new()
        .name("hp-daemon-accept".into())
        .spawn(move || {
            let _ = daemon.serve(listener);
        })?;
    Ok(handle)
}

/// Non-unix stub: the daemon transport (UDS) is unix-only in M0; Windows named pipes are
/// M3. `run` returns an `Unsupported` error so the `--session-daemon` entry degrades
/// gracefully on Windows until M3 lands.
#[cfg(not(unix))]
pub fn run(_salt: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the session daemon transport is unix-only in M0 (Windows named pipes are M3)",
    ))
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use crate::session::proto::SpawnSpec;
    use std::time::{Duration, Instant};

    // A unique temp socket path per test AND per run (pid + thread id), so parallel and
    // repeated runs never collide on the bind path.
    fn temp_socket(tag: &str) -> PathBuf {
        let dir = runtime_dir();
        dir.join(format!(
            "hp-daemon-test-{tag}-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ))
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_create_streams_data_and_exit_then_second_client_gets_replay() {
        let socket = temp_socket("loopback");
        let _accept = spawn_in_process(&socket).expect("daemon binds");

        // A short command that prints "hi" and exits 0. `/bin/echo` is gated to unix
        // (the whole module is `#[cfg(unix)]`); `printf` would work too.
        let client = DaemonClient::connect_path(&socket).expect("client connects");
        client.send(&ClientMsg::Hello { proto_ver: PROTO_VER }).unwrap();
        let hello = recv_until(&client, Duration::from_secs(2), |m| matches!(m, DaemonMsg::Hello { .. }));
        assert!(matches!(hello, Some(DaemonMsg::Hello { .. })), "handshake reply");

        // Create the session and learn its (daemon-minted) uid from the Created reply.
        client
            .send(&ClientMsg::Create(SpawnSpec {
                // Direct spawn `/bin/echo hi` (command + non-empty args → no shell,
                // verbatim argv). The command field also carries `/bin/echo` for the
                // direct-spawn path (resolve_spawn uses `command` as the file).
                command: Some("/bin/echo".into()),
                args: Some(vec!["hi".into()]),
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
        // protocol round-trip a re-attaching GUI uses to seed a fresh grid). This session
        // is a one-shot that has now EXITED, so the daemon has dropped it and the replay
        // is empty — exactly the in-process model (a dead uid is gone; M2's reconnect
        // re-spawns those). The non-empty "replay seeds a re-attach" guarantee for a
        // SURVIVING session is asserted in
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_client_attach_replays_a_surviving_sessions_output() {
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_clients_attached_to_one_session_both_receive_data() {
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ping_and_empty_list_sessions() {
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
}
