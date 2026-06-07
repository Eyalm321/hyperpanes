//! Single-instance gate + argv hand-off (replaces Electron `requestSingleInstanceLock`).
//!
//! Two distinct mechanisms — deliberately NOT the same primitive:
//!
//! - **Detector = a named mutex** (`CreateMutexW`; `GetLastError == ERROR_ALREADY_EXISTS`
//!   ⇒ a primary already runs). A mutex is the right race-free detector: the kernel makes
//!   "create-or-find" atomic, and it is auto-released when the owning process dies (so a
//!   crashed primary never wedges every future launch). Pipe-*connect* must NOT be the
//!   detector — at startup the primary may have the pipe down for a window and a secondary
//!   would wrongly promote itself.
//! - **Hand-off = a named pipe**. The primary runs a pipe server; a secondary connects and
//!   sends `{ argv, cwd }` as JSON, then exits. The primary feeds that to its launch
//!   routing (`crate::cli::routing`, wired up in the binary — not here).
//!
//! Both names derive from a stable per-user salt so two users on one machine don't collide,
//! and so a dev build (different userData) stays independent of an installed build — matching
//! the Electron behavior where the lock is keyed off the userData path.
//!
//! ## Testing
//! The pure pieces — name derivation, the per-user salt, and the `{argv,cwd}` JSON wire
//! shape — are covered cross-platform. The Win32 detector and the pipe round-trip are
//! covered in-process under `#[cfg(windows)]` (two mutex handles to one name; a server +
//! client over a real named pipe). The genuine TWO-PROCESS behavior (launch B while A runs →
//! A receives B's argv, B exits 0) is a manual/integration check — see `LIVE CHECK` below.
//!
//! ```text
//! LIVE CHECK (two real processes):
//!   1. Start instance A (becomes primary; holds the mutex; serves the pipe).
//!   2. Start instance B with some argv. B's `acquire()` sees ERROR_ALREADY_EXISTS →
//!      Secondary; B calls `forward({argv,cwd})` then exits 0.
//!   3. A's `run_server` handler fires with B's `{argv,cwd}`; A applies the routing.
//!   4. Kill A; start C → C's mutex create succeeds (no ERROR_ALREADY_EXISTS) → C is the
//!      new primary. (Confirms the crashed/closed-primary path releases the mutex.)
//! ```
//!
//! Owned by track `platform`.

use serde::{Deserialize, Serialize};

/// What a secondary instance hands the primary: the raw CLI argv and the launch cwd. The
/// primary decodes this and routes it (attach into the focused window, or open new
/// window(s)) exactly as Electron's `second-instance` event did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffMessage {
    pub argv: Vec<String>,
    pub cwd: String,
}

/// The OS object names this instance uses, both derived from one salt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceNames {
    /// Named-mutex name (the `Local\` namespace = per-logon-session, one instance per user).
    pub mutex: String,
    /// Full named-pipe path (`\\.\pipe\...`).
    pub pipe: String,
}

/// Derive the (mutex, pipe) names from an arbitrary salt. Deterministic: identical salt →
/// identical names; different salt → (with overwhelming probability) different names. The
/// salt is hashed so it is always a short, namespace-safe token regardless of its content
/// (a userData path can contain spaces, backslashes, drive colons, …).
pub fn instance_names(salt: &str) -> InstanceNames {
    let h = format!("{:016x}", fnv1a64(salt));
    InstanceNames {
        mutex: format!("Local\\hyperpanes.singleton.{h}"),
        pipe: format!(r"\\.\pipe\hyperpanes.handoff.{h}"),
    }
}

/// A stable per-user salt. Keyed off the user's roaming-appdata path (which already embeds
/// the username and is exactly what the Electron userData lock was keyed under); falls back
/// to the username, then a fixed default, so it is never empty.
pub fn user_salt() -> String {
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.is_empty() {
            return appdata.to_lowercase();
        }
    }
    if let Ok(user) = std::env::var("USERNAME") {
        if !user.is_empty() {
            return format!("user:{}", user.to_lowercase());
        }
    }
    "hyperpanes-default".to_string()
}

// FNV-1a (64-bit). Tiny, dependency-free, and stable across runs/processes — all we need to
// turn an arbitrary salt into a fixed-width hex token.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The outcome of trying to become the single instance.
pub enum Instance {
    /// We are the primary — hold this value for the app's lifetime (dropping it releases the
    /// mutex). Call [`PrimaryInstance::run_server`] to accept hand-offs from later launches.
    Primary(PrimaryInstance),
    /// A primary already runs — forward our argv to it via [`SecondaryInstance::forward`],
    /// then exit.
    Secondary(SecondaryInstance),
}

