//! Claim-holder identity: which local process holds a work-queue claim, and is it still
//! that same process?
//!
//! Boot recovery ([`WorkQueue::recover_in_flight`](super::work::WorkQueue::recover_in_flight))
//! used to requeue every `claimed` row on the premise that every worker died with the
//! previous owner. The session daemon broke that premise: worker panes (`hyperpanes worker`
//! runners and their children) live in the daemon and SURVIVE a GUI relaunch, so the new
//! primary was requeueing live claims, fencing out the surviving worker and running the
//! task twice. The runner now names its pid on claim (`holderPid`); the server checks that
//! pid is a live local process and records it here as a [`Holder`]: the pid plus a
//! process-start fingerprint, so a pid the OS later recycles for an unrelated process
//! (or the same number after a reboot) does not read as the original holder.
//!
//! Fingerprints: Linux uses `<boot_id>:<starttime>` from procfs (exact, survives nothing
//! across a reboot); other unix uses `ps -o lstart=` (start time to the second); Windows
//! records none and falls back to a pid-only liveness check. A missing fingerprint only
//! weakens pid-reuse detection, and the cost of a false "alive" is bounded: boot recovery
//! keeps the claim, re-arms its lease, and the lease reaper requeues it once that expires.

/// A claim's holder process, as recorded at claim time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// Process-start fingerprint (see the module doc); `None` where the platform gives none.
    pub start: Option<String>,
}

/// Identify a live local process by pid for recording as a claim holder. `None` when no
/// such live process exists here (a remote claimer, a stale or bogus pid): such a claim is
/// recorded without a holder and keeps the legacy boot-recovery behaviour.
pub fn identify(pid: u32) -> Option<Holder> {
    if pid == 0 || !pid_alive(pid) {
        return None;
    }
    Some(Holder {
        pid,
        start: process_start(pid),
    })
}

/// Is the recorded holder still the same live process? A fingerprint mismatch (or one
/// that can no longer be read) means the pid now belongs to someone else: dead.
pub fn is_alive(pid: u32, start: Option<&str>) -> bool {
    if pid == 0 || !pid_alive(pid) {
        return false;
    }
    match start {
        None => true,
        Some(recorded) => process_start(pid).as_deref() == Some(recorded),
    }
}

/// Is a process with this pid alive right now? Zombies count as DEAD: an exited process
/// that its parent has not reaped yet is not going to ack anything (and, for the
/// discovery guard, must not block a claim on `control.json`).
#[cfg(target_os = "linux")]
pub(crate) fn pid_alive(pid: u32) -> bool {
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
pub(crate) fn pid_alive(pid: u32) -> bool {
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
pub(crate) fn pid_alive(pid: u32) -> bool {
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

/// `<boot_id>:<starttime>`: `starttime` (field 22 of `/proc/<pid>/stat`, clock ticks since
/// boot) is unique per pid within one boot, and the boot id makes it unique across boots.
#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the comm's LAST ')' start at field 3 (state), so field 22 is index 19.
    let starttime = stat.rsplit(')').next()?.split_whitespace().nth(19)?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{}:{starttime}", boot_id.trim()))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_start(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    let start = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!start.is_empty()).then_some(start)
}

#[cfg(windows)]
fn process_start(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_identified_and_alive() {
        let me = identify(std::process::id()).expect("our own pid is a live local process");
        assert_eq!(me.pid, std::process::id());
        #[cfg(unix)]
        assert!(me.start.is_some(), "unix records a start fingerprint");
        assert!(is_alive(me.pid, me.start.as_deref()));
    }

    #[test]
    fn a_mismatched_fingerprint_reads_as_a_recycled_pid() {
        let me = identify(std::process::id()).unwrap();
        // Same pid, different start: the pid was reused by another process (or rebooted).
        if me.start.is_some() {
            assert!(!is_alive(me.pid, Some("other-boot:12345")));
        }
        // Pid-only records (no fingerprint) degrade to plain liveness.
        assert!(is_alive(me.pid, None));
    }

    #[test]
    fn an_exited_process_is_dead_and_unidentifiable() {
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit 0"])
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let pid = child.id();
        child.wait().unwrap(); // reaped: the pid is gone (no zombie)
        assert!(!is_alive(pid, None));
        assert!(identify(pid).is_none());
        assert!(identify(0).is_none());
        assert!(!is_alive(0, None));
    }
}
