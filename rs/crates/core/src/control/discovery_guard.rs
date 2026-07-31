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
//! The guard, run before `run_server` claims the file: the file is refused ONLY when it
//! records a pid that is alive, is not ours, and verifiably looks like a hyperpanes
//! process. Everything else claims cleanly — missing/corrupt file, our own pid
//! (in-process `ControlHost` restart), a dead pid (crashed owner — stale recovery needs
//! no manual cleanup), a live pid whose identity cannot be read, or a live pid that is
//! some unrelated program (pid reuse after reboot/churn). The guard fails OPEN: a
//! wrongly-refused legitimate launch would be a worse wedge than the clobber it
//! prevents, and the identity check is what keeps a recycled pid from bricking startup
//! forever.
//!
//! A refusal is also RETRIED briefly (see [`RETRY_TOTAL`]) while the live owner is
//! still there: during `restartApp` (both scopes) the outgoing instance may overlap the
//! incoming one for a moment. In practice same-flavor restarts are already serialized
//! by the `single_instance` flock (released only when the old process dies) plus the
//! relauncher's 2s sleep (`app::service_restart_request`), but the retry makes the
//! guard safe even if an exiting owner lingers past that.
//!
//! [`recorded_pid`] backs the same ownership test on the delete path
//! (`remove_discovery`), so a refused instance stopping cannot take the live owner's
//! file down with it either.
//!
//! Known limit: two instances that start simultaneously against a not-yet-written file
//! both pass the guard (last write wins). The incident class this closes is a dev build
//! joining a long-lived live instance, where the file always exists first.

use std::io;
use std::path::Path;
use std::time::Duration;

/// How long a refusal is retried while the recorded owner is alive, covering an
/// exiting owner (restart overlap) that has not released the file yet.
const RETRY_TOTAL: Duration = Duration::from_secs(5);
const RETRY_STEP: Duration = Duration::from_millis(250);

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

/// Refuse to claim `path` if it is owned by a live hyperpanes instance other than
/// `our_pid` (retrying briefly in case that owner is mid-exit). See the module docs
/// for the full claim/refuse matrix — everything short of a verified live foreign
/// owner claims.
pub async fn ensure_claimable(path: &Path, our_pid: u32) -> io::Result<()> {
    ensure_claimable_with(path, our_pid, RETRY_TOTAL, RETRY_STEP).await
}

async fn ensure_claimable_with(
    path: &Path,
    our_pid: u32,
    total: Duration,
    step: Duration,
) -> io::Result<()> {
    let deadline = std::time::Instant::now() + total;
    loop {
        let Some(owner) = live_foreign_hyperpanes_owner(path, our_pid) else {
            return Ok(());
        };
        if std::time::Instant::now() >= deadline {
            let msg = refusal_message(path, &owner);
            // Also log directly: the GUI host runs `run_server` as a detached task, so
            // the returned error alone would vanish there.
            eprintln!("[control] {msg}");
            return Err(io::Error::new(io::ErrorKind::AddrInUse, msg));
        }
        tokio::time::sleep(step).await;
    }
}

/// `Some(owner)` only when the file records a pid that is alive, is not ours, AND
/// verifiably looks like a hyperpanes process. `None` (claimable) for everything
/// else, including a live pid with unreadable identity — the guard fails open.
fn live_foreign_hyperpanes_owner(path: &Path, our_pid: u32) -> Option<Owner> {
    let owner = read_owner(path)?;
    if owner.pid == our_pid || !pid_alive(owner.pid) {
        return None;
    }
    match process_name(owner.pid) {
        Some(name) if is_hyperpanes_name(&name) => Some(owner),
        // Alive but some unrelated program (recycled pid), or alive with unreadable
        // identity (permissions, exotic platform): fail open, claim.
        _ => None,
    }
}

/// Does a process name/path look like one of ours? Matches the installed GUI
/// (`/usr/bin/hyperpanes`), dev builds living under a `hyperpanes` checkout, and the
/// core `headless` bin (whose basename carries no "hyperpanes").
fn is_hyperpanes_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("hyperpanes")
        || n.rsplit(['/', '\\', ' '])
            .any(|part| part.strip_suffix(".exe").unwrap_or(part) == "headless")
}

