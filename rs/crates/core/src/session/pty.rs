//! Port of the PTY layer in `src/main/session.ts` — a [`Pty`] trait + a `portable-pty`
//! implementation (conpty on Windows): spawn(file, args, cwd, env, cols, rows) → a
//! handle exposing onData / onExit (via an event sink), write, resize, kill.
//!
//! The backend is kept behind the trait so the conpty implementation is swappable
//! (e.g. the `portable-pty-psmux` fork adds modern conpty input-mode flags if the
//! default path mis-delivers `submit` / `send_keys`).
//!
//! ## Event delivery
//! `portable-pty` reads are blocking, so [`spawn_pty`] starts one OS thread that
//! drains the master reader and pushes [`PtyEvent`]s into a caller-supplied sink. The
//! sink is a plain `Fn` (not a tokio channel) so this module stays runtime-agnostic;
//! `session_manager` wraps an `UnboundedSender` in a closure. When the reader hits EOF
//! the thread waits on the child and emits a final [`PtyEvent::Exit`] with its code.
//!
//! ## Smoke test
//! A real-shell round-trip lives in `tests` behind `#[ignore]` (it spawns a process).
//! Run it explicitly with:
//! `cargo test --manifest-path rs/Cargo.toml -p hyperpanes-core session::pty -- --ignored --nocapture`
//!
//! ### Environment note (verified 2026-06-07; root-caused 2026-06-09)
//! ConPTY (spawned with `PSUEDOCONSOLE_INHERIT_CURSOR`, as portable-pty does) sends an
//! initial cursor-position query (`ESC [ 6 n`) to the master and emits NOTHING until
//! the terminal replies with a cursor position report (`ESC [ row ; col R`). A real
//! terminal answers automatically, which is why panes work; a raw test harness that
//! only reads will wait forever — in any session, interactive or not (this was
//! misdiagnosed as a headless-session property). A harness that answers the query
//! streams fine even in a sandboxed session — see `rs/spikes/conpty-probe`, which does
//! exactly that (and note: drop the master BEFORE joining a reader thread, or the
//! reader never sees EOF). The `#[ignore]`d smoke tests could be made headless-capable
//! the same way. The session engine's own logic is covered deterministically by the
//! `session_manager` pipeline + mock-pty tests, which need no pty at all.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};

use super::spawn::EnvMap;

/// An event produced by a live pty, mirroring node-pty's `onData` / `onExit`.
#[derive(Debug, Clone)]
pub enum PtyEvent {
    /// A raw output chunk from the slave. Bytes (not yet decoded) — the cwd sniffer
    /// and batcher operate on the decoded string downstream.
    Data(Vec<u8>),
    /// The child exited with this code. Always the last event; emitted exactly once.
    Exit(i32),
}

/// Everything needed to launch a pty: the resolved executable and argv (from
/// `session::spawn::resolve_spawn`), the working directory, the full child
/// environment (from `session::spawn::build_env`), and the initial grid size.
#[derive(Debug, Clone)]
pub struct PtySpec {
    pub file: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: EnvMap,
    pub cols: u16,
    pub rows: u16,
}

/// A live pty handle. `Send + Sync` so a `Session` can be shared across the async
/// runtime; all interior handles are mutex-guarded.
pub trait Pty: Send + Sync {
    /// Write bytes to the slave's stdin.
    fn write(&self, data: &[u8]) -> io::Result<()>;
    /// Resize the pty grid (clamped to ≥1×1 by the caller / `Session::resize`).
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
    /// Terminate the child. The reader thread then observes EOF and emits `Exit`.
    fn kill(&self) -> io::Result<()>;
}

/// `portable-pty` (conpty) implementation of [`Pty`]. The writer is `Arc`-shared with
/// the reader thread, which answers ConPTY's startup cursor query (see [`spawn_pty`]).
struct PortablePty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

impl Pty for PortablePty {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn kill(&self) -> io::Result<()> {
        self.killer.lock().unwrap().kill()
    }
}

/// Length of the longest suffix of `data` that is a strict prefix of one of the conpty
/// startup queries (`ESC[6n` / `ESC[c`) — bytes the handshake scanner must carry to the
/// next read so a query split across chunk boundaries is still matched.
fn query_prefix_suffix_len(data: &[u8]) -> usize {
    for keep in (1..=3usize.min(data.len())).rev() {
        let tail = &data[data.len() - keep..];
        if b"\x1b[6n".starts_with(tail) || b"\x1b[c".starts_with(tail) {
            return keep;
        }
    }
    0
}

