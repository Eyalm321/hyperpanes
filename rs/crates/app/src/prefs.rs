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

/// The selectable terminal fonts: a label + the on-disk path the glyph cache loads.
/// Only the ones actually installed are offered (see [`available_families`]).
pub const FONT_FAMILIES: [(&str, &str); 3] = [
    ("Cascadia Mono", "C:/Windows/Fonts/CascadiaMono.ttf"),
    ("Cascadia Code", "C:/Windows/Fonts/CascadiaCode.ttf"),
    ("Consolas", "C:/Windows/Fonts/consola.ttf"),
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
    /// Index into [`FONT_FAMILIES`] for the active terminal font.
    pub font_family: usize,
    /// Base (logical px, pre-DPI-scale) terminal font size.
    pub font_px: f32,
    /// Whether each pane draws its colored frame border + header tint.
    pub show_frame: bool,
    /// Whether each pane shows its accent color dot in the header.
    pub show_dot: bool,
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
            font_family: 0,
            font_px: DEFAULT_FONT_PX,
            show_frame: true,
            show_dot: true,
            scrollback: 5000,
            show_sidebar: true,
        }
    }
}

impl Settings {
    /// The resolved font path for the active family, clamped to an installed font so a
    /// stale index never loads nothing.
    pub fn font_path(&self) -> &'static str {
        let avail = available_families();
        let idx = if avail.iter().any(|(i, _)| *i == self.font_family) {
            self.font_family
        } else {
            avail.first().map(|(i, _)| *i).unwrap_or(0)
        };
        FONT_FAMILIES[idx.min(FONT_FAMILIES.len() - 1)].1
    }

    /// Clamp the base font size into the supported range.
    pub fn clamp_font(px: f32) -> f32 {
        px.clamp(MIN_FONT_PX, MAX_FONT_PX)
    }
}

/// The font families that actually exist on this machine, as `(index, label)` pairs.
/// Always non-empty (Consolas ships with Windows); falls back to all entries if none
/// resolve (e.g. a non-standard install) so the picker is never blank.
pub fn available_families() -> Vec<(usize, &'static str)> {
    let present: Vec<(usize, &'static str)> = FONT_FAMILIES
        .iter()
        .enumerate()
        .filter(|(_, (_, path))| std::path::Path::new(path).exists())
        .map(|(i, (label, _))| (i, *label))
        .collect();
    if present.is_empty() {
        FONT_FAMILIES.iter().enumerate().map(|(i, (l, _))| (i, *l)).collect()
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
