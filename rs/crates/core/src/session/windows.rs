//! **Windows named-pipe daemon transport** — best-effort M3 sketch, **WINDOWS-CI-PENDING**.
//!
//! ⚠️ This module is written carefully against the proven `single_instance::windows`
//! named-pipe sibling, BUT it has NOT been compiled or run: the development box is Linux, and
//! everything here is `#[cfg(windows)]`, so neither `cargo build` nor `cargo test` exercises a
//! single line. Treat it as a reviewed design, not a verified one. It exists so M4 can flip
//! the daemon default-on cross-platform with a small, contained amount of remaining Windows
//! work rather than a from-scratch port.
//!
//! ## What is here
//! * [`pipe_name`] — the salted `\\.\pipe\hyperpanesd.<hash>` name, derived exactly like the
//!   unix socket path (FNV-1a of the salt → 16-hex token), so the client and daemon agree on
//!   one endpoint per salt without re-hashing.
//! * [`run`] — the daemon entry: a named-pipe server that accepts connections and serves each
//!   over the SAME [`session::proto`](crate::session::proto) framing the UDS daemon uses,
//!   driving the SAME [`SessionRegistry`]. Mirrors the unix [`serve`](super) +
//!   [`handle_connection`](super) split and the M3 lifecycle (idle-exit + `Shutdown`).
//! * [`kill_daemon`] — connect + send [`ClientMsg::Shutdown`] (the `--kill-daemon` path).
//! * [`connect`] — a client-side blocking-ish connect for the
//!   [`DaemonSessionManager`](crate::session::daemon_client) to use on Windows (M4 wires it).
//!
//! ## Why no single-daemon LOCK here (yet)
//! The unix daemon gates one-per-salt with an `flock`. On Windows the natural analog is a
//! **named mutex** (exactly what `single_instance::windows` uses as its detector). That is
//! left as a precise TODO below rather than guessed at, because the create-mutex /
//! `ERROR_ALREADY_EXISTS` detector + the "first-pipe-instance" server arming have to be
//! ordered correctly to avoid the same false-promote race the single-instance module
//! documents — and that ordering is exactly the kind of thing that needs a real Windows run to
//! confirm. See `// TODO(m3-windows)` markers.
//!
//! ## Async vs blocking
//! The UDS daemon serves each connection on a blocking OS thread (`std::os::unix::net`). The
//! Windows named-pipe API in `tokio` is async (`tokio::net::windows::named_pipe`), like the
//! single-instance pipe server. So this serve loop is `async` and runs on the daemon's Tokio
//! runtime — the pty drivers already need that runtime, so there is no extra cost. The framing
//! helpers ([`read_frame`]/[`write_frame`]) are transport-agnostic blocking `Read`/`Write`;
//! over an async pipe we read/write through a small buffering shim. **This shim is the part
//! most in need of a real Windows test** (the proto's `read_frame` wants a synchronous
//! `Read`, so we accumulate bytes from the async pipe and decode framed messages out of the
//! buffer — see [`PipeConn`]).

#![cfg(windows)]

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

use crate::session::proto::{ClientMsg, DaemonMsg, SessionMeta, PROTO_VER, MAX_FRAME_LEN};
use crate::session_manager::{SessionEvent, SessionRegistry};

// Re-use the same idle grace knob as the unix side (the env override is platform-agnostic).
const DEFAULT_IDLE_GRACE_MS: u64 = 30_000;

