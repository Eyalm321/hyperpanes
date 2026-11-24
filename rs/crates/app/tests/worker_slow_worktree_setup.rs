//! End to end: a `--worktree` task whose worktree setup outlasts its lease stays with the runner
//! that claimed it.
//!
//! Two REAL `hyperpanes worker` runners compete on one queue of the REAL control server (lease
//! reaper on). The repo's `post-checkout` hook sleeps, so `git worktree add` (which runs it) takes
//! longer than the task's lease plus a reaper tick, the way a big checkout does on a loaded host.
//! The runner must renew the lease from the moment it claims, not only once the child starts:
//! otherwise the reaper requeues the task during setup, the second runner claims it under a new
//! fencing token, and the first runner's ack is fenced out after it ran the child anyway.
//!
//! Everything lives under a scratch dir: control file, `work.db`, repo, worktree root, and the
//! runners' `HOME`/`XDG_*`/`APPDATA`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyperpanes_core::control::server::{self, Shared, WorkAttach};
use hyperpanes_core::control::work::{EnqueueOpts, TaskState};
use hyperpanes_core::session_manager::SessionManager;

const QUEUE: &str = "slow-setup-e2e";
/// The task's lease: well under the setup time below.
const LEASE_MS: i64 = 2_000;
/// How long the post-checkout hook holds `git worktree add`: past the lease AND past a reaper tick
/// (every 5 s), so a lease nobody renews during setup is certainly swept.
const SETUP_SECS: u64 = 8;
const STEP: Duration = Duration::from_secs(90);

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

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        let root = std::env::temp_dir().join(format!(
            "hp-slow-setup-e2e-{}-{}",
            std::process::id(),
            now_ms()
        ));
        for sub in [
            "home", "data", "state", "config", "cache", "run", "appdata", "wtroot",
        ] {
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

/// The control server with the durable queue attached and the lease reaper ticking, on its own
/// runtime (dropped with the test).
struct Instance {
    rt: tokio::runtime::Runtime,
    shared: Arc<Shared>,
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
        assert!(matches!(attach, WorkAttach::Durable { .. }), "{attach:?}");
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
        Instance { rt, shared }
    }
}

/// Kill a runner if the test bails early, so a failure never leaks a process.
struct Reap(std::process::Child);

impl Drop for Reap {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

impl Reap {
    fn wait(&mut self, who: &str) -> std::process::ExitStatus {
        let deadline = Instant::now() + STEP;
        loop {
            if let Some(st) = self.0.try_wait().unwrap() {
                return st;
            }
            assert!(Instant::now() < deadline, "{who} never exited");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// One `hyperpanes worker --worktree` runner named `name`, fully scratch-isolated.
fn runner(s: &Path, repo: &Path, control: &Path, name: &str, extra: &[&str], child: &str) -> Reap {
    Reap(
        Command::new(env!("CARGO_BIN_EXE_hyperpanes"))
            .current_dir(repo)
            .args(["worker", "--queue", QUEUE, "--worker", name, "--worktree"])
            .args(["--base", "main", "--min-free", "0", "--allow-empty-result"])
            .arg("--worktree-root")
            .arg(s.join("wtroot"))
            .args(extra)
            .args(["--", "sh", "-c", child])
            .env("HYPERPANES_CONTROL_FILE", control)
            .env("HOME", s.join("home"))
            .env("XDG_DATA_HOME", s.join("data"))
            .env("XDG_STATE_HOME", s.join("state"))
            .env("XDG_CONFIG_HOME", s.join("config"))
            .env("XDG_CACHE_HOME", s.join("cache"))
            .env("XDG_RUNTIME_DIR", s.join("run"))
            .env("APPDATA", s.join("appdata"))
            // The repo's own hooks dir must apply: no host git config (core.hooksPath) leaks in.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            // Cache knobs off, so a scratch root on a small disk never trips them.
            .env("HYPERPANES_WORKTREE_SLOT_MAX", "0")
            .env("HYPERPANES_WORKTREE_CACHE_MAX", "0")
            .env("HYPERPANES_WORKTREE_SOFT_FREE", "0")
            .env_remove("HYPERPANES_WORKTREE_ROOT")
            .env_remove("HYPERPANES_WORKTREE_MAX_SLOTS")
            .env_remove("HYPERPANES_SHARED_TARGETS")
            .env_remove("HYPERPANES_CONTROL_TOKEN")
            .env_remove("HYPERPANES_CONTROL_PORT")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn hyperpanes worker"),
    )
}

#[test]
fn worktree_setup_slower_than_the_lease_keeps_the_claim_and_acks_once() {
    let s = Scratch::new();
    let control = s.0.join("state").join("control.json");
    let db = s.0.join("data").join("work.db");
    let runs = s.0.join("runs");

    // A repo whose checkout is slow: `git worktree add` runs post-checkout after checking out.
    let repo = s.0.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["commit", "--allow-empty", "-m", "A"]);
    let hook = repo.join(".git").join("hooks").join("post-checkout");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, format!("#!/bin/sh\nsleep {SETUP_SECS}\n")).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let app = Instance::boot(&control, &db);
    let task = app.shared.work.lock().unwrap().enqueue(
        QUEUE,
        "payload",
        EnqueueOpts {
            visibility_timeout_ms: LEASE_MS,
            ..Default::default()
        },
        now_ms(),
    );

    // Every run of the child appends one line: the task it ran and the fence it held.
    let child = format!(
        "echo \"$HP_TASK_ID $HP_FENCING_TOKEN\" >> '{}'",
        runs.display()
    );
    let mut first = runner(&s.0, &repo, &control, "first", &[], &child);
    wait_for("the first runner to claim the task", || {
        let t = app.shared.work.lock().unwrap().get(&task.id).unwrap();
        t.state == TaskState::Claimed && t.claimed_by.as_deref() == Some("first")
    });
    let fence = app
        .shared
        .work
        .lock()
        .unwrap()
        .get(&task.id)
        .unwrap()
        .fencing_token
        .unwrap();

    // A competitor that keeps polling for the whole setup: it gets the task only if the first
    // runner's lease lapses while `git worktree add` is still running.
    let mut second = runner(
        &s.0,
        &repo,
        &control,
        "second",
        &["--retry-window", &(SETUP_SECS + 4).to_string()],
        &child,
    );

    let first_status = first.wait("the first runner");
    let second_status = second.wait("the second runner");

    let t = app.shared.work.lock().unwrap().get(&task.id).unwrap();
    assert_eq!(
        (t.state, t.attempts, t.fencing_token),
        (TaskState::Done, 1, Some(fence)),
        "the claim must survive the slow setup: claimed once, acked with the original fence \
         (claimed_by={:?}, error={:?})",
        t.claimed_by,
        t.error
    );
    assert!(t.error.is_none(), "a clean run leaves no error: {:?}", t.error);
    let lines: Vec<String> = std::fs::read_to_string(&runs)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines,
        vec![format!("{} {fence}", task.id)],
        "the child must run exactly once, under the first claim"
    );
    assert!(
        first_status.success(),
        "first runner: {first_status} (its ack was refused)"
    );
    assert!(second_status.success(), "second runner: {second_status}");

    app.rt.shutdown_timeout(Duration::from_secs(5));
}
