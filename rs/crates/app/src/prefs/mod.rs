//! Preferences — Wave-2 feature plugging into **Seam #1** (mutate-then-resync state).
//!
//! A small persisted [`Settings`] blob (the native port of `store/useSettings.ts`,
//! MVP subset) plus the font-family table the terminal font is loaded from. Settings
//! live on the central [`crate::state::State`]; a change mutates them, persists the
//! blob, and flips `dirty` (font changes also flag a reload) so the next resync
//! re-projects them — the same contract every workspace mutation uses.
//!
//! Persisted to `%APPDATA%\avada\native-settings.json` via
//! `core::persistence::paths` (atomic write), distinct from the Electron build's
//! localStorage blob so the two never fight over a file.

use avada_core::persistence::paths;

pub mod rights;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
#[tracing::instrument(level = "debug", ret)]
pub fn is_custom_font(font: &str) -> bool {
    !font.is_empty() && !FONT_OPTIONS.iter().any(|(_, v)| *v == font)
}

/// The human label for a saved font value — the matching [`FONT_OPTIONS`] label, "Custom"
/// for a user-typed path, else the default. Used by the preview HUD.
#[tracing::instrument(level = "debug", ret)]
pub fn font_label(font: &str) -> &str {
    if let Some((label, _)) = FONT_OPTIONS.iter().find(|(_, v)| *v == font) {
        label
    } else if is_custom_font(font) {
        "Custom"
    } else {
        FONT_OPTIONS[0].0
    }
}

/// Fonts shipped with avada (OFL 1.1, baked into the binary) so they're always
/// available regardless of what the user has installed. Extracted to [`bundled_font_dir`]
/// on startup (see [`init_bundled_fonts`]); their file names match the [`FONT_OPTIONS`]
/// values so the picker resolves them. Licenses live in `assets/fonts/*-OFL.txt`.
pub const BUNDLED_FONTS: [(&str, &[u8]); 2] = [
    (
        "FiraCode-Regular.ttf",
        include_bytes!("../../assets/fonts/FiraCode-Regular.ttf"),
    ),
    (
        "JetBrainsMono-Regular.ttf",
        include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"),
    ),
];

/// Where the baked-in fonts are extracted: `%APPDATA%\avada\fonts`.
#[tracing::instrument(level = "debug", ret)]
pub fn bundled_font_dir() -> std::path::PathBuf {
    paths::user_data_dir().join("fonts")
}

/// Extract the baked-in fonts to [`bundled_font_dir`] (writing each only when missing or a
/// different size, so an app update refreshes them). Best-effort; call once at startup before
/// any font is resolved. A failure just means those fonts fall back like an uninstalled one.
#[tracing::instrument(level = "debug", ret)]
pub fn init_bundled_fonts() {
    let dir = bundled_font_dir();
    let _ = std::fs::create_dir_all(&dir);
    for (name, bytes) in BUNDLED_FONTS {
        let p = dir.join(name);
        let stale = std::fs::metadata(&p)
            .map(|m| m.len() as usize != bytes.len())
            .unwrap_or(true);
        if stale {
            let _ = std::fs::write(&p, bytes);
        }
    }
}

/// Resolve a candidate font-file name to an installed absolute path (forward-slashed), or
/// `None` if it isn't present in any font directory.
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
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

