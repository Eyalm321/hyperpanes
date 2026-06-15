//! Single-instance gate + argv hand-off (replaces Electron `requestSingleInstanceLock`).
//!
//! Two distinct mechanisms — deliberately NOT the same primitive:
//!
//! - **Detector = a named mutex** (`CreateMutexW`; `GetLastError == ERROR_ALREADY_EXISTS`
//!   ⇒ a primary already runs). A mutex is the right race-free detector: the kernel makes
//!   "create-or-find" atomic, and it is auto-released when the owning process dies (so a
//!   crashed primary never wedges every future launch). Pipe-*connect* must NOT be the
//!   detector — at startup the primary may have the pipe down for a window and a secondary
//!   would wrongly promote itself.
//! - **Hand-off = a named pipe**. The primary runs a pipe server; a secondary connects and
//!   sends `{ argv, cwd }` as JSON, then exits. The primary feeds that to its launch
//!   routing (`crate::cli::routing`, wired up in the binary — not here).
//!
//! Both names derive from a stable per-user salt so two users on one machine don't collide,
//! and so a dev build (different userData) stays independent of an installed build — matching
//! the Electron behavior where the lock is keyed off the userData path.
//!
//! ## Testing
//! The pure pieces — name derivation, the per-user salt, and the `{argv,cwd}` JSON wire
//! shape — are covered cross-platform. The Win32 detector and the pipe round-trip are
//! covered in-process under `#[cfg(windows)]` (two mutex handles to one name; a server +
//! client over a real named pipe). The genuine TWO-PROCESS behavior (launch B while A runs →
//! A receives B's argv, B exits 0) is a manual/integration check — see `LIVE CHECK` below.
//!
//! ```text
//! LIVE CHECK (two real processes):
//!   1. Start instance A (becomes primary; holds the mutex; serves the pipe).
//!   2. Start instance B with some argv. B's `acquire()` sees ERROR_ALREADY_EXISTS →
//!      Secondary; B calls `forward({argv,cwd})` then exits 0.
//!   3. A's `run_server` handler fires with B's `{argv,cwd}`; A applies the routing.
//!   4. Kill A; start C → C's mutex create succeeds (no ERROR_ALREADY_EXISTS) → C is the
//!      new primary. (Confirms the crashed/closed-primary path releases the mutex.)
//! ```
//!
//! Owned by track `platform`.

use serde::{Deserialize, Serialize};

/// What a secondary instance hands the primary: the raw CLI argv and the launch cwd. The
/// primary decodes this and routes it (attach into the focused window, or open new
/// window(s)) exactly as Electron's `second-instance` event did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffMessage {
    pub argv: Vec<String>,
    pub cwd: String,
}

/// The OS object names this instance uses, both derived from one salt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceNames {
    /// Named-mutex name (the `Local\` namespace = per-logon-session, one instance per user).
    pub mutex: String,
    /// Full named-pipe path (`\\.\pipe\...`).
    pub pipe: String,
}

/// Derive the (mutex, pipe) names from an arbitrary salt. Deterministic: identical salt →
/// identical names; different salt → (with overwhelming probability) different names. The
/// salt is hashed so it is always a short, namespace-safe token regardless of its content
/// (a userData path can contain spaces, backslashes, drive colons, …).
pub fn instance_names(salt: &str) -> InstanceNames {
    let h = format!("{:016x}", fnv1a64(salt));
    InstanceNames {
        mutex: format!("Local\\hyperpanes.singleton.{h}"),
        pipe: format!(r"\\.\pipe\hyperpanes.handoff.{h}"),
    }
}

