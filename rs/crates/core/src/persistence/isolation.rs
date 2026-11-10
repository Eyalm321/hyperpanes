//! Startup isolation check: refuse a *partly* isolated instance.
//!
//! A dev/test instance is "isolated" by pointing its control file, state dir, data dir and
//! config dir at a scratch dir. Isolating only SOME of them is the dangerous case, because the
//! instance then looks private (its own control file, its own port) while it quietly shares
//! the live data dir. That holds the durable work queue (`work.db`), `projects.json` and the
//! AI memory. The incident behind this check: a headless control server booted with
//! `HYPERPANES_CONTROL_FILE` + `XDG_STATE_HOME` overridden but not `XDG_DATA_HOME` opened the
//! live `work.db`, and its boot recovery requeued another project's in-flight task (see
//! `docs/dev-instance-isolation.md`).
//!
//! **Rule** (evaluated by [`evaluate`], a pure function over a resolved [`Layout`]): refuse
//! when the control file OR the state dir is diverted from its default while the data dir
//! still resolves to its default, unless `HYPERPANES_ALLOW_SHARED_DATA=1`. "Default" means
//! what `persistence::paths` resolves with no override env var: on Linux `$HOME/.local/...`,
//! with `XDG_*_HOME` ignored; on Windows the Roaming known folder, with `APPDATA` ignored. On
//! macOS every dir is `$HOME/Library/Application Support/hyperpanes`, so only a control file
//! placed outside it can trip the rule.
//!
//! The rule is deliberately asymmetric: a diverted data dir with a default state dir is NOT
//! refused here. That instance shares the live control file, which the discovery guard
//! (`control::discovery_guard`) already refuses. The work-queue single-owner lock
//! (`control::work_lock`) is the runtime backstop for whatever slips past both, and
//! `HYPERPANES_ALLOW_SHARED_DATA` never bypasses it.
//!
//! Only the processes that serve a control plane run this check: the headless daemon
//! (`app::run`) and the GUI. The CLI subcommands (`worker`, `pair`, …) and the session daemon
//! do not, because they never open the data dir's work queue.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::persistence::paths;

/// Set to `1`/`true`/`yes` to start even though the data dir is shared (see module docs).
pub const ALLOW_SHARED_DATA_ENV: &str = "HYPERPANES_ALLOW_SHARED_DATA";

/// The env var that moves the data dir on this platform: what a partly isolated instance
/// forgot to set.
#[cfg(not(any(windows, target_os = "macos")))]
pub const DATA_DIR_ENV_HINT: &str = "XDG_DATA_HOME";
#[cfg(windows)]
pub const DATA_DIR_ENV_HINT: &str = "APPDATA";
#[cfg(target_os = "macos")]
pub const DATA_DIR_ENV_HINT: &str = "HOME";

/// A fully isolated launch line for this platform, quoted in the refusal.
#[cfg(not(any(windows, target_os = "macos")))]
const ISOLATED_EXAMPLE: &str =
    "D=$(mktemp -d); HYPERPANES_CONTROL_FILE=$D/state/hyperpanes/control.json \
     XDG_STATE_HOME=$D/state XDG_DATA_HOME=$D/data XDG_CONFIG_HOME=$D/config <binary>";
#[cfg(windows)]
const ISOLATED_EXAMPLE: &str = "set APPDATA=C:\\tmp\\hp-dev & \
     set HYPERPANES_CONTROL_FILE=C:\\tmp\\hp-dev\\hyperpanes\\control.json & <binary>";
#[cfg(target_os = "macos")]
const ISOLATED_EXAMPLE: &str = "D=$(mktemp -d); HOME=$D \
     HYPERPANES_CONTROL_FILE=\"$D/Library/Application Support/hyperpanes/control.json\" <binary>";

/// The resolved locations an instance would use, next to their defaults. Built by
/// [`Layout::resolve`]; tests construct it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub control_file: PathBuf,
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub default_state_dir: PathBuf,
    pub default_data_dir: PathBuf,
}

