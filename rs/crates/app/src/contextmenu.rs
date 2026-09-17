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

use avada_core::tools::PaneKind;

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
    /// A row of the left panel's Files tree. Unlike the others this menu carries no index:
    /// each row bakes its own path into its [`Command`], so nothing has to survive the
    /// rebuild that a click on any of these rows triggers. See [`file_menu`].
    File,
}

/// A submenu kind a row can open (`0` = none, a plain item/separator).
pub mod sub {
    pub const NONE: i32 = 0;
    pub const COLOR: i32 = 2;
    pub const MOVE_TO_TAB: i32 = 3;
    pub const LAYOUT: i32 = 4;
    pub const REMINDER: i32 = 5;
    pub const OPEN_WITH: i32 = 6;
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
    /// `-1` separator · `0` item · `2`/`3`/`4`/`5`/`6` a submenu (see [`sub`]).
    pub kind: i32,
    pub checked: bool,
    pub show_check: bool,
    pub disabled: bool,
    pub danger: bool,
}

impl CtxEntry {
    #[tracing::instrument(level = "debug")]
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
    /// Labels for the "Open With" flyout, in the order their commands were appended past
    /// the visible rows — row `j` runs `commands[entries.len() + j]`. Empty for every menu
    /// that has no such flyout.
    pub openwith: Vec<SharedString>,
}

/// A small builder that keeps `entries` and `commands` in lock-step.
struct Build {
    entries: Vec<CtxEntry>,
    commands: Vec<Option<Command>>,
    openwith: Vec<SharedString>,
}

