//! Cursor-anchored context menus for the pane header + tab strip — the native port of
//! `src/renderer/components/contextMenus.tsx`.
//!
//! Each menu is built **fresh** the moment a right-click opens it (see
//! [`State::open_pane_context`](crate::state::State::open_pane_context) /
//! [`open_tab_context`](crate::state::State::open_tab_context)), so every label, gating flag and
//! checkmark reflects the live state at open time — exactly like the renderer's pure builders.
//! The rows are pushed into Slint models by the resync ([`crate::paneview`]); a click maps back
//! to the [`Command`] stored alongside each row (submenus — Change Color / Move to Tab / Layout —
//! carry their own payload and are resolved in the controller against the menu's target).

use slint::SharedString;

use crate::command::Command;
use crate::state::State;

/// Which surface a context menu targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxKind {
    Pane,
    Tab,
    /// The application (hamburger) menu. Reuses the cursor-anchored menu component anchored
    /// just below the top-bar hamburger, so it shares the items/submenu/separator/shortcut/
    /// checkmark/glyph styling rather than duplicating a popup. See [`app_menu`].
    App,
}

/// A submenu kind a row can open (`0` = none, a plain item/separator).
pub mod sub {
    pub const NONE: i32 = 0;
    pub const COLOR: i32 = 2;
    pub const MOVE_TO_TAB: i32 = 3;
    pub const LAYOUT: i32 = 4;
}

/// One rendered context-menu row.
#[derive(Clone)]
pub struct CtxEntry {
    pub label: SharedString,
    pub shortcut: SharedString,
    pub glyph: SharedString,
    /// `-1` separator · `0` item · `2`/`3`/`4` a submenu (see [`sub`]).
    pub kind: i32,
    pub checked: bool,
    pub show_check: bool,
    pub disabled: bool,
    pub danger: bool,
}

impl CtxEntry {
    fn sep() -> Self {
        CtxEntry {
            label: SharedString::new(),
            shortcut: SharedString::new(),
            glyph: SharedString::new(),
            kind: -1,
            checked: false,
            show_check: false,
            disabled: false,
            danger: false,
        }
    }
}

/// An open context menu: where it sits (window-logical px), what it targets, its display rows and
/// the [`Command`] each row runs (`None` for separators + submenu headers).
pub struct CtxMenu {
    pub kind: CtxKind,
    pub target: usize,
    pub x: f32,
    pub y: f32,
    pub entries: Vec<CtxEntry>,
    pub commands: Vec<Option<Command>>,
}

/// A small builder that keeps `entries` and `commands` in lock-step.
struct Build {
    entries: Vec<CtxEntry>,
    commands: Vec<Option<Command>>,
}

impl Build {
    fn new() -> Self {
        Build { entries: Vec::new(), commands: Vec::new() }
    }
    /// A plain action row.
    fn item(&mut self, label: &str, cmd: Command) -> &mut Self {
        self.row(label, "", "", false, false, false, false, sub::NONE, Some(cmd))
    }
    /// A row carrying every optional flag.
    #[allow(clippy::too_many_arguments)]
    fn row(
        &mut self,
        label: &str,
        shortcut: &str,
        glyph: &str,
        checked: bool,
        show_check: bool,
        disabled: bool,
        danger: bool,
        kind: i32,
        cmd: Option<Command>,
    ) -> &mut Self {
        self.entries.push(CtxEntry {
            label: label.into(),
            shortcut: shortcut.into(),
            glyph: glyph.into(),
            kind,
            checked,
            show_check,
            disabled,
            danger,
        });
        self.commands.push(cmd);
        self
    }
    fn sep(&mut self) -> &mut Self {
        self.entries.push(CtxEntry::sep());
        self.commands.push(None);
        self
    }
    fn finish(self, kind: CtxKind, target: usize, x: f32, y: f32) -> CtxMenu {
        CtxMenu { kind, target, x, y, entries: self.entries, commands: self.commands }
    }
}