impl Layout {
    /// Resolve from the live environment (`control_file` is the already-resolved discovery
    /// path — `paths::resolve_control_file()`). Paths are normalized so a symlinked or
    /// differently spelled dir compares equal to the real one.
    pub fn resolve(control_file: &Path) -> Layout {
        Layout {
            control_file: normalize(control_file),
            state_dir: normalize(&paths::state_dir()),
            data_dir: normalize(&paths::data_dir()),
            default_state_dir: normalize(&paths::default_state_dir()),
            default_data_dir: normalize(&paths::default_data_dir()),
        }
    }

    /// The default discovery file: `control.json` in the default state dir.
    pub(crate) fn default_control_file(&self) -> PathBuf {
        self.default_state_dir.join("control.json")
    }
}

/// Why a partly isolated instance was refused. `Display` is the full actionable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDataRefusal {
    /// One human line per diverted location (control file / state dir).
    pub diverted: Vec<String>,
    /// The (default, shared) data dir.
    pub data_dir: PathBuf,
}

impl fmt::Display for SharedDataRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refusing to start a partly isolated instance: {diverted}, but the data dir \
             {data} is still the DEFAULT one, shared with the live instance (its durable work \
             queue work.db, projects.json, AI memory). Booted like this, a dev/test instance \
             attaches the live work queue, and its boot recovery requeues other agents' \
             in-flight tasks. Fix: also point {env} at a scratch dir. A fully isolated launch \
             is: {example}. If sharing the data dir is really intended, set {allow}=1 (the \
             work.db single-owner lock still applies). See docs/dev-instance-isolation.md.",
            diverted = self.diverted.join("; "),
            data = self.data_dir.display(),
            env = DATA_DIR_ENV_HINT,
            example = ISOLATED_EXAMPLE,
            allow = ALLOW_SHARED_DATA_ENV,
        )
    }
}

impl std::error::Error for SharedDataRefusal {}

