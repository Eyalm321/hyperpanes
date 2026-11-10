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
//!   cache/slot-<k>.size   last measured on-disk bytes (fresh iff mtime >= the lock's)
//!   evidence/<name>.patch uncommitted work saved from a task worktree before removal (#86)
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

/// Env var for the per-slot size cap (`--slot-max-size`); `0` = off.
pub const SLOT_MAX_ENV: &str = "HYPERPANES_WORKTREE_SLOT_MAX";
/// Default per-slot cap: 20 GiB.
pub const DEFAULT_SLOT_MAX: u64 = 20 * 1024 * 1024 * 1024;
/// Env var for the root-wide cache budget (`--cache-max-size`); `0` = off.
pub const CACHE_MAX_ENV: &str = "HYPERPANES_WORKTREE_CACHE_MAX";
/// Default root-wide cache budget: 100 GiB.
pub const DEFAULT_CACHE_MAX: u64 = 100 * 1024 * 1024 * 1024;
/// Env var for the proactive free-space floor (`--soft-free`); `0` = off.
pub const SOFT_FREE_ENV: &str = "HYPERPANES_WORKTREE_SOFT_FREE";
/// Upper bound of the default soft free-space floor: 40 GiB. The effective default is
/// [`default_soft_free`]: the smaller of this and a tenth of the filesystem's total size, so a
/// small disk (a laptop SSD, a CI runner) doesn't evict every idle warm slot on every claim.
pub const DEFAULT_SOFT_FREE: u64 = 40 * 1024 * 1024 * 1024;
/// The default soft floor is at most `1 / SOFT_FREE_FS_DIVISOR` of the filesystem's total size.
pub const SOFT_FREE_FS_DIVISOR: u64 = 10;

/// A lone `*.lock` (no dir) older than this is debris.
const LONE_LOCK_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// How long a saved evidence patch (`evidence/*.patch`) is kept before the startup sweep deletes
/// it: the same 14 days as the cache-slot age GC.
pub const EVIDENCE_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);
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
    fs_space_existing(p).map(|(avail, _)| avail)
}

/// Total size in bytes of the filesystem holding `path` (walks up like [`available_bytes`]).
pub fn total_bytes(path: &Path) -> io::Result<u64> {
    let mut p = path;
    while !p.exists() {
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => p = parent,
            _ => break,
        }
    }
    fs_space_existing(p).map(|(_, total)| total)
}

/// Pure: the default soft free-space floor for a filesystem of `fs_total` bytes: the smaller of
/// [`DEFAULT_SOFT_FREE`] and a tenth of the disk. Unknown/zero size falls back to the cap.
pub fn default_soft_free(fs_total: Option<u64>) -> u64 {
    match fs_total {
        Some(t) if t > 0 => DEFAULT_SOFT_FREE.min(t / SOFT_FREE_FS_DIVISOR),
        _ => DEFAULT_SOFT_FREE,
    }
}

/// `(available to this user, total)` bytes of the filesystem holding the existing `path`.
#[cfg(unix)]
fn fs_space_existing(path: &Path) -> io::Result<(u64, u64)> {
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
    let frsize = st.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    let (bavail, blocks) = (st.f_bavail as u64, st.f_blocks as u64);
    Ok((bavail.saturating_mul(frsize), blocks.saturating_mul(frsize)))
}

#[cfg(windows)]
fn fs_space_existing(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut avail: u64 = 0;
    let mut total: u64 = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; `avail`/`total` are valid
    // out-pointers.
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut avail),
            Some(&mut total),
            None,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))?;
    Ok((avail, total))
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
    /// Held for its lock (RAII); only read on unix, for `raw_fd`.
    #[cfg_attr(not(unix), allow(dead_code))]
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
    /// Lines of `git status --porcelain` at removal time (uncommitted work in the orphan).
    pub dirty_lines: usize,
    /// Where that uncommitted work was saved before removal (`None` = clean, or the save failed).
    pub patch: Option<PathBuf>,
}

/// What a [`sweep`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub swept: Vec<SweptOrphan>,
    pub kept_live: Vec<PathBuf>,
    pub gc_slots: Vec<PathBuf>,
    pub removed_locks: Vec<PathBuf>,
    /// Evidence patches older than [`EVIDENCE_MAX_AGE`] that were deleted.
    pub gc_evidence: Vec<PathBuf>,
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
    sweep_with(
        repo_dir,
        key_dir,
        LONE_LOCK_MAX_AGE,
        SLOT_MAX_AGE,
        EVIDENCE_MAX_AGE,
    )
}

fn sweep_with(
    repo_dir: &Path,
    key_dir: &Path,
    lone_lock_max_age: Duration,
    slot_max_age: Duration,
    evidence_max_age: Duration,
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
            // Save the orphan's uncommitted work before it is removed (#86): no silent loss.
            let patch = if dirty_lines > 0 {
                save_uncommitted(dir, &evidence_path(key_dir, &name))
                    .ok()
                    .flatten()
            } else {
                None
            };
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
                patch,
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
                    remove_sidecar(&slot_dir);
                    rep.gc_slots.push(slot_dir);
                }
                drop(f);
            }
        }
    }

    // Evidence GC: saved patches are kept EVIDENCE_MAX_AGE, then deleted.
    if let Ok(rd) = std::fs::read_dir(key_dir.join(EVIDENCE_DIR)) {
        for p in rd.filter_map(|e| e.ok().map(|e| e.path())) {
            if p.extension().is_some_and(|e| e == "patch")
                && p.is_file()
                && age(&p).is_some_and(|a| a > evidence_max_age)
                && std::fs::remove_file(&p).is_ok()
            {
                rep.gc_evidence.push(p);
            }
        }
    }
    rep
}

// ───────────────────────── task result evidence (#86) ─────────────────────────

/// Sub-dir of a repo-key dir holding saved uncommitted-work patches.
pub const EVIDENCE_DIR: &str = "evidence";

/// `<key_dir>/evidence/<name>.patch`, where `<name>` is the task worktree's dir name
/// (`<queue>-<id8>-<pid>`), so a patch maps back to its task and run.
pub fn evidence_path(key_dir: &Path, name: &str) -> PathBuf {
    key_dir.join(EVIDENCE_DIR).join(format!("{name}.patch"))
}