fn idle_grace() -> Duration {
    let ms = std::env::var("HYPERPANES_DAEMON_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_GRACE_MS);
    Duration::from_millis(ms)
}

/// FNV-1a (64-bit) — identical to the unix `daemon_names` hash (and the single-instance
/// gate's), so a Windows daemon and client derive the same salted pipe.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The salted named-pipe path a daemon for `salt` binds — the Windows analog of the unix
/// `socket_path_for`. One endpoint per salt; the client connects to the SAME name.
pub fn pipe_name(salt: &str) -> String {
    let h = format!("{:016x}", fnv1a64(salt));
    format!(r"\\.\pipe\hyperpanesd.{h}")
}

/// Shared M3 lifecycle (mirror of the unix `Lifecycle`): connection counter + shutdown latch.
/// On Windows there is no socket FILE to unlink (a named pipe vanishes when its last instance
/// closes), so teardown is just "stop accepting + exit".
struct Lifecycle {
    active_conns: AtomicU64,
    shutting_down: AtomicBool,
}

impl Lifecycle {
    fn new() -> Self {
        Lifecycle { active_conns: AtomicU64::new(0), shutting_down: AtomicBool::new(false) }
    }
    fn conn_opened(&self) {
        self.active_conns.fetch_add(1, Ordering::SeqCst);
    }
    fn conn_closed(&self) {
        self.active_conns.fetch_sub(1, Ordering::SeqCst);
    }
    fn conn_count(&self) -> u64 {
        self.active_conns.load(Ordering::SeqCst)
    }
    fn begin_shutdown(&self) -> bool {
        !self.shutting_down.swap(true, Ordering::SeqCst)
    }
    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

/// Run the Windows daemon for `salt`, blocking until exit — the `#[cfg(windows)]` body behind
/// `hyperpanes --session-daemon <salt>`. Builds a Tokio runtime (pty drivers need it), arms
/// the idle monitor, and serves the named pipe forever.
///
/// TODO(m3-windows): add a named-MUTEX one-daemon-per-salt gate BEFORE binding the pipe, the
/// way `single_instance::windows::create_named_mutex` does (create the mutex; if
/// `ERROR_ALREADY_EXISTS`, another daemon already serves this salt → return `AddrInUse`). The
/// pipe's `first_pipe_instance(true)` ALSO fails if a server already owns the name, which is a
/// secondary guard, but the mutex is the race-free detector (see the single-instance module's
/// "pipe-connect must NOT be the detector" note). Until then this `run` will fail to arm the
/// first pipe instance if a stale one exists, which is acceptable for the WINDOWS-CI-PENDING
/// state but not ideal.
pub fn run(salt: &str) -> io::Result<()> {
    let pipe = pipe_name(salt);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let daemon = Daemon::new();
        daemon.start_idle_monitor(idle_grace());
        daemon.serve(&pipe).await
    })
}

