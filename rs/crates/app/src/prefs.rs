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

/// Candidate monospace fonts offered in the font picker: a label + the font-file name to
/// look for. Each is resolved against the system **and** per-user font folders (see
/// [`font_dirs`]); only the ones actually installed are offered (see [`available_families`]),
/// so the picker grows with whatever monospace fonts the user has (Cascadia/Consolas ship
/// with Windows; JetBrains/Fira/Hack/etc. appear if installed). The glyph cache loads the
/// resolved `.ttf`/`.ttc` path. Selection is persisted by path, so adding/reordering this
/// list never invalidates a saved choice.
pub const FONT_CANDIDATES: [(&str, &str); 14] = [
    ("Cascadia Mono", "CascadiaMono.ttf"),
    ("Cascadia Code", "CascadiaCode.ttf"),
    ("Consolas", "consola.ttf"),
    ("Courier New", "cour.ttf"),
    ("Lucida Console", "lucon.ttf"),
    ("JetBrains Mono", "JetBrainsMono-Regular.ttf"),
    ("Fira Code", "FiraCode-Regular.ttf"),
    ("Hack", "Hack-Regular.ttf"),
    ("Source Code Pro", "SourceCodePro-Regular.ttf"),
    ("DejaVu Sans Mono", "DejaVuSansMono.ttf"),
    ("Iosevka", "Iosevka-Regular.ttf"),
    ("Roboto Mono", "RobotoMono-Regular.ttf"),
    ("IBM Plex Mono", "IBMPlexMono-Regular.ttf"),
    ("Cascadia Code NF", "CascadiaCodeNF.ttf"),
];

/// The fallback font path used when nothing else resolves (always present on Windows).
const FALLBACK_FONT: &str = "C:/Windows/Fonts/consola.ttf";

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

/// Resolve a saved font path to an actually-loadable one: the path itself if present, else
/// the first available family, else the always-present fallback. Shared by the live settings
/// and the in-dialog appearance draft so both highlight the same active font.
pub fn resolve_or_default(font: &str) -> String {
    if !font.is_empty() && std::path::Path::new(font).exists() {
        return font.to_string();
    }
    available_families()
        .into_iter()
        .next()
        .map(|(_, path)| path)
        .unwrap_or_else(|| FALLBACK_FONT.to_string())
}

/// The monospace fonts actually installed on this machine, as `(label, path)` pairs, in
/// [`FONT_CANDIDATES`] order. Always non-empty (falls back to Consolas) so the picker is
/// never blank.
pub fn available_families() -> Vec<(String, String)> {
    let present: Vec<(String, String)> = FONT_CANDIDATES
        .iter()
        .filter_map(|(label, file)| resolve_font(file).map(|p| (label.to_string(), p)))
        .collect();
    if present.is_empty() {
        vec![("Consolas".to_string(), FALLBACK_FONT.to_string())]
    } else {
        present
    }
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
    fn available_is_never_empty() {
        assert!(!available_families().is_empty());
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
