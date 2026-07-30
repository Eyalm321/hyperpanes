//! Discovery-file ownership guard — a second instance must not silently take over a
//! `control.json` owned by a live instance.
//!
//! `core/src/app.rs:44-46` resolves the control file as `HYPERPANES_CONTROL_FILE` over
//! the XDG-derived default — and every spawned pane INHERITS that variable pointing at
//! the LIVE file (`session::spawn`), so a dev/test build booted from an agent pane with
//! only `XDG_STATE_HOME` overridden still targets the live `control.json`. Before this
//! guard it would overwrite the live port+token on start (`write_discovery`), and every
//! agent reading the file began failing with `fetch failed` / `unauthorized` /
//! `no such pane` — symptoms indistinguishable from a crashed agent, nothing pointing
//! at the hijacked file. The `single_instance` gate cannot catch this: it is
//! deliberately flavor-salted (`-headless`, see `app::run`) so the GUI app and a
//! headless daemon never meet there — correct for argv hand-off, useless for file
//! ownership.
//!
//! The guard: before `run_server` claims the file, read it; if it records a pid that is
//! ALIVE and not ours, refuse startup with an actionable message naming the live owner
//! and the exact env vars that make a dev instance isolated. A dead recorded pid
//! (crashed owner), a missing/corrupt file, or our own pid (in-process restart via
//! `ControlHost`) claims cleanly — stale recovery needs no manual cleanup.
//! [`recorded_pid`] backs the same ownership test on the delete path
//! (`remove_discovery`), so a refused instance stopping cannot take the live owner's
//! file down with it either.
//!
//! Known limit: two instances that start simultaneously against a not-yet-written file
//! both pass the guard (last write wins). The incident class this closes is a dev build
//! joining a long-lived live instance, where the file always exists first.

use std::io;
use std::path::Path;

/// The subset of the discovery shape (`server::Discovery`) the guard needs to judge
/// ownership. Extra fields (token, events, bindAddress) are ignored.
#[derive(serde::Deserialize)]
struct Owner {
    pid: u32,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    version: String,
}

fn read_owner(path: &Path) -> Option<Owner> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The pid recorded in the discovery file, if it parses. `remove_discovery` uses this
/// to ensure only the recorded owner deletes the file.
pub fn recorded_pid(path: &Path) -> Option<u32> {
    read_owner(path).map(|o| o.pid)
}

/// Refuse to claim `path` if it is owned by a live instance other than `our_pid`.
/// Missing, unreadable, or corrupt files are claimable (nothing live to protect);
/// so is a file recording our own pid or a dead pid.
pub fn ensure_claimable(path: &Path, our_pid: u32) -> io::Result<()> {
    let Some(owner) = read_owner(path) else {
        return Ok(());
    };
    if owner.pid == our_pid || !pid_alive(owner.pid) {
        return Ok(());
    }
    let msg = refusal_message(path, &owner);
    // Also log directly: the GUI host runs `run_server` as a detached task, so the
    // returned error alone would vanish there.
    eprintln!("[control] {msg}");
    Err(io::Error::new(io::ErrorKind::AddrInUse, msg))
}

fn refusal_message(path: &Path, owner: &Owner) -> String {
    format!(
        "refusing to overwrite control file {path}: a live hyperpanes instance owns it \
         (pid {pid}, port {port}, version {version}). Starting against the shared file \
         would hijack the live control plane — agents on the recorded port/token then \
         fail with 'fetch failed' / 'unauthorized' / 'no such pane' (see \
         docs/agent-recovery.md). To run an isolated dev instance set BOTH env vars: \
         XDG_STATE_HOME=<dir> and HYPERPANES_CONTROL_FILE=<dir>/control.json \
         (HYPERPANES_CONTROL_FILE overrides the XDG-derived default — core/src/app.rs:44-46). \
         If pid {pid} is not a hyperpanes instance (pid reuse), delete {path} and retry.",
        path = path.display(),
        pid = owner.pid,
        port = owner.port,
        version = owner.version,
    )
}

