//! The central app state — Wave-2 **Seam #1**.
//!
//! `State` owns every tab (workspace group), each with its own panes, layout,
//! split sizes, main-fraction, focus and zoom. All mutation flows through the
//! methods here; each one leaves the data consistent and flips `dirty` so the
//! next pump cycle *resyncs* the Slint models (see [`crate::paneview::resync`]).
//! That **mutate → set-dirty → resync** contract is the single seam Wave-2
//! features (palette, keybindings, prefs) extend: they only ever call these
//! methods (usually via a [`crate::command::Command`]) and never touch the UI
//! models directly.

use std::time::Instant;

use hyperpanes_core::layout::navigate::{neighbor_index, Direction};
use hyperpanes_core::layout::presets::{
    compute_dividers, compute_tiles, effective_layout, DividerKind, Layout,
};
use hyperpanes_core::layout::sizes::{
    clamp_fraction, equal_sizes, insert_size, remove_size, resize_at,
};
use hyperpanes_core::ai::service::AiProjectRef;
use hyperpanes_core::persistence::projects;
use hyperpanes_core::session_manager::{SessionManager, SpawnOptions};
use hyperpanes_core::workspace::io::{read_workspace, windows_of, write_workspace};
use hyperpanes_core::workspace::model::{GroupSpec, PaneSpec, WorkspaceFile};
use hyperpanes_terminal_widget::{Font, RenderOpts, SoftwareRenderer, TerminalPane};

use slint::{Color, Image, SharedString};

use crate::command::Command;
use crate::glow::Glow;
use crate::palette::{self, Entry};
use crate::prefs::{self, Settings};
use crate::sidebar::{self, Project};
use crate::theme;

/// Which Wave-2 overlay panel (if any) is mounted in the overlay slot (**Seam #3**).
/// Exactly one is shown at a time; opening one replaces the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Palette,
    Prefs,
    /// The "New pane" options dialog (Shift+＋ / the menus' "New pane…"). Configures a pane
    /// before it spawns; submitting routes through [`State::add_pane_opts`].
    NewPane,
}

/// A session detached from its window for re-hosting in another (Wave-1 multi-window
/// plumbing). Carries only the session `uid` + chrome; the PTY stays alive centrally in
/// the [`SessionManager`], so re-hosting is a replay-into-a-fresh-grid, never a restart.
#[derive(Debug, Clone)]
pub struct DetachedPane {
    pub uid: String,
    pub title: SharedString,
    pub subtitle: Option<SharedString>,
    pub pinned_accent: Option<Color>,
    /// Per-pane frame/dot overrides (carried across a re-host so a tinted project pane
    /// stays tinted, and a clean pane stays clean). `None` = inherit the global pref.
    pub show_frame: Option<bool>,
    pub show_dot: Option<bool>,
    /// The pane's per-pane zoom (terminal font px), carried across a re-host so a zoomed pane
    /// keeps its size when torn off / moved.
    pub font_px: f32,
}

/// A whole tab detached for re-hosting (the tab menu's "Move to New Window") or parked on the
/// closed-tab stack (for "Reopen Closed Tab"). Like [`DetachedPane`] the sessions stay alive
/// centrally, so re-hosting is a replay-into-fresh-grids, never a restart.
#[derive(Debug, Clone)]
pub struct DetachedTab {
    pub title: SharedString,
    pub layout: Layout,
    pub sizes: Vec<f64>,
    pub main_fraction: f64,
    /// The focused pane index, carried so a reopened/moved tab restores focus instead of
    /// snapping back to pane 0.
    pub focused: usize,
    /// The zoomed (maximised-in-tab) pane, carried so a reopened/moved tab keeps its
    /// maximized pane instead of dropping the zoom.
    pub zoomed: Option<usize>,
    pub panes: Vec<DetachedPane>,
}

/// A single preferences edit, carried by `Command::ApplySetting`. Keeps the `Command`
/// enum flat (one variant) while still typing each field of [`Settings`].
#[derive(Debug, Clone)]
pub enum Setting {
    /// Select the terminal font by its file path (see `prefs::available_families`).
    FontFamily(String),
    /// Select the frame palette by index into `theme::FRAME_PALETTES` (remaps pane accents).
    FramePalette(usize),
    /// Select the terminal colour theme by index into `theme::TERMINAL_THEMES`.
    TerminalTheme(usize),
    /// Set the default shell token for new panes ("" = system default).
    DefaultShell(String),
    /// Nudge the base font size by ±N points.
    FontDelta(i32),
    ShowFrame(bool),
    ShowDot(bool),
    /// Toggle whether terminal paths are clickable.
    ClickablePaths(bool),
    /// Set the editor-command template used to open clicked paths ("" = auto).
    EditorCommand(String),
    /// Toggle the idle-glow (AI-pane quiescence glow).
    IdleAlert(bool),
    /// Select the glow style by index into `glow::IdleEffect::OPTIONS`.
    IdleEffect(usize),
    /// Nudge the idle threshold (seconds) by ±N, clamped to the supported range.
    IdleSeconds(i32),
}

/// The "New pane" dialog's payload — full spawn options for a configured pane. The simple
/// [`State::add_pane`] / [`State::add_pane_cwd`] paths build a default of this. The native
/// port of the Electron `addPane({ label, color, showFrame, showDot, command, cwd, shell })`.
#[derive(Debug, Clone, Default)]
pub struct NewPaneOpts {
    /// Label override (empty → the slot default, e.g. "pane 3").
    pub label: Option<String>,
    pub cwd: Option<String>,
    /// A command to run instead of an interactive shell (empty → interactive).
    pub command: Option<String>,
    /// Shell token override ("" / `None` → the default-shell preference).
    pub shell: Option<String>,
    /// The chosen accent (the swatch). `None` = the by-slot palette color (a plain new pane).
    pub accent: Option<Color>,
    /// Explicit frame/dot. `None` = the default (tinted when `accent` is pinned, else off).
    pub show_frame: Option<bool>,
    pub show_dot: Option<bool>,
}

/// The in-dialog draft of the **appearance** settings. While Preferences is open these edit
/// the draft only — the live panes don't change until Done (mirrors the renderer's
/// `AppearanceDraft`). General/Terminal settings (shell, clickable paths, editor) are not
/// drafted; they apply immediately, exactly like the Electron dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefsDraft {
    pub font_family: String,
    pub frame_palette: usize,
    pub terminal_theme: usize,
    pub font_px: f32,
    pub show_frame: bool,
    pub show_dot: bool,
}

impl PrefsDraft {
    /// Snapshot the appearance subset of `s`.
    fn from_settings(s: &Settings) -> Self {
        PrefsDraft {
            font_family: s.font_family.clone(),
            frame_palette: s.frame_palette,
            terminal_theme: s.terminal_theme,
            font_px: s.font_px,
            show_frame: s.show_frame,
            show_dot: s.show_dot,
        }
    }
}

/// The ambient-AI subtitle state for one pane: the latest engine-produced summary line
/// (`full`, already redacted by `core::ai`) and a typewriter `reveal` cursor (chars shown
/// so far, as a float so the pump can advance it a sub-character per tick for a smooth
/// reveal). Empty `full` = no AI line for this pane. A manual subtitle and the per-pane
/// Mute flag both suppress the AI line at render time (manual always wins).
#[derive(Debug, Clone, Default)]
pub struct AiLine {
    pub full: String,
    pub reveal: f32,
    /// Cached `full.chars().count()` — the typewriter reveal needs this every frame for every
    /// AI pane, so it's computed once here (on `set_target`) rather than re-walking `full` each
    /// tick. Kept in lock-step with `full`: only `set_target` mutates either.
    pub len: usize,
}

impl AiLine {
    /// Set the target summary text; restart the typewriter when it actually changed.
    pub fn set_target(&mut self, text: &str) {
        if self.full != text {
            self.full = text.to_string();
            self.len = self.full.chars().count();
            self.reveal = 0.0;
        }
    }
}

/// One pane's controller-side state (terminal grid + placement + chrome).
pub struct PaneState {
    pub uid: String,
    /// The pane's editable label (the header title): "shell"/"pane N" by default, the
    /// first word of a launched command, or — once tinted to a git project — the repo
    /// name. Double-click the header to rename (see [`State::begin_rename_pane`]).
    pub title: SharedString,
    /// An optional secondary line under the label (user-set subtitle). `None` = none.
    pub subtitle: Option<SharedString>,
    /// Per-pane override of the global `show_frame`/`show_dot` Appearance prefs (mirrors
    /// the renderer's `pane.showFrame ?? globalShowFrame`). NEW panes default both to
    /// `Some(false)` (clean: no colored border/tint/dot); a git-project tint flips them to
    /// `Some(true)`; `None` would inherit the global pref. The pane still carries a `color`
    /// VALUE (its `accent`) even while clean.
    pub show_frame: Option<bool>,
    pub show_dot: Option<bool>,
    pub accent: Color,
    pub pane: TerminalPane,
    /// Cell dims currently applied to the bound session (to detect a real reflow).
    pub applied: (usize, usize),
    /// The latest rendered terminal image.
    pub surface: Image,
    /// Placement in logical px, recomputed on relayout.
    pub rect: (f32, f32, f32, f32),
    pub visible: bool,
    /// Whether the shell has produced its first output yet (gate the startup write).
    pub started: bool,
    pub startup: Option<String>,
    /// A fixed accent (e.g. a project color) that survives relabel; `None` = by-index.
    pub pinned_accent: Option<Color>,
    /// The terminal surface's on-screen logical size (from the widget's `geometry-changed`),
    /// used to hit-test clickable-path hover/click coordinates. `(0,0)` until first laid out.
    pub surf: (f32, f32),
    /// The current clickable-path hover hit (drives the link overlay), plus the cursor
    /// position (logical px within the surface) for tooltip placement. `None` = no link.
    pub link: Option<hyperpanes_terminal_widget::LinkHit>,
    pub link_cursor: (f32, f32),
    /// Idle-glow animation state — its `alpha` (0 when not glowing) is projected into the
    /// pane model each tick once the pane has been output-quiet past the idle threshold.
    pub glow: Glow,
    /// The pane's latest OSC window title (sniffed from pty output), used to detect an
    /// AI/agent CLI so the idle glow only arms on agent panes (mirrors `isAiPane`). "" until
    /// the shell sets a title.
    pub shell_title: String,
    /// Whether the pane's ambient-AI summary line is muted (the pane menu's "Mute AI Summary"
    /// toggle; mirrors the renderer's `ui.aiMuted` set). New panes default unmuted.
    pub ai_muted: bool,
    /// Ambient-AI subtitle + typewriter reveal state (the local projection of this pane's
    /// `meta['ai.subtitle']`; produced by the `core::ai` engine when enabled).
    pub ai: AiLine,
    /// The last polled value of the widget's transient bottom-right indicator ("toast" —
    /// copy/paste confirmations + the Ctrl-zoom font %), cached so the pump can detect a
    /// change and update/clear the row even when the surface itself isn't dirty.
    pub last_toast: String,
    /// Bumped each time Ctrl+F (re)invokes the in-pane search box on this pane, even while it's
    /// already open. Projected into `PaneItem::search_focus_seq` so the widget can (re)focus the
    /// query input on a reliable change signal rather than a one-shot `init`.
    pub search_focus_seq: i32,
    /// The pane's OWN terminal font size (logical px) — per-pane zoom. Ctrl+= / − / 0 adjust
    /// the FOCUSED pane only (Electron parity); a new pane starts at the configured base
    /// (`Settings::font_px`). Drives both the rendered glyphs and the indicator scaling.
    pub font_px: f32,
    /// The font loaded at `font_px × DPI-scale` (its own glyph cache + cell metrics), so each
    /// pane renders — and reflows — at its own zoom level independently of its neighbours.
    pub font: hyperpanes_terminal_widget::Font,
    /// Set when `font_px` changed → the pump reloads `font` at the current DPI scale and
    /// forces a repaint on the next tick (a DPI / family / base-size change reloads via
    /// [`State::reload_font`] instead).
    pub font_dirty: bool,
    /// The pane's latest reported working directory (OSC-7 / OSC-9;9 sniff), mirrored from
    /// the session's `Cwd` events. Surfaced into the control read-model's `/state` so agents
    /// (and `list_panes`) see each pane's live cwd. `None` until the shell reports one.
    pub cwd: Option<String>,
}

impl PaneState {
    /// Effective frame visibility: the per-pane override if set, else the global pref.
    pub fn frame_on(&self, global: bool) -> bool {
        self.show_frame.unwrap_or(global)
    }
    /// Effective dot visibility: the per-pane override if set, else the global pref.
    pub fn dot_on(&self, global: bool) -> bool {
        self.show_dot.unwrap_or(global)
    }
}