impl Build {
    #[tracing::instrument(level = "debug", skip_all)]
    fn new() -> Self {
        Build {
            entries: Vec::new(),
            commands: Vec::new(),
            openwith: Vec::new(),
        }
    }
    /// A plain action row.
    fn item(&mut self, label: &str, cmd: Command) -> &mut Self {
        self.row(
            label,
            "",
            0,
            false,
            false,
            false,
            false,
            sub::NONE,
            Some(cmd),
        )
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
    /// A row of the "Open With" flyout: a label the flyout draws, and the command it runs,
    /// which lands in the same past-the-visible-rows slots [`Build::extra`] uses.
    fn open_with(&mut self, label: &str, cmd: Command) -> &mut Self {
        self.openwith.push(label.into());
        self.extra(cmd)
    }
    #[tracing::instrument(level = "debug", skip_all)]
    fn finish(self, kind: CtxKind, target: usize, x: f32, y: f32) -> CtxMenu {
        CtxMenu {
            kind,
            target,
            x,
            y,
            entries: self.entries,
            commands: self.commands,
            openwith: self.openwith,
        }
    }
}

/// Build the pane menu for active-tab pane `idx`. Shared by the pane header (`in_taskbar`
/// false) and the single-layout taskbar (`in_taskbar` true) — the native port of
/// `buildPaneMenu(paneId, groupId, { inTaskbar })`. In the taskbar variant a leading
/// **Show** row is prepended (left-click already shows the pane, but it's offered as the
/// default row) and the **Maximize/Restore** row is dropped (maximize is meaningless when
/// the single preset already fills the area).
#[tracing::instrument(level = "debug", skip(state))]
pub fn pane_menu(state: &State, idx: usize, x: f32, y: f32, in_taskbar: bool) -> CtxMenu {
    let mut b = Build::new();
    let t = state.active_tab();
    let global_frame = state.settings.show_frame;
    let global_dot = state.settings.show_dot;
    // Live per-pane state at open time.
    //
    // `pty` is the read-only test. Half of this menu acts on a pane's *session* — talk and
    // dictation, paste, restart, refresh env, clear, the in-pane search over the terminal
    // grid — and a view pane (file browser / viewer / markdown preview) has no session at
    // all (D3). Those rows are omitted there rather than shown greyed: a row that can never
    // apply to this pane is not a disabled action, it is a different pane's menu.
    let (frame_on, dot_on, muted, talk_on, recording, has_sel, pty) = match t.panes.get(idx) {
        Some(p) => (
            p.frame_on(global_frame),
            p.dot_on(global_dot),
            p.ai_muted,
            p.talk,
            p.recording,
            if p.kind.is_pty() {
                p.pane.selection_text().is_some()
            } else {
                crate::viewpane::selected_range(&p.uid).is_some()
            },
            p.kind.is_pty(),
        ),
        None => (global_frame, global_dot, false, false, false, false, true),
    };
    let zoomed = t.zoomed == Some(idx);
    let fullscreen = state.fullscreen && t.focused == idx;
    let n = t.panes.len();
    let others = state.tabs.len() > 1;

    let zoom_sc = state
        .keymap
        .label_for("pane.toggleZoom")
        .unwrap_or_default();
    let full_sc = state
        .keymap
        .label_for("pane.toggleFullscreen")
        .unwrap_or_default();

    // Taskbar variant: a leading "Show" row (focus → the single preset shows it) + separator.
    if in_taskbar {
        b.item("Show", Command::FocusPane(idx));
        b.sep();
    }

    b.item("New Pane…", Command::OpenNewPane);
    b.item("Rename…", Command::BeginRenamePane(idx as i32));
    b.row(
        "Change Color",
        "",
        0,
        false,
        false,
        false,
        false,
        sub::COLOR,
        None,
    );
    b.row(
        "Show Frame",
        "",
        0,
        frame_on,
        true,
        false,
        false,
        sub::NONE,
        Some(Command::SetPaneFrame(idx, !frame_on)),
    );
    b.row(
        "Show Dot",
        "",
        0,
        dot_on,
        true,
        false,
        false,
        sub::NONE,
        Some(Command::SetPaneDot(idx, !dot_on)),
    );
    if pty {
        b.row(
            "Mute AI Summary",
            "",
            0,
            muted,
            true,
            false,
            false,
            sub::NONE,
            Some(Command::ToggleMuteAi(idx)),
        );
        b.row(
            "Talk (speak replies)",
            "",
            0,
            talk_on,
            true,
            false,
            false,
            sub::NONE,
            Some(Command::ToggleTalk(idx)),
        );
        // The other half of Talk: the header microphone, reachable from the menu too. Checked
        // while recording, so the row reads as the stop as well as the start.
        b.row(
            if recording {
                "Dictate (stop and type it)"
            } else {
                "Dictate (record speech)"
            },
            "",
            0,
            recording,
            true,
            false,
            false,
            sub::NONE,
            Some(Command::ToggleDictation(idx)),
        );
    }
    b.sep();
    // Maximize is meaningless on the taskbar's single surface, so it's dropped there.
    if !in_taskbar {
        b.row(
            if zoomed { "Restore" } else { "Maximize" },
            &zoom_sc,
            0,
            false,
            false,
            false,
            false,
            sub::NONE,
            Some(Command::ZoomPane(idx)),
        );
    }
    b.row(
        if fullscreen {
            "Exit Fullscreen"
        } else {
            "Fullscreen"
        },
        &full_sc,
        0,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::FullscreenPane(idx)),
    );
    if pty {
        // The widget's in-pane search shortcut, shown with the platform modifier label
        // (Slint's control slot is Cmd on macOS).
        let search_sc = format!("{}+F", crate::keybindings::CTRL_LABEL);
        b.row(
            "Search…",
            &search_sc,
            0,
            false,
            false,
            false,
            false,
            sub::NONE,
            Some(Command::SearchPane(idx)),
        );
        b.item("Restart", Command::RestartPane(idx));
        b.item("Refresh Env", Command::RefreshEnvPane(idx));
    }
    // Both survive on a view pane: its cwd is the file or directory it is showing, which is
    // exactly what "reveal it" and "browse from here" should mean.
    b.item("Open Folder", Command::RevealPaneCwd(idx));
    b.item("Browse Files", Command::OpenFileBrowser(idx));
    b.sep();
    b.row(
        "Copy",
        "",
        0,
        false,
        false,
        !has_sel,
        false,
        sub::NONE,
        Some(Command::CopyPane(idx)),
    );
    if pty {
        b.item("Paste", Command::PastePane(idx));
    }
    b.item("Select All", Command::SelectAllPane(idx));
    if pty {
        b.item("Clear", Command::ClearPane(idx));
    }
    // ---- Keymap presets — a tier-5 module surface's choice of dialect (Helix, VS Code, …).
    //
    // Flat checkable rows rather than a submenu: a module declares a handful of presets, and
    // a new `sub::` kind would mean new flyout markup for a list that fits inline. The row
    // for the module's *own* default sends `None`, not its name — so a module that later
    // renames or re-points its default is followed rather than frozen at today's spelling.
    if let Some(PaneKind::Module(m)) = t.panes.get(idx).map(|p| &p.kind) {
        let rows = crate::module_ui::grid::preset_rows(&m.id, &m.surface);
        if !rows.is_empty() {
            b.sep();
            let default = crate::module_ui::grid::default_preset_name(&m.id, &m.surface);
            for (name, label, active) in rows {
                let arg = if default.as_deref() == Some(name.as_str()) {
                    None
                } else {
                    Some(name)
                };
                b.row(
                    &label,
                    "",
                    0,
                    active,
                    true,
                    false,
                    false,
                    sub::NONE,
                    Some(Command::SetModulePreset(
                        m.id.as_str().to_string(),
                        m.surface.clone(),
                        arg,
                    )),
                );
            }
        }
    }
    b.sep();
    // ---- "Reminder ▸" — park the pane (session alive) until the chosen time. A single
    // submenu row; the flyout offers the four quick offsets plus an inline Custom input.
    // Disabled when this is the only pane of the only tab (parking it would empty the
    // window). Offsets resolve against the LOCAL clock at click time.
    let cant_park = state.tabs.len() <= 1 && n < 2;
    b.row(
        "Reminder",
        "",
        0,
        false,
        false,
        cant_park,
        false,
        sub::REMINDER,
        None,
    );
    b.sep();
    b.row(
        "Move to New Tab",
        "",
        0,
        false,
        false,
        n < 2,
        false,
        sub::NONE,
        Some(Command::MovePaneToNewTab(idx)),
    );
    if others {
        b.row(
            "Move to Tab",
            "",
            0,
            false,
            false,
            false,
            false,
            sub::MOVE_TO_TAB,
            None,
        );
    }
    b.sep();
    b.row(
        "Close Pane",
        "",
        0,
        false,
        false,
        false,
        true,
        sub::NONE,
        Some(Command::ClosePane(idx)),
    );

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
/// Build the row menu for a file or directory in the left panel's Files tree.
///
/// **Flat, with one exception.** The whole reason a clicked filename lands in the panel
/// instead of the OS handler is so the human can choose what happens next; hiding that
/// choice one level down would undo it. The "Open in …" rows are therefore one per
/// *installed* editor — resolved here, at open time, so an editor installed since launch
/// appears and one that was never installed never does. That costs a handful of filesystem
/// probes per right-click, which is the same bargain every other builder in this file
/// already makes by rebuilding from scratch.
///
/// The exception is **Open With**, which is a flyout because it is not our list: it is
/// whatever the OS says can open this kind of file, and on a working machine that is a
/// dozen applications — long enough to push every other verb off the bottom of the card,
/// and the one list here a human opens knowing they are going to browse it.
///
/// Each row carries its own fully-formed [`Command`] with the path baked in, so unlike the
/// pane and tab menus this one needs no target index — which is what lets the row list be
/// rebuilt from disk by the very click that dispatches from it.
#[tracing::instrument(level = "debug", skip(state))]
pub fn file_menu(
    state: &State,
    path: &std::path::Path,
    git: Option<&crate::state::GitOrigin>,
    x: f32,
    y: f32,
) -> CtxMenu {
    let mut b = Build::new();
    let p = path.display().to_string();
    let is_dir = path.is_dir();
    let is_md = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));

    // A row a module listed out of a repository gets the diff verb first, because it is why
    // that row was clicked. Everything below still applies: whatever revision put the name
    // in the list, the file on disk is an ordinary file.
    //
    // This is also the only place the diff verb can live. Running `git show` means spawning
    // a process, which no module may do; the module supplies the fact (which revision this
    // row came from) and the host supplies the verb.
    if let Some(g) = git {
        // The repository root itself is the whole-tree / whole-commit diff — the job the
        // panel's header button used to have, now reachable from the row that means "all
        // of it" instead of from a button the host no longer draws.
        let rel = path
            .strip_prefix(&g.root)
            .ok()
            .map(|r| r.to_string_lossy().into_owned())
            .filter(|r| !r.is_empty());
        // Untracked files have nothing in HEAD to diff against, so the row would open a
        // pane that prints nothing. Asked of git rather than inferred from the row: the row
        // is a module's claim, and this one spawns a process.
        let diffable = match (&g.rev, &rel) {
            (Some(_), _) => true,
            (None, None) => true,
            (None, Some(rel)) => avada_core::git::is_tracked(&g.root, rel),
        };
        if diffable {
            let label = match &g.rev {
                Some(_) => format!("Show Diff in {}", g.short),
                None => "Show Diff".to_string(),
            };
            b.item(
                &label,
                Command::GitDiff {
                    root: g.root.to_string_lossy().into_owned(),
                    rev: g.rev.clone(),
                    path: rel,
                },
            );
            b.sep();
        }
    }

    // Asked before any row is built, because two rows depend on the answer and the OS is
    // the one that knows it.
    let handlers = if is_dir {
        Vec::new()
    } else {
        avada_core::open::handlers_for(path)
    };

    if is_dir {
        b.item("Open as Root", Command::FilesSetRoot(p.clone()));
        b.item("New File Browser Pane", Command::FilesOpen(p.clone()));
        b.item("Open in Terminal", Command::TerminalAt(p.clone()));
    } else {
        if is_md {
            // First, because for a `.md` the rendered view is what "open" means to a human.
            b.item("Preview Markdown", Command::FilesOpen(p.clone()));
        } else {
            b.item("Open in Viewer", Command::FilesOpen(p.clone()));
        }
        for def in avada_core::tools::registry::editors() {
            if avada_core::tools::detect::resolve(def, &state.settings.tool_paths).is_none() {
                continue;
            }
            b.row(
                &format!("Open in {}", def.name),
                "",
                def.icon as i32,
                false,
                false,
                false,
                false,
                sub::NONE,
                Some(Command::OpenPathWith {
                    path: p.clone(),
                    tool: def.id.to_string(),
                }),
            );
        }
        // The human's own `editorCommand`, if they set one. Last of the "Open in …" rows
        // because the registry rows above are discovered — this one was asked for, so it
        // reads as the deliberate choice it is rather than competing with them. Absent
        // entirely when the preference is blank, which is the default: a row that opened
        // nothing would be worse than no row.
        //
        // No line or column: by the time a row is right-clicked, the position a clicked
        // terminal path carried has already gone to the Files module and the host kept no
        // copy. `Command::OpenPathInEditor` takes them anyway, for the caller that one day
        // does know.
        if !state.settings.editor_command.trim().is_empty() {
            b.item(
                "Open in Editor",
                Command::OpenPathInEditor {
                    path: p.clone(),
                    line: None,
                    col: None,
                },
            );
        }
        if !handlers.is_empty() {
            b.row(
                "Open With",
                "",
                0,
                false,
                false,
                false,
                false,
                sub::OPEN_WITH,
                None,
            );
        }
        b.sep();
        if is_runnable(path) {
            b.item("Run", Command::RunPath(p.clone()));
        }
        b.item("Open in Terminal", Command::TerminalAt(p.clone()));
        if let Some(parent) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            b.item(
                "Browse Containing Folder",
                Command::FilesSetRoot(parent.display().to_string()),
            );
        }
    }
    b.sep();
    b.item("Copy Path", Command::CopyPathText(p.clone()));
    b.item(reveal_label(), Command::RevealPath(p.clone()));

    // Past the visible rows, so every index above stays what it was — see `Build::extra`.
    for h in handlers {
        b.open_with(
            &h.name,
            Command::OpenPathInApp {
                path: p.clone(),
                app: h.launcher,
            },
        );
    }
    b.finish(CtxKind::File, 0, x, y)
}

