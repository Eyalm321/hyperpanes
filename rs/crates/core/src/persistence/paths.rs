//! userData-dir + file-path resolution (replaces Electron `app.getPath('userData')`).
//!
//! ⚠ PARITY-CRITICAL: this MUST resolve to the EXACT same folder the running Electron
//! app uses, or the MCP can't find `control.json` and last-session restore breaks.
//! Electron computes `userData` as `app.getPath('appData')/<productName>`, and the
//! product name comes from `package.json` — there is no `productName` override, so
//! `app.getName()` falls back to `name` = `"hyperpanes"`. Per platform `appData` is:
//!
//! - Windows: `%APPDATA%` (the Roaming profile). EMPIRICALLY VERIFIED on this machine:
//!   the production app's folder `%APPDATA%\hyperpanes` is the one holding
//!   `control.json` / `control-settings.json` / `last-workspace.json` / `projects.json`
//!   (the dev build uses `hyperpanes-dev`, which we deliberately do NOT target — the
//!   native binary replaces the production build). See the
//!   `reads_a_file_the_real_electron_app_wrote` test below.
//! - macOS: `~/Library/Application Support` (everything stays in the one folder —
//!   the platform convention).
//! - Linux: `$XDG_CONFIG_HOME` (default `~/.config`) — so `user_data_dir()` is the
//!   XDG config dir, exactly where an Electron build would have put it. On Linux the
//!   individual files are additionally split XDG-correctly: settings stay in config,
//!   durable user data goes to `$XDG_DATA_HOME` (default `~/.local/share`), and
//!   session/runtime state goes to `$XDG_STATE_HOME` (default `~/.local/state`).
//!   See [`config_dir`] / [`data_dir`] / [`state_dir`]; on Windows/macOS all three
//!   are the same folder, so those platforms' paths are byte-identical to before.
//!
//! Owned by track `unix-core` (Windows behavior frozen).

use std::path::{Path, PathBuf};

/// The Electron product name (= `package.json` `name`, no `productName` override),
/// which is the literal userData subfolder under the per-platform app-data base.
pub const PRODUCT_NAME: &str = "hyperpanes";

/// `%APPDATA%` (Windows Roaming app-data), mirroring Electron's `app.getPath('appData')`.
/// Electron prefers the `APPDATA` env var and falls back to the known folder; we do the
/// same (the `directories` crate resolves the Roaming known folder as the fallback).
#[cfg(windows)]
fn app_data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("APPDATA") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        // On Windows `data_dir()` is `{FOLDERID_RoamingAppData}` == `%APPDATA%`.
        return dirs.data_dir().to_path_buf();
    }
    PathBuf::from(".")
}

/// The user's home directory: `$HOME`, falling back to the OS-resolved home.
#[cfg(not(windows))]
fn home_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("HOME") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        return dirs.home_dir().to_path_buf();
    }
    PathBuf::from(".")
}

/// Resolve an XDG base dir: the env var if set to an absolute path (the spec says a
/// relative value must be ignored), else `home/<rel>`. Pure core is [`pick_base`].
#[cfg(not(any(windows, target_os = "macos")))]
fn xdg_dir(var: &str, home_rel: &str) -> PathBuf {
    pick_base(
        std::env::var_os(var).map(PathBuf::from),
        &home_dir(),
        home_rel,
    )
}

/// Pure XDG base-dir rule, split out for tests: an absolute, non-empty `env_value`
/// wins; anything else falls back to `home/<rel>`.
#[cfg(not(any(windows, target_os = "macos")))]
fn pick_base(env_value: Option<PathBuf>, home: &Path, rel: &str) -> PathBuf {
    if let Some(p) = env_value {
        if !p.as_os_str().is_empty() && p.is_absolute() {
            return p;
        }
    }
    home.join(rel)
}

/// The canonical userData directory — equal to Electron's `app.getPath('userData')`
/// for the production build on every platform:
/// Windows `%APPDATA%\hyperpanes`, macOS `~/Library/Application Support/hyperpanes`,
/// Linux `$XDG_CONFIG_HOME/hyperpanes` (default `~/.config/hyperpanes`).
pub fn user_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        app_data_dir().join(PRODUCT_NAME)
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .join("Library")
            .join("Application Support")
            .join(PRODUCT_NAME)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        xdg_dir("XDG_CONFIG_HOME", ".config").join(PRODUCT_NAME)
    }
}

/// Where user-edited settings live. Same as [`user_data_dir`] on every platform
/// (on Linux that already IS the XDG config dir).
pub fn config_dir() -> PathBuf {
    user_data_dir()
}

/// Where durable user data lives: [`user_data_dir`] on Windows/macOS;
/// `$XDG_DATA_HOME/hyperpanes` (default `~/.local/share/hyperpanes`) on Linux.
pub fn data_dir() -> PathBuf {
    #[cfg(any(windows, target_os = "macos"))]
    {
        user_data_dir()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        xdg_dir("XDG_DATA_HOME", ".local/share").join(PRODUCT_NAME)
    }
}