/// Snap an idle-alert threshold onto the dial's grid and into its bounds. The ± buttons can
/// only ever produce a value that already satisfies this; a JSON caller (control API) and an
/// old persisted blob can both hand over anything, and the dial has to be able to show the
/// result.
#[tracing::instrument(level = "debug", ret)]
pub fn idle_seconds_on_grid(secs: u32) -> u32 {
    let step = IDLE_STEP_SECONDS;
    let snapped = (secs / step) * step;
    snapped.clamp(MIN_IDLE_SECONDS, MAX_IDLE_SECONDS)
}

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
    /// Index into [`crate::theme::UI_PALETTES`] for the app *shell* colours — top bar,
    /// sidebar, menus, overlays. Independent of the two above, which colour a pane's dot
    /// and a pane's contents; a light shell over dark terminals is a supported combination.
    /// New in a build after the first settings files were written, so `serde(default)` on
    /// the struct is what makes an older file load as Mocha rather than fail.
    pub ui_palette: usize,
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
    /// Per-pane scrollback (history lines). Sizes every new terminal grid and re-sizes the
    /// live ones when patched (`ctl set scrollback N`). Capped at [`MAX_SCROLLBACK`].
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
    /// X11-style behavior). **ON by default.** Selecting a thing in a terminal is nearly always
    /// the first half of "and now put it somewhere", and the pane you most want it in — one
    /// where an agent holds the mouse grab and a plain drag is the program's, not ours — is
    /// exactly where the explicit copy gesture has the least to grab onto.
    ///
    /// The cost is real and is why Windows Terminal defaults the other way: with this on, an
    /// external copy does *not* survive "select the target, paste over it", because the select
    /// clobbered it. Turning it off restores that, and makes the body right-click modal (copy
    /// the selection if one exists, else paste) instead of always pasting.
    pub copy_on_select: bool,
    /// Whether terminals keep running in the background when Avada closes (the
    /// session-daemon quit-vs-keep-alive toggle, M3). **ON by default** — with the
    /// crash-surviving session daemon (`AVADA_SESSION_DAEMON=1`), an explicit quit then
    /// leaves the daemon + its PTY sessions alive so a relaunch re-attaches them; turning it
    /// OFF makes quit ask the daemon to shut down (kill its sessions + exit). INERT for the
    /// in-process backend (those PTYs die with the GUI regardless).
    pub keep_alive: bool,
    /// Tool ids (`avada_core::tools::registry::ToolDef::id`) the user has starred, in
    /// the order they chose. Favourites are what the left panel offers a mode for and what
    /// the new-pane menu lists first; every registered tool stays *listed* either way.
    /// Unknown ids are kept verbatim rather than dropped — a favourite set edited on a
    /// newer build must survive a round-trip through an older one.
    pub tool_favorites: Vec<String>,
    /// Tool id → an absolute path the user picked by hand, overriding `PATH` detection.
    /// Taken at face value (`tools::detect::resolve`): a human who points at a binary
    /// knows something the probe does not. An empty value is not an override.
    pub tool_paths: BTreeMap<String, String>,
    /// How a URL a tool asks to open gets routed: `"default"` (the OS handler),
    /// `"app"` (the browser named by [`Self::browser_app`]), or `"ask"` (choose at launch
    /// time). Stored as a token so the list can grow without invalidating the blob.
    /// There is deliberately no in-app browser — the *choice* is the feature.
    pub browser_mode: String,
    /// The `core::open::BrowserApp::id` used when `browser_mode == "app"`. Empty, or an id
    /// that is no longer installed, falls back to the OS default rather than failing to
    /// open — losing a browser must never turn a link into a dead click.
    pub browser_app: String,
    /// Whether closing a pane or tab asks first. **ON by default**: a × sits one pixel from
    /// the controls people actually aim at, and the thing behind it is a running shell.
    /// Turning it off only silences the *undoable* closes — the last pane of the last tab
    /// ends the window and nothing can bring it back, so that one asks regardless.
    pub confirm_close: bool,
    /// Log verbosity for every avada process: one of `error|warn|info|debug|trace`
    /// (see `avada_core::logging::LEVELS`). `debug` logs the entry and exit of every
    /// instrumented function with its parameters and return value. Read at process start;
    /// `AVADA_LOG`/`AVADA_DEBUG` in the environment override it for one launch.
    pub log_level: String,
    /// Minutes between firings of the Hyperpane **status loop** (the system tab's agent is
    /// asked to check every pane and recover the stuck ones). `0` disables it.
    pub status_loop_minutes: u32,
    /// Hours between firings of the **restart-all-monitored-agents loop** (every tool pane
    /// is respawned into the same session). `0` disables it.
    pub restart_loop_hours: u32,
    /// What the status loop types into the Hyperpane pane's agent on every firing. Empty =
    /// the built-in prompt ([`crate::loops::DEFAULT_STATUS_PROMPT`]); a single line, since it
    /// is submitted with one Enter. Capped at [`MAX_STATUS_LOOP_PROMPT`] chars.
    pub status_loop_prompt: String,
}

/// [`Settings::browser_mode`] — hand the URL to the OS default handler.
pub const BROWSER_MODE_DEFAULT: &str = "default";
/// [`Settings::browser_mode`] — hand the URL to [`Settings::browser_app`].
pub const BROWSER_MODE_APP: &str = "app";
/// [`Settings::browser_mode`] — ask which browser, every time.
pub const BROWSER_MODE_ASK: &str = "ask";