/// Whether `label` is still a default auto-name ("shell" / "pane N"), so a git-project tint
/// may overwrite it — never a name the user chose. Mirrors the renderer's `/^(shell|pane \d+)$/i`
/// test. A bare number (e.g. "42") is NOT treated as default: it's a valid user rename, and
/// silently overwriting it with the repo name on a cwd change would clobber that choice.
fn is_default_label(label: &str) -> bool {
    let l = label.trim();
    if l.eq_ignore_ascii_case("shell") {
        return true;
    }
    if let Some(rest) = l.strip_prefix("pane ").or_else(|| l.strip_prefix("Pane ")) {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// One tab = a self-contained workspace group (the Rust port of `useWorkspace`'s
/// `Group`). Background tabs keep their `PaneState`s — and thus their live
/// sessions — alive; only the active tab is mounted in the UI models.
pub struct Tab {
    pub title: SharedString,
    pub panes: Vec<PaneState>,
    pub layout: Layout,
    pub sizes: Vec<f64>,
    pub main_fraction: f64,
    pub focused: usize,
    /// Index of the zoomed (maximised-in-tab) pane, if any.
    pub zoomed: Option<usize>,
}

impl Tab {
    fn empty(title: SharedString) -> Self {
        Tab {
            title,
            panes: Vec::new(),
            layout: Layout::Auto,
            sizes: Vec::new(),
            main_fraction: 0.6,
            focused: 0,
            zoomed: None,
        }
    }

    /// Recolor panes so each unpinned pane's accent tracks its creation slot in the
    /// current palette. A pinned accent (a manual recolor or a git-project color) is
    /// preserved. Pane LABELS are stable and user-owned, so they're left untouched —
    /// unlike the old build, panes are no longer renumbered 1..N (that was the bare-number
    /// "title" bug). The color VALUE stays assigned even on a clean (frame-off) pane.
    fn relabel(&mut self, palette: usize) {
        for (i, p) in self.panes.iter_mut().enumerate() {
            p.accent = p.pinned_accent.unwrap_or_else(|| theme::accent_for(i, palette));
        }
    }

    /// The concrete preset this tab currently tiles with (auto resolved).
    pub fn effective(&self) -> Layout {
        effective_layout(self.layout, self.panes.len())
    }
}

/// The whole window's workspace state.
pub struct State {
    /// The base font (loaded at the configured `font_px`) — the template a new pane copies its
    /// size from; per-pane rendering uses each pane's own [`PaneState::font`].
    pub font: hyperpanes_terminal_widget::Font,
    /// The DPI scale of the last pump tick, so pane fonts (created/zoomed outside the pump,
    /// where the scale isn't known) can be loaded at the right physical size.
    pub last_scale: f32,
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_uid: usize,
    tab_seq: usize,
    pub fullscreen: bool,
    /// Index of the tab whose title is being edited inline (-1 = none).
    pub editing_tab: i32,
    /// Index (within the active tab) of the pane whose label is being edited inline
    /// (-1 = none). Double-clicking a pane header sets this; it drives the inline editor.
    pub editing_pane: i32,
    pub last_blink: Instant,
    pub cursor_on: bool,
    pub frames: u32,
    pub last_hud: Instant,
    /// The UI models (tabs / panes / dividers) need a full rebuild.
    pub dirty: bool,
    // ---- Wave-2: overlay panels (Seam #3) ----
    /// Which overlay panel is mounted (palette / prefs / sidebar / none).
    pub overlay: Overlay,
    /// Persisted appearance preferences (font, frame/dot).
    pub settings: Settings,
    /// The user's keybinding overrides — consulted (override-first) by the key router. Edited
    /// live from the Preferences → Keybindings panel.
    pub keymap: crate::keybindings::Keymap,
    /// The binding id currently capturing a new chord in the editor (`None` = not capturing).
    /// While set, that editor row grabs focus and the next key combo rebinds it.
    pub capturing_binding: Option<String>,
    /// While capturing, the label of the binding the last-pressed chord clashes with (`None` =
    /// no clash yet). Drives the editor's "Used by <label>" message; cleared on a clean bind.
    pub capture_conflict: Option<String>,
    /// Set when the font family/size changed — the pump reloads the font (it owns the
    /// DPI scale) then clears this.
    pub font_reload: bool,
    /// The in-dialog appearance draft (Some while Preferences is open). Appearance edits go
    /// here and only commit to `settings` (and the panes) on Done.
    pub prefs_draft: Option<PrefsDraft>,
    /// Whether the "unsaved appearance changes" save/discard prompt is showing.
    pub prefs_confirm: bool,
    /// Whether the font picker is in "Custom…" mode (showing the free-text font path field).
    pub font_custom: bool,
    // ---- appearance preview: a real, locked (no-pty) terminal showing sample output ----
    /// The preview pane (fed canned sample output once; never bound to a session).
    preview_pane: TerminalPane,
    /// The font the preview renders with, reloaded when the drafted family/size/scale change.
    preview_font: Option<Font>,
    /// Cache key for `preview_font`: `(font_path, px, scale)`.
    preview_key: (String, f32, f32),
    /// Last terminal-theme index applied to the preview pane (-1 = none yet).
    preview_theme: i32,
    /// Last cursor on/off state rendered into the preview (so the caret blinks).
    preview_cursor: bool,
    /// The latest rendered preview image (shown in the Appearance preview).
    pub preview_surface: Image,
    /// Animates the idle-glow demo on the AI-features preview (always "idle" so the chosen
    /// effect plays continuously while Preferences is open).
    pub preview_glow: Glow,
    /// The self-playing Tetris shown in the preview pane (ambient animation).
    pub preview_tetris: crate::tetris::Tetris,
    /// When the Tetris last advanced a frame (it steps on a fixed cadence, not every tick).
    pub preview_tetris_last: Instant,
    /// Cached, newest-first git-project list for the sidebar rail.
    pub projects: Vec<Project>,
    /// Whether the projects flyout (behind the 📁 icon) is currently expanded. The rail
    /// itself is gated by `settings.show_sidebar`; this is just the flyout panel state.
    pub sidebar_open: bool,
    // ---- command palette working state ----
    /// The registry snapshot built when the palette opened.
    palette_entries: Vec<Entry>,
    /// Indices into `palette_entries` that survive the current query, best-first.
    pub palette_view: Vec<usize>,
    /// The highlighted row within `palette_view`.
    pub palette_sel: usize,
    /// The live search query.
    pub palette_query: String,
    // ---- hold-Esc-to-exit-fullscreen tracking (no key-release events, so we
    // infer a held key from rapid auto-repeat) ----
    esc_last: Option<Instant>,
    esc_hold_start: Option<Instant>,
    /// True while Esc is being held — drives the hint + its progress fill.
    pub esc_holding: bool,
    esc_fired: bool,
    // ---- context menus (Phase-5 parity) ----
    /// The open cursor-anchored context menu (pane header / tab strip), if any. Built fresh on
    /// each right-click so its gating + checkmarks reflect the moment it opened.
    pub ctx: Option<crate::contextmenu::CtxMenu>,
    /// Most-recently-closed tabs (sessions kept alive centrally) for "Reopen Closed Tab",
    /// newest last. Capped — evicted entries' sessions are killed.
    pub closed_tabs: Vec<DetachedTab>,
}

/// What the key router should do with an Escape press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscOutcome {
    /// A tap — send Escape to the focused shell.
    Forward,
    /// Held in fullscreen — leave fullscreen (and don't forward).
    Exit,
    /// An auto-repeat tail — swallow (so a hold doesn't spam the shell).
    Ignore,
}

impl State {
    /// Fresh state with a single empty tab; the caller seeds pane 0 via [`Self::add_pane`].
    pub fn new(font: hyperpanes_terminal_widget::Font) -> Self {
        let mut s = State {
            font,
            last_scale: 1.0,
            tabs: Vec::new(),
            active: 0,
            next_uid: 0,
            tab_seq: 0,
            fullscreen: false,
            editing_tab: -1,
            editing_pane: -1,
            last_blink: Instant::now(),
            cursor_on: true,
            frames: 0,
            last_hud: Instant::now(),
            dirty: true,
            overlay: Overlay::None,
            settings: prefs::load(),
            keymap: crate::keybindings::Keymap::load(),
            capturing_binding: None,
            capture_conflict: None,
            // Apply the saved font family/size on the first pump (it owns the scale).
            font_reload: true,
            prefs_draft: None,
            prefs_confirm: false,
            font_custom: false,
            // Sized to the preview frame: the Tetris board (20 cols × 26 rows) plus a 2-row
            // HUD above and below (the font preview). See `preview_frame`.
            preview_pane: TerminalPane::new(20, 30, Box::new(SoftwareRenderer::new())),
            preview_font: None,
            preview_key: (String::new(), 0.0, 0.0),
            preview_theme: -1,
            preview_cursor: false,
            preview_surface: Image::default(),
            preview_glow: Glow::new(0x9E37_79B9_7F4A_7C15),
            preview_tetris: crate::tetris::Tetris::new(0x5DEE_CE2F_1234_ABCD),
            preview_tetris_last: Instant::now(),
            // Seed the rail's badge with the remembered projects up front (so the count
            // is right before any pane reports a cwd).
            projects: sidebar::list(),
            sidebar_open: false,
            palette_entries: Vec::new(),
            palette_view: Vec::new(),
            palette_sel: 0,
            palette_query: String::new(),
            esc_last: None,
            esc_hold_start: None,
            esc_holding: false,
            esc_fired: false,
            ctx: None,
            closed_tabs: Vec::new(),
        };
        let tab = s.fresh_tab();
        s.tabs.push(tab);
        // Seed the preview with the first composed frame so it's never blank before the
        // first animation tick (the pump advances + re-feeds it while Preferences is open).
        let frame = s.preview_frame();
        s.preview_pane.feed(&frame);
        s
    }

    /// Render the appearance preview (a real, locked terminal) with the drafted font + theme,
    /// returning the freshly-rendered image when anything changed (else `None`). Called by the
    /// pump while Preferences is open; `scale` is the window DPI scale.
    pub fn render_preview(&mut self, scale: f32, cursor_on: bool) -> Option<Image> {
        let (font_path, px, theme_idx) = match &self.prefs_draft {
            Some(d) => (prefs::resolve_or_default(&d.font_family), d.font_px, d.terminal_theme),
            None => (self.settings.font_path(), self.settings.font_px, self.settings.terminal_theme),
        };
        let key = (font_path.clone(), px, scale);
        let mut changed = false;
        if self.preview_font.is_none() || self.preview_key != key {
            self.preview_font = Some(theme::load_font_at(&font_path, px, scale));
            self.preview_key = key;
            changed = true;
        }
        if self.preview_theme != theme_idx as i32 {
            self.preview_pane.set_palette(theme::terminal_theme(theme_idx));
            self.preview_theme = theme_idx as i32;
            changed = true;
        }
        // Locked (no pty), but the caret still blinks like a real terminal.
        if self.preview_cursor != cursor_on {
            self.preview_cursor = cursor_on;
            changed = true;
        }
        if changed || self.preview_pane.take_dirty() {
            let font = self.preview_font.as_mut().unwrap();
            self.preview_surface = self.preview_pane.render(font, &RenderOpts { cursor_on });
            Some(self.preview_surface.clone())
        } else {
            None
        }
    }

    /// Advance the preview's ambient Tetris on its fixed cadence, feeding the new composed
    /// frame into the locked preview terminal so it animates. Called by the pump while
    /// Preferences is open; cheap no-op between frames.
    pub fn animate_preview_tetris(&mut self) {
        if self.preview_tetris_last.elapsed() >= std::time::Duration::from_millis(90) {
            self.preview_tetris.step();
            let frame = self.preview_frame();
            self.preview_pane.feed(&frame);
            self.preview_tetris_last = Instant::now();
        }
    }

    /// Compose the full preview frame: a 2-row Tetris HUD (score / level / lines), the board
    /// coloured from the drafted frame palette, then a 2-row HUD (NEXT swatch + the font name
    /// at the drafted size — so the font family/size still preview live). Reflects the
    /// appearance DRAFT while the dialog is open (else the committed settings).
    fn preview_frame(&self) -> String {
        let (palette_idx, px, font_value) = match &self.prefs_draft {
            Some(d) => (d.frame_palette, d.font_px, d.font_family.as_str()),
            None => (self.settings.frame_palette, self.settings.font_px, self.settings.font_family.as_str()),
        };
        let colors = theme::frame_palette(palette_idx);
        let (ar, ag, ab) = colors[0]; // accent = the palette's first slot
        let t = &self.preview_tetris;
        let board = t.render(colors);
        let (nr, ng, nb) = colors[t.next_kind() % colors.len()];
        let label = prefs::font_label(font_value);

        let accent = format!("\x1b[38;2;{};{};{}m", ar, ag, ab);
        let dim = "\x1b[2m";
        let reset = "\x1b[0m";
        let trunc = |s: &str, n: usize| s.chars().take(n).collect::<String>();

        let mut s = String::with_capacity(2400);
        s.push_str("\x1b[H\x1b[?25l"); // home + hide cursor (it's an animation, not a prompt)
        // top HUD: score (accent) + level/lines
        s.push_str(&accent);
        s.push_str(&format!("SCORE {:06}", t.score()));
        s.push_str(reset);
        s.push_str("\x1b[K\r\n");
        s.push_str(dim);
        s.push_str(&format!("LEVEL {}  LINES {}", t.level(), t.lines()));
        s.push_str(reset);
        s.push_str("\x1b[K\r\n");
        // the palette-coloured board (H rows)
        s.push_str(&board);
        s.push_str("\r\n");
        // bottom HUD: the NEXT piece (letter + swatch, in its colour), then font name + size
        s.push_str(dim);
        s.push_str("NEXT ");
        s.push_str(reset);
        s.push_str(&format!(
            "\x1b[38;2;{};{};{}m{} \u{2588}\u{2588}\x1b[0m",
            nr, ng, nb, t.next_letter()
        ));
        s.push_str("\x1b[K\r\n");
        s.push_str(dim);
        s.push_str(&format!("{}  {}px", trunc(label, 13), px as i32));
        s.push_str(reset);
        s.push_str("\x1b[K"); // last line: no trailing newline → never scrolls the grid
        s
    }

    fn fresh_tab(&mut self) -> Tab {
        self.tab_seq += 1;
        Tab::empty(format!("term {}", self.tab_seq).into())
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Locate the (tab, pane) holding session `uid` across *all* tabs (events for
    /// background tabs still need to reach their pane).
    pub fn find_pane(&mut self, uid: &str) -> Option<(usize, usize)> {
        for (ti, t) in self.tabs.iter().enumerate() {
            if let Some(pi) = t.panes.iter().position(|p| p.uid == uid) {
                return Some((ti, pi));
            }
        }
        None
    }

    fn make_pane(
        &mut self,
        mgr: &SessionManager,
        idx: usize,
        opts: NewPaneOpts,
    ) -> Option<PaneState> {
        let uid = format!("pane-{}", self.next_uid);
        self.next_uid += 1;
        let palette = self.settings.frame_palette;
        let cwd = opts.cwd.filter(|c| !c.is_empty());
        let accent = opts.accent;
        // A command to run instead of an interactive shell ("" → interactive).
        let command = opts.command.filter(|c| !c.is_empty());
        // Shell: an explicit token from the dialog is used verbatim; otherwise honour the
        // default-shell preference ("" = prefer pwsh when available, else core's default).
        let shell = match opts.shell {
            Some(s) if !s.is_empty() => Some(s),
            _ => prefs::effective_shell(&self.settings.default_shell),
        };
        // Inject shell integration so the shell reports its cwd (OSC-7 for pwsh/bash, OSC
        // 9;9 for cmd). That's what lets a pane's cwd → git-project tint (and clickable-path
        // resolution) actually fire — without it pwsh never emits a cwd OSC. Additive: the
        // resolved shell is classified, and the init script must be deployed next to the
        // binary (build.rs in dev, packaging for release), else this is simply `None`.
        let shell_path = shell
            .clone()
            .unwrap_or_else(hyperpanes_core::session::spawn::default_shell);
        // Integration applies to the interactive branch only; a one-off `command` pane is
        // not an interactive shell, so skip it there (core would ignore it anyway).
        let integration = command.is_none().then(|| {
            hyperpanes_core::shell_integration::integration_for(
                &shell_path,
                &hyperpanes_core::shell_integration::shell_integration_dir(),
            )
            .map(|si| hyperpanes_core::session_manager::Integration {
                args: si.args,
                env: si.env.into_iter().collect(),
            })
        }).flatten();
        let (cols, rows) = (80u16, 24u16);
        if let Err(e) = mgr.create(SpawnOptions {
            uid: uid.clone(),
            cols: Some(cols),
            rows: Some(rows),
            pane_id: Some(uid.clone()),
            cwd,
            shell,
            command,
            integration,
            ..Default::default()
        }) {
            eprintln!("[hyperpanes] failed to spawn {uid}: {e}");
            return None;
        }
        let mut pane =
            TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new()));
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        let glow = Glow::new(crate::glow::seed_from(&uid));
        // A pane spawned WITH an accent is a project/dialog pane: by default tint it on. A
        // plain new pane is clean — it still gets a palette color VALUE by slot, but its
        // frame/dot overrides default OFF (mirrors `addPane`). The New Pane dialog passes
        // explicit `show_frame`/`show_dot` (both default off — a fresh pane is clean) which
        // win over this default.
        let project = accent.is_some();
        let show_frame = opts.show_frame.unwrap_or(project);
        let show_dot = opts.show_dot.unwrap_or(project);
        let label = match opts.label {
            Some(l) if !l.trim().is_empty() => l,
            _ if idx == 0 => "shell".to_string(),
            _ => format!("pane {}", idx + 1),
        };
        // Each pane owns its font (per-pane zoom); start at the configured base size.
        let font_px = self.settings.font_px;
        let font = theme::load_font_at(&self.settings.font_path(), font_px, self.last_scale);
        Some(PaneState {
            uid,
            title: label.into(),
            subtitle: None,
            show_frame: Some(show_frame),
            show_dot: Some(show_dot),
            accent: accent.unwrap_or_else(|| theme::accent_for(idx, palette)),
            pane,
            applied: (cols as usize, rows as usize),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: false,
            startup: None,
            pinned_accent: accent,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            search_focus_seq: 0,
            font_px,
            font,
            font_dirty: false,
            cwd: None,
        })
    }

    // ---- pane mutations (act on the active tab) ----

    /// Spawn a new pane + shell in the active tab and focus it.
    pub fn add_pane(&mut self, mgr: &SessionManager) {
        self.add_pane_cwd(mgr, None, None);
    }

    /// Spawn a new pane in the active tab with an optional working directory + accent
    /// (used to open a sidebar project cd'd into its repo), and focus it.
    pub fn add_pane_cwd(&mut self, mgr: &SessionManager, cwd: Option<String>, accent: Option<Color>) {
        self.add_pane_opts(mgr, NewPaneOpts { cwd, accent, ..Default::default() });
    }

    /// Spawn a new pane in the active tab from the full [`NewPaneOpts`] (the New Pane
    /// dialog's payload), and focus it.
    pub fn add_pane_opts(&mut self, mgr: &SessionManager, opts: NewPaneOpts) {
        let idx = self.active_tab().panes.len();
        let Some(ps) = self.make_pane(mgr, idx, opts) else {
            return;
        };
        let auto = self.active_tab().layout == Layout::Auto;
        let t = self.active_tab_mut();
        t.sizes = if auto {
            equal_sizes(idx + 1)
        } else {
            insert_size(&t.sizes, idx)
        };
        t.panes.push(ps);
        t.focused = idx;
        t.zoomed = None;
        self.dirty = true;
    }

    /// Close pane `idx` in the active tab (see [`Self::close_pane_in`]).
    pub fn close_pane(&mut self, idx: usize, mgr: &SessionManager) -> bool {
        self.close_pane_in(self.active, idx, mgr)
    }

    /// Remove pane `idx` of tab `ti` **without** killing its session, returning the
    /// removed [`PaneState`] and whether the window still has panes (`false` = the
    /// workspace emptied → the caller should close the window). An emptied non-last tab
    /// is dropped. Shared by [`Self::close_pane_in`] (which then kills the session) and
    /// pane re-host (which keeps the session alive to rebind it in another window).
    fn take_pane_in(&mut self, ti: usize, idx: usize) -> Option<(PaneState, bool)> {
        if ti >= self.tabs.len() {
            return None;
        }
        let palette = self.settings.frame_palette;
        let t = &mut self.tabs[ti];
        if idx >= t.panes.len() {
            return None;
        }
        let ps = t.panes.remove(idx);
        let auto = t.layout == Layout::Auto;
        t.sizes = if auto {
            equal_sizes(t.panes.len())
        } else {
            remove_size(&t.sizes, idx)
        };
        self.dirty = true;
        if t.panes.is_empty() {
            if self.tabs.len() <= 1 {
                // Last pane of the last tab → workspace emptied. Leave the empty tab in
                // place (the window is about to close).
                return Some((ps, false));
            }
            // Drop the now-empty tab and fix the active index.
            self.tabs.remove(ti);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if ti < self.active {
                self.active -= 1;
            }
            self.editing_tab = -1;
            return Some((ps, true));
        }
        let t = &mut self.tabs[ti];
        if t.focused >= t.panes.len() {
            t.focused = t.panes.len() - 1;
        } else if idx < t.focused {
            t.focused -= 1;
        }
        t.zoomed = match t.zoomed {
            Some(z) if z == idx => None,
            Some(z) if z > idx => Some(z - 1),
            other => other,
        };
        t.relabel(palette);
        Some((ps, true))
    }

    /// Close pane `idx` of tab `ti`, killing its session. An emptied tab is
    /// dropped; closing the last pane of the last tab returns `false` (caller
    /// quits). Works for background tabs too (used by self-exiting shells).
    pub fn close_pane_in(&mut self, ti: usize, idx: usize, mgr: &SessionManager) -> bool {
        match self.take_pane_in(ti, idx) {
            Some((ps, alive)) => {
                mgr.kill(&ps.uid);
                alive
            }
            None => true,
        }
    }

    /// Detach the focused pane of the active tab for re-hosting in another window:
    /// remove it **without** killing its session (the PTY stays alive centrally),
    /// returning the rebind info + whether this window still has panes. `None` when the
    /// active tab has no panes.
    pub fn detach_focused(&mut self, mgr: &SessionManager) -> Option<(DetachedPane, bool)> {
        let _ = mgr; // sessions are NOT touched here — that's the whole point of detach.
        let ti = self.active;
        let idx = self.tabs.get(ti)?.focused;
        let (ps, alive) = self.take_pane_in(ti, idx)?;
        Some((
            DetachedPane {
                uid: ps.uid,
                title: ps.title,
                subtitle: ps.subtitle,
                pinned_accent: ps.pinned_accent,
                show_frame: ps.show_frame,
                show_dot: ps.show_dot,
                font_px: ps.font_px,
            },
            alive,
        ))
    }

    /// Re-host a detached session at the end of the active tab (see [`Self::adopt_pane_at`]).
    pub fn adopt_pane(&mut self, mgr: &SessionManager, det: DetachedPane) {
        let at = self.active_tab().panes.len();
        self.adopt_pane_at(mgr, det, at);
    }

    /// Re-host a detached session in the active tab at insertion index `at`: build a fresh
    /// terminal grid, prime it from the session's **replay buffer** (recent scrollback — so
    /// no blank pane and no PTY restart), rebind it to the existing `uid`, and focus it.
    /// `at` is clamped to `0..=len`, so a stitch can insert the pane at a hovered slot.
    pub fn adopt_pane_at(&mut self, mgr: &SessionManager, det: DetachedPane, at: usize) {
        let palette = self.settings.frame_palette;
        let (cols, rows) = (80u16, 24u16);
        let mut pane =
            TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new()));
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        // Replay the rolling buffer so the re-hosted pane shows recent output instantly.
        if let Some(replay) = mgr.replay(&det.uid) {
            pane.feed(&replay);
        }
        let glow = Glow::new(crate::glow::seed_from(&det.uid));
        let font = theme::load_font_at(&self.settings.font_path(), det.font_px, self.last_scale);
        let ps = PaneState {
            uid: det.uid,
            title: det.title,
            subtitle: det.subtitle,
            show_frame: det.show_frame,
            show_dot: det.show_dot,
            accent: det.pinned_accent.unwrap_or_else(|| theme::accent_for(at, palette)),
            pane,
            applied: (cols as usize, rows as usize),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: true, // the session is already running — don't re-send any startup.
            startup: None,
            pinned_accent: det.pinned_accent,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            search_focus_seq: 0,
            font_px: det.font_px,
            font,
            font_dirty: false,
            cwd: None,
        };
        let auto = self.active_tab().layout == Layout::Auto;
        let t = self.active_tab_mut();
        let at = at.min(t.panes.len());
        t.sizes = if auto {
            equal_sizes(t.panes.len() + 1)
        } else {
            insert_size(&t.sizes, at)
        };
        t.panes.insert(at, ps);
        t.focused = at;
        t.zoomed = None;
        t.relabel(palette);
        self.dirty = true;
    }

    /// Re-host a detached session as a **brand-new tab** (dock-as-tab on a tear-off drop):
    /// append a fresh tab, switch to it, and adopt the pane into it.
    pub fn adopt_pane_as_tab(&mut self, mgr: &SessionManager, det: DetachedPane) {
        let tab = self.fresh_tab();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        self.adopt_pane(mgr, det);
    }

    /// Detach a **specific** pane (by `uid`) from wherever it lives (any tab) for re-hosting
    /// elsewhere — like [`Self::detach_focused`] but targets the dragged pane. Searching all
    /// tabs (not just the active one) keeps a drop correct even after the active tab changed
    /// mid-drag (e.g. a spring-load switched tabs). Returns the rebind info + whether this
    /// window still has panes; `None` if the uid isn't here. `take_pane_in` keeps the active
    /// tab pointing at the same tab across the removal.
    pub fn detach_uid(&mut self, uid: &str) -> Option<(DetachedPane, bool)> {
        let (ti, idx) = self.find_pane(uid)?;
        let (ps, alive) = self.take_pane_in(ti, idx)?;
        Some((
            DetachedPane {
                uid: ps.uid,
                title: ps.title,
                subtitle: ps.subtitle,
                pinned_accent: ps.pinned_accent,
                show_frame: ps.show_frame,
                show_dot: ps.show_dot,
                font_px: ps.font_px,
            },
            alive,
        ))
    }

    /// Whether the active tab currently hosts pane `uid` (used to choose reorder-in-place
    /// vs cross-tab move when a pane is dropped in the pane area).
    pub fn active_has_uid(&self, uid: &str) -> bool {
        self.active_tab().panes.iter().any(|p| p.uid == uid)
    }

    /// Move pane `from` to insertion index `to` within the active tab (in-window reorder),
    /// carrying its split size with it so the layout stays stable. Focus follows the moved
    /// pane. No-op when the move is a no-op or the indices are out of range.
    pub fn reorder_pane(&mut self, from: usize, to: usize) {
        let palette = self.settings.frame_palette;
        let t = self.active_tab_mut();
        let n = t.panes.len();
        if from >= n || to > n {
            return;
        }
        // Translate the insertion index into the post-removal slot.
        let dest = if to > from { to - 1 } else { to };
        if dest == from {
            return;
        }
        let pane = t.panes.remove(from);
        t.panes.insert(dest, pane);
        if t.sizes.len() == n {
            let s = t.sizes.remove(from);
            t.sizes.insert(dest, s);
        }
        t.focused = dest;
        t.zoomed = match t.zoomed {
            Some(z) if z == from => Some(dest),
            _ => t.zoomed,
        };
        t.relabel(palette);
        self.dirty = true;
    }

    /// Move tab `from` to index `to` (in-strip tab reorder), keeping the same tab active.
    pub fn reorder_tab(&mut self, from: usize, to: usize) {
        let n = self.tabs.len();
        if from >= n || to > n {
            return;
        }
        let dest = if to > from { to - 1 } else { to };
        if dest == from {
            return;
        }
        let active_title_idx = self.active;
        let tab = self.tabs.remove(from);
        self.tabs.insert(dest, tab);
        // Keep the previously-active tab active across the shuffle.
        self.active = if active_title_idx == from {
            dest
        } else {
            // recompute where the old active landed
            let mut a = active_title_idx;
            if from < a {
                a -= 1;
            }
            if dest <= a {
                a += 1;
            }
            a.min(self.tabs.len() - 1)
        };
        self.editing_tab = -1;
        self.dirty = true;
    }

    /// Every live session uid this window hosts (used to kill them when the window
    /// closes — in Wave 1 each session is referenced by exactly one window).
    pub fn session_uids(&self) -> Vec<String> {
        self.tabs
            .iter()
            .flat_map(|t| t.panes.iter().map(|p| p.uid.clone()))
            // Tabs parked on the closed-tab stack keep their sessions alive (for reopen), so
            // they must be killed when the window closes too — else they'd leak.
            .chain(
                self.closed_tabs
                    .iter()
                    .flat_map(|t| t.panes.iter().map(|p| p.uid.clone())),
            )
            .collect()
    }

    /// A session exited on its own — drop its pane wherever it lives. Returns
    /// `false` if that emptied the whole workspace (caller quits).
    pub fn pane_exited(&mut self, uid: &str, mgr: &SessionManager) -> bool {
        match self.find_pane(uid) {
            Some((ti, pi)) => self.close_pane_in(ti, pi, mgr),
            None => true,
        }
    }

    pub fn focus_pane(&mut self, idx: usize) {
        // Clicking into a pane cancels any in-progress tab rename.
        if self.editing_tab != -1 {
            self.editing_tab = -1;
            self.dirty = true;
        }
        let t = self.active_tab_mut();
        if idx < t.panes.len() && t.focused != idx {
            t.focused = idx;
            if t.zoomed.is_some() {
                t.zoomed = Some(idx); // zoom follows focus
            }
            self.dirty = true;
        }
    }

    /// Move focus in `dir`. When soloed (zoom, fullscreen, or single), cycle the pane order.
    pub fn focus_dir(&mut self, dir: Direction) {
        let fullscreen = self.fullscreen;
        let t = self.active_tab_mut();
        let n = t.panes.len();
        if n < 2 {
            return;
        }
        let eff = t.effective();
        let next = if t.zoomed.is_some() || fullscreen || eff == Layout::Single {
            let delta = matches!(dir, Direction::Right | Direction::Down);
            Some(if delta {
                (t.focused + 1) % n
            } else {
                (t.focused + n - 1) % n
            })
        } else {
            let tiles = compute_tiles(eff, n, &t.sizes, t.main_fraction, t.focused as i32);
            neighbor_index(&tiles, t.focused, dir)
        };
        if let Some(next) = next {
            t.focused = next;
            if t.zoomed.is_some() {
                t.zoomed = Some(next);
            }
            self.dirty = true;
        }
    }

    // ---- layout / zoom ----

    pub fn set_layout(&mut self, layout: Layout) {
        let t = self.active_tab_mut();
        if t.layout != layout {
            t.layout = layout;
            self.dirty = true;
        }
    }

    /// Toggle zoom (maximise-in-tab) of the focused pane.
    pub fn toggle_zoom(&mut self) {
        let t = self.active_tab_mut();
        if t.panes.is_empty() {
            return;
        }
        let f = t.focused;
        t.zoomed = if t.zoomed == Some(f) { None } else { Some(f) };
        self.dirty = true;
    }

    /// Drag a divider: move the boundary by `delta` (a fraction of the area).
    /// Resizing an `auto` tab promotes it to the concrete preset it was showing,
    /// so the dragged sizes stick (mirrors the React Divider, Q7).
    pub fn resize_divider(&mut self, kind: DividerKind, index: i32, delta: f64) {
        let n = self.active_tab().panes.len();
        let eff = self.active_tab().effective();
        let t = self.active_tab_mut();
        if t.layout == Layout::Auto {
            t.layout = eff;
            if t.sizes.len() != n {
                t.sizes = equal_sizes(n);
            }
        }
        match kind {
            DividerKind::Main => {
                let before = t.main_fraction;
                t.main_fraction = clamp_fraction(t.main_fraction + delta);
                crate::dbg_log(&format!(
                    "    resize main: {before:.3} + {delta:.4} -> {:.3} (layout={:?})",
                    t.main_fraction, t.layout
                ));
            }
            DividerKind::Size => {
                if index >= 0 {
                    let before = t.sizes.clone();
                    t.sizes = resize_at(&t.sizes, index as usize, delta);
                    crate::dbg_log(&format!(
                        "    resize sizes[{index}] delta={delta:.4}: {before:?} -> {:?} (layout={:?})",
                        t.sizes, t.layout
                    ));
                }
            }
        }
        self.dirty = true;
    }

    /// Whether the active tab tiles as rows (so a stitch edge band runs along the
    /// vertical axis → top/bottom rather than left/right). Used by the drag hit-test.
    pub fn active_is_rows(&self) -> bool {
        self.active_tab().effective() == Layout::Rows
    }

    /// The current active tab's draggable dividers (empty when zoomed or fullscreen — both
    /// solo a single pane, so there are no seams to drag).
    pub fn dividers(&self) -> Vec<hyperpanes_core::layout::presets::DividerDesc> {
        let t = self.active_tab();
        if t.zoomed.is_some() || self.fullscreen {
            return Vec::new();
        }
        compute_dividers(t.effective(), t.panes.len(), &t.sizes, t.main_fraction)
    }

    // ---- tabs ----

    pub fn new_tab(&mut self, mgr: &SessionManager) {
        let tab = self.fresh_tab();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.add_pane(mgr); // seed one shell so the tab is usable
        self.editing_tab = -1;
        self.dirty = true;
    }

    /// Close tab `idx`, killing its sessions. Returns `false` if nothing remains
    /// (caller quits the window).
    pub fn close_tab(&mut self, idx: usize, mgr: &SessionManager) -> bool {
        if idx >= self.tabs.len() {
            return true;
        }
        if self.tabs.len() <= 1 {
            // Last tab: kill its sessions and signal quit.
            for p in &self.tabs[idx].panes {
                mgr.kill(&p.uid);
            }
            return false;
        }
        let tab = self.tabs.remove(idx);
        for p in &tab.panes {
            mgr.kill(&p.uid);
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.editing_tab = -1;
        self.dirty = true;
        true
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() && idx != self.active {
            self.active = idx;
            self.editing_tab = -1;
            self.dirty = true;
        }
    }

    /// Move the active tab by `delta` (±1), wrapping around — the Ctrl+Tab / Ctrl+Shift+Tab
    /// keybindings. No-op with a single tab.
    pub fn cycle_tab(&mut self, delta: i32) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let next = (self.active as i32 + delta).rem_euclid(n as i32) as usize;
        self.switch_tab(next);
    }

    /// Nudge the FOCUSED pane's terminal font size by `delta` px (clamped) — the Ctrl+= /
    /// Ctrl+- font-zoom keybindings (and Ctrl+wheel). Zoom is per-pane (Electron parity): only
    /// the focused pane re-grids; its neighbours keep their own size. The pump reloads the
    /// pane's font at the current DPI scale (via `font_dirty`) and re-flows it.
    pub fn font_zoom(&mut self, delta: i32) {
        let f = self.active_tab().focused;
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            let next = Settings::clamp_font(p.font_px + delta as f32);
            if next != p.font_px {
                p.font_px = next;
                p.font_dirty = true;
            }
        }
        self.show_zoom_toast();
    }

    /// Reset the FOCUSED pane's terminal font size to the configured base — the Ctrl+0
    /// keybinding. Only the focused pane is affected (per-pane zoom).
    pub fn font_reset(&mut self) {
        let base = self.settings.font_px;
        let f = self.active_tab().focused;
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            if p.font_px != base {
                p.font_px = base;
                p.font_dirty = true;
            }
        }
        self.show_zoom_toast();
    }

    /// Flash the focused pane's current zoom as a `%` indicator in its bottom-right — the same
    /// transient "toast" the widget uses for copy/paste confirmations. Percentage is relative
    /// to the default font size, matching the Electron zoom badge.
    fn show_zoom_toast(&mut self) {
        let f = self.active_tab().focused;
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            let pct = (p.font_px / prefs::DEFAULT_FONT_PX * 100.0).round() as i32;
            p.pane.set_toast(format!("{pct}%"));
        }
        self.dirty = true;
    }

    pub fn begin_rename(&mut self, idx: i32) {
        if idx >= 0 && (idx as usize) < self.tabs.len() {
            self.editing_tab = idx;
            self.dirty = true;
        }
    }

    pub fn rename_tab(&mut self, idx: i32, title: &str) {
        if idx >= 0 && (idx as usize) < self.tabs.len() {
            let title = title.trim();
            if !title.is_empty() {
                self.tabs[idx as usize].title = title.into();
            }
        }
        self.editing_tab = -1;
        self.dirty = true;
    }

    /// Begin editing pane `idx`'s label inline (double-click on its header). Cancels any
    /// in-progress tab rename first.
    pub fn begin_rename_pane(&mut self, idx: i32) {
        self.editing_tab = -1;
        if idx >= 0 && (idx as usize) < self.active_tab().panes.len() {
            self.editing_pane = idx;
            self.dirty = true;
        }
    }

    /// Commit a pane label rename (blank keeps the prior label, mirroring the renderer).
    pub fn rename_pane(&mut self, idx: i32, title: &str) {
        if idx >= 0 && (idx as usize) < self.active_tab().panes.len() {
            let title = title.trim();
            if !title.is_empty() {
                self.active_tab_mut().panes[idx as usize].title = title.into();
            }
        }
        self.editing_pane = -1;
        self.dirty = true;
    }

    /// A pane reported a cwd: refresh the remembered-projects list (the sidebar), AND — if
    /// that cwd sits inside a git repo — TINT this specific pane to the project (the native
    /// port of `applyProjectToPane`): adopt the project color, turn the per-pane frame + dot
    /// ON, and rename the pane to the repo name **only if its label is still a default**.
    /// A clean pane outside any repo is left untouched (stays frame/dot OFF).
    /// Returns the resolved git project (root path + name) so the caller can feed the
    /// ambient-AI engine's `on_cwd`; `None` when the cwd isn't inside a git repo.
    pub fn note_pane_cwd(&mut self, uid: &str, cwd: &str) -> Option<AiProjectRef> {
        let root = sidebar::git_root_of(cwd)?;
        let project = projects::upsert_project_by_root(&root.to_string_lossy());
        let color = parse_hex(&project.color);
        if let Some((ti, pi)) = self.find_pane(uid) {
            let p = &mut self.tabs[ti].panes[pi];
            p.accent = color;
            p.pinned_accent = Some(color);
            p.show_frame = Some(true);
            p.show_dot = Some(true);
            if is_default_label(&p.title) {
                p.title = project.name.clone().into();
            }
            // Drop a subtitle that merely duplicated the repo name (it's now the label).
            if p.subtitle.as_deref() == Some(project.name.as_str()) {
                p.subtitle = None;
            }
        }
        // Refresh the cached, newest-first project list (rail badge + flyout).
        self.projects = sidebar::list();
        self.dirty = true;
        Some(AiProjectRef {
            path: root.to_string_lossy().to_string(),
            name: project.name,
        })
    }

    /// Apply an ambient-AI subtitle produced by the engine to the pane with session `uid`
    /// (the typewriter reveal restarts when the text changes). No-op for an unknown uid.
    pub fn set_ai_subtitle(&mut self, uid: &str, text: &str) {
        if let Some((ti, pi)) = self.find_pane(uid) {
            self.tabs[ti].panes[pi].ai.set_target(text);
        }
    }

    /// The ambient-AI watch list for this window: one entry per pane across all tabs, keyed
    /// by its session uid (used as the stable pane id), carrying the current label + Mute
    /// flag so the engine can summarise unmuted panes and clear muted ones.
    pub fn ai_pane_publish(&self) -> Vec<hyperpanes_core::ai::service::AiPanePublish> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            for p in &tab.panes {
                out.push(hyperpanes_core::ai::service::AiPanePublish {
                    pane_id: p.uid.clone(),
                    session_uid: p.uid.clone(),
                    label: p.title.to_string(),
                    muted: p.ai_muted,
                });
            }
        }
        out
    }

    /// A cheap signature of the AI watch list (uid + label + mute), so the controller only
    /// re-publishes the pane context when something the engine cares about changed.
    pub fn ai_context_sig(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        };
        for tab in &self.tabs {
            for p in &tab.panes {
                mix(p.uid.as_bytes());
                mix(b"\x1f");
                mix(p.title.as_bytes());
                mix(if p.ai_muted { b"\x01" } else { b"\x00" });
                mix(b"\x1e");
            }
        }
        h
    }

    pub fn set_fullscreen(&mut self, on: bool) {
        if self.fullscreen != on {
            self.fullscreen = on;
            self.dirty = true;
        }
    }

    // ---- Wave-2: overlay panels (Seam #3) ----

    /// Whether any overlay panel is currently mounted.
    pub fn overlay_open(&self) -> bool {
        self.overlay != Overlay::None
    }

    /// Close whatever overlay is open. Preferences routes through the appearance
    /// save/discard guard (Esc / scrim click); every other overlay closes immediately.
    pub fn close_overlay(&mut self) {
        if self.overlay == Overlay::Prefs {
            self.prefs_request_close();
            return;
        }
        self.close_overlay_now();
    }

    /// Actually tear down the overlay (clears any appearance draft + confirm prompt).
    fn close_overlay_now(&mut self) {
        if self.overlay != Overlay::None {
            self.overlay = Overlay::None;
            self.prefs_draft = None;
            self.prefs_confirm = false;
            self.font_custom = false;
            self.capturing_binding = None;
            self.capture_conflict = None;
            self.dirty = true;
        }
    }

    // ---- New Pane dialog ----

    /// Open the "New pane" options dialog (Shift+＋ / the menus' "New pane…").
    pub fn open_new_pane(&mut self) {
        self.overlay = Overlay::NewPane;
        self.dirty = true;
    }

    /// The active frame palette's 8 swatches (the New Pane dialog's color row). The default
    /// swatch index the dialog seeds is the next palette-rotation slot (mirrors the
    /// renderer's `nextColor(seq)`), computed in the resync from `panes.len()`.
    pub fn frame_swatches(&self) -> Vec<Color> {
        theme::frame_palette(self.settings.frame_palette)
            .iter()
            .map(|(r, g, b)| Color::from_rgb_u8(*r, *g, *b))
            .collect()
    }

    // ---- command palette ----

    /// Open the palette: snapshot the command registry from current state, reset the
    /// query + selection. Rebuilt every open so pane/layout entries stay fresh.
    pub fn open_palette(&mut self) {
        self.palette_entries = palette::build(self);
        self.palette_query.clear();
        self.palette_view = (0..self.palette_entries.len()).collect();
        self.palette_sel = 0;
        self.overlay = Overlay::Palette;
        self.dirty = true;
    }

    /// Update the palette query → refilter + re-rank, keeping the selection in range.
    pub fn palette_set_query(&mut self, query: &str) {
        self.palette_query = query.to_string();
        self.palette_view = palette::filter(&self.palette_entries, query);
        self.palette_sel = 0;
        self.dirty = true;
    }

    /// Move the palette selection by `delta` rows, clamped to the visible results.
    pub fn palette_nav(&mut self, delta: i32) {
        let n = self.palette_view.len();
        if n == 0 {
            return;
        }
        let cur = self.palette_sel as i32;
        let next = (cur + delta).clamp(0, n as i32 - 1);
        if next as usize != self.palette_sel {
            self.palette_sel = next as usize;
            self.dirty = true;
        }
    }

    /// Set the palette selection to a specific visible row (e.g. a mouse click).
    pub fn palette_select(&mut self, idx: usize) {
        if idx < self.palette_view.len() && idx != self.palette_sel {
            self.palette_sel = idx;
            self.dirty = true;
        }
    }

    /// The command for the currently-highlighted palette row (consumed on activate).
    pub fn palette_command(&self) -> Option<Command> {
        let entry = self.palette_view.get(self.palette_sel)?;
        self.palette_entries.get(*entry).map(|e| e.command.clone())
    }

    /// The visible palette rows as `(title, subtitle)` pairs, in display order.
    pub fn palette_rows(&self) -> Vec<(SharedString, SharedString)> {
        self.palette_view
            .iter()
            .filter_map(|i| self.palette_entries.get(*i))
            .map(|e| (e.title.as_str().into(), e.subtitle.as_str().into()))
            .collect()
    }

    // ---- preferences ----

    pub fn open_prefs(&mut self) {
        self.overlay = Overlay::Prefs;
        // Snapshot the appearance settings into a draft so edits preview without touching
        // the live panes until Done.
        self.prefs_draft = Some(PrefsDraft::from_settings(&self.settings));
        self.prefs_confirm = false;
        self.font_custom = prefs::is_custom_font(&self.settings.font_family);
        self.dirty = true;
    }

    /// Font picker: select option `idx` from `prefs::FONT_OPTIONS`, or enter "Custom…" mode
    /// when `idx` is the trailing Custom entry (== `FONT_OPTIONS.len()`). Edits the draft.
    pub fn font_select(&mut self, idx: usize) {
        let Some(d) = self.prefs_draft.as_mut() else { return };
        if let Some((_, value)) = prefs::FONT_OPTIONS.get(idx) {
            d.font_family = value.to_string();
            self.font_custom = false;
        } else {
            // Custom… — start from an empty field unless the current value is already custom.
            if !prefs::is_custom_font(&d.font_family) {
                d.font_family.clear();
            }
            self.font_custom = true;
        }
        self.dirty = true;
    }

    /// Font picker: set the custom font path typed in the "Custom…" field (edits the draft).
    pub fn font_custom_value(&mut self, value: String) {
        if let Some(d) = self.prefs_draft.as_mut() {
            d.font_family = value;
            self.font_custom = true;
            self.dirty = true;
        }
    }

    /// The appearance values the dialog should display: the draft while Preferences is open,
    /// else the committed settings. Returns `(resolved_font_path, frame_palette, terminal_theme,
    /// font_px, show_frame, show_dot)`.
    pub fn appearance_view(&self) -> (String, usize, usize, f32, bool, bool) {
        match &self.prefs_draft {
            Some(d) => (
                prefs::resolve_or_default(&d.font_family),
                d.frame_palette,
                d.terminal_theme,
                d.font_px,
                d.show_frame,
                d.show_dot,
            ),
            None => (
                self.settings.font_path(),
                self.settings.frame_palette,
                self.settings.terminal_theme,
                self.settings.font_px,
                self.settings.show_frame,
                self.settings.show_dot,
            ),
        }
    }

    /// Edit the appearance **draft** (no live change). Used for the appearance settings while
    /// the dialog is open; a no-op if there's no draft or `s` isn't an appearance setting.
    pub fn draft_setting(&mut self, s: Setting) {
        let Some(d) = self.prefs_draft.as_mut() else { return };
        match s {
            Setting::FontFamily(path) => d.font_family = path,
            Setting::FramePalette(idx) => d.frame_palette = idx,
            Setting::TerminalTheme(idx) => d.terminal_theme = idx,
            Setting::FontDelta(delta) => d.font_px = Settings::clamp_font(d.font_px + delta as f32),
            Setting::ShowFrame(on) => d.show_frame = on,
            Setting::ShowDot(on) => d.show_dot = on,
            // Non-appearance settings never reach the draft.
            Setting::DefaultShell(_)
            | Setting::ClickablePaths(_)
            | Setting::EditorCommand(_)
            | Setting::IdleAlert(_)
            | Setting::IdleEffect(_)
            | Setting::IdleSeconds(_) => {}
        }
        self.dirty = true;
    }

    /// Whether the appearance draft differs from the committed settings (un-applied edits).
    pub fn prefs_dirty(&self) -> bool {
        match &self.prefs_draft {
            Some(d) => *d != PrefsDraft::from_settings(&self.settings),
            None => false,
        }
    }

    /// Commit the appearance draft to the live settings (Done / Save): apply each changed
    /// field via [`Self::apply_setting`] so font reload + palette remap happen, then close.
    pub fn prefs_done(&mut self) {
        if let Some(d) = self.prefs_draft.take() {
            if d.font_family != self.settings.font_family {
                self.apply_setting(Setting::FontFamily(d.font_family.clone()));
            }
            if d.frame_palette != self.settings.frame_palette {
                self.apply_setting(Setting::FramePalette(d.frame_palette));
            }
            if d.terminal_theme != self.settings.terminal_theme {
                self.apply_setting(Setting::TerminalTheme(d.terminal_theme));
            }
            if d.font_px != self.settings.font_px {
                // Apply the absolute drafted size (apply_setting takes a delta).
                self.apply_setting(Setting::FontDelta(
                    (d.font_px - self.settings.font_px).round() as i32,
                ));
            }
            if d.show_frame != self.settings.show_frame {
                self.apply_setting(Setting::ShowFrame(d.show_frame));
            }
            if d.show_dot != self.settings.show_dot {
                self.apply_setting(Setting::ShowDot(d.show_dot));
            }
        }
        self.close_overlay_now();
    }

    /// Esc / scrim click while Preferences is open: prompt to save/discard if there are
    /// un-applied appearance edits, otherwise just close (discarding the empty draft).
    pub fn prefs_request_close(&mut self) {
        if self.prefs_dirty() {
            self.prefs_confirm = true;
            self.dirty = true;
        } else {
            self.close_overlay_now();
        }
    }

    /// Resolve the save/discard prompt: 0 = keep editing · 1 = discard · 2 = save.
    pub fn prefs_confirm_resolve(&mut self, action: i32) {
        match action {
            0 => {
                self.prefs_confirm = false;
                self.dirty = true;
            }
            1 => self.close_overlay_now(),       // discard the draft
            2 => self.prefs_done(),              // commit the draft
            _ => {}
        }
    }

    /// Apply a single preferences edit: mutate the settings, persist the blob, and flag
    /// a resync (font edits additionally request a font reload on the next pump).
    pub fn apply_setting(&mut self, s: Setting) {
        match s {
            Setting::FontFamily(path) => {
                if self.settings.font_family != path {
                    self.settings.font_family = path;
                    self.font_reload = true;
                }
            }
            Setting::FramePalette(idx) => {
                if self.settings.frame_palette != idx {
                    self.settings.frame_palette = idx;
                    // Recompute every pane's accent against the new palette (by creation
                    // slot); pinned project colors are preserved by `relabel`.
                    for t in &mut self.tabs {
                        t.relabel(idx);
                    }
                }
            }
            Setting::TerminalTheme(idx) => {
                if self.settings.terminal_theme != idx {
                    self.settings.terminal_theme = idx;
                    // Repaint every open pane with the new colour theme.
                    let theme = theme::terminal_theme(idx);
                    for t in &mut self.tabs {
                        for p in &mut t.panes {
                            p.pane.set_palette(theme);
                        }
                    }
                }
            }
            Setting::FontDelta(d) => {
                let next = Settings::clamp_font(self.settings.font_px + d as f32);
                if next != self.settings.font_px {
                    self.settings.font_px = next;
                    // The Appearance font-size pref sets the base for everything: re-base every
                    // pane to it (resetting any per-pane zoom), then reload all fonts.
                    for t in &mut self.tabs {
                        for p in &mut t.panes {
                            p.font_px = next;
                        }
                    }
                    self.font_reload = true;
                }
            }
            Setting::DefaultShell(shell) => self.settings.default_shell = shell,
            Setting::ShowFrame(on) => self.settings.show_frame = on,
            Setting::ShowDot(on) => self.settings.show_dot = on,
            Setting::ClickablePaths(on) => self.settings.clickable_paths = on,
            Setting::EditorCommand(cmd) => self.settings.editor_command = cmd,
            Setting::IdleAlert(on) => self.settings.idle_alert = on,
            Setting::IdleEffect(idx) => {
                if let Some((token, _)) = crate::glow::IdleEffect::OPTIONS.get(idx) {
                    self.settings.idle_effect = (*token).to_string();
                }
            }
            Setting::IdleSeconds(d) => {
                // `d` is a ±1 step; the dial moves in whole IDLE_STEP_SECONDS jumps and
                // snaps any odd persisted value onto the grid.
                let step = prefs::IDLE_STEP_SECONDS as i32;
                let cur = self.settings.idle_alert_seconds as i32;
                let steps = (cur / step + d)
                    .clamp(prefs::MIN_IDLE_SECONDS as i32 / step, prefs::MAX_IDLE_SECONDS as i32 / step);
                self.settings.idle_alert_seconds = (steps * step) as u32;
            }
        }
        prefs::save(&self.settings);
        self.dirty = true;
    }

    // ---- keybindings editor (Preferences → Keybindings) ----

    /// Begin capturing a new chord for binding `id`: the editor row grabs focus and shows
    /// "Press a chord…"; the next captured combo rebinds it (or Esc cancels).
    pub fn begin_rebind(&mut self, id: &str) {
        self.capturing_binding = Some(id.to_string());
        self.capture_conflict = None;
        self.dirty = true;
    }

    /// Cancel an in-progress chord capture (Esc, or clicking elsewhere).
    pub fn cancel_rebind(&mut self) {
        self.capture_conflict = None;
        if self.capturing_binding.take().is_some() {
            self.dirty = true;
        }
    }

    /// Handle a key captured while rebinding: Escape cancels, a bare modifier is ignored (keep
    /// waiting for a real key), and any other combo becomes the binding's override (persisted)
    /// and ends the capture. If the combo is already held by another binding it is **stolen** —
    /// that binding becomes unbound (its row then shows "Unbound"). No-op when not capturing.
    pub fn capture_chord(&mut self, ctrl: bool, alt: bool, shift: bool, text: &str) {
        let Some(id) = self.capturing_binding.clone() else {
            return;
        };
        if crate::is_key(text, slint::platform::Key::Escape) {
            self.cancel_rebind();
            return;
        }
        // A bare modifier press has no key token yet — keep the capture open.
        let Some(key) = crate::key_tok_from_text(text, ctrl) else {
            return;
        };
        let chord = crate::keybindings::Chord { ctrl, alt, shift, key };
        // Steal the chord from its current owner (if any) — that binding becomes unbound.
        if let Some(other) = self.keymap.owner_of(chord, &id) {
            self.keymap.unbind(other);
        }
        self.keymap.set(&id, chord);
        self.capturing_binding = None;
        self.capture_conflict = None;
        self.dirty = true;
    }

    /// Unbind binding `id` (clear its chord — nothing fires it) and end any capture on it. The
    /// row then shows "Unbound" until rebound or reset to default.
    pub fn unbind_binding(&mut self, id: &str) {
        self.keymap.unbind(id);
        if self.capturing_binding.as_deref() == Some(id) {
            self.capturing_binding = None;
            self.capture_conflict = None;
        }
        self.dirty = true;
    }

    /// Reset binding `id` to its default chord (drop the override/unbind).
    pub fn reset_binding(&mut self, id: &str) {
        self.keymap.reset(id);
        if self.capturing_binding.as_deref() == Some(id) {
            self.capturing_binding = None;
            self.capture_conflict = None;
        }
        self.dirty = true;
    }

    /// Reset every binding to its default (clear all overrides).
    pub fn reset_all_bindings(&mut self) {
        self.keymap.reset_all();
        self.capturing_binding = None;
        self.capture_conflict = None;
        self.dirty = true;
    }

    /// Reload the base font + EVERY pane's font (each at its own per-pane `font_px`) from the
    /// current settings at DPI `scale`, forcing every pane to re-grid at the new cell metrics
    /// (resets each pane's `applied`). Called by the pump (which owns the scale) on a DPI
    /// change or when `font_reload` is set (a font-family / base-size pref change).
    pub fn reload_font(&mut self, scale: f32) {
        let path = self.settings.font_path();
        self.font = theme::load_font_at(&path, self.settings.font_px, scale);
        for t in &mut self.tabs {
            for p in &mut t.panes {
                p.font = theme::load_font_at(&path, p.font_px, scale);
                p.font_dirty = false;
                p.applied = (0, 0); // force a reflow at the new cell size
            }
        }
        self.font_reload = false;
        self.dirty = true;
    }

    // ---- clickable paths (terminal link hover / activation) ----

    /// Record a pane's on-screen terminal-surface size (logical px) from the widget's
    /// `geometry-changed`, used to hit-test link coordinates. `idx` is an active-tab pane.
    pub fn set_pane_surf(&mut self, idx: usize, w: f32, h: f32) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.surf = (w, h);
        }
    }

    /// Hover hit-test for a clickable path under the cursor (logical px within the pane
    /// surface). Updates the pane's link-overlay state. No-op (and clears any link) when
    /// clickable paths are disabled. `idx` is an active-tab pane.
    pub fn pane_link_moved(&mut self, idx: usize, x: f32, y: f32) {
        let on = self.settings.clickable_paths;
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if !on {
                if p.link.is_some() {
                    p.link = None;
                    self.dirty = true;
                }
                return;
            }
            let (w, h) = p.surf;
            let hit = p.pane.link_at(x, y, w, h);
            // Only repaint when the hovered link actually changes.
            if hit != p.link {
                p.link = hit;
                p.link_cursor = (x, y);
                self.dirty = true;
            } else if p.link.is_some() {
                p.link_cursor = (x, y); // keep the tooltip tracking the cursor
            }
        }
    }

    /// Clear a pane's hover link when the pointer leaves its surface.
    pub fn pane_link_exited(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if p.link.take().is_some() {
                self.dirty = true;
            }
        }
    }

    /// Activate the link under a click: open (plain) or copy (ctrl). Returns the action so
    /// the caller can touch the OS (clipboard / launch). `None` when clickable paths are off
    /// or the click missed a verified path. `idx` is an active-tab pane.
    pub fn pane_link_activate(
        &mut self,
        idx: usize,
        x: f32,
        y: f32,
        ctrl: bool,
    ) -> Option<hyperpanes_terminal_widget::LinkAction> {
        if !self.settings.clickable_paths {
            return None;
        }
        let editor = self.settings.editor_command.clone();
        let p = self.active_tab_mut().panes.get_mut(idx)?;
        let (w, h) = p.surf;
        p.pane.activate_link(x, y, w, h, ctrl, &editor)
    }

    // ---- sidebar / projects ----

    /// Show/hide the whole right-edge rail (`Ctrl+Shift+B`, the ☰ menu, the palette).
    /// Persisted like the other appearance prefs; hiding it also collapses the flyout.
    pub fn toggle_sidebar(&mut self) {
        self.settings.show_sidebar = !self.settings.show_sidebar;
        if !self.settings.show_sidebar {
            self.sidebar_open = false;
        }
        prefs::save(&self.settings);
        self.dirty = true;
    }

    /// Toggle the projects flyout behind the 📁 icon; refresh the list when opening it.
    pub fn toggle_projects(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open {
            self.projects = sidebar::list();
        }
        self.dirty = true;
    }

    /// The cached project rows as `(name, color)` for the flyout.
    pub fn project_rows(&self) -> Vec<(SharedString, Color)> {
        self.projects
            .iter()
            .map(|p| (p.name.as_str().into(), parse_hex(&p.color)))
            .collect()
    }

    /// Open project `idx` (from the flyout) in a new pane cd'd into its repo, focused.
    /// Collapses the flyout afterwards (mirrors the Electron click behaviour).
    pub fn open_project(&mut self, idx: usize, mgr: &SessionManager) {
        let Some(p) = self.projects.get(idx).cloned() else {
            return;
        };
        self.sidebar_open = false;
        self.add_pane_cwd(mgr, Some(p.path.clone()), Some(parse_hex(&p.color)));
    }

    /// Recolor project at flyout row `idx` to palette swatch `swatch`, persist via core,
    /// and refresh the cache so the dot updates immediately.
    pub fn set_project_color(&mut self, idx: usize, swatch: usize) {
        let Some(p) = self.projects.get(idx) else { return };
        let Some(color) = projects::PROJECT_COLORS.get(swatch) else { return };
        projects::set_project_color(&p.id, color);
        self.projects = sidebar::list();
        self.dirty = true;
    }

    /// Rename project at flyout row `idx` (no-op on an empty/unchanged name).
    pub fn rename_project(&mut self, idx: usize, name: &str) {
        let name = name.trim();
        let Some(p) = self.projects.get(idx) else { return };
        if name.is_empty() || name == p.name {
            return;
        }
        let id = p.id.clone();
        projects::rename_project(&id, name);
        self.projects = sidebar::list();
        self.dirty = true;
    }

    /// Forget project at flyout row `idx`.
    pub fn remove_project(&mut self, idx: usize) {
        let Some(p) = self.projects.get(idx) else { return };
        projects::remove_project(&p.id);
        self.projects = sidebar::list();
        self.dirty = true;
    }

    /// Record an Escape key event and decide what to do with it. A lone tap
    /// forwards to the shell; holding Escape (rapid auto-repeat) while in
    /// fullscreen sets [`Self::esc_holding`] (so the hint + its progress fill
    /// appear) and, after [`HOLD`], leaves fullscreen. The repeat tail is
    /// swallowed so the hold doesn't spam the shell with escapes.
    pub fn note_esc(&mut self) -> EscOutcome {
        // A gap under this means "still held" (auto-repeat — incl. the OS's
        // initial repeat delay); a longer gap starts a fresh tap.
        const RAPID: std::time::Duration = std::time::Duration::from_millis(600);
        // How long to hold (from the first repeat) before leaving fullscreen.
        const HOLD: std::time::Duration = std::time::Duration::from_millis(600);

        let now = Instant::now();
        let cont = self.esc_last.is_some_and(|l| now.duration_since(l) < RAPID);
        self.esc_last = Some(now);

        if !cont {
            // Fresh tap → goes to the shell.
            if self.esc_holding {
                self.dirty = true;
            }
            self.esc_holding = false;
            self.esc_hold_start = None;
            self.esc_fired = false;
            return EscOutcome::Forward;
        }

        // Continuation (held). Start the progress clock on the first repeat.
        if !self.esc_holding {
            self.esc_holding = true;
            self.esc_hold_start = Some(now);
            self.dirty = true;
        }
        if self.fullscreen
            && !self.esc_fired
            && self.esc_hold_start.is_some_and(|s| now.duration_since(s) >= HOLD)
        {
            self.esc_fired = true;
            self.esc_holding = false;
            self.dirty = true;
            return EscOutcome::Exit;
        }
        EscOutcome::Ignore
    }

    /// Clear the held-Esc state once the auto-repeat stops (no key-release event
    /// reaches us, so we time it out). Returns whether anything changed.
    pub fn tick_esc(&mut self) -> bool {
        const RELEASE: std::time::Duration = std::time::Duration::from_millis(250);
        if self.esc_holding && self.esc_last.is_some_and(|l| l.elapsed() >= RELEASE) {
            self.esc_holding = false;
            self.esc_hold_start = None;
            self.esc_fired = false;
            self.dirty = true;
            return true;
        }
        false
    }
}