/// The menu for a right-clicked URL or commit link. A file link never comes here — it gets
/// [`file_menu`], so a path reads the same whether it was right-clicked on the screen or in
/// the explorer. URLs and commits have no explorer row, so they get the two things a link can
/// do: go there, or take the text. Both run through the same commands a left-click uses.
#[tracing::instrument(level = "debug")]
pub fn link_menu(hit: &avada_terminal_widget::LinkHit, x: f32, y: f32) -> CtxMenu {
    let mut b = Build::new();
    if hit.is_commit {
        b.item(
            "Show Commit",
            Command::ShowCommit {
                cwd: hit.commit_cwd.clone(),
                hash: hit.abs_path.clone(),
            },
        );
        b.sep();
        b.item("Copy Hash", Command::CopyPathText(hit.abs_path.clone()));
    } else {
        b.item("Open Link", Command::OpenLink(hit.abs_path.clone()));
        b.sep();
        b.item("Copy Link", Command::CopyPathText(hit.abs_path.clone()));
    }
    b.finish(CtxKind::File, 0, x, y)
}

/// Whether "Run" is worth offering: the file says how to run itself (a shebang or a kind we
/// know an interpreter for), or the filesystem says it is a program.
#[tracing::instrument(level = "debug", ret)]
fn is_runnable(path: &std::path::Path) -> bool {
    if crate::command::run_prefix(path).is_some() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    }
    #[cfg(not(unix))]
    {
        avada_core::paths::is_executable_ext(&path.to_string_lossy())
    }
}