/// Where session/runtime state lives: [`user_data_dir`] on Windows/macOS;
/// `$XDG_STATE_HOME/hyperpanes` (default `~/.local/state/hyperpanes`) on Linux.
pub fn state_dir() -> PathBuf {
    #[cfg(any(windows, target_os = "macos"))]
    {
        user_data_dir()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        xdg_dir("XDG_STATE_HOME", ".local/state").join(PRODUCT_NAME)
    }
}

/// Discovery file the control server writes (port/token/pid/version/events URL).
/// Runtime state → [`state_dir`].
pub fn control_json() -> PathBuf {
    state_dir().join("control.json")
}

/// `{ enabled, allowInput }` control-server settings. User setting → [`config_dir`].
pub fn control_settings_json() -> PathBuf {
    config_dir().join("control-settings.json")
}

/// Persisted paired-device tokens (mobile clients). Lives beside `control.json` in the state
/// dir — the running server reads it on start and `hyperpanes pair`/`devices`/`revoke` drive it
/// through the control API, so the two always agree.
pub fn device_tokens_json() -> PathBuf {
    state_dir().join("device-tokens.json")
}

/// The last saved session (restored on launch). Session state → [`state_dir`].
pub fn last_workspace_json() -> PathBuf {
    state_dir().join("last-workspace.json")
}

/// Remembered git-project history. Durable user data → [`data_dir`].
pub fn projects_json() -> PathBuf {
    data_dir().join("projects.json")
}

/// The durable work-queue DB (SQLite/WAL) backing the `/queues` + `/tasks` routes. A
/// real embedder opens this via `WorkQueue::open` (tests stay in-memory). Durable user
/// data → [`data_dir`], alongside `projects.json`.
pub fn work_db() -> PathBuf {
    data_dir().join("work.db")
}

/// The Claude-account registry (goals system account rotation): the list of `CLAUDE_CONFIG_DIR`s
/// to rotate agents across. Optional — absent falls back to discovery (see
/// [`crate::claude_accounts::load`]). Durable user data → [`data_dir`].
pub fn claude_accounts_json() -> PathBuf {
    data_dir().join("claude-accounts.json")
}

/// Ambient-AI settings. User setting → [`config_dir`].
pub fn ai_settings_json() -> PathBuf {
    config_dir().join("ai-settings.json")
}

/// Ambient-AI per-pane memory. Durable user data → [`data_dir`].
pub fn ai_memory_json() -> PathBuf {
    data_dir().join("ai-memory.json")
}

/// Persisted control-pane id map (session uid → external pane id). Written by the GUI's
/// control host whenever the map changes and reloaded on start, so a relaunch can still
/// resolve a re-attached control-spawned pane's `HYPERPANES_PANE_ID` (which is baked into
/// the pane's environment at spawn and keys the Claude session markers below).
/// Runtime state → [`state_dir`].
pub fn control_pane_ids_json() -> PathBuf {
    state_dir().join("control-pane-ids.json")
}

/// Durable session→prompt queue (see [`crate::resume_queue`]): messages waiting for a
/// Claude conversation to (re)appear — typed into its pane on the next SessionStart
/// marker. Runtime state → [`state_dir`].
pub fn resume_prompts_json() -> PathBuf {
    state_dir().join("resume-prompts.json")
}

/// Directory of per-pane Claude session markers (`<pane-id>.json`), written by the
/// Claude Code SessionStart/SessionEnd hook (`resources/claude/hp-claude-session-hook.sh`)
/// and read by the relaunch snapshot so a restored pane can `claude --resume` its
/// conversation. Runtime state → [`state_dir`]: a marker only describes a live pane,
/// so it must not survive into backups/dotfile syncs as durable data.
pub fn claude_sessions_dir() -> PathBuf {
    state_dir().join("claude-sessions")
}