/// Spawn a pty running `spec`, delivering output and exit via `on_event`. The returned
/// handle drives write/resize/kill; output flows on a background thread until the
/// child exits (then a single [`PtyEvent::Exit`] is sent and the thread ends).
pub fn spawn_pty(
    spec: &PtySpec,
    on_event: impl Fn(PtyEvent) + Send + 'static,
) -> io::Result<Box<dyn Pty>> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: spec.rows.max(1),
            cols: spec.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| io::Error::other(e.to_string()))?;

    let mut cmd = CommandBuilder::new(&spec.file);
    cmd.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        cmd.cwd(cwd);
    }
    // Replace the inherited environment with our fully-resolved map (build_env already
    // started from process.env, so nothing essential is dropped).
    cmd.env_clear();
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| io::Error::other(e.to_string()))?;
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .map_err(|e| io::Error::other(e.to_string()))?,
    ));

    // The reader thread starts BEFORE the child is spawned, and answers ConPTY's startup
    // cursor query inline. With `PSUEDOCONSOLE_INHERIT_CURSOR` (portable-pty's hardcoded
    // flags), the pseudoconsole host asks the terminal for the cursor position (`ESC[6n`)
    // and stalls the whole console session — including, for some shells, the parent's
    // `CreateProcessW` (pwsh 7: 1.0–1.1s, its handshake timeout) and the child's first
    // instruction (cmd: ran only at ~1.1s after launch) — until a reply arrives. The GUI
    // widget does answer DSR queries, but only once the render pump is alive, which is
    // hundreds of ms after spawn (and at startup the window doesn't even exist yet). So
    // the FIRST `ESC[6n` is answered right here with `ESC[1;1R` and STRIPPED from the
    // stream (the widget must not see it, or its late duplicate reply would reach the
    // shell as stray input). Windows-only: on POSIX there is no host handshake, and an
    // early `ESC[6n` is a real child query the widget must answer with the true cursor.
    // Later queries pass through untouched. The child handle arrives over a channel once
    // spawned; the thread reaps it for the exit code at EOF.
    let (child_tx, child_rx) = std::sync::mpsc::channel::<Box<dyn Child + Send + Sync>>();
    let handshake_writer = Arc::clone(&writer);
    thread::Builder::new()
        .name("hp-pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 65536];
            // Startup queries the host gates on: DSR (`ESC[6n`, cursor position) and DA1
            // (`ESC[c`, device attributes). The 1.24 redistributable host asks BOTH and
            // stalls the console session ~1.1s per unanswered query (its timeout); in-box
            // conhost asks only DSR. Answers are written here, the query bytes are
            // STRIPPED (so the GUI widget can't send a late duplicate reply that would
            // reach the shell as stray input), and everything else forwards immediately —
            // only a ≤3-byte possible-query-prefix is carried across chunk boundaries.
            // Scanning stops once both are answered or after the first 512 bytes (the
            // handshake is always at the very front of the stream).
            let mut want_dsr = cfg!(windows);
            let mut want_da = cfg!(windows);
            let mut scanned: usize = 0;
            let mut carry: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF — pseudoconsole closed
                    Ok(n) => {
                        if want_dsr || want_da {
                            let mut data = std::mem::take(&mut carry);
                            data.extend_from_slice(&buf[..n]);
                            if want_dsr {
                                if let Some(pos) =
                                    data.windows(4).position(|w| w == b"\x1b[6n")
                                {
                                    if let Ok(mut w) = handshake_writer.lock() {
                                        let _ = w.write_all(b"\x1b[1;1R");
                                        let _ = w.flush();
                                    }
                                    want_dsr = false;
                                    data.drain(pos..pos + 4);
                                }
                            }
                            if want_da {
                                if let Some(pos) =
                                    data.windows(3).position(|w| w == b"\x1b[c")
                                {
                                    if let Ok(mut w) = handshake_writer.lock() {
                                        // VT102 device attributes — the same class of
                                        // answer the GUI grid gives to later DA queries.
                                        let _ = w.write_all(b"\x1b[?6c");
                                        let _ = w.flush();
                                    }
                                    want_da = false;
                                    data.drain(pos..pos + 3);
                                }
                            }
                            scanned += data.len();
                            if scanned > 512 {
                                want_dsr = false;
                                want_da = false;
                            } else if want_dsr || want_da {
                                // Hold back a trailing run that could be the start of a
                                // query split across reads (a suffix of "\x1b[6n"/"\x1b[c").
                                let keep = query_prefix_suffix_len(&data);
                                if keep > 0 {
                                    carry = data.split_off(data.len() - keep);
                                }
                            }
                            if !data.is_empty() {
                                on_event(PtyEvent::Data(data));
                            }
                            continue;
                        }
                        on_event(PtyEvent::Data(buf[..n].to_vec()));
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            if !carry.is_empty() {
                on_event(PtyEvent::Data(std::mem::take(&mut carry)));
            }
            // `recv` fails only if spawn_command failed (sender dropped) — no child, no Exit.
            if let Ok(mut child) = child_rx.recv() {
                let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
                on_event(PtyEvent::Exit(code));
            }
        })
        .map_err(|e| io::Error::other(e.to_string()))?;

    // With the handshake answered concurrently by the reader thread, this no longer
    // stalls for shells that wait on the console connection during process creation.
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| io::Error::other(e.to_string()))?;

    // A killer cloned out before the child moves to the reader thread, so kill()
    // can signal independently of the thread blocked in `wait()`.
    let mut child = child;
    let killer = child.clone_killer();
    let _ = child_tx.send(child);

    // Dropping `pair.slave` closes the slave handle so the reader sees EOF when the
    // child exits.
    drop(pair.slave);

    Ok(Box::new(PortablePty {
        master: Mutex::new(pair.master),
        writer,
        killer: Mutex::new(killer),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    // An INTERACTIVE shell spec. We drive interactive shells (not one-shot
    // `cmd /c ...`) because Windows ConPTY scrapes a live screen: a process that
    // exits instantly may tear the pseudo-console down before its output is scraped,
    // so a short command's text can be lost. Driving an interactive shell — which is
    // exactly how the engine runs panes — keeps ConPTY alive long enough to observe.
    fn interactive_spec() -> PtySpec {
        let env: EnvMap = std::env::vars().collect();
        if cfg!(windows) {
            PtySpec { file: "cmd.exe".into(), args: vec![], cwd: None, env, cols: 80, rows: 24 }
        } else {
            PtySpec { file: "/bin/sh".into(), args: vec!["-i".into()], cwd: None, env, cols: 80, rows: 24 }
        }
    }

    // Drain events until `pred(accumulated_output)` holds or the deadline passes.
    // Returns the full decoded output and the exit code if one arrived.
    fn drain_until(
        rx: &mpsc::Receiver<PtyEvent>,
        timeout: Duration,
        mut pred: impl FnMut(&str) -> bool,
    ) -> (String, Option<i32>) {
        let mut out = String::new();
        let mut exit = None;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(PtyEvent::Data(b)) => {
                    out.push_str(&String::from_utf8_lossy(&b));
                    if pred(&out) {
                        break;
                    }
                }
                Ok(PtyEvent::Exit(code)) => {
                    exit = Some(code);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        (out, exit)
    }

    /// Real-shell smoke: drive an interactive shell to echo a marker, confirm we see
    /// it on `Data`, then `exit` the shell and confirm a terminal `Exit`. Ignored by
    /// default (spawns a process); run with `-- --ignored`.
    #[test]
    #[ignore = "spawns a real shell; run explicitly with --ignored"]
    fn smoke_echo_roundtrip() {
        let (tx, rx) = mpsc::channel::<PtyEvent>();
        let pty = spawn_pty(&interactive_spec(), move |ev| {
            let _ = tx.send(ev);
        })
        .expect("spawn pty");

        pty.write(b"echo HYPERPANES_PTY_OK\r\n").expect("write");
        // Wait for the echoed marker on the shell's *output* line (not just the typed
        // echo) — require it to appear after a newline so the command echo alone can't
        // satisfy it on every shell.
        let (out, _) = drain_until(&rx, Duration::from_secs(10), |o| {
            o.matches("HYPERPANES_PTY_OK").count() >= 2 || o.contains("HYPERPANES_PTY_OK\r\n")
        });
        assert!(out.contains("HYPERPANES_PTY_OK"), "marker missing; got: {out:?}");

        pty.write(b"exit\r\n").expect("write exit");
        let (_, exit) = drain_until(&rx, Duration::from_secs(10), |_| false);
        assert!(exit.is_some(), "expected an Exit event after the shell exited");
    }

    /// resize + write + kill against a long-lived interactive shell, then confirm a
    /// terminal `Exit` follows the kill. Ignored by default.
    #[test]
    #[ignore = "spawns a real shell; run explicitly with --ignored"]
    fn smoke_kill_emits_exit() {
        let (tx, rx) = mpsc::channel::<PtyEvent>();
        let pty = spawn_pty(&interactive_spec(), move |ev| {
            let _ = tx.send(ev);
        })
        .expect("spawn pty");

        // Let the shell come up so its process handle is valid before we kill.
        let (_, _) = drain_until(&rx, Duration::from_secs(5), |o| !o.is_empty());
        pty.resize(100, 30).expect("resize");
        pty.write(b"echo hi\r\n").expect("write");
        pty.kill().expect("kill");

        let (_, exit) = drain_until(&rx, Duration::from_secs(10), |_| false);
        assert!(exit.is_some(), "expected an Exit event after kill()");
    }
}