/// Build the pane menu for active-tab pane `idx`. Shared by the pane header (`in_taskbar`
/// false) and the single-layout taskbar (`in_taskbar` true) — the native port of
/// `buildPaneMenu(paneId, groupId, { inTaskbar })`. In the taskbar variant a leading
/// **Show** row is prepended (left-click already shows the pane, but it's offered as the
/// default row) and the **Maximize/Restore** row is dropped (maximize is meaningless when
/// the single preset already fills the area).
pub fn pane_menu(state: &State, idx: usize, x: f32, y: f32, in_taskbar: bool) -> CtxMenu {
    let mut b = Build::new();
    let t = state.active_tab();
    let global_frame = state.settings.show_frame;
    let global_dot = state.settings.show_dot;
    // Live per-pane state at open time.
    let (frame_on, dot_on, muted, has_sel) = match t.panes.get(idx) {
        Some(p) => (
            p.frame_on(global_frame),
            p.dot_on(global_dot),
            p.ai_muted,
            p.pane.selection_text().is_some(),
        ),
        None => (global_frame, global_dot, false, false),
    };
    let zoomed = t.zoomed == Some(idx);
    let fullscreen = state.fullscreen && t.focused == idx;
    let n = t.panes.len();
    let others = state.tabs.len() > 1;

    let zoom_sc = state.keymap.label_for("pane.toggleZoom").unwrap_or_default();
    let full_sc = state.keymap.label_for("pane.toggleFullscreen").unwrap_or_default();

    // Taskbar variant: a leading "Show" row (focus → the single preset shows it) + separator.
    if in_taskbar {
        b.item("Show", Command::FocusPane(idx));
        b.sep();
    }

    b.item("New Pane…", Command::OpenNewPane);
    b.item("Rename…", Command::BeginRenamePane(idx as i32));
    b.row("Change Color", "", "", false, false, false, false, sub::COLOR, None);
    b.row("Show Frame", "", "", frame_on, true, false, false, sub::NONE, Some(Command::SetPaneFrame(idx, !frame_on)));
    b.row("Show Dot", "", "", dot_on, true, false, false, sub::NONE, Some(Command::SetPaneDot(idx, !dot_on)));
    b.row("Mute AI Summary", "", "", muted, true, false, false, sub::NONE, Some(Command::ToggleMuteAi(idx)));
    b.sep();
    // Maximize is meaningless on the taskbar's single surface, so it's dropped there.
    if !in_taskbar {
        b.row(
            if zoomed { "Restore" } else { "Maximize" },
            &zoom_sc, "", false, false, false, false, sub::NONE, Some(Command::ZoomPane(idx)),
        );
    }
    b.row(
        if fullscreen { "Exit Fullscreen" } else { "Fullscreen" },
        &full_sc, "", false, false, false, false, sub::NONE, Some(Command::FullscreenPane(idx)),
    );
    // The widget's in-pane search is Ctrl+F (not an app keybinding), shown literally.
    b.row("Search…", "Ctrl+F", "", false, false, false, false, sub::NONE, Some(Command::SearchPane(idx)));
    b.item("Restart", Command::RestartPane(idx));
    b.item("Refresh Env", Command::RefreshEnvPane(idx));
    b.item("Open Linked Terminal", Command::OpenLinkedTerminal(idx));
    b.item("Open Folder", Command::RevealPaneCwd(idx));
    b.sep();
    b.row("Copy", "", "", false, false, !has_sel, false, sub::NONE, Some(Command::CopyPane(idx)));
    b.item("Paste", Command::PastePane(idx));
    b.item("Select All", Command::SelectAllPane(idx));
    b.item("Clear", Command::ClearPane(idx));
    b.sep();
    // ---- Track F: "Remind at…" — park the pane (session alive) until the chosen time.
    // Plain rows rather than a custom submenu (the menu component is owned by another
    // track); disabled when this is the only pane of the only tab (parking it would empty
    // the window). Offsets resolve against the LOCAL clock at click time.
    {
        let cant_park = state.tabs.len() <= 1 && n < 2;
        use crate::state::ReminderOffset as Off;
        for (label, off) in [
            ("Remind in 15 min", Off::Min15),
            ("Remind in 1 hour", Off::Hour1),
            ("Remind in 3 hours", Off::Hour3),
            ("Remind tomorrow 9 AM", Off::Tomorrow9),
        ] {
            b.row(label, "", "", false, false, cant_park, false, sub::NONE,
                Some(Command::RemindPane(idx, off)));
        }
    }
    b.sep();
    b.row("Move to New Tab", "", "", false, false, n < 2, false, sub::NONE, Some(Command::MovePaneToNewTab(idx)));
    if others {
        b.row("Move to Tab", "", "", false, false, false, false, sub::MOVE_TO_TAB, None);
    }
    b.sep();
    b.row("Close Pane", "", "", false, false, false, true, sub::NONE, Some(Command::ClosePane(idx)));

    b.finish(CtxKind::Pane, idx, x, y)
}

