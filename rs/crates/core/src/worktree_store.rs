//! Disk-backed storage for `hyperpanes worker --worktree` task worktrees and their shared warm
//! build caches.
//!
//! The old layout put every task worktree in `std::env::temp_dir()`, which is RAM-backed tmpfs
//! with a per-user quota on many Linux hosts; a few cold Rust builds (~7.5G `target/` each)
//! exhausted it. This module owns the replacement, kept pure/testable (and CI fmt/clippy gated)
//! apart from the runner:
//!
//! ```text
//! <root>/<repo-key>/
//!   repo                  human-readable: canonical git common dir
//!   wt/<name>/            a task worktree
//!   wt/<name>.lock        its owner lock (flock held by the runner / the task's process tree)
//!   cache/slot-<k>/       build-cache slot k (reused warm across tasks)
//!   cache/slot-<k>.lock   its lock; mtime = last release (the "last used" stamp)
//! ```
//!
//! Liveness is decided by `flock`, never by pid: a lock that can be taken means its owner (and
//! every process that inherited the fd) is gone.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::persistence::paths::cache_dir;

/// Env var overriding the worktree root.
pub const ROOT_ENV: &str = "HYPERPANES_WORKTREE_ROOT";
/// Env var overriding the minimum free space required to start a worktree.
pub const MIN_FREE_ENV: &str = "HYPERPANES_WORKTREE_MIN_FREE";
/// Default free-space floor: 10 GiB.
pub const DEFAULT_MIN_FREE: u64 = 10 * 1024 * 1024 * 1024;
/// Env override for the cache-slot cap (`--max-cache-slots`); `0` = unlimited (the default).
pub const MAX_SLOTS_ENV: &str = "HYPERPANES_WORKTREE_MAX_SLOTS";

/// A lone `*.lock` (no dir) older than this is debris.
const LONE_LOCK_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// A cache slot unused for this long is reclaimed.
const SLOT_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

// ───────────────────────── root + repo key ─────────────────────────

/// `<cache_dir>/worktrees` — disk-backed on every platform.
pub fn default_root() -> PathBuf {
    cache_dir().join("worktrees")
}

/// Pure root resolution: `flag` > `env` (empty = unset) > [`default_root`]; a relative value is
/// made absolute against `cwd`.
pub fn resolve_root(flag: Option<&Path>, env: Option<OsString>, cwd: &Path) -> PathBuf {
    let chosen: Option<PathBuf> = flag
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| env.filter(|v| !v.is_empty()).map(PathBuf::from));
    match chosen {
        Some(p) if p.is_absolute() => p,
        Some(p) => cwd.join(p),
        None => default_root(),
    }
}

/// [`resolve_root`] reading the real `HYPERPANES_WORKTREE_ROOT`.
pub fn root_from_env(flag: Option<&Path>, cwd: &Path) -> PathBuf {
    resolve_root(flag, std::env::var_os(ROOT_ENV), cwd)
}

/// Hand-rolled FNV-1a-64 (stable across Rust versions, unlike `DefaultHasher`).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `<sanitized basename>-<8 hex of FNV-1a-64 of the canonical path>`. The basename is the
/// parent dir's name when `common_dir` ends in `.git`, else its own name. Every linked worktree
/// of one repo shares the same git common dir, hence the same key.
pub fn repo_key(common_dir: &Path) -> String {
    let canon = std::fs::canonicalize(common_dir).unwrap_or_else(|_| common_dir.to_path_buf());
    let name_of = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
    let base = if canon.file_name().is_some_and(|n| n == ".git") {
        canon.parent().and_then(name_of)
    } else {
        name_of(&canon)
    }
    .unwrap_or_default();
    let mut safe: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    safe = safe.trim_start_matches('.').to_string();
    if safe.is_empty() {
        safe = "repo".to_string();
    }
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        canon.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = canon.to_string_lossy().into_owned().into_bytes();
    format!("{safe}-{}", &format!("{:016x}", fnv1a64(&bytes))[..8])
}

// ───────────────────────── sizes + free space ─────────────────────────

/// Parse `10G`, `500MiB`, `1024`, `0`: bytes or K/M/G/T with optional B/iB (binary units),
/// case-insensitive.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(digits_end);
    if num.is_empty() {
        return Err(format!(
            "invalid size '{s}': expected a number with optional K/M/G/T suffix"
        ));
    }
    let n: u64 = num.parse().map_err(|_| format!("invalid size '{s}'"))?;
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        other => return Err(format!("invalid size '{s}': unknown unit '{other}'")),
    };
    n.checked_mul(1u64 << shift)
        .ok_or_else(|| format!("size '{s}' overflows"))
}

