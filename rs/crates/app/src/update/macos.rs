//! macOS apply step for the self-updater (see `mod.rs`). Owned by the Wave-1
//! `app-unix-shared` track.
//!
//! macOS is **NotifyOnly**: the shared check flow still finds and announces a newer
//! release ("vX available — you have vY"), but instead of downloading an installer the
//! General panel offers "View release", which opens the GitHub releases page with
//! `open` (via `core::paths::os_open`, wired in `app.rs`'s pref-action 19/20 handlers
//! behind [`super::apply_strategy`]).
//
// Future self-update hook (don't wire `launch_installer` until it lands):
//  - .app swap: download the .dmg/.zip, verify, swap the bundle next to the running one
//    and relaunch (Sparkle-style). Needs codesigning + notarization first — an unsigned
//    swapped bundle would trip Gatekeeper harder than a fresh download does.

use super::ApplyStrategy;
use std::path::Path;

/// No in-app install on macOS yet — notify and point at the releases page.
pub const APPLY_STRATEGY: ApplyStrategy = ApplyStrategy::NotifyOnly;

/// Not supported while [`APPLY_STRATEGY`] is `NotifyOnly`; the UI never offers it (the
/// NotifyOnly branch opens the releases page instead), so this is a defensive backstop.
pub fn launch_installer(_path: &Path) -> Result<(), String> {
    Err("in-app install is not supported on macOS — get the new release from GitHub".to_string())
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
