//! Why did that pane's program die? An append-only record of every pane-session end.
//!
//! The GUI and the session daemon both run with stdout/stderr on `/dev/null` (the GUI is
//! relaunched detached; the daemon is spawned into its own scope), so an `eprintln!` about a
//! kill reaches nobody. When a GUI-only restart took down several agent panes, nothing on the
//! machine could say which code path ended them. This module is that missing record: one JSON
//! line per event in [`paths::session_events_log`](super::paths::session_events_log).
//!
//! * The **GUI** records every kill it asks for, with the reason its caller gave
//!   (`user-close-pane`, `window-closed`, `control:closePane`, …) and the pane id it showed.
//! * The **session registry** (in the daemon, or in-process) records the kill it carried out
//!   and every natural exit with its code, so a pane whose program ended on its own is told
//!   apart from one that was killed, even when no GUI is attached.
//!
//! Best effort by design: a failed write is dropped, never surfaced, and never panics — a
//! diagnostics log must not take down the process it is diagnosing. The file is capped at
//! [`MAX_BYTES`]; on overflow it is renamed to `session-events.log.1` (replacing the previous
//! one) and a fresh file starts, so at most twice the cap sits on disk.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

/// The file is rotated once it reaches this size.
pub const MAX_BYTES: u64 = 1024 * 1024;

/// Which process wrote a line. Set once at startup ([`set_role`]); `app` when never set.
static ROLE: OnceLock<&'static str> = OnceLock::new();

/// Serializes appends + rotation within this process (two processes may still interleave
/// whole lines: each line is one `write_all` on an `O_APPEND` handle).
static LOCK: Mutex<()> = Mutex::new(());

/// Name the writing process in every line it records (`gui`, `daemon`, `headless`, …) and
/// turn [`record`] on. Only the first call wins. A process that never calls it (unit tests,
/// library embedders) records nothing to the live state dir.
pub fn set_role(role: &'static str) {
    let _ = ROLE.set(role);
}

fn role() -> &'static str {
    ROLE.get().copied().unwrap_or("app")
}

/// One recorded event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionEvent<'a> {
    /// Milliseconds since the Unix epoch.
    pub ts: u64,
    /// `gui`, `daemon`, … — see [`set_role`].
    pub role: &'a str,
    /// The writing process.
    pub pid: u32,
    /// `kill-request` (the GUI or control API asked), `kill` (the registry killed the pty),
    /// `kill-all`, or `exit` (the program ended on its own).
    pub event: &'a str,
    /// The session uid.
    pub uid: &'a str,
    /// Why: the caller's reason for a kill, `code=<n>` for an exit.
    pub reason: &'a str,
    /// The pane id the GUI shows for this session, when the writer knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<&'a str>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record `event` for `uid` in the live log. A no-op until [`set_role`] ran, so tests that
/// drive real sessions never write the user's state dir. See the module docs.
pub fn record(event: &str, uid: &str, reason: &str, pane_id: Option<&str>) {
    if ROLE.get().is_none() {
        return;
    }
    record_to(
        &super::paths::session_events_log(),
        event,
        uid,
        reason,
        pane_id,
    );
}

/// [`record`] into an explicit file (tests, and any embedder with its own state dir).
pub fn record_to(path: &Path, event: &str, uid: &str, reason: &str, pane_id: Option<&str>) {
    let line = SessionEvent {
        ts: now_ms(),
        role: role(),
        pid: std::process::id(),
        event,
        uid,
        reason,
        pane_id,
    };
    let Ok(mut json) = serde_json::to_string(&line) else {
        return;
    };
    json.push('\n');
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _ = append(path, json.as_bytes());
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

fn append(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= MAX_BYTES) {
        let _ = std::fs::rename(path, rotated(path));
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty scratch dir under the system temp dir.
    fn scratch() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hp-session-log-{}-{}-{}",
            std::process::id(),
            now_ms(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn lines(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is JSON"))
            .collect()
    }

    #[test]
    fn records_one_json_line_per_event_with_reason_and_pane() {
        let dir = scratch();
        let log = dir.join("nested").join("session-events.log");
        record_to(
            &log,
            "kill-request",
            "u1",
            "user-close-pane",
            Some("pane-1"),
        );
        record_to(&log, "exit", "u2", "code=0", None);
        let got = lines(&log);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["event"], "kill-request");
        assert_eq!(got[0]["uid"], "u1");
        assert_eq!(got[0]["reason"], "user-close-pane");
        assert_eq!(got[0]["pane_id"], "pane-1");
        assert_eq!(got[0]["pid"], std::process::id());
        assert!(got[0]["ts"].as_u64().unwrap() > 0);
        assert_eq!(got[1]["event"], "exit");
        assert!(got[1].get("pane_id").is_none(), "absent pane id is omitted");
    }

    #[test]
    fn rotates_at_the_cap_keeping_one_previous_file() {
        let dir = scratch();
        let log = dir.join("session-events.log");
        std::fs::write(&log, vec![b'x'; MAX_BYTES as usize]).unwrap();
        record_to(&log, "kill", "u3", "control:closePane", None);
        assert_eq!(lines(&log).len(), 1, "a fresh file after rotation");
        let old = rotated(&log);
        assert_eq!(std::fs::metadata(&old).unwrap().len(), MAX_BYTES);
        // A second rotation replaces the previous `.1`.
        std::fs::write(&log, vec![b'y'; MAX_BYTES as usize]).unwrap();
        record_to(&log, "kill", "u4", "x", None);
        assert_eq!(std::fs::read(&old).unwrap()[0], b'y');
    }

    #[test]
    fn an_unwritable_path_is_silently_dropped() {
        let dir = scratch();
        let file = dir.join("a-file");
        std::fs::write(&file, b"").unwrap();
        // Parent is a regular file: create_dir_all fails; record_to must not panic.
        record_to(&file.join("log"), "kill", "u", "r", None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn record_is_off_until_a_role_is_set() {
        // No test in this crate calls set_role: `record` must not touch the live state dir.
        assert!(ROLE.get().is_none());
        record("kill", "u-test", "must-not-be-written", None);
    }
}