/// Human-readable binary size, e.g. `9.3 GiB`.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Free bytes available to this user on the filesystem holding `path` (walks up to the nearest
/// existing ancestor, so a not-yet-created root can be checked).
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    let mut p = path;
    while !p.exists() {
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => p = parent,
            _ => break,
        }
    }
    available_bytes_existing(p)
}

#[cfg(unix)]
fn available_bytes_existing(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c` is a valid NUL-terminated path; `st` is a valid out-pointer for statvfs.
    let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs returned 0, so the struct is initialised.
    let st = unsafe { st.assume_init() };
    // Field widths differ per platform (u32 on macOS), so the casts are needed there.
    #[allow(clippy::unnecessary_cast)]
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(windows)]
fn available_bytes_existing(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut avail: u64 = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; `avail` is a valid out-pointer.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut avail), None, None) }
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(avail)
}

// ───────────────────────── task owner lock ─────────────────────────

/// Owner lock for one task worktree. Holds an exclusive `flock` on `wt/<name>.lock` for as long
/// as it lives; dropping removes the lock file. The worker makes this fd inheritable by the
/// child so the lock outlives a runner that dies with a live task tree.
#[derive(Debug)]
pub struct TaskLock {
    name: String,
    lock_path: PathBuf,
    wt_path: PathBuf,
    file: File,
}

impl TaskLock {
    /// Claim a unique name (`base`, then `base-1`, `base-2`…) under `<key_dir>/wt/`, lock it and
    /// write human-readable info. The worktree dir itself is created by the caller afterwards.
    pub fn create(
        key_dir: &Path,
        base: &str,
        queue: &str,
        task: &str,
        branch: &str,
    ) -> io::Result<TaskLock> {
        let wt = key_dir.join("wt");
        std::fs::create_dir_all(&wt)?;
        let mut n = 0u32;
        loop {
            let name = if n == 0 {
                base.to_string()
            } else {
                format!("{base}-{n}")
            };
            n += 1;
            let lock_path = wt.join(format!("{name}.lock"));
            let wt_path = wt.join(&name);
            let mut file = match OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            };
            match file.try_lock() {
                Ok(()) => {}
                // A concurrent sweep briefly held it between create and lock: next name.
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(e)) => {
                    let _ = std::fs::remove_file(&lock_path);
                    return Err(e);
                }
            }
            let info = format!(
                "pid={}\nqueue={queue}\ntask={task}\nbranch={branch}\n",
                std::process::id()
            );
            let _ = file.write_all(info.as_bytes());
            let _ = file.flush();
            if wt_path.exists() {
                // Leftover dir with a recycled name and no lock: not ours to reuse.
                drop(file);
                let _ = std::fs::remove_file(&lock_path);
                continue;
            }
            return Ok(TaskLock {
                name,
                lock_path,
                wt_path,
                file,
            });
        }
    }

    /// The name chosen under `wt/`.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Where the worktree should be created (`wt/<name>`).
    pub fn worktree_path(&self) -> &Path {
        &self.wt_path
    }
    /// The lock file path.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
    /// The locked fd, so the worker can clear FD_CLOEXEC for this task's child only.
    #[cfg(unix)]
    pub fn raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.file.as_raw_fd()
    }
}

impl Drop for TaskLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
        // `file` closes after this, releasing the flock.
    }
}

// ───────────────────────── orphan sweep ─────────────────────────

/// One reclaimed orphan worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweptOrphan {
    pub path: PathBuf,
    pub branch: String,
    /// Lines of `git status --porcelain` at removal time (uncommitted work that was discarded).
    pub dirty_lines: usize,
}

/// What a [`sweep`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub swept: Vec<SweptOrphan>,
    pub kept_live: Vec<PathBuf>,
    pub gc_slots: Vec<PathBuf>,
    pub removed_locks: Vec<PathBuf>,
}

enum LockState {
    /// No lock file at all.
    Missing,
    /// Lock free: owner dead. Holds the lock while the caller works.
    Free(File),
    /// Someone holds it.
    Live,
    Unknown,
}

fn probe_lock(path: &Path) -> LockState {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => match f.try_lock() {
            Ok(()) => LockState::Free(f),
            Err(TryLockError::WouldBlock) => LockState::Live,
            Err(TryLockError::Error(_)) => LockState::Unknown,
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => LockState::Missing,
        Err(_) => LockState::Unknown,
    }
}

fn age(path: &Path) -> Option<Duration> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(m).ok()
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Reclaim orphans under one repo-key dir; see the module docs for the layout. `repo_dir` is any
/// working dir of the owning repo (git commands run there).
pub fn sweep(repo_dir: &Path, key_dir: &Path) -> SweepReport {
    sweep_with(repo_dir, key_dir, LONE_LOCK_MAX_AGE, SLOT_MAX_AGE)
}

fn sweep_with(
    repo_dir: &Path,
    key_dir: &Path,
    lone_lock_max_age: Duration,
    slot_max_age: Duration,
) -> SweepReport {
    let mut rep = SweepReport::default();
    let wt = key_dir.join("wt");
    if let Ok(rd) = std::fs::read_dir(&wt) {
        // Dirs first: the decision is made by directly opening each lock, never from the listing.
        let entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        for dir in entries.iter().filter(|p| p.is_dir()) {
            let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            // `<name>.cache` is a task's uncached build dir: owned by `<name>.lock`, plain delete.
            let uncached_owner = name.strip_suffix(UNCACHED_SUFFIX);
            let lock_path = wt.join(format!("{}.lock", uncached_owner.unwrap_or(&name)));
            let held = match probe_lock(&lock_path) {
                LockState::Live | LockState::Unknown => {
                    rep.kept_live.push(dir.clone());
                    continue;
                }
                LockState::Missing => None,
                LockState::Free(f) => Some(f),
            };
            if uncached_owner.is_some() {
                let _ = std::fs::remove_dir_all(dir);
                drop(held);
                continue;
            }
            let dirty_lines = git_out(dir, &["status", "--porcelain"])
                .map(|s| s.lines().count())
                .unwrap_or(0);
            let branch = git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
            let removed = Command::new("git")
                .arg("-C")
                .arg(repo_dir)
                .args(["worktree", "remove", "--force"])
                .arg(dir)
                .output()
                .is_ok_and(|o| o.status.success());
            if !removed || dir.exists() {
                let _ = std::fs::remove_dir_all(dir);
            }
            if held.is_some() {
                // Still holding the lock: removal can't race a new owner's open.
                let _ = std::fs::remove_file(&lock_path);
            }
            drop(held);
            rep.swept.push(SweptOrphan {
                path: dir.clone(),
                branch,
                dirty_lines,
            });
        }
        // Lone lock files (no dir) that are free and stale.
        for p in entries
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "lock") && p.is_file())
        {
            let dir = p.with_extension("");
            if dir.exists() {
                continue;
            }
            if age(p).is_some_and(|a| a > lone_lock_max_age) {
                if let LockState::Free(f) = probe_lock(p) {
                    let _ = std::fs::remove_file(p);
                    drop(f);
                    rep.removed_locks.push(p.clone());
                }
            }
        }
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["worktree", "prune"])
        .output();

    // Cache-slot GC. Slot lock files are never deleted (an inode race with acquirers).
    let cache = key_dir.join("cache");
    if let Ok(rd) = std::fs::read_dir(&cache) {
        for p in rd.filter_map(|e| e.ok().map(|e| e.path())) {
            let is_lock = p.extension().is_some_and(|e| e == "lock");
            let stem_ok = p
                .file_stem()
                .is_some_and(|s| s.to_string_lossy().starts_with("slot-"));
            if !is_lock || !stem_ok {
                continue;
            }
            let slot_dir = p.with_extension("");
            if !slot_dir.exists() || !age(&p).is_some_and(|a| a > slot_max_age) {
                continue;
            }
            if let LockState::Free(f) = probe_lock(&p) {
                if std::fs::remove_dir_all(&slot_dir).is_ok() {
                    rep.gc_slots.push(slot_dir);
                }
                drop(f);
            }
        }
    }
    rep
}

// ───────────────────────── cache slots ─────────────────────────

/// An exclusive build-cache slot: a dir reused warm by successive tasks, never shared by two
/// concurrent ones. Dropping stamps the lock file's mtime (last-used) then unlocks.
#[derive(Debug)]
pub struct CacheSlot {
    pub index: usize,
    pub path: PathBuf,
    lock: File,
}

/// Take the lowest free `cache/slot-<k>` under `key_dir` (no cap).
pub fn acquire_slot(key_dir: &Path) -> io::Result<CacheSlot> {
    acquire_slot_capped(key_dir, 0).map(|s| s.expect("an uncapped acquire always finds a slot"))
}

/// Take the lowest free `cache/slot-<k>` with `k < max_slots` (`0` = no cap). `Ok(None)` when
/// every slot under the cap is busy: the caller then runs the task with an uncached per-task dir
/// (a cold build) instead of blocking or sharing a slot.
pub fn acquire_slot_capped(key_dir: &Path, max_slots: usize) -> io::Result<Option<CacheSlot>> {
    let cache = key_dir.join("cache");
    std::fs::create_dir_all(&cache)?;
    let mut k = 0usize;
    loop {
        if max_slots > 0 && k >= max_slots {
            return Ok(None);
        }
        let lock_path = cache.join(format!("slot-{k}.lock"));
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        match f.try_lock() {
            Ok(()) => {
                let path = cache.join(format!("slot-{k}"));
                std::fs::create_dir_all(&path)?;
                return Ok(Some(CacheSlot {
                    index: k,
                    path,
                    lock: f,
                }));
            }
            Err(TryLockError::WouldBlock) => k += 1,
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

impl Drop for CacheSlot {
    fn drop(&mut self) {
        let _ = self.lock.set_modified(SystemTime::now());
        let _ = self.lock.unlock();
    }
}

/// Disk-pressure eviction: delete idle cache slots under `root` (every repo key, since they all
/// share the root's filesystem), least recently used first (lock-file mtime = last release),
/// until `enough()` says the space is back. A slot is idle when its lock can be taken. The lock
/// is held while the slot dir is deleted and its lock file is kept (deleting slot lock files
/// races acquirers). Returns the evicted slot dirs. Called by the disk guard before it gives up
/// on a claim, so peak-concurrency slots don't pin disk until the 14-day GC.
pub fn evict_idle_slots(root: &Path, mut enough: impl FnMut() -> bool) -> Vec<PathBuf> {
    let mut slots: Vec<(SystemTime, PathBuf)> = Vec::new();
    for key in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let cache = key.path().join("cache");
        for p in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
            let p = p.path();
            let is_slot_lock = p.extension().is_some_and(|e| e == "lock")
                && p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with("slot-"));
            if !is_slot_lock || !p.with_extension("").is_dir() {
                continue;
            }
            let mtime = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            slots.push((mtime, p));
        }
    }
    slots.sort();
    let mut evicted = Vec::new();
    for (_, lock_path) in slots {
        if enough() {
            break;
        }
        if let LockState::Free(f) = probe_lock(&lock_path) {
            let dir = lock_path.with_extension("");
            if std::fs::remove_dir_all(&dir).is_ok() {
                evicted.push(dir);
            }
            drop(f);
        }
    }
    evicted
}

/// Uncached per-task cache dir used when every slot under the cap is busy: `wt/<name>.cache`,
/// owned by the task's [`TaskLock`] (the sweep reclaims it with its worktree) and deleted when
/// the task ends.
pub fn uncached_dir(lock: &TaskLock) -> PathBuf {
    let mut name = lock.worktree_path().as_os_str().to_os_string();
    name.push(UNCACHED_SUFFIX);
    PathBuf::from(name)
}

/// Suffix of [`uncached_dir`] entries under `wt/`.
pub const UNCACHED_SUFFIX: &str = ".cache";

/// Record the canonical git common dir in `<key_dir>/repo` (for humans), creating `key_dir`.
pub fn write_repo_file(key_dir: &Path, common_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(key_dir)?;
    let canon = std::fs::canonicalize(common_dir).unwrap_or_else(|_| common_dir.to_path_buf());
    std::fs::write(key_dir.join("repo"), format!("{}\n", canon.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "hp-wtstore-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(repo: &Path, args: &[&str]) -> String {
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
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo(tag: &str) -> PathBuf {
        let r = scratch(tag);
        git(&r, &["init", "-b", "main"]);
        git(&r, &["commit", "--allow-empty", "-m", "A"]);
        r
    }

    #[test]
    fn resolve_root_precedence() {
        let cwd = Path::new("/work");
        let f = Path::new("/flag");
        assert_eq!(
            resolve_root(Some(f), Some("/env".into()), cwd),
            PathBuf::from("/flag")
        );
        assert_eq!(
            resolve_root(None, Some("/env".into()), cwd),
            PathBuf::from("/env")
        );
        // Empty env / empty flag count as unset.
        assert_eq!(resolve_root(None, Some("".into()), cwd), default_root());
        assert_eq!(
            resolve_root(Some(Path::new("")), Some("/env".into()), cwd),
            PathBuf::from("/env")
        );
        assert_eq!(resolve_root(None, None, cwd), default_root());
        // Relative values resolve against cwd.
        assert_eq!(
            resolve_root(Some(Path::new("rel/x")), None, cwd),
            PathBuf::from("/work/rel/x")
        );
        assert_eq!(
            resolve_root(None, Some("e".into()), cwd),
            PathBuf::from("/work/e")
        );
    }

    #[test]
    fn default_root_is_disk_backed_not_temp() {
        let r = default_root();
        assert!(r.ends_with("hyperpanes/worktrees"), "{r:?}");
        assert!(!r.starts_with(std::env::temp_dir()), "{r:?}");
        #[cfg(target_os = "linux")]
        assert!(!r.starts_with("/tmp"), "{r:?}");
    }

    #[test]
    fn repo_key_is_stable_sanitized_and_distinct() {
        let a = scratch("key a b");
        let dot_git = a.join(".git");
        std::fs::create_dir_all(&dot_git).unwrap();
        let k1 = repo_key(&dot_git);
        assert_eq!(k1, repo_key(&dot_git));
        // basename = parent dir (sanitized), then `-` and 8 hex.
        let (name, hex) = k1.rsplit_once('-').unwrap();
        assert!(name.starts_with("hp-wtstore-key_a_b-"), "{k1}");
        assert_eq!(hex.len(), 8);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(k1
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)));
        let b = scratch("key b");
        assert_ne!(k1, repo_key(&b.join(".git")));
        // Non-.git common dir (bare repo) uses its own name.
        let bare = b.join("proj.git2");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(repo_key(&bare).starts_with("proj.git2-"));
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn fnv_known_vector() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn slots_are_distinct_while_held_and_reused_after() {
        let kd = scratch("slots");
        let s0 = acquire_slot(&kd).unwrap();
        assert_eq!(s0.index, 0);
        // Concurrent acquirers (threads) get distinct slots.
        let kd2 = kd.clone();
        let got = std::thread::spawn(move || {
            let a = acquire_slot(&kd2).unwrap();
            let b = acquire_slot(&kd2).unwrap();
            (a.index, b.index)
        })
        .join()
        .unwrap();
        assert_eq!(got, (1, 2));
        assert!(s0.path.is_dir());
        drop(s0);
        let again = acquire_slot(&kd).unwrap();
        assert_eq!(again.index, 0, "slot 0 reused warm after release");
        // Lock files are never deleted.
        drop(again);
        assert!(kd.join("cache/slot-0.lock").exists());
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn slot_drop_stamps_mtime() {
        let kd = scratch("stamp");
        let s = acquire_slot(&kd).unwrap();
        let lock = kd.join("cache/slot-0.lock");
        let old = SystemTime::now() - Duration::from_secs(40 * 24 * 3600);
        s.lock.set_modified(old).unwrap();
        drop(s);
        assert!(age(&lock).unwrap() < Duration::from_secs(60));
        let _ = std::fs::remove_dir_all(&kd);
    }

    fn add_wt(repo: &Path, kd: &Path, name: &str) -> PathBuf {
        let p = kd.join("wt").join(name);
        std::fs::create_dir_all(kd.join("wt")).unwrap();
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                &format!("b/{name}"),
                p.to_str().unwrap(),
            ],
        );
        p
    }

    #[test]
    fn sweep_removes_unlocked_orphan_keeps_live() {
        let r = repo("sweep");
        let kd = scratch("sweepkd");
        // Orphan with a free lock file + uncommitted work.
        let orphan = add_wt(&r, &kd, "orphan");
        std::fs::write(orphan.join("dirty.txt"), "x").unwrap();
        std::fs::write(kd.join("wt/orphan.lock"), "").unwrap();
        // Live: lock held by "us".
        let tl = TaskLock::create(&kd, "live", "q", "t1", "b/live").unwrap();
        git(
            &r,
            &[
                "worktree",
                "add",
                "-b",
                "b/live",
                tl.worktree_path().to_str().unwrap(),
            ],
        );
        // No lock file at all.
        let nolock = add_wt(&r, &kd, "nolock");

        let rep = sweep(&r, &kd);
        assert!(!orphan.exists());
        assert!(!nolock.exists());
        assert!(!kd.join("wt/orphan.lock").exists());
        assert!(tl.worktree_path().exists(), "live worktree kept");
        assert_eq!(rep.kept_live, vec![tl.worktree_path().to_path_buf()]);
        assert_eq!(rep.swept.len(), 2);
        let o = rep.swept.iter().find(|s| s.path == orphan).unwrap();
        assert_eq!(o.dirty_lines, 1);
        assert_eq!(o.branch, "b/orphan");
        let list = git(&r, &["worktree", "list"]);
        assert!(!list.contains("orphan"), "{list}");
        // The branch survives the sweep.
        git(&r, &["rev-parse", "--verify", "b/orphan"]);
        let live = tl.worktree_path().to_path_buf();
        drop(tl);
        assert!(!live.with_extension("lock").exists());
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn task_lock_names_are_unique() {
        let kd = scratch("names");
        let a = TaskLock::create(&kd, "n", "q", "t", "b").unwrap();
        let b = TaskLock::create(&kd, "n", "q", "t", "b").unwrap();
        assert_eq!(a.name(), "n");
        assert_eq!(b.name(), "n-1");
        let info = std::fs::read_to_string(a.lock_path()).unwrap();
        assert!(
            info.contains("queue=q") && info.contains("branch=b"),
            "{info}"
        );
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn sweep_gcs_stale_lone_locks_and_old_slots() {
        let r = repo("gc");
        let kd = scratch("gckd");
        std::fs::create_dir_all(kd.join("wt")).unwrap();
        let lone = kd.join("wt/lone.lock");
        std::fs::write(&lone, "").unwrap();
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        File::options()
            .write(true)
            .open(&lone)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let fresh = kd.join("wt/fresh.lock");
        std::fs::write(&fresh, "").unwrap();

        // Slots: 0 old+free (gc), 1 old+held (keep), 2 recent+free (keep).
        let ancient = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        let s0 = acquire_slot(&kd).unwrap();
        let s1 = acquire_slot(&kd).unwrap();
        let s2 = acquire_slot(&kd).unwrap();
        let (p0, p1, p2) = (s0.path.clone(), s1.path.clone(), s2.path.clone());
        drop(s0);
        drop(s2);
        for k in [0, 1] {
            File::options()
                .write(true)
                .open(kd.join(format!("cache/slot-{k}.lock")))
                .unwrap()
                .set_modified(ancient)
                .unwrap();
        }
        let rep = sweep(&r, &kd);
        assert!(!lone.exists());
        assert!(fresh.exists());
        assert_eq!(rep.gc_slots, vec![p0.clone()]);
        assert!(!p0.exists());
        assert!(p1.exists() && p2.exists());
        assert!(
            kd.join("cache/slot-0.lock").exists(),
            "slot locks never deleted"
        );
        drop(s1);
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn parse_size_table() {
        for (s, v) in [
            ("0", 0u64),
            ("1024", 1024),
            ("1b", 1),
            ("2K", 2048),
            ("2kb", 2048),
            ("2KiB", 2048),
            ("500MiB", 500 << 20),
            ("10G", 10 << 30),
            ("10g", 10 << 30),
            ("1T", 1 << 40),
            (" 3 GB ", 3 << 30),
        ] {
            assert_eq!(parse_size(s), Ok(v), "{s}");
        }
        for bad in ["", "G", "-1", "1.5G", "10X", "abc", "99999999999999T"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
        assert_eq!(DEFAULT_MIN_FREE, 10 << 30);
    }

    #[test]
    fn fmt_bytes_reads_well() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(10 << 30), "10.0 GiB");
        assert_eq!(fmt_bytes((9.3 * (1u64 << 30) as f64) as u64), "9.3 GiB");
    }

    #[test]
    fn available_bytes_is_positive() {
        assert!(available_bytes(&std::env::temp_dir()).unwrap() > 0);
        // A not-yet-created path resolves via its nearest existing ancestor.
        let p = std::env::temp_dir().join("hp-wtstore-nope/deeper");
        assert!(available_bytes(&p).unwrap() > 0);
    }

    #[test]
    fn capped_slots_return_none_when_all_busy() {
        let kd = scratch("cap");
        let a = acquire_slot_capped(&kd, 2).unwrap().expect("slot 0");
        let b = acquire_slot_capped(&kd, 2).unwrap().expect("slot 1");
        assert_eq!((a.index, b.index), (0, 1));
        assert!(
            acquire_slot_capped(&kd, 2).unwrap().is_none(),
            "cap reached: no third slot"
        );
        assert!(
            !kd.join("cache/slot-2.lock").exists(),
            "nothing created past the cap"
        );
        drop(a);
        assert_eq!(acquire_slot_capped(&kd, 2).unwrap().unwrap().index, 0);
        // 0 = unlimited.
        assert_eq!(acquire_slot_capped(&kd, 0).unwrap().unwrap().index, 0);
        drop(b);
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn eviction_is_lru_first_skips_busy_slots_and_stops_when_enough() {
        let root = scratch("evict");
        let (ka, kb) = (root.join("a-00000000"), root.join("b-11111111"));
        // Three idle slots across two repo keys with distinct ages, plus one busy (oldest).
        let mk = |kd: &Path, age_h: u64| {
            let s = acquire_slot(kd).unwrap();
            std::fs::write(s.path.join("artifact"), b"x").unwrap();
            let lock = kd.join(format!("cache/slot-{}.lock", s.index));
            (s, lock, age_h)
        };
        let busy = mk(&ka, 100);
        let a1 = mk(&ka, 50);
        let b0 = mk(&kb, 70);
        let b1 = mk(&kb, 10);
        let (busy_slot, a1_slot, b0_slot, b1_slot) = (busy.0, a1.0, b0.0, b1.0);
        let paths = [
            busy_slot.path.clone(),
            a1_slot.path.clone(),
            b0_slot.path.clone(),
            b1_slot.path.clone(),
        ];
        drop(a1_slot);
        drop(b0_slot);
        drop(b1_slot);
        for (lock, h) in [
            (&busy.1, busy.2),
            (&a1.1, a1.2),
            (&b0.1, b0.2),
            (&b1.1, b1.2),
        ] {
            File::options()
                .write(true)
                .open(lock)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(h * 3600))
                .unwrap();
        }
        // Enough after one eviction: only the least recently used IDLE slot goes (b0, 70h),
        // never the busy one even though it is older.
        let b0_dir = paths[2].clone();
        let ev = evict_idle_slots(&root, || !b0_dir.exists());
        assert_eq!(ev, vec![paths[2].clone()]);
        assert!(paths[0].exists() && paths[1].exists() && paths[3].exists());
        // Never enough: every idle slot goes, oldest first; the busy one and all locks stay.
        let ev = evict_idle_slots(&root, || false);
        assert_eq!(ev, vec![paths[1].clone(), paths[3].clone()]);
        assert!(paths[0].join("artifact").exists(), "busy slot untouched");
        assert!(ka.join("cache/slot-1.lock").exists() && kb.join("cache/slot-0.lock").exists());
        // Evicted slots are reacquirable (recreated empty).
        let again = acquire_slot(&kb).unwrap();
        assert!(again.path.is_dir() && !again.path.join("artifact").exists());
        drop(again);
        drop(busy_slot);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_reclaims_orphan_uncached_dirs_and_keeps_live_ones() {
        let r = repo("unc");
        let kd = scratch("unckd");
        // Live: lock held by us, uncached dir present.
        let live = TaskLock::create(&kd, "q-live0000-1", "q", "t", "b").unwrap();
        let live_unc = uncached_dir(&live);
        std::fs::create_dir_all(&live_unc).unwrap();
        assert!(live_unc.to_string_lossy().ends_with("q-live0000-1.cache"));
        // Orphan: uncached dir with no lock file at all (owner gone).
        let dead_unc = kd.join("wt/q-dead0000-2.cache");
        std::fs::create_dir_all(dead_unc.join("cargo-target")).unwrap();
        let rep = sweep(&r, &kd);
        assert!(live_unc.exists(), "live task's uncached dir kept");
        assert!(!dead_unc.exists(), "orphan uncached dir reclaimed");
        assert!(
            rep.swept.is_empty(),
            "uncached dirs are not reported as worktrees"
        );
        drop(live);
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&kd);
    }
}
