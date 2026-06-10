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
    pub const REMINDER: i32 = 5;
}

/// `pick(int)` rows at/above this base are not row indices: the Reminder flyout's Custom
/// input encodes its Rust-parsed minutes as `BASE + minutes` through the frozen `pick`
/// channel (the same encode-on-a-frozen-callback trick as the reopen-chain sentinel) —
/// decoded by [`State::ctx_command`]. Mirrored by `custom-remind-base` in contextmenu.slint.
pub const CTX_CUSTOM_REMIND_BASE: usize = 1_000_000;

/// One rendered context-menu row.
#[derive(Clone)]
pub struct CtxEntry {
    pub label: SharedString,
    pub shortcut: SharedString,
    /// Drawn-icon kind (see [`crate::theme::menu_icon`]); `0` = no icon.
    pub icon: i32,
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
            icon: 0,
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
        self.row(label, "", 0, false, false, false, false, sub::NONE, Some(cmd))
    }
    /// A row carrying every optional flag.
    #[allow(clippy::too_many_arguments)]
    fn row(
        &mut self,
        label: &str,
        shortcut: &str,
        icon: i32,
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
            icon,
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
    /// Append a command slot PAST the visible rows (no entry). The Slint side addresses
    /// these as `entries.length + n` — the Reminder flyout's quick-offset rows, which live
    /// in the flyout (not the top-level model) but still dispatch through `pick(int)`.
    /// Call only after every visible row is pushed so the indices stay stable.
    fn extra(&mut self, cmd: Command) -> &mut Self {
        self.commands.push(Some(cmd));
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
    b.row("Change Color", "", 0,false, false, false, false, sub::COLOR, None);
    b.row("Show Frame", "", 0,frame_on, true, false, false, sub::NONE, Some(Command::SetPaneFrame(idx, !frame_on)));
    b.row("Show Dot", "", 0,dot_on, true, false, false, sub::NONE, Some(Command::SetPaneDot(idx, !dot_on)));
    b.row("Mute AI Summary", "", 0,muted, true, false, false, sub::NONE, Some(Command::ToggleMuteAi(idx)));
    b.sep();
    // Maximize is meaningless on the taskbar's single surface, so it's dropped there.
    if !in_taskbar {
        b.row(
            if zoomed { "Restore" } else { "Maximize" },
            &zoom_sc, 0, false, false, false, false, sub::NONE, Some(Command::ZoomPane(idx)),
        );
    }
    b.row(
        if fullscreen { "Exit Fullscreen" } else { "Fullscreen" },
        &full_sc, 0, false, false, false, false, sub::NONE, Some(Command::FullscreenPane(idx)),
    );
    // The widget's in-pane search is Ctrl+F (not an app keybinding), shown literally.
    b.row("Search…", "Ctrl+F", 0, false, false, false, false, sub::NONE, Some(Command::SearchPane(idx)));
    b.item("Restart", Command::RestartPane(idx));
    b.item("Refresh Env", Command::RefreshEnvPane(idx));
    b.item("Open Folder", Command::RevealPaneCwd(idx));
    b.sep();
    b.row("Copy", "", 0,false, false, !has_sel, false, sub::NONE, Some(Command::CopyPane(idx)));
    b.item("Paste", Command::PastePane(idx));
    b.item("Select All", Command::SelectAllPane(idx));
    b.item("Clear", Command::ClearPane(idx));
    b.sep();
    // ---- "Reminder ▸" — park the pane (session alive) until the chosen time. A single
    // submenu row; the flyout offers the four quick offsets plus an inline Custom input.
    // Disabled when this is the only pane of the only tab (parking it would empty the
    // window). Offsets resolve against the LOCAL clock at click time.
    let cant_park = state.tabs.len() <= 1 && n < 2;
    b.row("Reminder", "", 0,false, false, cant_park, false, sub::REMINDER, None);
    b.sep();
    b.row("Move to New Tab", "", 0,false, false, n < 2, false, sub::NONE, Some(Command::MovePaneToNewTab(idx)));
    if others {
        b.row("Move to Tab", "", 0,false, false, false, false, sub::MOVE_TO_TAB, None);
    }
    b.sep();
    b.row("Close Pane", "", 0,false, false, false, true, sub::NONE, Some(Command::ClosePane(idx)));

    // The Reminder flyout's quick offsets, in hidden slots past the visible rows (the
    // Slint flyout dispatches `pick(entries.length + j)` — order must match its rows).
    {
        use crate::state::ReminderOffset as Off;
        for off in [Off::Min15, Off::Hour1, Off::Hour3, Off::Tomorrow9] {
            b.extra(Command::RemindPane(idx, off));
        }
    }

    b.finish(CtxKind::Pane, idx, x, y)
}

/// Build the tab-strip menu for tab `idx`.
pub fn tab_menu(state: &State, idx: usize, x: f32, y: f32) -> CtxMenu {
    let mut b = Build::new();
    let only = state.tabs.len() < 2;
    let is_last = idx + 1 >= state.tabs.len();
    let no_closed = state.closed_tabs.is_empty();

    let new_sc = state.keymap.label_for("tab.new").unwrap_or_default();

    b.row("New Tab", &new_sc, 0, false, false, false, false, sub::NONE, Some(Command::NewTab));
    b.item("Rename…", Command::BeginRename(idx as i32));
    b.item("Duplicate Tab", Command::DuplicateTab(idx));
    b.row("Move to New Window", "", 0,false, false, only, false, sub::NONE, Some(Command::MoveTabToNewWindow(idx)));
    b.sep();
    b.item("Close Tab", Command::CloseTab(idx));
    b.row("Close Other Tabs", "", 0,false, false, only, false, sub::NONE, Some(Command::CloseOtherTabs(idx)));
    b.row("Close Tabs to the Right", "", 0,false, false, is_last, false, sub::NONE, Some(Command::CloseTabsToRight(idx)));
    b.row("Reopen Closed Tab", "", 0,false, false, no_closed, false, sub::NONE, Some(Command::ReopenClosedTab));
    b.sep();
    b.row("Layout", "", 0,false, false, false, false, sub::LAYOUT, None);

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
        "New pane…", "", crate::theme::menu_icon::NEW_PANE,
        false, false, false, false, sub::NONE, Some(Command::OpenNewPane),
    );
    b.row(
        "Command palette", &palette_sc, crate::theme::menu_icon::COMMAND_PALETTE,
        false, false, false, false, sub::NONE, Some(Command::PaletteOpen),
    );
    b.sep();
    // Layout submenu header: drawn icon + label of the CURRENT layout (the submenu lists
    // Automatic + the 5 presets with the radio ✓ on the current). The current label sits in
    // the shortcut slot (mirrors Electron's "{current.label} ▸").
    b.row(
        "Layout",
        crate::theme::layout_label(cur),
        crate::theme::layout_icon_kind(cur),
        false, false, false, false, sub::LAYOUT, None,
    );
    b.sep();
    b.row(
        "Open workspace…", "", crate::theme::menu_icon::OPEN_WORKSPACE,
        false, false, false, false, sub::NONE, Some(Command::OpenWorkspace),
    );
    b.row(
        "Save workspace…", "", crate::theme::menu_icon::SAVE_WORKSPACE,
        false, false, false, false, sub::NONE, Some(Command::SaveWorkspace),
    );
    b.sep();
    b.row(
        "Preferences…", "", crate::theme::menu_icon::PREFERENCES,
        false, false, false, false, sub::NONE, Some(Command::PrefsOpen),
    );

    // Target = the active tab, so the Layout submenu (which routes through `ctx_target` →
    // `SetTabLayout`) retargets the *current* tab's layout (mirrors Electron's `setLayout`).
    b.finish(CtxKind::App, state.active, x, y)
}