/// A stable per-user salt, never empty. Windows: keyed off the user's roaming-appdata
/// path (which already embeds the username and is exactly what the Electron userData
/// lock was keyed under), falling back to the username. Unix: keyed off
/// `$XDG_RUNTIME_DIR` (already per-user) then `$HOME` then `$USER` — the seam-doc
/// shape, landed by the `unix-core` track under the granted `mod.rs` exception.
pub fn user_salt() -> String {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return appdata.to_lowercase();
            }
        }
        if let Ok(user) = std::env::var("USERNAME") {
            if !user.is_empty() {
                return format!("user:{}", user.to_lowercase());
            }
        }
        "hyperpanes-default".to_string()
    }
    #[cfg(not(windows))]
    {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            if !dir.is_empty() {
                return dir;
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return home;
            }
        }
        if let Ok(user) = std::env::var("USER") {
            if !user.is_empty() {
                return format!("user:{user}");
            }
        }
        "hyperpanes-default".to_string()
    }
}

// FNV-1a (64-bit). Tiny, dependency-free, and stable across runs/processes — all we need to
// turn an arbitrary salt into a fixed-width hex token.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The outcome of trying to become the single instance.
pub enum Instance {
    /// We are the primary — hold this value for the app's lifetime (dropping it releases the
    /// mutex). Call [`PrimaryInstance::run_server`] to accept hand-offs from later launches.
    Primary(PrimaryInstance),
    /// A primary already runs — forward our argv to it via [`SecondaryInstance::forward`],
    /// then exit.
    Secondary(SecondaryInstance),
}

// The per-platform half of the seam: `acquire` + the Primary/Secondary instance types.
// `windows.rs` is the original named-mutex + named-pipe implementation (moved verbatim);
// `unix.rs` is the current Unsupported stub, owned by the Wave-1 `unix-core` track
// (expected shape: an O_EXCL/flock lock file as the detector + a unix domain socket for
// the hand-off). The surface is frozen in `docs/ports-seams.md`:
//   pub fn acquire(salt: &str) -> io::Result<Instance>;
//   pub struct PrimaryInstance;   // pipe_name(&self); async run_server(self, FnMut(HandoffMessage))
//   pub struct SecondaryInstance; // pipe_name(&self); async forward(&self, &HandoffMessage)
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(not(windows))]
#[path = "unix.rs"]
mod platform;

pub use platform::{acquire, PrimaryInstance, SecondaryInstance};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_are_deterministic_for_a_salt() {
        let a = instance_names("C:\\Users\\me\\AppData\\Roaming\\hyperpanes");
        let b = instance_names("C:\\Users\\me\\AppData\\Roaming\\hyperpanes");
        assert_eq!(a, b);
    }

    #[test]
    fn different_salts_yield_different_names() {
        let a = instance_names("user-a");
        let b = instance_names("user-b");
        assert_ne!(a.mutex, b.mutex);
        assert_ne!(a.pipe, b.pipe);
    }

    #[test]
    fn names_use_the_expected_namespaces() {
        let n = instance_names("anything");
        assert!(n.mutex.starts_with("Local\\hyperpanes.singleton."));
        assert!(n.pipe.starts_with(r"\\.\pipe\hyperpanes.handoff."));
    }

    #[test]
    fn the_salt_is_hashed_not_embedded_raw() {
        // A salt full of path separators / colons must NOT leak into a pipe name verbatim;
        // it is reduced to a fixed-width hex token.
        let n = instance_names("C:\\a b\\c");
        let token = n.pipe.rsplit('.').next().unwrap();
        assert_eq!(token.len(), 16);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn user_salt_is_never_empty() {
        assert!(!user_salt().is_empty());
    }

    #[test]
    fn handoff_message_json_round_trips() {
        let msg = HandoffMessage {
            argv: vec![
                "hyperpanes".to_string(),
                "--tab".to_string(),
                ".".to_string(),
            ],
            cwd: "C:\\work".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: HandoffMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn handoff_message_wire_shape_is_argv_and_cwd() {
        let msg = HandoffMessage {
            argv: vec!["a".to_string()],
            cwd: "b".to_string(),
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert!(v.get("argv").is_some());
        assert!(v.get("cwd").is_some());
    }
}
