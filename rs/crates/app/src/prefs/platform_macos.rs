//! macOS `PlatformDefaults` provider (see `mod.rs` for the frozen surface). Owned by the
//! Wave-1 `app-unix-shared` track.

/// The default-shell choices offered in the Terminal section (label + spawn token; empty
/// = the system default resolved via [`preferred_shell`]). zsh first after System — the
/// macOS login-shell default since Catalina. fish is listed even though it's a Homebrew
/// install; a missing shell simply fails to spawn with a visible error.
pub const SHELL_OPTIONS: [(&str, &str); 4] = [
    ("System", ""),
    ("zsh", "zsh"),
    ("bash", "bash"),
    ("fish", "fish"),
];

/// The fallback font path used when nothing else resolves. Monaco ships as a plain `.ttf`
/// on modern macOS (verified on the build mini) — preferred over `Menlo.ttc` because the
/// shared prefs contract resolves to `.ttf` paths; swash loads `.ttc` collections too
/// (index 0) if a user picks one as a custom font.
pub const FALLBACK_FONT: &str = "/System/Library/Fonts/Monaco.ttf";

/// The shell to prefer when the user picked "System": the login shell from `$SHELL` when
/// it's set and present, else `/bin/zsh` — the macOS default login shell, always present.
pub fn preferred_shell() -> Option<String> {
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.is_empty() && std::path::Path::new(&shell).exists() {
            return Some(shell);
        }
    }
    Some("/bin/zsh".to_string())
}

/// The directories scanned for the candidate font files: the per-user folder first (a
/// user-installed font wins), the local and system font libraries, and the baked-in font
/// dir last (so the shipped OFL fonts always resolve).
pub fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::Path::new(&home).join("Library").join("Fonts"));
    }
    dirs.push(std::path::PathBuf::from("/Library/Fonts"));
    dirs.push(std::path::PathBuf::from("/System/Library/Fonts"));
    dirs.push(super::bundled_font_dir());
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_options_offer_system_first() {
        assert_eq!(SHELL_OPTIONS[0], ("System", ""));
        for (_, tok) in &SHELL_OPTIONS[1..] {
            assert!(!tok.is_empty() && !tok.contains('/'));
        }
    }

    #[test]
    fn font_dirs_end_with_the_bundled_dir() {
        let dirs = font_dirs();
        assert_eq!(dirs.last(), Some(&super::super::bundled_font_dir()));
        assert!(dirs.iter().any(|d| d == std::path::Path::new("/System/Library/Fonts")));
    }

    #[test]
    fn preferred_shell_always_resolves_on_macos() {
        // $SHELL or the /bin/zsh default — never None on this platform.
        let s = preferred_shell().expect("macOS always has a system shell");
        assert!(std::path::Path::new(&s).exists());
    }

    #[test]
    fn fallback_font_exists_and_is_a_ttf() {
        // the frozen prefs::tests contract expects font_path() to end in .ttf
        assert!(FALLBACK_FONT.ends_with(".ttf"));
        assert!(std::path::Path::new(FALLBACK_FONT).exists());
    }
}