/// Parse the Reminder flyout's Custom input into minutes-from-now (1..=1440):
/// - a plain number = minutes (`"45"`),
/// - `1h30` / `2h` / `1h30m` / `90m` durations,
/// - `HH:MM` = that local time today, or tomorrow once it has already passed
///   (`now_secs_since_midnight` is the local clock, see `state::local_secs_since_midnight`).
///
/// `None` for anything else (empty, zero, malformed, past the 24 h cap — the cap keeps the
/// due label's "tomorrow HH:MM" arithmetic in `state::due_for` honest).
pub fn parse_custom_duration(s: &str, now_secs_since_midnight: u64) -> Option<u32> {
    const DAY_MIN: u32 = 24 * 60;
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    // HH:MM — an absolute local time; rolls to tomorrow when not strictly in the future.
    if let Some((hh, mm)) = s.split_once(':') {
        let (h, m): (u32, u32) = (hh.parse().ok()?, mm.parse().ok()?);
        if h >= 24 || m >= 60 {
            return None;
        }
        let target = (h * 3_600 + m * 60) as u64;
        let now = now_secs_since_midnight;
        let delta = if target > now { target - now } else { target + 86_400 - now };
        return Some((((delta + 59) / 60) as u32).max(1));
    }
    // NhM / Nh — hours with optional trailing minutes (the `m` suffix optional there too).
    if let Some((hh, rest)) = s.split_once('h') {
        let h: u32 = hh.parse().ok()?;
        let rest = rest.strip_suffix('m').unwrap_or(rest);
        let m: u32 = if rest.is_empty() { 0 } else { rest.parse().ok()? };
        if m >= 60 {
            return None;
        }
        let total = h * 60 + m;
        return (1..=DAY_MIN).contains(&total).then_some(total);
    }
    // 90m / bare minutes.
    let digits = s.strip_suffix('m').unwrap_or(&s);
    let m: u32 = digits.parse().ok()?;
    (1..=DAY_MIN).contains(&m).then_some(m)
}

