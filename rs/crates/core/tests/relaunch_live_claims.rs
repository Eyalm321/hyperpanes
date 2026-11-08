//! Boot recovery across a GUI relaunch, exercised with REAL processes and the real control
//! API: a work-queue claim held by a worker that SURVIVES the relaunch must stay claimed
//! with its original fencing token, while a claim whose holder died must still be requeued
//! at boot.
//!
//! The bug: with the session daemon on, worker panes (`hyperpanes worker` runners and their
//! children) live in the daemon and survive `restartApp {scope:"gui"}` or a GUI crash. The
//! relaunched GUI attaches `work.db` and its boot recovery used to requeue EVERY claimed row
//! ("every worker died with us", a premise that predates the daemon). The surviving worker's
//! ack then hit a stale fence (409) and the task was claimed and run a second time.
//!
//! Processes, each re-executing THIS test binary into [`child_entry`] (role via
//! [`ROLE_ENV`]):
//! - `primary`: the GUI/headless control host. Builds a real `Shared`, runs the production
//!   `attach_durable_work_queue()` (boot recovery included), serves the control API with
//!   `run_server` (fresh port + token in `control.json`, as on every loopback start) and the
//!   lease reaper, exactly like `ControlHost::start`.
//! - `worker`: stands in for a daemon-hosted `hyperpanes worker` runner. It claims over HTTP
//!   naming its own pid (`holderPid`), then acks on command, re-reading `control.json`
//!   first (the runner's rediscovery after a relaunch).
//!
//! A relaunch = the primary dies (clean exit or SIGKILL) while the worker process lives on,
//! then a new primary boots against the same scratch data dir. Every child runs with `HOME`,
//! `XDG_*`, `APPDATA` and `HYPERPANES_CONTROL_FILE` inside a per-test scratch root under
//! `CARGO_TARGET_TMPDIR`, so no test can reach the developer's live app or `work.db`.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperpanes_core::control::server::{self, Shared};
use hyperpanes_core::control::work::{EnqueueOpts, TaskState, WorkQueue};
use hyperpanes_core::session_manager::SessionManager;