/// What the OS calls "show me this file in the file manager".
#[tracing::instrument(level = "debug", ret)]
fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(target_os = "windows") {
        "Show in Explorer"
    } else {
        "Show in File Manager"
    }
}

#[tracing::instrument(level = "debug", skip(state))]
pub fn tab_menu(state: &State, idx: usize, x: f32, y: f32) -> CtxMenu {
    let mut b = Build::new();
    let only = state.tabs.len() < 2;
    let is_last = idx + 1 >= state.tabs.len();
    let no_closed = state.closed.is_empty();
    // The system tab (the always-on "Hyperpane") is pinned to this window: `State::detach_tab`
    // refuses it, so the row is greyed rather than offered as a click that does nothing.
    let system = state.tabs.get(idx).is_some_and(|t| t.system);

    let new_sc = state.keymap.label_for("tab.new").unwrap_or_default();

    b.row(
        "New Tab",
        &new_sc,
        0,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::NewTab),
    );
    b.item("Rename…", Command::BeginRename(idx as i32));
    b.item("Duplicate Tab", Command::DuplicateTab(idx));
    b.row(
        "Move to New Window",
        "",
        0,
        false,
        false,
        only || system,
        false,
        sub::NONE,
        Some(Command::MoveTabToNewWindow(idx)),
    );
    b.sep();
    b.item("Close Tab", Command::CloseTab(idx));
    b.row(
        "Close Other Tabs",
        "",
        0,
        false,
        false,
        only,
        false,
        sub::NONE,
        Some(Command::CloseOtherTabs(idx)),
    );
    b.row(
        "Close Tabs to the Right",
        "",
        0,
        false,
        false,
        is_last,
        false,
        sub::NONE,
        Some(Command::CloseTabsToRight(idx)),
    );
    b.row(
        "Reopen Closed",
        "",
        0,
        false,
        false,
        no_closed,
        false,
        sub::NONE,
        Some(Command::ReopenClosedTab),
    );
    b.sep();
    b.row(
        "Layout",
        "",
        0,
        false,
        false,
        false,
        false,
        sub::LAYOUT,
        None,
    );

    b.finish(CtxKind::Tab, idx, x, y)
}

