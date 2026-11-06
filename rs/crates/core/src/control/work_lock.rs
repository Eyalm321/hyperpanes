//! Single-owner lock for the durable work-queue DB (`work.db`).
//!
//! Exactly one process — the *owner* — may attach a given `work.db`, and only the owner runs
//! boot recovery ([`WorkQueue::recover_in_flight`](super::work::WorkQueue::recover_in_flight))
//! and the lease reaper against it. Boot recovery is only correct under that assumption: it
//! requeues EVERY `claimed` row because "every worker pane is a child of this process, so on
//! restart they are all dead". A second process that opens the same DB (a half-isolated dev
//! build, a headless control server beside the GUI) makes that false — its boot recovery
//! requeued a live instance's claimed task, staling the worker's fencing token and running the
//! task twice (the incident behind this module; see `docs/dev-instance-isolation.md`). Two
//! writers are unsafe even without recovery: each `WorkQueue` caches the `seq` / fencing-token
//! counters at open, so two of them mint colliding ids and tokens.
//!
//! **Mechanism.** An exclusive OS file lock on `work.db.lock`, beside the DB
//! ([`std::fs::File::try_lock`]: `flock(LOCK_EX|LOCK_NB)` on unix, `LockFileEx` on Windows).
//! The kernel drops the lock when the owning process dies, however it dies, so there is no
//! stale-lock cleanup and pid reuse cannot fake an owner. The file is opened `O_CLOEXEC` (std's
//! default), so the session daemon, pty children and the restart relauncher never inherit it.
//! After acquiring, the owner writes an [`OwnerRecord`] into the file so a refused process can
//! name who holds it (unreadable on Windows while locked — the message then omits it). The lock
//! file is never deleted: unlinking a locked file would let a newcomer lock a fresh inode at the
//! same path and become a second owner.
//!
//! **Lifetime = the process.** An acquired lock is parked in a process-global registry and held
//! until exit, so the GUI's in-process control-host restart (toggle control off/on → a fresh
//! `Shared`, while the old one may still be alive in a lingering handler) re-attaches as the
//! same owner instead of contending with itself.
//!
//! **Restart overlap.** [`acquire`] retries for a bounded window (the embedder passes
//! [`ACQUIRE_WAIT`]) so an incoming primary that boots while the outgoing one is still exiting
//! (`restartApp`, a headless respawn) waits for it rather than giving up. In practice the GUI's
//! relaunch is already serialized behind the single-instance lock, which the old process holds
//! until it dies. A process still refused after the window does NOT attach. It keeps the
//! in-memory queue, so neither boot recovery nor the reaper can ever reach the owner's DB.
//!
//! **Fails open on lock errors** other than contention (a filesystem without lock support):
//! losing the primary's durable queue would be a worse wedge than the incident, which the
//! startup isolation check (`persistence::isolation`) separately guards against.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How long an embedder waits for a live owner to let go before staying in-memory. Matches
/// the discovery guard's restart-overlap window (`control::discovery_guard`).
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(5);
const ACQUIRE_STEP: Duration = Duration::from_millis(100);

/// Who holds a `work.db` — written into the lock file by the owner right after acquiring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OwnerRecord {
    pub pid: u32,
    /// The owner's executable path (best-effort), e.g. `/usr/bin/hyperpanes`.
    pub exe: String,
    pub version: String,
    /// The owner's discovery file — tells an operator WHICH instance holds the DB.
    pub control_file: String,
    /// ms since the unix epoch.
    pub started_at: i64,
}

impl OwnerRecord {
    /// A record describing THIS process.
    pub fn current(version: &str, control_file: &Path, now_ms: i64) -> OwnerRecord {
        OwnerRecord {
            pid: std::process::id(),
            exe: std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            version: version.to_string(),
            control_file: control_file.display().to_string(),
            started_at: now_ms,
        }
    }
}

/// Outcome of [`acquire`].
#[derive(Debug)]
pub enum Acquired {
    /// This call took the lock; we are the owner from now until process exit.
    Owner,
    /// This process already owned it (in-process control-host restart).
    AlreadyOwner,
    /// The lock could not be taken for a reason other than contention (no lock support on
    /// this filesystem, permissions). The caller fails open; the error is for the log.
    Unavailable(io::Error),
}

