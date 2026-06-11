//! Windows apply step for the self-updater (see `mod.rs`): the silent NSIS install flow,
//! moved verbatim from the old single-file `update.rs`.

use super::ApplyStrategy;
use std::path::Path;

/// Windows applies updates by running the downloaded NSIS installer silently.
pub const APPLY_STRATEGY: ApplyStrategy = ApplyStrategy::SilentInstaller;

/// Launch the staged installer **silently** (NSIS `/S`) as a detached process. The caller
/// then quits the app so the installer can replace the files. Returns the spawn error string
/// on failure (so the panel can surface it instead of silently doing nothing).
pub fn launch_installer(path: &Path) -> Result<(), String> {
    std::process::Command::new(path)
        .arg("/S")
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
