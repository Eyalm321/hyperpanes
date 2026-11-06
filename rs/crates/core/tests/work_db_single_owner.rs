//! Single-owner guard for the durable work queue (`work.db`), exercised across REAL
//! processes — the shape of the incident that motivated it.
//!
//! The incident: an "isolated" dev/headless control server was booted with
//! `HYPERPANES_CONTROL_FILE` + `XDG_STATE_HOME` overridden but NOT `XDG_DATA_HOME`, so it
//! opened the LIVE `~/.local/share/hyperpanes/work.db`. Its boot recovery
//! (`recover_in_flight`) requeued another project's claimed task out from under the live
//! worker: the worker's fencing token went stale and the task ran twice.
//!
//! Each test drives child processes that re-exec THIS test binary into [`child_entry`]
//! (selected by [`ROLE_ENV`]). A child builds a real `Shared` and calls the production
//! `attach_durable_work_queue()`, exactly like the GUI control host and the headless daemon
//! do, so the guard under test is the one that ships. Every child runs with `HOME`,
//! `XDG_*`, `APPDATA` and `HYPERPANES_CONTROL_FILE` pointed at a per-test scratch dir under
//! `CARGO_TARGET_TMPDIR`, so no test can ever reach the developer's live state.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyperpanes_core::control::server::Shared;
use hyperpanes_core::control::work::{EnqueueOpts, LeaseOutcome, TaskState, WorkQueue};
use hyperpanes_core::session_manager::SessionManager;

/// Selects the child-process role; unset ⇒ [`child_entry`] is a no-op.
const ROLE_ENV: &str = "HP_WORKDB_TEST_ROLE";
const QUEUE: &str = "devguard-incident";
/// Generous upper bound on any single child step (the guard's own retry window is ~5s).
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- scratch env -----------------------------------------------------------------------

