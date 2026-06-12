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

/// The fixed font-family choices offered in the picker (label + value). macOS keeps
/// flat font directories with stable file names, so values are file names resolved
/// against [`font_dirs`] like Windows (no fontconfig here). Menlo ships as a `.ttc`
/// collection — swash loads index 0 (the Regular face).
pub const FONT_OPTIONS: [(&str, &str); 7] = [
    ("System default (Monaco)", ""),
    ("Menlo", "Menlo.ttc"),
    ("Monaco", "Monaco.ttf"),
    ("SF Mono", "SFNSMono.ttf"),
    ("Courier New", "Courier New.ttf"),
    // Fira Code + JetBrains Mono are baked in (see BUNDLED_FONTS), so they always render.
    ("Fira Code", "FiraCode-Regular.ttf"),
    ("JetBrains Mono", "JetBrainsMono-Regular.ttf"),
];

/// The path the empty "System default" value resolves to: Monaco (always installed).
pub fn default_font() -> String {
    super::resolve_font("Monaco.ttf").unwrap_or_else(|| FALLBACK_FONT.to_string())
}

/// Family-name resolution beyond the file-name join — not needed on macOS, where the
/// picker values are real file names under the system font libraries.
pub fn resolve_family(_family: &str) -> Option<String> {
    None
}

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
    // Where the classic cross-platform fonts (Courier New, etc.) live on modern macOS.
    dirs.push(std::path::PathBuf::from("/System/Library/Fonts/Supplemental"));
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

    #[test]
    fn default_font_resolves_to_a_real_file() {
        // Monaco always ships with macOS, so the empty "System default" value must
        // resolve to an actually-existing file on any Mac.
        let p = super::super::resolve_or_default("");
        assert!(std::path::Path::new(&p).exists(), "unresolved default font: {p}");
    }
}