/// Another live process owns the DB — the caller must NOT attach.
#[derive(Debug, Clone)]
pub struct Contended {
    pub db_path: PathBuf,
    pub lock_path: PathBuf,
    /// The owner's record, when readable (always on unix; not on Windows while locked).
    pub owner: Option<OwnerRecord>,
}

impl Contended {
    /// The actionable refusal line logged by the embedder.
    pub fn message(&self) -> String {
        let who = match &self.owner {
            Some(o) => format!(
                "another live hyperpanes instance owns it (pid {}, {}{}, control file {})",
                o.pid,
                if o.exe.is_empty() {
                    "exe unknown"
                } else {
                    &o.exe
                },
                if o.version.is_empty() {
                    String::new()
                } else {
                    format!(" v{}", o.version)
                },
                if o.control_file.is_empty() {
                    "unknown"
                } else {
                    &o.control_file
                },
            ),
            None => format!(
                "another live process holds its lock {}",
                self.lock_path.display()
            ),
        };
        format!(
            "NOT attaching the durable work queue {db}: {who}. This instance keeps an \
             in-memory queue and will never run boot recovery or the lease reaper against \
             that DB (doing so would requeue the owner's in-flight tasks: stale fencing \
             tokens, duplicate runs). To run an isolated dev/test instance, point {env} at a \
             scratch dir (together with XDG_STATE_HOME, XDG_CONFIG_HOME and \
             HYPERPANES_CONTROL_FILE); see docs/dev-instance-isolation.md.",
            db = self.db_path.display(),
            env = crate::persistence::isolation::DATA_DIR_ENV_HINT,
        )
    }
}

/// The lock file guarding `db`: `<db>.lock` (e.g. `work.db.lock`).
pub fn lock_path_for(db: &Path) -> PathBuf {
    let mut name = db
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "work.db".into());
    name.push(".lock");
    db.with_file_name(name)
}

/// Locks this process owns, keyed by lock path, held (open) until exit.
fn registry() -> &'static Mutex<HashMap<PathBuf, File>> {
    static HELD: OnceLock<Mutex<HashMap<PathBuf, File>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A stable registry key: the lock path with its (existing) parent canonicalized, so two
/// spellings of the same dir map to one entry.
fn registry_key(lock_path: &Path) -> PathBuf {
    match (lock_path.parent(), lock_path.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| lock_path.to_path_buf()),
        _ => lock_path.to_path_buf(),
    }
}

/// Become the single owner of `db`, retrying for up to `wait` while another live process
/// holds it. `me` is recorded in the lock file on success. See the module docs.
pub fn acquire(db: &Path, me: &OwnerRecord, wait: Duration) -> Result<Acquired, Box<Contended>> {
    let lock_path = lock_path_for(db);
    if let Some(dir) = lock_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let key = registry_key(&lock_path);
    // Hold the registry across the whole attempt: two threads of one process acquiring the
    // same lock must resolve to one owner, never block each other on the OS lock.
    let mut held = registry().lock().unwrap_or_else(|e| e.into_inner());
    if held.contains_key(&key) {
        return Ok(Acquired::AlreadyOwner);
    }
    // Never truncate on open: until we hold the lock, the contents are the OWNER's record.
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => return Ok(Acquired::Unavailable(e)),
    };
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(Box::new(Contended {
                        db_path: db.to_path_buf(),
                        owner: read_owner(&lock_path),
                        lock_path,
                    }));
                }
                std::thread::sleep(ACQUIRE_STEP);
            }
            Err(TryLockError::Error(e)) => return Ok(Acquired::Unavailable(e)),
        }
    }
    // Ours. Record who we are (best-effort — the lock, not the record, is what gates).
    if let Ok(json) = serde_json::to_vec_pretty(me) {
        let _ = file
            .set_len(0)
            .and_then(|_| file.seek(SeekFrom::Start(0)))
            .and_then(|_| file.write_all(&json))
            .and_then(|_| file.sync_data());
    }
    held.insert(key, file);
    Ok(Acquired::Owner)
}