/// Commits on `branch` beyond `base_sha` (`git rev-list --count <base_sha>..<branch>`), run in
/// `dir` (any worktree of the repo). `base_sha` must be the exact sha the task was forked from,
/// never a moving ref, or a base that advanced mid-task would hide (or invent) commits.
pub fn commits_beyond(dir: &Path, base_sha: &str, branch: &str) -> Result<u64, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-list", "--count"])
        .arg(format!("{base_sha}..refs/heads/{branch}"))
        .output()
        .map_err(|e| format!("git rev-list: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git rev-list {base_sha}..{branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim()
        .parse()
        .map_err(|_| format!("git rev-list: unexpected output '{}'", s.trim()))
}

/// Save a worktree's uncommitted work (tracked changes AND untracked, non-ignored files) to
/// `dest` as a binary-safe patch against its HEAD, applicable with `git apply`. `Ok(None)` when
/// the tree is clean (nothing written). Stages everything into the worktree's own index to do
/// it (never a commit: the branch and HEAD are untouched), so call it only on a worktree that is
/// about to be removed. A patch can hold anything the agent left lying around (a stray, un-ignored
/// `.env`), so on unix the evidence dir is created `0700` and the patch `0600`, and callers log
/// only its path, never its contents.
pub fn save_uncommitted(worktree: &Path, dest: &Path) -> Result<Option<PathBuf>, String> {
    let status = git_out(worktree, &["status", "--porcelain"])
        .ok_or_else(|| format!("git status failed in {}", worktree.display()))?;
    if status.is_empty() {
        return Ok(None);
    }
    let added = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["add", "-A"])
        .output()
        .map_err(|e| format!("git add -A: {e}"))?;
    if !added.status.success() {
        return Err(format!(
            "git add -A: {}",
            String::from_utf8_lossy(&added.stderr).trim()
        ));
    }
    let diff = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--cached", "--binary", "HEAD"])
        .output()
        .map_err(|e| format!("git diff: {e}"))?;
    if !diff.status.success() {
        return Err(format!(
            "git diff --cached: {}",
            String::from_utf8_lossy(&diff.stderr).trim()
        ));
    }
    if diff.stdout.is_empty() {
        return Ok(None);
    }
    if let Some(parent) = dest.parent() {
        create_private_dir(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    write_private(dest, &diff.stdout)
        .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
    Ok(Some(dest.to_path_buf()))
}

/// `create_dir_all`, with the leaf forced to owner-only (`0700`) on unix even if it pre-existed.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Write `bytes` to `path`, owner-only (`0600`) on unix.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut o = OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        o.mode(0o600);
        let mut f = o.open(path)?;
        // `mode` only applies on create; a pre-existing file is tightened too.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        o.open(path)?.write_all(bytes)
    }
}

// ───────────────────────── cache slots ─────────────────────────

/// An exclusive build-cache slot: a dir reused warm by successive tasks, never shared by two
/// concurrent ones. Dropping stamps the lock file's mtime (last-used) then unlocks.
#[derive(Debug)]
pub struct CacheSlot {
    pub index: usize,
    pub path: PathBuf,
    lock: File,
    /// Set by [`CacheSlot::release_with_size`], which already stamped the lock mtime *before*
    /// writing the size sidecar; `Drop` must not re-stamp it (that would make the sidecar look
    /// stale).
    stamped: bool,
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
                    stamped: false,
                }));
            }
            Err(TryLockError::WouldBlock) => k += 1,
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

impl Drop for CacheSlot {
    fn drop(&mut self) {
        if !self.stamped {
            let _ = self.lock.set_modified(SystemTime::now());
        }
        let _ = self.lock.unlock();
    }
}

impl CacheSlot {
    /// Ordered release: stamp the lock mtime (last used) FIRST, write the `size` sidecar AFTER,
    /// then unlock. A sidecar whose mtime is >= the lock's mtime was therefore measured after the
    /// last release, i.e. it is fresh (see [`read_fresh_sidecar`]).
    pub fn release_with_size(mut self, size: u64) {
        let _ = self.lock.set_modified(SystemTime::now());
        write_sidecar(&self.path, size);
        self.stamped = true;
        // Drop unlocks.
    }

    /// Release path used by the runner after the task's child exits: measure the slot, enforce the
    /// per-slot cap `cap` (0 = off) while the flock is still held, then [`Self::release_with_size`].
    pub fn release_enforcing(self, cap: u64, log: &dyn Fn(String)) {
        self.release_enforcing_with(cap, &slot_usage, log)
    }

    fn release_enforcing_with(self, cap: u64, walk: &dyn Fn(&Path) -> u64, log: &dyn Fn(String)) {
        let size = measure_slot(&self.path, walk, log);
        let out = apply_slot_cap(&self.path, size, cap, false);
        log_outcome(&self.path, &out, cap, log);
        let after = out.after;
        self.release_with_size(after);
    }

    /// Acquire-time backstop for slots left over by older runners: use the fresh sidecar when
    /// there is one (no walk), else measure; then enforce the cap. Call with the slot just taken.
    pub fn enforce_on_acquire(&self, cap: u64, log: &dyn Fn(String)) {
        self.enforce_on_acquire_with(cap, &slot_usage, log)
    }

    fn enforce_on_acquire_with(&self, cap: u64, walk: &dyn Fn(&Path) -> u64, log: &dyn Fn(String)) {
        if cap == 0 {
            return;
        }
        let size = slot_size(&self.path, walk, log);
        let out = apply_slot_cap(&self.path, size, cap, false);
        if out.action != SlotPrune::Keep {
            log_outcome(&self.path, &out, cap, log);
            write_sidecar(&self.path, out.after);
        }
    }
}

// ───────────────────────── slot size cap ─────────────────────────

/// What to do with an over-cap slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotPrune {
    /// Within the cap (or the cap is off).
    Keep,
    /// Delete the `incremental/` dirs; that alone gets the slot under the cap.
    Incremental,
    /// Even without `incremental/` the slot is over: delete the slot's contents.
    Wipe,
}

/// Pure decision: `size` is the slot's on-disk bytes, `incremental_bytes` how much of it lives in
/// `incremental/` dirs, `cap` the per-slot cap (`0` = off).
pub fn plan_slot_prune(size: u64, incremental_bytes: u64, cap: u64) -> SlotPrune {
    if cap == 0 || size <= cap {
        SlotPrune::Keep
    } else if size.saturating_sub(incremental_bytes) <= cap {
        SlotPrune::Incremental
    } else {
        SlotPrune::Wipe
    }
}

/// Result of applying [`plan_slot_prune`] to one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotPruneOutcome {
    pub before: u64,
    pub after: u64,
    pub action: SlotPrune,
    pub incremental_bytes: u64,
}

