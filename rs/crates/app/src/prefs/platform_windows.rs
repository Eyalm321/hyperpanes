//! Windows `PlatformDefaults` provider (see `mod.rs`): the shell picker list, the
//! preferred-system-shell probe, the font directories, and the always-present fallback
//! font. Moved verbatim from the old single-file `prefs.rs`.

/// The default-shell choices offered in the Terminal section: a label + the shell token
/// passed to `SpawnOptions::shell` (empty = the system default resolved in core's spawn).
/// The native port of the renderer's `ShellPicker` options (kept to the common Windows
/// shells; an unlisted shell still works via the persisted string, this is just the picker).
pub const SHELL_OPTIONS: [(&str, &str); 4] = [
    ("System", ""),
    ("pwsh", "pwsh"),
    ("PowerShell", "powershell"),
    ("cmd", "cmd"),
];

/// The fallback font path used when nothing else resolves (always present on Windows).
pub const FALLBACK_FONT: &str = "C:/Windows/Fonts/consola.ttf";

/// The shell to prefer when the user picked "System" (empty token): **pwsh** (PowerShell 7)
/// when it's available, else `None` to let core pick the OS default. Mirrors the renderer's
/// "use pwsh if installed" default.
pub fn preferred_shell() -> Option<String> {
    pwsh_available().then(|| "pwsh".to_string())
}

/// Whether `pwsh.exe` (PowerShell 7+) resolves — its canonical install dir, then `PATH`.
fn pwsh_available() -> bool {
    if std::path::Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe").exists() {
        return true;
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join("pwsh.exe").exists()))
        .unwrap_or(false)
}

/// The directories scanned for the candidate font files: the system font folder, the per-user
/// font folder (where user-installed fonts land on modern Windows), and the baked-in font dir
/// (so the shipped OFL fonts always resolve even when not installed).
pub fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![std::path::PathBuf::from("C:/Windows/Fonts")];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(std::path::Path::new(&local).join("Microsoft").join("Windows").join("Fonts"));
    }
    dirs.push(super::bundled_font_dir());
    dirs
}
