//! Linux apply step for the self-updater (see `mod.rs`). Owned by the Wave-1
//! `app-unix-shared` track.
//!
//! Linux is **NotifyOnly**: the shared check flow still finds and announces a newer
//! release ("vX available — you have vY"), but instead of downloading an installer the
//! General panel offers "View release", which opens the GitHub releases page with
//! `xdg-open` (via `core::paths::os_open`, wired in `app.rs`'s pref-action 19/20
//! handlers behind [`super::apply_strategy`]).
//
// Future self-update hook (don't wire `launch_installer` until one of these lands):
//  - AppImage: when `$APPIMAGE` is set, download the new .AppImage next to it, fsync,
//    rename over the old one, re-exec. (`appimageupdatetool` zsync delta is optional.)
//  - Distro packages (.deb/flatpak): never self-replace — stay NotifyOnly.

use super::ApplyStrategy;
use std::path::Path;

/// No in-app install on Linux yet — notify and point at the releases page.
pub const APPLY_STRATEGY: ApplyStrategy = ApplyStrategy::NotifyOnly;

/// Not supported while [`APPLY_STRATEGY`] is `NotifyOnly`; the UI never offers it (the
/// NotifyOnly branch opens the releases page instead), so this is a defensive backstop.
pub fn launch_installer(_path: &Path) -> Result<(), String> {
    Err("in-app install is not supported on Linux — get the new release from GitHub".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_is_notify_only_and_install_is_refused() {
        assert_eq!(APPLY_STRATEGY, ApplyStrategy::NotifyOnly);
        assert!(launch_installer(Path::new("/tmp/nope")).is_err());
    }
}