/// Write `contents` to `path` atomically: create the parent dir, write to a sibling
/// temp file, then rename over the target (a single filesystem op — readers never see
/// a half-written file). `std::fs::rename` replaces the destination on Windows.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string());
    // Same-directory temp so the rename stays on one volume; pid keeps it unique
    // across concurrent writers.
    let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn user_data_dir_is_appdata_hyperpanes() {
        let dir = user_data_dir();
        assert!(
            dir.ends_with(PRODUCT_NAME),
            "userData dir must end in the product name: {dir:?}"
        );
        // Must equal exactly what Electron resolves: %APPDATA%\hyperpanes.
        if let Some(appdata) = std::env::var_os("APPDATA") {
            assert_eq!(dir, Path::new(&appdata).join(PRODUCT_NAME));
        }
    }

    // On Windows and macOS every file lives in the single userData folder.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn canonical_file_paths_live_under_user_data_dir() {
        let dir = user_data_dir();
        for p in [
            control_json(),
            control_settings_json(),
            last_workspace_json(),
            projects_json(),
            ai_settings_json(),
            ai_memory_json(),
        ] {
            assert!(p.starts_with(&dir), "{p:?} should live under {dir:?}");
        }
        assert_eq!(control_json().file_name().unwrap(), "control.json");
        assert_eq!(
            control_settings_json().file_name().unwrap(),
            "control-settings.json"
        );
        assert_eq!(
            last_workspace_json().file_name().unwrap(),
            "last-workspace.json"
        );
        assert_eq!(projects_json().file_name().unwrap(), "projects.json");
        assert_eq!(ai_settings_json().file_name().unwrap(), "ai-settings.json");
        assert_eq!(ai_memory_json().file_name().unwrap(), "ai-memory.json");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_user_data_dir_is_application_support() {
        let dir = user_data_dir();
        assert!(
            dir.ends_with(Path::new("Library/Application Support").join(PRODUCT_NAME)),
            "expected ~/Library/Application Support/{PRODUCT_NAME}, got {dir:?}"
        );
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    mod linux {
        use super::super::*;

        #[test]
        fn absolute_env_value_wins() {
            let home = Path::new("/home/me");
            assert_eq!(
                pick_base(Some(PathBuf::from("/custom/cfg")), home, ".config"),
                PathBuf::from("/custom/cfg")
            );
        }

        #[test]
        fn empty_or_relative_env_value_falls_back_to_home() {
            let home = Path::new("/home/me");
            assert_eq!(
                pick_base(Some(PathBuf::from("")), home, ".config"),
                PathBuf::from("/home/me/.config")
            );
            // The XDG spec: a relative value must be ignored.
            assert_eq!(
                pick_base(Some(PathBuf::from("rel/cfg")), home, ".config"),
                PathBuf::from("/home/me/.config")
            );
            assert_eq!(
                pick_base(None, home, ".local/state"),
                PathBuf::from("/home/me/.local/state")
            );
        }

        // The live dirs honor the ambient env: compute the expectation from the same
        // env the code reads (no env mutation — tests run in parallel).
        fn expected(var: &str, rel: &str) -> PathBuf {
            pick_base(std::env::var_os(var).map(PathBuf::from), &home_dir(), rel).join(PRODUCT_NAME)
        }

        #[test]
        fn user_data_dir_is_the_xdg_config_dir() {
            // Electron parity: userData on Linux is $XDG_CONFIG_HOME/hyperpanes.
            assert_eq!(user_data_dir(), expected("XDG_CONFIG_HOME", ".config"));
            assert_eq!(config_dir(), user_data_dir());
        }

        #[test]
        fn data_and_state_dirs_are_xdg_correct() {
            assert_eq!(data_dir(), expected("XDG_DATA_HOME", ".local/share"));
            assert_eq!(state_dir(), expected("XDG_STATE_HOME", ".local/state"));
        }

        #[test]
        fn files_are_classified_config_data_state() {
            // Settings → config; durable data → data; session/runtime state → state.
            assert_eq!(
                control_settings_json(),
                config_dir().join("control-settings.json")
            );
            assert_eq!(ai_settings_json(), config_dir().join("ai-settings.json"));
            assert_eq!(projects_json(), data_dir().join("projects.json"));
            assert_eq!(ai_memory_json(), data_dir().join("ai-memory.json"));
            assert_eq!(control_json(), state_dir().join("control.json"));
            assert_eq!(
                last_workspace_json(),
                state_dir().join("last-workspace.json")
            );
        }
    }

    /// EMPIRICAL parity check (the userData-path trap in the risk register). The
    /// production Electron app writes `control.json` into `app.getPath('userData')`.
    /// If that file is present on this machine, our path resolution MUST point at it
    /// and it must parse as JSON — proving the Rust path resolution matches Electron's.
    #[test]
    fn reads_a_file_the_real_electron_app_wrote() {
        // Try each known app-written file; control.json is the MCP-critical one.
        let candidates = [
            control_json(),
            control_settings_json(),
            last_workspace_json(),
            projects_json(),
        ];
        let found = candidates.iter().find(|p| p.exists());
        let Some(path) = found else {
            eprintln!(
                "skipping empirical check: no app-written file present under {:?} \
                 (run the Electron app once to populate it)",
                user_data_dir()
            );
            return;
        };
        let raw = std::fs::read_to_string(path).expect("read the app-written file");
        let value: serde_json::Value =
            serde_json::from_str(&raw).expect("app-written file must be valid JSON");
        assert!(
            value.is_object() || value.is_array(),
            "expected a JSON object/array in {path:?}"
        );
    }

    #[test]
    fn write_atomic_creates_dirs_and_replaces() {
        let base = std::env::temp_dir().join(format!("hp-paths-test-{}-{}", std::process::id(), 1));
        let target = base.join("nested").join("file.json");
        write_atomic(&target, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        // Overwrite atomically.
        write_atomic(&target, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":2}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
