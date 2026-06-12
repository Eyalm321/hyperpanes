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

// The per-platform `PlatformDefaults` provider: the shell-picker list (`SHELL_OPTIONS`),
// the preferred-system-shell probe (`preferred_shell`), the font picker list
// (`FONT_OPTIONS`), the font directories (`font_dirs`), the default-font and
// family-name resolvers (`default_font` / `resolve_family`), and the always-present
// fallback font path (`FALLBACK_FONT`). One cfg-selected module per OS; the surface is
// frozen in `docs/ports-seams.md`.
#[cfg(windows)]
#[path = "platform_windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "platform_macos.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "platform_linux.rs"]
mod platform;

pub use platform::{font_dirs, FONT_OPTIONS, SHELL_OPTIONS};

// The fixed font-family choices offered in the picker (`FONT_OPTIONS`) live in the
// platform provider now: each OS offers fonts that actually exist there (the old shared
// list was Windows file names, which never resolved on Linux/macOS — every pick silently
// fell back to the same bundled font). The shape is shared: (label, value) pairs, the
// empty value = the platform default, the rest resolved by [`resolve_or_default`]
// (file-name join, then a platform family lookup). A "Custom…" entry (handled in the UI)
// lets the user type any font-file path. Selection is persisted by value.

/// Whether `font` is a user-typed custom value (non-empty and not one of [`FONT_OPTIONS`]).
pub fn is_custom_font(font: &str) -> bool {
    !font.is_empty() && !FONT_OPTIONS.iter().any(|(_, v)| *v == font)
}

/// The human label for a saved font value — the matching [`FONT_OPTIONS`] label, "Custom"
/// for a user-typed path, else the default. Used by the preview HUD.
pub fn font_label(font: &str) -> &str {
    if let Some((label, _)) = FONT_OPTIONS.iter().find(|(_, v)| *v == font) {
        label
    } else if is_custom_font(font) {
        "Custom"
    } else {
        FONT_OPTIONS[0].0
    }
}

