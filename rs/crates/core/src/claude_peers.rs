//! Reader for Claude Code's **peer-session registry** — the on-disk record each `claude`
//! session publishes so other sessions can find and message it.
//!
//! Claude Code >= 2.1.224 writes `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` for every session
//! with cross-session messaging enabled, and keeps a `status` field in it that says whether
//! that session is mid-turn. That is an **authoritative** liveness signal, published by the
//! agent itself — unlike [`crate::control::readmodel::Activity`], which infers the same thing
//! from pty output going quiet. A pane whose agent is thinking with no output looks idle to the
//! heuristic; here it reads `busy`.
//!
//! Two things make reading this less obvious than it sounds:
//!
//! * **The registry follows `CLAUDE_CONFIG_DIR`.** Goal panes rotate accounts
//!   ([`crate::claude_accounts`]), so their records do NOT land in `~/.claude/sessions`. We scan
//!   every known account dir, de-duplicating by the resolved path because
//!   `setup-claude-accounts.sh` symlinks the rotated accounts at one shared store.
//! * **A record outlives its process.** The file is not always cleaned up on exit, so a `pid`
//!   is only meaningful once confirmed alive. [`Peer::is_live`] is the caller's gate.
//!
//! This module is READ-ONLY and does no I/O beyond directory reads: it never writes a record,
//! never connects to a socket, and never delivers anything. Delivery, if it is ever wired, is a
//! separate concern that would use [`Peer::messaging_socket_path`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Whether a peer session is mid-turn, as the session itself reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    /// Finished its turn with nothing queued.
    Idle,
    /// Working on a turn.
    Busy,
    /// Running a shell command (the Bash tool). Observed on a live orchestrator; agents sit
    /// here a lot, so folding it into `Busy` would lose a real distinction.
    Shell,
    /// A value this build does not know. Kept distinct from `Idle` on purpose: the vocabulary
    /// is undocumented and has already grown once (`shell` was not in the set this was first
    /// written against), and silently reading a new state as "idle" would tell a watchdog the
    /// agent is free when it is not.
    Unknown,
}

impl PeerStatus {
    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("idle") => PeerStatus::Idle,
            Some("busy") => PeerStatus::Busy,
            Some("shell") => PeerStatus::Shell,
            _ => PeerStatus::Unknown,
        }
    }

    /// The wire spelling, for surfacing alongside the existing activity vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerStatus::Idle => "idle",
            PeerStatus::Busy => "busy",
            PeerStatus::Shell => "shell",
            PeerStatus::Unknown => "unknown",
        }
    }

    /// Whether the session has finished its turn with nothing queued.
    ///
    /// The ONLY safe way to consume this enum. Ask "is it idle", never "is it busy": the
    /// status vocabulary is Claude Code's and it grows, so anything not positively known to be
    /// idle has to count as working. A watchdog that inverts this nudges live agents.
    pub fn is_idle(self) -> bool {
        matches!(self, PeerStatus::Idle)
    }
}

/// One session's published record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// OS process id of the `claude` session. Also the record's file stem.
    pub pid: u32,
    /// Claude Code's own session id (the one `claude --resume` takes).
    pub session_id: String,
    /// Addressable name — what `SendMessage` targets and `/list-agents` shows.
    pub name: String,
    /// `true` when the name came from `--name` / `/rename` rather than being derived from the
    /// cwd basename. Hyperpanes sets `--name` on goal panes, so a derived name on one of ours
    /// means the flag did not survive the spawn.
    pub name_is_explicit: bool,
    /// Working directory the session was started in.
    pub cwd: String,
    /// Self-reported turn state.
    pub status: PeerStatus,
    /// Unix-domain socket other sessions deliver to. `None` when the session could not bind one.
    pub messaging_socket_path: Option<String>,
    /// Which account dir this record was found under — the discovery boundary. Two sessions can
    /// only see each other when their registries resolve to the same directory.
    pub registry_dir: PathBuf,
}

impl Peer {
    /// Whether the recorded pid is still running.
    ///
    /// A stale record is indistinguishable from a live one by content alone, so every consumer
    /// has to ask. Deliberately NOT folded into parsing: tests stay pure, and a caller that
    /// wants the raw record (to report a stale entry, say) can still get it.
    pub fn is_live(&self) -> bool {
        process_is_live(self.pid)
    }
}

#[cfg(unix)]
fn process_is_live(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).is_dir()
}

#[cfg(not(unix))]
fn process_is_live(_pid: u32) -> bool {
    // No cheap, dependency-free liveness probe here; treat the record as live and let the
    // caller's own pane bookkeeping decide. Reporting `false` would be worse — it would mark
    // every healthy Windows agent dead.
    true
}

/// Parse one `<pid>.json` record. `None` when the bytes are not a record we understand.
///
/// Only `pid` is required. Everything else degrades: a record missing `name` or `cwd` is still
/// useful for liveness, and refusing it outright would hide a live agent from a watchdog.
pub fn parse_record(bytes: &str, registry_dir: &Path) -> Option<Peer> {
    let v: serde_json::Value = serde_json::from_str(bytes).ok()?;
    let pid = v.get("pid")?.as_u64()?;
    if pid == 0 || pid > u32::MAX as u64 {
        return None;
    }
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(Peer {
        pid: pid as u32,
        session_id: s("sessionId"),
        name: s("name"),
        name_is_explicit: v.get("nameSource").and_then(|x| x.as_str()) == Some("user"),
        cwd: s("cwd"),
        status: PeerStatus::parse(v.get("status").and_then(|x| x.as_str())),
        messaging_socket_path: v
            .get("messagingSocketPath")
            .and_then(|x| x.as_str())
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string()),
        registry_dir: registry_dir.to_path_buf(),
    })
}

