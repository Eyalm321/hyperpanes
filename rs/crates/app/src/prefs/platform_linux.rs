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

/// The fixed font-family choices offered in the picker (label + value). Unlike Windows,
/// distros scatter fonts across per-family subdirectories with no stable file names, so
/// the system entries are fontconfig **family names** (resolved via [`resolve_family`])
/// rather than file names; the two bundled OFL fonts keep their file-name values. The
/// families listed cover the common distro spreads (Noto = Fedora/openSUSE default,
/// Liberation = RHEL-family, DejaVu = Debian/Ubuntu default, Source Code Pro = popular
/// install); a missing family simply falls back when loaded.
pub const FONT_OPTIONS: [(&str, &str); 7] = [
    ("System default (monospace)", ""),
    ("Noto Sans Mono", "Noto Sans Mono"),
    ("DejaVu Sans Mono", "DejaVu Sans Mono"),
    ("Liberation Mono", "Liberation Mono"),
    ("Source Code Pro", "Source Code Pro"),
    // Fira Code + JetBrains Mono are baked in (see BUNDLED_FONTS), so they always render.
    ("Fira Code", "FiraCode-Regular.ttf"),
    ("JetBrains Mono", "JetBrainsMono-Regular.ttf"),
];

/// The path the empty "System default" value resolves to: whatever fontconfig aliases
/// `monospace` to (the distro/user default — the same font every other terminal shows),
/// else the bundled JetBrains Mono, else the conventional DejaVu path.
pub fn default_font() -> String {
    if let Some(p) = fc_query("fc-match", "monospace") {
        return p;
    }
    super::resolve_font("JetBrainsMono-Regular.ttf").unwrap_or_else(|| FALLBACK_FONT.to_string())
}

/// Resolve a fontconfig family name to an installed font file. `fc-list` (unlike
/// `fc-match`) returns only *actually installed* faces of the family — an uninstalled
/// pick must return `None` so the caller falls back, not silently substitute.
pub fn resolve_family(family: &str) -> Option<String> {
    fc_query("fc-list", family)
}

/// Shared fontconfig shell-out: the file of the best face for `pattern`. For `fc-list`
/// the Regular face is preferred over the (alphabetically first) Bold/Italic siblings.
fn fc_query(cmd: &str, pattern: &str) -> Option<String> {
    let out = std::process::Command::new(cmd)
        .args(["--format", "%{file}\\n", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let files: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let pick = files
        .iter()
        .find(|f| f.contains("Regular") || f.contains("regular"))
        .or_else(|| files.first())?;
    std::path::Path::new(pick).exists().then(|| pick.to_string())
}

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
    fn default_font_resolves_to_a_real_file_where_fontconfig_is_installed() {
        // Any desktop Linux (and CI ubuntu-latest) has fontconfig, so the empty "System
        // default" value must resolve via `fc-match monospace` to an actually-existing
        // file. Skipped quietly without fontconfig (the bundled fonts cover that at runtime).
        if std::process::Command::new("fc-match").arg("--version").output().is_ok() {
            let p = super::super::resolve_or_default("");
            assert!(std::path::Path::new(&p).exists(), "unresolved default font: {p}");
        }
    }

    #[test]
    fn resolve_family_finds_installed_families_only() {
        if std::process::Command::new("fc-list").arg("--version").output().is_err() {
            return; // no fontconfig on this box — runtime falls back to bundled fonts
        }
        // A family no distro ships must NOT resolve (fc-list, unlike fc-match, never
        // substitutes) — that silent substitution was the original "every pick looks
        // the same" bug.
        assert_eq!(resolve_family("No Such Font Family Xyzzy"), None);
        // DejaVu is present on every distro our gates run; when it is, the family
        // resolves to a real file even though it lives in a distro-specific subdir.
        if let Some(p) = resolve_family("DejaVu Sans Mono") {
            assert!(std::path::Path::new(&p).exists());
        }
    }
}
