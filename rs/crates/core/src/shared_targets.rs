//! The host's shared cargo target dirs, one per repo or workspace, and their bounded cleanup.
//!
//! Builds that are not a worker task (a goal spec agent's verify/acceptance build, a packaging
//! script) share one warm target per repo under [`default_root`] (`<cache>/targets/<name>`),
//! instead of an ad-hoc target dir per goal or per task that nothing ever deletes. That is the
//! duplicate-target root cause behind a disk fill: the same crates built into four or more dirs
//! per repo, plus `incremental/` sessions that only grow.
//!
//! [`sweep`] keeps the shared root bounded without a daemon. It runs where the worker runner
//! already cleans up: at runner start and in `hyperpanes worker --prune-cache`.
//!
//! - An `incremental/<crate>-<hash>/` session dir untouched for [`INCREMENTAL_MAX_AGE`] is removed.
//!   Cargo only ever reuses the newest session of a crate it rebuilds; an old one belongs to a
//!   branch, profile or feature set nobody builds any more.
//! - A whole target untouched for [`TARGET_IDLE_MAX_AGE`] is removed: its repo is no longer built.
//! - A target over its size cap ([`TARGET_MAX_BYTES`], `$HYPERPANES_SHARED_TARGET_MAX`, `0` = off)
//!   has whole artifact *units* removed, least recently used first, but only units unused for
//!   [`TARGET_MIN_IDLE`] (`$HYPERPANES_SHARED_TARGET_MIN_IDLE`: `48h`, `2d`, `90m` or plain
//!   seconds). A unit is everything sharing one `<name>-<16 hex>` key in a profile dir:
//!   `deps/<name>-<hash>*` (a `lib` prefix ignored), `build/<name>-<hash>/` and
//!   `.fingerprint/<name>-<hash>/`. Cargo never deletes old variants itself, so a target used
//!   daily would otherwise grow without bound. A unit used within the window is never removed,
//!   even if the target stays over the cap (that is logged).
//! - A target cargo is building in (a held `<profile>/.cargo-lock` flock) is never touched.
//! - Only a cargo target is touched at all: a dir holding cargo's `CACHEDIR.TAG` signature and a
//!   `debug/` or `release/` profile. Anything else under the root is left alone and reported.
//! - A root that is `/`, the home dir or an ancestor of it is refused: no sweep, one reason. One
//!   wrong `$HYPERPANES_SHARED_TARGETS` must never age out a home's month-old dirs.
//!
//! ```text
//! <root>/<name>/                  one shared CARGO_TARGET_DIR (e.g. targets/app, targets/core)
//!   <profile>/.cargo-lock         cargo's build lock (flock held while it builds)
//!   <profile>/.fingerprint/       touched on every build
//!   <profile>/incremental/<crate>-<hash>/s-…  incremental sessions
//!   <triple>/<profile>/…          the same per cross target
//! ```

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::persistence::paths::cache_dir;
use crate::worktree_store::slot_usage;

/// Env var overriding the shared targets root.
pub const ROOT_ENV: &str = "HYPERPANES_SHARED_TARGETS";

/// An incremental session dir older than this is removed.
pub const INCREMENTAL_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 3600);

/// A whole shared target untouched this long is removed.
pub const TARGET_IDLE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Per-target size cap: over this, least recently used units are pruned.
pub const TARGET_MAX_BYTES: u64 = 40 * (1 << 30);

/// A unit must be unused this long before the size cap may prune it.
pub const TARGET_MIN_IDLE: Duration = Duration::from_secs(2 * 24 * 3600);

/// Env var overriding [`TARGET_MAX_BYTES`] (bytes or K/M/G/T binary units; `0` = off).
pub const MAX_ENV: &str = "HYPERPANES_SHARED_TARGET_MAX";

/// Env var overriding [`TARGET_MIN_IDLE`] (`48h`, `2d`, `90m`, `30s` or plain seconds).
pub const MIN_IDLE_ENV: &str = "HYPERPANES_SHARED_TARGET_MIN_IDLE";

/// `<cache>/targets` (`~/.cache/hyperpanes/targets` on Linux).
pub fn default_root() -> PathBuf {
    cache_dir().join("targets")
}

/// `$HYPERPANES_SHARED_TARGETS` when set and non-empty, else [`default_root`].
pub fn root_from_env() -> PathBuf {
    match std::env::var_os(ROOT_ENV) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => default_root(),
    }
}