/// Phase-5: context menus + the actions they invoke. Kept in its own `impl` block so the
/// right-click feature reads as one unit (it only ever calls the same mutate→set-dirty seam).
impl State {
    // ---- context-menu lifecycle ----

    /// Open the pane header menu for active-tab pane `idx`, anchored at window-logical `(x, y)`.
    /// Built fresh so gating + checkmarks reflect the moment of the right-click; never changes
    /// the focused pane or active tab.
    pub fn open_pane_context(&mut self, idx: usize, x: f32, y: f32) {
        if idx < self.active_tab().panes.len() {
            self.ctx = Some(crate::contextmenu::pane_menu(self, idx, x, y, false));
            self.dirty = true;
        }
    }

    /// Open the single-layout taskbar's pane menu for pane `idx` (the `inTaskbar` variant:
    /// a leading Show row, no Maximize), anchored at window-logical `(x, y)`.
    pub fn open_taskbar_context(&mut self, idx: usize, x: f32, y: f32) {
        if idx < self.active_tab().panes.len() {
            self.ctx = Some(crate::contextmenu::pane_menu(self, idx, x, y, true));
            self.dirty = true;
        }
    }

    /// Open the application (hamburger) menu, anchored at window-logical `(x, y)`.
    pub fn open_app_context(&mut self, x: f32, y: f32) {
        self.ctx = Some(crate::contextmenu::app_menu(self, x, y));
        self.dirty = true;
    }

