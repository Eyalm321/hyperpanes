//! Linux `PlatformDefaults` provider stub (see `mod.rs` for the frozen surface). Owned by
//! the Wave-1 `app-unix-shared` track; these defaults are sane but minimal.

/// The default-shell choices offered in the Terminal section (label + spawn token; empty
/// = the system default resolved in core's spawn, i.e. `$SHELL`).
pub const SHELL_OPTIONS: [(&str, &str); 4] = [
    ("System", ""),
    ("bash", "bash"),
    ("zsh", "zsh"),
    ("fish", "fish"),
];

/// The fallback font path used when nothing else resolves. The bundled OFL fonts (extracted
/// at startup) are the real safety net; this path is the conventional DejaVu location.
pub const FALLBACK_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf";

/// The shell to prefer when the user picked "System": none — core resolves `$SHELL`.
pub fn preferred_shell() -> Option<String> {
    None
}

/// The directories scanned for the candidate font files: the system + per-user font
/// folders, and the baked-in font dir (so the shipped OFL fonts always resolve).
pub fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![
        std::path::PathBuf::from("/usr/share/fonts"),
        std::path::PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::Path::new(&home).join(".local").join("share").join("fonts"));
        dirs.push(std::path::Path::new(&home).join(".fonts"));
    }
    dirs.push(super::bundled_font_dir());
    dirs
}
