//! macOS `PlatformDefaults` provider stub (see `mod.rs` for the frozen surface). Owned by
//! the Wave-1 `app-unix-shared` track; these defaults are sane but minimal.

/// The default-shell choices offered in the Terminal section (label + spawn token; empty
/// = the system default resolved in core's spawn, i.e. `$SHELL`).
pub const SHELL_OPTIONS: [(&str, &str); 4] = [
    ("System", ""),
    ("zsh", "zsh"),
    ("bash", "bash"),
    ("fish", "fish"),
];

/// The fallback font path used when nothing else resolves (Menlo ships with macOS).
pub const FALLBACK_FONT: &str = "/System/Library/Fonts/Menlo.ttc";

/// The shell to prefer when the user picked "System": none — core resolves `$SHELL`.
pub fn preferred_shell() -> Option<String> {
    None
}

/// The directories scanned for the candidate font files: the system + per-user font
/// folders, and the baked-in font dir (so the shipped OFL fonts always resolve).
pub fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![
        std::path::PathBuf::from("/System/Library/Fonts"),
        std::path::PathBuf::from("/Library/Fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::Path::new(&home).join("Library").join("Fonts"));
    }
    dirs.push(super::bundled_font_dir());
    dirs
}