    /// Open the tab-strip menu for tab `idx`, anchored at window-logical `(x, y)`.
    pub fn open_tab_context(&mut self, idx: usize, x: f32, y: f32) {
        if idx < self.tabs.len() {
            self.ctx = Some(crate::contextmenu::tab_menu(self, idx, x, y));
            self.dirty = true;
        }
    }

    /// Dismiss the open context menu (select / Esc / click-away).
    pub fn close_context(&mut self) {
        if self.ctx.take().is_some() {
            self.dirty = true;
        }
    }

    /// Whether a context menu is currently open.
    pub fn ctx_open(&self) -> bool {
        self.ctx.is_some()
    }

    /// The command bound to top-level context row `row`, if any (separators / submenu headers
    /// have none).
    pub fn ctx_command(&self, row: usize) -> Option<Command> {
        self.ctx.as_ref().and_then(|c| c.commands.get(row).cloned().flatten())
    }

    /// The open menu's target index (pane idx for a pane menu, tab idx for a tab menu).
    pub fn ctx_target(&self) -> Option<usize> {
        self.ctx.as_ref().map(|c| c.target)
    }

    // ---- pane chrome actions ----

    /// Recolor active-tab pane `idx` to swatch `swatch` of the current frame palette: adopt the
    /// color, pin it, and turn the per-pane frame + dot ON (mirrors `ColorSwatches`' pickColor).
    pub fn recolor_pane(&mut self, idx: usize, swatch: usize) {
        let palette = self.settings.frame_palette;
        let colors = theme::frame_palette(palette);
        let Some((r, g, b)) = colors.get(swatch).copied() else {
            return;
        };
        let color = Color::from_rgb_u8(r, g, b);
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.accent = color;
            p.pinned_accent = Some(color);
            p.show_frame = Some(true);
            p.show_dot = Some(true);
            self.dirty = true;
        }
    }

    /// Set pane `idx`'s per-pane frame override.
    pub fn set_pane_frame(&mut self, idx: usize, on: bool) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.show_frame = Some(on);
            self.dirty = true;
        }
    }

    /// Set pane `idx`'s per-pane dot override.
    pub fn set_pane_dot(&mut self, idx: usize, on: bool) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.show_dot = Some(on);
            self.dirty = true;
        }
    }

    /// Toggle whether pane `idx`'s ambient-AI summary line is muted.
    pub fn toggle_mute_ai(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.ai_muted = !p.ai_muted;
            self.dirty = true;
        }
    }

    /// Maximize/restore (zoom-in-tab) pane `idx`. Focuses it first, then toggles its zoom.
    pub fn zoom_pane(&mut self, idx: usize) {
        let t = self.active_tab_mut();
        if idx >= t.panes.len() {
            return;
        }
        t.focused = idx;
        t.zoomed = if t.zoomed == Some(idx) { None } else { Some(idx) };
        self.dirty = true;
    }

    /// Open the in-pane search box on pane `idx` (or, if already open, re-focus it). Bumps the
    /// focus sequence so the widget (re)focuses the query input even when the box was already up.
    pub fn open_search(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.search_open();
            p.search_focus_seq = p.search_focus_seq.wrapping_add(1);
            self.dirty = true;
        }
    }

    /// Set pane `idx`'s search query (find-as-you-type).
    pub fn pane_search_query(&mut self, idx: usize, query: &str) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.search_set_query(query);
            self.dirty = true;
        }
    }

    /// Step pane `idx`'s search to the next/previous match.
    pub fn pane_search_step(&mut self, idx: usize, forward: bool) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.search_step(forward);
            self.dirty = true;
        }
    }

    /// Close pane `idx`'s search box.
    pub fn pane_search_close(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.search_close();
            self.dirty = true;
        }
    }

    /// Copy pane `idx`'s current selection to the clipboard (no-op without a selection).
    pub fn copy_pane(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.copy_selection();
            self.dirty = true;
        }
    }

    /// Paste the clipboard into pane `idx`'s session (the widget owns the clipboard; the
    /// controller owns the transport, so we write the returned text via the manager).
    pub fn paste_pane(&mut self, idx: usize, mgr: &SessionManager) {
        let payload = self.active_tab_mut().panes.get_mut(idx).and_then(|p| {
            let text = p.pane.paste_from_clipboard()?;
            // Drop any active drag-selection: its highlight should clear on paste, and a
            // lingering "live" selection could otherwise be re-copied (pasting stale text).
            p.pane.selection_clear();
            // Snap the viewport to the live edge so the caret lands at the end of the pasted
            // text (visible), regardless of where the pane was scrolled when pasting.
            p.pane.scroll_to_bottom();
            Some((p.uid.clone(), text))
        });
        if let Some((uid, text)) = payload {
            mgr.write(&uid, &text);
            self.dirty = true;
        }
    }

    /// Select all of pane `idx`'s viewport.
    pub fn select_all_pane(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.select_all();
            self.dirty = true;
        }
    }

    // ---- text selection (drag-to-select; copy-on-release) ----
    // The widget reports pointer-down/drag/up in the pane's logical-px space; the controller
    // hit-tests against the pane's on-screen surface size (`surf`, recorded from the widget's
    // geometry callback) and the pane's own font cell metrics. Marking `dirty` re-runs resync,
    // which re-projects the selection highlight rects into the pane model each tick.

    /// Begin a selection in pane `idx` at the pressed point (logical px within the surface).
    pub fn pane_selection_begin(&mut self, idx: usize, x: f32, y: f32) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            let (w, h) = p.surf;
            p.pane.selection_begin(x, y, w, h);
            self.dirty = true;
        }
    }

    /// Extend the in-progress selection in pane `idx` to the dragged point.
    pub fn pane_selection_update(&mut self, idx: usize, x: f32, y: f32) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            let (w, h) = p.surf;
            p.pane.selection_update(x, y, w, h);
            self.dirty = true;
        }
    }

    /// Finish a selection in pane `idx`: a real drag copies to the clipboard (raising the
    /// "Copied …" toast) and keeps its highlight; a stationary click clears the zero-size
    /// selection so it doesn't linger or block the next click.
    pub fn pane_selection_end(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if p.pane.selection_is_drag() {
                p.pane.copy_selection();
            } else {
                p.pane.selection_clear();
            }
            self.dirty = true;
        }
    }

    /// Clear pane `idx`'s screen + scrollback.
    pub fn clear_pane(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.clear();
            self.dirty = true;
        }
    }

    /// Restart pane `idx`'s shell: spawn a fresh session, swap it into the pane slot (resetting
    /// the grid), and kill the old session. The cwd resets to the default (we don't track a
    /// per-pane cwd), otherwise chrome (title / color / frame) is preserved.
    pub fn restart_pane(&mut self, idx: usize, mgr: &SessionManager) {
        let (cols, rows) = match self.active_tab().panes.get(idx) {
            Some(p) => p.applied,
            None => return,
        };
        let (cols, rows) = (cols.max(2) as u16, rows.max(1) as u16);
        let uid = format!("pane-{}", self.next_uid);
        self.next_uid += 1;
        let shell = prefs::effective_shell(&self.settings.default_shell);
        let shell_path = shell
            .clone()
            .unwrap_or_else(hyperpanes_core::session::spawn::default_shell);
        let integration = hyperpanes_core::shell_integration::integration_for(
            &shell_path,
            &hyperpanes_core::shell_integration::shell_integration_dir(),
        )
        .map(|si| hyperpanes_core::session_manager::Integration {
            args: si.args,
            env: si.env.into_iter().collect(),
        });
        if let Err(e) = mgr.create(SpawnOptions {
            uid: uid.clone(),
            cols: Some(cols),
            rows: Some(rows),
            pane_id: Some(uid.clone()),
            cwd: None,
            shell,
            integration,
            ..Default::default()
        }) {
            eprintln!("[hyperpanes] failed to restart {uid}: {e}");
            return;
        }
        let mut newgrid =
            TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new()));
        newgrid.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            let old = std::mem::replace(&mut p.uid, uid);
            mgr.kill(&old);
            p.pane = newgrid;
            p.applied = (cols as usize, rows as usize);
            p.started = false;
            p.startup = None;
            p.shell_title = String::new();
            p.surface = Image::default();
        }
        self.dirty = true;
    }

    // ---- move panes across tabs ----

    /// Remove active-tab pane `idx` **without** killing its session, as a [`DetachedPane`] (the
    /// session stays alive centrally for replay-primed re-host). An emptied tab is dropped by
    /// [`Self::take_pane_in`], which also fixes the active index.
    fn detach_pane_idx(&mut self, idx: usize) -> Option<DetachedPane> {
        let (ps, _alive) = self.take_pane_in(self.active, idx)?;
        Some(DetachedPane {
            uid: ps.uid,
            title: ps.title,
            subtitle: ps.subtitle,
            pinned_accent: ps.pinned_accent,
            show_frame: ps.show_frame,
            show_dot: ps.show_dot,
            font_px: ps.font_px,
        })
    }

    /// Adopt a detached session at the end of tab `ti` **without** changing the active tab
    /// (re-host into a background tab — replay-primed, no PTY restart). Used by move-to-tab +
    /// reopen-closed-tab.
    fn adopt_into_tab(&mut self, mgr: &SessionManager, det: DetachedPane, ti: usize) {
        if ti >= self.tabs.len() {
            return;
        }
        let palette = self.settings.frame_palette;
        let (cols, rows) = (80u16, 24u16);
        let mut pane =
            TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new()));
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        if let Some(replay) = mgr.replay(&det.uid) {
            pane.feed(&replay);
        }
        let glow = Glow::new(crate::glow::seed_from(&det.uid));
        let at = self.tabs[ti].panes.len();
        let accent = det
            .pinned_accent
            .unwrap_or_else(|| theme::accent_for(at, palette));
        let font = theme::load_font_at(&self.settings.font_path(), det.font_px, self.last_scale);
        let ps = PaneState {
            uid: det.uid,
            title: det.title,
            subtitle: det.subtitle,
            show_frame: det.show_frame,
            show_dot: det.show_dot,
            accent,
            pane,
            applied: (cols as usize, rows as usize),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: true,
            startup: None,
            pinned_accent: det.pinned_accent,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            search_focus_seq: 0,
            font_px: det.font_px,
            font,
            font_dirty: false,
            cwd: None,
        };
        let auto = self.tabs[ti].layout == Layout::Auto;
        let t = &mut self.tabs[ti];
        t.sizes = if auto {
            equal_sizes(at + 1)
        } else {
            insert_size(&t.sizes, at)
        };
        t.panes.push(ps);
        t.zoomed = None;
        t.relabel(palette);
        self.dirty = true;
    }

    /// Move active-tab pane `idx` into a brand-new tab (the pane menu's "Move to New Tab",
    /// gated to ≥2 panes so the source tab survives), switching to it.
    pub fn move_pane_to_new_tab(&mut self, idx: usize, mgr: &SessionManager) {
        if self.active_tab().panes.len() < 2 {
            return;
        }
        let Some(dp) = self.detach_pane_idx(idx) else {
            return;
        };
        let tab = self.fresh_tab();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        self.adopt_pane(mgr, dp);
    }

    /// Move active-tab pane `idx` into existing tab `target` (the "Move to Tab" submenu), without
    /// switching away from the current tab. Handles the source tab being dropped when its last
    /// pane leaves (which shifts `target` when the source sat before it).
    pub fn move_pane_to_tab(&mut self, idx: usize, target: usize, mgr: &SessionManager) {
        if target >= self.tabs.len() || target == self.active {
            return;
        }
        let src = self.active;
        let before = self.tabs.len();
        let Some(dp) = self.detach_pane_idx(idx) else {
            return;
        };
        let mut target = target;
        if self.tabs.len() < before && src < target {
            target -= 1;
        }
        if target >= self.tabs.len() {
            return;
        }
        self.adopt_into_tab(mgr, dp, target);
    }

    // ---- tab actions ----

    /// Duplicate tab `idx`: a fresh tab adopting its layout + title with the same number of
    /// (fresh-shell) panes, switched to. (Sessions aren't cloned — the renderer spawns new
    /// shells too.)
    pub fn duplicate_tab(&mut self, idx: usize, mgr: &SessionManager) {
        let Some(src) = self.tabs.get(idx) else {
            return;
        };
        let layout = src.layout;
        let title = src.title.clone();
        let sizes = src.sizes.clone();
        let main_fraction = src.main_fraction;
        let focused = src.focused;
        let zoomed = src.zoomed;
        // Snapshot each source pane's chrome so the duplicate carries it (label, color/pin,
        // frame/dot, subtitle, per-pane zoom) — not just the pane count + layout. The accent is
        // pinned only when the source's was; an unpinned pane re-derives its by-slot color at
        // the same index, so order-preserving duplication keeps the same colors.
        let chrome: Vec<(NewPaneOpts, Option<SharedString>, f32)> = src
            .panes
            .iter()
            .map(|p| {
                (
                    NewPaneOpts {
                        label: Some(p.title.to_string()),
                        accent: p.pinned_accent,
                        show_frame: p.show_frame,
                        show_dot: p.show_dot,
                        ..Default::default()
                    },
                    p.subtitle.clone(),
                    p.font_px,
                )
            })
            .collect();
        let mut tab = self.fresh_tab();
        tab.layout = layout;
        tab.title = title;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        if chrome.is_empty() {
            // A tab should always have ≥1 pane, but guard the empty case with a default pane.
            self.add_pane(mgr);
        } else {
            for (opts, subtitle, font_px) in chrome {
                self.add_pane_opts(mgr, opts);
                if let Some(p) = self.active_tab_mut().panes.last_mut() {
                    p.subtitle = subtitle;
                    if (p.font_px - font_px).abs() > f32::EPSILON {
                        p.font_px = font_px;
                        p.font_dirty = true; // pump reloads the font at the carried zoom
                    }
                }
            }
        }
        // Carry the split geometry + focus/zoom from the source (clamped to the new pane count).
        let t = self.active_tab_mut();
        if sizes.len() == t.panes.len() {
            t.sizes = sizes;
        }
        t.main_fraction = main_fraction;
        if !t.panes.is_empty() {
            t.focused = focused.min(t.panes.len() - 1);
        }
        t.zoomed = zoomed.filter(|&z| z < t.panes.len());
        self.dirty = true;
    }

    /// Park a closed tab on the reopen stack, capping it (evicted entries' sessions are killed).
    fn push_closed(&mut self, tab: DetachedTab, mgr: &SessionManager) {
        const CLOSED_STACK_CAP: usize = 10;
        self.closed_tabs.push(tab);
        while self.closed_tabs.len() > CLOSED_STACK_CAP {
            let evicted = self.closed_tabs.remove(0);
            for p in &evicted.panes {
                mgr.kill(&p.uid);
            }
        }
    }

    /// Detach the whole of tab `idx` (its panes as live [`DetachedPane`]s, plus title/layout/
    /// sizes) for re-hosting or parking. Requires ≥2 tabs; fixes the active index. Returns the
    /// detached tab + `source_alive` (always `true` here — other tabs remain).
    pub fn detach_tab(&mut self, idx: usize) -> Option<(DetachedTab, bool)> {
        if idx >= self.tabs.len() || self.tabs.len() < 2 {
            return None;
        }
        let tab = self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.editing_tab = -1;
        self.dirty = true;
        let focused = tab.focused;
        let zoomed = tab.zoomed;
        let panes = tab
            .panes
            .into_iter()
            .map(|p| DetachedPane {
                uid: p.uid,
                title: p.title,
                subtitle: p.subtitle,
                pinned_accent: p.pinned_accent,
                show_frame: p.show_frame,
                show_dot: p.show_dot,
                font_px: p.font_px,
            })
            .collect();
        Some((
            DetachedTab {
                title: tab.title,
                layout: tab.layout,
                sizes: tab.sizes,
                main_fraction: tab.main_fraction,
                focused,
                zoomed,
                panes,
            },
            true,
        ))
    }

    /// Close tab `idx` reopenably: with ≥2 tabs it's parked (sessions alive) on the closed stack;
    /// the last tab is killed for real (returns `false` → the window quits).
    pub fn close_tab_menu(&mut self, idx: usize, mgr: &SessionManager) -> bool {
        if self.tabs.len() >= 2 {
            if let Some((det, _)) = self.detach_tab(idx) {
                self.push_closed(det, mgr);
            }
            true
        } else {
            self.close_tab(idx, mgr)
        }
    }

    /// Close every tab except `idx` (all reopenable). Removes from the highest index down so the
    /// surviving indices stay valid.
    pub fn close_other_tabs(&mut self, idx: usize, mgr: &SessionManager) {
        if idx >= self.tabs.len() {
            return;
        }
        let mut others: Vec<usize> = (0..self.tabs.len()).filter(|&i| i != idx).collect();
        others.sort_unstable_by(|a, b| b.cmp(a));
        for i in others {
            self.close_tab_menu(i, mgr);
        }
    }

    /// Close every tab to the right of `idx` (all reopenable), highest index first.
    pub fn close_tabs_to_right(&mut self, idx: usize, mgr: &SessionManager) {
        let mut i = self.tabs.len();
        while i > idx + 1 {
            i -= 1;
            self.close_tab_menu(i, mgr);
        }
    }

    /// Reopen the most-recently closed tab (replay-primed; its sessions were kept alive), as a
    /// fresh tab switched to. No-op when the stack is empty.
    pub fn reopen_closed_tab(&mut self, mgr: &SessionManager) {
        let Some(det) = self.closed_tabs.pop() else {
            return;
        };
        let mut tab = Tab::empty(det.title.clone());
        tab.layout = det.layout;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        let ti = self.active;
        for dp in det.panes {
            self.adopt_into_tab(mgr, dp, ti);
        }
        if det.sizes.len() == self.tabs[ti].panes.len() {
            self.tabs[ti].sizes = det.sizes;
        }
        self.tabs[ti].main_fraction = det.main_fraction;
        // Restore the detached focus + zoom (clamped to the adopted pane count) rather than
        // snapping to pane 0 / dropping the maximized pane. `adopt_into_tab` clears `zoomed`
        // on each pane, so this must run after the adopt loop.
        let n = self.tabs[ti].panes.len();
        if n > 0 {
            self.tabs[ti].focused = det.focused.min(n - 1);
        }
        self.tabs[ti].zoomed = det.zoomed.filter(|&z| z < n);
        self.dirty = true;
    }

    /// Fill a freshly-spawned window's initial (empty) tab with a detached tab's panes + its
    /// title/layout/sizes (the seed for the tab menu's "Move to New Window"). Replay-primed.
    pub fn adopt_tab(&mut self, mgr: &SessionManager, det: DetachedTab) {
        let ti = self.active;
        self.tabs[ti].title = det.title;
        self.tabs[ti].layout = det.layout;
        for dp in det.panes {
            self.adopt_into_tab(mgr, dp, ti);
        }
        if det.sizes.len() == self.tabs[ti].panes.len() {
            self.tabs[ti].sizes = det.sizes;
        }
        self.tabs[ti].main_fraction = det.main_fraction;
        // Restore the detached focus + zoom (clamped to the adopted pane count) rather than
        // snapping to pane 0 / dropping the maximized pane. `adopt_into_tab` clears `zoomed`
        // on each pane, so this must run after the adopt loop.
        let n = self.tabs[ti].panes.len();
        if n > 0 {
            self.tabs[ti].focused = det.focused.min(n - 1);
        }
        self.tabs[ti].zoomed = det.zoomed.filter(|&z| z < n);
        self.dirty = true;
    }

    /// Set tab `idx`'s layout (the tab menu's Layout submenu).
    pub fn set_tab_layout(&mut self, idx: usize, layout: Layout) {
        if let Some(t) = self.tabs.get_mut(idx) {
            if t.layout != layout {
                t.layout = layout;
                self.dirty = true;
            }
        }
    }

    // ---- workspace file (application menu: Open / Save) ----

    /// Whether the single-layout pane taskbar should show: the active tab uses the explicit
    /// `single` preset, has more than one pane, and we're not in fullscreen. (The single
    /// preset renders only the focused pane, so the strip is how the hidden panes stay
    /// reachable — the native port of Electron's `PaneTaskbar` gate.)
    pub fn taskbar_visible(&self) -> bool {
        let t = self.active_tab();
        t.layout == Layout::Single && t.panes.len() > 1 && !self.fullscreen
    }

    /// Snapshot the **active tab** into the persistable file shape — the native port of
    /// `serializeWorkspace()` (`{ name, layout, panes }`; runtime-only fields dropped). Pane
    /// identity is the label + color; the cwd/command aren't tracked per-pane in the native
    /// state, so a reloaded pane re-spawns a plain shell at its saved label/color.
    pub fn to_workspace_file(&self) -> WorkspaceFile {
        let t = self.active_tab();
        let panes: Vec<PaneSpec> = t
            .panes
            .iter()
            .map(|p| PaneSpec {
                label: Some(p.title.to_string()),
                color: Some(color_hex(p.accent)),
                ..Default::default()
            })
            .collect();
        WorkspaceFile {
            name: Some(t.title.to_string()),
            layout: Some(theme::layout_name(t.layout).to_string()),
            panes: Some(panes),
            ..Default::default()
        }
    }

    /// "Save workspace…": pick a destination via the native save dialog and write the active
    /// tab's serialized workspace there. No-op if the dialog is cancelled.
    pub fn save_workspace(&mut self) {
        let file = self.to_workspace_file();
        let default_name = match &file.name {
            Some(n) if !n.is_empty() => format!("{n}.json"),
            _ => "workspace.json".to_string(),
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Workspace", &["json"])
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };
        if !write_workspace(&path, &file) {
            eprintln!("[hyperpanes] failed to write workspace {}", path.display());
        }
    }

    /// "Open workspace…": pick a `workspace.json` via the native open dialog, read + validate
    /// it, and load its groups as new tabs (switching to the first). Non-destructive: existing
    /// tabs/sessions are left intact. No-op if cancelled or the file has no panes.
    pub fn open_workspace(&mut self, mgr: &SessionManager) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Workspace", &["json"])
            .pick_file()
        else {
            return;
        };
        let Some(file) = read_workspace(&path) else {
            eprintln!("[hyperpanes] {} is not a valid workspace", path.display());
            return;
        };
        self.load_workspace(file, mgr);
    }

    /// Load a parsed workspace file: append a tab per group of its first window and switch to
    /// the first appended tab. Each pane re-spawns from its spec (label/color/cwd/command/
    /// shell). Shared by [`Self::open_workspace`].
    pub fn load_workspace(&mut self, file: WorkspaceFile, mgr: &SessionManager) {
        let windows = windows_of(Some(&file));
        let Some(win) = windows.into_iter().next() else {
            return;
        };
        let first_new = self.tabs.len();
        for g in win.groups {
            self.append_tab_from_group(mgr, g);
        }
        if self.tabs.len() > first_new {
            self.active = first_new;
            self.editing_tab = -1;
            self.dirty = true;
        }
    }

    /// Build a tab from a `GroupSpec` (spawning a pane per spec) and append it.
    fn append_tab_from_group(&mut self, mgr: &SessionManager, g: GroupSpec) {
        let palette = self.settings.frame_palette;
        let title: SharedString = match g.title {
            Some(t) if !t.is_empty() => t.into(),
            _ => {
                self.tab_seq += 1;
                format!("term {}", self.tab_seq).into()
            }
        };
        let layout = g.layout.as_deref().map(layout_from_name).unwrap_or(Layout::Auto);
        let mut tab = Tab::empty(title);
        tab.layout = layout;
        for (i, spec) in g.panes.iter().enumerate() {
            if let Some(ps) = self.make_pane_from_spec(mgr, i, spec) {
                tab.panes.push(ps);
            }
        }
        let n = tab.panes.len();
        if n == 0 {
            return; // a contentless group — skip it
        }
        tab.sizes = match &g.sizes {
            Some(s) if s.len() == n => s.clone(),
            _ => equal_sizes(n),
        };
        if let Some(mf) = g.main_fraction {
            tab.main_fraction = clamp_fraction(mf);
        }
        tab.focused = g.focused.map(|f| (f as usize).min(n - 1)).unwrap_or(0);
        tab.zoomed = g.zoomed.map(|z| (z as usize).min(n - 1));
        tab.relabel(palette);
        self.tabs.push(tab);
    }

    /// Spawn a pane from a `PaneSpec` (its command/args/cwd/shell), returning the `PaneState`.
    /// A spec with a `color` is treated like a project pane (tinted: frame + dot on, accent
    /// pinned); a colorless spec is a clean pane coloured by slot.
    fn make_pane_from_spec(
        &mut self,
        mgr: &SessionManager,
        idx: usize,
        spec: &PaneSpec,
    ) -> Option<PaneState> {
        let uid = format!("pane-{}", self.next_uid);
        self.next_uid += 1;
        let palette = self.settings.frame_palette;
        let shell = spec
            .shell
            .clone()
            .or_else(|| prefs::effective_shell(&self.settings.default_shell));
        let shell_path = shell
            .clone()
            .unwrap_or_else(hyperpanes_core::session::spawn::default_shell);
        let integration = hyperpanes_core::shell_integration::integration_for(
            &shell_path,
            &hyperpanes_core::shell_integration::shell_integration_dir(),
        )
        .map(|si| hyperpanes_core::session_manager::Integration {
            args: si.args,
            env: si.env.into_iter().collect(),
        });
        let (cols, rows) = (80u16, 24u16);
        if let Err(e) = mgr.create(SpawnOptions {
            uid: uid.clone(),
            cols: Some(cols),
            rows: Some(rows),
            pane_id: Some(uid.clone()),
            cwd: spec.cwd.clone(),
            shell,
            command: spec.command.clone(),
            args: spec.args.clone(),
            integration,
            ..Default::default()
        }) {
            eprintln!("[hyperpanes] failed to spawn {uid}: {e}");
            return None;
        }
        let mut pane =
            TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new()));
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        let glow = Glow::new(crate::glow::seed_from(&uid));
        let pinned = spec.color.as_deref().map(parse_hex);
        let project = pinned.is_some();
        let label = match &spec.label {
            Some(l) if !l.is_empty() => l.clone(),
            _ if idx == 0 => "shell".to_string(),
            _ => format!("pane {}", idx + 1),
        };
        let font_px = self.settings.font_px;
        let font = theme::load_font_at(&self.settings.font_path(), font_px, self.last_scale);
        Some(PaneState {
            uid,
            title: label.into(),
            subtitle: None,
            show_frame: Some(project),
            show_dot: Some(project),
            accent: pinned.unwrap_or_else(|| theme::accent_for(idx, palette)),
            pane,
            applied: (cols as usize, rows as usize),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: false,
            startup: None,
            pinned_accent: pinned,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            search_focus_seq: 0,
            font_px,
            font,
            font_dirty: false,
            cwd: None,
        })
    }
}