impl Default for Settings {
    #[tracing::instrument(level = "debug", ret)]
    fn default() -> Self {
        Settings {
            font_family: String::new(),
            frame_palette: 0,
            terminal_theme: 0,
            ui_palette: 0,
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
            copy_on_select: true,
            keep_alive: true,
            tool_favorites: Vec::new(),
            tool_paths: BTreeMap::new(),
            browser_mode: String::from(BROWSER_MODE_DEFAULT),
            browser_app: String::new(),
            confirm_close: true,
            log_level: avada_core::logging::DEFAULT_LEVEL.to_string(),
            status_loop_minutes: 15,
            restart_loop_hours: 24,
            status_loop_prompt: String::new(),
        }
    }
}

impl Settings {
    /// The resolved font path to load: the saved `font_family` path if it's still present,
    /// else the first available family, else the always-present fallback. So a font that was
    /// uninstalled (or a blank default) never loads nothing.
    #[tracing::instrument(level = "debug", ret)]
    pub fn font_path(&self) -> String {
        resolve_or_default(&self.font_family)
    }

    /// Clamp the base font size into the supported range.
    #[tracing::instrument(level = "debug", ret)]
    pub fn clamp_font(px: f32) -> f32 {
        px.clamp(MIN_FONT_PX, MAX_FONT_PX)
    }

    /// Whether `id` is starred.
    #[tracing::instrument(level = "debug", ret)]
    pub fn is_favorite_tool(&self, id: &str) -> bool {
        self.tool_favorites.iter().any(|f| f == id)
    }

    /// Star/unstar `id`, keeping the user's chosen order. Starring appends (newest last)
    /// rather than sorting by the registry, because the order IS the preference.
    #[tracing::instrument(level = "debug", ret)]
    pub fn toggle_favorite_tool(&mut self, id: &str) {
        match self.tool_favorites.iter().position(|f| f == id) {
            Some(i) => {
                self.tool_favorites.remove(i);
            }
            None => self.tool_favorites.push(id.to_string()),
        }
    }

    /// Where a URL should go, resolved against what is actually installed.
    ///
    /// `Some(launcher)` names a specific browser; `None` means "the OS default handler",
    /// which is also what a mode of `"app"` degrades to when the chosen browser has been
    /// uninstalled. `"ask"` is NOT resolved here — that one needs a human, so the caller
    /// checks [`Self::browser_asks`] first.
    #[tracing::instrument(level = "debug", ret)]
    pub fn browser_launcher(&self) -> Option<String> {
        if self.browser_mode != BROWSER_MODE_APP || self.browser_app.is_empty() {
            return None;
        }
        avada_core::open::list_browsers()
            .into_iter()
            .find(|b| b.id == self.browser_app)
            .map(|b| b.launcher)
    }

    /// Whether opening a URL should put the choice to the user.
    #[tracing::instrument(level = "debug", ret)]
    pub fn browser_asks(&self) -> bool {
        self.browser_mode == BROWSER_MODE_ASK
    }

