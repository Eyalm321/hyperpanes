//! Unix implementation of the single-instance seam (see `mod.rs` for the frozen
//! surface). Owned by the Wave-1 `unix-core` track.
//!
//! Mirrors the Windows split of detector vs hand-off:
//!
//! - **Detector = an exclusively-flocked lock file** under the user runtime dir
//!   (`$XDG_RUNTIME_DIR`, falling back to `$TMPDIR` then `/tmp`). `File::try_lock`
//!   is `flock(LOCK_EX | LOCK_NB)` on unix: the kernel makes acquire atomic and
//!   releases the lock when the holding process dies — so a crashed primary never
//!   wedges future launches, and a stale lock FILE (dead pid inside) is harmless
//!   because the flock itself is what gates, not the file's existence.
//! - **Hand-off = a unix-domain socket** carrying the same `{argv,cwd}` JSON as the
//!   Windows named pipe. On Linux it lives in the abstract namespace (no filesystem
//!   entry → nothing stale to clean up); on macOS it is a path socket in the runtime
//!   dir, and the new primary unlinks any leftover before binding (safe: holding the
//!   flock proves the previous primary is gone).

use super::*;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Lock + socket endpoints derived from one salt (the unix analog of `InstanceNames`).
#[derive(Debug, Clone)]
struct UnixNames {
    lock: PathBuf,
    /// Display form of the hand-off endpoint (`@name` marks the abstract namespace).
    endpoint: String,
    #[cfg(target_os = "linux")]
    abstract_name: String,
    #[cfg(not(target_os = "linux"))]
    socket: PathBuf,
}

/// The per-user runtime dir: `$XDG_RUNTIME_DIR` (Linux), `$TMPDIR` (macOS per-user
/// confstr dir), `/tmp` as the last resort. Relative values are ignored.
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