/// Build the tab-strip menu for tab `idx`.
pub fn tab_menu(state: &State, idx: usize, x: f32, y: f32) -> CtxMenu {
    let mut b = Build::new();
    let only = state.tabs.len() < 2;
    let is_last = idx + 1 >= state.tabs.len();
    let no_closed = state.closed_tabs.is_empty();

    let new_sc = state.keymap.label_for("tab.new").unwrap_or_default();

    b.row("New Tab", &new_sc, "", false, false, false, false, sub::NONE, Some(Command::NewTab));
    b.item("Rename…", Command::BeginRename(idx as i32));
    b.item("Duplicate Tab", Command::DuplicateTab(idx));
    b.row("Move to New Window", "", "", false, false, only, false, sub::NONE, Some(Command::MoveTabToNewWindow(idx)));
    b.sep();
    b.item("Close Tab", Command::CloseTab(idx));
    b.row("Close Other Tabs", "", "", false, false, only, false, sub::NONE, Some(Command::CloseOtherTabs(idx)));
    b.row("Close Tabs to the Right", "", "", false, false, is_last, false, sub::NONE, Some(Command::CloseTabsToRight(idx)));
    b.row("Reopen Closed Tab", "", "", false, false, no_closed, false, sub::NONE, Some(Command::ReopenClosedTab));
    b.sep();
    b.row("Layout", "", "", false, false, false, false, sub::LAYOUT, None);

    b.finish(CtxKind::Tab, idx, x, y)
}

/// Build the application (hamburger) menu, anchored at window-logical `(x, y)`. The native
/// port of the Electron `TopBar` menu: New pane · Command palette (+shortcut) · — · Layout ▸
/// (cascading submenu, radio ✓) · — · Open/Save workspace · — · Preferences. The Layout
/// submenu rows come from the [`crate::theme::LAYOUT_MENU`] model the
/// resync pushes into `ctx_layouts` (with the live checkmark + the Automatic "— <resolved>"
/// hint), exactly like the tab menu's Layout submenu.
pub fn app_menu(state: &State, x: f32, y: f32) -> CtxMenu {
    let mut b = Build::new();
    let cur = state.active_tab().layout;
    let palette_sc = state.keymap.label_for("palette.toggle").unwrap_or_default();

    b.row(
        "New pane…", "", crate::theme::menu_glyph::NEW_PANE,
        false, false, false, false, sub::NONE, Some(Command::OpenNewPane),
    );
    b.row(
        "Command palette", &palette_sc, crate::theme::menu_glyph::COMMAND_PALETTE,
        false, false, false, false, sub::NONE, Some(Command::PaletteOpen),
    );
    b.sep();
    // Layout submenu header: glyph + label of the CURRENT layout (the submenu lists Automatic
    // + the 5 presets with the radio ✓ on the current). The current label sits in the shortcut
    // slot (mirrors Electron's "{current.label} ▸").
    b.row(
        "Layout",
        crate::theme::layout_label(cur),
        crate::theme::layout_icon(cur),
        false, false, false, false, sub::LAYOUT, None,
    );
    b.sep();
    b.row(
        "Open workspace…", "", crate::theme::menu_glyph::OPEN_WORKSPACE,
        false, false, false, false, sub::NONE, Some(Command::OpenWorkspace),
    );
    b.row(
        "Save workspace…", "", crate::theme::menu_glyph::SAVE_WORKSPACE,
        false, false, false, false, sub::NONE, Some(Command::SaveWorkspace),
    );
    b.sep();
    b.row(
        "Preferences…", "", crate::theme::menu_glyph::PREFERENCES,
        false, false, false, false, sub::NONE, Some(Command::PrefsOpen),
    );

    // Target = the active tab, so the Layout submenu (which routes through `ctx_target` →
    // `SetTabLayout`) retargets the *current* tab's layout (mirrors Electron's `setLayout`).
    b.finish(CtxKind::App, state.active, x, y)
}
