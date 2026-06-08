//! Preferences — Wave-2 feature plugging into **Seam #1** (mutate-then-resync state).
//!
//! A small persisted [`Settings`] blob (the native port of `store/useSettings.ts`,
//! MVP subset) plus the font-family table the terminal font is loaded from. Settings
//! live on the central [`crate::state::State`]; a change mutates them, persists the
//! blob, and flips `dirty` (font changes also flag a reload) so the next resync
//! re-projects them — the same contract every workspace mutation uses.
//!
//! Persisted to `%APPDATA%\hyperpanes\native-settings.json` via
//! `core::persistence::paths` (atomic write), distinct from the Electron build's
//! localStorage blob so the two never fight over a file.

use hyperpanes_core::persistence::paths;
use serde::{Deserialize, Serialize};

/// The fixed font-family choices offered in the picker — a 1:1 mirror of the renderer's
/// `FONT_OPTIONS` (label + value): the empty value is the built-in default; every other
/// value is the font-file name resolved against the system/per-user font folders (see
/// [`font_dirs`]). Shown as a fixed list (not filtered by what's installed) so it matches
/// the Electron dropdown exactly; a missing font simply falls back when loaded. A "Custom…"
/// entry (handled in the UI) lets the user type any font-file path. Selection is persisted
/// by value.
pub const FONT_OPTIONS: [(&str, &str); 8] = [
    ("System default (Consolas)", ""),
    ("Cascadia Code", "CascadiaCode.ttf"),
    ("Cascadia Mono", "CascadiaMono.ttf"),
    ("Consolas", "consola.ttf"),
    ("Courier New", "cour.ttf"),
    ("Fira Code", "FiraCode-Regular.ttf"),
    ("JetBrains Mono", "JetBrainsMono-Regular.ttf"),
    ("Menlo", "Menlo.ttc"), // macOS font; absent on Windows → falls back (parity entry)
];

/// The fallback font path used when nothing else resolves (always present on Windows).
const FALLBACK_FONT: &str = "C:/Windows/Fonts/consola.ttf";

/// Whether `font` is a user-typed custom value (non-empty and not one of [`FONT_OPTIONS`]).
pub fn is_custom_font(font: &str) -> bool {
    !font.is_empty() && !FONT_OPTIONS.iter().any(|(_, v)| *v == font)
}

/// The directories scanned for the candidate font files: the system font folder plus the
/// per-user font folder (where user-installed fonts land on modern Windows).
fn font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![std::path::PathBuf::from("C:/Windows/Fonts")];
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(std::path::Path::new(&local).join("Microsoft").join("Windows").join("Fonts"));
    }
    dirs
}

/// Resolve a candidate font-file name to an installed absolute path (forward-slashed), or
/// `None` if it isn't present in any font directory.
fn resolve_font(file: &str) -> Option<String> {
    font_dirs().into_iter().find_map(|d| {
        let p = d.join(file);
        p.exists().then(|| p.to_string_lossy().replace('\\', "/"))
    })
}

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

/// Base (un-scaled) terminal font size bounds, mirroring `useSettings`' clamps.
pub const MIN_FONT_PX: f32 = 8.0;
pub const MAX_FONT_PX: f32 = 32.0;
pub const DEFAULT_FONT_PX: f32 = 14.0;

/// Persisted app-wide preferences (native MVP subset of the renderer `Settings`).
/// Every field has a sensible default so an older/partial blob never breaks load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Absolute path of the active terminal font file ("" = the first available default).
    /// Persisted by path so the picker list can grow/reorder without invalidating it.
    pub font_family: String,
    /// Index into [`crate::theme::FRAME_PALETTES`] for the active pane dot/frame palette.
    /// Switching it remaps panes by creation slot (the native port of `framePalette`).
    pub frame_palette: usize,
    /// Default shell for new panes (the token from [`SHELL_OPTIONS`], e.g. "pwsh"). Empty
    /// = the system default. Mirrors the renderer `Settings.defaultShell`.
    pub default_shell: String,
    /// Base (logical px, pre-DPI-scale) terminal font size.
    pub font_px: f32,
    /// Whether each pane draws its colored frame border + header tint.
    pub show_frame: bool,
    /// Whether each pane shows its accent color dot in the header.
    pub show_dot: bool,
    /// Whether file paths in terminal output are clickable (plain click opens, Ctrl+click
    /// copies the resolved absolute path). Mirrors `Settings.clickablePaths`.
    pub clickable_paths: bool,
    /// Command template used to open a clicked path ("" = auto-detect VS Code, else the OS
    /// default handler). Placeholders: `{path}` `{line}` `{col}`. Mirrors `editorCommand`.
    pub editor_command: String,
    /// Per-pane scrollback (history lines). Persisted for forward-compat with the
    /// renderer blob; the native terminal grid currently keeps a fixed buffer.
    pub scrollback: u32,
    /// Whether the right-edge sidebar rail (quick-pane + git-projects history) is
    /// shown. Hidden in fullscreen regardless of this. Mirrors `useSettings.showSidebar`.
    pub show_sidebar: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font_family: String::new(),
            frame_palette: 0,
            default_shell: String::new(),
            font_px: DEFAULT_FONT_PX,
            show_frame: true,
            show_dot: true,
            clickable_paths: true,
            editor_command: String::new(),
            scrollback: 5000,
            show_sidebar: true,
        }
    }
}