/// What one [`sweep`] removed (or, dry, would remove), with the on-disk bytes of each.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SharedSweep {
    /// Incremental session dirs past [`INCREMENTAL_MAX_AGE`].
    pub incremental: Vec<(PathBuf, u64)>,
    /// Whole targets past [`TARGET_IDLE_MAX_AGE`].
    pub idle_targets: Vec<(PathBuf, u64)>,
    /// Targets pruned (or, dry, to be pruned) to fit the size cap, or still over it.
    pub pruned: Vec<PrunedTarget>,
    /// Targets skipped because cargo is building in them.
    pub busy: Vec<PathBuf>,
    /// Dirs under the root that are no cargo target (no `CACHEDIR.TAG` signature, or no profile):
    /// never touched.
    pub not_targets: Vec<PathBuf>,
    /// Why the whole root was refused (`/`, the home dir, an ancestor of it, a relative path):
    /// nothing was swept.
    pub refused: Option<String>,
}

/// The size-cap pruning of one target.
#[derive(Debug, PartialEq, Eq)]
pub struct PrunedTarget {
    pub target: PathBuf,
    /// Units removed (or, dry, to be removed).
    pub units: usize,
    /// Bytes those units held.
    pub freed: u64,
    /// The target's on-disk size before pruning, and after (estimated in a dry run).
    pub before: u64,
    pub after: u64,
    pub cap: u64,
    pub min_idle: Duration,
    /// Still over the cap: the rest was used within `min_idle`.
    pub still_over: bool,
}

impl SharedSweep {
    /// Bytes removed (or, dry, removable).
    pub fn freed(&self) -> u64 {
        self.incremental
            .iter()
            .chain(self.idle_targets.iter())
            .map(|(_, n)| *n)
            .sum::<u64>()
            + self.pruned.iter().map(|p| p.freed).sum::<u64>()
    }
}

/// The age limits of a [`sweep`].
#[derive(Debug, Clone, Copy)]
pub struct SweepAges {
    pub incremental: Duration,
    pub idle_target: Duration,
    /// Per-target size cap in bytes; `0` = no size pruning.
    pub max_bytes: u64,
    /// Minimum unused time before the size cap may prune a unit.
    pub min_idle: Duration,
}

impl Default for SweepAges {
    fn default() -> Self {
        Self {
            incremental: INCREMENTAL_MAX_AGE,
            idle_target: TARGET_IDLE_MAX_AGE,
            max_bytes: TARGET_MAX_BYTES,
            min_idle: TARGET_MIN_IDLE,
        }
    }
}

/// Parses `48h`, `2d`, `90m`, `30s` or plain seconds (`172800`).
pub fn parse_idle(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    let end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(end);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("invalid duration '{s}': expected e.g. 48h, 2d or seconds"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        other => return Err(format!("invalid duration '{s}': unknown unit '{other}'")),
    };
    n.checked_mul(mult)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration '{s}' overflows"))
}

impl SweepAges {
    /// The defaults with the size cap and min idle read through `get` (an env lookup). An invalid
    /// value is an `Err` naming it; the caller decides (the runner warns and uses the default).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut a = Self::default();
        if let Some(v) = get(MAX_ENV).filter(|v| !v.trim().is_empty()) {
            a.max_bytes =
                crate::worktree_store::parse_size(&v).map_err(|e| format!("{MAX_ENV}: {e}"))?;
        }
        if let Some(v) = get(MIN_IDLE_ENV).filter(|v| !v.trim().is_empty()) {
            a.min_idle = parse_idle(&v).map_err(|e| format!("{MIN_IDLE_ENV}: {e}"))?;
        }
        Ok(a)
    }

    /// [`Self::from_lookup`] over the process environment.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(&|k| std::env::var(k).ok())
    }
}

fn is_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(p).ok()?.modified().ok()
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_dir(p))
        .collect();
    out.sort();
    out
}

/// The profile dirs of a target: `<t>/<profile>` and `<t>/<triple>/<profile>`, i.e. every dir at
/// depth 1 or 2 that holds `.fingerprint` or `incremental` or `.cargo-lock`.
fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let looks_like_profile = |d: &Path| {
        d.join(".fingerprint").exists()
            || d.join("incremental").exists()
            || d.join(".cargo-lock").exists()
    };
    let mut out = Vec::new();
    for d in subdirs(target) {
        if looks_like_profile(&d) {
            out.push(d.clone());
        }
        for dd in subdirs(&d) {
            if looks_like_profile(&dd) {
                out.push(dd);
            }
        }
    }
    out
}