/// Fonts shipped with hyperpanes (OFL 1.1, baked into the binary) so they're always
/// available regardless of what the user has installed. Extracted to [`bundled_font_dir`]
/// on startup (see [`init_bundled_fonts`]); their file names match the [`FONT_OPTIONS`]
/// values so the picker resolves them. Licenses live in `assets/fonts/*-OFL.txt`.
pub const BUNDLED_FONTS: [(&str, &[u8]); 2] = [
    ("FiraCode-Regular.ttf", include_bytes!("../../assets/fonts/FiraCode-Regular.ttf")),
    ("JetBrainsMono-Regular.ttf", include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf")),
];

/// Where the baked-in fonts are extracted: `%APPDATA%\hyperpanes\fonts`.
pub fn bundled_font_dir() -> std::path::PathBuf {
    paths::user_data_dir().join("fonts")
}

/// Extract the baked-in fonts to [`bundled_font_dir`] (writing each only when missing or a
/// different size, so an app update refreshes them). Best-effort; call once at startup before
/// any font is resolved. A failure just means those fonts fall back like an uninstalled one.
pub fn init_bundled_fonts() {
    let dir = bundled_font_dir();
    let _ = std::fs::create_dir_all(&dir);
    for (name, bytes) in BUNDLED_FONTS {
        let p = dir.join(name);
        let stale = std::fs::metadata(&p).map(|m| m.len() as usize != bytes.len()).unwrap_or(true);
        if stale {
            let _ = std::fs::write(&p, bytes);
        }
    }
}

/// Resolve a candidate font-file name to an installed absolute path (forward-slashed), or
/// `None` if it isn't present in any font directory.
fn resolve_font(file: &str) -> Option<String> {
    font_dirs().into_iter().find_map(|d| {
        let p = d.join(file);
        p.exists().then(|| p.to_string_lossy().replace('\\', "/"))
    })
}

/// Resolve the shell token to spawn for a new pane. An explicit pick (`default_shell`) is
/// used verbatim; the empty "System" default asks the platform provider for its preferred
/// shell (Windows: pwsh when installed), falling back to the OS default that core resolves.
/// Returns `None` to mean "let core pick the system shell".
pub fn effective_shell(default_shell: &str) -> Option<String> {
    if !default_shell.is_empty() {
        return Some(default_shell.to_string());
    }
    platform::preferred_shell()
}

/// Base (un-scaled) terminal font size bounds, mirroring `useSettings`' clamps.
pub const MIN_FONT_PX: f32 = 8.0;
pub const MAX_FONT_PX: f32 = 32.0;
pub const DEFAULT_FONT_PX: f32 = 14.0;

/// Idle-alert threshold bounds (seconds a pane must stay output-quiet before it glows).
/// The dial steps in [`IDLE_STEP_SECONDS`] jumps, so the bounds are whole multiples of it.
pub const MIN_IDLE_SECONDS: u32 = 30;
pub const MAX_IDLE_SECONDS: u32 = 1800;
pub const DEFAULT_IDLE_SECONDS: u32 = 30;
/// The ± step (seconds) the "Idle after" dial moves by.
pub const IDLE_STEP_SECONDS: u32 = 30;

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
    /// Index into [`crate::theme::TERMINAL_THEMES`] for the active terminal colour theme
    /// (the terminal's own bg/fg + 16 ANSI colours). Mirrors `terminalTheme`.
    pub terminal_theme: usize,
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
    /// Whether a pane softly glows its frame once its agent/shell has gone output-quiet
    /// for [`Self::idle_alert_seconds`] (the AI-pane quiescence glow). Mirrors `idleAlert`.
    pub idle_alert: bool,
    /// The active glow style token (firefly / pulse / blink / solid). Stored by name so
    /// the list can grow without invalidating the blob. Mirrors `idleEffect`.
    pub idle_effect: String,
    /// How long a pane must stay output-quiet before it glows, in seconds (clamped to
    /// [`MIN_IDLE_SECONDS`]..=[`MAX_IDLE_SECONDS`]). Mirrors `idleAlertSeconds`.
    pub idle_alert_seconds: u32,
    /// Whether the app does a quiet GitHub-releases check on startup (Task 8). Off by default
    /// — when on, an available update surfaces a hint in Preferences → General; it never
    /// downloads or installs without consent, and an offline check is silently skipped.
    pub auto_update: bool,
    /// Whether finishing a drag-selection copies it to the clipboard immediately (the PuTTY/
    /// X11-style behavior). OFF by default, matching Windows Terminal: selecting only
    /// highlights, so an external copy survives "select the target, paste over it", and the
    /// body right-click is modal (copy the selection if one exists, else paste). When ON,
    /// right-click always pastes — the selection was already copied on release.
    pub copy_on_select: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font_family: String::new(),
            frame_palette: 0,
            terminal_theme: 0,
            default_shell: String::new(),
            font_px: DEFAULT_FONT_PX,
            show_frame: true,
            show_dot: true,
            clickable_paths: true,
            editor_command: String::new(),
            scrollback: 5000,
            show_sidebar: true,
            idle_alert: true,
            idle_effect: String::from("firefly"),
            idle_alert_seconds: DEFAULT_IDLE_SECONDS,
            auto_update: false,
            copy_on_select: false,
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

/// Resolve a saved font value to an actually-loadable font path. Handles the value shapes:
/// empty (→ the platform default via `default_font`), a [`FONT_OPTIONS`] value — a font-file
/// name looked up in the font folders, or (Linux) a fontconfig family name — or a custom
/// absolute path. Anything that can't be found falls back to the platform default, so
/// loading never fails. Shared by the live settings and the in-dialog appearance draft so
/// both highlight the same font.
pub fn resolve_or_default(font: &str) -> String {
    if font.is_empty() {
        return platform::default_font();
    }
    // A custom absolute path (contains a separator) is used verbatim when it exists.
    if (font.contains('/') || font.contains('\\')) && std::path::Path::new(font).exists() {
        return font.replace('\\', "/");
    }
    // Otherwise a font-file name in the font folders, then a platform family lookup.
    resolve_font(font)
        .or_else(|| platform::resolve_family(font))
        .unwrap_or_else(platform::default_font)
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
        // font_path resolves to an installed font path string (.ttc covers a
        // possible Menlo.ttc default on macOS, .otf an OpenType fc-match on Linux).
        let p = s.font_path();
        assert!(p.ends_with(".ttf") || p.ends_with(".ttc") || p.ends_with(".otf"));
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
        // resolves to a loadable font file (missing fonts fall back to the platform
        // default). `.otf` joins the accepted set for Linux families that ship OpenType
        // (e.g. Source Code Pro on Fedora).
        assert_eq!(FONT_OPTIONS[0].1, "");
        assert!(FONT_OPTIONS.len() >= 7);
        for (_, value) in FONT_OPTIONS {
            let p = resolve_or_default(value);
            assert!(
                p.ends_with(".ttf") || p.ends_with(".ttc") || p.ends_with(".otf"),
                "unresolved: {value} -> {p}"
            );
        }
    }

    #[test]
    fn custom_font_detection() {
        assert!(!is_custom_font(""));
        assert!(!is_custom_font(FONT_OPTIONS[1].1)); // a preset value
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

    // ---- serde round-trip + default-tolerance (#15) ----

    /// A `Settings` with every field moved off its default, so a round-trip that
    /// silently drops a field can't hide behind a default value.
    fn non_default_settings() -> Settings {
        Settings {
            font_family: "C:/Fonts/Custom.ttf".into(),
            frame_palette: 2,
            terminal_theme: 1,
            default_shell: "cmd".into(),
            font_px: 18.0,
            show_frame: false,
            show_dot: false,
            clickable_paths: false,
            editor_command: "code -g {path}:{line}".into(),
            scrollback: 9000,
            show_sidebar: false,
            idle_alert: false,
            idle_effect: "pulse".into(),
            idle_alert_seconds: 120,
            auto_update: true,
            copy_on_select: true,
        }
    }

    #[test]
    fn settings_round_trip_is_lossless() {
        let s = non_default_settings();
        let json = serde_json::to_string_pretty(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s, "every field must survive serialize → deserialize");
        // The persisted blob speaks camelCase (the renderer-compatible dialect).
        assert!(json.contains("\"fontFamily\""));
        assert!(json.contains("\"idleAlertSeconds\""));
    }

    #[test]
    fn unknown_keys_in_the_blob_are_tolerated() {
        // Forward-tolerance: a blob written by a NEWER build (extra keys) must still
        // load — known fields are taken, unknown ones ignored, missing ones defaulted.
        let s: Settings = serde_json::from_str(
            r#"{ "fontPx": 20.0, "someFutureSetting": { "x": 1 }, "another": [1,2] }"#,
        )
        .expect("unknown keys must not be fatal");
        assert_eq!(s.font_px, 20.0);
        assert_eq!(s.idle_effect, "firefly"); // defaulted
    }

    #[test]
    fn empty_and_corrupt_blobs_fall_back_to_defaults() {
        // `{}` → all defaults (the load() contract for a first run)…
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        // …and outright corruption fails to parse (load() then returns defaults).
        assert!(serde_json::from_str::<Settings>("not json").is_err());
        assert!(serde_json::from_str::<Settings>("{\"fontPx\": \"big\"}").is_err());
    }
}