// ===========================================================================================
// Windows implementation
// ===========================================================================================

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, HANDLE,
    };
    use windows::Win32::System::Threading::CreateMutexW;

    // RAII wrapper around the mutex HANDLE. Closing the handle releases our reference to the
    // named mutex; when the last reference in the system is gone the name is freed and a
    // fresh launch becomes primary. `Send` so the guard can live inside a spawned server
    // future (a HANDLE is just a kernel index; CloseHandle is valid from any thread).
    struct MutexGuard {
        handle: HANDLE,
    }
    unsafe impl Send for MutexGuard {}
    impl Drop for MutexGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    // Create-or-open the named mutex. Returns the guard plus whether it ALREADY existed
    // (i.e. a primary is already running). `GetLastError` is read immediately after the
    // success-returning `CreateMutexW` (whose windows-rs wrapper does not touch the
    // thread-error on the Ok path), so ERROR_ALREADY_EXISTS is preserved.
    fn create_named_mutex(name: &str) -> windows::core::Result<(MutexGuard, bool)> {
        let wide = HSTRING::from(name);
        let handle = unsafe { CreateMutexW(None, false, &wide) }?;
        let already_existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((MutexGuard { handle }, already_existed))
    }

    pub fn acquire(salt: &str) -> io::Result<Instance> {
        let names = instance_names(salt);
        let (guard, already) = create_named_mutex(&names.mutex).map_err(win_err)?;
        if already {
            // Drop our redundant handle right away; the primary owns the mutex. We only
            // needed the create to DETECT it.
            drop(guard);
            Ok(Instance::Secondary(SecondaryInstance { names }))
        } else {
            Ok(Instance::Primary(PrimaryInstance {
                names,
                _mutex: guard,
            }))
        }
    }

    pub struct PrimaryInstance {
        names: InstanceNames,
        _mutex: MutexGuard,
    }

    impl PrimaryInstance {
        /// The pipe path we serve (exposed mainly for diagnostics/tests).
        pub fn pipe_name(&self) -> &str {
            &self.names.pipe
        }

        /// Accept hand-offs forever, invoking `handler` with each decoded
        /// [`HandoffMessage`]. Consumes `self` so the mutex guard lives for as long as the
        /// server runs (typically the whole app). Spawn it on the tokio runtime:
        /// `tokio::spawn(primary.run_server(|msg| route(msg)))`.
        ///
        /// Per-connection errors (a secondary that died mid-send, a malformed payload) are
        /// swallowed and the loop continues — one bad launch must never take down the
        /// primary's hand-off channel. Only failure to (re)bind the pipe is fatal.
        pub async fn run_server<F>(self, mut handler: F) -> io::Result<()>
        where
            F: FnMut(HandoffMessage),
        {
            let pipe = self.names.pipe.clone();
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&pipe)?;
            loop {
                // Wait for a secondary to connect.
                if server.connect().await.is_err() {
                    server = ServerOptions::new().create(&pipe)?;
                    continue;
                }
                let connected = std::mem::replace(
                    &mut server,
                    // Pre-create the NEXT instance so a connect arriving while we read the
                    // current one is not refused.
                    ServerOptions::new().create(&pipe)?,
                );
                let mut connected = connected;
                match read_to_end(&mut connected).await {
                    Ok(bytes) => {
                        if let Ok(msg) = serde_json::from_slice::<HandoffMessage>(&bytes) {
                            handler(msg);
                        }
                    }
                    Err(_) => { /* drop a half-sent hand-off, keep serving */ }
                }
                drop(connected);
            }
        }
    }

    pub struct SecondaryInstance {
        names: InstanceNames,
    }

    impl SecondaryInstance {
        /// The pipe path we forward to (exposed mainly for diagnostics/tests).
        pub fn pipe_name(&self) -> &str {
            &self.names.pipe
        }

        /// Connect to the primary's pipe and send our `{argv,cwd}` as JSON. Retries briefly
        /// while the pipe is momentarily busy (the primary is between accepting one
        /// connection and re-arming the next instance).
        pub async fn forward(&self, msg: &HandoffMessage) -> io::Result<()> {
            let bytes = serde_json::to_vec(msg)?;
            let mut client = open_client(&self.names.pipe).await?;
            client.write_all(&bytes).await?;
            client.flush().await?;
            client.shutdown().await?;
            Ok(())
        }
    }

    // Open the client pipe, tolerating the brief ERROR_PIPE_BUSY window between server
    // instances.
    async fn open_client(
        pipe: &str,
    ) -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
        const PIPE_BUSY: i32 = ERROR_PIPE_BUSY.0 as i32;
        for _ in 0..50 {
            match ClientOptions::new().open(pipe) {
                Ok(c) => return Ok(c),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "named pipe stayed busy",
        ))
    }

    // Read the full payload until the peer closes its write side. A named pipe surfaces the
    // peer's close as either Ok(0) or a BrokenPipe error; both mean EOF here.
    async fn read_to_end(server: &mut NamedPipeServer) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match server.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
                Err(e) => return Err(e),
            }
        }
        Ok(buf)
    }

    fn win_err(e: windows::core::Error) -> io::Error {
        io::Error::other(format!("Win32: {e}"))
    }

    #[cfg(test)]
    mod win_tests {
        use super::*;

        // The detector logic in-process: two handles to ONE name. The first sees a fresh
        // mutex; the second sees ERROR_ALREADY_EXISTS. After both drop, a third is fresh
        // again — proving the release path that lets a new launch become primary.
        #[test]
        fn named_mutex_detects_an_existing_holder() {
            let name = format!(
                "Local\\hyperpanes.test.mutex.{:?}",
                std::thread::current().id()
            );
            let (g1, a1) = create_named_mutex(&name).unwrap();
            assert!(!a1, "first create should be the fresh owner");
            let (g2, a2) = create_named_mutex(&name).unwrap();
            assert!(a2, "second create must report ERROR_ALREADY_EXISTS");
            drop(g2);
            drop(g1);
            let (g3, a3) = create_named_mutex(&name).unwrap();
            assert!(!a3, "after all handles drop, the name is free again");
            drop(g3);
        }

        // A real named-pipe round trip in one process: server accepts, client forwards a
        // HandoffMessage, server decodes the exact bytes back.
        #[tokio::test]
        async fn pipe_round_trips_a_handoff_message() {
            let pipe = format!(
                r"\\.\pipe\hyperpanes.test.handoff.{:?}",
                std::thread::current().id()
            );
            let mut server = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&pipe)
                .unwrap();

            let sent = HandoffMessage {
                argv: vec![
                    "hyperpanes".to_string(),
                    "--new-window".to_string(),
                    "C:\\proj".to_string(),
                ],
                cwd: "C:\\Users\\me".to_string(),
            };

            let client_pipe = pipe.clone();
            let sent_for_client = sent.clone();
            let client = tokio::spawn(async move {
                let mut c = open_client(&client_pipe).await.unwrap();
                let bytes = serde_json::to_vec(&sent_for_client).unwrap();
                c.write_all(&bytes).await.unwrap();
                c.flush().await.unwrap();
                c.shutdown().await.unwrap();
            });

            server.connect().await.unwrap();
            let bytes = read_to_end(&mut server).await.unwrap();
            client.await.unwrap();

            let got: HandoffMessage = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(got, sent);
        }
    }
}