/// Format a Slint [`Color`] as `#rrggbb` (the workspace-file pane color format).
fn color_hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

/// Parse a workspace-file layout token (`"single"`/`"columns"`/… / `"main-stack"`) back to a
/// [`Layout`], defaulting to `Auto` for an unknown/absent token.
fn layout_from_name(name: &str) -> Layout {
    match name {
        "single" => Layout::Single,
        "columns" => Layout::Columns,
        "rows" => Layout::Rows,
        "grid" => Layout::Grid,
        "main-stack" => Layout::MainStack,
        _ => Layout::Auto,
    }
}

/// Parse a `#rrggbb` hex string (the project palette format) into a Slint [`Color`],
/// falling back to the default accent on a malformed value.
pub fn parse_hex(s: &str) -> Color {
    let h = s.trim_start_matches('#');
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return Color::from_rgb_u8(r, g, b);
        }
    }
    theme::accent_for(0, 0)
}

#[cfg(test)]
mod ctx_menu_borrow_tests {
    //! Regression for the issue-#18 crash: changing the layout via the hamburger Layout
    //! submenu panicked with `RefCell already borrowed`.
    //!
    //! The `on_ctx_layout` callback (and its five `ctx_target()` siblings) did
    //! `if let Some(t) = win.state.borrow().ctx_target() { run_command(...) }`, and in Rust
    //! edition 2021 the `state.borrow()` temporary lives for the *whole* `if let` arm. Inside
    //! the arm, `run_command` takes `state.borrow_mut()` — a second, mutable borrow of the same
    //! `RefCell` → panic. The fix binds `ctx_target()` to a local so the shared borrow is
    //! released before the command runs. These tests reproduce that exact borrow ordering
    //! against a `RefCell<State>` (as `Window::state` is), so the regression can't return.
    use super::*;
    use std::cell::RefCell;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    /// The fixed shape: read the target out of the shared borrow first, then mutate. The
    /// hamburger Layout submenu opens via `open_app_context` (target = active tab) and routes
    /// `ctx_target` → `set_tab_layout`. This must not panic at any layout.
    #[test]
    fn layout_submenu_pick_does_not_double_borrow() {
        let cell = RefCell::new(fresh());
        cell.borrow_mut().open_app_context(0.0, 0.0);

        for layout in [
            Layout::Single,
            Layout::Columns,
            Layout::Rows,
            Layout::Grid,
            Layout::MainStack,
            Layout::Auto,
        ] {
            // Mirror `on_ctx_layout`'s FIXED body: bind the target out of the borrow, THEN mutate.
            let target = cell.borrow().ctx_target();
            if let Some(t) = target {
                cell.borrow_mut().set_tab_layout(t, layout);
            }
            assert_eq!(cell.borrow().active_tab().layout, layout);
        }
    }