/// [`parse_custom_duration`] against the live local clock — the pure bridge app.rs wires
/// into the `ReminderCustom` Slint global's `parse-minutes` callback.
pub fn parse_custom_minutes_now(s: &str) -> Option<u32> {
    parse_custom_duration(s, crate::state::local_secs_since_midnight())
}

#[cfg(test)]
mod tests {
    use super::parse_custom_duration;

    const NOON: u64 = 12 * 3_600;

    #[test]
    fn bare_numbers_are_minutes() {
        assert_eq!(parse_custom_duration("45", NOON), Some(45));
        assert_eq!(parse_custom_duration(" 5 ", NOON), Some(5));
        assert_eq!(parse_custom_duration("1440", NOON), Some(1440));
    }

    #[test]
    fn duration_suffix_forms() {
        assert_eq!(parse_custom_duration("90m", NOON), Some(90));
        assert_eq!(parse_custom_duration("2h", NOON), Some(120));
        assert_eq!(parse_custom_duration("1h30", NOON), Some(90));
        assert_eq!(parse_custom_duration("1h30m", NOON), Some(90));
        assert_eq!(parse_custom_duration("0h45", NOON), Some(45));
        assert_eq!(parse_custom_duration("24h", NOON), Some(1440));
    }

    #[test]
    fn absolute_times_resolve_today_or_tomorrow() {
        // 14:30 from noon → 2h30 out.
        assert_eq!(parse_custom_duration("14:30", NOON), Some(150));
        // 09:00 from noon already passed → tomorrow morning (21 h).
        assert_eq!(parse_custom_duration("9:00", NOON), Some(21 * 60));
        // Exactly "now" rolls a full day.
        assert_eq!(parse_custom_duration("12:00", NOON), Some(1440));
        // Sub-minute remainders round UP so the reminder never fires early.
        assert_eq!(parse_custom_duration("14:30", NOON + 30), Some(150));
    }

    #[test]
    fn garbage_zero_and_out_of_range_are_rejected() {
        for bad in ["", "  ", "0", "0m", "0h", "abc", "1d", "h30", ":30", "25:00",
                    "12:60", "1h60", "-5", "1441", "25h"] {
            assert_eq!(parse_custom_duration(bad, NOON), None, "{bad:?} must not parse");
        }
    }
}
