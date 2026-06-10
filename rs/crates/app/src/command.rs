//! Command dispatch — Wave-2 **Seam #2**.
//!
//! Every user action — a top-bar click, a key shortcut, and (in Wave 2) a command
//! palette entry or a keybinding — is expressed as a [`Command`] and run through
//! [`dispatch`]. `dispatch` mutates the central [`State`] and returns an
//! [`Effect`] for the thin set of concerns that live outside the state (quitting,
//! OS fullscreen). Wave-2 features add variants here and emit them; they never
//! reach into the UI or the window glue themselves.

use hyperpanes_core::layout::navigate::Direction;
use hyperpanes_core::layout::presets::{DividerKind, Layout};
use hyperpanes_core::session_manager::SessionManager;

use crate::state::{DetachedPane, DetachedTab, NewPaneOpts, ReminderOffset, Setting, State};
use crate::theme;

/// An action against the workspace. Construct these from any input source.
#[derive(Debug, Clone)]
pub enum Command {
    // panes
    /// Immediately spawn a default pane (the plain ＋ click / palette "New pane").
    NewPane,
    /// Open the "New pane" options dialog (Shift+＋ / the menus' "New pane…").
    OpenNewPane,
    /// Submit the New Pane dialog: spawn a pane from the configured options + close the dialog.
    SubmitNewPane(NewPaneOpts),
    CloseFocused,
    ClosePane(usize),
    FocusPane(usize),
    FocusDir(Direction),
    // layout
    SetLayout(Layout),
    CycleLayout,
    ToggleZoom,
    ToggleFullscreen,
    // font zoom (Ctrl+= / Ctrl+- / Ctrl+0)
    /// Nudge the global terminal font size by `0` px (clamped), re-gridding every pane.
    FontZoom(i32),
    /// Reset the global terminal font size to its default.
    FontReset,
    ResizeDivider {
        kind: DividerKind,
        index: i32,
        delta: f64,
    },
    // tabs
    NewTab,
    CloseTab(usize),
    SwitchTab(usize),
    /// Switch to the next tab, wrapping around (Ctrl+Tab).
    NextTab,
    /// Switch to the previous tab, wrapping around (Ctrl+Shift+Tab).
    PrevTab,
    BeginRename(i32),
    RenameTab(i32, String),
    /// Begin editing pane `0`'s label inline (double-click on its header).
    BeginRenamePane(i32),
    /// Commit pane `0`'s label to `1` (blank keeps the prior label).
    RenamePane(i32, String),
    // ---- pane context-menu actions (target a specific pane by active-tab index) ----
    /// Recolor pane `0` to swatch `1` of the active frame palette (pins it + frame/dot on).
    RecolorPane(usize, usize),
    /// Set pane `0`'s per-pane frame override to `1`.
    SetPaneFrame(usize, bool),
    /// Set pane `0`'s per-pane dot override to `1`.
    SetPaneDot(usize, bool),
    /// Toggle whether pane `0`'s ambient-AI summary line is muted.
    ToggleMuteAi(usize),
    /// Maximize/restore (zoom-in-tab) pane `0`.
    ZoomPane(usize),
    /// Fullscreen/exit-fullscreen pane `0`.
    FullscreenPane(usize),
    /// Restart pane `0`'s shell (kills + respawns its session in place).
    RestartPane(usize),
    /// Re-resolve a FRESH (registry-backed) environment and restart pane `0`'s shell in
    /// place, keeping its live cwd + env overrides (#28; the pane menu's "Refresh Env").
    RefreshEnvPane(usize),
    /// Spawn a new pane with the same cwd + env overrides as pane `0` (#27; the pane
    /// menu's "Open Linked Terminal" — act/authenticate with the source pane's context).
    OpenLinkedTerminal(usize),
    /// Open pane `0`'s current working directory in the OS file explorer (#23).
    RevealPaneCwd(usize),
    /// Open the in-pane search box on pane `0`.
    SearchPane(usize),
    /// Open the in-pane search box on the focused pane (the Ctrl+F keybinding).
    SearchFocused,
    /// Copy pane `0`'s current selection to the clipboard.
    CopyPane(usize),
    /// Paste the clipboard into pane `0`'s session.
    PastePane(usize),
    /// Paste the clipboard into the focused pane's session (the Ctrl+V keybinding). Reads
    /// the OS clipboard fresh app-side (arboard, with open retries) instead of forwarding a
    /// raw 0x16 for the shell to resolve — PSReadLine's own clipboard read has no retry and
    /// can come up empty/stale right after an external copy (#9). Unbinding `pane.paste`
    /// in Preferences restores the literal-0x16 passthrough for shells that want it.
    PasteFocused,
    /// Select all of pane `0`'s viewport.
    SelectAllPane(usize),
    /// Clear pane `0`'s screen + scrollback.
    ClearPane(usize),
    // ---- reminder panes (Track F) ----
    /// Park pane `0` until quick-offset `1` from now: it leaves the layout but its session
    /// stays alive; it lives in the sidebar bell list until restored.
    RemindPane(usize, ReminderOffset),
    /// Toggle the sidebar bell's reminder-list panel.
    ToggleReminders,
    /// Re-dock the parked pane with session uid `0` into the active tab + clear its reminder.
    RestoreReminder(String),
    /// Move pane `0` into a brand-new tab (disabled when its tab has <2 panes).
    MovePaneToNewTab(usize),
    /// Move pane `0` into existing tab `1`.
    MovePaneToTab(usize, usize),
    // ---- tab context-menu actions (target a specific tab by index) ----
    /// Duplicate tab `0` (a fresh tab with the same number of panes + its layout).
    DuplicateTab(usize),
    /// Close every tab except tab `0`.
    CloseOtherTabs(usize),
    /// Close every tab to the right of tab `0`.
    CloseTabsToRight(usize),
    /// Reopen the most-recently closed tab (replay-primed; no-op when none).
    ReopenClosedTab,
    /// Set tab `0`'s layout to `1`.
    SetTabLayout(usize, Layout),
    /// Move the whole of tab `0` to a new OS window.
    MoveTabToNewWindow(usize),
    // ---- context-menu lifecycle ----
    /// Open the pane context menu for pane `0` at window-logical `(1, 2)`.
    OpenPaneContext(usize, f32, f32),
    /// Open the single-layout taskbar's pane menu for pane `0` at `(1, 2)` (the `inTaskbar`
    /// variant: a leading Show row, no Maximize).
    OpenTaskbarContext(usize, f32, f32),
    /// Open the tab context menu for tab `0` at window-logical `(1, 2)`.
    OpenTabContext(usize, f32, f32),
    /// Open the application (hamburger) menu at window-logical `(0, 1)`.
    OpenAppContext(f32, f32),
    /// Dismiss the open context menu.
    CloseContext,
    // ---- workspace file (application menu) ----
    /// Pick a `workspace.json` and load it (the application menu's "Open workspace…").
    OpenWorkspace,
    /// Serialize the active tab and save it to a chosen file (the menu's "Save workspace…").
    SaveWorkspace,
    // ---- multi-window (Phase 4) ----
    /// Open a fresh OS window with an empty tab.
    NewWindow,
    /// Re-host the focused pane in a new OS window (replay-primed, no PTY restart).
    MovePaneToNewWindow,
    // ---- Wave-2 overlays (Seam #3) ----
    /// Dismiss whatever overlay panel is open.
    CloseOverlay,
    // command palette
    PaletteOpen,
    PaletteQuery(String),
    /// Move the highlighted palette row by ±1.
    PaletteNav(i32),
    /// Highlight a specific visible palette row (a mouse hover/click).
    PaletteSelect(usize),
    /// Run the highlighted palette row's command (then close the palette).
    PaletteActivate,
    // preferences
    PrefsOpen,
    ApplySetting(Setting),
    /// Edit the appearance draft (previews only; commits on Done).
    DraftSetting(Setting),
    /// Commit the appearance draft and close (the Done button / Save).
    PrefsDone,
    /// Resolve the save/discard prompt: 0 keep · 1 discard · 2 save.
    PrefsConfirm(i32),
    /// Font picker: select option `i` (== FONT_OPTIONS.len() → Custom… mode).
    FontSelect(usize),
    /// Font picker: set the custom font path typed in the Custom… field.
    FontCustomValue(String),
    // sidebar / projects
    /// Show/hide the whole right-edge rail.
    ToggleSidebar,
    /// Expand/collapse the projects flyout behind the 📁 icon.
    ToggleProjects,
    OpenProject(usize),
    /// Recolor flyout row `0` to palette swatch `1`.
    SetProjectColor(usize, usize),
    /// Rename flyout row `0` to `1`.
    RenameProject(usize, String),
    /// Forget flyout row `0`.
    RemoveProject(usize),
}

