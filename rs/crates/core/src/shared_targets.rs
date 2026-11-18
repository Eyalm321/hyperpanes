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
//! - A target cargo is building in (a held `<profile>/.cargo-lock` flock) is never touched.
//!
//! ```text
//! <root>/<name>/                  one shared CARGO_TARGET_DIR (e.g. targets/app, targets/core)
//!   <profile>/.cargo-lock         cargo's build lock (flock held while it builds)
//!   <profile>/.fingerprint/       touched on every build
//!   <profile>/incremental/<crate>-<hash>/s-…  incremental sessions
//!   <triple>/<profile>/…          the same per cross target
//! ```

use std::fs::{OpenOptions, TryLockError};
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
    /// Targets skipped because cargo is building in them.
    pub busy: Vec<PathBuf>,
}

impl SharedSweep {
    /// Bytes removed (or, dry, removable).
    pub fn freed(&self) -> u64 {
        self.incremental
            .iter()
            .chain(self.idle_targets.iter())
            .map(|(_, n)| *n)
            .sum()
    }
}

/// The age limits of a [`sweep`].
#[derive(Debug, Clone, Copy)]
pub struct SweepAges {
    pub incremental: Duration,
    pub idle_target: Duration,
}

impl Default for SweepAges {
    fn default() -> Self {
        Self {
            incremental: INCREMENTAL_MAX_AGE,
            idle_target: TARGET_IDLE_MAX_AGE,
        }
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

/// Whether cargo holds a target's build lock (any profile's `.cargo-lock`). An unreadable lock
/// counts as held, so a doubt never deletes.
fn building(profiles: &[PathBuf]) -> bool {
    profiles.iter().any(|p| {
        let lock = p.join(".cargo-lock");
        match OpenOptions::new().read(true).write(true).open(&lock) {
            Ok(f) => match f.try_lock() {
                Ok(()) => false,
                Err(TryLockError::WouldBlock) => true,
                Err(TryLockError::Error(_)) => true,
            },
            Err(e) => e.kind() != std::io::ErrorKind::NotFound,
        }
    })
}

/// When a target was last built in: the newest mtime of the target, its profile dirs and their
/// `.fingerprint` dirs (cargo writes there on every build).
fn last_used(target: &Path, profiles: &[PathBuf]) -> Option<SystemTime> {
    std::iter::once(target.to_path_buf())
        .chain(profiles.iter().cloned())
        .chain(profiles.iter().map(|p| p.join(".fingerprint")))
        .filter_map(|p| mtime(&p))
        .max()
}

fn older_than(t: Option<SystemTime>, now: SystemTime, max: Duration) -> bool {
    t.and_then(|t| now.duration_since(t).ok())
        .is_some_and(|age| age > max)
}

/// Sweep every shared target under `root` (see the module docs). `now` is injected for tests;
/// `dry_run` only plans. A missing root is an empty sweep. Symlinks are never followed.
pub fn sweep(root: &Path, now: SystemTime, ages: SweepAges, dry_run: bool) -> SharedSweep {
    let mut rep = SharedSweep::default();
    for target in subdirs(root) {
        let profiles = profile_dirs(&target);
        if building(&profiles) {
            rep.busy.push(target);
            continue;
        }
        if older_than(last_used(&target, &profiles), now, ages.idle_target) {
            let size = slot_usage(&target);
            if dry_run || std::fs::remove_dir_all(&target).is_ok() {
                rep.idle_targets.push((target, size));
            }
            continue;
        }
        for profile in &profiles {
            for session in subdirs(&profile.join("incremental")) {
                if older_than(mtime(&session), now, ages.incremental) {
                    let size = slot_usage(&session);
                    if dry_run || std::fs::remove_dir_all(&session).is_ok() {
                        rep.incremental.push((session, size));
                    }
                }
            }
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
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

    fn touch_dir(p: &Path, t: SystemTime) {
        File::open(p).unwrap().set_modified(t).unwrap();
    }

    /// A target with a `debug` profile: `.fingerprint`, `.cargo-lock`, and incremental sessions
    /// `fresh` and `stale`, every dir stamped `used`, the stale session `stale_at`.
    fn target(root: &Path, name: &str, used: SystemTime, stale_at: SystemTime) -> PathBuf {
        let t = root.join(name);
        let debug = t.join("debug");
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
        let rep = sweep(&root, now, SweepAges::default(), false);
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
        let rep = sweep(&root, now, SweepAges::default(), false);
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
        let rep = sweep(&root, now, SweepAges::default(), false);
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
        let rep = sweep(&root, now, SweepAges::default(), true);
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
        std::fs::create_dir_all(win.join("incremental/w-3/s-c")).unwrap();
        std::fs::create_dir_all(win.join(".fingerprint")).unwrap();
        touch_dir(&win.join("incremental/w-3"), now - 10 * DAY);
        let rep = sweep(&root, now, SweepAges::default(), false);
        assert_eq!(
            rep.incremental
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>(),
            vec![win.join("incremental/w-3")]
        );
        assert_eq!(
            sweep(&root.join("nope"), now, SweepAges::default(), false),
            SharedSweep::default()
        );
    }
}
