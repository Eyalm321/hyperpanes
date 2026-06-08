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
use hyperpanes_core::persistence::projects;
use hyperpanes_core::session_manager::{SessionManager, SpawnOptions};
use hyperpanes_terminal_widget::{Font, RenderOpts, SoftwareRenderer, TerminalPane};

use slint::{Color, Image, SharedString};

use crate::command::Command;
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
}

/// A session detached from its window for re-hosting in another (Wave-1 multi-window
/// plumbing). Carries only the session `uid` + chrome; the PTY stays alive centrally in
/// the [`SessionManager`], so re-hosting is a replay-into-a-fresh-grid, never a restart.
#[derive(Debug, Clone)]
pub struct DetachedPane {
    pub uid: String,
    pub title: SharedString,
    pub pinned_accent: Option<Color>,
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

/// One pane's controller-side state (terminal grid + placement + chrome).
pub struct PaneState {
    pub uid: String,
    pub title: SharedString,
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

    /// Re-label + recolor panes so titles/accents stay 1..N in order. A pinned accent
    /// (a project color) is preserved.
    fn relabel(&mut self, palette: usize) {
        for (i, p) in self.panes.iter_mut().enumerate() {
            p.title = format!("{}", i + 1).into();
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
    pub font: hyperpanes_terminal_widget::Font,
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_uid: usize,
    tab_seq: usize,
    pub fullscreen: bool,
    /// Index of the tab whose title is being edited inline (-1 = none).
    pub editing_tab: i32,
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
            tabs: Vec::new(),
            active: 0,
            next_uid: 0,
            tab_seq: 0,
            fullscreen: false,
            editing_tab: -1,
            last_blink: Instant::now(),
            cursor_on: true,
            frames: 0,
            last_hud: Instant::now(),
            dirty: true,
            overlay: Overlay::None,
            settings: prefs::load(),
            // Apply the saved font family/size on the first pump (it owns the scale).
            font_reload: true,
            prefs_draft: None,
            prefs_confirm: false,
            font_custom: false,
            preview_pane: TerminalPane::new(64, 7, Box::new(SoftwareRenderer::new())),
            preview_font: None,
            preview_key: (String::new(), 0.0, 0.0),
            preview_theme: -1,
            preview_cursor: false,
            preview_surface: Image::default(),
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
        };
        let tab = s.fresh_tab();
        s.tabs.push(tab);
        // Canned sample output for the appearance preview (a real, locked terminal). ANSI SGR
        // so the terminal theme's colours show: green prompt, dim build line, blue "Finished".
        s.preview_pane.feed(
            "\x1b[32m$\x1b[0m cargo run\r\n\
             \x1b[90m   Compiling hyperpanes v0.1.0\x1b[0m\r\n\
             \x1b[34m    Finished\x1b[0m dev in 1.24s\r\n\
             \x1b[35mthe quick brown fox\x1b[0m \x1b[36mjumps 0123\x1b[0m\r\n\
             $ ",
        );
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
        cwd: Option<String>,
        accent: Option<Color>,
    ) -> Option<PaneState> {
        let uid = format!("pane-{}", self.next_uid);
        self.next_uid += 1;
        let palette = self.settings.frame_palette;
        // Honour the default-shell preference ("" = let core pick the system default).
        let shell = if self.settings.default_shell.is_empty() {
            None
        } else {
            Some(self.settings.default_shell.clone())
        };
        let (cols, rows) = (80u16, 24u16);
        if let Err(e) = mgr.create(SpawnOptions {
            uid: uid.clone(),
            cols: Some(cols),
            rows: Some(rows),
            pane_id: Some(uid.clone()),
            cwd,
            shell,
            ..Default::default()
        }) {
            eprintln!("[hyperpanes] failed to spawn {uid}: {e}");
            return None;
        }
        let mut pane =
            TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new()));
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        Some(PaneState {
            uid,
            title: format!("{}", idx + 1).into(),
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
        let idx = self.active_tab().panes.len();
        let Some(ps) = self.make_pane(mgr, idx, cwd, accent) else {
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
            DetachedPane { uid: ps.uid, title: ps.title, pinned_accent: ps.pinned_accent },
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
        let ps = PaneState {
            uid: det.uid,
            title: det.title,
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
            DetachedPane { uid: ps.uid, title: ps.title, pinned_accent: ps.pinned_accent },
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

    /// Move focus in `dir`. When soloed (zoom or single), cycle the pane order.
    pub fn focus_dir(&mut self, dir: Direction) {
        let t = self.active_tab_mut();
        let n = t.panes.len();
        if n < 2 {
            return;
        }
        let eff = t.effective();
        let next = if t.zoomed.is_some() || eff == Layout::Single {
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

    /// The current active tab's draggable dividers (empty when zoomed).
    pub fn dividers(&self) -> Vec<hyperpanes_core::layout::presets::DividerDesc> {
        let t = self.active_tab();
        if t.zoomed.is_some() {
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
            self.dirty = true;
        }
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
            Setting::DefaultShell(_) | Setting::ClickablePaths(_) | Setting::EditorCommand(_) => {}
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
                    self.font_reload = true;
                }
            }
            Setting::DefaultShell(shell) => self.settings.default_shell = shell,
            Setting::ShowFrame(on) => self.settings.show_frame = on,
            Setting::ShowDot(on) => self.settings.show_dot = on,
            Setting::ClickablePaths(on) => self.settings.clickable_paths = on,
            Setting::EditorCommand(cmd) => self.settings.editor_command = cmd,
        }
        prefs::save(&self.settings);
        self.dirty = true;
    }

    /// Reload the terminal font from the current settings at DPI `scale`, forcing every
    /// pane to re-grid at the new cell metrics (resets each pane's `applied`). Called by
    /// the pump (which owns the scale) when `font_reload` is set.
    pub fn reload_font(&mut self, scale: f32) {
        self.font = theme::load_font_at(&self.settings.font_path(), self.settings.font_px, scale);
        for t in &mut self.tabs {
            for p in &mut t.panes {
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

    /// A pane reported a cwd — if it's inside a repo, remember it and refresh the cache
    /// (so the rail's count badge + an open flyout update live).
    pub fn note_cwd(&mut self, cwd: &str) {
        if let Some(list) = sidebar::note_cwd(cwd) {
            self.projects = list;
            if self.settings.show_sidebar {
                self.dirty = true;
            }
        }
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

/// Parse a `#rrggbb` hex string (the project palette format) into a Slint [`Color`],
/// falling back to the default accent on a malformed value.
fn parse_hex(s: &str) -> Color {
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