/// Selects the child-process role; unset ⇒ [`child_entry`] is a no-op.
const ROLE_ENV: &str = "HP_RELAUNCH_TEST_ROLE";
/// Extra child args: `HP_RELAUNCH_LEASE_MS` = the worker's claim lease.
const LEASE_ENV: &str = "HP_RELAUNCH_LEASE_MS";
const QUEUE: &str = "relaunch-live-claims";
/// Generous upper bound on any single child step (the attach lock waits up to ~5s).
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// A lease the reaper can never expire within a test: only boot recovery can move it.
const LONG_LEASE_MS: i64 = 600_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- scratch env -----------------------------------------------------------------------

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "relaunch-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        for sub in ["home", "data", "state", "config", "run", "appdata"] {
            std::fs::create_dir_all(root.join(sub)).expect("create scratch dir");
        }
        Scratch { root }
    }

    fn control_file(&self) -> PathBuf {
        self.root
            .join("state")
            .join("hyperpanes")
            .join("control.json")
    }

    fn child(&self, role: &str) -> Command {
        let exe = std::env::current_exe().expect("current test exe");
        let mut cmd = Command::new(exe);
        cmd.args([
            "child_entry",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_ENV, role)
        .env("HOME", self.root.join("home"))
        .env("XDG_DATA_HOME", self.root.join("data"))
        .env("XDG_STATE_HOME", self.root.join("state"))
        .env("XDG_CONFIG_HOME", self.root.join("config"))
        .env("XDG_RUNTIME_DIR", self.root.join("run"))
        .env("APPDATA", self.root.join("appdata"))
        .env("HYPERPANES_CONTROL_FILE", self.control_file())
        .env_remove("HYPERPANES_ALLOW_SHARED_DATA")
        .env_remove("HYPERPANES_CONTROL_TOKEN")
        .env_remove("HYPERPANES_CONTROL_PORT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
        cmd
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A running child with a line-oriented stdout channel and a command stdin.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Proc {
    fn spawn(mut cmd: Command) -> Proc {
        let mut child = cmd.spawn().expect("spawn child");
        let stdout = child.stdout.take().expect("child stdout");
        let stdin = child.stdin.take();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Proc {
            child,
            stdin,
            lines: rx,
        }
    }

    /// Wait for the next protocol line `@@<prefix>...`; returns the remainder.
    fn expect(&self, prefix: &str) -> String {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let tag = format!("@@{prefix}");
                    if let Some(pos) = line.find(&tag) {
                        return line[pos + tag.len()..].trim().to_string();
                    }
                }
                Err(_) => panic!("child never printed a {prefix:?} line"),
            }
        }
    }

    fn send(&mut self, cmd: &str) {
        let stdin = self.stdin.as_mut().expect("child stdin");
        writeln!(stdin, "{cmd}").expect("write child stdin");
        stdin.flush().expect("flush child stdin");
    }

    /// `restartApp {scope:"gui"}`: the outgoing GUI exits cleanly.
    fn exit_cleanly(&mut self) {
        self.send("exit");
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                assert!(status.success(), "child exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// SIGKILL-equivalent: a crash, or a process dying with the session daemon.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Read a task straight from the DB file (a bare `WorkQueue::open` never recovers).
fn inspect(db: &Path, id: &str) -> hyperpanes_core::control::work::Task {
    WorkQueue::open(db)
        .expect("open work.db for inspection")
        .get(id)
        .expect("task exists")
}

/// A booted primary: the process, its `work.db` path, what boot recovery did, its port.
struct Booted {
    proc: Proc,
    db: PathBuf,
    recovered: usize,
    kept: usize,
    port: u16,
}

/// Boot a primary and wait until it serves.
fn boot_primary(s: &Scratch, enqueue: bool) -> Booted {
    let mut cmd = s.child("primary");
    if enqueue {
        cmd.env("HP_RELAUNCH_ENQUEUE", "1");
    }
    let p = Proc::spawn(cmd);
    let db = PathBuf::from(p.expect("DB "));
    assert!(
        db.starts_with(&s.root),
        "work.db {} escaped the scratch root",
        db.display()
    );
    let attached = p.expect("ATTACHED ");
    let mut it = attached.split_whitespace();
    assert_eq!(it.next(), Some("durable"), "primary must own work.db");
    let recovered: usize = it.next().unwrap().parse().unwrap();
    let kept: usize = it.next().unwrap().parse().unwrap();
    let port: u16 = p.expect("SERVING ").parse().unwrap();
    Booted {
        proc: p,
        db,
        recovered,
        kept,
        port,
    }
}

/// Start a worker that claims one task through the live primary. Returns
/// `(proc, task_id, fence)`.
fn start_worker(s: &Scratch, lease_ms: i64) -> (Proc, String, u64) {
    let w = Proc::spawn({
        let mut c = s.child("worker");
        c.env(LEASE_ENV, lease_ms.to_string());
        c
    });
    let claimed = w.expect("CLAIMED ");
    let mut it = claimed.split_whitespace();
    let id = it.next().expect("task id").to_string();
    let fence: u64 = it.next().expect("fence").parse().expect("fence u64");
    (w, id, fence)
}

// ---- tests -----------------------------------------------------------------------------

/// THE BUG, restartApp {scope:"gui"} shape: the outgoing GUI exits cleanly, the worker
/// (living in the session daemon) does not, and the relaunched GUI attaches `work.db`.
#[test]
fn gui_relaunch_keeps_a_surviving_workers_claim_and_fence() {
    relaunch_with_surviving_worker("gui-relaunch", false);
}

/// Same, GUI crash shape: the outgoing GUI is SIGKILLed while the daemon (and so the
/// worker) lives on.
#[test]
fn gui_crash_keeps_a_surviving_workers_claim_and_fence() {
    relaunch_with_surviving_worker("gui-crash", true);
}

fn relaunch_with_surviving_worker(name: &str, crash: bool) {
    let s = Scratch::new(name);
    let mut gui1 = boot_primary(&s, true);
    let db = gui1.db.clone();
    let (mut worker, id, fence) = start_worker(&s, LONG_LEASE_MS);
    assert_eq!(inspect(&db, &id).state, TaskState::Claimed);

    if crash {
        gui1.proc.kill();
    } else {
        gui1.proc.exit_cleanly();
    }

    // The relaunched GUI boots while the worker process is still alive.
    let Booted {
        proc: mut gui2,
        recovered,
        kept,
        port: port2,
        ..
    } = boot_primary(&s, false);
    assert_ne!(
        gui1.port, port2,
        "a loopback relaunch serves on a fresh port"
    );
    let t = inspect(&db, &id);
    assert_eq!(
        t.state,
        TaskState::Claimed,
        "boot recovery requeued a claim whose worker is still alive (the bug): \
         recovered={recovered} kept={kept} error={:?}",
        t.error
    );
    assert_eq!(
        t.fencing_token,
        Some(fence),
        "the live claim's fence must be untouched"
    );
    assert_eq!((recovered, kept), (0, 1));

    // The surviving worker finds the new GUI and acks with its ORIGINAL fence.
    worker.send("ack");
    assert_eq!(
        worker.expect("ACK "),
        "200",
        "the surviving worker's ack must succeed"
    );
    let t = inspect(&db, &id);
    assert_eq!(t.state, TaskState::Done);
    assert_eq!(t.fencing_token, Some(fence));
    assert_eq!(t.attempts, 1, "claimed once, run once: no duplicate run");

    worker.exit_cleanly();
    gui2.exit_cleanly();
}

/// The app was down longer than the claim's lease (the GUI crashed and was reopened later)
/// while the worker kept running. The relaunched GUI's reaper ticks immediately on boot, so
/// without the boot-time lease re-arm it would requeue the live claim at once; with it the
/// worker gets a full lease to reconnect and acks with its original fence.
#[test]
fn relaunch_after_the_lease_lapsed_rearms_a_live_claim() {
    let s = Scratch::new("lapsed");
    let mut gui1 = boot_primary(&s, true);
    let db = gui1.db.clone();
    let (mut worker, id, fence) = start_worker(&s, 1_500);
    gui1.proc.kill();
    std::thread::sleep(Duration::from_millis(2_500)); // the lease expires while the app is down
    let boot = now_ms();

    let mut gui2 = boot_primary(&s, false);
    assert_eq!((gui2.recovered, gui2.kept), (0, 1));
    std::thread::sleep(Duration::from_millis(300)); // let the reaper's first tick run
    let t = inspect(&db, &id);
    assert_eq!(
        t.state,
        TaskState::Claimed,
        "the live claim was reaped: {:?}",
        t.error
    );
    assert_eq!(t.fencing_token, Some(fence));
    assert!(
        t.visibility_deadline.unwrap() >= boot + 30_000,
        "re-armed to boot + visibility_timeout_ms (30s default), got {:?} (boot {boot})",
        t.visibility_deadline
    );

    worker.send("ack");
    assert_eq!(worker.expect("ACK "), "200");
    assert_eq!(inspect(&db, &id).state, TaskState::Done);
    worker.exit_cleanly();
    gui2.proc.exit_cleanly();
}

/// restartApp {scope:"full"} / a crash that takes the session daemon down / a reboot: the
/// worker dies too, so the next boot must requeue its claim IMMEDIATELY (zero added
/// latency): the lease is 10 minutes and the claim is re-claimable the moment the new
/// primary serves.
#[test]
fn full_restart_requeues_a_dead_workers_claim_at_boot() {
    let s = Scratch::new("full-restart");
    let mut gui1 = boot_primary(&s, true);
    let db = gui1.db.clone();
    let (mut worker, id, fence) = start_worker(&s, LONG_LEASE_MS);
    worker.kill(); // the daemon exits and takes its panes down
    gui1.proc.kill();

    let mut gui2 = boot_primary(&s, false);
    assert_eq!((gui2.recovered, gui2.kept), (1, 0));
    let t = inspect(&db, &id);
    assert_eq!(
        t.state,
        TaskState::Queued,
        "a dead holder's claim must requeue at boot"
    );
    assert_eq!(
        t.fencing_token,
        Some(fence),
        "requeue keeps the stale token (fences it)"
    );

    // Re-claimable right away by a fresh worker, with a strictly newer fence.
    let (mut worker2, id2, fence2) = start_worker(&s, LONG_LEASE_MS);
    assert_eq!(id2, id);
    assert!(fence2 > fence);
    worker2.exit_cleanly();
    gui2.proc.exit_cleanly();
}

/// A claim that never named a holder (an in-process claim, an MCP `claim_task`, a client
/// predating `holderPid`) keeps the legacy behaviour: requeued at boot even though the
/// process that made it is still around.
#[test]
fn holderless_claim_is_requeued_at_boot() {
    let s = Scratch::new("holderless");
    let mut gui1 = boot_primary(&s, true);
    let db = gui1.db.clone();
    let w = Proc::spawn({
        let mut c = s.child("worker");
        c.env(LEASE_ENV, LONG_LEASE_MS.to_string())
            .env("HP_RELAUNCH_NO_HOLDER", "1");
        c
    });
    let claimed = w.expect("CLAIMED ");
    let id = claimed.split_whitespace().next().unwrap().to_string();
    gui1.proc.exit_cleanly();
    let mut gui2 = boot_primary(&s, false);
    assert_eq!((gui2.recovered, gui2.kept), (1, 0));
    assert_eq!(inspect(&db, &id).state, TaskState::Queued);
    drop(w);
    gui2.proc.exit_cleanly();
}

// ---- child-process entry ---------------------------------------------------------------

fn new_shared() -> Arc<Shared> {
    let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
    let sessions = Arc::new(SessionManager::new(etx));
    let control_file = std::env::var_os("HYPERPANES_CONTROL_FILE")
        .map(PathBuf::from)
        .expect("child runs with HYPERPANES_CONTROL_FILE");
    let speech = control_file.with_file_name("speech.json");
    Shared::new(sessions, false, "0.0.0-test", control_file, speech)
}

fn say(line: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "@@{line}");
    let _ = out.flush();
}

/// `(port, token)` from the current `control.json`, if it is there and complete.
fn try_discover() -> Option<(u16, String)> {
    let path = std::env::var_os("HYPERPANES_CONTROL_FILE")?;
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some((v["port"].as_u64()? as u16, v["token"].as_str()?.to_string()))
}

fn discover() -> (u16, String) {
    try_discover().expect("control.json names a port and token")
}

/// Minimal blocking HTTP/1.1 POST (core has no blocking HTTP client). Returns
/// `(status, body)`.
fn post(port: u16, token: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect control API");
    s.set_read_timeout(Some(STEP_TIMEOUT)).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).expect("read response");
    let status: u16 = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("status code");
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
#[ignore = "child-process entry point, re-exec'd by the tests in this file"]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    match role.as_str() {
        "primary" => {
            say(format!(
                "DB {}",
                hyperpanes_core::persistence::paths::work_db().display()
            ));
            let shared = new_shared();
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            shared.set_runtime(rt.handle().clone());
            // Production order (ControlHost::start / app::run): attach (boot recovery) first,
            // then serve + reap.
            // `kept` = claims still claimed after boot recovery (counted rather than read off
            // `WorkAttach`, so this file also compiles, and goes red, on the pre-fix code).
            let recovered = match shared.attach_durable_work_queue() {
                server::WorkAttach::Durable { recovered, .. } => recovered,
                other => panic!("primary failed to attach work.db: {other:?}"),
            };
            let kept = shared.work.lock().unwrap().counts(QUEUE).claimed;
            say(format!(
                "ATTACHED {} {recovered} {kept}",
                shared.work_queue_mode().as_str()
            ));
            if std::env::var_os("HP_RELAUNCH_ENQUEUE").is_some() {
                shared.work.lock().unwrap().enqueue(
                    QUEUE,
                    "payload",
                    EnqueueOpts::default(),
                    now_ms(),
                );
            }
            rt.spawn(server::run_server(Arc::clone(&shared)));
            rt.spawn(server::run_reaper_ticker(Arc::clone(&shared)));
            // Serving once control.json names our port.
            let deadline = Instant::now() + STEP_TIMEOUT;
            loop {
                let port = shared.port();
                if port != 0 && try_discover().map(|d| d.0) == Some(port) {
                    say(format!("SERVING {port}"));
                    break;
                }
                assert!(Instant::now() < deadline, "server never came up");
                std::thread::sleep(Duration::from_millis(20));
            }
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if line.trim() == "exit" {
                    break;
                }
            }
            server::remove_discovery(&shared);
        }
        "worker" => {
            let lease: i64 = std::env::var(LEASE_ENV).unwrap().parse().unwrap();
            let body = if std::env::var_os("HP_RELAUNCH_NO_HOLDER").is_some() {
                format!(r#"{{"worker":"w","leaseMs":{lease}}}"#)
            } else {
                format!(
                    r#"{{"worker":"w","leaseMs":{lease},"holderPid":{}}}"#,
                    std::process::id()
                )
            };
            let (port, token) = discover();
            let (status, out) = post(port, &token, &format!("/queues/{QUEUE}/claim"), &body);
            assert_eq!(status, 200, "claim failed: {out}");
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            let task = &v["tasks"][0];
            let id = task["id"].as_str().expect("claimed a task").to_string();
            let fence = task["fencingToken"].as_u64().unwrap();
            say(format!("CLAIMED {id} {fence}"));
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                match line.trim() {
                    "ack" => {
                        // Rediscover: the relaunched app re-minted its port and token.
                        let (port, token) = discover();
                        let (status, _) = post(
                            port,
                            &token,
                            &format!("/tasks/{id}/ack"),
                            &format!(r#"{{"fencingToken":{fence},"result":"ok"}}"#),
                        );
                        say(format!("ACK {status}"));
                    }
                    "exit" => break,
                    _ => {}
                }
            }
        }
        other => panic!("unknown {ROLE_ENV} {other:?}"),
    }
}

#[test]
fn scratch_never_points_at_live_state() {
    // Guard rail for this file: the scratch control file and DB live under the cargo target
    // tmpdir, never under the developer's real state/data dirs.
    let s = Scratch::new("guard");
    assert!(s.control_file().starts_with(env!("CARGO_TARGET_TMPDIR")));
}