fn unix_names(salt: &str) -> UnixNames {
    let h = format!("{:016x}", fnv1a64(salt));
    let lock = runtime_dir().join(format!("hyperpanes.singleton.{h}.lock"));
    #[cfg(target_os = "linux")]
    {
        let name = format!("hyperpanes.handoff.{h}");
        UnixNames {
            lock,
            endpoint: format!("@{name}"),
            abstract_name: name,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let socket = runtime_dir().join(format!("hyperpanes.handoff.{h}.sock"));
        UnixNames {
            lock,
            endpoint: socket.to_string_lossy().into_owned(),
            socket,
        }
    }
}

pub fn acquire(salt: &str) -> io::Result<Instance> {
    let names = unix_names(salt);
    if let Some(dir) = names.lock.parent() {
        fs::create_dir_all(dir)?;
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&names.lock)?;
    match lock.try_lock() {
        Ok(()) => {
            // We hold the flock — any previous content is a dead primary's. Record
            // our pid purely for diagnostics (the flock is the actual gate).
            let _ = lock.set_len(0);
            let _ = (&lock).write_all(std::process::id().to_string().as_bytes());
            Ok(Instance::Primary(PrimaryInstance { names, _lock: lock }))
        }
        Err(fs::TryLockError::WouldBlock) => Ok(Instance::Secondary(SecondaryInstance { names })),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

pub struct PrimaryInstance {
    names: UnixNames,
    // Held for the app's lifetime (run_server consumes self to keep it alive). The
    // kernel drops the flock when the fd closes — including on process death. The
    // lock FILE is deliberately never unlinked: removing it would race a concurrent
    // launch that already opened the old inode.
    _lock: fs::File,
}

impl PrimaryInstance {
    /// The hand-off endpoint we serve (exposed mainly for diagnostics/tests).
    pub fn pipe_name(&self) -> &str {
        &self.names.endpoint
    }

    /// Accept hand-offs forever, invoking `handler` with each decoded
    /// [`HandoffMessage`]. Consumes `self` so the lock lives for as long as the
    /// server runs (typically the whole app). Spawn it on the tokio runtime:
    /// `tokio::spawn(primary.run_server(|msg| route(msg)))`.
    ///
    /// Per-connection errors (a secondary that died mid-send, a malformed payload)
    /// are swallowed and the loop continues — one bad launch must never take down
    /// the primary's hand-off channel. Only failure to bind the socket is fatal.
    pub async fn run_server<F>(self, mut handler: F) -> io::Result<()>
    where
        F: FnMut(HandoffMessage),
    {
        let listener = bind_listener(&self.names)?;
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let mut bytes = Vec::new();
            if stream.read_to_end(&mut bytes).await.is_ok() {
                if let Ok(msg) = serde_json::from_slice::<HandoffMessage>(&bytes) {
                    handler(msg);
                }
            }
        }
    }
}

pub struct SecondaryInstance {
    names: UnixNames,
}

impl SecondaryInstance {
    /// The hand-off endpoint we forward to (exposed mainly for diagnostics/tests).
    pub fn pipe_name(&self) -> &str {
        &self.names.endpoint
    }

    /// Connect to the primary's socket and send our `{argv,cwd}` as JSON. Retries
    /// briefly while the socket is not yet accepting (we may have lost the acquire
    /// race to a primary that is still between `acquire` and `run_server`).
    pub async fn forward(&self, msg: &HandoffMessage) -> io::Result<()> {
        let bytes = serde_json::to_vec(msg)?;
        let mut stream = connect_with_retry(&self.names).await?;
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        stream.shutdown().await?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn bind_listener(names: &UnixNames) -> io::Result<UnixListener> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(names.abstract_name.as_bytes())?;
    let listener = std::os::unix::net::UnixListener::bind_addr(&addr)?;
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

#[cfg(not(target_os = "linux"))]
fn bind_listener(names: &UnixNames) -> io::Result<UnixListener> {
    // We hold the flock, so an existing socket file is a dead primary's leftover.
    let _ = fs::remove_file(&names.socket);
    UnixListener::bind(&names.socket)
}

#[cfg(target_os = "linux")]
fn connect_once(names: &UnixNames) -> io::Result<UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(names.abstract_name.as_bytes())?;
    let stream = std::os::unix::net::UnixStream::connect_addr(&addr)?;
    stream.set_nonblocking(true)?;
    UnixStream::from_std(stream)
}

#[cfg(not(target_os = "linux"))]
fn connect_once(names: &UnixNames) -> io::Result<UnixStream> {
    let stream = std::os::unix::net::UnixStream::connect(&names.socket)?;
    stream.set_nonblocking(true)?;
    UnixStream::from_std(stream)
}

// Tolerate the brief window where the primary holds the flock but has not bound its
// listener yet (mirrors the Windows ERROR_PIPE_BUSY retry).
async fn connect_with_retry(names: &UnixNames) -> io::Result<UnixStream> {
    let mut last = io::Error::new(io::ErrorKind::TimedOut, "hand-off socket never came up");
    for _ in 0..50 {
        match connect_once(names) {
            Ok(s) => return Ok(s),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                last = e;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod unix_tests {
    use super::*;

    // Unique per test AND per run, so parallel/repeated runs never share a lock.
    fn test_salt(tag: &str) -> String {
        format!(
            "hp-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )
    }

    // The detector in-process: the first acquire is primary, the second sees the held
    // flock and becomes secondary; after the primary drops, the name is free again —
    // proving the release path that lets a new launch become primary.
    #[test]
    fn second_acquire_is_secondary_and_lock_frees_on_drop() {
        let salt = test_salt("detect");
        let first = acquire(&salt).unwrap();
        assert!(matches!(first, Instance::Primary(_)), "first acquire must be primary");
        let second = acquire(&salt).unwrap();
        assert!(
            matches!(second, Instance::Secondary(_)),
            "second acquire must see the held lock"
        );
        drop(second);
        drop(first);
        let third = acquire(&salt).unwrap();
        assert!(
            matches!(third, Instance::Primary(_)),
            "after the primary drops, the lock is free again"
        );
    }

    // A stale lock file (dead pid inside, no flock held) must not block a new launch.
    #[test]
    fn stale_lock_file_from_a_dead_primary_is_taken_over() {
        let salt = test_salt("stale");
        let names = unix_names(&salt);
        fs::create_dir_all(names.lock.parent().unwrap()).unwrap();
        fs::write(&names.lock, b"999999999").unwrap();
        let got = acquire(&salt).unwrap();
        assert!(
            matches!(got, Instance::Primary(_)),
            "an unlocked (stale) lock file must be taken over"
        );
    }

    // A real socket round trip in one process: primary serves, secondary forwards a
    // HandoffMessage, the handler receives the exact message back.
    #[tokio::test]
    async fn socket_round_trips_a_handoff_message() {
        let salt = test_salt("roundtrip");
        let Instance::Primary(primary) = acquire(&salt).unwrap() else {
            panic!("expected primary");
        };
        let Instance::Secondary(secondary) = acquire(&salt).unwrap() else {
            panic!("expected secondary");
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(primary.run_server(move |msg| {
            let _ = tx.send(msg);
        }));

        let sent = HandoffMessage {
            argv: vec![
                "hyperpanes".to_string(),
                "--new-window".to_string(),
                "/home/me/proj".to_string(),
            ],
            cwd: "/home/me".to_string(),
        };
        secondary.forward(&sent).await.unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("handler should fire within 5s")
            .expect("server should stay alive");
        assert_eq!(got, sent);
    }
}