/// Build the application (hamburger) menu, anchored at window-logical `(x, y)`. The native
/// port of the Electron `TopBar` menu: New pane · Command palette (+shortcut) · — · Layout ▸
/// (cascading submenu, radio ✓) · — · Open/Save workspace · — · Preferences. The Layout
/// submenu rows come from the [`crate::theme::LAYOUT_MENU`] model the
/// resync pushes into `ctx_layouts` (with the live checkmark + the Automatic "— <resolved>"
/// hint), exactly like the tab menu's Layout submenu.
#[tracing::instrument(level = "debug", skip(state))]
pub fn app_menu(state: &State, x: f32, y: f32) -> CtxMenu {
    let mut b = Build::new();
    let cur = state.active_tab().layout;
    let palette_sc = state.keymap.label_for("palette.toggle").unwrap_or_default();

    b.row(
        "New pane…",
        "",
        crate::theme::menu_icon::NEW_PANE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::OpenNewPane),
    );
    b.row(
        "Command palette",
        &palette_sc,
        crate::theme::menu_icon::COMMAND_PALETTE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::PaletteOpen),
    );
    // The left panel's third entry point (the top bar's button and Ctrl+B are the other
    // two). Checked, because it toggles a persistent surface rather than firing an action.
    b.row(
        "Left panel",
        &state.keymap.label_for("panel.toggle").unwrap_or_default(),
        crate::theme::menu_icon::LEFT_PANEL,
        state.left_panel_open,
        true,
        false,
        false,
        sub::NONE,
        Some(Command::ToggleLeftPanel),
    );
    b.sep();
    // Layout submenu header: drawn icon + label of the CURRENT layout (the submenu lists
    // Automatic + the 5 presets with the radio ✓ on the current). The current label sits in
    // the shortcut slot (mirrors Electron's "{current.label} ▸").
    b.row(
        "Layout",
        &crate::theme::layout_label(cur),
        crate::theme::layout_icon_kind(cur),
        false,
        false,
        false,
        false,
        sub::LAYOUT,
        None,
    );
    b.sep();
    b.row(
        "Open workspace…",
        "",
        crate::theme::menu_icon::OPEN_WORKSPACE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::OpenWorkspace),
    );
    b.row(
        "Save workspace…",
        "",
        crate::theme::menu_icon::SAVE_WORKSPACE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::SaveWorkspace),
    );
    b.row(
        "Save workspace as…",
        "",
        crate::theme::menu_icon::SAVE_WORKSPACE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::SaveWorkspaceAs),
    );
    b.row(
        "Save to this repo",
        "",
        crate::theme::menu_icon::SAVE_WORKSPACE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::SaveProject),
    );
    b.sep();
    // Workspace SETS (M6): a named collection of workspaces, opened as a batch.
    b.row(
        "Open set…",
        "",
        crate::theme::menu_icon::OPEN_WORKSPACE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::OpenSet),
    );
    b.row(
        "Save set…",
        "",
        crate::theme::menu_icon::SAVE_WORKSPACE,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::SaveSet),
    );
    b.sep();
    b.row(
        "Preferences…",
        "",
        crate::theme::menu_icon::PREFERENCES,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::PrefsOpen),
    );
    b.sep();
    // Relaunching the GUI is how a new build goes live. It is safe to offer here because the
    // panes are not ours to lose: the session daemon owns every PTY and outlives us, so this
    // is a client reconnecting, not a workspace being torn down.
    b.row(
        "Restart Avada",
        "",
        crate::theme::menu_icon::RESTART,
        false,
        false,
        false,
        false,
        sub::NONE,
        Some(Command::RestartApp),
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
#[tracing::instrument(level = "debug", ret)]
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
        let delta = if target > now {
            target - now
        } else {
            target + 86_400 - now
        };
        return Some((delta.div_ceil(60) as u32).max(1));
    }
    // NhM / Nh — hours with optional trailing minutes (the `m` suffix optional there too).
    if let Some((hh, rest)) = s.split_once('h') {
        let h: u32 = hh.parse().ok()?;
        let rest = rest.strip_suffix('m').unwrap_or(rest);
        let m: u32 = if rest.is_empty() {
            0
        } else {
            rest.parse().ok()?
        };
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
#[tracing::instrument(level = "debug", ret)]
pub fn parse_custom_minutes_now(s: &str) -> Option<u32> {
    parse_custom_duration(s, crate::state::local_secs_since_midnight())
}

#[cfg(test)]
mod read_only_menu_tests {
    //! Half the pane menu acts on a pane's SESSION, and a view pane has none (D3). Those
    //! rows are omitted rather than greyed: a row that can never apply to this pane is not
    //! a disabled action, it is a different pane's menu.
    use super::*;
    use crate::state::{DetachedPane, State};
    use avada_core::session_manager::SessionManager;
    use avada_core::tools::PaneKind;

    fn labels(st: &State) -> Vec<String> {
        pane_menu(st, 0, 0.0, 0.0, false)
            .entries
            .iter()
            .map(|e| e.label.to_string())
            .collect()
    }

    fn with_kind(kind: PaneKind) -> State {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = SessionManager::new(tx);
        let mut st = State::new(crate::theme::load_font(1.0));
        st.adopt_pane(
            &mgr,
            DetachedPane {
                uid: "p".into(),
                title: "p".into(),
                subtitle: None,
                pinned_accent: None,
                show_frame: None,
                show_dot: None,
                font_px: 14.0,
                spawn_command: None,
                spawn_args: None,
                spawn_shell: None,
                kind,
                tool_session: None,
                cwd: None,
            },
        );
        st
    }

    /// The session rows are gone on a view pane — and every one of them is present on a
    /// terminal pane, so the test fails if a label is simply renamed out from under it.
    #[test]
    fn a_view_pane_is_offered_nothing_that_needs_a_session() {
        let session_only = [
            "Mute AI Summary",
            "Talk (speak replies)",
            "Search…",
            "Restart",
            "Refresh Env",
            "Paste",
            "Clear",
        ];
        let term = labels(&with_kind(PaneKind::Terminal));
        for row in session_only {
            assert!(term.iter().any(|l| l == row), "{row} exists on a pty pane");
        }
        // Dictation's label flips with the recording state, so it is matched by prefix.
        assert!(
            term.iter().any(|l| l.starts_with("Dictate")),
            "the microphone row exists on a pty pane"
        );

        let view = labels(&with_kind(PaneKind::Markdown));
        for row in session_only {
            assert!(!view.iter().any(|l| l == row), "{row} must be absent");
        }
        assert!(
            !view.iter().any(|l| l.starts_with("Dictate")),
            "a read-only pane has nowhere to type a transcript"
        );
    }

    /// The Restart row has to reach the app tick, not just look like a menu item: the whole
    /// point is that a freshly installed build goes live from here. Follows the row to the
    /// command it carries and dispatches it, so a renamed label or a row wired to `None`
    /// fails here rather than silently doing nothing on click.
    #[test]
    fn the_app_menu_can_restart_the_gui() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = SessionManager::new(tx);
        let mut st = State::new(crate::theme::load_font(1.0));
        let menu = app_menu(&st, 0.0, 0.0);
        let at = menu
            .entries
            .iter()
            .position(|e| e.label == "Restart Avada")
            .expect("the app menu offers a restart");
        let cmd = menu.commands[at]
            .clone()
            .expect("the row carries a command");
        assert!(matches!(
            crate::command::dispatch(&mut st, cmd, &mgr),
            crate::command::Effect::RestartApp
        ));
    }

    /// The rows that survive: a view pane still has content to copy, a cwd to open, and a
    /// pane to rename, colour, move and close.
    #[test]
    fn a_view_pane_keeps_everything_that_is_about_the_pane_itself() {
        let view = labels(&with_kind(PaneKind::Markdown));
        for row in [
            "Copy",
            "Select All",
            "Open Folder",
            "Browse Files",
            "Rename…",
            "Close Pane",
        ] {
            assert!(view.iter().any(|l| l == row), "{row} must survive");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{file_menu, is_runnable, link_menu, parse_custom_duration};
    use crate::command::Command;

    /// The `editorCommand` preference reaches a human through exactly one row, and only
    /// when they set it.
    ///
    /// Worth a test of its own because this setting spent a long release cycle validating,
    /// persisting and round-tripping through the control API while being wired to nothing at
    /// all --- the feature matrix carried it as `dead`. A row that vanishes when the
    /// preference is blank is the whole contract: the default is blank, and a visible "Open
    /// in Editor" that launched nothing would be a worse lie than the silence it replaced.
    #[test]
    fn the_open_in_editor_row_exists_only_when_a_template_is_set() {
        let file = std::env::temp_dir().join("avada-editor-row-test.txt");
        std::fs::write(&file, b"x").expect("write probe file");
        let labels = |st: &crate::state::State| -> Vec<String> {
            file_menu(st, &file, None, 0.0, 0.0)
                .entries
                .iter()
                .map(|e| e.label.to_string())
                .collect()
        };

        let mut st = crate::state::State::new(crate::theme::load_font(1.0));
        assert!(
            !labels(&st).iter().any(|l| l == "Open in Editor"),
            "blank is the default, and it must offer nothing"
        );
        // Blank-but-not-empty is still blank --- the same trim `plan_open` applies, so the
        // row can never promise an open that `open_resolved_path` would decline to perform.
        st.settings.editor_command = "   ".into();
        assert!(!labels(&st).iter().any(|l| l == "Open in Editor"));

        st.settings.editor_command = "subl {path}:{line}:{col}".into();
        let rows = labels(&st);
        let at = rows.iter().position(|l| l == "Open in Editor");
        assert!(at.is_some(), "a set template must offer the row: {rows:?}");
        // Below the discovered editors, above the OS's own list: asked-for beats detected,
        // and neither belongs after the separator where the non-opening verbs live.
        assert!(
            rows[..at.unwrap()].iter().any(|l| l.starts_with("Open in")),
            "the row must sit under the registry editors, not replace them: {rows:?}"
        );

        // The row carries the path and no position, and reads the template at run time
        // rather than baking it in --- see `Command::OpenPathInEditor`.
        let m = file_menu(&st, &file, None, 0.0, 0.0);
        assert!(
            matches!(
                &m.commands[at.unwrap()],
                Some(Command::OpenPathInEditor { path, line: None, col: None })
                    if path == &file.display().to_string()
            ),
            "row must carry the path and no position: {:?}",
            m.commands[at.unwrap()]
        );
        let _ = std::fs::remove_file(&file);
    }

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
        for bad in [
            "", "  ", "0", "0m", "0h", "abc", "1d", "h30", ":30", "25:00", "12:60", "1h60", "-5",
            "1441", "25h",
        ] {
            assert_eq!(
                parse_custom_duration(bad, NOON),
                None,
                "{bad:?} must not parse"
            );
        }
    }

    fn link(is_url: bool, is_commit: bool) -> avada_terminal_widget::LinkHit {
        avada_terminal_widget::LinkHit {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            abs_path: if is_url {
                "https://example.com/x".into()
            } else {
                "0123456789abcdef0123456789abcdef01234567".into()
            },
            line: None,
            col: None,
            tip: String::new(),
            is_url,
            is_commit,
            commit_cwd: "/repo".into(),
            exists: true,
            is_osc8: false,
        }
    }

    #[test]
    fn a_right_clicked_url_can_be_opened_or_copied() {
        let m = link_menu(&link(true, false), 0.0, 0.0);
        let labels: Vec<&str> = m.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["Open Link", "", "Copy Link"]);
        assert!(
            matches!(&m.commands[0], Some(Command::OpenLink(u)) if u == "https://example.com/x")
        );
        assert!(
            matches!(&m.commands[2], Some(Command::CopyPathText(u)) if u == "https://example.com/x")
        );
    }

    #[test]
    fn a_right_clicked_commit_shows_or_copies_the_hash() {
        let m = link_menu(&link(false, true), 0.0, 0.0);
        let labels: Vec<&str> = m.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["Show Commit", "", "Copy Hash"]);
        assert!(matches!(
            &m.commands[0],
            Some(Command::ShowCommit { cwd, hash }) if cwd == "/repo" && hash.starts_with("0123")
        ));
    }

    #[test]
    fn run_is_offered_for_a_kind_we_know_how_to_run() {
        // Nothing here exists on disk: the name alone has to carry the answer, because
        // that is all a clicked path in terminal output gives us.
        for yes in ["a.py", "b.sh", "c.rb", "d.js"] {
            assert!(is_runnable(std::path::Path::new(yes)), "{yes} runs");
        }
        for no in ["notes.md", "data.json", "README", "img.png"] {
            assert!(!is_runnable(std::path::Path::new(no)), "{no} does not");
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_executable_bit_is_enough_on_its_own() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("hp-runnable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("tool");
        std::fs::write(&f, b"binary").unwrap();
        assert!(!is_runnable(&f), "no extension, not executable");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            is_runnable(&f),
            "no extension, but the OS says it is a program"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod module_preset_menu_tests {
    //! A tier-5 surface's choice of keymap dialect is offered in the pane menu it belongs
    //! to, as flat checkable rows.
    use super::*;
    use crate::state::DetachedPane;
    use avada_core::module::grid::{DeclareKeymap, GridAction, KeymapPreset};
    use avada_core::rights::ModuleId;
    use avada_core::session_manager::SessionManager;
    use avada_core::tools::kind::ModulePaneRef;
    use avada_core::tools::PaneKind;

    const MODULE: &str = "bshuler/avada-editor";

    fn id() -> ModuleId {
        ModuleId::new(MODULE).unwrap()
    }

    fn with_kind(kind: PaneKind) -> State {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mgr = SessionManager::new(tx);
        let mut st = State::new(crate::theme::load_font(1.0));
        st.adopt_pane(
            &mgr,
            DetachedPane {
                uid: "p".into(),
                title: "p".into(),
                subtitle: None,
                pinned_accent: None,
                show_frame: None,
                show_dot: None,
                font_px: 14.0,
                spawn_command: None,
                spawn_args: None,
                spawn_shell: None,
                kind,
                tool_session: None,
                cwd: None,
            },
        );
        st
    }

    fn module_pane(surface: &str) -> PaneKind {
        PaneKind::Module(ModulePaneRef::new(MODULE, surface, None).expect("a valid pane ref"))
    }

    fn keymap(surface: &str) -> DeclareKeymap {
        DeclareKeymap {
            surface: surface.into(),
            actions: vec![GridAction {
                id: "move.left".into(),
                label: "Move left".into(),
            }],
            presets: vec![
                KeymapPreset {
                    name: "helix".into(),
                    label: "Helix".into(),
                    bindings: [("h".to_string(), "move.left".to_string())]
                        .into_iter()
                        .collect(),
                },
                KeymapPreset {
                    name: "vscode".into(),
                    label: "VS Code".into(),
                    bindings: [("arrowleft".to_string(), "move.left".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
            default_preset: Some("helix".into()),
        }
    }

    /// What a preset row carries, flattened so it can be compared: its label, whether it
    /// is ticked, and the choice its command would make. `Command` is not `PartialEq`, so
    /// the payload is unwrapped here rather than the enum compared whole.
    type Row = (String, bool, Option<String>);

    fn preset_rows(st: &State) -> Vec<Row> {
        let menu = pane_menu(st, 0, 0.0, 0.0, false);
        menu.entries
            .iter()
            .zip(menu.commands.iter())
            .filter_map(|(e, c)| match c {
                Some(Command::SetModulePreset(m, s, preset)) => {
                    assert_eq!((m.as_str(), s.as_str()), (MODULE, "editor"));
                    Some((e.label.to_string(), e.checked, preset.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// The rows exist, exactly one is ticked, and the row for the module's *own* default
    /// clears the choice rather than pinning today's spelling of it.
    #[test]
    fn the_default_row_clears_the_choice_and_the_others_name_themselves() {
        crate::module_ui::grid::set_keymap(&id(), keymap("editor"));
        let st = with_kind(module_pane("editor"));

        assert_eq!(
            preset_rows(&st),
            vec![
                ("Helix".to_string(), true, None),
                ("VS Code".to_string(), false, Some("vscode".to_string())),
            ]
        );

        // Choosing the other dialect moves the tick and nothing else.
        crate::module_ui::grid::set_preset(&id(), "editor", Some("vscode"));
        let ticked: Vec<(String, bool)> = preset_rows(&st)
            .into_iter()
            .map(|(l, c, _)| (l, c))
            .collect();
        assert_eq!(
            ticked,
            vec![("Helix".to_string(), false), ("VS Code".to_string(), true)]
        );
        crate::module_ui::grid::forget(&id());
    }

    /// A surface that has declared no keymap — and any pane that is not a module at all —
    /// gets no rows and no stray separator.
    #[test]
    fn a_pane_with_no_declared_presets_grows_no_rows() {
        crate::module_ui::grid::forget(&id());
        let bare = with_kind(module_pane("editor"));
        assert!(
            preset_rows(&bare).is_empty(),
            "nothing declared, nothing offered"
        );
        let before = pane_menu(&bare, 0, 0.0, 0.0, false).entries.len();

        // The declaration is per surface: a keymap for `diff` says nothing about `editor`.
        crate::module_ui::grid::set_keymap(&id(), keymap("diff"));
        assert!(
            preset_rows(&bare).is_empty(),
            "a sibling surface's dialect is not this one's"
        );
        assert_eq!(
            pane_menu(&bare, 0, 0.0, 0.0, false).entries.len(),
            before,
            "no rows means no separator either"
        );

        // And a terminal pane is never asked about dialects, keymap or no keymap.
        crate::module_ui::grid::set_keymap(&id(), keymap("editor"));
        assert!(preset_rows(&with_kind(PaneKind::Terminal)).is_empty());
        assert!(preset_rows(&with_kind(PaneKind::Markdown)).is_empty());
        crate::module_ui::grid::forget(&id());
    }
}
