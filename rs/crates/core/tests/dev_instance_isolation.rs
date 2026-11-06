//! End-to-end: the real `headless` binary refuses a *partly* isolated boot, the incident
//! configuration where `HYPERPANES_CONTROL_FILE` + `XDG_STATE_HOME` are overridden but
//! `XDG_DATA_HOME` is not. It starts when `HYPERPANES_ALLOW_SHARED_DATA=1` or when the data
//! dir is isolated too.
//!
//! Safety: every boot runs with `HOME` pointed at a per-test scratch dir, so the "default"
//! data dir the refusal protects is `<scratch>/home/.local/share/hyperpanes`, never the
//! developer's real one. `XDG_RUNTIME_DIR` is scratch as well, so the single-instance socket
//! cannot hand off to a real headless daemon. Even a broken guard can only touch scratch.
//!
//! Linux-only: the XDG split exists only there (Windows/macOS keep every dir in one folder;
//! the pure rule is unit-tested in `persistence::isolation` on every platform).
#![cfg(all(unix, not(target_os = "macos")))]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BOOT_TIMEOUT: Duration = Duration::from_secs(20);

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("devguard-{name}-{}-{nanos}", std::process::id()));
        for sub in [
            "home", "xstate", "xconfig", "xdata", "run", "xstate2", "xconfig2", "run2",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        Scratch { root }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn control_file(&self) -> PathBuf {
        self.p("xstate/hyperpanes/control.json")
    }

    /// The default data dir as seen by the child (`$HOME/.local/share/hyperpanes`).
    fn default_data_dir(&self) -> PathBuf {
        self.p("home/.local/share/hyperpanes")
    }

    /// The incident configuration: control file + state dir (+ config) overridden, data
    /// dir NOT, so it falls through to `$HOME/.local/share`.
    fn partly_isolated(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_headless"));
        cmd.env("HOME", self.p("home"))
            .env("XDG_STATE_HOME", self.p("xstate"))
            .env("XDG_CONFIG_HOME", self.p("xconfig"))
            .env("XDG_RUNTIME_DIR", self.p("run"))
            .env("HYPERPANES_CONTROL_FILE", self.control_file())
            .env_remove("XDG_DATA_HOME")
            .env_remove("HYPERPANES_ALLOW_SHARED_DATA")
            .env_remove("HYPERPANES_PANE_ID")
            .current_dir(&self.root)
            .stdin(Stdio::null());
        cmd
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Kill-on-drop guard so a test that fails mid-way never leaks a running headless.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait for `path` to exist while the child keeps running.
fn wait_for_file(child: &mut Running, path: &Path) {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    while !path.exists() {
        if let Ok(Some(status)) = child.0.try_wait() {
            panic!("headless exited early ({status}) before writing {path:?}");
        }
        assert!(Instant::now() < deadline, "{path:?} never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn headless_refuses_a_partly_isolated_boot() {
    let s = Scratch::new("refuse");
    let mut cmd = s.partly_isolated();
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn headless");
    // `wait_with_output` drains stderr; the guard must make it exit on its own, promptly.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = rx
        .recv_timeout(BOOT_TIMEOUT)
        .expect("a partly isolated headless must refuse and exit, not keep running")
        .expect("wait headless");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "must exit non-zero; stderr: {stderr}"
    );
    assert!(stderr.contains("refusing to start"), "{stderr}");
    assert!(
        stderr.contains("XDG_DATA_HOME"),
        "names the env var to set: {stderr}"
    );
    assert!(
        stderr.contains("HYPERPANES_ALLOW_SHARED_DATA"),
        "names the override: {stderr}"
    );
    // Refused BEFORE touching anything: no work.db in the shared data dir, no discovery file.
    assert!(
        !s.default_data_dir().join("work.db").exists(),
        "the refused instance must not have opened the shared work.db"
    );
    assert!(
        !s.control_file().exists(),
        "must not have written control.json"
    );
}

#[test]
fn headless_starts_partly_isolated_with_allow_shared_data() {
    let s = Scratch::new("allow");
    let mut cmd = s.partly_isolated();
    cmd.env("HYPERPANES_ALLOW_SHARED_DATA", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = Running(cmd.spawn().expect("spawn headless"));
    wait_for_file(&mut child, &s.control_file());
    // It really is sharing the (scratch) default data dir, as its owner.
    wait_for_file(&mut child, &s.default_data_dir().join("work.db.lock"));
    assert!(s.default_data_dir().join("work.db").exists());
}

#[test]
fn headless_starts_fully_isolated() {
    let s = Scratch::new("isolated");
    let mut cmd = s.partly_isolated();
    cmd.env("XDG_DATA_HOME", s.p("xdata"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = Running(cmd.spawn().expect("spawn headless"));
    wait_for_file(&mut child, &s.control_file());
    wait_for_file(&mut child, &s.p("xdata/hyperpanes/work.db"));
    assert!(
        !s.default_data_dir().join("work.db").exists(),
        "an isolated instance must not touch the default data dir"
    );
}

/// `GET /health` (unauthenticated) from the instance whose discovery file is `control_file`.
fn health(control_file: &Path) -> String {
    let disc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(control_file).unwrap()).unwrap();
    let port = disc["port"].as_u64().expect("port") as u16;
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    conn.set_read_timeout(Some(BOOT_TIMEOUT)).unwrap();
    write!(
        conn,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = String::new();
    conn.read_to_string(&mut raw).unwrap();
    raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}

/// The incident at the binary level: two control servers on ONE data dir. The first owns
/// work.db (`workQueue: "durable"`); the second, booted while the first is live, is refused
/// the DB after the restart-overlap wait and says so on `/health` (`"refused"`). Both are
/// otherwise fully isolated from each other, so neither the startup check nor the discovery
/// guard is what stops it: only the single-owner lock.
#[test]
fn second_headless_on_a_shared_data_dir_reports_refused() {
    let s = Scratch::new("two");
    let mut a = s.partly_isolated();
    a.env("XDG_DATA_HOME", s.p("xdata"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut a = Running(a.spawn().expect("spawn headless A"));
    wait_for_file(&mut a, &s.control_file());
    assert!(
        health(&s.control_file()).contains(r#""workQueue":"durable""#),
        "the first instance owns work.db"
    );

    let b_control = s.p("xstate2/hyperpanes/control.json");
    let mut b = s.partly_isolated();
    b.env("XDG_DATA_HOME", s.p("xdata"))
        .env("XDG_STATE_HOME", s.p("xstate2"))
        .env("XDG_CONFIG_HOME", s.p("xconfig2"))
        .env("XDG_RUNTIME_DIR", s.p("run2"))
        .env("HYPERPANES_CONTROL_FILE", &b_control)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut b = Running(b.spawn().expect("spawn headless B"));
    wait_for_file(&mut b, &b_control);
    let b_health = health(&b_control);
    assert!(
        b_health.contains(r#""workQueue":"refused""#),
        "the second instance must be refused the shared work.db: {b_health}"
    );
    assert!(
        health(&s.control_file()).contains(r#""workQueue":"durable""#),
        "the owner is unaffected"
    );
}
