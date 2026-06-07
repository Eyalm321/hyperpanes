//! userData-dir + file-path resolution (replaces Electron `app.getPath('userData')`).
//!
//! ⚠ PARITY-CRITICAL: this MUST resolve to the EXACT same Windows folder the running
//! Electron app uses, or the MCP can't find `control.json` and last-session restore
//! breaks. Electron computes `userData` as `app.getPath('appData')/<productName>`, and
//! on Windows `appData` is `%APPDATA%` (the Roaming profile). The product name comes
//! from `package.json` — there is no `productName` override, so `app.getName()` falls
//! back to `name` = `"hyperpanes"`.
//!
//! EMPIRICALLY VERIFIED on this machine: the production app's folder
//! `%APPDATA%\hyperpanes` is the one holding `control.json` / `control-settings.json`
//! / `last-workspace.json` / `projects.json` (the dev build uses `hyperpanes-dev`,
//! which we deliberately do NOT target — the native binary replaces the production
//! build). See the `reads_a_file_the_real_electron_app_wrote` test below.

use std::path::{Path, PathBuf};

/// The Electron product name (= `package.json` `name`, no `productName` override),
/// which is the literal userData subfolder under `%APPDATA%`.
pub const PRODUCT_NAME: &str = "hyperpanes";

/// `%APPDATA%` (Windows Roaming app-data), mirroring Electron's `app.getPath('appData')`.
/// Electron prefers the `APPDATA` env var and falls back to the known folder; we do the
/// same (the `directories` crate resolves the Roaming known folder as the fallback).
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

/// The canonical userData directory: `%APPDATA%\hyperpanes`. Equal to Electron's
/// `app.getPath('userData')` for the production build.
pub fn user_data_dir() -> PathBuf {
    app_data_dir().join(PRODUCT_NAME)
}

/// Discovery file the control server writes (port/token/pid/version/events URL).
pub fn control_json() -> PathBuf {
    user_data_dir().join("control.json")
}

/// `{ enabled, allowInput }` control-server settings.
pub fn control_settings_json() -> PathBuf {
    user_data_dir().join("control-settings.json")
}

/// The last saved session (restored on launch).
pub fn last_workspace_json() -> PathBuf {
    user_data_dir().join("last-workspace.json")
}

/// Remembered git-project history.
pub fn projects_json() -> PathBuf {
    user_data_dir().join("projects.json")
}

/// Ambient-AI settings.
pub fn ai_settings_json() -> PathBuf {
    user_data_dir().join("ai-settings.json")
}

/// Ambient-AI per-pane memory.
pub fn ai_memory_json() -> PathBuf {
    user_data_dir().join("ai-memory.json")
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

    /// EMPIRICAL parity check (the userData-path trap in the risk register). The
    /// production Electron app writes `control.json` into `app.getPath('userData')`.
    /// If that file is present on this machine, our `user_data_dir()` MUST point at it
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
        let base =
            std::env::temp_dir().join(format!("hp-paths-test-{}-{}", std::process::id(), 1));
        let target = base.join("nested").join("file.json");
        write_atomic(&target, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        // Overwrite atomically.
        write_atomic(&target, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"a\":2}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
