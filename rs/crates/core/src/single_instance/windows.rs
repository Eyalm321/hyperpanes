//! Windows implementation of the single-instance seam (see `mod.rs`): the named-mutex
//! detector + the named-pipe argv hand-off, moved verbatim from the old single-file
//! `single_instance.rs`. Includes the in-process Win32 tests (two mutex handles to one
//! name; a real named-pipe round trip).

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