/// A per-test scratch root standing in for the user's home. Every path a child could
/// resolve state/data/config from points inside it.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "workdb-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        for sub in ["home", "data", "state", "config", "run", "appdata"] {
            std::fs::create_dir_all(root.join(sub)).expect("create scratch dir");
        }
        Scratch { root }
    }

    /// A command re-executing this test binary into [`child_entry`] as `role`, fully
    /// isolated: both instances of a test share ONE data dir (that is the point — two
    /// openers on the same work.db) but nothing outside the scratch root.
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
        .env(
            "HYPERPANES_CONTROL_FILE",
            self.root
                .join("state")
                .join("hyperpanes")
                .join("control.json"),
        )
        .env_remove("HYPERPANES_ALLOW_SHARED_DATA")
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

    /// Wait for the next stdout line starting with `prefix`; returns the remainder.
    fn expect(&self, prefix: &str) -> String {
        let deadline = std::time::Instant::now() + STEP_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    // Protocol lines carry a `@@` sentinel and may share a line with
                    // libtest's own `test child_entry ... ` banner.
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

    /// Wait (bounded) for the child to exit on its own.
    fn wait_exit(&mut self) {
        let deadline = std::time::Instant::now() + STEP_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                assert!(status.success(), "child exited with {status}");
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child did not exit within {STEP_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// SIGKILL-equivalent: the outgoing GUI/daemon dying without any cleanup.
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

/// Start a `primary` child: it attaches the durable queue, enqueues + claims one task with
/// a long lease (so the lease reaper can never be what moves it), and keeps the DB open.
/// Returns `(proc, db_path, task_id, fencing_token)`.
fn start_primary(s: &Scratch) -> (Proc, PathBuf, String, u64) {
    let p = Proc::spawn(s.child("primary"));
    let db = PathBuf::from(p.expect("DB "));
    let claimed = p.expect("CLAIMED ");
    let mut it = claimed.split_whitespace();
    let id = it.next().expect("task id").to_string();
    let fence: u64 = it.next().expect("fence").parse().expect("fence u64");
    // Precondition: the primary really is durable — its claim is on disk, not in memory.
    let task = inspect(&db, &id);
    assert_eq!(
        task.0,
        TaskState::Claimed,
        "primary's claim must be on disk"
    );
    assert_eq!(task.1, Some(fence));
    (p, db, id, fence)
}

/// Read a task's `(state, fencing_token, error)` straight from the DB file, without any
/// recovery (a bare `WorkQueue::open` never requeues).
fn inspect(db: &Path, id: &str) -> (TaskState, Option<u64>, Option<String>) {
    let wq = WorkQueue::open(db).expect("open work.db for inspection");
    let t = wq.get(id).expect("task exists");
    (t.state, t.fencing_token, t.error)
}

// ---- tests -----------------------------------------------------------------------------

/// THE INCIDENT. A live primary holds a claimed task; a second instance boots against the
/// same work.db (the half-isolated dev server). The second opener must NOT requeue the
/// claimed task, and the live worker's fencing token must stay valid (its ack succeeds).
#[test]
fn second_opener_does_not_requeue_claimed_task() {
    let s = Scratch::new("incident");
    let (mut primary, db, id, fence) = start_primary(&s);

    // The second instance boots and runs the production attach path to completion.
    let mut second = Proc::spawn(s.child("opener"));
    second.expect("ATTACHED");
    second.wait_exit();

    let (state, token, error) = inspect(&db, &id);
    assert_eq!(
        state,
        TaskState::Claimed,
        "a second opener requeued the live primary's claimed task (incident reproduced): \
         error={error:?}"
    );
    assert_eq!(
        token,
        Some(fence),
        "the live claim's fencing token must be untouched"
    );

    // ...so the live worker finishes normally instead of being fenced out (no duplicate run).
    primary.send("ack");
    assert_eq!(primary.expect("ACK "), "ok");
    primary.send("exit");
    primary.wait_exit();
}

/// GUI relaunch / crash: the owning process dies with a task claimed, and the NEXT primary
/// (the relaunched GUI or a restarted headless daemon) must still recover it on boot.
#[test]
fn relaunched_primary_recovers_its_claimed_task() {
    let s = Scratch::new("relaunch");
    let (mut primary, db, id, fence) = start_primary(&s);
    primary.kill();

    let mut next = Proc::spawn(s.child("opener"));
    next.expect("ATTACHED");
    next.wait_exit();

    // Boot recovery, not the lease reaper: the claim's lease is 10 minutes and no child
    // runs the reaper ticker, so only `recover_in_flight` can have moved it.
    let (state, token, _) = inspect(&db, &id);
    assert_eq!(
        state,
        TaskState::Queued,
        "relaunched primary must recover the claim"
    );
    assert_eq!(
        token,
        Some(fence),
        "requeue keeps the stale token (fences the dead worker)"
    );
}

/// Restart overlap: the incoming primary boots while the outgoing one is still alive (the
/// restartApp window). It must wait for the outgoing owner instead of recovering under it,
/// then take over and recover once the outgoing owner is gone.
#[test]
fn relaunch_overlap_waits_for_outgoing_owner() {
    let s = Scratch::new("overlap");
    let (mut outgoing, db, id, _fence) = start_primary(&s);

    let mut incoming = Proc::spawn(s.child("opener"));
    // While the outgoing owner lives, the incoming instance must not have touched its claim.
    std::thread::sleep(Duration::from_millis(1000));
    let (state, _, error) = inspect(&db, &id);
    assert_eq!(
        state,
        TaskState::Claimed,
        "incoming instance recovered while the outgoing owner was still alive: {error:?}"
    );

    outgoing.send("exit");
    outgoing.wait_exit();
    incoming.expect("ATTACHED");
    incoming.wait_exit();

    let (state, _, error) = inspect(&db, &id);
    assert_eq!(
        state,
        TaskState::Queued,
        "incoming primary must take over and recover once the outgoing owner exits: {error:?}"
    );
}

/// In-process control-host restart (Settings → toggle control off/on): a SECOND `Shared` in
/// the SAME process — the first one may still be alive, held by a lingering handler — is
/// the same instance, stays the owner, and recovers exactly as before.
#[test]
fn same_process_reattach_stays_owner() {
    let s = Scratch::new("reattach");
    let mut p = Proc::spawn(s.child("reattach"));
    assert_eq!(p.expect("REATTACH "), "queued");
    p.wait_exit();
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

/// Emit one protocol line to the parent: straight to the stdout handle (bypassing the test
/// harness's print capture) and flushed, so the parent sees it while this child is still
/// blocked on stdin.
fn say(line: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "@@{line}");
    let _ = out.flush();
}

fn enqueue_and_claim(shared: &Shared) -> (String, u64) {
    let now = now_ms();
    let mut wq = shared.work.lock().unwrap();
    wq.enqueue(QUEUE, "payload", EnqueueOpts::default(), now);
    let c = wq
        .claim(QUEUE, "worker-A", 600_000, now)
        .expect("claim the task just enqueued");
    (c.task.id, c.fencing_token)
}

#[test]
#[ignore = "child-process entry point, re-exec'd by the tests in this file"]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let db = hyperpanes_core::persistence::paths::work_db();
    say(format!("DB {}", db.display()));
    match role.as_str() {
        "primary" => {
            let shared = new_shared();
            shared.attach_durable_work_queue();
            let (id, fence) = enqueue_and_claim(&shared);
            say(format!("CLAIMED {id} {fence}"));
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                match line.trim() {
                    "ack" => {
                        let out =
                            shared
                                .work
                                .lock()
                                .unwrap()
                                .ack(&id, fence, Some("done"), now_ms());
                        let word = match out {
                            LeaseOutcome::Ok(_) => "ok",
                            LeaseOutcome::Conflict => "conflict",
                            LeaseOutcome::NotFound => "notfound",
                        };
                        say(format!("ACK {word}"));
                    }
                    "exit" => break,
                    _ => {}
                }
            }
        }
        "opener" => {
            let shared = new_shared();
            shared.attach_durable_work_queue();
            say("ATTACHED".to_string());
        }
        "reattach" => {
            let first = new_shared();
            first.attach_durable_work_queue();
            let (id, _) = enqueue_and_claim(&first);
            // `first` stays alive on purpose (a lingering Arc after ControlHost::stop).
            let second = new_shared();
            second.attach_durable_work_queue();
            let state = second
                .work
                .lock()
                .unwrap()
                .get(&id)
                .map(|t| t.state.as_str().to_string())
                .unwrap_or_else(|| "missing".to_string());
            say(format!("REATTACH {state}"));
            drop(first);
        }
        other => panic!("unknown {ROLE_ENV} {other:?}"),
    }
}