/// Takes cargo's build lock (`.cargo-lock`) of every profile of a target and keeps it, so a cargo
/// that starts while the target is swept waits ("Blocking waiting for file lock") instead of
/// building in a dir being removed. `None` when cargo holds one (or one cannot be opened or
/// locked): the target is busy, and a doubt never deletes. Outside a dry run a profile with no lock
/// file yet gets one, as cargo itself would create it; a dry run creates nothing and skips it.
fn lock_target(profiles: &[PathBuf], dry_run: bool) -> Option<Vec<File>> {
    let mut held = Vec::new();
    for p in profiles {
        let lock = p.join(".cargo-lock");
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create(!dry_run)
            .truncate(false)
            .open(&lock);
        match opened {
            Ok(f) => match f.try_lock() {
                Ok(()) => held.push(f),
                Err(_) => return None,
            },
            Err(e) if dry_run && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(held)
}

/// The removal of one dir; a test hook sees each path before it goes.
type Remove<'a> = &'a dyn Fn(&Path) -> bool;

fn remove(path: &Path) -> bool {
    std::fs::remove_dir_all(path).is_ok()
}

/// When a target was last built in: the newest mtime of the target, its profile dirs and their
/// `.fingerprint` dirs (cargo adds entries there on every build that rebuilds a unit; a build
/// that rebuilds nothing moves none of them, so a target only no-op-built for the idle age goes,
/// at the cost of one cold build).
fn last_used(target: &Path, profiles: &[PathBuf]) -> Option<SystemTime> {
    std::iter::once(target.to_path_buf())
        .chain(profiles.iter().cloned())
        .chain(profiles.iter().map(|p| p.join(".fingerprint")))
        .filter_map(|p| mtime(&p))
        .max()
}

/// The first line cargo writes in every target dir's `CACHEDIR.TAG`.
pub const CARGO_TAG_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Whether `dir` is a cargo target: cargo's `CACHEDIR.TAG` signature, and a `debug/` or `release/`
/// profile dir (not symlinks).
fn is_cargo_target(dir: &Path) -> bool {
    let tagged = std::fs::symlink_metadata(dir.join("CACHEDIR.TAG")).is_ok_and(|m| m.is_file())
        && std::fs::read_to_string(dir.join("CACHEDIR.TAG"))
            .is_ok_and(|t| t.starts_with(CARGO_TAG_SIGNATURE));
    tagged && (is_dir(&dir.join("debug")) || is_dir(&dir.join("release")))
}

/// The user's home dir, which [`sweep`] never takes (nor an ancestor of it) as its root.
pub fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Why `root` must not be swept: not absolute, `/` (or a drive root), `home` itself or an ancestor
/// of `home`. Both are compared resolved when they exist, so a symlinked or `..` path reads as
/// where it points. `None`: the root may be swept.
pub fn root_refusal(root: &Path, home: Option<&Path>) -> Option<String> {
    if !root.is_absolute() {
        return Some(format!("{} is not an absolute path", root.display()));
    }
    let resolved = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if resolved.parent().is_none() {
        return Some(format!("{} is a filesystem root", root.display()));
    }
    if let Some(home) = home {
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        if home.starts_with(&resolved) {
            return Some(format!(
                "{} is the home dir or an ancestor of it",
                root.display()
            ));
        }
    }
    None
}

fn older_than(t: Option<SystemTime>, now: SystemTime, max: Duration) -> bool {
    t.and_then(|t| now.duration_since(t).ok())
        .is_some_and(|age| age > max)
}

/// Sweep every shared target under `root` (see the module docs). `home` is the home dir the root
/// must not be nor contain (`root_refusal`); `now` is injected for tests; `dry_run` only plans. A
/// missing root is an empty sweep. Symlinks are never followed.
pub fn sweep(
    root: &Path,
    home: Option<&Path>,
    now: SystemTime,
    ages: SweepAges,
    dry_run: bool,
) -> SharedSweep {
    sweep_with(root, home, now, ages, dry_run, &remove)
}

fn sweep_with(
    root: &Path,
    home: Option<&Path>,
    now: SystemTime,
    ages: SweepAges,
    dry_run: bool,
    remove: Remove<'_>,
) -> SharedSweep {
    let mut rep = SharedSweep::default();
    if let Some(why) = root_refusal(root, home) {
        rep.refused = Some(why);
        return rep;
    }
    for target in subdirs(root) {
        if !is_cargo_target(&target) {
            rep.not_targets.push(target);
            continue;
        }
        let profiles = profile_dirs(&target);
        // Held until this target is done: dropped at the end of the loop body.
        let Some(held) = lock_target(&profiles, dry_run) else {
            rep.busy.push(target);
            continue;
        };
        if older_than(last_used(&target, &profiles), now, ages.idle_target) {
            let size = slot_usage(&target);
            // Windows cannot delete a file a handle holds open: let go of the locks right before
            // the whole target goes. Elsewhere they are held through the removal.
            #[cfg(windows)]
            drop(held);
            if dry_run || remove(&target) {
                rep.idle_targets.push((target, size));
            }
            continue;
        }
        for profile in &profiles {
            for session in subdirs(&profile.join("incremental")) {
                if older_than(mtime(&session), now, ages.incremental) {
                    let size = slot_usage(&session);
                    if dry_run || remove(&session) {
                        rep.incremental.push((session, size));
                    }
                }
            }
        }
        if let Some(p) = prune_to_cap(&target, &profiles, now, ages, dry_run) {
            rep.pruned.push(p);
        }
        #[cfg(not(windows))]
        drop(held);
    }
    rep
}

/// Splits `s` into (`name`, `hash`) when it ends in `-<16 hex>` optionally followed by `.ext`.
fn split_key(s: &str) -> Option<(&str, &str)> {
    let stem_end = s.find('.').unwrap_or(s.len());
    let stem = &s[..stem_end];
    let i = stem.len().checked_sub(17)?;
    let (name, rest) = stem.split_at(i);
    let hash = rest.strip_prefix('-')?;
    (!name.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some((name, hash))
}

/// Unit key of a `deps/` entry: `lib` prefix stripped, `-` and `_` unified.
fn unit_key(file_name: &str, strip_lib: bool) -> Option<String> {
    let (name, hash) = split_key(file_name)?;
    let name = if strip_lib {
        name.strip_prefix("lib")
            .filter(|n| !n.is_empty())
            .unwrap_or(name)
    } else {
        name
    };
    Some(format!("{}-{hash}", name.replace('-', "_")))
}

/// On-disk bytes of `paths` (never following symlinks), a hardlinked file counted once via `seen`.
fn measure(paths: &[PathBuf], seen: &mut HashSet<(u64, u64)>) -> u64 {
    let mut total = 0u64;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = stack.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if md.nlink() > 1 && !md.is_dir() && !seen.insert((md.dev(), md.ino())) {
                continue;
            }
            total = total.saturating_add(md.blocks().saturating_mul(512));
        }
        #[cfg(not(unix))]
        {
            let _ = &seen;
            total = total.saturating_add(md.len());
        }
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    total
}

/// Newest use of `paths`: per file `max(atime, mtime)` (so an atime older than the mtime, or an
/// unavailable one, falls back to the mtime); per dir its mtime only (reading a dir moves its atime).
fn newest_use(paths: &[PathBuf]) -> Option<SystemTime> {
    let mut best: Option<SystemTime> = None;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = stack.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        let m = md.modified().ok();
        let t = if md.is_dir() {
            m
        } else {
            match (m, md.accessed().ok()) {
                (Some(m), Some(a)) => Some(m.max(a)),
                (m, a) => m.or(a),
            }
        };
        best = best.max(t);
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    best
}

struct Unit {
    paths: Vec<PathBuf>,
    last_use: SystemTime,
    bytes: u64,
}

/// Every unit of a target (see the module docs), across its profile dirs.
fn collect_units(profiles: &[PathBuf]) -> Vec<Unit> {
    let mut out = Vec::new();
    for profile in profiles {
        let mut units: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for (sub, is_deps) in [("deps", true), ("build", false), (".fingerprint", false)] {
            let dir = profile.join(sub);
            if !is_dir(&dir) {
                continue;
            }
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let key = if is_deps {
                    unit_key(&name, true)
                } else if is_dir(&e.path()) {
                    unit_key(&name, false)
                } else {
                    None
                };
                if let Some(k) = key {
                    units.entry(k).or_default().push(e.path());
                }
            }
        }
        for (_, paths) in units {
            let Some(last_use) = newest_use(&paths) else {
                continue;
            };
            let bytes = measure(&paths, &mut HashSet::new());
            out.push(Unit {
                paths,
                last_use,
                bytes,
            });
        }
    }
    out
}

fn remove_entry(p: &Path) -> bool {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p).is_ok(),
        Ok(_) => std::fs::remove_file(p).is_ok(),
        Err(_) => true,
    }
}