fn refusal_message(path: &Path, owner: &Owner) -> String {
    format!(
        "refusing to overwrite control file {path}: a live hyperpanes instance owns it \
         (pid {pid}, port {port}, version {version}). Starting against the shared file \
         would hijack the live control plane — agents on the recorded port/token then \
         fail with 'fetch failed' / 'unauthorized' / 'no such pane' (see \
         docs/agent-recovery.md). To run an isolated dev instance, relaunch with both \
         env vars set: XDG_STATE_HOME=<dir> HYPERPANES_CONTROL_FILE=<dir>/control.json \
         <your-binary> (HYPERPANES_CONTROL_FILE overrides the XDG-derived default — \
         core/src/app.rs:44-46). If pid {pid} is not a hyperpanes instance, delete \
         {path} and retry.",
        path = path.display(),
        pid = owner.pid,
        port = owner.port,
        version = owner.version,
    )
}

/// Is a process with this pid alive right now? Zombies count as DEAD: an exiting
/// owner that its parent has not reaped yet must not block a claim.
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state field is the first token after the comm's closing paren (the comm
    // itself may contain parens, so split on the LAST one): "pid (comm) S ...".
    match stat
        .rsplit(')')
        .next()
        .and_then(|rest| rest.trim_start().chars().next())
    {
        None | Some('Z') | Some('X') => false,
        Some(_) => true,
    }
}