    /// Check every token-valued and range-bound field against what the app can actually
    /// consume, naming the first offender. The consumers are forgiving on READ (an unknown
    /// effect token paints as firefly, an out-of-range palette index wraps to the default),
    /// which is right for an old file — but it also means a bad value written by a script
    /// would persist silently and take effect as something other than what was asked. So
    /// [`save`] (and the control plane's settings patch) refuse a blob that fails this.
    #[tracing::instrument(level = "debug", ret)]
    pub fn validate(&self) -> Result<(), String> {
        let palettes = crate::theme::UI_PALETTES.len();
        if self.ui_palette >= palettes {
            return Err(format!(
                "uiPalette {} is out of range (0..{palettes})",
                self.ui_palette
            ));
        }
        let effects = crate::glow::IdleEffect::OPTIONS;
        if !effects.iter().any(|(t, _)| *t == self.idle_effect) {
            return Err(format!(
                "idleEffect {:?} is not one of {}",
                self.idle_effect,
                join_tokens(effects.iter().map(|(t, _)| *t))
            ));
        }
        let modes = [BROWSER_MODE_DEFAULT, BROWSER_MODE_APP, BROWSER_MODE_ASK];
        if !modes.contains(&self.browser_mode.as_str()) {
            return Err(format!(
                "browserMode {:?} is not one of {}",
                self.browser_mode,
                join_tokens(modes.iter().copied())
            ));
        }
        if !self.default_shell.is_empty()
            && !SHELL_OPTIONS.iter().any(|(_, t)| *t == self.default_shell)
        {
            return Err(format!(
                "defaultShell {:?} is not one of {} (\"\" = the system shell)",
                self.default_shell,
                join_tokens(
                    SHELL_OPTIONS
                        .iter()
                        .map(|(_, t)| *t)
                        .filter(|t| !t.is_empty())
                )
            ));
        }
        if !avada_core::logging::valid_level(&self.log_level) {
            return Err(format!(
                "logLevel {:?} is not one of {}",
                self.log_level,
                join_tokens(avada_core::logging::LEVELS.iter().copied())
            ));
        }
        if self.status_loop_minutes > MAX_STATUS_LOOP_MINUTES {
            return Err(format!(
                "statusLoopMinutes {} is out of range (0 = off, else 1..={MAX_STATUS_LOOP_MINUTES})",
                self.status_loop_minutes
            ));
        }
        if self.restart_loop_hours > MAX_RESTART_LOOP_HOURS {
            return Err(format!(
                "restartLoopHours {} is out of range (0 = off, else 1..={MAX_RESTART_LOOP_HOURS})",
                self.restart_loop_hours
            ));
        }
        if self.scrollback > MAX_SCROLLBACK {
            return Err(format!(
                "scrollback {} is out of range (0..={MAX_SCROLLBACK})",
                self.scrollback
            ));
        }
        if self.status_loop_prompt.chars().count() > MAX_STATUS_LOOP_PROMPT {
            return Err(format!(
                "statusLoopPrompt is longer than {MAX_STATUS_LOOP_PROMPT} chars"
            ));
        }
        if self.status_loop_prompt.contains(['\n', '\r']) {
            return Err(String::from("statusLoopPrompt must be a single line"));
        }
        Ok(())
    }
}

/// Upper bound for [`Settings::status_loop_minutes`] (a week); `0` means off.
pub const MAX_STATUS_LOOP_MINUTES: u32 = 10_080;
/// Upper bound for [`Settings::scrollback`]: alacritty's own ceiling on history lines.
pub const MAX_SCROLLBACK: u32 = 100_000;
/// Upper bound for [`Settings::status_loop_prompt`] in chars; one Enter submits it, so a
/// paragraph is plenty.
pub const MAX_STATUS_LOOP_PROMPT: usize = 4000;
/// Upper bound for [`Settings::restart_loop_hours`] (thirty days); `0` means off.
pub const MAX_RESTART_LOOP_HOURS: u32 = 720;

