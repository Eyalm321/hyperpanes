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
use hyperpanes_core::session_manager::{SessionManager, SpawnOptions};
use hyperpanes_terminal_widget::{SoftwareRenderer, TerminalPane};

use slint::{Color, Image, SharedString};

use crate::theme;

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

    /// Re-label + recolor panes so titles/accents stay 1..N in order.
    fn relabel(&mut self) {
        for (i, p) in self.panes.iter_mut().enumerate() {
            p.title = format!("{}", i + 1).into();
            p.accent = theme::accent_for(i);
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
            esc_last: None,
            esc_hold_start: None,
            esc_holding: false,
            esc_fired: false,
        };
        let tab = s.fresh_tab();
        s.tabs.push(tab);
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

    fn make_pane(&mut self, mgr: &SessionManager, idx: usize) -> Option<PaneState> {
        let uid = format!("pane-{}", self.next_uid);
        self.next_uid += 1;
        let (cols, rows) = (80u16, 24u16);
        if let Err(e) = mgr.create(SpawnOptions {
            uid: uid.clone(),
            cols: Some(cols),
            rows: Some(rows),
            pane_id: Some(uid.clone()),
            ..Default::default()
        }) {
            eprintln!("[hyperpanes] failed to spawn {uid}: {e}");
            return None;
        }
        Some(PaneState {
            uid,
            title: format!("{}", idx + 1).into(),
            accent: theme::accent_for(idx),
            pane: TerminalPane::new(cols as usize, rows as usize, Box::new(SoftwareRenderer::new())),
            applied: (cols as usize, rows as usize),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: false,
            startup: None,
        })
    }

    // ---- pane mutations (act on the active tab) ----

    /// Spawn a new pane + shell in the active tab and focus it.
    pub fn add_pane(&mut self, mgr: &SessionManager) {
        let idx = self.active_tab().panes.len();
        let Some(ps) = self.make_pane(mgr, idx) else {
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

    /// Close pane `idx` of tab `ti`, killing its session. An emptied tab is
    /// dropped; closing the last pane of the last tab returns `false` (caller
    /// quits). Works for background tabs too (used by self-exiting shells).
    pub fn close_pane_in(&mut self, ti: usize, idx: usize, mgr: &SessionManager) -> bool {
        if ti >= self.tabs.len() {
            return true;
        }
        let t = &mut self.tabs[ti];
        if idx >= t.panes.len() {
            return true;
        }
        let ps = t.panes.remove(idx);
        let auto = t.layout == Layout::Auto;
        t.sizes = if auto {
            equal_sizes(t.panes.len())
        } else {
            remove_size(&t.sizes, idx)
        };
        let empty = t.panes.is_empty();
        mgr.kill(&ps.uid);
        if empty {
            // No panes left → drop the whole tab.
            return self.close_tab(ti, mgr);
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
        t.relabel();
        self.dirty = true;
        true
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