/// Kill the running Windows daemon for `salt` (the `--kill-daemon` path): connect + send
/// `Shutdown`. No-op if the pipe isn't there. Mirrors the unix `kill_daemon`.
pub fn kill_daemon(salt: &str) -> io::Result<bool> {
    let pipe = pipe_name(salt);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        match ClientOptions::new().open(&pipe) {
            Ok(mut client) => {
                let bytes = frame_bytes(&ClientMsg::Shutdown)?;
                client.write_all(&bytes).await?;
                client.flush().await?;
                let _ = client.shutdown().await;
                Ok(true)
            }
            // No server listening → nothing to kill.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    })
}

/// Client-side connect for the Windows `DaemonSessionManager` (M4 wires this in place of the
/// unix `UnixStream::connect`). Tolerates the brief `ERROR_PIPE_BUSY` window between server
/// instances, like `single_instance::windows::open_client`.
///
/// TODO(m3-windows): the unix client (`daemon_client.rs`) is built around a blocking
/// `std::os::unix::net::UnixStream` + a reader thread. The Windows pipe is async, so M4 must
/// either (a) wrap this async pipe behind a blocking adapter the existing reader thread can
/// use, or (b) add a `#[cfg(windows)]` reader task. (a) keeps the most code shared; this
/// function returns the raw async client for whichever path M4 picks.
pub async fn connect(
    salt: &str,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use windows::Win32::Foundation::ERROR_PIPE_BUSY;
    const PIPE_BUSY: i32 = ERROR_PIPE_BUSY.0 as i32;
    let pipe = pipe_name(salt);
    for _ in 0..50 {
        match ClientOptions::new().open(&pipe) {
            Ok(c) => return Ok(c),
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "named pipe stayed busy"))
}

/// The running Windows daemon — the same shape as the unix `Daemon` (a `SessionRegistry`, an
/// event broadcast bus, a cwd cache, the lifecycle), adapted to the async pipe transport.
#[derive(Clone)]
struct Daemon {
    registry: SessionRegistry,
    bus: tokio::sync::broadcast::Sender<SessionEvent>,
    cwds: Arc<Mutex<std::collections::HashMap<String, String>>>,
    lifecycle: Arc<Lifecycle>,
}

impl Daemon {
    fn new() -> Self {
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let registry = SessionRegistry::new(etx);
        let (bus, _) = tokio::sync::broadcast::channel::<SessionEvent>(4096);
        let cwds: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::default();

        // Event pump: registry mpsc → cwd cache + broadcast bus (identical to the unix pump).
        let bus_tx = bus.clone();
        let cwds_pump = Arc::clone(&cwds);
        tokio::spawn(async move {
            while let Some(ev) = erx.recv().await {
                if let SessionEvent::Cwd { uid, cwd } = &ev {
                    cwds_pump.lock().unwrap().insert(uid.clone(), cwd.clone());
                }
                let _ = bus_tx.send(ev);
            }
        });

        Daemon { registry, bus, cwds, lifecycle: Arc::new(Lifecycle::new()) }
    }

    /// Idle-exit monitor (mirror of the unix one): 0 sessions AND 0 clients through the grace
    /// → exit. On Windows we just `process::exit(0)` (no socket file to unlink).
    fn start_idle_monitor(&self, grace: Duration) {
        let lifecycle = Arc::clone(&self.lifecycle);
        let registry = self.registry.clone();
        std::thread::spawn(move || {
            let mut armed: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let idle = registry.uids().is_empty() && lifecycle.conn_count() == 0;
                if !idle {
                    armed = None;
                    continue;
                }
                match armed {
                    None => armed = Some(Instant::now()),
                    Some(since) if since.elapsed() >= grace => {
                        if lifecycle.begin_shutdown() {
                            registry.kill_all();
                            std::process::exit(0);
                        }
                        break;
                    }
                    Some(_) => {}
                }
            }
        });
    }

    /// Accept connections forever over the named pipe, serving each as its own task. Mirrors
    /// `single_instance::windows::run_server`'s pre-arm-the-next-instance pattern so a connect
    /// arriving while we hand off the current one is never refused.
    async fn serve(&self, pipe: &str) -> io::Result<()> {
        // TODO(m3-windows): `first_pipe_instance(true)` requires we are the FIRST server for
        // this name — pair it with the named-mutex gate (see `run`) so a stale instance is
        // detected race-free rather than surfacing here as a bind error.
        let mut server = ServerOptions::new().first_pipe_instance(true).create(pipe)?;
        loop {
            if self.lifecycle.is_shutting_down() {
                return Ok(());
            }
            // Wait for a client; re-arm on a failed connect.
            if server.connect().await.is_err() {
                server = ServerOptions::new().create(pipe)?;
                continue;
            }
            let connected = std::mem::replace(
                &mut server,
                ServerOptions::new().create(pipe)?, // pre-arm the next instance
            );
            let daemon = self.clone();
            tokio::spawn(async move {
                daemon.handle_connection(connected).await;
            });
        }
    }

    /// Serve one client over a connected pipe instance. Mirrors the unix `handle_connection`:
    /// read+dispatch `ClientMsg`s on this task while a sibling task forwards broadcast events
    /// for the uids this connection attached to. (Single-task simplification vs the unix
    /// two-thread split — the async pipe is full-duplex, so we can `tokio::select!` between the
    /// inbound frames and the bus on one task; the buffering shim feeds `read_frame`.)
    async fn handle_connection(&self, conn: NamedPipeServer) {
        self.lifecycle.conn_opened();
        let mut pc = PipeConn::new(conn);
        let mut bus_rx = self.bus.subscribe();
        let attached: std::collections::HashSet<String> = Default::default();
        let mut attached = attached;

        loop {
            tokio::select! {
                // Inbound client request.
                msg = pc.read_msg() => {
                    match msg {
                        Ok(Some(m)) => {
                            if !self.dispatch(m, &mut pc, &mut attached).await {
                                break; // a control path (Shutdown) asked to close
                            }
                        }
                        Ok(None) | Err(_) => break, // EOF / error → connection done
                    }
                }
                // Outbound broadcast event for an attached uid.
                ev = bus_rx.recv() => {
                    if let Ok(ev) = ev {
                        if attached.contains(event_uid(&ev)) {
                            if pc.write_msg(&DaemonMsg::Event(ev)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        }
        self.lifecycle.conn_closed();
    }

    /// Dispatch one `ClientMsg` (mirror of the unix `dispatch`). Returns `false` to close.
    async fn dispatch(
        &self,
        msg: ClientMsg,
        pc: &mut PipeConn,
        attached: &mut std::collections::HashSet<String>,
    ) -> bool {
        match msg {
            ClientMsg::Hello { .. } => {
                let _ = pc
                    .write_msg(&DaemonMsg::Hello { proto_ver: PROTO_VER, daemon_pid: std::process::id() })
                    .await;
            }
            ClientMsg::ListSessions => {
                let _ = pc.write_msg(&DaemonMsg::Sessions(self.list_sessions())).await;
            }
            ClientMsg::Attach { uid } => {
                attached.insert(uid.clone());
                let data = self.registry.replay(&uid).unwrap_or_default();
                let _ = pc.write_msg(&DaemonMsg::Replay { uid, data }).await;
            }
            ClientMsg::Create(spec) => {
                let uid = spec.uid.clone().unwrap_or_else(|| self.registry.mint_uid());
                attached.insert(uid.clone());
                let opts = spec.into_options(uid.clone());
                let created = self.registry.create(opts);
                let _ = pc.write_msg(&DaemonMsg::Created { uid: uid.clone() }).await;
                if created.is_err() {
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
                let _ = pc.write_msg(&DaemonMsg::Screen { uid, text }).await;
            }
            ClientMsg::Ping => {
                let _ = pc.write_msg(&DaemonMsg::Pong).await;
            }
            ClientMsg::Shutdown => {
                if self.lifecycle.begin_shutdown() {
                    self.registry.kill_all();
                    // No socket file to unlink on Windows; exit the process cleanly.
                    std::process::exit(0);
                }
                return false;
            }
        }
        true
    }

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

/// A buffering shim that decodes the proto's length-prefixed frames out of an async named
/// pipe. The proto's `read_frame`/`write_frame` want synchronous `Read`/`Write`; rather than
/// fight that, we re-implement the (tiny, identical) framing directly over the async pipe.
///
/// TODO(m3-windows): this duplicates the framing the proto module already has for synchronous
/// streams. If M4 adds a blocking adapter over the async pipe instead, this whole shim can be
/// dropped in favour of the shared `read_frame`/`write_frame`. Kept self-contained here so the
/// Windows transport is reviewable in one file.
struct PipeConn {
    pipe: NamedPipeServer,
    buf: Vec<u8>,
}

impl PipeConn {
    fn new(pipe: NamedPipeServer) -> Self {
        PipeConn { pipe, buf: Vec::with_capacity(8192) }
    }

    /// Read the next framed `ClientMsg`, accumulating bytes until a whole frame is buffered.
    /// `Ok(None)` on a clean EOF between frames.
    async fn read_msg(&mut self) -> io::Result<Option<ClientMsg>> {
        loop {
            // Do we already have a complete frame buffered?
            if let Some(msg) = self.try_decode()? {
                return Ok(Some(msg));
            }
            let mut tmp = [0u8; 4096];
            match self.pipe.read(&mut tmp).await {
                Ok(0) => {
                    return if self.buf.is_empty() {
                        Ok(None) // clean EOF between frames
                    } else {
                        Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF mid-frame"))
                    };
                }
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    /// Try to pull one complete frame out of `buf` (a `u32` LE length then that many JSON
    /// bytes). Returns `Ok(None)` if not enough is buffered yet.
    fn try_decode(&mut self) -> io::Result<Option<ClientMsg>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len > MAX_FRAME_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds MAX_FRAME_LEN"));
        }
        let total = 4 + len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let body = self.buf[4..total].to_vec();
        self.buf.drain(..total);
        let msg = serde_json::from_slice(&body).map_err(io::Error::other)?;
        Ok(Some(msg))
    }

    /// Write one framed `DaemonMsg`.
    async fn write_msg(&mut self, msg: &DaemonMsg) -> io::Result<()> {
        let bytes = frame_bytes(msg)?;
        self.pipe.write_all(&bytes).await?;
        self.pipe.flush().await
    }
}

/// Serialize a message into a length-prefixed frame (the same wire shape as
/// `proto::write_frame`, materialized as bytes for the async writers here).
fn frame_bytes(msg: &impl serde::Serialize) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame body exceeds u32"))?;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame body exceeds MAX_FRAME_LEN"));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}
