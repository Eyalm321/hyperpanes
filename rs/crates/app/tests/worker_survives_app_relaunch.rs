//! End to end: the REAL `hyperpanes worker` runner keeps its claim across a relaunch of the
//! REAL control server, and acks with its original fencing token.
//!
//! This is the GUI-relaunch-with-the-session-daemon-on shape: the runner (a daemon pane in
//! production) outlives the control server. The relaunched server serves on a fresh port
//! with a freshly minted token (loopback default), and re-runs the production boot recovery
//! (`attach_durable_work_queue_at`, which in the same process re-attaches as the existing
//! `work.db` owner and recovers again, exactly like a new owner would). Both halves of the
//! fix are needed for this to pass:
//! - server: boot recovery keeps the claim because its holder (the runner's pid, sent as
//!   `holderPid`) is still alive, instead of requeueing it;
//! - runner: it re-reads its control file on connection failure / 401, finds the new port
//!   and token, keeps heartbeating, and acks.
//!
//! Everything lives under a scratch dir: the control file and `work.db` are explicit paths,
//! and the runner gets `HOME`/`XDG_*`/`APPDATA` pointed into the scratch root too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperpanes_core::control::server::{self, Shared, WorkAttach};
use hyperpanes_core::control::work::{EnqueueOpts, TaskState, WorkQueue};
use hyperpanes_core::session_manager::SessionManager;

const QUEUE: &str = "relaunch-e2e";
const STEP: Duration = Duration::from_secs(60);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + STEP;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        let root = std::env::temp_dir().join(format!(
            "hp-relaunch-e2e-{}-{}",
            std::process::id(),
            now_ms()
        ));
        for sub in ["home", "data", "state", "config", "run", "appdata"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Scratch(root)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One control-server "app instance": a fresh `Shared` that attaches the durable queue (boot
/// recovery) and serves the control API + lease reaper on its OWN runtime, so stopping it
/// drops every task, including axum's per-connection tasks: a kept-alive client connection
/// dies with the instance, as it does when the app process exits.
struct Instance {
    rt: tokio::runtime::Runtime,
    shared: Arc<Shared>,
    attach: WorkAttach,
}

impl Instance {
    fn boot(control: &Path, db: &Path) -> Instance {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
        let sessions = Arc::new(SessionManager::new(etx));
        let shared = Shared::new(
            sessions,
            false,
            "0.0.0-test",
            control.to_path_buf(),
            control.with_file_name("speech.json"),
        );
        shared.set_runtime(rt.handle().clone());
        let attach = shared.attach_durable_work_queue_at(db, Duration::from_secs(5));
        let s1 = Arc::clone(&shared);
        rt.spawn(async move {
            let _ = server::run_server(s1).await;
        });
        rt.spawn(server::run_reaper_ticker(Arc::clone(&shared)));
        wait_for("the control server to publish its port", || {
            let port = shared.port();
            port != 0
                && std::fs::read_to_string(control)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                    .and_then(|v| v["port"].as_u64())
                    == Some(port as u64)
        });
        Instance { rt, shared, attach }
    }

    /// The app going away: listener, open connections and reaper all stop at once.
    fn stop(self) {
        let port = self.shared.port();
        self.rt.shutdown_timeout(Duration::from_secs(5));
        drop(self.shared); // closes this instance's work.db connection
        wait_for("the old port to stop accepting", || {
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
        });
    }
}

/// Kill the runner if the test bails early, so a failure never leaks a process.
struct Reap(std::process::Child);

impl Drop for Reap {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(unix)]
#[test]
fn worker_keeps_its_claim_and_acks_across_a_control_server_relaunch() {
    let s = Scratch::new();
    let control = s.0.join("state").join("control.json");
    let db = s.0.join("data").join("work.db");
    let started = s.0.join("started");
    let go = s.0.join("go");

    let app1 = Instance::boot(&control, &db);
    assert!(
        matches!(app1.attach, WorkAttach::Durable { .. }),
        "{:?}",
        app1.attach
    );
    let task = app1.shared.work.lock().unwrap().enqueue(
        QUEUE,
        "payload",
        // A short lease, so the runner's heartbeat has to keep it alive across the outage.
        EnqueueOpts {
            visibility_timeout_ms: 4_000,
            ..Default::default()
        },
        now_ms(),
    );
    let port1 = app1.shared.port();

    let child_script = format!(
        "touch '{}'; while [ ! -f '{}' ]; do sleep 0.1; done",
        started.display(),
        go.display()
    );
    let mut worker = Reap(
        Command::new(env!("CARGO_BIN_EXE_hyperpanes"))
            .args(["worker", "--queue", QUEUE, "--", "sh", "-c", &child_script])
            .env("HYPERPANES_CONTROL_FILE", &control)
            .env("HOME", s.0.join("home"))
            .env("XDG_DATA_HOME", s.0.join("data"))
            .env("XDG_STATE_HOME", s.0.join("state"))
            .env("XDG_CONFIG_HOME", s.0.join("config"))
            .env("XDG_RUNTIME_DIR", s.0.join("run"))
            .env("APPDATA", s.0.join("appdata"))
            .env_remove("HYPERPANES_CONTROL_TOKEN")
            .env_remove("HYPERPANES_CONTROL_PORT")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn hyperpanes worker"),
    );
    wait_for("the worker's child to start", || started.exists());
    let claimed = app1.shared.work.lock().unwrap().get(&task.id).unwrap();
    assert_eq!(claimed.state, TaskState::Claimed);
    let fence = claimed.fencing_token.unwrap();

    // Relaunch: the app goes away for longer than the lease's heartbeat interval while the
    // runner keeps running, then comes back on a new port with a new token.
    app1.stop();
    std::thread::sleep(Duration::from_millis(3_000));
    let app2 = Instance::boot(&control, &db);
    match app2.attach {
        WorkAttach::Durable { recovered, kept } => assert_eq!(
            (recovered, kept),
            (0, 1),
            "boot recovery must keep the live runner's claim"
        ),
        ref other => panic!("relaunched app failed to attach work.db: {other:?}"),
    }
    assert_ne!(app2.shared.port(), port1);
    let t = app2.shared.work.lock().unwrap().get(&task.id).unwrap();
    assert_eq!(t.state, TaskState::Claimed);
    assert_eq!(t.fencing_token, Some(fence));

    // Outlive the re-armed lease (boot + 4s) past a reaper tick (every 5s, first at boot):
    // only the runner's re-discovered heartbeat keeps the claim from being reaped now.
    std::thread::sleep(Duration::from_millis(6_500));
    let t = app2.shared.work.lock().unwrap().get(&task.id).unwrap();
    assert_eq!(
        t.state,
        TaskState::Claimed,
        "the heartbeat did not reach the relaunched server: {:?}",
        t.error
    );

    std::fs::write(&go, b"").unwrap();
    let deadline = Instant::now() + STEP;
    let status = loop {
        if let Some(st) = worker.0.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < deadline, "worker never exited");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "worker exited with {status}");

    let done = app2.shared.work.lock().unwrap().get(&task.id).unwrap();
    assert_eq!(done.state, TaskState::Done, "error={:?}", done.error);
    assert_eq!(
        done.fencing_token,
        Some(fence),
        "acked with the ORIGINAL fence"
    );
    assert_eq!(done.attempts, 1, "claimed once, run once");
    // The DB on disk agrees (no in-memory illusion).
    let on_disk = WorkQueue::open(&db).unwrap().get(&task.id).unwrap();
    assert_eq!(on_disk.state, TaskState::Done);
    app2.stop();
}