/// Read the owner record from a lock file (None when absent, unreadable or corrupt).
pub fn read_owner(lock_path: &Path) -> Option<OwnerRecord> {
    let mut text = String::new();
    File::open(lock_path).ok()?.read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-work-lock-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn me() -> OwnerRecord {
        OwnerRecord::current("9.9.9", Path::new("/tmp/x/control.json"), 42)
    }

    #[test]
    fn lock_path_sits_beside_the_db() {
        assert_eq!(
            lock_path_for(Path::new("/d/hyperpanes/work.db")),
            PathBuf::from("/d/hyperpanes/work.db.lock")
        );
    }

    #[test]
    fn first_acquire_owns_and_records_the_owner() {
        let dir = scratch("owns");
        let db = dir.join("work.db");
        assert!(matches!(
            acquire(&db, &me(), Duration::ZERO),
            Ok(Acquired::Owner)
        ));
        // Readable while held on unix (flock is advisory).
        #[cfg(unix)]
        {
            let rec = read_owner(&lock_path_for(&db)).expect("owner record");
            assert_eq!(rec.pid, std::process::id());
            assert_eq!(rec.version, "9.9.9");
            assert_eq!(rec.control_file, "/tmp/x/control.json");
        }
    }

    #[test]
    fn reacquire_in_the_same_process_is_already_owner() {
        let dir = scratch("again");
        let db = dir.join("work.db");
        assert!(matches!(
            acquire(&db, &me(), Duration::ZERO),
            Ok(Acquired::Owner)
        ));
        assert!(matches!(
            acquire(&db, &me(), Duration::ZERO),
            Ok(Acquired::AlreadyOwner)
        ));
        // A different spelling of the same dir resolves to the same entry.
        let dotted = dir.join(".").join("work.db");
        assert!(matches!(
            acquire(&dotted, &me(), Duration::ZERO),
            Ok(Acquired::AlreadyOwner)
        ));
    }

    /// A lock held through a separate open file description — what another process holding
    /// it looks like to us — is contended, and the refusal names the recorded owner.
    #[test]
    fn foreign_holder_is_contended_after_the_wait() {
        let dir = scratch("foreign");
        let db = dir.join("work.db");
        let lock = lock_path_for(&db);
        let mut foreign = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .unwrap();
        foreign.try_lock().expect("foreign lock");
        let rec = OwnerRecord {
            pid: 4242,
            exe: "/usr/bin/hyperpanes".into(),
            version: "0.0.33".into(),
            control_file: "/home/u/.local/state/hyperpanes/control.json".into(),
            started_at: 1,
        };
        foreign
            .write_all(&serde_json::to_vec(&rec).unwrap())
            .unwrap();
        foreign.sync_data().unwrap();

        let t0 = Instant::now();
        let err = acquire(&db, &me(), Duration::from_millis(300)).expect_err("contended");
        assert!(
            t0.elapsed() >= Duration::from_millis(300),
            "waited the window"
        );
        #[cfg(unix)]
        {
            assert_eq!(err.owner.as_ref().map(|o| o.pid), Some(4242));
            let msg = err.message();
            assert!(msg.contains("pid 4242"), "{msg}");
            assert!(msg.contains("/usr/bin/hyperpanes"), "{msg}");
        }
        let msg = err.message();
        assert!(msg.contains("NOT attaching"), "{msg}");
        assert!(msg.contains("HYPERPANES_CONTROL_FILE"), "{msg}");
        // The contender must not have clobbered the owner's record.
        #[cfg(unix)]
        assert_eq!(read_owner(&lock).map(|o| o.pid), Some(4242));
        drop(foreign);
    }

    #[test]
    fn released_lock_is_taken_within_the_wait() {
        let dir = scratch("release");
        let db = dir.join("work.db");
        let foreign = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path_for(&db))
            .unwrap();
        foreign.try_lock().expect("foreign lock");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(foreign); // the outgoing owner exits
        });
        assert!(matches!(
            acquire(&db, &me(), Duration::from_secs(5)),
            Ok(Acquired::Owner)
        ));
        releaser.join().unwrap();
    }
}