/// Prunes least-recently-used units of a (locked) target until it fits the size cap, never a unit
/// used within `ages.min_idle`. `None` when pruning is off or the target is within its cap.
fn prune_to_cap(
    target: &Path,
    profiles: &[PathBuf],
    now: SystemTime,
    ages: SweepAges,
    dry_run: bool,
) -> Option<PrunedTarget> {
    if ages.max_bytes == 0 {
        return None;
    }
    let before = measure(&[target.to_path_buf()], &mut HashSet::new());
    if before <= ages.max_bytes {
        return None;
    }
    let mut units = collect_units(profiles);
    units.sort_by_key(|u| u.last_use);
    let mut est = before;
    let (mut n, mut freed) = (0usize, 0u64);
    for u in &units {
        if est <= ages.max_bytes || !older_than(Some(u.last_use), now, ages.min_idle) {
            break;
        }
        let mut ok = true;
        if !dry_run {
            for p in &u.paths {
                ok &= remove_entry(p);
            }
        }
        if ok {
            n += 1;
            freed += u.bytes;
            est = est.saturating_sub(u.bytes);
        }
    }
    let after = if dry_run || n == 0 {
        est
    } else {
        measure(&[target.to_path_buf()], &mut HashSet::new())
    };
    Some(PrunedTarget {
        target: target.to_path_buf(),
        units: n,
        freed,
        before,
        after,
        cap: ages.max_bytes,
        min_idle: ages.min_idle,
        still_over: after > ages.max_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::TryLockError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DAY: Duration = Duration::from_secs(24 * 3600);

    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "hp-shtgt-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Stamps a dir's mtime. Windows opens a dir only with FILE_FLAG_BACKUP_SEMANTICS, and needs
    /// write access to set its times.
    fn touch_dir(p: &Path, t: SystemTime) {
        #[cfg(windows)]
        let f = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            OpenOptions::new()
                .write(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(p)
                .unwrap()
        };
        #[cfg(not(windows))]
        let f = File::open(p).unwrap();
        f.set_modified(t).unwrap();
    }

    /// A target with a `debug` profile: `.fingerprint`, `.cargo-lock`, and incremental sessions
    /// `fresh` and `stale`, every dir stamped `used`, the stale session `stale_at`.
    fn target(root: &Path, name: &str, used: SystemTime, stale_at: SystemTime) -> PathBuf {
        let t = root.join(name);
        let debug = t.join("debug");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(
            t.join("CACHEDIR.TAG"),
            format!("{CARGO_TAG_SIGNATURE}\n# cargo\n"),
        )
        .unwrap();
        for d in [
            "incremental/fresh-1/s-a",
            "incremental/stale-2/s-b",
            ".fingerprint",
        ] {
            std::fs::create_dir_all(debug.join(d)).unwrap();
        }
        std::fs::write(
            debug.join("incremental/stale-2/s-b/dep-graph.bin"),
            vec![0u8; 4096],
        )
        .unwrap();
        File::create(debug.join(".cargo-lock")).unwrap();
        touch_dir(&debug.join("incremental/fresh-1"), used);
        touch_dir(&debug.join("incremental/stale-2"), stale_at);
        for d in [debug.join(".fingerprint"), debug.clone(), t.clone()] {
            touch_dir(&d, used);
        }
        t
    }

    #[test]
    fn removes_only_stale_incremental_sessions_of_a_target_in_use() {
        let root = scratch("inc");
        let now = SystemTime::now();
        let t = target(&root, "app", now - DAY, now - 4 * DAY);
        let rep = sweep(&root, None, now, SweepAges::default(), false);
        assert_eq!(rep.incremental.len(), 1, "{rep:?}");
        assert_eq!(rep.incremental[0].0, t.join("debug/incremental/stale-2"));
        assert!(rep.incremental[0].1 >= 4096);
        assert!(rep.idle_targets.is_empty());
        assert!(!t.join("debug/incremental/stale-2").exists());
        assert!(t.join("debug/incremental/fresh-1").exists());
        assert!(
            t.join("debug/.fingerprint").exists(),
            "the warm build stays"
        );
    }

    #[test]
    fn removes_a_whole_target_idle_past_its_age() {
        let root = scratch("idle");
        let now = SystemTime::now();
        let idle = target(&root, "old-goal", now - 31 * DAY, now - 31 * DAY);
        let used = target(&root, "core", now - 2 * DAY, now - 2 * DAY);
        let rep = sweep(&root, None, now, SweepAges::default(), false);
        assert_eq!(
            rep.idle_targets
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>(),
            vec![idle.clone()]
        );
        assert!(!idle.exists());
        assert!(
            used.join("debug/incremental/stale-2").exists(),
            "2 days is under the incremental age"
        );
    }

    #[test]
    fn never_touches_a_target_cargo_is_building_in() {
        let root = scratch("busy");
        let now = SystemTime::now();
        let t = target(&root, "app", now - 40 * DAY, now - 40 * DAY);
        let held = File::open(t.join("debug/.cargo-lock")).unwrap();
        held.lock().unwrap();
        let rep = sweep(&root, None, now, SweepAges::default(), false);
        assert_eq!(rep.busy, vec![t.clone()]);
        assert_eq!(rep.freed(), 0);
        assert!(t.join("debug/incremental/stale-2").exists());
        drop(held);
    }

    #[test]
    fn a_dry_run_plans_and_deletes_nothing() {
        let root = scratch("dry");
        let now = SystemTime::now();
        let t = target(&root, "app", now - DAY, now - 5 * DAY);
        let rep = sweep(&root, None, now, SweepAges::default(), true);
        assert_eq!(rep.incremental.len(), 1);
        assert!(rep.freed() >= 4096);
        assert!(t.join("debug/incremental/stale-2").exists());
    }

    #[test]
    fn a_cross_target_profile_is_swept_too_and_a_missing_root_is_empty() {
        let root = scratch("cross");
        let now = SystemTime::now();
        let t = root.join("app");
        let win = t.join("x86_64-pc-windows-msvc/debug");
        std::fs::create_dir_all(t.join("debug/build")).unwrap();
        std::fs::write(t.join("CACHEDIR.TAG"), CARGO_TAG_SIGNATURE).unwrap();
        std::fs::create_dir_all(win.join("incremental/w-3/s-c")).unwrap();
        std::fs::create_dir_all(win.join(".fingerprint")).unwrap();
        touch_dir(&win.join("incremental/w-3"), now - 10 * DAY);
        let rep = sweep(&root, None, now, SweepAges::default(), false);
        assert_eq!(
            rep.incremental
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>(),
            vec![win.join("incremental/w-3")]
        );
        assert_eq!(
            sweep(&root.join("nope"), None, now, SweepAges::default(), false),
            SharedSweep::default()
        );
    }

    #[test]
    fn leaves_any_dir_that_is_no_cargo_target_alone_however_old() {
        let root = scratch("notarget");
        let now = SystemTime::now();
        // Old dirs a wrong root would hold: plain files, a profile-shaped dir with no tag, a tag
        // with no profile, a tag that is not cargo's.
        let photos = root.join("photos");
        std::fs::create_dir_all(photos.join("2020")).unwrap();
        let untagged = root.join("untagged");
        std::fs::create_dir_all(untagged.join("debug/incremental/x-1/s")).unwrap();
        std::fs::create_dir_all(untagged.join("debug/.fingerprint")).unwrap();
        let tag_only = root.join("tag-only");
        std::fs::create_dir_all(&tag_only).unwrap();
        std::fs::write(tag_only.join("CACHEDIR.TAG"), CARGO_TAG_SIGNATURE).unwrap();
        let other_tag = root.join("other-tag");
        std::fs::create_dir_all(other_tag.join("debug")).unwrap();
        std::fs::write(other_tag.join("CACHEDIR.TAG"), "Signature: not cargo").unwrap();
        for d in [&photos, &untagged, &tag_only, &other_tag] {
            touch_dir(d, now - 400 * DAY);
        }
        touch_dir(&untagged.join("debug/incremental/x-1"), now - 400 * DAY);
        let rep = sweep(&root, None, now, SweepAges::default(), false);
        assert_eq!(rep.freed(), 0, "{rep:?}");
        assert_eq!(
            rep.not_targets,
            vec![
                other_tag.clone(),
                photos.clone(),
                tag_only.clone(),
                untagged.clone()
            ]
        );
        for d in [&photos, &untagged, &tag_only, &other_tag] {
            assert!(d.exists(), "{} kept", d.display());
        }
        assert!(untagged.join("debug/incremental/x-1").exists());
    }

    #[test]
    fn refuses_a_filesystem_root_the_home_dir_an_ancestor_of_it_or_a_relative_root() {
        let base = scratch("refuse");
        let home = base.join("home/me");
        std::fs::create_dir_all(home.join(".cache/hyperpanes/targets")).unwrap();
        let now = SystemTime::now();
        // A real target right under home: a sweep of home would see it.
        let t = target(&home, "proj", now - 40 * DAY, now - 40 * DAY);
        for root in [
            PathBuf::from("/"),
            home.clone(),
            base.join("home"),
            base.clone(),
            home.join("..").join("me"),
        ] {
            let rep = sweep(&root, Some(&home), now, SweepAges::default(), false);
            assert!(rep.refused.is_some(), "{} refused", root.display());
            assert_eq!(rep.freed(), 0);
            assert!(rep.not_targets.is_empty() && rep.busy.is_empty());
        }
        assert!(t.exists(), "nothing under home was touched");
        let relative = sweep(
            Path::new("targets"),
            Some(&home),
            now,
            SweepAges::default(),
            false,
        );
        assert!(relative.refused.unwrap().contains("not an absolute path"));
        let ok = home.join(".cache/hyperpanes/targets");
        assert_eq!(
            root_refusal(&ok, Some(&home)),
            None,
            "a root inside home is fine"
        );
        assert_eq!(
            sweep(&ok, Some(&home), now, SweepAges::default(), false).refused,
            None
        );
    }

    #[test]
    fn holds_cargo_s_lock_through_every_removal_so_a_new_cargo_waits() {
        let root = scratch("hold");
        let now = SystemTime::now();
        let t = target(&root, "app", now - DAY, now - 5 * DAY);
        let lock = t.join("debug/.cargo-lock");
        let seen = std::cell::RefCell::new(Vec::new());
        let probe = |p: &Path| {
            // A cargo starting now: its lock attempt must find the sweep's lock held.
            let busy = matches!(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock)
                    .unwrap()
                    .try_lock(),
                Err(TryLockError::WouldBlock)
            );
            seen.borrow_mut().push((p.to_path_buf(), busy));
            remove(p)
        };
        let rep = sweep_with(&root, None, now, SweepAges::default(), false, &probe);
        assert_eq!(rep.incremental.len(), 1);
        assert_eq!(
            *seen.borrow(),
            vec![(t.join("debug/incremental/stale-2"), true)]
        );
        assert!(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock)
                .unwrap()
                .try_lock()
                .is_ok(),
            "released after the sweep"
        );
    }

    #[test]
    fn creates_a_missing_lock_before_sweeping_but_a_dry_run_creates_nothing() {
        let root = scratch("nolock");
        let now = SystemTime::now();
        let t = target(&root, "app", now - DAY, now - 5 * DAY);
        let lock = t.join("debug/.cargo-lock");
        std::fs::remove_file(&lock).unwrap();
        touch_dir(&t.join("debug"), now - DAY);
        let dry = sweep(&root, None, now, SweepAges::default(), true);
        assert_eq!(dry.incremental.len(), 1);
        assert!(!lock.exists(), "a dry run writes nothing");
        let rep = sweep(&root, None, now, SweepAges::default(), false);
        assert_eq!(rep.incremental.len(), 1);
        assert!(lock.exists());
    }

    // ── size-cap pruning ──

    /// Stamps atime and mtime of `p` and everything under it (symlinks left alone).
    fn stamp(p: &Path, t: SystemTime) {
        let md = std::fs::symlink_metadata(p).unwrap();
        if md.file_type().is_symlink() {
            return;
        }
        if md.is_dir() {
            for e in std::fs::read_dir(p).unwrap().flatten() {
                stamp(&e.path(), t);
            }
        }
        #[cfg(windows)]
        let f = {
            use std::os::windows::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .custom_flags(0x0200_0000)
                .open(p)
                .unwrap()
        };
        #[cfg(not(windows))]
        let f = File::open(p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
            .unwrap();
    }

    const KEY_LEN: usize = 16;

    /// One unit `<name>-<hash>` in `debug`: a lib (100 KiB), its .d, a build dir, a fingerprint dir.
    fn unit(debug: &Path, name: &str, n: u32, used: SystemTime) -> String {
        let key = format!("{name}-{n:0>KEY_LEN$x}");
        let paths = [
            debug.join("deps").join(format!("lib{key}.rlib")),
            debug.join("deps").join(format!("{key}.d")),
            debug.join("build").join(&key).join("out.bin"),
            debug.join(".fingerprint").join(&key).join("fp"),
        ];
        for (i, p) in paths.iter().enumerate() {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![1u8; if i == 0 { 100 * 1024 } else { 16 }]).unwrap();
        }
        for p in [
            debug.join("deps").join(format!("lib{key}.rlib")),
            debug.join("deps").join(format!("{key}.d")),
            debug.join("build").join(&key),
            debug.join(".fingerprint").join(&key),
        ] {
            stamp(&p, used);
        }
        key
    }

    fn present(debug: &Path, key: &str) -> [bool; 4] {
        [
            debug.join("deps").join(format!("lib{key}.rlib")).exists(),
            debug.join("deps").join(format!("{key}.d")).exists(),
            debug.join("build").join(key).exists(),
            debug.join(".fingerprint").join(key).exists(),
        ]
    }

    fn capped(max_bytes: u64) -> SweepAges {
        SweepAges {
            max_bytes,
            ..SweepAges::default()
        }
    }

    /// `n` units, unit i used `(n - i)` days ago (unit 0 the oldest, `n` days), in a fresh target.
    fn five_units(tag: &str, now: SystemTime) -> (PathBuf, PathBuf, Vec<String>) {
        let root = scratch(tag);
        let t = target(&root, "app", now - DAY, now - DAY);
        let debug = t.join("debug");
        let keys = (0..5u32)
            .map(|i| unit(&debug, "myapp", i, now - (10 - i) * DAY))
            .collect();
        (root, debug, keys)
    }

    #[test]
    fn under_the_cap_nothing_is_pruned() {
        let now = SystemTime::now();
        let (root, debug, keys) = five_units("cap-under", now);
        let rep = sweep(&root, None, now, capped(10 << 20), false);
        assert!(rep.pruned.is_empty(), "{rep:?}");
        assert!(keys.iter().all(|k| present(&debug, k) == [true; 4]));
    }

    #[test]
    fn over_the_cap_the_oldest_units_go_first_and_pruning_stops_under_the_cap() {
        let now = SystemTime::now();
        let (root, debug, keys) = five_units("cap-lru", now);
        let rep = sweep(&root, None, now, capped(300 * 1024), false);
        assert_eq!(rep.pruned.len(), 1, "{rep:?}");
        let p = &rep.pruned[0];
        assert_eq!(p.units, 3, "{p:?}");
        assert!(!p.still_over && p.after <= 300 * 1024 && p.before > 300 * 1024);
        for k in &keys[..3] {
            assert_eq!(present(&debug, k), [false; 4], "{k}");
        }
        for k in &keys[3..] {
            assert_eq!(present(&debug, k), [true; 4], "{k}");
        }
        assert!(rep.freed() >= 3 * 100 * 1024);
    }

    #[test]
    fn units_used_within_the_min_idle_window_are_kept_even_over_the_cap() {
        let now = SystemTime::now();
        let root = scratch("cap-recent");
        let t = target(&root, "app", now - DAY, now - DAY);
        let debug = t.join("debug");
        let keys: Vec<String> = (0..4u32)
            .map(|i| unit(&debug, "myapp", i, now - DAY / 2))
            .collect();
        let rep = sweep(&root, None, now, capped(100 * 1024), false);
        assert_eq!(rep.pruned.len(), 1, "{rep:?}");
        assert_eq!(rep.pruned[0].units, 0);
        assert!(rep.pruned[0].still_over);
        assert!(keys.iter().all(|k| present(&debug, k) == [true; 4]));
    }

    #[test]
    fn a_locked_target_is_not_pruned() {
        let now = SystemTime::now();
        let (root, debug, keys) = five_units("cap-locked", now);
        let held = File::open(debug.join(".cargo-lock")).unwrap();
        held.lock().unwrap();
        let rep = sweep(&root, None, now, capped(100 * 1024), false);
        assert_eq!(rep.busy.len(), 1);
        assert!(rep.pruned.is_empty());
        assert!(keys.iter().all(|k| present(&debug, k) == [true; 4]));
        drop(held);
    }

    #[test]
    fn a_dry_run_reports_the_pruning_and_deletes_nothing() {
        let now = SystemTime::now();
        let (root, debug, keys) = five_units("cap-dry", now);
        let rep = sweep(&root, None, now, capped(300 * 1024), true);
        assert_eq!(rep.pruned.len(), 1, "{rep:?}");
        assert_eq!(rep.pruned[0].units, 3);
        assert!(rep.freed() >= 3 * 100 * 1024);
        assert!(keys.iter().all(|k| present(&debug, k) == [true; 4]));
    }

    #[test]
    fn a_unit_s_deps_build_and_fingerprint_entries_go_together() {
        let now = SystemTime::now();
        let root = scratch("cap-unit");
        let t = target(&root, "app", now - DAY, now - DAY);
        let debug = t.join("debug");
        let old = unit(&debug, "my_lib", 1, now - 9 * DAY);
        let new = unit(&debug, "my_lib", 2, now - DAY / 2);
        // The cap sits between one unit and two, so exactly the old unit goes.
        let rep = sweep(&root, None, now, capped(180 * 1024), false);
        assert_eq!(rep.pruned[0].units, 1, "{rep:?}");
        assert_eq!(present(&debug, &old), [false; 4]);
        assert_eq!(present(&debug, &new), [true; 4]);
    }

    #[test]
    fn a_cap_of_zero_disables_pruning() {
        let now = SystemTime::now();
        let (root, debug, keys) = five_units("cap-zero", now);
        let rep = sweep(&root, None, now, capped(0), false);
        assert!(rep.pruned.is_empty());
        assert!(keys.iter().all(|k| present(&debug, k) == [true; 4]));
    }

    #[cfg(unix)]
    #[test]
    fn pruning_never_follows_symlinks() {
        let now = SystemTime::now();
        let (root, debug, keys) = five_units("cap-symlink", now);
        let outside = scratch("cap-symlink-out");
        std::fs::write(outside.join("keep.bin"), vec![1u8; 1024]).unwrap();
        // A symlinked build dir and a symlinked deps file, both keyed like a unit.
        let key = format!("myapp-{:0>KEY_LEN$x}", 77);
        std::os::unix::fs::symlink(&outside, debug.join("build").join(&key)).unwrap();
        std::os::unix::fs::symlink(
            outside.join("keep.bin"),
            debug.join("deps").join(format!("lib{key}.rlib")),
        )
        .unwrap();
        let rep = sweep(&root, None, now, capped(1), false);
        assert_eq!(rep.pruned.len(), 1, "{rep:?}");
        assert!(outside.join("keep.bin").exists(), "the link target stays");
        assert!(!keys.iter().any(|k| present(&debug, k).contains(&true)));
    }

    #[test]
    fn parses_the_idle_window_and_reads_the_knobs() {
        assert_eq!(parse_idle("48h").unwrap(), 2 * DAY);
        assert_eq!(parse_idle("2d").unwrap(), 2 * DAY);
        assert_eq!(parse_idle("90").unwrap(), Duration::from_secs(90));
        assert!(parse_idle("soon").is_err());
        let a = SweepAges::from_lookup(&|k| match k {
            MAX_ENV => Some("10G".into()),
            MIN_IDLE_ENV => Some("12h".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(a.max_bytes, 10 << 30);
        assert_eq!(a.min_idle, Duration::from_secs(12 * 3600));
        assert!(SweepAges::from_lookup(&|_| Some("x".into())).is_err());
    }
}