/// On-disk usage of `path` (like `du`): allocated blocks on unix, apparent length elsewhere.
/// Uses `symlink_metadata` and never follows symlinks.
pub fn slot_usage(path: &Path) -> u64 {
    fn usage(md: &std::fs::Metadata) -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            md.blocks().saturating_mul(512)
        }
        #[cfg(not(unix))]
        {
            md.len()
        }
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        total = total.saturating_add(usage(&md));
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    total
}

/// Every dir named `incremental` under `<slot>/cargo-target`, at the depths cargo uses
/// (`<profile>/incremental`, `<triple>/<profile>/incremental`). Symlinks are never followed.
pub fn incremental_dirs(slot: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let is_dir = std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir());
            if !is_dir {
                continue;
            }
            if e.file_name() == "incremental" {
                out.push(p);
            } else if depth < 3 {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&slot.join("cargo-target"), 1, &mut out);
    out
}

/// `cache/slot-<k>.size`, next to the slot's dir and lock.
fn sidecar_path(slot_dir: &Path) -> PathBuf {
    let mut n = slot_dir.as_os_str().to_os_string();
    n.push(".size");
    PathBuf::from(n)
}

fn slot_lock_path(slot_dir: &Path) -> PathBuf {
    let mut n = slot_dir.as_os_str().to_os_string();
    n.push(".lock");
    PathBuf::from(n)
}