/// The pure rule (see module docs). `allow_shared` = `HYPERPANES_ALLOW_SHARED_DATA` is set.
pub fn evaluate(layout: &Layout, allow_shared: bool) -> Result<(), SharedDataRefusal> {
    let mut diverted = Vec::new();
    if layout.control_file != layout.default_control_file() {
        diverted.push(format!(
            "its control file is diverted to {} (HYPERPANES_CONTROL_FILE; default {})",
            layout.control_file.display(),
            layout.default_control_file().display()
        ));
    }
    if layout.state_dir != layout.default_state_dir {
        diverted.push(format!(
            "its state dir is diverted to {} ({}; default {})",
            layout.state_dir.display(),
            STATE_DIR_ENV_HINT,
            layout.default_state_dir.display()
        ));
    }
    let data_is_default = layout.data_dir == layout.default_data_dir;
    if diverted.is_empty() || !data_is_default || allow_shared {
        return Ok(());
    }
    Err(SharedDataRefusal {
        diverted,
        data_dir: layout.data_dir.clone(),
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
const STATE_DIR_ENV_HINT: &str = "XDG_STATE_HOME";
#[cfg(windows)]
const STATE_DIR_ENV_HINT: &str = "APPDATA";
#[cfg(target_os = "macos")]
const STATE_DIR_ENV_HINT: &str = "HOME";

/// Run the check against the live environment. The control-plane entry points call this
/// first and refuse to start on `Err`.
pub fn check(control_file: &Path) -> Result<(), SharedDataRefusal> {
    evaluate(&Layout::resolve(control_file), allow_shared_data())
}

/// `HYPERPANES_ALLOW_SHARED_DATA` is `1`/`true`/`yes`.
pub fn allow_shared_data() -> bool {
    matches!(
        std::env::var(ALLOW_SHARED_DATA_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Canonicalize the longest existing ancestor of `p` and re-append the rest, so a path that
/// does not exist yet (a fresh scratch dir, a not-yet-written control file) still compares
/// by its real location.
pub(crate) fn normalize(p: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(real) = std::fs::canonicalize(&cur) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                tail.push(name.to_os_string());
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live default layout of a user whose home is `/home/u`.
    fn default_layout() -> Layout {
        Layout {
            control_file: "/home/u/.local/state/hyperpanes/control.json".into(),
            state_dir: "/home/u/.local/state/hyperpanes".into(),
            data_dir: "/home/u/.local/share/hyperpanes".into(),
            default_state_dir: "/home/u/.local/state/hyperpanes".into(),
            default_data_dir: "/home/u/.local/share/hyperpanes".into(),
        }
    }

    #[test]
    fn the_primary_with_default_paths_starts() {
        assert_eq!(evaluate(&default_layout(), false), Ok(()));
    }

    /// The incident: control file + state dir overridden, data dir forgotten.
    #[test]
    fn control_file_and_state_overridden_but_data_default_is_refused() {
        let l = Layout {
            control_file: "/tmp/dev/xstate/hyperpanes/control.json".into(),
            state_dir: "/tmp/dev/xstate/hyperpanes".into(),
            ..default_layout()
        };
        let err = evaluate(&l, false).expect_err("partly isolated must be refused");
        assert_eq!(err.diverted.len(), 2, "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains(DATA_DIR_ENV_HINT),
            "names the env var to set: {msg}"
        );
        assert!(
            msg.contains(ALLOW_SHARED_DATA_ENV),
            "names the override: {msg}"
        );
        assert!(msg.contains("/home/u/.local/share/hyperpanes"), "{msg}");
        assert!(
            msg.contains("/tmp/dev/xstate/hyperpanes/control.json"),
            "{msg}"
        );
    }

    /// Only XDG_STATE_HOME overridden (the control file inherited from a pane = the live
    /// default) is still partly isolated.
    #[test]
    fn state_dir_alone_overridden_is_refused() {
        let l = Layout {
            state_dir: "/tmp/dev/xstate/hyperpanes".into(),
            ..default_layout()
        };
        let err = evaluate(&l, false).expect_err("refused");
        assert_eq!(err.diverted.len(), 1);
        assert!(err.diverted[0].contains("state dir"), "{err}");
    }

    #[test]
    fn control_file_alone_overridden_is_refused() {
        let l = Layout {
            control_file: "/tmp/dev/control.json".into(),
            ..default_layout()
        };
        let err = evaluate(&l, false).expect_err("refused");
        assert!(err.diverted[0].contains("HYPERPANES_CONTROL_FILE"), "{err}");
    }

    #[test]
    fn allow_shared_data_overrides_the_refusal() {
        let l = Layout {
            control_file: "/tmp/dev/xstate/hyperpanes/control.json".into(),
            state_dir: "/tmp/dev/xstate/hyperpanes".into(),
            ..default_layout()
        };
        assert!(evaluate(&l, false).is_err());
        assert_eq!(evaluate(&l, true), Ok(()));
    }

    #[test]
    fn fully_isolated_instance_starts() {
        let l = Layout {
            control_file: "/tmp/dev/xstate/hyperpanes/control.json".into(),
            state_dir: "/tmp/dev/xstate/hyperpanes".into(),
            data_dir: "/tmp/dev/xdata/hyperpanes".into(),
            ..default_layout()
        };
        assert_eq!(evaluate(&l, false), Ok(()));
    }

    /// Diverted data dir with default state/control: not this check's business (the
    /// discovery guard refuses the shared live control file).
    #[test]
    fn data_dir_alone_diverted_is_not_refused() {
        let l = Layout {
            data_dir: "/tmp/dev/xdata/hyperpanes".into(),
            ..default_layout()
        };
        assert_eq!(evaluate(&l, false), Ok(()));
    }

    #[test]
    fn normalize_resolves_existing_ancestors_and_keeps_the_missing_tail() {
        let base = std::env::temp_dir().join(format!("hp-iso-norm-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let real = std::fs::canonicalize(&base).unwrap();
        let p = base.join(".").join("not-yet").join("control.json");
        assert_eq!(normalize(&p), real.join("not-yet").join("control.json"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