/// Every registry directory to scan: `<account>/sessions` for each known account.
///
/// De-duplicated by canonical path, because the rotated accounts are symlinked at one shared
/// store — without this the same record is read four times and reported as four peers.
pub fn registry_dirs() -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for account in crate::claude_accounts::config_dirs() {
        let dir = account.join("sessions");
        if !dir.is_dir() {
            continue;
        }
        let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(dir);
    }
    out
}

/// Read every peer record across all account registries, newest-pid-wins on collision.
///
/// Returns records whether or not their process is alive — call [`Peer::is_live`] to filter.
/// Errors are swallowed per-entry: one unreadable file must not hide the rest.
pub fn read_all() -> Vec<Peer> {
    let mut by_pid: BTreeMap<u32, Peer> = BTreeMap::new();
    for dir in registry_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(peer) = parse_record(&text, &dir) {
                by_pid.insert(peer.pid, peer);
            }
        }
    }
    by_pid.into_values().collect()
}

/// The live peer owning `pid`, when it published a record.
pub fn by_pid(pid: u32) -> Option<Peer> {
    read_all().into_iter().find(|p| p.pid == pid && p.is_live())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        PathBuf::from("/tmp/example/.claude/sessions")
    }

    /// A real record, captured from a live goal orchestrator.
    const ORCHESTRATOR: &str = r#"{"pid":331629,"sessionId":"bc7d1320-6ab5-4c0f-a40d-72f38e56833c",
        "cwd":"/home/u/dev/canora-sync","version":"2.1.276","peerProtocol":1,
        "peerFeatures":["notify_idle"],"kind":"interactive","entrypoint":"cli",
        "messagingSocketPath":"/run/user/1000/cc-socks/331629.sock",
        "name":"goals-canora-sync","nameSource":"user","status":"busy"}"#;

    #[test]
    fn parses_a_live_orchestrator_record() {
        let p = parse_record(ORCHESTRATOR, &dir()).expect("record parses");
        assert_eq!(p.pid, 331629);
        assert_eq!(p.name, "goals-canora-sync");
        assert_eq!(p.status, PeerStatus::Busy);
        assert_eq!(
            p.messaging_socket_path.as_deref(),
            Some("/run/user/1000/cc-socks/331629.sock")
        );
        // `--name` took: a derived name here would mean the flag never reached the spawn.
        assert!(p.name_is_explicit);
    }

    #[test]
    fn a_derived_name_is_not_explicit() {
        let raw = r#"{"pid":42,"name":"hyperpanes-9a","nameSource":"derived","status":"idle"}"#;
        let p = parse_record(raw, &dir()).unwrap();
        assert!(!p.name_is_explicit);
        assert_eq!(p.status, PeerStatus::Idle);
    }

    /// An unrecognised status must NOT read as idle — a watchdog would take that as "free".
    #[test]
    fn an_unknown_status_is_not_idle() {
        for raw in [
            r#"{"pid":1,"status":"compacting"}"#,
            r#"{"pid":1}"#,
            r#"{"pid":1,"status":null}"#,
        ] {
            let p = parse_record(raw, &dir()).unwrap();
            assert_eq!(p.status, PeerStatus::Unknown, "{raw}");
            assert_ne!(p.status, PeerStatus::Idle);
        }
    }

    /// Only `pid` is load-bearing; a sparse record still reports liveness.
    #[test]
    fn a_sparse_record_still_parses() {
        let p = parse_record(r#"{"pid":7}"#, &dir()).unwrap();
        assert_eq!(p.pid, 7);
        assert!(p.name.is_empty());
        assert!(p.messaging_socket_path.is_none());
    }

    #[test]
    fn rubbish_and_impossible_pids_are_refused() {
        assert!(parse_record("not json", &dir()).is_none());
        assert!(parse_record(r#"{"no":"pid"}"#, &dir()).is_none());
        assert!(parse_record(r#"{"pid":0}"#, &dir()).is_none());
        assert!(parse_record(r#"{"pid":99999999999}"#, &dir()).is_none());
        // An empty socket path is "did not bind", not a path to try to connect to.
        let p = parse_record(r#"{"pid":5,"messagingSocketPath":""}"#, &dir()).unwrap();
        assert!(p.messaging_socket_path.is_none());
    }

    #[test]
    fn status_spellings_round_trip() {
        assert_eq!(PeerStatus::Idle.as_str(), "idle");
        assert_eq!(PeerStatus::Busy.as_str(), "busy");
        assert_eq!(PeerStatus::Shell.as_str(), "shell");
        assert_eq!(PeerStatus::Unknown.as_str(), "unknown");
    }

    /// `shell` was found on a LIVE orchestrator after this module was first written against a
    /// vocabulary of just idle/busy — the reason `is_idle` is the only sanctioned accessor.
    #[test]
    fn only_idle_counts_as_idle() {
        assert!(PeerStatus::Idle.is_idle());
        for s in [PeerStatus::Busy, PeerStatus::Shell, PeerStatus::Unknown] {
            assert!(!s.is_idle(), "{s:?} must not read as idle");
        }
        let running_a_bash_tool = parse_record(r#"{"pid":9,"status":"shell"}"#, &dir()).unwrap();
        assert_eq!(running_a_bash_tool.status, PeerStatus::Shell);
        assert!(!running_a_bash_tool.status.is_idle());
    }
}