#[cfg(windows)]
pub use imp::{acquire, PrimaryInstance, SecondaryInstance};

// ===========================================================================================
// Non-Windows fallback — this is a Windows-first product; the OS-bound half is unavailable
// elsewhere, but the pure pieces (names/salt/wire shape) still compile and test everywhere.
// ===========================================================================================

#[cfg(not(windows))]
mod imp_stub {
    use super::*;
    use std::io;

    pub fn acquire(_salt: &str) -> io::Result<Instance> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "single-instance is implemented for Windows only",
        ))
    }

    pub struct PrimaryInstance {
        _priv: (),
    }
    pub struct SecondaryInstance {
        _priv: (),
    }
}

#[cfg(not(windows))]
pub use imp_stub::{acquire, PrimaryInstance, SecondaryInstance};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_deterministic_for_a_salt() {
        let a = instance_names("C:\\Users\\me\\AppData\\Roaming\\hyperpanes");
        let b = instance_names("C:\\Users\\me\\AppData\\Roaming\\hyperpanes");
        assert_eq!(a, b);
    }

    #[test]
    fn different_salts_yield_different_names() {
        let a = instance_names("user-a");
        let b = instance_names("user-b");
        assert_ne!(a.mutex, b.mutex);
        assert_ne!(a.pipe, b.pipe);
    }

    #[test]
    fn names_use_the_expected_namespaces() {
        let n = instance_names("anything");
        assert!(n.mutex.starts_with("Local\\hyperpanes.singleton."));
        assert!(n.pipe.starts_with(r"\\.\pipe\hyperpanes.handoff."));
    }

    #[test]
    fn the_salt_is_hashed_not_embedded_raw() {
        // A salt full of path separators / colons must NOT leak into a pipe name verbatim;
        // it is reduced to a fixed-width hex token.
        let n = instance_names("C:\\a b\\c");
        let token = n.pipe.rsplit('.').next().unwrap();
        assert_eq!(token.len(), 16);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn user_salt_is_never_empty() {
        assert!(!user_salt().is_empty());
    }

    #[test]
    fn handoff_message_json_round_trips() {
        let msg = HandoffMessage {
            argv: vec!["hyperpanes".to_string(), "--tab".to_string(), ".".to_string()],
            cwd: "C:\\work".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: HandoffMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn handoff_message_wire_shape_is_argv_and_cwd() {
        let msg = HandoffMessage {
            argv: vec!["a".to_string()],
            cwd: "b".to_string(),
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert!(v.get("argv").is_some());
        assert!(v.get("cwd").is_some());
    }
}