impl Settings {
    /// The resolved font path to load: the saved `font_family` path if it's still present,
    /// else the first available family, else the always-present fallback. So a font that was
    /// uninstalled (or a blank default) never loads nothing.
    pub fn font_path(&self) -> String {
        resolve_or_default(&self.font_family)
    }

    /// Clamp the base font size into the supported range.
    pub fn clamp_font(px: f32) -> f32 {
        px.clamp(MIN_FONT_PX, MAX_FONT_PX)
    }
}

/// Resolve a saved font value to an actually-loadable `.ttf`/`.ttc` path. Handles the three
/// value shapes: empty (the default → Consolas/fallback), a bare font-file name from
/// [`FONT_OPTIONS`] (looked up in the font folders), or a custom absolute path. Anything that
/// can't be found falls back to the always-present Consolas, so loading never fails. Shared by
/// the live settings and the in-dialog appearance draft so both highlight the same font.
pub fn resolve_or_default(font: &str) -> String {
    if font.is_empty() {
        return resolve_font("consola.ttf").unwrap_or_else(|| FALLBACK_FONT.to_string());
    }
    // A custom absolute path (contains a separator) is used verbatim when it exists.
    if (font.contains('/') || font.contains('\\')) && std::path::Path::new(font).exists() {
        return font.replace('\\', "/");
    }
    // Otherwise treat it as a font-file name and look it up in the font folders.
    resolve_font(font)
        .or_else(|| resolve_font("consola.ttf"))
        .unwrap_or_else(|| FALLBACK_FONT.to_string())
}

/// Load the persisted settings (defaults on a missing/corrupt file).
pub fn load() -> Settings {
    let path = paths::user_data_dir().join("native-settings.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    serde_json::from_str::<Settings>(&raw).unwrap_or_default()
}

/// Persist `settings` atomically. Errors are swallowed (a settings write failing must
/// never take down the UI) but logged via the debug log.
pub fn save(settings: &Settings) {
    let path = paths::user_data_dir().join("native-settings.json");
    match serde_json::to_string_pretty(settings) {
        Ok(json) => {
            if let Err(e) = paths::write_atomic(&path, json.as_bytes()) {
                crate::dbg_log(&format!("settings save failed: {e}"));
            }
        }
        Err(e) => crate::dbg_log(&format!("settings serialize failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let s = Settings::default();
        assert!(s.show_frame && s.show_dot);
        assert_eq!(s.font_px, DEFAULT_FONT_PX);
        // font_path resolves to an installed font path string.
        assert!(s.font_path().ends_with(".ttf"));
    }

    #[test]
    fn clamp_keeps_in_range() {
        assert_eq!(Settings::clamp_font(2.0), MIN_FONT_PX);
        assert_eq!(Settings::clamp_font(99.0), MAX_FONT_PX);
        assert_eq!(Settings::clamp_font(15.0), 15.0);
    }

    #[test]
    fn font_options_present_and_resolve() {
        // The fixed list mirrors the renderer (System default first) and every value
        // resolves to a loadable .ttf/.ttc (missing fonts fall back to Consolas).
        assert_eq!(FONT_OPTIONS[0].1, "");
        assert!(FONT_OPTIONS.len() >= 8);
        for (_, value) in FONT_OPTIONS {
            let p = resolve_or_default(value);
            assert!(p.ends_with(".ttf") || p.ends_with(".ttc"), "unresolved: {value} -> {p}");
        }
    }

    #[test]
    fn custom_font_detection() {
        assert!(!is_custom_font(""));
        assert!(!is_custom_font("consola.ttf")); // a preset value
        assert!(is_custom_font("C:/Fonts/MyFont.ttf"));
    }

    #[test]
    fn partial_blob_fills_defaults() {
        // A blob missing fields should still parse (serde default) — simulate by
        // round-tripping a minimal object.
        let s: Settings = serde_json::from_str("{\"fontPx\": 18.0}").unwrap();
        assert_eq!(s.font_px, 18.0);
        assert!(s.show_frame); // defaulted
    }
}