    /// Pin the root cause: the OLD callback shape (mutating while the `ctx_target()` borrow is
    /// still held across the `if let` arm) double-borrows and panics. If a future refactor lets
    /// the shared borrow escape again, this catches it.
    #[test]
    fn holding_the_ctx_borrow_across_a_mutation_panics() {
        let cell = RefCell::new(fresh());
        cell.borrow_mut().open_app_context(0.0, 0.0);

        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // The buggy pattern: `state.borrow()` (via ctx_target) outlives the arm in 2021,
            // so the inner `borrow_mut()` is a re-entrant borrow → panic.
            if let Some(t) = cell.borrow().ctx_target() {
                cell.borrow_mut().set_tab_layout(t, Layout::Grid);
            }
        }))
        .is_err();
        assert!(crashed, "the held-borrow pattern must still double-borrow");
    }

    /// Task 7 (right-click chaining): opening a second context menu *while one is open* must
    /// REPLACE it — `State::ctx` is a single slot, never a stack — so the net state is exactly
    /// the new menu (kind + anchor swapped, old target gone) in one transition. This pins the
    /// state half of `App::reopen_context_at_cursor`, and mirrors its borrow-safe shape: every
    /// read is bound to a local and the shared borrow is dropped before the reopen mutates.
    #[test]
    fn right_click_chain_replaces_the_open_menu() {
        use crate::contextmenu::CtxKind;
        let cell = RefCell::new(fresh());

        // Menu A: the tab menu for tab 0 at one anchor.
        cell.borrow_mut().open_tab_context(0, 10.0, 20.0);
        {
            let st = cell.borrow();
            let m = st.ctx.as_ref().expect("menu A should be open");
            assert_eq!(m.kind, CtxKind::Tab);
            assert_eq!(m.target, 0);
            assert_eq!((m.x, m.y), (10.0, 20.0));
        }

        // The chain: a reopen reads via a local-bound borrow that is released before mutating
        // (the #18 rule), then opens a *different* surface (the app menu) at a *new* anchor.
        let was_open = cell.borrow().ctx_open();
        assert!(was_open, "a menu must be open for a chain to replace it");
        cell.borrow_mut().open_app_context(99.0, 88.0);

        // Net state is exactly menu B — A was replaced, not stacked.
        {
            let st = cell.borrow();
            let m = st.ctx.as_ref().expect("menu B should be open");
            assert_eq!(m.kind, CtxKind::App);
            assert_eq!((m.x, m.y), (99.0, 88.0));
        }
        assert!(cell.borrow().ctx_open());

        // A right-click with no chain target (or a left-click away) is a plain dismiss.
        cell.borrow_mut().close_context();
        assert!(!cell.borrow().ctx_open());
        assert!(cell.borrow().ctx_target().is_none());
    }
}
