//! Linux `PlatformDefaults` provider (see `mod.rs` for the frozen surface). Owned by the
//! Wave-1 `app-unix-shared` track.

/// The default-shell choices offered in the Terminal section (label + spawn token; empty
/// = the system default resolved via [`preferred_shell`], i.e. `$SHELL`). Bare names —
/// core's spawn resolves them on `PATH`; an unlisted shell still works via the persisted
/// string, this is just the picker.
pub const SHELL_OPTIONS: [(&str, &str); 4] = [
    ("System", ""),
    ("bash", "bash"),
    ("zsh", "zsh"),
    ("fish", "fish"),
];

/// The fallback font path used when nothing else resolves: the conventional Debian/Ubuntu
/// DejaVu location. Not guaranteed on every distro — the bundled OFL fonts (extracted at
/// startup, last in [`font_dirs`]) are the true safety net, and `theme::load_font_at`
/// falls through to them when this path is missing too.
pub const FALLBACK_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf";

/// The shell to prefer when the user picked "System": the login shell from `$SHELL` when
/// it's set and actually present on disk, else `None` to let core pick the OS default.
pub fn preferred_shell() -> Option<String> {
    let shell = std::env::var("SHELL").ok()?;
    (!shell.is_empty() && std::path::Path::new(&shell).exists()).then_some(shell)
}

/// The directories scanned for the candidate font files. Per-user folders first (a
/// user-installed font wins), then the fontconfig-conventional system folders, and the
/// baked-in font dir last (so the shipped OFL fonts always resolve). The shared
/// `resolve_font` joins `dir/file` without recursing, so the common monospace subdirs
/// (Debian/Ubuntu `truetype/dejavu`, Arch `TTF`) are listed explicitly.
pub fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::Path::new(&home);
        dirs.push(home.join(".local").join("share").join("fonts"));
        dirs.push(home.join(".fonts"));
    }
    dirs.extend(
        [
            "/usr/share/fonts",
            "/usr/share/fonts/truetype",
            "/usr/share/fonts/truetype/dejavu",
            "/usr/share/fonts/TTF",
            "/usr/local/share/fonts",
        ]
        .map(std::path::PathBuf::from),
    );
    dirs.push(super::bundled_font_dir());
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_options_offer_system_first() {
        assert_eq!(SHELL_OPTIONS[0], ("System", ""));
        assert!(SHELL_OPTIONS.len() >= 2);
        // every non-System token is a bare name core can resolve on PATH
        for (_, tok) in &SHELL_OPTIONS[1..] {
            assert!(!tok.is_empty() && !tok.contains('/'));
        }
    }

    #[test]
    fn font_dirs_end_with_the_bundled_dir() {
        let dirs = font_dirs();
        assert_eq!(dirs.last(), Some(&super::super::bundled_font_dir()));
        assert!(dirs.iter().any(|d| d == std::path::Path::new("/usr/share/fonts")));
    }

    #[test]
    fn preferred_shell_is_the_existing_login_shell() {
        // $SHELL is set to a real shell on any sane Linux box running this suite.
        if let Some(s) = preferred_shell() {
            assert!(std::path::Path::new(&s).exists());
        }
    }

    #[test]
    fn fallback_font_is_a_ttf_path() {
        // the frozen prefs::tests contract expects font_path() to end in .ttf
        assert!(FALLBACK_FONT.ends_with(".ttf"));
    }

    #[test]
    fn default_font_resolves_to_a_real_file_where_dejavu_is_installed() {
        // On the distros our gates run (WSL Ubuntu, CI ubuntu-latest) DejaVu is present,
        // so the empty "System default" value must resolve to an actually-existing file.
        // Skipped quietly on a distro without it (the bundled fonts cover those at runtime).
        if std::path::Path::new(FALLBACK_FONT).exists() {
            let p = super::super::resolve_or_default("");
            assert!(std::path::Path::new(&p).exists(), "unresolved default font: {p}");
        }
    }
}