/// `"a", "b", "c"` — the accepted-values list in a validation message.
#[tracing::instrument(level = "debug", skip_all)]
fn join_tokens<'a>(tokens: impl Iterator<Item = &'a str>) -> String {
    tokens
        .map(|t| format!("{t:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve a saved font value to an actually-loadable font path. Handles the value shapes:
/// empty (→ the platform default via `default_font`), a [`FONT_OPTIONS`] value — a font-file
/// name looked up in the font folders, or (Linux) a fontconfig family name — or a custom
/// absolute path. Anything that can't be found falls back to the platform default, so
/// loading never fails. Shared by the live settings and the in-dialog appearance draft so
/// both highlight the same font.
#[tracing::instrument(level = "debug", ret)]
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
#[tracing::instrument(level = "debug", ret)]
pub fn load() -> Settings {
    let path = paths::user_data_dir().join("native-settings.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    serde_json::from_str::<Settings>(&raw).unwrap_or_default()
}

/// Persist `settings` atomically. Errors are swallowed (a settings write failing must
/// never take down the UI) but logged — a blob that fails [`Settings::validate`] is
/// refused and logged at `warn`, since that one is a caller's bug rather than the disk's.
#[tracing::instrument(level = "debug", ret)]
pub fn save(settings: &Settings) {
    if let Err(e) = try_save(settings) {
        tracing::warn!("settings not saved: {e}");
    }
}

/// [`save`] with the outcome: `Err` names the invalid field (nothing was written) or the
/// I/O failure. For callers that can relay the reason — the control plane's `PATCH
/// /settings` and the preferences dialog.
#[tracing::instrument(level = "debug", ret)]
pub fn try_save(settings: &Settings) -> Result<(), String> {
    settings.validate()?;
    let path = paths::user_data_dir().join("native-settings.json");
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("settings serialize failed: {e}"))?;
    paths::write_atomic(&path, json.as_bytes()).map_err(|e| format!("settings save failed: {e}"))
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
            ui_palette: 3,
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
            copy_on_select: false, // non-default (defaults to true)
            keep_alive: false, // non-default (defaults to true)
            // The tool/browser prefs. Every one is off its default so the round-trip
            // test below can't pass by accidentally re-deriving a default value —
            // in particular `tool_paths`, whose map would round-trip as empty either way.
            tool_favorites: vec!["claude".into(), "codex".into()],
            tool_paths: [("claude".to_string(), "/opt/bin/claude".to_string())]
                .into_iter()
                .collect(),
            browser_mode: BROWSER_MODE_ASK.into(),
            browser_app: "com.google.Chrome".into(),
            confirm_close: false, // non-default (defaults to true)
            log_level: "debug".into(),
            status_loop_minutes: 5,
            restart_loop_hours: 6,
            status_loop_prompt: String::from("status please"),
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

    #[test]
    fn validate_accepts_the_defaults_and_every_listed_token() {
        assert_eq!(Settings::default().validate(), Ok(()));
        let mut s = Settings::default();
        for i in 0..crate::theme::UI_PALETTES.len() {
            s.ui_palette = i;
            assert_eq!(s.validate(), Ok(()), "uiPalette {i}");
        }
        for (tok, _) in crate::glow::IdleEffect::OPTIONS {
            s.idle_effect = tok.into();
            assert_eq!(s.validate(), Ok(()), "idleEffect {tok}");
        }
        for mode in [BROWSER_MODE_DEFAULT, BROWSER_MODE_APP, BROWSER_MODE_ASK] {
            s.browser_mode = mode.into();
            assert_eq!(s.validate(), Ok(()), "browserMode {mode}");
        }
        for (_, tok) in SHELL_OPTIONS {
            s.default_shell = tok.into();
            assert_eq!(s.validate(), Ok(()), "defaultShell {tok:?}");
        }
        for level in avada_core::logging::LEVELS {
            s.log_level = level.into();
            assert_eq!(s.validate(), Ok(()), "logLevel {level}");
        }
        // Case-insensitive, like the logger itself.
        s.log_level = "DEBUG".into();
        assert_eq!(s.validate(), Ok(()));
        // Both loop bounds, and off.
        for m in [0, 1, MAX_STATUS_LOOP_MINUTES] {
            s.status_loop_minutes = m;
            assert_eq!(s.validate(), Ok(()), "statusLoopMinutes {m}");
        }
        for h in [0, 1, MAX_RESTART_LOOP_HOURS] {
            s.restart_loop_hours = h;
            assert_eq!(s.validate(), Ok(()), "restartLoopHours {h}");
        }
    }

    #[test]
    fn validate_names_the_offending_field() {
        fn err_of(mutate: impl FnOnce(&mut Settings)) -> String {
            let mut s = Settings::default();
            mutate(&mut s);
            s.validate().expect_err("must be rejected")
        }
        let n = crate::theme::UI_PALETTES.len();
        assert!(err_of(|s| s.ui_palette = n).starts_with("uiPalette"));
        assert!(err_of(|s| s.idle_effect = "strobe".into()).starts_with("idleEffect"));
        assert!(err_of(|s| s.idle_effect = String::new()).starts_with("idleEffect"));
        assert!(err_of(|s| s.browser_mode = "chrome".into()).starts_with("browserMode"));
        assert!(err_of(|s| s.default_shell = "/bin/nope".into()).starts_with("defaultShell"));
        assert!(err_of(|s| s.log_level = "verbose".into()).starts_with("logLevel"));
        assert!(
            err_of(|s| s.status_loop_minutes = MAX_STATUS_LOOP_MINUTES + 1)
                .starts_with("statusLoopMinutes")
        );
        assert!(
            err_of(|s| s.restart_loop_hours = MAX_RESTART_LOOP_HOURS + 1)
                .starts_with("restartLoopHours")
        );
        // The message carries the bad value, so a script author can see what was sent.
        assert!(err_of(|s| s.idle_effect = "strobe".into()).contains("\"strobe\""));
    }

    #[test]
    fn try_save_refuses_an_invalid_blob_before_touching_the_disk() {
        let s = Settings {
            log_level: "loud".into(),
            ..Default::default()
        };
        let err = try_save(&s).expect_err("an invalid blob must not be persisted");
        assert!(err.starts_with("logLevel"), "{err}");
    }
}
