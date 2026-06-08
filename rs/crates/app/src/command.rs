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

use crate::state::{Setting, State};
use crate::theme;

/// An action against the workspace. Construct these from any input source.
#[derive(Debug, Clone)]
pub enum Command {
    // panes
    NewPane,
    CloseFocused,
    ClosePane(usize),
    FocusPane(usize),
    FocusDir(Direction),
    // layout
    SetLayout(Layout),
    CycleLayout,
    ToggleZoom,
    ToggleFullscreen,
    ResizeDivider {
        kind: DividerKind,
        index: i32,
        delta: f64,
    },
    // tabs
    NewTab,
    CloseTab(usize),
    SwitchTab(usize),
    BeginRename(i32),
    RenameTab(i32, String),
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
    // sidebar / projects
    ToggleSidebar,
    OpenProject(usize),
}

/// A side effect the controller must apply outside the state (UI/window layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    /// The workspace is empty — hide/close the window.
    Quit,
    /// Apply OS fullscreen (true) or restore (false).
    SetFullscreen(bool),
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
    match cmd {
        Command::NewPane => state.add_pane(mgr),
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
        Command::ResizeDivider { kind, index, delta } => state.resize_divider(kind, index, delta),
        Command::NewTab => state.new_tab(mgr),
        Command::CloseTab(i) => {
            if !state.close_tab(i, mgr) {
                return Effect::Quit;
            }
        }
        Command::SwitchTab(i) => state.switch_tab(i),
        Command::BeginRename(i) => state.begin_rename(i),
        Command::RenameTab(i, t) => state.rename_tab(i, &t),
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
        Command::ToggleSidebar => state.toggle_sidebar(),
        Command::OpenProject(i) => state.open_project(i, mgr),
    }
    Effect::None
}

/// Map a layout menu id (from the Slint picker) to a `SetLayout` command.
pub fn set_layout_from_id(id: i32) -> Command {
    Command::SetLayout(theme::layout_from_id(id))
}