/// Non-Linux unix (macOS) has no procfs, std exposes no `kill(2)`, and this crate's
/// Cargo.toml is frozen (no `libc`) — so probe with `ps`. No output ⇒ dead; a
/// leading `Z` state ⇒ zombie, dead for our purposes.
#[cfg(all(unix, not(target_os = "linux")))]
fn pid_alive(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    {
        Ok(out) => {
            let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            !stat.is_empty() && !stat.starts_with('Z')
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

/// The process's name/path, best-effort: comm + argv0 on Linux, `ps -o comm=`
/// elsewhere on unix, the image path on Windows. `None` when unreadable.
#[cfg(target_os = "linux")]
fn process_name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok();
    let argv0 = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|bytes| {
            bytes
                .split(|&b| b == 0)
                .next()
                .map(|s| String::from_utf8_lossy(s).into_owned())
        });
    if comm.is_none() && argv0.is_none() {
        return None;
    }
    Some(format!(
        "{} {}",
        comm.unwrap_or_default().trim(),
        argv0.unwrap_or_default()
    ))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_name(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(windows)]
fn process_name(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let res = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    res.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::server::{remove_discovery, run_server, Shared};
    use crate::session_manager::{SessionEvent, SessionManager};
    use std::path::PathBuf;
    use std::sync::Arc;

    // A pid no Linux ever hands out (default pid_max is 4194304) — same convention as
    // the single_instance stale-lock test.
    const DEAD_PID: u32 = 999_999_999;

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
        // Speech settings live beside the control file so this scratch instance never
        // touches the real one (g1/talk added this parameter after g6 was written).
        let speech = control.with_file_name("speech.json");
        Shared::new(sessions, false, "0.0.0", control, speech)
    }

    /// A live process that LOOKS like hyperpanes: `sleep` behind a symlink named
    /// `hyperpanes` (comm and argv0 follow the invoked path). A symlink, not a copy:
    /// copying opens the target for write, and that fd inherited across another
    /// test's concurrent fork makes exec fail with ETXTBSY. Kill+reap when done.
    #[cfg(unix)]
    fn fake_hyperpanes(dir: &Path, secs: &str) -> std::process::Child {
        let sleep_bin = ["/usr/bin/sleep", "/bin/sleep"]
            .iter()
            .find(|p| Path::new(p).exists())
            .expect("no sleep binary");
        let fake = dir.join("hyperpanes");
        std::os::unix::fs::symlink(sleep_bin, &fake).unwrap();
        std::process::Command::new(&fake).arg(secs).spawn().unwrap()
    }

    // Claim instantly: no retry budget needed for the claimable cases.
    async fn claimable_now(path: &Path, our_pid: u32) -> bool {
        ensure_claimable_with(path, our_pid, Duration::ZERO, Duration::ZERO)
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn missing_or_corrupt_file_is_claimable() {
        let dir = scratch("claimable");
        assert!(claimable_now(&dir.join("control.json"), 42).await);
        std::fs::write(dir.join("control.json"), b"not json {").unwrap();
        assert!(claimable_now(&dir.join("control.json"), 42).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn own_pid_and_dead_pid_are_claimable() {
        let dir = scratch("stale");
        let path = write_control(&dir, std::process::id());
        assert!(claimable_now(&path, std::process::id()).await);
        let path = write_control(&dir, DEAD_PID);
        assert!(claimable_now(&path, std::process::id()).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Pid reuse fails OPEN: pid 1 is alive but is init/systemd, not hyperpanes — a
    // recycled pid must never brick a legitimate launch forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_foreign_non_hyperpanes_pid_is_claimed() {
        let dir = scratch("pid-reuse");
        let path = write_control(&dir, 1);
        assert!(claimable_now(&path, std::process::id()).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_hyperpanes_pid_refuses_and_the_message_is_actionable() {
        let dir = scratch("refuse");
        let mut child = fake_hyperpanes(&dir, "30");
        let path = write_control(&dir, child.id());
        let err = ensure_claimable_with(&path, std::process::id(), Duration::ZERO, Duration::ZERO)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        let msg = err.to_string();
        // The message must name the live owner…
        assert!(
            msg.contains(&format!("pid {}", child.id())),
            "names the live pid: {msg}"
        );
        assert!(msg.contains("port 41419"), "names the live port: {msg}");
        assert!(msg.contains("version 0.0.27"), "names the version: {msg}");
        assert!(
            msg.contains(&path.display().to_string()),
            "names the file: {msg}"
        );
        // …and the exact copy-pasteable isolation recipe + the precedence rule.
        assert!(
            msg.contains("XDG_STATE_HOME=<dir> HYPERPANES_CONTROL_FILE=<dir>/control.json"),
            "gives the exact env line: {msg}"
        );
        assert!(
            msg.contains("app.rs:44-46"),
            "cites the precedence site: {msg}"
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The restartApp overlap: the recorded owner is a live hyperpanes process that is
    // on its way out. The bounded retry outlasts it and the claim proceeds — the
    // incoming instance of a restart is never refused. (Same-flavor restarts are also
    // serialized by the single_instance flock before this guard even runs.)
    #[cfg(unix)]
    #[tokio::test]
    async fn exiting_hyperpanes_owner_is_claimed_after_retry() {
        let dir = scratch("restart");
        let mut child = fake_hyperpanes(&dir, "1");
        let path = write_control(&dir, child.id());
        let started = std::time::Instant::now();
        ensure_claimable_with(
            &path,
            std::process::id(),
            Duration::from_secs(10),
            Duration::from_millis(50),
        )
        .await
        .expect("an exiting owner must be claimable within the retry window");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "claimed by retry, not by exhausting the budget"
        );
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn recorded_pid_reads_the_owner() {
        let dir = scratch("recorded");
        let path = write_control(&dir, 777);
        assert_eq!(recorded_pid(&path), Some(777));
        assert_eq!(recorded_pid(&dir.join("nope.json")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hyperpanes_names_match_and_strangers_do_not() {
        assert!(is_hyperpanes_name("hyperpanes /usr/bin/hyperpanes"));
        assert!(is_hyperpanes_name("headless ./target/debug/headless"));
        assert!(is_hyperpanes_name(
            r"C:\Program Files\hyperpanes\hyperpanes.exe"
        ));
        assert!(!is_hyperpanes_name("systemd /usr/lib/systemd/systemd"));
        assert!(!is_hyperpanes_name("chrome --headless=new"));
    }

    // Acceptance A: instance B (this test's server) cannot overwrite a control file
    // owned by live instance A (a real running process named hyperpanes). Before the
    // guard, run_server served forever after silently clobbering the file — this test
    // then failed on the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_server_refuses_a_control_file_owned_by_a_live_foreign_pid() {
        let dir = scratch("server-refuse");
        let mut child = fake_hyperpanes(&dir, "30");
        let path = write_control(&dir, child.id());
        let before = std::fs::read_to_string(&path).unwrap();
        let res = tokio::time::timeout(
            Duration::from_secs(15), // outlasts the guard's 5s live-owner retry
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
        let _ = child.kill();
        let _ = child.wait();
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
    #[cfg(unix)]
    #[tokio::test]
    async fn isolated_dev_file_claims_cleanly_and_leaves_the_shared_file_alone() {
        let dir = scratch("server-isolated");
        let mut child = fake_hyperpanes(&dir, "30"); // stand-in live owner of the shared file
        let shared_file = write_control(&dir, child.id());
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
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The delete path honors ownership too: a stopping instance must not remove a
    // discovery file it does not own (ControlHost::stop calls remove_discovery
    // unconditionally — without this, a refused dev GUI would delete the live file
    // on quit, trading hijack-by-overwrite for hijack-by-deletion).
    #[test]
    fn remove_discovery_only_deletes_our_own_file() {
        let dir = scratch("remove");
        let foreign = write_control(&dir, DEAD_PID + 1);
        remove_discovery(&test_shared(foreign.clone()));
        assert!(foreign.exists(), "a foreign owner's file must survive");
        let ours = write_control(&dir, std::process::id());
        remove_discovery(&test_shared(ours.clone()));
        assert!(!ours.exists(), "our own file is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