/// Is a process with this pid alive right now?
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Non-Linux unix (macOS) has no procfs, std exposes no `kill(2)`, and this crate's
/// Cargo.toml is frozen (no `libc`) — so probe with `kill -0`. Exit 0 means alive;
/// EPERM ("operation not permitted") also means alive, just another user's.
#[cfg(all(unix, not(target_os = "linux")))]
fn pid_alive(pid: u32) -> bool {
    match std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
    {
        Ok(out) => {
            out.status.success()
                || String::from_utf8_lossy(&out.stderr)
                    .to_lowercase()
                    .contains("not permitted")
        }
        Err(_) => false,
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => {
            let mut code = 0u32;
            let alive = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok()
                && code == STILL_ACTIVE.0 as u32;
            let _ = unsafe { CloseHandle(handle) };
            alive
        }
        // Access denied ⇒ the process exists but is another user's / elevated.
        Err(e) => e.code() == windows::core::HRESULT::from(ERROR_ACCESS_DENIED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::server::{remove_discovery, run_server, Shared};
    use crate::session_manager::{SessionEvent, SessionManager};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    // A pid no Linux ever hands out (default pid_max is 4194304) — same convention as
    // the single_instance stale-lock test.
    const DEAD_PID: u32 = 999_999_999;
    // pid 1 (init / the namespace root) is always alive and never this test process.
    const LIVE_FOREIGN_PID: u32 = 1;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hp-guard-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_control(dir: &Path, pid: u32) -> PathBuf {
        let path = dir.join("control.json");
        let json = format!(
            "{{\n  \"port\": 41419,\n  \"token\": \"t\",\n  \"pid\": {pid},\n  \
             \"version\": \"0.0.27\",\n  \"events\": \"ws://127.0.0.1:41419/events?token=t\"\n}}"
        );
        std::fs::write(&path, json).unwrap();
        path
    }

    fn test_shared(control: PathBuf) -> Arc<Shared> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let sessions = Arc::new(SessionManager::new(tx));
        Shared::new(sessions, false, "0.0.0", control)
    }

    #[test]
    fn missing_or_corrupt_file_is_claimable() {
        let dir = scratch("claimable");
        assert!(ensure_claimable(&dir.join("control.json"), 42).is_ok());
        std::fs::write(dir.join("control.json"), b"not json {").unwrap();
        assert!(ensure_claimable(&dir.join("control.json"), 42).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn own_pid_and_dead_pid_are_claimable() {
        let dir = scratch("stale");
        let path = write_control(&dir, std::process::id());
        assert!(ensure_claimable(&path, std::process::id()).is_ok());
        let path = write_control(&dir, DEAD_PID);
        assert!(ensure_claimable(&path, std::process::id()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_foreign_pid_refuses_and_the_message_is_actionable() {
        let dir = scratch("refuse");
        let path = write_control(&dir, LIVE_FOREIGN_PID);
        let err = ensure_claimable(&path, std::process::id()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        let msg = err.to_string();
        // The message must name the live owner…
        assert!(msg.contains("pid 1"), "names the live pid: {msg}");
        assert!(msg.contains("port 41419"), "names the live port: {msg}");
        assert!(msg.contains("version 0.0.27"), "names the version: {msg}");
        assert!(
            msg.contains(&path.display().to_string()),
            "names the file: {msg}"
        );
        // …and the exact isolation recipe + the precedence rule it exists for.
        assert!(
            msg.contains("XDG_STATE_HOME=<dir>"),
            "names XDG_STATE_HOME: {msg}"
        );
        assert!(
            msg.contains("HYPERPANES_CONTROL_FILE=<dir>/control.json"),
            "names HYPERPANES_CONTROL_FILE: {msg}"
        );
        assert!(
            msg.contains("app.rs:44-46"),
            "cites the precedence site: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorded_pid_reads_the_owner() {
        let dir = scratch("recorded");
        let path = write_control(&dir, 777);
        assert_eq!(recorded_pid(&path), Some(777));
        assert_eq!(recorded_pid(&dir.join("nope.json")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Acceptance A: instance B (this test's server) cannot overwrite a control file
    // owned by live instance A (pid 1). Before the guard, run_server served forever
    // after silently clobbering the file — this test then failed on the timeout.
    #[tokio::test]
    async fn run_server_refuses_a_control_file_owned_by_a_live_foreign_pid() {
        let dir = scratch("server-refuse");
        let path = write_control(&dir, LIVE_FOREIGN_PID);
        let before = std::fs::read_to_string(&path).unwrap();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            run_server(test_shared(path.clone())),
        )
        .await
        .expect("run_server must fail fast, not serve");
        let err = res.expect_err("must refuse to claim a live instance's file");
        assert!(err
            .to_string()
            .contains("refusing to overwrite control file"));
        // The live owner's file is byte-for-byte untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Acceptance B: a dead recorded pid is stale — the new instance claims the file
    // cleanly, no manual cleanup.
    #[tokio::test]
    async fn run_server_takes_over_a_stale_file_from_a_dead_pid() {
        let dir = scratch("server-stale");
        let path = write_control(&dir, DEAD_PID);
        let server = tokio::spawn(run_server(test_shared(path.clone())));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if recorded_pid(&path) == Some(std::process::id()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stale file was never claimed: {:?}",
                std::fs::read_to_string(&path)
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Acceptance C: an isolated dev instance (its own control file in its own dir)
    // starts with zero friction and never touches the shared file.
    #[tokio::test]
    async fn isolated_dev_file_claims_cleanly_and_leaves_the_shared_file_alone() {
        let dir = scratch("server-isolated");
        let shared_file = write_control(&dir, LIVE_FOREIGN_PID); // stand-in for the live file
        let live_before = std::fs::read_to_string(&shared_file).unwrap();
        let isolated = dir.join("isolated").join("control.json");
        std::fs::create_dir_all(isolated.parent().unwrap()).unwrap();
        let server = tokio::spawn(run_server(test_shared(isolated.clone())));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while recorded_pid(&isolated) != Some(std::process::id()) {
            assert!(
                std::time::Instant::now() < deadline,
                "isolated file never claimed"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        server.abort();
        assert_eq!(std::fs::read_to_string(&shared_file).unwrap(), live_before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The delete path honors ownership too: a stopping instance must not remove a
    // discovery file it does not own (ControlHost::stop calls remove_discovery
    // unconditionally — without this, a refused dev GUI would delete the live file
    // on quit, trading hijack-by-overwrite for hijack-by-deletion).
    #[test]
    fn remove_discovery_only_deletes_our_own_file() {
        let dir = scratch("remove");
        let foreign = write_control(&dir, LIVE_FOREIGN_PID);
        remove_discovery(&test_shared(foreign.clone()));
        assert!(foreign.exists(), "a foreign owner's file must survive");
        let ours = write_control(&dir, std::process::id());
        remove_discovery(&test_shared(ours.clone()));
        assert!(!ours.exists(), "our own file is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
