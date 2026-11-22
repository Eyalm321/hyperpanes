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
    /// Dirs under the root that are no cargo target (no `CACHEDIR.TAG` signature, or no profile):
    /// never touched.
    pub not_targets: Vec<PathBuf>,
    /// Why the whole root was refused (`/`, the home dir, an ancestor of it, a relative path):
    /// nothing was swept.
    pub refused: Option<String>,
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
        #[cfg(not(windows))]
        drop(held);
    }
    rep
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
}