fn mtime_of(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// Best-effort sidecar write; call only while holding the slot's flock.
fn write_sidecar(slot_dir: &Path, size: u64) {
    let Ok(mut f) = File::create(sidecar_path(slot_dir)) else {
        return;
    };
    let _ = f.write_all(size.to_string().as_bytes());
    // Stamp with the same clock `set_modified` used for the lock: the kernel's coarse file
    // clock can lag it, which would make a just-written sidecar look older than the release.
    let _ = f.set_modified(SystemTime::now());
}

fn remove_sidecar(slot_dir: &Path) {
    let _ = std::fs::remove_file(sidecar_path(slot_dir));
}

fn read_sidecar(slot_dir: &Path) -> Option<u64> {
    std::fs::read_to_string(sidecar_path(slot_dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The sidecar value, only when it was written at or after the last release (sidecar mtime >=
/// lock mtime): the release path stamps the lock first and writes the sidecar second.
pub fn read_fresh_sidecar(slot_dir: &Path) -> Option<u64> {
    let side = mtime_of(&sidecar_path(slot_dir))?;
    let lock = mtime_of(&slot_lock_path(slot_dir))?;
    (side >= lock).then(|| read_sidecar(slot_dir))?
}

fn slot_label(slot_dir: &Path) -> String {
    slot_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn measure_slot(slot_dir: &Path, walk: &dyn Fn(&Path) -> u64, log: &dyn Fn(String)) -> u64 {
    let t = std::time::Instant::now();
    let b = walk(slot_dir);
    log(format!(
        "{}: measured {} in {:.1}s",
        slot_label(slot_dir),
        fmt_bytes(b),
        t.elapsed().as_secs_f64()
    ));
    b
}

/// Slot size: the fresh sidecar if there is one, else a (logged) walk whose result is stored.
/// Caller must hold the slot's flock.
fn slot_size(slot_dir: &Path, walk: &dyn Fn(&Path) -> u64, log: &dyn Fn(String)) -> u64 {
    if let Some(v) = read_fresh_sidecar(slot_dir) {
        return v;
    }
    let b = measure_slot(slot_dir, walk, log);
    write_sidecar(slot_dir, b);
    b
}

/// Delete everything inside `dir`, keeping `dir` itself.
fn wipe_contents(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            let _ = std::fs::remove_dir_all(&p);
        } else {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Apply the per-slot cap to `slot_dir` (`size` = its current usage). Caller must hold the slot's
/// flock. Slot lock files live outside `slot_dir`, so they are never touched. `dry_run` only plans.
fn apply_slot_cap(slot_dir: &Path, size: u64, cap: u64, dry_run: bool) -> SlotPruneOutcome {
    let mut out = SlotPruneOutcome {
        before: size,
        after: size,
        action: SlotPrune::Keep,
        incremental_bytes: 0,
    };
    if cap == 0 || size <= cap {
        return out;
    }
    let dirs = incremental_dirs(slot_dir);
    out.incremental_bytes = dirs.iter().map(|d| slot_usage(d)).sum();
    out.action = plan_slot_prune(size, out.incremental_bytes, cap);
    match out.action {
        SlotPrune::Keep => {}
        SlotPrune::Incremental => {
            if !dry_run {
                for d in &dirs {
                    let _ = std::fs::remove_dir_all(d);
                }
            }
            out.after = size.saturating_sub(out.incremental_bytes);
        }
        SlotPrune::Wipe => {
            if !dry_run {
                wipe_contents(slot_dir);
            }
            out.after = 0;
        }
    }
    out
}

fn log_outcome(slot_dir: &Path, out: &SlotPruneOutcome, cap: u64, log: &dyn Fn(String)) {
    let label = slot_label(slot_dir);
    match out.action {
        SlotPrune::Keep => {}
        SlotPrune::Incremental => log(format!(
            "{label}: pruned incremental {} ({} > cap {})",
            fmt_bytes(out.incremental_bytes),
            fmt_bytes(out.before),
            fmt_bytes(cap)
        )),
        SlotPrune::Wipe => log(format!(
            "{label}: wiped slot contents {} ({} > cap {})",
            fmt_bytes(out.before),
            fmt_bytes(out.before),
            fmt_bytes(cap)
        )),
    }
}

/// Resolve a size setting: flag > env (empty = unset) > default. `0` means off.
pub fn resolve_size_setting(
    flag: Option<u64>,
    env: Option<String>,
    env_name: &str,
    default: u64,
) -> Result<u64, String> {
    match (flag, env) {
        (Some(v), _) => Ok(v),
        (None, Some(v)) if !v.trim().is_empty() => {
            parse_size(&v).map_err(|e| format!("{env_name}: {e}"))
        }
        _ => Ok(default),
    }
}

// ───────────────────────── root-wide budget + soft floor ─────────────────────────

/// The three size knobs; `0` disables each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    pub slot_max: u64,
    pub cache_max: u64,
    pub soft_free: u64,
}

impl CachePolicy {
    /// Resolve each knob from its flag, then its real env var, then the default. The soft floor's
    /// default depends on the size of the filesystem holding `root` ([`default_soft_free`]).
    pub fn resolve(
        slot_max: Option<u64>,
        cache_max: Option<u64>,
        soft_free: Option<u64>,
        root: &Path,
    ) -> Result<Self, String> {
        let env = |n: &str| std::env::var(n).ok();
        Ok(Self {
            slot_max: resolve_size_setting(
                slot_max,
                env(SLOT_MAX_ENV),
                SLOT_MAX_ENV,
                DEFAULT_SLOT_MAX,
            )?,
            cache_max: resolve_size_setting(
                cache_max,
                env(CACHE_MAX_ENV),
                CACHE_MAX_ENV,
                DEFAULT_CACHE_MAX,
            )?,
            soft_free: resolve_size_setting(
                soft_free,
                env(SOFT_FREE_ENV),
                SOFT_FREE_ENV,
                default_soft_free(total_bytes(root).ok()),
            )?,
        })
    }
}

/// One slot as seen by the root-wide planner.
#[derive(Debug, Clone)]
pub struct SlotInfo {
    /// `<repo-key>/slot-<k>`, for messages.
    pub label: String,
    pub dir: PathBuf,
    pub size: u64,
    pub last_used: SystemTime,
    /// Locked by a running task: counted in the total but never evicted.
    pub busy: bool,
}

/// Why a slot is evicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictReason {
    Budget { total: u64, budget: u64 },
    Floor { free: u64, floor: u64 },
}

/// Pure planner: which idle slots to evict, least recently used first, until both the root-wide
/// `budget` (0 = off; total = sum of ALL slot sizes) and the free-space `floor` (0 = off; `free`
/// is the current free bytes) would be satisfied. Busy and empty slots are never picked.
pub fn plan_evictions(
    slots: &[SlotInfo],
    budget: u64,
    free: Option<u64>,
    floor: u64,
) -> Vec<(usize, EvictReason)> {
    let total: u64 = slots.iter().map(|s| s.size).sum();
    let need_budget = if budget > 0 && total > budget {
        total - budget
    } else {
        0
    };
    let (need_floor, free_now) = match free {
        Some(f) if floor > 0 && f < floor => (floor - f, f),
        _ => (0, 0),
    };
    let need = need_budget.max(need_floor);
    if need == 0 {
        return Vec::new();
    }
    let mut idle: Vec<usize> = (0..slots.len())
        .filter(|&i| !slots[i].busy && slots[i].size > 0)
        .collect();
    idle.sort_by_key(|&i| (slots[i].last_used, i));
    let mut plan = Vec::new();
    let mut freed = 0u64;
    for i in idle {
        if freed >= need {
            break;
        }
        let reason = if freed < need_budget {
            EvictReason::Budget { total, budget }
        } else {
            EvictReason::Floor {
                free: free_now,
                floor,
            }
        };
        plan.push((i, reason));
        freed += slots[i].size;
    }
    plan
}

struct SlotRef {
    label: String,
    dir: PathBuf,
    lock_path: PathBuf,
}

/// Every `<root>/<key>/cache/slot-<k>` that has both a lock file and a dir, sorted by label.
fn list_slots(root: &Path) -> Vec<SlotRef> {
    let mut v = Vec::new();
    for key in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let key_name = key.file_name().to_string_lossy().into_owned();
        for p in std::fs::read_dir(key.path().join("cache"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = p.path();
            let is_slot_lock = p.extension().is_some_and(|e| e == "lock")
                && p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with("slot-"));
            let dir = p.with_extension("");
            if is_slot_lock && dir.is_dir() {
                v.push(SlotRef {
                    label: format!("{key_name}/{}", slot_label(&dir)),
                    dir,
                    lock_path: p,
                });
            }
        }
    }
    v.sort_by(|a, b| a.label.cmp(&b.label));
    v
}

fn collect_infos(
    root: &Path,
    overrides: Option<&std::collections::HashMap<PathBuf, u64>>,
    walk: &dyn Fn(&Path) -> u64,
    log: &dyn Fn(String),
) -> Vec<SlotInfo> {
    let mut v = Vec::new();
    for s in list_slots(root) {
        let last_used = mtime_of(&s.lock_path).unwrap_or(SystemTime::UNIX_EPOCH);
        let (mut size, busy) = match probe_lock(&s.lock_path) {
            LockState::Free(f) => {
                let sz = slot_size(&s.dir, walk, log);
                drop(f);
                (sz, false)
            }
            LockState::Live | LockState::Unknown => (read_sidecar(&s.dir).unwrap_or(0), true),
            LockState::Missing => continue,
        };
        if let Some(o) = overrides.and_then(|m| m.get(&s.dir)) {
            size = *o;
        }
        v.push(SlotInfo {
            label: s.label,
            dir: s.dir,
            size,
            last_used,
            busy,
        });
    }
    v
}

fn enforce_root_with(
    root: &Path,
    policy: CachePolicy,
    avail: &dyn Fn() -> Option<u64>,
    dry_run: bool,
    overrides: Option<&std::collections::HashMap<PathBuf, u64>>,
    walk: &dyn Fn(&Path) -> u64,
    log: &dyn Fn(String),
) -> Vec<PathBuf> {
    let free = if policy.soft_free > 0 { avail() } else { None };
    let floor_hit = free.is_some_and(|f| f < policy.soft_free);
    if policy.cache_max == 0 && !floor_hit {
        return Vec::new();
    }
    let infos = collect_infos(root, overrides, walk, log);
    let plan = plan_evictions(&infos, policy.cache_max, free, policy.soft_free);
    let mut evicted = Vec::new();
    let would = if dry_run { "would evict" } else { "evicted" };
    for (i, reason) in plan {
        let s = &infos[i];
        // Re-probe: the slot may have been taken since it was measured.
        let LockState::Free(f) = probe_lock(&slot_lock_path(&s.dir)) else {
            continue;
        };
        if !dry_run && std::fs::remove_dir_all(&s.dir).is_err() {
            continue;
        }
        if !dry_run {
            remove_sidecar(&s.dir);
        }
        drop(f);
        let why = match reason {
            EvictReason::Budget { total, budget } => {
                format!("cache {} > budget {}", fmt_bytes(total), fmt_bytes(budget))
            }
            EvictReason::Floor { free, floor } => {
                format!("free {} < soft floor {}", fmt_bytes(free), fmt_bytes(floor))
            }
        };
        let detail = match reason {
            EvictReason::Budget { .. } => format!("{}; {why}", fmt_bytes(s.size)),
            EvictReason::Floor { .. } => why,
        };
        log(format!("{would} idle {} ({detail})", s.label));
        evicted.push(s.dir.clone());
    }
    evicted
}

/// Root-wide enforcement of the budget and soft floor over every repo key under `root`:
/// idle slots are evicted LRU while holding their lock; busy slots count but are skipped.
/// Returns the evicted slot dirs.
pub fn enforce_root(
    root: &Path,
    policy: CachePolicy,
    avail: &dyn Fn() -> Option<u64>,
    log: &dyn Fn(String),
) -> Vec<PathBuf> {
    enforce_root_with(root, policy, avail, false, None, &slot_usage, log)
}

/// `hyperpanes worker --prune-cache [--dry-run]`: for every idle slot apply the per-slot cap,
/// then the root-wide eviction; busy slots are reported and skipped. One `log` line per slot.
pub fn prune_cache(
    root: &Path,
    policy: CachePolicy,
    avail: &dyn Fn() -> Option<u64>,
    dry_run: bool,
    log: &dyn Fn(String),
) {
    prune_cache_with(root, policy, avail, dry_run, &slot_usage, log)
}

fn prune_cache_with(
    root: &Path,
    policy: CachePolicy,
    avail: &dyn Fn() -> Option<u64>,
    dry_run: bool,
    walk: &dyn Fn(&Path) -> u64,
    log: &dyn Fn(String),
) {
    let mut after_sizes = std::collections::HashMap::new();
    for s in list_slots(root) {
        let LockState::Free(f) = probe_lock(&s.lock_path) else {
            log(format!("{}: busy, skipped", s.label));
            continue;
        };
        let size = slot_size(&s.dir, walk, &|_| {});
        let out = apply_slot_cap(&s.dir, size, policy.slot_max, dry_run);
        if !dry_run && out.action != SlotPrune::Keep {
            write_sidecar(&s.dir, out.after);
        }
        let what = match out.action {
            SlotPrune::Keep => "kept".to_string(),
            SlotPrune::Incremental if dry_run => "would prune incremental".to_string(),
            SlotPrune::Incremental => "pruned incremental".to_string(),
            SlotPrune::Wipe if dry_run => "would wipe contents".to_string(),
            SlotPrune::Wipe => "wiped contents".to_string(),
        };
        log(format!(
            "{}: {what} ({} -> {}, cap {})",
            s.label,
            fmt_bytes(out.before),
            fmt_bytes(out.after),
            if policy.slot_max == 0 {
                "off".to_string()
            } else {
                fmt_bytes(policy.slot_max)
            }
        ));
        after_sizes.insert(s.dir.clone(), out.after);
        drop(f);
    }
    enforce_root_with(root, policy, avail, dry_run, Some(&after_sizes), walk, log);
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
                remove_sidecar(&dir);
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
        // Its uncommitted work was saved before removal (#86); the clean orphan saved nothing.
        let patch = o.patch.as_ref().expect("dirty orphan saved a patch");
        assert_eq!(patch, &evidence_path(&kd, "orphan"));
        assert!(std::fs::read_to_string(patch)
            .unwrap()
            .contains("dirty.txt"));
        let n = rep.swept.iter().find(|s| s.path == nolock).unwrap();
        assert_eq!(n.patch, None);
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
        // Readable while held only on unix (flock is advisory); Windows' LockFileEx is a
        // mandatory byte-range lock, so another handle can't read it until it's released.
        #[cfg(unix)]
        {
            let info = std::fs::read_to_string(a.lock_path()).unwrap();
            assert!(
                info.contains("queue=q") && info.contains("branch=b"),
                "{info}"
            );
        }
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

    // ───────────── slot size cap / budget / soft floor ─────────────

    use std::cell::{Cell, RefCell};

    fn fill(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; bytes]).unwrap();
    }

    /// A slot with incremental + deps data, as cargo would lay it out. Returns the slot.
    fn build_slot(key_dir: &Path, incr: usize, deps: usize) -> CacheSlot {
        let slot = acquire_slot(key_dir).unwrap();
        fill(
            &slot.path.join("cargo-target/debug/incremental/a/blob"),
            incr,
        );
        fill(&slot.path.join("cargo-target/debug/deps/lib.rlib"), deps);
        slot
    }

    fn no_log(_: String) {}

    /// A sibling test spawning `git` can briefly leak a lock fd into its child (until exec), making
    /// an idle slot read busy. Retry an eviction call a few times until it evicts something.
    fn retry_nonempty(mut f: impl FnMut() -> Vec<PathBuf>) -> Vec<PathBuf> {
        for _ in 0..50 {
            let v = f();
            if !v.is_empty() {
                return v;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Vec::new()
    }

    #[test]
    fn default_soft_free_is_a_tenth_of_the_disk_capped_at_40g() {
        const G: u64 = 1024 * 1024 * 1024;
        // A small disk (laptop SSD / CI runner) gets a tenth of its size, not a flat 40G.
        assert_eq!(default_soft_free(Some(100 * G)), 10 * G);
        assert_eq!(default_soft_free(Some(256 * G)), 25 * G + 6 * G / 10);
        // A big disk hits the 40G cap.
        assert_eq!(default_soft_free(Some(2048 * G)), 40 * G);
        assert_eq!(default_soft_free(Some(400 * G)), 40 * G);
        // Unknown or zero size falls back to the cap.
        assert_eq!(default_soft_free(None), 40 * G);
        assert_eq!(default_soft_free(Some(0)), 40 * G);
    }

    #[test]
    fn soft_free_explicit_setting_beats_the_disk_default() {
        // flag > env > default; `0` stays "off".
        let d = default_soft_free(Some(100 * 1024 * 1024 * 1024));
        assert_eq!(resolve_size_setting(Some(0), None, SOFT_FREE_ENV, d), Ok(0));
        assert_eq!(
            resolve_size_setting(None, Some("5G".into()), SOFT_FREE_ENV, d),
            Ok(5 * 1024 * 1024 * 1024)
        );
        assert_eq!(resolve_size_setting(None, None, SOFT_FREE_ENV, d), Ok(d));
    }

    #[test]
    fn total_bytes_is_at_least_available_bytes() {
        let t = std::env::temp_dir();
        let total = total_bytes(&t).unwrap();
        assert!(total > 0);
        assert!(total >= available_bytes(&t).unwrap());
    }

    #[test]
    fn plan_slot_prune_table() {
        use SlotPrune::*;
        // (size, incremental, cap, expected)
        let cases = [
            (10, 5, 0, Keep),
            (10, 5, 10, Keep),
            (10, 0, 20, Keep),
            (11, 5, 10, Incremental),
            (11, 1, 10, Incremental),
            (11, 0, 10, Wipe),
            (30, 5, 10, Wipe),
            (30, 20, 10, Incremental),
            (30, 99, 10, Incremental),
        ];
        for (size, inc, cap, want) in cases {
            assert_eq!(plan_slot_prune(size, inc, cap), want, "{size}/{inc}/{cap}");
        }
    }

    #[test]
    fn usage_counts_nested_files_and_does_not_follow_symlinks() {
        let d = scratch("usage");
        let outside = scratch("usage-out");
        fill(&outside.join("big"), 1 << 20);
        fill(&d.join("a/b/c/f1"), 64 * 1024);
        fill(&d.join("a/f2"), 64 * 1024);
        let base = slot_usage(&d);
        assert!(base >= 128 * 1024, "nested files counted, got {base}");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, d.join("link")).unwrap();
            let with_link = slot_usage(&d);
            assert!(
                with_link < base + 64 * 1024,
                "symlink target must not be followed ({base} -> {with_link})"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn incremental_dirs_found_at_profile_and_triple_depth_only_under_cargo_target() {
        let d = scratch("incdirs");
        fill(&d.join("cargo-target/debug/incremental/x"), 1);
        fill(
            &d.join("cargo-target/x86_64-unknown-linux-gnu/release/incremental/y"),
            1,
        );
        fill(&d.join("other/debug/incremental/z"), 1);
        fill(&d.join("cargo-target/debug/deps/w"), 1);
        let mut got = incremental_dirs(&d);
        got.sort();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got.iter().all(|p| p.starts_with(d.join("cargo-target"))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn release_prunes_incremental_first_and_keeps_deps() {
        let kd = scratch("rel-inc");
        let slot = build_slot(&kd, 64 * 1024, 64 * 1024);
        let dir = slot.path.clone();
        let total = slot_usage(&dir);
        let incr = slot_usage(&dir.join("cargo-target/debug/incremental"));
        let cap = total - incr / 2; // over cap, but dropping incremental/ is enough
        slot.release_enforcing_with(cap, &slot_usage, &no_log);
        assert!(!dir.join("cargo-target/debug/incremental").exists());
        assert!(dir.join("cargo-target/debug/deps/lib.rlib").exists());
        assert!(kd.join("cache/slot-0.lock").exists(), "lock file survives");
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn release_wipes_contents_when_still_over_cap_and_keeps_lock() {
        let kd = scratch("rel-wipe");
        let slot = build_slot(&kd, 64 * 1024, 64 * 1024);
        let dir = slot.path.clone();
        slot.release_enforcing_with(1024, &slot_usage, &no_log);
        assert!(dir.is_dir(), "slot dir itself kept");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "contents wiped"
        );
        assert!(kd.join("cache/slot-0.lock").exists(), "lock file survives");
        assert_eq!(read_sidecar(&dir), Some(0));
        // The slot is immediately reusable.
        assert!(acquire_slot(&kd).is_ok());
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn release_with_cap_off_never_prunes() {
        let kd = scratch("rel-off");
        let slot = build_slot(&kd, 64 * 1024, 64 * 1024);
        let dir = slot.path.clone();
        slot.release_enforcing_with(0, &slot_usage, &no_log);
        assert!(dir.join("cargo-target/debug/incremental").exists());
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn sidecar_is_fresh_after_ordered_release_and_stale_after_plain_drop() {
        let kd = scratch("side");
        let slot = acquire_slot(&kd).unwrap();
        let dir = slot.path.clone();
        slot.release_with_size(1234);
        assert_eq!(read_fresh_sidecar(&dir), Some(1234));
        assert!(
            mtime_of(&sidecar_path(&dir)).unwrap() >= mtime_of(&slot_lock_path(&dir)).unwrap(),
            "lock stamped first, sidecar written after"
        );
        // A later release through Drop re-stamps the lock after the sidecar: stale.
        std::thread::sleep(Duration::from_millis(30));
        drop(acquire_slot(&kd).unwrap());
        assert_eq!(read_fresh_sidecar(&dir), None);
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn acquire_walks_when_sidecar_missing_or_stale_and_skips_when_fresh() {
        let kd = scratch("acq");
        let slot = build_slot(&kd, 4 * 1024, 4 * 1024);
        let dir = slot.path.clone();
        drop(slot); // plain Drop: no sidecar
        let walks = Cell::new(0u32);
        let walker = |p: &Path| {
            walks.set(walks.get() + 1);
            slot_usage(p)
        };
        let big_cap = 1 << 30;
        // Missing sidecar -> walk.
        let s = acquire_slot(&kd).unwrap();
        s.enforce_on_acquire_with(big_cap, &walker, &no_log);
        assert_eq!(walks.get(), 1);
        drop(s); // Drop re-stamps lock: sidecar now stale
        let s = acquire_slot(&kd).unwrap();
        s.enforce_on_acquire_with(big_cap, &walker, &no_log);
        assert_eq!(walks.get(), 2, "stale sidecar walks again");
        s.release_with_size(slot_usage(&dir));
        // Fresh sidecar -> no walk.
        let s = acquire_slot(&kd).unwrap();
        s.enforce_on_acquire_with(big_cap, &walker, &no_log);
        assert_eq!(walks.get(), 2, "fresh sidecar skips the walk");
        drop(s);
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn acquire_backstop_prunes_oversized_slot_left_by_older_runner() {
        let kd = scratch("acq-prune");
        drop(build_slot(&kd, 64 * 1024, 64 * 1024));
        let s = acquire_slot(&kd).unwrap();
        s.enforce_on_acquire_with(1024, &slot_usage, &no_log);
        assert_eq!(std::fs::read_dir(&s.path).unwrap().count(), 0);
        drop(s);
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn walk_is_logged_with_duration() {
        let kd = scratch("walklog");
        let slot = build_slot(&kd, 4096, 4096);
        let lines = RefCell::new(Vec::new());
        slot.release_enforcing_with(0, &slot_usage, &|m| lines.borrow_mut().push(m));
        let l = lines.borrow();
        assert!(
            l[0].starts_with("slot-0: measured ") && l[0].contains(" in "),
            "{l:?}"
        );
        let _ = std::fs::remove_dir_all(&kd);
    }

    /// REGRESSION (#74): on the base nothing bounds a slot, so an oversized idle slot survives
    /// the sweep and a fresh acquire untouched. With the cap enforced at acquire it is pruned.
    #[test]
    fn regression_oversized_idle_slot_is_pruned_not_kept() {
        let r = repo("reg74");
        let kd = scratch("reg74kd");
        let slot = build_slot(&kd, 128 * 1024, 128 * 1024);
        let dir = slot.path.clone();
        drop(slot);
        let before = slot_usage(&dir);
        sweep(&r, &kd); // base behaviour: age GC only, a fresh slot is kept as-is
        let s = acquire_slot_capped(&kd, 0).unwrap().unwrap();
        s.enforce_on_acquire(64 * 1024, &no_log);
        let after = slot_usage(&dir);
        assert!(
            after <= 64 * 1024 && after < before,
            "oversized slot must be bounded by the cap: {before} -> {after}"
        );
        drop(s);
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&kd);
    }

    #[test]
    fn locked_slot_is_never_pruned_or_evicted() {
        let root = scratch("locked");
        let kd = root.join("myapp-1a2b3c4d");
        let busy = build_slot(&kd, 64 * 1024, 64 * 1024);
        let dir = busy.path.clone();
        let policy = CachePolicy {
            slot_max: 1024,
            cache_max: 1,
            soft_free: 1 << 40,
        };
        prune_cache_with(&root, policy, &|| Some(0), false, &slot_usage, &no_log);
        let evicted = enforce_root(&root, policy, &|| Some(0), &no_log);
        assert!(evicted.is_empty());
        assert!(dir.join("cargo-target/debug/deps/lib.rlib").exists());
        assert!(dir.join("cargo-target/debug/incremental").exists());
        drop(busy);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn info(label: &str, size: u64, age_secs: u64, busy: bool) -> SlotInfo {
        SlotInfo {
            label: label.into(),
            dir: PathBuf::from(label),
            size,
            last_used: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
            busy,
        }
    }

    #[test]
    fn budget_eviction_is_lru_and_stops_once_under_budget() {
        let slots = [
            info("new", 40, 10, false),
            info("old", 40, 300, false),
            info("mid", 40, 100, false),
        ];
        // total 120 > 100: evicting just the oldest (40) is enough.
        let plan = plan_evictions(&slots, 100, None, 0);
        assert_eq!(plan.iter().map(|p| p.0).collect::<Vec<_>>(), vec![1]);
        // total 120 > 50: need 70 -> oldest then mid.
        let plan = plan_evictions(&slots, 50, None, 0);
        assert_eq!(plan.iter().map(|p| p.0).collect::<Vec<_>>(), vec![1, 2]);
        // Within budget / budget off: nothing.
        assert!(plan_evictions(&slots, 120, None, 0).is_empty());
        assert!(plan_evictions(&slots, 0, None, 0).is_empty());
    }

    #[test]
    fn busy_slots_count_toward_total_but_are_never_planned() {
        let slots = [info("busy", 90, 500, true), info("idle", 40, 10, false)];
        let plan = plan_evictions(&slots, 100, None, 0);
        assert_eq!(plan.iter().map(|p| p.0).collect::<Vec<_>>(), vec![1]);
        let all_busy = [info("b", 500, 1, true)];
        assert!(plan_evictions(&all_busy, 10, None, 0).is_empty());
    }

    #[test]
    fn soft_floor_plans_until_projected_free_meets_floor() {
        let slots = [
            info("a", 30, 300, false),
            info("b", 30, 200, false),
            info("c", 30, 100, false),
        ];
        // free 35, floor 100: need 65 -> three? 30+30=60 <65 so all three.
        let plan = plan_evictions(&slots, 0, Some(35), 100);
        assert_eq!(plan.len(), 3);
        // free 50, floor 100: need 50 -> two.
        let plan = plan_evictions(&slots, 0, Some(50), 100);
        assert_eq!(plan.iter().map(|p| p.0).collect::<Vec<_>>(), vec![0, 1]);
        assert!(matches!(
            plan[0].1,
            EvictReason::Floor {
                free: 50,
                floor: 100
            }
        ));
        // free >= floor or floor off: nothing.
        assert!(plan_evictions(&slots, 0, Some(100), 100).is_empty());
        assert!(plan_evictions(&slots, 0, Some(1), 0).is_empty());
    }

    #[test]
    fn enforce_root_applies_soft_floor_with_injected_free_bytes() {
        let root = scratch("floor");
        let kd = root.join("myapp-1a2b3c4d");
        let a = build_slot(&kd, 64 * 1024, 64 * 1024);
        let b = acquire_slot(&kd).unwrap();
        fill(&b.path.join("cargo-target/debug/deps/lib.rlib"), 64 * 1024);
        let (da, db) = (a.path.clone(), b.path.clone());
        a.release_with_size(slot_usage(&da));
        std::thread::sleep(Duration::from_millis(30));
        b.release_with_size(slot_usage(&db));
        let policy = CachePolicy {
            slot_max: 0,
            cache_max: 0,
            soft_free: 1000,
        };
        // Plenty of space: nothing happens.
        assert!(enforce_root(&root, policy, &|| Some(5000), &no_log).is_empty());
        // Low: the oldest (slot-0) goes first; sizes dwarf the shortfall of 100.
        let ev = retry_nonempty(|| enforce_root(&root, policy, &|| Some(900), &no_log));
        assert_eq!(ev, vec![da.clone()]);
        assert!(!da.exists() && db.exists());
        assert_eq!(read_sidecar(&da), None, "sidecar removed with the slot");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enforce_root_budget_sums_across_repo_keys() {
        let root = scratch("budget");
        let a = build_slot(&root.join("myapp-1a2b3c4d"), 0, 64 * 1024);
        let da = a.path.clone();
        a.release_with_size(slot_usage(&da));
        std::thread::sleep(Duration::from_millis(30));
        let b = build_slot(&root.join("webapp-5e6f7a8b"), 0, 64 * 1024);
        let db = b.path.clone();
        b.release_with_size(slot_usage(&db));
        let one = slot_usage(&da);
        let policy = CachePolicy {
            slot_max: 0,
            cache_max: one + one / 2,
            soft_free: 0,
        };
        let lines = RefCell::new(Vec::new());
        let ev = retry_nonempty(|| {
            enforce_root(&root, policy, &|| None, &|m| lines.borrow_mut().push(m))
        });
        assert_eq!(
            ev,
            vec![da.clone()],
            "oldest across keys evicted, then under budget"
        );
        assert!(db.exists());
        assert!(lines.borrow().iter().any(|l| l
            .starts_with("evicted idle myapp-1a2b3c4d/slot-0 (")
            && l.contains("> budget")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_cache_dry_run_deletes_nothing_and_lists_actions() {
        let root = scratch("dry");
        let kd = root.join("myapp-1a2b3c4d");
        drop(build_slot(&kd, 64 * 1024, 64 * 1024));
        let busy_kd = root.join("webapp-5e6f7a8b");
        let busy = build_slot(&busy_kd, 64 * 1024, 64 * 1024);
        let policy = CachePolicy {
            slot_max: 1024,
            cache_max: 1,
            soft_free: 0,
        };
        let lines = RefCell::new(Vec::new());
        prune_cache_with(&root, policy, &|| None, true, &slot_usage, &|m| {
            lines.borrow_mut().push(m)
        });
        let l = lines.borrow().join("\n");
        assert!(
            l.contains("myapp-1a2b3c4d/slot-0: would wipe contents"),
            "{l}"
        );
        assert!(l.contains("webapp-5e6f7a8b/slot-0: busy, skipped"), "{l}");
        assert!(kd
            .join("cache/slot-0/cargo-target/debug/incremental")
            .exists());
        assert!(kd
            .join("cache/slot-0/cargo-target/debug/deps/lib.rlib")
            .exists());
        drop(busy);
        // Real run prunes the idle slot. A concurrently spawned `git` child in a sibling test can
        // briefly inherit the lock's fd (until its exec), so retry while the slot reads busy.
        let lines2 = RefCell::new(Vec::new());
        for _ in 0..50 {
            lines2.borrow_mut().clear();
            prune_cache_with(
                &root,
                CachePolicy {
                    cache_max: 0,
                    ..policy
                },
                &|| None,
                false,
                &slot_usage,
                &|m| lines2.borrow_mut().push(m),
            );
            if !lines2
                .borrow()
                .iter()
                .any(|l| l.contains("myapp") && l.contains("busy"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let left: Vec<_> = std::fs::read_dir(kd.join("cache/slot-0"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(left.is_empty(), "{left:?} {:?}", lines2.borrow());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn size_setting_precedence_and_errors() {
        assert_eq!(
            resolve_size_setting(Some(5), Some("9G".into()), "E", 1),
            Ok(5)
        );
        assert_eq!(
            resolve_size_setting(None, Some("2K".into()), "E", 1),
            Ok(2048)
        );
        assert_eq!(resolve_size_setting(None, Some("0".into()), "E", 1), Ok(0));
        assert_eq!(resolve_size_setting(None, Some("  ".into()), "E", 7), Ok(7));
        assert_eq!(resolve_size_setting(None, None, "E", 7), Ok(7));
        let e = resolve_size_setting(None, Some("lots".into()), "MY_VAR", 1).unwrap_err();
        assert!(e.starts_with("MY_VAR:"), "{e}");
    }

    #[test]
    fn save_uncommitted_captures_tracked_and_untracked_skips_ignored() {
        let r = repo("evid");
        std::fs::write(r.join("a.txt"), "one\n").unwrap();
        std::fs::write(r.join(".gitignore"), "target/\n").unwrap();
        git(&r, &["add", "-A"]);
        git(&r, &["commit", "-m", "base"]);
        let dest = scratch("evidkd").join("evidence/t.patch");

        // Clean tree → nothing written.
        assert_eq!(save_uncommitted(&r, &dest), Ok(None));
        assert!(!dest.exists());

        // Tracked edit + new untracked file + ignored build output.
        std::fs::write(r.join("a.txt"), "two\n").unwrap();
        std::fs::write(r.join("new.txt"), "fresh\n").unwrap();
        std::fs::create_dir_all(r.join("target")).unwrap();
        std::fs::write(r.join("target/blob"), "huge").unwrap();
        assert_eq!(save_uncommitted(&r, &dest), Ok(Some(dest.clone())));
        let patch = std::fs::read_to_string(&dest).unwrap();
        assert!(patch.contains("+two"), "{patch}");
        assert!(
            patch.contains("new.txt") && patch.contains("+fresh"),
            "{patch}"
        );
        assert!(
            !patch.contains("target/blob"),
            "ignored files stay out: {patch}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dest), 0o600, "patch is owner-only");
            assert_eq!(
                mode(dest.parent().unwrap()),
                0o700,
                "evidence dir is owner-only"
            );
        }
        // Saving never commits: HEAD is unchanged.
        assert_eq!(git(&r, &["rev-list", "--count", "HEAD"]), "2");

        // The patch round-trips: a clean checkout of the base applies it.
        git(&r, &["reset", "--hard", "HEAD"]);
        git(&r, &["clean", "-fd"]);
        git(&r, &["apply", dest.to_str().unwrap()]);
        assert_eq!(std::fs::read_to_string(r.join("a.txt")).unwrap(), "two\n");
        assert_eq!(
            std::fs::read_to_string(r.join("new.txt")).unwrap(),
            "fresh\n"
        );
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(dest.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn commits_beyond_counts_from_the_pinned_base_sha() {
        let r = repo("beyond");
        let base = git(&r, &["rev-parse", "HEAD"]);
        git(&r, &["checkout", "-b", "worker/q/t"]);
        assert_eq!(commits_beyond(&r, &base, "worker/q/t"), Ok(0));
        git(&r, &["commit", "--allow-empty", "-m", "work"]);
        assert_eq!(commits_beyond(&r, &base, "worker/q/t"), Ok(1));
        // main moving on later doesn't matter: the count is against the pinned sha.
        git(&r, &["checkout", "main"]);
        git(&r, &["commit", "--allow-empty", "-m", "advance"]);
        assert_eq!(commits_beyond(&r, &base, "worker/q/t"), Ok(1));
        assert!(commits_beyond(&r, &base, "no/such/branch").is_err());
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn sweep_gcs_old_evidence_only() {
        let r = repo("evgc");
        let kd = scratch("evgckd");
        let old = evidence_path(&kd, "old");
        let fresh = evidence_path(&kd, "fresh");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&fresh, "x").unwrap();
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now() - EVIDENCE_MAX_AGE - Duration::from_secs(3600))
            .unwrap();
        let rep = sweep(&r, &kd);
        assert_eq!(rep.gc_evidence, vec![old.clone()]);
        assert!(!old.exists());
        assert!(fresh.exists());
        let _ = std::fs::remove_dir_all(&r);
        let _ = std::fs::remove_dir_all(&kd);
    }
}