/// A side effect the controller must apply outside the state (UI/window layer). The
/// multi-window layer ([`crate::app`]) applies these against the owning window + the
/// app-level window registry.
#[derive(Debug)]
pub enum Effect {
    None,
    /// The workspace is empty — close this window (and quit when it was the last).
    Quit,
    /// Apply OS fullscreen (true) or restore (false) to this window.
    SetFullscreen(bool),
    /// Open a fresh empty OS window.
    NewWindow,
    /// Re-host `det` in a new OS window; `source_alive` is `false` when detaching it
    /// emptied this window (so the controller closes it).
    MoveToNewWindow { det: DetachedPane, source_alive: bool },
    /// Re-host a whole tab (its panes, title + layout) in a new OS window. `source_alive`
    /// is `false` when moving it emptied this window.
    MoveTabToNewWindow { tab: DetachedTab, source_alive: bool },
}

/// The keyboard layout-cycle order (skips `single`, which the menu still offers).
const LAYOUT_CYCLE: [Layout; 5] = [
    Layout::Auto,
    Layout::Columns,
    Layout::Rows,
    Layout::Grid,
    Layout::MainStack,
];

/// Run `cmd` against `state`. Returns any [`Effect`] the caller must apply.
pub fn dispatch(state: &mut State, cmd: Command, mgr: &SessionManager) -> Effect {
    // Any action other than renaming itself cancels an in-progress tab rename,
    // so the inline edit box never lingers when you interact elsewhere.
    if state.editing_tab != -1
        && !matches!(cmd, Command::BeginRename(_) | Command::RenameTab(..))
    {
        state.editing_tab = -1;
        state.dirty = true;
    }
    // Likewise, any action other than a pane rename cancels an in-progress pane-label edit
    // (so the inline box never lingers when you interact elsewhere).
    if state.editing_pane != -1
        && !matches!(cmd, Command::BeginRenamePane(_) | Command::RenamePane(..))
    {
        state.editing_pane = -1;
        state.dirty = true;
    }
    match cmd {
        Command::NewPane => state.add_pane(mgr),
        Command::OpenNewPane => state.open_new_pane(),
        Command::SubmitNewPane(opts) => {
            state.add_pane_opts(mgr, opts);
            state.close_overlay();
        }
        Command::CloseFocused => {
            let f = state.active_tab().focused;
            if !state.close_pane(f, mgr) {
                return Effect::Quit;
            }
        }
        Command::ClosePane(i) => {
            if !state.close_pane(i, mgr) {
                return Effect::Quit;
            }
        }
        Command::FocusPane(i) => state.focus_pane(i),
        Command::FocusDir(d) => state.focus_dir(d),
        Command::SetLayout(l) => state.set_layout(l),
        Command::CycleLayout => {
            let cur = state.active_tab().layout;
            let i = LAYOUT_CYCLE.iter().position(|l| *l == cur).unwrap_or(0);
            state.set_layout(LAYOUT_CYCLE[(i + 1) % LAYOUT_CYCLE.len()]);
        }
        Command::ToggleZoom => state.toggle_zoom(),
        Command::ToggleFullscreen => {
            let on = !state.fullscreen;
            state.set_fullscreen(on);
            return Effect::SetFullscreen(on);
        }
        Command::FontZoom(delta) => state.font_zoom(delta),
        Command::FontReset => state.font_reset(),
        Command::ResizeDivider { kind, index, delta } => state.resize_divider(kind, index, delta),
        Command::NewTab => state.new_tab(mgr),
        Command::CloseTab(i) => {
            // Reopenable close: with ≥2 tabs the tab is parked (sessions alive) on the closed
            // stack; closing the last tab still kills + quits.
            if !state.close_tab_menu(i, mgr) {
                return Effect::Quit;
            }
        }
        Command::SwitchTab(i) => state.switch_tab(i),
        Command::NextTab => state.cycle_tab(1),
        Command::PrevTab => state.cycle_tab(-1),
        Command::BeginRename(i) => state.begin_rename(i),
        Command::RenameTab(i, t) => state.rename_tab(i, &t),
        Command::BeginRenamePane(i) => state.begin_rename_pane(i),
        Command::RenamePane(i, t) => state.rename_pane(i, &t),
        // ---- pane context-menu actions ----
        Command::RecolorPane(i, swatch) => state.recolor_pane(i, swatch),
        Command::SetPaneFrame(i, on) => state.set_pane_frame(i, on),
        Command::SetPaneDot(i, on) => state.set_pane_dot(i, on),
        Command::ToggleMuteAi(i) => state.toggle_mute_ai(i),
        Command::ZoomPane(i) => state.zoom_pane(i),
        Command::FullscreenPane(i) => {
            state.focus_pane(i);
            let on = !state.fullscreen;
            state.set_fullscreen(on);
            return Effect::SetFullscreen(on);
        }
        Command::RestartPane(i) => state.restart_pane(i, mgr),
        Command::RefreshEnvPane(i) => state.refresh_env_pane(i, mgr),
        Command::OpenLinkedTerminal(i) => state.open_linked_terminal(i, mgr),
        Command::RevealPaneCwd(i) => {
            // Open the pane's live cwd (reported by shell integration) in the OS file explorer.
            if let Some(cwd) = state.active_tab().panes.get(i).and_then(|p| p.cwd.clone()) {
                #[cfg(windows)]
                let _ = std::process::Command::new("explorer").arg(&cwd).spawn();
                #[cfg(not(windows))]
                let _ = std::process::Command::new("xdg-open").arg(&cwd).spawn();
            }
        }
        Command::SearchPane(i) => state.open_search(i),
        Command::SearchFocused => {
            let f = state.active_tab().focused;
            state.open_search(f);
        }
        Command::CopyPane(i) => state.copy_pane(i),
        Command::PastePane(i) => state.paste_pane(i, mgr),
        Command::PasteFocused => {
            let f = state.active_tab().focused;
            state.paste_pane(f, mgr);
        }
        Command::SelectAllPane(i) => state.select_all_pane(i),
        Command::ClearPane(i) => state.clear_pane(i),
        // ---- reminder panes ----
        Command::RemindPane(i, off) => state.remind_pane(i, off),
        Command::ToggleReminders => state.toggle_reminders(),
        Command::RestoreReminder(uid) => state.restore_reminder(&uid, mgr),
        Command::MovePaneToNewTab(i) => state.move_pane_to_new_tab(i, mgr),
        Command::MovePaneToTab(i, t) => state.move_pane_to_tab(i, t, mgr),
        // ---- tab context-menu actions ----
        Command::DuplicateTab(i) => state.duplicate_tab(i, mgr),
        Command::CloseOtherTabs(i) => state.close_other_tabs(i, mgr),
        Command::CloseTabsToRight(i) => state.close_tabs_to_right(i, mgr),
        Command::ReopenClosedTab => state.reopen_closed_tab(mgr),
        Command::SetTabLayout(i, l) => state.set_tab_layout(i, l),
        Command::MoveTabToNewWindow(i) => {
            if let Some((tab, source_alive)) = state.detach_tab(i) {
                return Effect::MoveTabToNewWindow { tab, source_alive };
            }
        }
        // ---- context-menu lifecycle ----
        Command::OpenPaneContext(i, x, y) => state.open_pane_context(i, x, y),
        Command::OpenTaskbarContext(i, x, y) => state.open_taskbar_context(i, x, y),
        Command::OpenTabContext(i, x, y) => state.open_tab_context(i, x, y),
        Command::OpenAppContext(x, y) => state.open_app_context(x, y),
        Command::CloseContext => state.close_context(),
        // ---- workspace file (application menu) ----
        Command::OpenWorkspace => state.open_workspace(mgr),
        Command::SaveWorkspace => state.save_workspace(),
        // ---- multi-window ----
        Command::NewWindow => return Effect::NewWindow,
        Command::MovePaneToNewWindow => {
            if let Some((det, source_alive)) = state.detach_focused(mgr) {
                return Effect::MoveToNewWindow { det, source_alive };
            }
        }
        // ---- Wave-2 overlays ----
        Command::CloseOverlay => state.close_overlay(),
        Command::PaletteOpen => state.open_palette(),
        Command::PaletteQuery(q) => state.palette_set_query(&q),
        Command::PaletteNav(d) => state.palette_nav(d),
        Command::PaletteSelect(i) => state.palette_select(i),
        Command::PaletteActivate => {
            // Run the highlighted entry's command through the same dispatch, then close.
            if let Some(inner) = state.palette_command() {
                state.close_overlay();
                return dispatch(state, inner, mgr);
            }
            state.close_overlay();
        }
        Command::PrefsOpen => state.open_prefs(),
        Command::ApplySetting(s) => state.apply_setting(s),
        Command::DraftSetting(s) => state.draft_setting(s),
        Command::PrefsDone => state.prefs_done(),
        Command::PrefsConfirm(a) => state.prefs_confirm_resolve(a),
        Command::FontSelect(i) => state.font_select(i),
        Command::FontCustomValue(v) => state.font_custom_value(v),
        Command::ToggleSidebar => state.toggle_sidebar(),
        Command::ToggleProjects => state.toggle_projects(),
        Command::OpenProject(i) => state.open_project(i, mgr),
        Command::SetProjectColor(i, swatch) => state.set_project_color(i, swatch),
        Command::RenameProject(i, name) => state.rename_project(i, &name),
        Command::RemoveProject(i) => state.remove_project(i),
    }
    Effect::None
}

/// Map a layout menu id (from the Slint picker) to a `SetLayout` command.
pub fn set_layout_from_id(id: i32) -> Command {
    Command::SetLayout(theme::layout_from_id(id))
}
