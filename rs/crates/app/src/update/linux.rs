//! Linux apply step stub for the self-updater (see `mod.rs`). Owned by the Wave-1
//! `app-unix-shared` track: until a native flow ships, the platform is NotifyOnly —
//! surface "update available" and open the releases page; never run an installer.

use super::ApplyStrategy;
use std::path::Path;

/// No in-app install on Linux yet — notify and point at the releases page.
pub const APPLY_STRATEGY: ApplyStrategy = ApplyStrategy::NotifyOnly;

/// Stub: in-app install is not supported on this platform (the UI should not offer it
/// while [`APPLY_STRATEGY`] is `NotifyOnly`). The T4 track fills the real flow.
pub fn launch_installer(_path: &Path) -> Result<(), String> {
    Err("in-app install is not supported on this platform yet".to_string())
}
