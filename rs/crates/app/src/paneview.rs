//! The view bridge: turns the central [`State`] into the Slint models and drives
//! the per-frame pump. This is the *resync* half of the Seam #1 contract — it
//! only ever reads `State` (plus the relayout it performs on the active tab) and
//! writes the UI models; it never decides workspace policy.

use std::time::{Duration, Instant};

use avada_core::layout::presets::{
    compute_tiles, effective_layout, DividerKind, Layout, Orientation,
};
use avada_core::session_manager::SessionManager;
use avada_core::tools::kind::ModulePaneRef;
use avada_core::tools::PaneKind;
use avada_terminal_widget::{cells_for_px, RenderOpts};

use slint::{Color, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::contextmenu::CtxKind;
use crate::prefs;
use crate::state::{Overlay, PaneState, State};
use crate::theme;
use crate::{
    AppWindow, ClaudeSessionItem, CtxTab, DividerItem, FramePaletteOption, HiRect, KeybindingItem,
    LayoutOption, LeftPaneRow, LeftPanelAdapter, LeftSessionRow, LeftTabRow, MenuEntry,
    PaletteItem, PaneItem, PaneViewRow, PrefBrowserRow, PrefOption, PrefToolRow, ProjectItem,
    TabItem, WorktreeRow,
};

/// Thickness (logical px) of the draggable divider hit-area.
const DIVIDER_THICK: f32 = 10.0;

/// Characters revealed per pump tick for the ambient-AI subtitle typewriter (~8 ms/tick, so
/// ≈75 chars/sec — a brisk but legible reveal).
const AI_REVEAL_PER_TICK: f32 = 0.6;

/// Per-pane inset (logical px) within its tile — matches the Electron app's
/// `.hp-pane { inset: 3px }`, giving 6px gaps between panes + a 3px edge margin.
const PANE_GAP: f32 = 3.0;

/// Space the pane frame takes from the terminal body so the grid matches the
/// displayed area: 1px borders + the 26px header + the body's 2/0/2/4 padding.
const PANE_CHROME_W: f32 = 6.0; // 2px borders + 4px left pad
const PANE_CHROME_H: f32 = 32.0; // 2px borders + 26px header + 4px top/bottom pad

/// The Slint models a single window owns and its resync writes into. Each OS window has
/// its own `Ui` (its own model set), so windows are fully independent.
pub struct Ui {
    pub panes: Rc<VecModel<PaneItem>>,
    pub tabs: Rc<VecModel<TabItem>>,
    pub dividers: Rc<VecModel<DividerItem>>,
    pub layouts: Rc<VecModel<LayoutOption>>,
    // ---- Wave-2 overlay models ----
    pub palette: Rc<VecModel<PaletteItem>>,
    pub projects: Rc<VecModel<ProjectItem>>,
    /// New-goal dialog: filenames of images attached to the in-progress goal.
    pub goal_images: Rc<VecModel<slint::SharedString>>,
    /// New-goal box: the focused field's ↓-option rows (history / projects / model tiers).
    pub goal_menu: Rc<VecModel<PaletteItem>>,
    /// New-goal box: the four chip labels (project + orch/spec/impl models).
    pub goal_chips: Rc<VecModel<slint::SharedString>>,
    pub families: Rc<VecModel<PrefOption>>,
    pub palettes: Rc<VecModel<FramePaletteOption>>,
    /// Shell-palette picker rows. Reuses `FramePaletteOption` because the row is the same
    /// shape (label + swatches + active); the swatches here are four representative tokens
    /// rather than the eight pane hues.
    pub ui_palettes: Rc<VecModel<FramePaletteOption>>,
    /// Which shell palette the Slint `Theme` global currently holds. The tokens are pushed
    /// one property at a time, so re-pushing them every frame would be pure waste — and
    /// `usize::MAX` is a value no index can equal, which makes the first sync a guaranteed
    /// push without a separate "have we ever pushed" flag.
    pub ui_palette_pushed: std::cell::Cell<usize>,
    pub shells: Rc<VecModel<PrefOption>>,
    pub themes: Rc<VecModel<PrefOption>>,
    pub idle_effects: Rc<VecModel<PrefOption>>,
    pub keybindings: Rc<VecModel<KeybindingItem>>,
    // ---- New Pane dialog models ----
    pub np_swatches: Rc<VecModel<Color>>,
    pub np_shells: Rc<VecModel<PrefOption>>,
    // ---- context-menu models ----
    pub ctx_entries: Rc<VecModel<MenuEntry>>,
    pub ctx_swatches: Rc<VecModel<Color>>,
    pub ctx_tabs: Rc<VecModel<CtxTab>>,
    pub ctx_openwith: Rc<VecModel<SharedString>>,
    pub ctx_layouts: Rc<VecModel<LayoutOption>>,
    // ---- sidebar worktree subtrees ----
    /// Per-project worktree models, keyed by repo path and reused across ticks so each
    /// `ProjectItem.worktrees` keeps a STABLE model identity. Without this the projection's
    /// per-tick rebuild would recreate the worktree rows every frame, dropping in-flight
    /// clicks on the trash icons. Pruned to the live project set each resync.
    pub wt_models: RefCell<HashMap<String, Rc<VecModel<WorktreeRow>>>>,
    /// Per-project Claude-session models, keyed by repo path — the same stable-identity reuse
    /// as `wt_models` (the resume-row repeater must not be rebuilt each frame, or an in-flight
    /// click on a session row would be dropped). Pruned to the live project set each resync.
    pub claude_models: RefCell<HashMap<String, Rc<VecModel<ClaudeSessionItem>>>>,
    // ---- the left slide-out panel's three sections (mux plan M5) ----
    /// The workspace tree's tab rows (each carrying its own pane rows).
    pub lp_tabs: Rc<VecModel<LeftTabRow>>,
    /// The detached (adoptable) session rows.
    pub lp_detached: Rc<VecModel<LeftSessionRow>>,
    /// Per-tab pane models for the tree, keyed by tab index and reused across ticks so each
    /// `LeftTabRow.panes` keeps a STABLE model identity — the same reason `wt_models` exists:
    /// rebuilding the inner repeater every frame would drop an in-flight click or, worse, the
    /// pointer grab of a pane drag mid-flight. Pruned to the live tab count each resync.
    pub lp_pane_models: RefCell<HashMap<usize, Rc<VecModel<LeftPaneRow>>>>,
    /// Preferences → Tools: every registry tool, favourited or not.
    pub pref_tools: Rc<VecModel<PrefToolRow>>,
    /// Preferences → Browser: the browsers this OS reports as installed.
    pub pref_browsers: Rc<VecModel<PrefBrowserRow>>,
    /// The "Open link with…" chooser's rows — a *snapshot* taken when the overlay opened
    /// (`State::ask_browsers`), not the prefs list, so a scan finishing mid-choice can't
    /// renumber the row under the cursor.
    pub ask_browsers: Rc<VecModel<PrefBrowserRow>>,
    /// Where `PATH` (and the well-known dirs) found each tool, scanned ONCE and kept.
    /// Deliberately computed with an EMPTY override map, so it is the answer to "what
    /// would we find on our own" and never changes when the user edits an override —
    /// which is exactly what makes it safe to cache for the life of the window. `None`
    /// means "not scanned yet"; the scan happens the first time Preferences opens, so a
    /// session that never opens it never touches the filesystem for this.
    pub tool_detected: RefCell<Option<HashMap<&'static str, String>>>,
    /// Same one-shot treatment for the installed-browser list.
    pub browsers_found: RefCell<Option<Vec<avada_core::open::BrowserApp>>>,
}

impl Ui {
    /// A fresh, empty model set for one window.
    #[tracing::instrument(level = "debug")]
    pub fn new() -> Rc<Ui> {
        Rc::new(Ui {
            panes: Rc::new(VecModel::default()),
            tabs: Rc::new(VecModel::default()),
            dividers: Rc::new(VecModel::default()),
            layouts: Rc::new(VecModel::default()),
            palette: Rc::new(VecModel::default()),
            projects: Rc::new(VecModel::default()),
            goal_images: Rc::new(VecModel::default()),
            goal_menu: Rc::new(VecModel::default()),
            goal_chips: Rc::new(VecModel::default()),
            families: Rc::new(VecModel::default()),
            palettes: Rc::new(VecModel::default()),
            ui_palettes: Rc::new(VecModel::default()),
            ui_palette_pushed: std::cell::Cell::new(usize::MAX),
            shells: Rc::new(VecModel::default()),
            themes: Rc::new(VecModel::default()),
            idle_effects: Rc::new(VecModel::default()),
            keybindings: Rc::new(VecModel::default()),
            np_swatches: Rc::new(VecModel::default()),
            np_shells: Rc::new(VecModel::default()),
            ctx_entries: Rc::new(VecModel::default()),
            ctx_swatches: Rc::new(VecModel::default()),
            ctx_tabs: Rc::new(VecModel::default()),
            ctx_openwith: Rc::new(VecModel::default()),
            ctx_layouts: Rc::new(VecModel::default()),
            wt_models: RefCell::new(HashMap::new()),
            claude_models: RefCell::new(HashMap::new()),
            lp_tabs: Rc::new(VecModel::default()),
            lp_detached: Rc::new(VecModel::default()),
            lp_pane_models: RefCell::new(HashMap::new()),
            pref_tools: Rc::new(VecModel::default()),
            pref_browsers: Rc::new(VecModel::default()),
            ask_browsers: Rc::new(VecModel::default()),
            tool_detected: RefCell::new(None),
            browsers_found: RefCell::new(None),
        })
    }

    /// Bind this window's models to its `AppWindow` instance.
    #[tracing::instrument(level = "debug", ret, skip(self, app))]
    pub fn attach(&self, app: &AppWindow) {
        app.set_panes(ModelRc::from(self.panes.clone()));
        // Track V3: the image pane's adapter (row model, checker tile, fit callback).
        crate::imagepane::attach(app);
        // Track H4: the tier-5 module grid pane's frame + run models.
        crate::gridpane::attach(app);
        app.set_tabs(ModelRc::from(self.tabs.clone()));
        app.set_dividers(ModelRc::from(self.dividers.clone()));
        app.set_layouts(ModelRc::from(self.layouts.clone()));
        app.set_palette(ModelRc::from(self.palette.clone()));
        app.set_projects(ModelRc::from(self.projects.clone()));
        app.set_goal_images(ModelRc::from(self.goal_images.clone()));
        app.set_goal_menu(ModelRc::from(self.goal_menu.clone()));
        app.set_goal_chips(ModelRc::from(self.goal_chips.clone()));
        app.set_pref_families(ModelRc::from(self.families.clone()));
        app.set_pref_palettes(ModelRc::from(self.palettes.clone()));
        app.set_pref_ui_palettes(ModelRc::from(self.ui_palettes.clone()));
        app.set_pref_shells(ModelRc::from(self.shells.clone()));
        app.set_pref_tools(ModelRc::from(self.pref_tools.clone()));
        app.set_pref_browsers(ModelRc::from(self.pref_browsers.clone()));
        app.set_ask_browsers(ModelRc::from(self.ask_browsers.clone()));
        app.set_pref_themes(ModelRc::from(self.themes.clone()));
        app.set_pref_idle_effects(ModelRc::from(self.idle_effects.clone()));
        app.set_pref_keybindings(ModelRc::from(self.keybindings.clone()));
        app.set_np_swatches(ModelRc::from(self.np_swatches.clone()));
        app.set_np_shells(ModelRc::from(self.np_shells.clone()));
        app.set_ctx_entries(ModelRc::from(self.ctx_entries.clone()));
        app.set_ctx_swatches(ModelRc::from(self.ctx_swatches.clone()));
        app.set_ctx_tabs(ModelRc::from(self.ctx_tabs.clone()));
        app.set_ctx_openwith(ModelRc::from(self.ctx_openwith.clone()));
        app.set_ctx_layouts(ModelRc::from(self.ctx_layouts.clone()));
        // The left panel is wired through a global (like RemindersAdapter) rather than new
        // AppWindow properties, so the whole feature stays self-contained.
        use slint::ComponentHandle as _;
        let lp = app.global::<LeftPanelAdapter>();
        lp.set_tabs(ModelRc::from(self.lp_tabs.clone()));
        lp.set_detached(ModelRc::from(self.lp_detached.clone()));
    }
}

/// Copy a shell palette into the Slint `Theme` global.
///
/// Only the fourteen *base* tokens are written. The aliases at the bottom of
/// `ui/theme.slint` (`brand`, `ink`, `hover`, …) are plain `out` properties bound to the
/// bases, so they re-derive themselves the instant a base changes — setting them here would
/// need a setter per alias and would silently rot the moment someone adds a fifteenth.
///
/// `from_argb_encoded` rather than `from_rgb_u8`: `scrim` and `veil` are translucent, and
/// the alpha is part of the palette (a light theme wants a lighter shadow).
#[tracing::instrument(level = "debug", ret, skip(app))]
fn push_ui_palette(app: &AppWindow, p: crate::theme::UiPalette) {
    use slint::ComponentHandle as _;
    let t = app.global::<crate::Theme>();
    let c = slint::Color::from_argb_encoded;
    t.set_bg(c(p.bg));
    t.set_mantle(c(p.mantle));
    t.set_surface(c(p.surface));
    t.set_surface2(c(p.surface2));
    t.set_border(c(p.border));
    t.set_text(c(p.text));
    t.set_subtext(c(p.subtext));
    t.set_faint(c(p.faint));
    t.set_accent(c(p.accent));
    t.set_danger(c(p.danger));
    t.set_ok(c(p.ok));
    t.set_warn(c(p.warn));
    t.set_link(c(p.link));
    t.set_scrim(c(p.scrim));
    t.set_veil(c(p.veil));
}

/// Push `items` into `model`, reusing the existing elements when the row count is
/// unchanged (`set_row_data`) and only rebuilding (`set_vec`) when it differs.
/// Reuse is essential: `set_vec` destroys + recreates the repeated Slint elements,
/// which would drop a divider's pointer grab mid-drag and reset pane focus.
/// A fresh `ModelRc` from a plain `Vec`, for a list that is rebuilt wholesale rather than
/// diffed into a live `VecModel`. The `changed`-callback hazard [`sync_model`] exists to
/// avoid does not apply to a list of pure rows with no `changed` handler on them.
fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

#[tracing::instrument(level = "debug", skip_all)]
fn sync_model<T: Clone + 'static>(model: &VecModel<T>, items: Vec<T>) {
    // Grow/shrink at the tail and update the rest in place, rather than `set_vec`. `set_vec`
    // replaces the whole model, which makes a Repeater DESTROY and re-create every element —
    // and a freshly created element never fires a `changed` callback for its initial property
    // value. That silently swallowed the keyboard hand-off (`refocus-seq`) on every tab switch
    // where the pane count differed, and on the first pane of every launch. Incremental row
    // edits keep the elements alive, so their `changed` handlers keep working.
    while model.row_count() > items.len() {
        model.remove(model.row_count() - 1);
    }
    let have = model.row_count();
    for (i, it) in items.into_iter().enumerate() {
        if i < have {
            model.set_row_data(i, it);
        } else {
            model.push(it);
        }
    }
}

/// Re-apply the mouse cursor after a hovered-link flip (see the call site in [`resync`]).
/// Slint samples a TouchArea's `mouse-cursor` only while dispatching a mouse event, so the
/// pointer/text switch otherwise waits for the next real OS mouse move — leaving the cursor
/// stale when the mouse comes to rest exactly as a link appears under it. Re-posting the move
/// through the OS doesn't work (a same-point `SetCursorPos` — even a 1px out-and-back, which
/// Windows coalesces — never reaches winit as a move), and a synthetic Slint `PointerMoved`
/// re-sampled unreliably. So set the Win32 cursor directly from the link state the resync
/// just wrote — `link_active` — which is authoritative. winit may re-assert its stored icon
/// on a later `WM_SETCURSOR`, but every real mouse move re-samples through Slint and
/// converges (verified live: one 1px move corrects it). No-op when the cursor is outside the
/// window.
#[cfg(windows)]
#[tracing::instrument(level = "debug", ret, skip(app))]
fn replay_cursor_pos(app: &AppWindow, link_active: bool) {
    use slint::ComponentHandle;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, LoadCursorW, SetCursor, IDC_HAND, IDC_IBEAM,
    };
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) }.is_err() {
        return;
    }
    let win = app.window();
    let pos = win.position();
    let size = win.size();
    let (dx, dy) = (p.x - pos.x, p.y - pos.y);
    if dx < 0 || dy < 0 || dx as u32 >= size.width || dy as u32 >= size.height {
        return;
    }
    unsafe {
        let idc = if link_active { IDC_HAND } else { IDC_IBEAM };
        if let Ok(cur) = LoadCursorW(None, idc) {
            SetCursor(Some(cur));
        }
    }
}

#[cfg(not(windows))]
#[tracing::instrument(level = "debug", ret, skip(_app))]
fn replay_cursor_pos(_app: &AppWindow, _link_active: bool) {}

/// Build a model row for pane `i`. `editing` flags the pane whose label is being renamed
/// inline; `show_frame`/`show_dot` are the GLOBAL Appearance prefs, folded here over each
/// pane's per-pane override (a clean new pane resolves OFF, a git-project pane ON).
// Nine arguments, deliberately: every one is a value resolved BEFORE the `&mut state.tabs`
// borrow in the caller, so folding them into a struct would only move the same nine reads
// one line up. Kept flat until the projection itself grows a type.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "debug", ret, skip(ps))]
/// The pane's cell size in *logical* px, recovered from the layout pass's own two answers
/// rather than re-derived from the font and the scale factor.
///
/// `surf` is `applied` multiplied by the device cell size and divided by the scale, so
/// dividing it back is the one arithmetic that cannot disagree with what `fit_grid` used —
/// and disagreeing by a fraction of a pixel is what makes a grid drift a column off its
/// cursor at the right-hand edge.
#[tracing::instrument(level = "debug", ret, skip(ps))]
fn cell_px(ps: &PaneState) -> (f32, f32) {
    let (w, h) = ps.surf;
    let (cols, rows) = ps.applied;
    match (cols, rows) {
        (0, _) | (_, 0) => (0.0, 0.0),
        (c, r) => (w / c as f32, h / r as f32),
    }
}

// Every argument is one axis of how a pane is drawn; bundling them into a struct would
// only move the same list one call site up.
#[allow(clippy::too_many_arguments)]
fn pane_item(
    ps: &PaneState,
    focused: bool,
    editing: bool,
    show_frame: bool,
    show_dot: bool,
    font_px: f32,
    kind: &PaneKind,
    live: i32,
    palette: usize,
) -> PaneItem {
    let (x, y, w, h) = ps.rect;
    // Project the clickable-path hover overlay (if any) into the model row.
    let (lx, ly) = ps.link_cursor;
    let link = ps.link.as_ref();
    // Drag-to-select highlight rects, hit-tested against the pane's on-screen surface size.
    let (sw, sh) = ps.surf;
    let selection_rects: Vec<HiRect> = ps
        .pane
        .selection_rects(sw, sh)
        .into_iter()
        .map(|(x, y, w, h)| HiRect { x, y, w, h })
        .collect();
    // Search-match highlights for the viewport: every visible match dimmed, the active one drawn
    // distinctly. Hit-tested against the same on-screen surface as the selection rects.
    let (search_rect_v, search_active) = ps.pane.search_view_rects(sw, sh);
    let search_rects: Vec<HiRect> = search_rect_v
        .into_iter()
        .map(|(x, y, w, h)| HiRect { x, y, w, h })
        .collect();
    let (search_active_on, search_active_rect) = match search_active {
        Some((x, y, w, h)) => (true, HiRect { x, y, w, h }),
        None => (
            false,
            HiRect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
        ),
    };
    // Vim-style scrollbar (right edge): thumb geometry + show-then-fade opacity, hit-tested against
    // the pane's on-screen surface height. `None` while idle/faded or with no scrollback → opacity 0.
    let (scrollbar_opacity, scrollbar_thumb_y, scrollbar_thumb_h) = match ps.pane.scrollbar(sh) {
        Some((y, h, op)) => (op, y, h),
        None => (0.0, 0.0, 0.0),
    };
    // Jump-to-bottom HUD: shown whenever the viewport is scrolled up off the live edge.
    let scroll_offset = ps.pane.scroll_offset() as i32;
    // Whether the running app grabbed the mouse → forward pointer events to it (Claude/vim).
    let app_grabs_mouse = ps.pane.app_grabs_mouse();
    // In-pane search box state (opened from the pane menu's "Search…").
    let search_open = ps.pane.search_is_open();
    let (cur, total) = ps.pane.search_count();
    let search_count: SharedString = if !search_open || total == 0 {
        SharedString::new()
    } else {
        format!("{cur} / {total}").into()
    };
    // Ambient-AI line: the typewriter-revealed prefix of the engine's summary, shown only
    // when there's no manual subtitle (which always wins) and the pane isn't muted.
    // Family B (non-PTY) panes: the row projection for the file browser / viewer /
    // markdown preview. Gated on the kind rather than computed unconditionally
    // because `model_for` stats the target, and this runs once per pane per tick —
    // a terminal pane must not pay a syscall for a view it does not have.
    // `is_view` is the kind's own answer now, not a list repeated here: the ranged
    // check this replaced (`kind >= 2 && kind <= 4` in the .slint) already had Browser
    // on the wrong side of it, and a fourth view kind would have joined it there.
    let is_view = kind.is_view();
    // Track H4: a tier-5 module surface paints cells, not rows, so it goes through
    // `gridpane`'s own models. Done *beside* the row projection below rather than instead of
    // it, because a module pane's tier is not knowable from its kind — the same pane is a
    // rows pane until the module sends its first frame.
    // Track V3 × H4: a tier-5 module can ship a *picture* as well as cells or a document. An
    // image is neither rows nor a grid — it is one texture with its own geometry — so a module
    // surface that has shipped at least one picture goes through `imagepane`'s per-uid model,
    // exactly as the built-in `PaneKind::Image` does, and skips the grid/placeholder step. The
    // store's `generation` is the discriminator: a module pane is a grid pane until its first
    // picture, the same way it is a rows pane until its first frame.
    let mut has_image = matches!(kind, PaneKind::Image);
    if let PaneKind::Module(m) = kind {
        if crate::module_ui::image::generation(&m.id, &m.surface) > 0 {
            has_image = true;
            crate::imagepane::project_module(&ps.uid, &m.id, &m.surface);
        } else {
            let (cw, ch) = cell_px(ps);
            crate::gridpane::project(&ps.uid, m, cw, ch, font_px);
        }
    }
    let (view_rows, view_title): (ModelRc<PaneViewRow>, SharedString) =
        if matches!(kind, PaneKind::Image) {
            // Track V3: the picture goes through `imagepane`'s own per-uid model (a texture,
            // not rows), so the `ViewPane` this pane still instantiates gets nothing to list.
            crate::imagepane::project(&ps.uid, ps.cwd.as_deref());
            (
                ModelRc::from(Rc::new(VecModel::from(Vec::<PaneViewRow>::new()))),
                crate::viewpane::view_title(kind, ps.cwd.as_deref()).into(),
            )
        } else if is_view {
            (
                crate::viewpane::model_for(&ps.uid, kind, ps.cwd.as_deref(), palette),
                crate::viewpane::view_title(kind, ps.cwd.as_deref()).into(),
            )
        } else {
            (
                ModelRc::from(Rc::new(VecModel::from(Vec::<PaneViewRow>::new()))),
                SharedString::new(),
            )
        };
    // Only meaningful for a view pane; -1/-1 everywhere else says "nothing selected".
    let (view_sel_lo, view_sel_hi) = match is_view
        .then(|| crate::viewpane::selected_range(&ps.uid))
        .flatten()
    {
        Some((lo, hi)) => (lo as i32, hi as i32),
        None => (-1, -1),
    };
    let manual_subtitle = ps.subtitle.as_ref().is_some_and(|s| !s.is_empty());
    let ai_subtitle: SharedString = if manual_subtitle || ps.ai_muted || ps.ai.full.is_empty() {
        SharedString::new()
    } else {
        let shown = (ps.ai.reveal as usize).min(ps.ai.len);
        ps.ai.full.chars().take(shown).collect::<String>().into()
    };
    PaneItem {
        surface: ps.surface.clone(),
        title: ps.title.clone(),
        subtitle: ps.subtitle.clone().unwrap_or_default(),
        ai_subtitle,
        // The cached shell-type badge (computed once at pane creation; "" → not shown).
        shell_type: ps.shell_label.as_str().into(),
        // Speaker glyph in the header, shown only while this pane's "talk" is on.
        talk: ps.talk,
        // Lights the header microphone while this pane's dictation recorder is live.
        recording: ps.recording,
        show_frame: ps.frame_on(show_frame),
        show_dot: ps.dot_on(show_dot),
        editing,
        accent: ps.accent,
        uid: ps.uid.as_str().into(),
        x,
        y,
        w,
        h,
        visible: ps.visible,
        focused,
        glow: ps.glow.alpha,
        link_visible: link.is_some(),
        // An OSC 8 link opens on a PLAIN click even under a mouse grab (see `widget.slint`).
        link_declared: link.is_some_and(|l| l.is_osc8),
        link_x: link.map(|l| l.x).unwrap_or(0.0),
        link_y: link.map(|l| l.y).unwrap_or(0.0),
        link_w: link.map(|l| l.w).unwrap_or(0.0),
        link_tip: link.map(|l| l.tip.clone()).unwrap_or_default().into(),
        link_tip_x: lx + 12.0,
        link_tip_y: ly + 16.0,
        selection_rects: ModelRc::from(Rc::new(VecModel::from(selection_rects))),
        search_open,
        search_count,
        search_focus_seq: ps.search_focus_seq,
        refocus_seq: ps.refocus_seq,
        search_rects: ModelRc::from(Rc::new(VecModel::from(search_rects))),
        search_active_on,
        search_active_rect,
        toast: ps.last_toast.clone().into(),
        // The live terminal font px (logical) — drives the widget's indicator scaling.
        font_px,
        scrollbar_opacity,
        scrollbar_thumb_y,
        scrollbar_thumb_h,
        scroll_offset,
        app_grabs_mouse,
        // The native app drops a pane the moment its session exits, so a live pane is never
        // "exited"; the field exists for the taskbar's Electron-parity badge.
        exited: false,
        // ---- tool identity ----
        // What this pane is RUNNING, projected for the header. `kind` is the MERGED
        // identity from `State::effective_kind` (the pane's own kind, or the OSC-title
        // sniff when it is a plain terminal) — this projection is the one place the two
        // are combined, which is what keeps a sniff out of what a relaunch replays.
        // `ui_icon`/`ui_name` return 0/"" for a plain terminal and for a tool id this
        // build does not know, so an unknown kind shows no mark rather than a wrong one.
        kind: kind.ui_kind(),
        is_view,
        // A texture pane: the built-in image view, or a module surface that shipped a picture.
        // Drives the `.slint` to draw `ImagePane` over the (empty) `ViewPane`/placeholder.
        has_image,
        tool_icon: kind.ui_icon(),
        tool_name: kind.ui_name().into(),
        agent_live: live,
        tool_brand: match kind.tool() {
            Some(t) => slint::Color::from_rgb_u8(t.brand.0, t.brand.1, t.brand.2),
            // No brand: fall back to the pane's own accent so the mark is never invisible.
            None => ps.accent,
        },
        view_rows,
        view_title,
        view_sel_lo,
        view_sel_hi,
    }
}

/// Inset a layout tile by the inter-pane gap → the pane box that is actually drawn.
fn pane_box(x: f32, y: f32, w: f32, h: f32) -> (f32, f32, f32, f32) {
    (
        x + PANE_GAP,
        y + PANE_GAP,
        (w - 2.0 * PANE_GAP).max(1.0),
        (h - 2.0 * PANE_GAP).max(1.0),
    )
}

/// Size `p`'s grid to what a pane box of `gw`×`gh` logical px can hold, restarting the pty
/// debounce clock if that moved. Returns the grid, for callers that also need to derive the
/// hit-test surface from it.
///
/// Pixel geometry (`rect`, `surf`, `visible`) stays the CALLER's business on purpose: a
/// background tab has to size its grids without claiming any of the drawn area.
fn fit_grid(p: &mut PaneState, gw: f32, gh: f32, scale: f32, now: Instant) -> (usize, usize) {
    // size the grid to the terminal body (frame chrome removed) so cells match. Each pane
    // uses its OWN font cell metrics (per-pane zoom), so panes can differ in cols/rows.
    let tw = (gw - PANE_CHROME_W).max(1.0);
    let th = (gh - PANE_CHROME_H).max(1.0);
    let (cols, rows) = cells_for_px(tw * scale, th * scale, p.font.cell_w, p.font.cell_h);
    if (cols, rows) != p.applied {
        p.pane.resize(cols, rows);
        p.applied = (cols, rows);
        // Restart the debounce clock: the pty is told only once this has stopped moving
        // (see `flush_pty_resizes`). The LOCAL grid reflows right now, which is lossless.
        p.pty_since = now;
        // The grid rewrapped — recompute any open search so its highlights track the
        // reflowed text instead of drifting against stale match coordinates.
        p.pane.search_reflow();
    }
    (cols, rows)
}

/// Recompute the active tab's pane rects (and reflow any pane whose pixel size
/// changed). Honors zoom (solo the zoomed pane full-area).
#[tracing::instrument(level = "debug", ret, skip(state))]
fn relayout_active(state: &mut State, area: (f32, f32), scale: f32) {
    let (aw, ah) = area;
    let active = state.active;
    // Fullscreen solos the focused pane (OS fullscreen + bars hidden in app.slint), like
    // Electron's `fullscreenPaneId`: one pane fills the screen, the rest go invisible.
    let fullscreen = state.fullscreen;
    let tab = &mut state.tabs[active];
    let n = tab.panes.len();
    if n == 0 {
        return;
    }
    for p in tab.panes.iter_mut() {
        p.visible = false;
    }

    let now = Instant::now();
    let place = |p: &mut PaneState, x: f32, y: f32, w: f32, h: f32| {
        let (gx, gy, gw, gh) = pane_box(x, y, w, h);
        p.rect = (gx, gy, gw, gh);
        p.visible = true;
        let (cols, rows) = fit_grid(p, gw, gh, scale, now);
        // The selection / link / search-highlight hit-test surface, set authoritatively here
        // every tick. Slint's `geometry-changed` is unreliable: it doesn't fire for a pane
        // created already at its final size (a freshly *launched* pane stays at surf (0,0), which
        // breaks hit-testing), and when adding a pane reflows its neighbours their surf can go
        // stale (highlights then drift against the old size). The pump always knows the exact
        // size, so we own it here.
        //
        // This is the size of the RENDERED IMAGE, not of the widget it sits in, and the two are
        // not the same: `cols`/`rows` are FLOORED from the body, and widget.slint pins the
        // surface at `source.width x source.height` physical px (1:1, no `image-fit: fill`), so
        // the drawn image is up to a cell narrower and a row shorter than the body it sits in.
        // Every consumer divides this by cols/rows to recover the cell size, so handing over the
        // body instead made every cell fractionally too big — an error that accumulates with the
        // column index, putting the pointer's idea of the cell under it up to a full cell to the
        // left of the glyph the human is aiming at by the right-hand edge of a pane. Deriving it
        // from the same cols/rows the renderer used cannot drift by construction.
        p.surf = (
            (cols as f32 * p.font.cell_w as f32 / scale).max(1.0),
            (rows as f32 * p.font.cell_h as f32 / scale).max(1.0),
        );
    };

    // Fullscreen wins over zoom: solo the focused pane, full-area.
    if fullscreen {
        let f = tab.focused.min(n - 1);
        place(&mut tab.panes[f], 0.0, 0.0, aw, ah);
        return;
    }

    if let Some(z) = tab.zoomed {
        if z < n {
            place(&mut tab.panes[z], 0.0, 0.0, aw, ah);
        }
        return;
    }

    let eff = effective_layout(tab.layout, n);
    let tiles = compute_tiles(eff, n, &tab.sizes, tab.main_fraction, tab.focused as i32);
    for t in &tiles {
        let x = (t.rect.x * aw as f64) as f32;
        let y = (t.rect.y * ah as f64) as f32;
        let w = (t.rect.w * aw as f64) as f32;
        let h = (t.rect.h * ah as f64) as f32;
        let p = &mut tab.panes[t.index];
        if t.visible {
            place(p, x, y, w, h);
        } else {
            p.rect = (x, y, w, h);
            p.visible = false;
        }
    }
}

/// Size the grids of every tab that is NOT the active one, to the geometry each would get
/// if it were shown right now.
///
/// A pane is born at [`State::spawn_cells`], which can only guess: it copies the focused
/// pane of the *active* tab, or falls back to 80x24 when there is nothing to copy. That is
/// honest about the pty that was just spawned, but it is not the size of the tab the pane
/// actually landed in — and `relayout_active` only ever touches `state.tabs[state.active]`,
/// so a pane restored (or attached) into a background tab kept its birth grid until the human
/// happened to visit that tab. A workspace restored with a 194x50 window would leave such a
/// pane painting into an 80x24 corner of its box, with the rest of the pane blank.
///
/// This is a SIZE-ONLY pass: it never sets `rect`, `surf` or `visible`, because a background
/// tab draws nothing and those belong to whoever draws. `relayout_active` stays authoritative
/// for the tab on screen (it alone knows about fullscreen and the focused-pane solo).
///
/// It also removes a papercut that predates the bug: a window resized while a tab was in the
/// background used to reflow that tab the instant it was shown, costing its shell a
/// SIGWINCH-driven erase 300ms later. The size is now already correct when it appears.
#[tracing::instrument(level = "debug", skip(state))]
fn relayout_background(state: &mut State, area: (f32, f32), scale: f32) {
    let (aw, ah) = area;
    let active = state.active;
    let now = Instant::now();
    for (i, tab) in state.tabs.iter_mut().enumerate() {
        if i == active {
            continue;
        }
        let n = tab.panes.len();
        if n == 0 {
            continue;
        }
        // A zoomed background tab will fill its area with the one pane, so size it that way.
        // An out-of-range index is stale bookkeeping rather than a zoom — fall through to the
        // tiles instead of leaving every pane in the tab unsized.
        if let Some(z) = tab.zoomed.filter(|z| *z < n) {
            let (_, _, gw, gh) = pane_box(0.0, 0.0, aw, ah);
            fit_grid(&mut tab.panes[z], gw, gh, scale, now);
            continue;
        }
        let eff = effective_layout(tab.layout, n);
        let tiles = compute_tiles(eff, n, &tab.sizes, tab.main_fraction, tab.focused as i32);
        for t in &tiles {
            // Same rule as the active pass: a tile the layout hides gets no grid of its own.
            if !t.visible {
                continue;
            }
            let w = (t.rect.w * aw as f64) as f32;
            let h = (t.rect.h * ah as f64) as f32;
            let (_, _, gw, gh) = pane_box(0.0, 0.0, w, h);
            fit_grid(&mut tab.panes[t.index], gw, gh, scale, now);
        }
    }
}

/// Pixel rects for the active tab's draggable dividers.
#[tracing::instrument(level = "debug", ret, skip(state))]
fn build_dividers(state: &State, area: (f32, f32)) -> Vec<DividerItem> {
    let (aw, ah) = area;
    state
        .dividers()
        .iter()
        .map(|d| {
            let vertical = d.orientation == Orientation::Vertical;
            let main = d.kind == DividerKind::Main;
            if vertical {
                DividerItem {
                    x: d.at as f32 * aw - DIVIDER_THICK / 2.0,
                    y: 0.0,
                    w: DIVIDER_THICK,
                    h: ah,
                    vertical: true,
                    index: d.index,
                    main,
                }
            } else {
                DividerItem {
                    x: 0.0,
                    y: d.at as f32 * ah - DIVIDER_THICK / 2.0,
                    w: aw,
                    h: DIVIDER_THICK,
                    vertical: false,
                    index: d.index,
                    main,
                }
            }
        })
        .collect()
}

/// Rebuild every UI model + scalar from `State` (the resync step). Called when
/// `state.dirty` is set.
/// Push the module rail onto [`RailAdapter`](crate::RailAdapter): the strip's module
/// buttons, which one is active, and the active entry's rows.
///
/// Split out of [`resync`] so the UI tests can drive the same projection from a
/// `ModuleRail` the host's own [`RailEvent`](avada_core::module::RailEvent)s built,
/// instead of hand-filling the adapter and proving only that Slint stores properties.
/// The active entry is written from Rust rather than read back from the strip, so the
/// panel can never claim a module entry the model has already dropped.
pub fn fill_rail(app: &AppWindow, rail: &crate::leftpanel::ModuleRail) {
    use slint::ComponentHandle;
    let ad = app.global::<crate::RailAdapter>();
    let entries = rail.entries();
    ad.set_present(!entries.is_empty());
    ad.set_entries(model(
        entries
            .iter()
            .map(|v| crate::RailEntryRow {
                key: v.key.as_str().into(),
                id: v.entry.id.as_str().into(),
                label: v.entry.label.as_str().into(),
                icon: crate::leftpanel::icon_commands(v.entry.icon.as_deref()).into(),
                module: v.module.as_str().into(),
                tier: v.entry.tier as i32,
                order: v.entry.order,
            })
            .collect(),
    ));
    ad.set_active(rail.active.clone().unwrap_or_default().as_str().into());
    ad.set_active_label(
        rail.active
            .as_deref()
            .and_then(|k| rail.lookup(k))
            .map(|v| v.entry.label)
            .unwrap_or_default()
            .into(),
    );
    ad.set_rows(model(
        rail.active_rows()
            .iter()
            .map(|r| crate::RailRowItem {
                id: r.id.as_str().into(),
                label: r.label.as_str().into(),
                detail: r.detail.as_str().into(),
                depth: r.depth as i32,
                expandable: r.expandable,
                expanded: r.expanded,
                icon: crate::leftpanel::icon_commands(r.icon.as_deref()).into(),
                // Shipped verbatim as well as interpreted: the two marks the panel draws
                // get their own booleans, and the whole set goes across so a mark this
                // build does not know is still announced rather than silently dropped.
                marks: r.marks.join(" ").into(),
                selected: crate::leftpanel::has_mark(r, crate::leftpanel::MARK_SELECTED),
                dimmed: crate::leftpanel::has_mark(r, crate::leftpanel::MARK_HIDDEN),
            })
            .collect(),
    ));

    // A tier-1 entry gets the filter box. The module is not asked first: `rail.query` is a
    // notification and the host only delivers it to a module that subscribed, so a module
    // that does not want one simply never hears from it. (A `filterable` flag on
    // `RailEntry` would say so properly — see the SDK note in the Wave 2 report.)
    let tier1 = rail
        .active
        .as_deref()
        .and_then(|k| rail.lookup(k))
        .is_some_and(|v| v.entry.tier as i32 == 1);
    ad.set_filterable(tier1);

    // Both written every tick, which is inert: the view watches `scroll_seq` and only a
    // change in it moves the viewport (see `RailRowView`'s list in leftpanel.slint).
    ad.set_scroll_y(rail.scroll_y());
    ad.set_scroll_seq(rail.scroll_seq);
}

/// How long the computed grid size must hold still before the pty is told about it.
///
/// A pty resize is a SIGWINCH, and a shell answers one by repositioning its prompt and
/// erasing to the end of the display (zsh: `\r\e[A\e[A…\e[J`) — so every intermediate size
/// the layout passes through on its way to the final one costs the user real scrollback.
/// Startup is the worst case: the window opens at its preferred size, the OS then restores
/// the saved frame, and each pass used to fire its own SIGWINCH. On a re-attach that meant
/// the last command's OUTPUT was erased out of the restored pane — the process survived, the
/// thing you restarted the app to look at did not.
///
/// Long enough to swallow that churn and an interactive window drag, short enough that a
/// deliberate resize reaches the shell before it can be noticed.
const PTY_RESIZE_SETTLE: Duration = Duration::from_millis(300);

/// How long a reveal keeps re-asserting its scroll target at a module's row list (see
/// [`crate::leftpanel::ModuleRail::bump_scroll`]). Long enough to outlast the panel being
/// instantiated and the ListView measuring its new model; short enough that the human
/// cannot scroll away inside it and be dragged back.
pub const RAIL_SCROLL_HOLD: Duration = Duration::from_millis(250);

/// Push any pane whose grid has finished moving to its session, and nothing sooner.
///
/// Runs every tick — NOT from [`resync`], which only runs on a dirty state and would leave a
/// settled size pending indefinitely on an idle app.
#[tracing::instrument(level = "debug", skip_all)]
fn flush_pty_resizes(state: &mut State, mgr: &SessionManager, now: Instant) {
    flush_pty_resizes_to(state, now, |uid, cols, rows| mgr.resize(uid, cols, rows));
}

/// [`flush_pty_resizes`] with the session backend abstracted to `deliver`, which answers
/// whether the resize actually reached a pty. Tests drive the debounce and the retry through
/// this without spawning real PTYs; production passes `SessionManager::resize`.
fn flush_pty_resizes_to(
    state: &mut State,
    now: Instant,
    mut deliver: impl FnMut(&str, u16, u16) -> bool,
) {
    for tab in &mut state.tabs {
        for p in &mut tab.panes {
            if p.applied != p.pty && now.duration_since(p.pty_since) >= PTY_RESIZE_SETTLE {
                // Degenerate sizes never reach a pty (a pane that has not been laid out yet
                // sits at (0, 0) — telling the shell that would be a lie, not a resize).
                // A dropped resize stays pending. `deliver` answers `false` when it went
                // nowhere — the commonest case being a control-spawned pane laid out before
                // the daemon has confirmed its session — and recording the delivery anyway
                // left that pane's pty at its 80x24 spawn size permanently, with a grid the
                // program never fills, no scrollback, and so a dead mouse wheel. `pty_since`
                // is already past the settle window, so this retries on the next tick.
                if p.applied.0 >= 2
                    && p.applied.1 >= 1
                    && deliver(&p.uid, p.applied.0 as u16, p.applied.1 as u16)
                {
                    p.pty = p.applied;
                }
            }
        }
    }
}

/// Offer every on-screen tier-5 surface its cell size, once its pane has stopped moving.
///
/// Deliberately stateless: it re-offers the same size every tick, and
/// [`crate::module_runtime::ModuleRuntime::grid_resize`] is what remembers and drops the
/// repeats. Keeping the memory there rather than here means the size a module was told is
/// answered by the thing that told it, and survives a pane being closed and reopened at the
/// same size — which is the case where a fresh notification would be pure noise.
///
/// Shares the pty path's debounce clock (`p.pty_since`) and its degenerate-size floor for
/// the same reasons: a size still being dragged is not a size, and a pane that has not been
/// laid out yet sits at (0, 0), which would be a lie rather than a resize.
pub(crate) fn flush_grid_resizes(
    state: &State,
    now: Instant,
    mut deliver: impl FnMut(&ModulePaneRef, u16, u16),
) {
    for tab in &state.tabs {
        for p in &tab.panes {
            let PaneKind::Module(m) = &p.kind else {
                continue;
            };
            if p.applied.0 >= 2
                && p.applied.1 >= 1
                && now.duration_since(p.pty_since) >= PTY_RESIZE_SETTLE
            {
                deliver(m, p.applied.0 as u16, p.applied.1 as u16);
            }
        }
    }
}

#[tracing::instrument(level = "debug", skip_all)]
pub fn resync(
    state: &mut State,
    app: &AppWindow,
    ui: &Ui,
    area: (f32, f32),
    scale: f32,
    mgr: &SessionManager,
) {
    relayout_active(state, area, scale);
    relayout_background(state, area, scale);

    // tab strip
    let active = state.active;
    let tabs: Vec<TabItem> = state
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabItem {
            title: t.title.clone(),
            active: i == active,
            system: t.system,
        })
        .collect();
    sync_model(&ui.tabs, tabs);

    // layout picker
    let cur = state.active_tab().layout;
    let layouts: Vec<LayoutOption> = theme::LAYOUT_MENU
        .iter()
        .map(|l| LayoutOption {
            id: theme::layout_id(*l),
            label: theme::layout_label(*l).as_ref().into(),
            active: *l == cur,
            hint: SharedString::new(),
        })
        .collect();
    sync_model(&ui.layouts, layouts);

    // Appearance controls reflect the DRAFT while Preferences is open (so edits preview
    // without touching the live panes), else the committed settings. Read before the panes
    // rather than beside the other appearance controls further down, because a source pane's
    // syntax colours are baked into its rows: `view_ui` is an input to the projection, so a
    // palette previewed in Preferences recolours the code too.
    let (_view_font, view_palette, view_theme, view_ui, view_px, view_frame, view_dot) =
        state.appearance_view();

    // panes
    let show_frame = state.settings.show_frame;
    let show_dot = state.settings.show_dot;
    let editing_pane = state.editing_pane;
    // The merged tool identity + liveness badge, resolved BEFORE the tab borrow: both read
    // `State`'s runtime side-maps, which the per-pane borrow below would otherwise block.
    let tool_row: Vec<(PaneKind, i32)> = state
        .active_tab()
        .panes
        .iter()
        .map(|p| (state.effective_kind(p), state.liveness_ui(&p.uid)))
        .collect();
    let t = state.active_tab();
    let focused = t.focused;
    let items: Vec<PaneItem> = t
        .panes
        .iter()
        .enumerate()
        .map(|(i, p)| {
            pane_item(
                p,
                i == focused,
                i as i32 == editing_pane,
                show_frame,
                show_dot,
                p.font_px,
                &tool_row[i].0,
                tool_row[i].1,
                view_ui,
            )
        })
        .collect();
    // Slint samples a TouchArea's `mouse-cursor` only while dispatching a mouse event, and the
    // link hover hit-test lands in the model one pump tick AFTER the move that triggered it —
    // so the pointer/text cursor switch is permanently one OS mouse-move late (the tooltip and
    // underline are model-driven and fine). When a pane's hovered-link presence flips, replay
    // the cursor at its current position: the same-point SetCursorPos posts a fresh
    // WM_MOUSEMOVE, which re-runs Slint's input pipeline against the now-correct property.
    let link_flipped = (0..items.len()).any(|i| {
        ui.panes
            .row_data(i)
            .is_some_and(|old| old.link_visible != items[i].link_visible)
    });
    sync_model(&ui.panes, items);
    if link_flipped {
        // MUST be deferred: resync runs inside the pump with the window's state RefCell
        // borrowed, and re-entering UI callbacks here is the #18 re-borrow crash class.
        // Two shots: 30ms covers the common settle, and 250ms outlasts the window in which
        // Slint's input pipeline still re-samples the PRE-flip cursor on real mouse moves
        // (~100ms observed live) — a move in that window re-asserts the stale cursor over
        // the first replay. Each shot reads the model live, so a flip in between can't
        // apply an outdated cursor.
        for delay in [30u64, 250] {
            let weak = {
                use slint::ComponentHandle;
                app.as_weak()
            };
            slint::Timer::single_shot(Duration::from_millis(delay), move || {
                if let Some(app) = weak.upgrade() {
                    let active = app.get_panes().iter().any(|p| p.link_visible);
                    replay_cursor_pos(&app, active);
                }
            });
        }
    }

    // dividers
    let divs = build_dividers(state, area);
    tracing::debug!(
        "resync: active={} layout={:?} panes={} dividers={} {:?}",
        state.active,
        state.active_tab().layout,
        state.active_tab().panes.len(),
        divs.len(),
        divs.iter()
            .map(|d| format!(
                "(x={:.0},y={:.0},w={:.0},h={:.0},vert={})",
                d.x, d.y, d.w, d.h, d.vertical
            ))
            .collect::<Vec<_>>()
    );
    sync_model(&ui.dividers, divs);

    // scalars
    app.set_editing_tab(state.editing_tab);
    app.set_zoomed(state.active_tab().zoomed.is_some());
    app.set_fullscreen(state.fullscreen);
    app.set_esc_holding(state.esc_holding);
    // Terminal zoom factor of the FOCUSED pane (its font px / base) → scales the fullscreen
    // hint (fullscreen solos the focused pane), tracking that pane's per-pane zoom.
    let focused_px = {
        let t = state.active_tab();
        t.panes
            .get(t.focused)
            .map(|p| p.font_px)
            .unwrap_or(prefs::DEFAULT_FONT_PX)
    };
    app.set_zoom_factor(focused_px / prefs::DEFAULT_FONT_PX);
    // Single-layout pane taskbar gate (the hidden-panes strip; see State::taskbar_visible).
    app.set_taskbar_visible(state.taskbar_visible());

    // ---- Wave-2 overlay projection ----
    let kind = match state.overlay {
        Overlay::None => 0,
        Overlay::Palette => 1,
        Overlay::Prefs => 2,
        Overlay::NewPane => 3,
        Overlay::AddProject => 4,
        Overlay::NewGoal => 5,
        Overlay::AskBrowser => 6,
        Overlay::ConfirmClose => 7,
    };
    app.set_overlay_kind(kind);

    // ---- module capability rights (track H2) ----
    // Filled on every resync, not only while Preferences is open: the ask toast is mounted
    // app-wide because a module asks for a capability when it needs one, which has nothing
    // to do with whether the user happens to be looking at the rights page.
    crate::prefs::rights::fill(
        app,
        &state.rights.lock().unwrap(),
        state.rights_selected.as_ref(),
        state.rights_workspace().as_deref(),
    );
    // `open` is the deep link INTO the rights page (a toast saying "review this"). It is a
    // one-shot: closing Preferences must not leave the next open pinned to rights.
    if state.overlay != Overlay::Prefs {
        use slint::ComponentHandle as _;
        app.global::<crate::RightsAdapter>().set_open(false);
    }

    // Close confirmation: what's about to close, and whether this is the close that ends the
    // window (the card drops its reassurance and its "don't ask again" when it is — that one
    // really is final). Only projected while the card is up.
    if let Some(pc) = state.pending_close.as_ref() {
        app.set_cc_title(pc.title.clone());
        app.set_cc_final(pc.final_close);
        app.set_cc_ask(state.settings.confirm_close);
    }

    // "Open link with…" chooser: the held URL + the browser rows snapshotted at open. Only
    // refreshed while the card is up — `sync_model` rewrites every row, and there is no
    // reason to pay for that on every tick of a session that never opens a link.
    if state.overlay == Overlay::AskBrowser {
        app.set_ask_url(state.ask_url.as_str().into());
        let rows: Vec<PrefBrowserRow> = state
            .ask_browsers
            .iter()
            .map(|b| PrefBrowserRow {
                id: b.id.as_str().into(),
                name: b.name.as_str().into(),
                // The chooser is a one-shot question, not a setting: no row is "current".
                active: false,
            })
            .collect();
        sync_model(&ui.ask_browsers, rows);
    }

    // Add-Project dialog: mirror the inline validation error (`""` = none). The dialog is
    // re-instantiated each time `kind` becomes 4, but the error must update live while it
    // stays open (a failed submit keeps the overlay mounted).
    app.set_ap_error(state.add_project_error.as_str().into());

    // New Pane dialog seeds: the default swatch index (next palette-rotation slot), the
    // palette swatches, and the shell-picker options (id 0 = "Use default shell"). The dialog
    // reads these once at open (it's re-instantiated each time `kind` becomes 3), so
    // refreshing them every resync is harmless.
    let np_swatches = state.frame_swatches();
    let np_default_idx = if np_swatches.is_empty() {
        0
    } else {
        (state.active_tab().panes.len() % np_swatches.len()) as i32
    };
    app.set_np_default_idx(np_default_idx);
    sync_model(&ui.np_swatches, np_swatches);
    let np_shells: Vec<PrefOption> = prefs::SHELL_OPTIONS
        .iter()
        .enumerate()
        .map(|(id, (label, _))| PrefOption {
            id: id as i32,
            label: if id == 0 {
                "Use default shell".into()
            } else {
                (*label).into()
            },
            active: false,
        })
        .collect();
    sync_model(&ui.np_shells, np_shells);

    // command palette rows + selection
    let palette: Vec<PaletteItem> = state
        .palette_rows()
        .into_iter()
        .map(|(title, subtitle)| PaletteItem { title, subtitle })
        .collect();
    sync_model(&ui.palette, palette);
    app.set_palette_sel(state.palette_sel as i32);
    // The query display is a controller-owned mirror (the key router edits
    // state.palette_query while the palette is open — no focused TextInput).
    app.set_palette_query(state.palette_query.as_str().into());

    // The shell palette follows the DRAFT, not the committed setting: a window's own chrome
    // has nowhere to be previewed except in the window, so picking a palette recolours the
    // whole shell at once and Cancel puts it back (`prefs_cancel` drops the draft, which
    // moves `view_ui` back to the committed index and this same push undoes it).
    if ui.ui_palette_pushed.get() != view_ui {
        ui.ui_palette_pushed.set(view_ui);
        push_ui_palette(app, theme::ui_palette(view_ui));
    }

    // Font family: the fixed option list (mirrors the renderer) + a trailing "Custom…"
    // entry. Active = the option whose value matches the drafted raw value, or Custom when
    // the picker is in custom mode (a user-typed font path).
    let raw_font = match &state.prefs_draft {
        Some(d) => d.font_family.clone(),
        None => state.settings.font_family.clone(),
    };
    let custom = state.font_custom;
    let mut font_label = String::new();
    let mut families: Vec<PrefOption> = prefs::FONT_OPTIONS
        .iter()
        .enumerate()
        .map(|(id, (label, value))| {
            let active = !custom && *value == raw_font;
            if active {
                font_label = (*label).to_string();
            }
            PrefOption {
                id: id as i32,
                label: (*label).into(),
                active,
            }
        })
        .collect();
    families.push(PrefOption {
        id: prefs::FONT_OPTIONS.len() as i32,
        label: "Custom…".into(),
        active: custom,
    });
    if custom {
        font_label = "Custom…".to_string();
    } else if font_label.is_empty() {
        font_label = prefs::FONT_OPTIONS[0].0.to_string();
    }
    sync_model(&ui.families, families);
    app.set_pref_font_label(font_label.into());
    app.set_pref_font_custom(custom);
    app.set_pref_font_custom_value(raw_font.into());
    // Preview header accent = the drafted palette's first slot (the surface itself is
    // rendered by the controller's locked preview terminal; see State::render_preview).
    app.set_pref_preview_accent(theme::accent_for(0, view_palette));

    // frame-palette options (label + 8 slot color chips), active = drafted/current
    let palettes: Vec<FramePaletteOption> = theme::FRAME_PALETTES
        .iter()
        .enumerate()
        .map(|(id, (label, slots))| {
            let colors: Vec<slint::Color> = slots
                .iter()
                .map(|(r, g, b)| slint::Color::from_rgb_u8(*r, *g, *b))
                .collect();
            FramePaletteOption {
                id: id as i32,
                label: (*label).into(),
                active: id == view_palette,
                colors: ModelRc::from(Rc::new(VecModel::from(colors))),
            }
        })
        .collect();
    sync_model(&ui.palettes, palettes);

    // shell-palette options. Four swatches, in the order that actually tells them apart:
    // the window ground, the raised surface, the accent, and the ink — enough to see at a
    // glance that Latte is light and High Contrast is not.
    let ui_palettes: Vec<FramePaletteOption> = theme::UI_PALETTES
        .iter()
        .enumerate()
        .map(|(id, p)| FramePaletteOption {
            id: id as i32,
            label: p.name.into(),
            active: id == view_ui,
            colors: ModelRc::from(Rc::new(VecModel::from(vec![
                slint::Color::from_argb_encoded(p.bg),
                slint::Color::from_argb_encoded(p.surface2),
                slint::Color::from_argb_encoded(p.accent),
                slint::Color::from_argb_encoded(p.text),
            ]))),
        })
        .collect();
    sync_model(&ui.ui_palettes, ui_palettes);

    // terminal colour-theme options (active = drafted/current); preview colors come from it.
    let mut theme_label = String::new();
    let themes: Vec<PrefOption> = theme::TERMINAL_THEMES
        .iter()
        .enumerate()
        .map(|(id, (label, _))| {
            let active = id == view_theme;
            if active {
                theme_label = (*label).to_string();
            }
            PrefOption {
                id: id as i32,
                label: (*label).into(),
                active,
            }
        })
        .collect();
    sync_model(&ui.themes, themes);
    app.set_pref_theme_label(theme_label.into());
    // preview letterbox background = the drafted theme's background colour.
    app.set_pref_preview_bg(theme::theme_color(view_theme, 0));

    // default-shell options; active = the one whose token matches the saved setting.
    let mut shell_label = prefs::SHELL_OPTIONS[0].0.to_string();
    let shells: Vec<PrefOption> = prefs::SHELL_OPTIONS
        .iter()
        .enumerate()
        .map(|(id, (label, value))| {
            let active = *value == state.settings.default_shell;
            if active {
                shell_label = (*label).to_string();
            }
            PrefOption {
                id: id as i32,
                label: (*label).into(),
                active,
            }
        })
        .collect();
    sync_model(&ui.shells, shells);
    app.set_pref_shell_label(shell_label.into());

    // idle-glow: the effect picker (active = the saved token) + the toggle/threshold scalars.
    let active_effect = crate::glow::IdleEffect::from_token(&state.settings.idle_effect);
    let mut idle_label = crate::glow::IdleEffect::OPTIONS[0].1.to_string();
    let idle_effects: Vec<PrefOption> = crate::glow::IdleEffect::OPTIONS
        .iter()
        .enumerate()
        .map(|(id, (_, label))| {
            let active = id == active_effect.index();
            if active {
                idle_label = (*label).to_string();
            }
            PrefOption {
                id: id as i32,
                label: (*label).into(),
                active,
            }
        })
        .collect();
    sync_model(&ui.idle_effects, idle_effects);
    app.set_pref_idle_alert(state.settings.idle_alert);
    app.set_pref_idle_effect_label(idle_label.into());
    app.set_pref_idle_seconds(state.settings.idle_alert_seconds as i32);

    // keybindings list (the EFFECTIVE keymap — overrides over defaults), grouped by category
    // and rendered as <kbd> chips. Each row is editable: click to capture a new chord, with a
    // per-row reset when overridden. `capturing` marks the row currently capturing input.
    let capturing = state.capturing_binding.clone();
    let mut prev_cat = "";
    let mut keybindings: Vec<KeybindingItem> = state
        .keymap
        .rows()
        .into_iter()
        .map(|r| {
            let group_first = r.category != prev_cat;
            prev_cat = r.category;
            let parts: Vec<SharedString> = r.parts.into_iter().map(Into::into).collect();
            KeybindingItem {
                id: r.id.into(),
                label: r.label.into(),
                parts: ModelRc::from(Rc::new(VecModel::from(parts))),
                category: r.category.into(),
                group_first,
                overridden: r.overridden,
                capturing: capturing.as_deref() == Some(r.id),
                unbound: r.unbound,
                static_row: false,
            }
        })
        .collect();
    // The non-rebindable "Focus pane by number → Alt 1…9" documentation row, appended right
    // after the last Panes binding (mirrors Electron's static row under the Panes group).
    if let Some(pos) = keybindings.iter().rposition(|k| k.category == "Panes") {
        keybindings.insert(
            pos + 1,
            KeybindingItem {
                id: SharedString::new(),
                label: "Focus pane by number".into(),
                parts: ModelRc::from(Rc::new(VecModel::<SharedString>::default())),
                category: "Panes".into(),
                group_first: false,
                overridden: false,
                capturing: false,
                unbound: false,
                static_row: true,
            },
        );
    }
    // Every tier-5 module surface that declared a keymap gets its own category, appended
    // after the app's own. One editor, two stores: a module row's id carries `module:` and
    // routes to the module keymap store (see `module_ui::grid::parse_row_id`), so capture,
    // unbind, reset and reset-all all work on a module action without a second UI.
    for (module, surface) in crate::module_ui::grid::declared() {
        let category = format!("{} · {surface}", module.as_str());
        let mut first = true;
        for (action, label, chord) in crate::module_ui::grid::binding_rows(&module, &surface) {
            let id = crate::module_ui::grid::row_id(&module, &surface, &action);
            let parts: Vec<SharedString> = crate::module_ui::grid::chord_parts(&chord)
                .into_iter()
                .map(Into::into)
                .collect();
            let unbound = parts.is_empty();
            keybindings.push(KeybindingItem {
                capturing: capturing.as_deref() == Some(id.as_str()),
                id: id.into(),
                label: label.into(),
                parts: ModelRc::from(Rc::new(VecModel::from(parts))),
                category: category.clone().into(),
                group_first: std::mem::take(&mut first),
                overridden: crate::module_ui::grid::overridden(&module, &action),
                unbound,
                static_row: false,
            });
        }
    }
    sync_model(&ui.keybindings, keybindings);
    app.set_pref_keybinds_overridden(
        state.keymap.any_overridden() || crate::module_ui::grid::any_overridden(),
    );
    app.set_pref_kb_conflict(state.capture_conflict.clone().unwrap_or_default().into());

    // Dialog appearance scalars come from the draft view; the actual panes keep the
    // committed show_frame/show_dot until Done.
    app.set_pref_fontpx(view_px.round() as i32);
    app.set_pref_frame(view_frame);
    app.set_pref_dot(view_dot);
    app.set_show_frame(state.settings.show_frame);
    app.set_show_dot(state.settings.show_dot);
    app.set_prefs_confirm(state.prefs_confirm);
    app.set_pref_clickable(state.settings.clickable_paths);
    app.set_pref_copy_on_select(state.settings.copy_on_select);
    app.set_pref_confirm_close(state.settings.confirm_close);
    app.set_pref_editor(state.settings.editor_command.clone().into());

    // sidebar / projects: the rail gating + flyout state + rows
    app.set_show_sidebar(state.settings.show_sidebar);
    app.set_sidebar_open(state.sidebar_open);
    // Refresh the worktree cache on the closed→open edge, then build the two-level tree:
    // each project header carries its enumerated worktrees (only while the flyout is open —
    // no point spawning git for a hidden panel). Order matches `state.projects`, so the
    // flyout row index `i` indexes both the model and `state.projects` (used by delete).
    let sidebar_open = state.sidebar_open;
    crate::sidebar::note_flyout_open(sidebar_open);
    let projects: Vec<ProjectItem> = state
        .project_rows()
        .into_iter()
        .zip(state.projects.iter())
        .map(|((name, color), proj)| {
            let rows: Vec<WorktreeRow> = if sidebar_open {
                crate::sidebar::worktrees_for(&proj.path)
                    .into_iter()
                    .map(|w| WorktreeRow {
                        path: w.path.into(),
                        branch: w.branch.into(),
                        is_main: w.is_main,
                        locked: w.locked,
                        prunable: w.prunable,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            // This project's recent agent sessions — read (cached) only while the flyout is
            // open, mirroring the worktree gate (no point scanning ~/.claude for a hidden
            // panel). Filtered Rust-side by the project's history search query (#25); the
            // segment/query pair itself lives in the sidebar's per-project map, fed by the
            // pump's `HistoryUi` poll (see `pump`).
            let (segment, query) = crate::sidebar::history_ui_for(&proj.path);
            let (sessions, has_history): (Vec<ClaudeSessionItem>, bool) = if sidebar_open {
                let rows = crate::sidebar::claude_sessions_for(&proj.path, &query)
                    .into_iter()
                    .map(|s| ClaudeSessionItem {
                        id: s.id.into(),
                        source: s.source.into(),
                        summary: s.summary.into(),
                        when: s.when.into(),
                        count: s.count,
                    })
                    .collect();
                (rows, crate::sidebar::claude_has_history(&proj.path))
            } else {
                (Vec::new(), false)
            };
            // Reuse this project's worktree model (stable identity) and update its contents
            // in place, so the inner repeater isn't rebuilt every frame (see `wt_models`).
            let model = ui
                .wt_models
                .borrow_mut()
                .entry(proj.path.clone())
                .or_insert_with(|| Rc::new(VecModel::default()))
                .clone();
            sync_model(&model, rows);
            // Same stable-identity reuse for the session rows (see `claude_models`).
            let cmodel = ui
                .claude_models
                .borrow_mut()
                .entry(proj.path.clone())
                .or_insert_with(|| Rc::new(VecModel::default()))
                .clone();
            sync_model(&cmodel, sessions);
            ProjectItem {
                name,
                color,
                worktrees: ModelRc::from(model),
                sessions: ModelRc::from(cmodel),
                has_history,
                segment,
                query: query.into(),
            }
        })
        .collect();
    // Drop cached worktree + session models for projects no longer present.
    {
        let live: std::collections::HashSet<&str> =
            state.projects.iter().map(|p| p.path.as_str()).collect();
        ui.wt_models
            .borrow_mut()
            .retain(|k, _| live.contains(k.as_str()));
        ui.claude_models
            .borrow_mut()
            .retain(|k, _| live.contains(k.as_str()));
    }
    sync_model(&ui.projects, projects);

    // ---- the left slide-out panel (mux plan M5) ----
    // Three sections, all projected read-only out of `State` + the session manager:
    //   1. the workspace tree — every tab and its panes (NOT just the active tab, which is
    //      why the liveness/idle flags are recomputed here instead of read off
    //      `PaneState::glow` — the pump only animates the active tab's glow);
    //   2. the saved-workspace library, served from `leftpanel`'s cache (rescanned on the
    //      panel's closed→open edge, never stat'ed per tick);
    //   3. the detached sessions — live sessions this window isn't showing.
    // The panel's `open` flag also gates the work: with the panel shut the sections are
    // left exactly as they are, so a closed panel costs one bool write.
    {
        use slint::ComponentHandle as _;
        let lp = app.global::<LeftPanelAdapter>();
        let open = state.left_panel_open;
        lp.set_open(open);
        if open {
            let now_ms = crate::glow::now_epoch_ms();
            let idle_on = state.settings.idle_alert;
            let idle_threshold_ms = state.settings.idle_alert_seconds as u64 * 1000;
            let active = state.active;
            let palette = state.settings.frame_palette;
            let tab_rows: Vec<LeftTabRow> = state
                .tabs
                .iter()
                .enumerate()
                .map(|(ti, t)| {
                    let rows: Vec<LeftPaneRow> = t
                        .panes
                        .iter()
                        .enumerate()
                        .map(|(pi, p)| {
                            let last = mgr.last_output_at(&p.uid);
                            // The tree's mark comes off the EFFECTIVE kind, the same merged
                            // identity the header's mark is drawn from (the pane's own kind,
                            // or the OSC-title sniff when it is a plain terminal), so the two
                            // views of one pane can never disagree about what it is running.
                            let kind = state.effective_kind(p);
                            LeftPaneRow {
                                uid: p.uid.as_str().into(),
                                title: p.title.clone(),
                                tint: p.accent,
                                focused: ti == active && pi == t.focused,
                                live: crate::leftpanel::liveness(last, now_ms),
                                idle: crate::leftpanel::is_idle(
                                    &p.shell_title,
                                    last,
                                    now_ms,
                                    idle_on,
                                    idle_threshold_ms,
                                ),
                                mark: crate::leftpanel::pane_mark_kind(&kind),
                                brand: crate::leftpanel::pane_mark_ink(&kind, p.accent),
                            }
                        })
                        .collect();
                    // Stable per-tab model identity (see `lp_pane_models`).
                    let model = ui
                        .lp_pane_models
                        .borrow_mut()
                        .entry(ti)
                        .or_insert_with(|| Rc::new(VecModel::default()))
                        .clone();
                    sync_model(&model, rows);
                    LeftTabRow {
                        title: t.title.clone(),
                        active: ti == active,
                        // The group's colour = the halo of the pane this tab focuses, so a
                        // row in the tree matches the glow you get when you switch to it.
                        // An empty tab has no pane to borrow from: give it its own slot in
                        // the active palette rather than leaving it uncoloured.
                        tint: t
                            .panes
                            .get(t.focused)
                            .or_else(|| t.panes.first())
                            .map(|p| p.accent)
                            .unwrap_or_else(|| crate::theme::accent_for(ti, palette)),
                        panes: ModelRc::from(model),
                    }
                })
                .collect();
            let live_tabs = state.tabs.len();
            ui.lp_pane_models.borrow_mut().retain(|k, _| *k < live_tabs);
            sync_model(&ui.lp_tabs, tab_rows);

            let claimed = state.claimed_uids();
            let det_rows: Vec<LeftSessionRow> = crate::leftpanel::detached(mgr, &claimed)
                .into_iter()
                .map(|d| LeftSessionRow {
                    uid: d.uid.into(),
                    label: d.label.into(),
                    detail: d.detail.into(),
                    live: crate::leftpanel::liveness(d.last_output_at, now_ms),
                })
                .collect();
            sync_model(&ui.lp_detached, det_rows);

            // ---- the module rail (track H4) ----
            // Entries and rows are a stored projection like the explorer's: `State` folded
            // the host's rail events into `state.rail`, and the resync just ships them.
            fill_rail(app, &state.rail);
        }
    }

    // ---- new-goal dialog: attached-image filenames ----
    let goal_image_names: Vec<slint::SharedString> = state
        .goal_draft_images
        .iter()
        .map(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
                .into()
        })
        .collect();
    sync_model(&ui.goal_images, goal_image_names);

    // ---- new-goal box: controller-owned mirrors (text, field focus, option list, chips) ----
    app.set_goal_text(state.goal_text.as_str().into());
    app.set_goal_field(state.goal_field as i32);
    app.set_goal_menu_open(state.goal_menu_open);
    app.set_goal_menu_sel(state.goal_menu_sel as i32);
    app.set_goal_focus_tick(state.goal_focus_seq);
    app.set_goal_settext_value(state.goal_text.as_str().into());
    app.set_goal_settext_tick(state.goal_settext_seq);
    app.set_goal_options_open(state.goal_options_open);
    let goal_menu: Vec<PaletteItem> = state
        .goal_menu_rows()
        .into_iter()
        .map(|(title, subtitle)| PaletteItem {
            title: title.into(),
            subtitle: subtitle.into(),
        })
        .collect();
    sync_model(&ui.goal_menu, goal_menu);
    let goal_chips: Vec<slint::SharedString> = state
        .goal_chip_labels()
        .into_iter()
        .map(Into::into)
        .collect();
    sync_model(&ui.goal_chips, goal_chips);

    // ---- context menu (pane header / tab strip) ----
    let ctx_kind = match state.ctx.as_ref() {
        None => 0,
        Some(c) => match c.kind {
            CtxKind::Pane => 1,
            CtxKind::Tab => 2,
            CtxKind::App => 3,
            // The Files-row menu draws as a plain list — no swatches, no submenus — so it
            // shares the App shape rather than claiming a kind of its own in the UI.
            CtxKind::File => 3,
        },
    };
    app.set_ctx_kind(ctx_kind);
    if let Some(c) = state.ctx.as_ref() {
        app.set_ctx_x(c.x);
        app.set_ctx_y(c.y);
        let entries: Vec<MenuEntry> = c
            .entries
            .iter()
            .map(|e| MenuEntry {
                label: e.label.clone(),
                shortcut: e.shortcut.clone(),
                icon: e.icon,
                kind: e.kind,
                checked: e.checked,
                show_check: e.show_check,
                disabled: e.disabled,
                danger: e.danger,
            })
            .collect();
        sync_model(&ui.ctx_entries, entries);

        match c.kind {
            CtxKind::Pane => {
                // Change-Color swatches = the active frame palette's slots; the selected ring
                // tracks whichever slot matches the pane's current color (when frame is on).
                let slots = theme::frame_palette(state.settings.frame_palette);
                let swatches: Vec<Color> = slots
                    .iter()
                    .map(|(r, g, b)| Color::from_rgb_u8(*r, *g, *b))
                    .collect();
                sync_model(&ui.ctx_swatches, swatches);
                let t = state.active_tab();
                let (frame_on, dot_on, cur_swatch) = match t.panes.get(c.target) {
                    Some(p) => {
                        let cur = slots
                            .iter()
                            .position(|(r, g, b)| Color::from_rgb_u8(*r, *g, *b) == p.accent)
                            .map(|i| i as i32)
                            .unwrap_or(-1);
                        (
                            p.frame_on(state.settings.show_frame),
                            p.dot_on(state.settings.show_dot),
                            cur,
                        )
                    }
                    None => (false, false, -1),
                };
                app.set_ctx_frame(frame_on);
                app.set_ctx_dot(dot_on);
                app.set_ctx_cur_swatch(cur_swatch);
                // Move-to-Tab destinations = every tab other than the pane's (active) tab.
                let tabs: Vec<CtxTab> = state
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != state.active)
                    .map(|(i, t)| CtxTab {
                        label: if t.title.is_empty() {
                            "workspace".into()
                        } else {
                            t.title.clone()
                        },
                        idx: i as i32,
                    })
                    .collect();
                sync_model(&ui.ctx_tabs, tabs);
                sync_model(&ui.ctx_layouts, Vec::new());
                sync_model(&ui.ctx_openwith, Vec::new());
            }
            CtxKind::Tab => {
                // Layout submenu reflects the TARGET tab's layout (checkmark on current).
                let cur = state
                    .tabs
                    .get(c.target)
                    .map(|t| t.layout)
                    .unwrap_or_else(|| state.active_tab().layout);
                let layouts: Vec<LayoutOption> = theme::LAYOUT_MENU
                    .iter()
                    .map(|l| LayoutOption {
                        id: theme::layout_id(*l),
                        label: theme::layout_label(*l).as_ref().into(),
                        active: *l == cur,
                        hint: SharedString::new(),
                    })
                    .collect();
                sync_model(&ui.ctx_layouts, layouts);
                sync_model(&ui.ctx_swatches, Vec::new());
                sync_model(&ui.ctx_tabs, Vec::new());
                sync_model(&ui.ctx_openwith, Vec::new());
            }
            CtxKind::App => {
                // The application menu's Layout submenu: Automatic + the 5 presets, radio ✓ on
                // the active tab's current layout, with the Automatic row carrying a live
                // "— <resolved>" hint of what auto tiles as right now (by pane count).
                let cur = state.active_tab().layout;
                let n = state.active_tab().panes.len();
                let auto_resolved = effective_layout(Layout::Auto, n);
                let layouts: Vec<LayoutOption> = theme::LAYOUT_MENU
                    .iter()
                    .map(|l| LayoutOption {
                        id: theme::layout_id(*l),
                        label: theme::layout_label(*l).as_ref().into(),
                        active: *l == cur,
                        hint: if *l == Layout::Auto {
                            format!("— {}", theme::layout_name(auto_resolved)).into()
                        } else {
                            SharedString::new()
                        },
                    })
                    .collect();
                sync_model(&ui.ctx_layouts, layouts);
                sync_model(&ui.ctx_swatches, Vec::new());
                sync_model(&ui.ctx_tabs, Vec::new());
                sync_model(&ui.ctx_openwith, Vec::new());
            }
            // Every visible row of the file menu carries its own command, and its one
            // submenu — "Open With" — is a plain list of labels rather than a radio set or
            // a swatch grid, so the other three auxiliary models are cleared.
            CtxKind::File => {
                sync_model(&ui.ctx_layouts, Vec::new());
                sync_model(&ui.ctx_swatches, Vec::new());
                sync_model(&ui.ctx_tabs, Vec::new());
                sync_model(&ui.ctx_openwith, c.openwith.clone());
            }
        }
    }
}

/// What one [`pump`] pass did, surfaced to the app's adaptive idle cadence (#3) + perf log
/// (#1). `rendered` is the number of pane surfaces repainted (for throughput accounting);
/// `active` is whether the pass did work that warrants staying at the FAST cadence.
pub struct PumpResult {
    pub rendered: usize,
    pub active: bool,
}

/// One UI-thread render tick for a **single window**: resync if dirty, blink the
/// cursor, render dirty visible panes, refresh the HUD. Session output is drained
/// centrally (see [`crate::app::App::tick`]) and fed into panes before this runs, so the
/// pump no longer touches the event channel — that's what lets one engine feed N windows.
///
/// Returns a [`PumpResult`]: how many panes repainted, and whether anything *active* happened
/// (streamed content repainted, a glow/toast/typewriter animation advanced, or the prefs
/// preview is animating). A bare cursor-blink repaint is deliberately NOT "active", so an
/// otherwise-idle window lets the pump settle to the slow cadence (the blink still toggles
/// fine at the ~31 Hz idle rate).
#[tracing::instrument(level = "debug", skip_all)]
pub fn pump(
    app: &AppWindow,
    state: &mut State,
    ui: &Ui,
    area: (f32, f32),
    scale: f32,
    mgr: &SessionManager,
) -> PumpResult {
    // ---- fold in sidebar history-UI edits (#25/#26) ----
    // The Worktrees|History segmented bar + the history search box write each edit into the
    // `HistoryUi` global (a seq bump carrying the project row + its segment/query pair) —
    // a polled side-channel instead of a new app-level callback. Consume it here and dirty
    // the state so the resync below re-projects with the new filter/segment.
    {
        use slint::ComponentHandle as _;
        let hist = app.global::<crate::HistoryUi>();
        if crate::sidebar::history_seq_changed(hist.get_seq()) {
            let proj = hist.get_proj();
            if proj >= 0 {
                if let Some(p) = state.projects.get(proj as usize) {
                    crate::sidebar::set_history_ui(&p.path, hist.get_segment(), &hist.get_query());
                    state.dirty = true;
                }
            }
        }
    }

    // ---- fold in finished background sidebar scans (#6) ----
    // Worktree enumerations + Claude session scans run on the `history_scan` thread; the
    // projection only ever reads the sidebar caches. When a finished scan lands, dirty the
    // state so the resync below re-projects the flyout with the fresh rows.
    if crate::history_scan::drain() {
        state.dirty = true;
    }

    // ---- keep the left panel's liveness dots honest (M5) ----
    // A dot's brightness fades with WALL-CLOCK time, and nothing else dirties the state when
    // the workspace is quiet — the dots would freeze at whatever the last resync projected.
    // While the panel is open, a ~1s heartbeat re-runs the projection (the same "dirty →
    // resync" path everything else uses); closed, this costs one bool test per tick.
    if state.left_panel_open
        && crate::leftpanel::heartbeat_due(&mut state.left_panel_beat, Instant::now())
    {
        state.dirty = true;
    }

    // ---- keep the left panel anchored on the SELECTED pane (K) ----
    // A module surface on the rail describes the pane you are looking at, and focus
    // moves from a dozen places (clicks, drags, navigation, closing a pane, restoring a
    // workspace) — instrumenting every one of them would be a list that goes stale. One
    // guarded check per tick is the deterministic version: `sync_left_root` compares the
    // anchor cwd first and does nothing at all when it has not moved, so the quiet tick
    // costs a string compare and reads no filesystem. Closed panel, not even that.
    if state.left_panel_open {
        state.sync_left_root();
    }

    // ---- keep the KEYBOARD on the selected pane (L) ----
    // Only the pane header's own TouchAreas call `tp.focus()`. Selecting a pane any other
    // way — clicking its tab, Ctrl+Tab, closing its neighbour — left the focus ring on the
    // new pane and the keystrokes on the old one. Same shape as the left-panel anchor
    // above: one compare per tick, and it does nothing at all until the selection moves.
    state.sync_pane_keyboard_focus();

    // ---- expire a held-Esc once auto-repeat stops (no key-release event) ----
    state.tick_esc();

    // ---- apply pending font reloads (we own the DPI scale here) ----
    // A DPI scale change (or a font-family / base-size pref change → `font_reload`) reloads
    // the base font + every pane's font at its own per-pane `font_px`.
    if state.font_reload || (scale - state.last_scale).abs() > 1e-3 {
        state.reload_font(scale);
    }
    // Per-pane zoom: reload any focused-pane Ctrl± change (marked via `font_dirty`) at the
    // current scale BEFORE resync, so relayout re-grids that pane at its new cell metrics.
    {
        let path = state.settings.font_path();
        for t in &mut state.tabs {
            for p in &mut t.panes {
                if p.font_dirty {
                    p.font = theme::load_font_at(&path, p.font_px, scale);
                    p.font_dirty = false;
                    p.applied = (0, 0); // force a reflow + repaint at the new cell size
                }
            }
        }
    }
    state.last_scale = scale;

    // Whether this pass warrants staying at the fast cadence (#3). A structural resync, a
    // content repaint, an animation step, or the live prefs preview all count; a bare cursor
    // blink does not (handled in the per-pane loop below).
    let mut active = false;

    // ---- a reveal's scroll, held open for a few frames ----
    // The clock lives here rather than in `resync` so that closing the panel mid-window ends
    // the hold like any other frame does, instead of leaving the pump dirty forever.
    if let Some(since) = state.rail_scroll_hold {
        if Instant::now().duration_since(since) < RAIL_SCROLL_HOLD {
            state.rail.bump_scroll();
            state.dirty = true;
        } else {
            state.rail_scroll_hold = None;
        }
    }

    // ---- resync models when state changed ----
    if state.dirty {
        resync(state, app, ui, area, scale, mgr);
        state.dirty = false;
        active = true;
    }

    // Tell the ptys about any grid size that has stopped moving. Outside the `dirty` gate on
    // purpose: the last layout pass of a resize is the one that leaves a size pending, and it
    // is also the one after which nothing dirties the state again.
    flush_pty_resizes(state, mgr, Instant::now());

    // ---- cursor blink (~530 ms) ----
    let blink_changed = if state.last_blink.elapsed() >= Duration::from_millis(530) {
        state.cursor_on = !state.cursor_on;
        state.last_blink = Instant::now();
        true
    } else {
        false
    };
    let opts = RenderOpts {
        cursor_on: state.cursor_on,
    };

    // ---- render the appearance preview (a real, locked terminal) while Prefs is open ----
    // Caret blinks in sync with the panes' cursor.
    if state.overlay == Overlay::Prefs {
        // Advance the ambient Tetris animation, then re-render the preview (no caret — it's
        // an animation, not a prompt). The preview animates continuously, so keep fast cadence.
        active = true;
        state.animate_preview_tetris();
        if let Some(img) = state.render_preview(scale, false) {
            app.set_pref_preview_surface(img);
        }
        // Animate the idle-glow demo for the AI-features preview: always "idle" so the
        // selected effect plays continuously (the .slint only shows it on the AI tab).
        let eff = crate::glow::IdleEffect::from_token(&state.settings.idle_effect);
        let a = state.preview_glow.update(eff, true, Instant::now());
        app.set_pref_preview_glow(a);

        // ---- Tools page ----
        // The filesystem scan is one-shot per window (see `tool_detected`); the rows
        // themselves are rebuilt each tick, which is pure map lookups.
        {
            let mut cache = ui.tool_detected.borrow_mut();
            let detected = cache.get_or_insert_with(|| {
                let none = std::collections::BTreeMap::new();
                avada_core::tools::detect::resolve_all(&none)
                    .into_iter()
                    .map(|(id, r)| (id, r.path.display().to_string()))
                    .collect()
            });
            let rows: Vec<PrefToolRow> = avada_core::tools::TOOLS
                .iter()
                .map(|t| PrefToolRow {
                    id: t.id.into(),
                    name: t.name.into(),
                    icon: t.icon as i32,
                    brand: Color::from_rgb_u8(t.brand.0, t.brand.1, t.brand.2),
                    favorite: state.settings.is_favorite_tool(t.id),
                    detected: detected.get(t.id).map(|s| s.as_str()).unwrap_or("").into(),
                    r#override: state
                        .settings
                        .tool_paths
                        .get(t.id)
                        .map(|s| s.as_str())
                        .unwrap_or("")
                        .into(),
                })
                .collect();
            sync_model(&ui.pref_tools, rows);
        }

        // ---- Browser page ----
        {
            let mut cache = ui.browsers_found.borrow_mut();
            let found = cache.get_or_insert_with(avada_core::open::list_browsers);
            let rows: Vec<PrefBrowserRow> = found
                .iter()
                .map(|b| PrefBrowserRow {
                    id: b.id.as_str().into(),
                    name: b.name.as_str().into(),
                    active: b.id == state.settings.browser_app,
                })
                .collect();
            sync_model(&ui.pref_browsers, rows);
        }
        app.set_pref_browser_mode(match state.settings.browser_mode.as_str() {
            prefs::BROWSER_MODE_APP => 1,
            prefs::BROWSER_MODE_ASK => 2,
            _ => 0,
        });
    }

    // ---- idle-glow inputs (read once per tick) ----
    let idle_on = state.settings.idle_alert;
    let idle_effect = crate::glow::IdleEffect::from_token(&state.settings.idle_effect);
    let idle_threshold_ms = state.settings.idle_alert_seconds as u64 * 1000;
    let glow_now = Instant::now();
    let glow_now_ms = crate::glow::now_epoch_ms();

    // ---- render dirty (visible) panes of the active tab → model ----
    // Per-pane chrome inputs (read before the tab borrow): the global frame/dot prefs each
    // pane's override folds over, and which pane (if any) is being renamed inline.
    let show_frame = state.settings.show_frame;
    let show_dot = state.settings.show_dot;
    let editing_pane = state.editing_pane;
    let active_idx = state.active;
    let focused = state.tabs[active_idx].focused;
    // Same merge as `resync`, resolved before the `&mut state.tabs[..]` borrow below.
    let tool_row: Vec<(PaneKind, i32)> = state.tabs[active_idx]
        .panes
        .iter()
        .map(|p| (state.effective_kind(p), state.liveness_ui(&p.uid)))
        .collect();
    // The palette the projection was built against — see the same read in `resync`.
    // Taken before the mutable tab borrow below, which would otherwise block it.
    let view_ui = state.appearance_view().3;
    let tab = &mut state.tabs[active_idx];
    let n = tab.panes.len();
    let mut rendered = 0usize;
    for (i, tool) in tool_row.iter().enumerate().take(n) {
        let ps = &mut tab.panes[i];
        // Advance this pane's idle glow every tick. A pane is "idle" once it's been
        // output-quiet past the threshold (the agent finished + is waiting); the alpha
        // animates while idle and resets to 0 otherwise.
        let prev_glow = ps.glow.alpha;
        // Only AGENT panes glow — a plain quiet shell never does, matching the Electron
        // `isAiPane && idle` gate. "Is this an agent" now has two answers and either will
        // do: the pane's merged tool identity (authoritative for a pane spawned AS a tool,
        // which may never print a title at all) or the title sniff (which catches the
        // agents that do). Before the first, a `avada claude` pane sat there quiet and
        // unglowing because nothing had written a title for it to match.
        let is_agent =
            !matches!(tool.0, PaneKind::Terminal) || crate::glow::is_ai_pane(&ps.shell_title);
        let idle = idle_on
            && ps.visible
            && is_agent
            && match mgr.last_output_at(&ps.uid) {
                Some(ms) => glow_now_ms.saturating_sub(ms) >= idle_threshold_ms,
                None => false,
            };
        ps.glow.update(idle_effect, idle, glow_now);
        let glow_changed = (ps.glow.alpha - prev_glow).abs() > 0.004;

        if !ps.visible {
            let _ = ps.pane.take_dirty();
            continue;
        }
        // Edge-autoscroll: if a selection drag is held at the top/bottom edge, scroll one line and
        // grow the selection into off-screen scrollback. It marks the grid dirty (so the surface
        // re-renders below) and warrants the fast cadence while it's moving.
        if ps.pane.selection_autoscroll_tick() {
            active = true;
        }
        // Poll the transient bottom-right indicator each tick so it appears + auto-expires
        // (copy/paste confirmations + the Ctrl-zoom font %). A change alone refreshes the row.
        let toast = ps.pane.toast_text().unwrap_or_default();
        let toast_changed = ps.last_toast != toast;
        if toast_changed {
            ps.last_toast = toast;
        }
        // ---- ambient-AI subtitle typewriter reveal ----
        // Advance the reveal cursor toward the full summary length, but only while the AI line
        // is actually shown (no manual subtitle, not muted). A change re-pushes the row.
        let manual_subtitle = ps.subtitle.as_ref().is_some_and(|s| !s.is_empty());
        let ai_target = if manual_subtitle || ps.ai_muted {
            0
        } else {
            ps.ai.len
        };
        let ai_changed = if ai_target > 0 && (ps.ai.reveal as usize) < ai_target {
            ps.ai.reveal = (ps.ai.reveal + AI_REVEAL_PER_TICK).min(ai_target as f32);
            true
        } else {
            false
        };
        // Vim scrollbar fade: while the bar is showing/fading its opacity changes every tick, so
        // re-push the row and stay fast until it disappears (then one final clearing push).
        let bar_on = ps.pane.scrollbar(ps.surf.1).is_some();
        let scrollbar_changed = bar_on || ps.scrollbar_on;
        ps.scrollbar_on = bar_on;
        let focus_blink = i == focused && blink_changed;
        let pane_dirty = ps.pane.take_dirty();
        // Repaint the surface only for terminal/cursor changes; a glow-only, toast-only or
        // typewriter-only change just re-pushes the (unchanged) surface with the new line.
        if pane_dirty || focus_blink {
            ps.surface = ps.pane.render(&mut ps.font, &opts);
            rendered += 1;
            // Real content (terminal output / cursor move) keeps the fast cadence; a bare
            // cursor-blink flip (focus_blink with no pane_dirty) does NOT — it must stay
            // possible to settle to the idle cadence while a pane just blinks.
            if pane_dirty {
                active = true;
            }
        } else if !glow_changed && !toast_changed && !ai_changed && !scrollbar_changed {
            continue;
        }
        // An advancing glow/toast/typewriter/scrollbar animation also warrants the fast cadence.
        if glow_changed || toast_changed || ai_changed || scrollbar_changed {
            active = true;
        }
        if i < ui.panes.row_count() {
            ui.panes.set_row_data(
                i,
                pane_item(
                    ps,
                    i == focused,
                    i as i32 == editing_pane,
                    show_frame,
                    show_dot,
                    ps.font_px,
                    &tool.0,
                    tool.1,
                    view_ui,
                ),
            );
        }
    }
    if rendered > 0 {
        state.frames += 1;
    }

    // ---- HUD ----
    if state.last_hud.elapsed() >= Duration::from_millis(500) {
        let fps = state.frames as f32 / state.last_hud.elapsed().as_secs_f32();
        let t = state.active_tab();
        app.set_hud(
            format!(
                "{} · {} panes · {:.0} fps",
                theme::layout_name(t.layout),
                t.panes.len(),
                fps
            )
            .into(),
        );
        state.frames = 0;
        state.last_hud = Instant::now();
    }

    PumpResult { rendered, active }
}

#[cfg(test)]
mod pty_resize_tests {
    //! The pty is told about a grid size only once that size has stopped moving.
    //!
    //! Every SIGWINCH costs the user scrollback: a shell answers one by repositioning its
    //! prompt and erasing to the end of the display. Startup passes through at least one
    //! intermediate size (the window opens at its preferred size before the OS restores the
    //! saved frame), and pushing those cost a re-attached pane the output of the last command
    //! it ran — which is precisely what a restart-to-see-changes is supposed to preserve.
    use super::*;
    use crate::state::DetachedPane;
    use avada_core::tools::PaneKind;

    fn fresh() -> State {
        State::new(crate::theme::load_font(1.0))
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: "t".into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    /// A size that is still moving is never pushed; the one it settles on is pushed once.
    #[test]
    fn only_a_settled_grid_size_reaches_the_pty() {
        let mut st = fresh();
        st.adopt_pane(&mgr(), det("a"));
        let start = Instant::now();
        {
            let p = &mut st.active_tab_mut().panes[0];
            p.pty = (47, 18); // what the session already has
            p.applied = (100, 30); // an intermediate layout pass
            p.pty_since = start;
        }
        let mut sent: Vec<(String, u16, u16)> = Vec::new();

        // Still inside the settle window: nothing goes out, however many ticks run.
        flush_pty_resizes_to(
            &mut st,
            start + PTY_RESIZE_SETTLE - Duration::from_millis(1),
            |u, c, r| {
                sent.push((u.into(), c, r));
                true
            },
        );
        assert!(
            sent.is_empty(),
            "an intermediate size must not reach the shell"
        );

        // The layout moves again, back to what the pty already has — the clock restarts and
        // the push that was about to fire is now a no-op, which is the startup case exactly.
        {
            let p = &mut st.active_tab_mut().panes[0];
            p.applied = (47, 18);
            p.pty_since = start + Duration::from_millis(50);
        }
        flush_pty_resizes_to(&mut st, start + Duration::from_secs(5), |u, c, r| {
            sent.push((u.into(), c, r));
            true
        });
        assert!(
            sent.is_empty(),
            "settling back on the size the pty already has costs zero SIGWINCH"
        );

        // A real, settled change does go out — once.
        {
            let p = &mut st.active_tab_mut().panes[0];
            p.applied = (60, 20);
            p.pty_since = start + Duration::from_secs(5);
        }
        flush_pty_resizes_to(&mut st, start + Duration::from_secs(10), |u, c, r| {
            sent.push((u.into(), c, r));
            true
        });
        assert_eq!(sent, vec![("a".to_string(), 60, 20)]);
        assert_eq!(st.active_tab().panes[0].pty, (60, 20));
    }

    /// A resize that went nowhere must stay pending and be retried, not be recorded as
    /// delivered.
    ///
    /// This is the mouse-wheel bug. A control-spawned pane is adopted and laid out a tick or
    /// two after `create`, before the daemon has confirmed the session — and the daemon's
    /// `Resize` arm silently no-ops on a uid it does not know yet. Committing `p.pty` anyway
    /// meant the pane never asked again: its pty stayed at the 80x24 spawn size for life, the
    /// program painted in the top-left corner of a much larger grid, its cursor never reached
    /// the last row, nothing was ever pushed into scrollback, and the wheel had no history to
    /// scroll (and no scrollbar to show).
    #[test]
    fn a_dropped_resize_is_retried_rather_than_recorded_as_delivered() {
        let mut st = fresh();
        st.adopt_pane(&mgr(), det("a"));
        let start = Instant::now();
        {
            let p = &mut st.active_tab_mut().panes[0];
            p.pty = (80, 24); // the spawn default the pty is stuck at
            p.applied = (96, 23);
            p.pty_since = start;
        }
        let settled = start + PTY_RESIZE_SETTLE;

        let mut attempts = 0;
        flush_pty_resizes_to(&mut st, settled, |_, _, _| {
            attempts += 1;
            false // the daemon has not registered the session yet
        });
        assert_eq!(attempts, 1);
        assert_eq!(
            st.active_tab().panes[0].pty,
            (80, 24),
            "a resize the session dropped must not be booked as delivered"
        );

        // Next tick, with the session now confirmed. `pty_since` is already past the settle
        // window, so the retry needs no further wait.
        flush_pty_resizes_to(&mut st, settled, |_, _, _| {
            attempts += 1;
            true
        });
        assert_eq!(attempts, 2, "the dropped resize is tried again");
        assert_eq!(st.active_tab().panes[0].pty, (96, 23));

        // And once it has landed, it stops being sent.
        flush_pty_resizes_to(&mut st, settled + Duration::from_secs(5), |_, _, _| {
            attempts += 1;
            true
        });
        assert_eq!(attempts, 2, "a settled, delivered size is not resent");
    }

    /// A pane the pump has never laid out sits at (0, 0). That is "no size yet", not a grid,
    /// and handing it to a pty would be a lie rather than a resize.
    #[test]
    fn an_unlaid_out_pane_is_never_reported() {
        let mut st = fresh();
        st.adopt_pane(&mgr(), det("a"));
        let start = Instant::now();
        {
            let p = &mut st.active_tab_mut().panes[0];
            p.pty = (47, 18);
            p.applied = (0, 0);
            p.pty_since = start;
        }
        let mut sent = 0;
        flush_pty_resizes_to(&mut st, start + Duration::from_secs(10), |_, _, _| {
            sent += 1;
            true
        });
        assert_eq!(sent, 0);
        assert_eq!(st.active_tab().panes[0].pty, (47, 18));
    }
}

#[cfg(test)]
mod image_arm_tests {
    //! Track V3: the `PaneKind::Image` arm of `pane_item`. A picture is not a row list,
    //! so the arm hands the `ViewPane` nothing and projects the texture through
    //! `imagepane` instead — fall through to the generic view arm and the pane would
    //! list a "not available" NOTICE and never decode the file.
    use super::*;
    use crate::imagepane::testpng::{encode, TempFile};
    use crate::state::DetachedPane;
    use avada_core::tools::PaneKind;

    fn fresh() -> State {
        State::new(crate::theme::load_font(1.0))
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn image_pane(uid: &str, target: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: "t".into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::Image,
            tool_session: None,
            cwd: Some(target.into()),
        }
    }

    #[test]
    fn an_image_pane_projects_a_texture_and_no_rows() {
        let f = TempFile::write("cat.png", &encode(6, 4));
        let mut st = fresh();
        st.adopt_pane(&mgr(), image_pane("img-arm", f.path()));
        let ps = &st.active_tab().panes[0];
        let item = pane_item(ps, true, false, false, false, 14.0, &PaneKind::Image, 0, 0);
        assert!(item.is_view, "an image pane is a Family B view");
        assert_eq!(item.kind, PaneKind::Image.ui_kind());
        assert_eq!(
            item.view_rows.row_count(),
            0,
            "the row list must stay empty: the picture is not a row"
        );
        assert!(
            item.view_title.ends_with("/cat.png"),
            "the crumb is the view title: {}",
            item.view_title
        );
        let row = crate::imagepane::row("img-arm").expect("the arm decoded the file");
        assert!(row.ok, "{}", row.error);
        assert_eq!((row.w, row.h), (6, 4));
        assert!(crate::imagepane::forget("img-arm"));
    }

    fn module_image_pane(uid: &str, surface: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: "t".into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::Module(
                ModulePaneRef::new("bshuler/avada-image", surface, None).expect("a valid pane ref"),
            ),
            tool_session: None,
            cwd: None,
        }
    }

    /// Track V3 × H4: a module surface that has shipped a picture draws as an image, not a
    /// grid. The same `has_image` flag the built-in `PaneKind::Image` sets is set here, the
    /// texture goes through `imagepane`'s per-uid model, and — the point of the whole split —
    /// the grid/placeholder path is skipped so the picture never draws over its own scaffold.
    #[test]
    fn a_module_that_ships_a_picture_draws_it_and_not_a_grid() {
        use avada_core::module::host::ImageData;
        let module = avada_core::rights::ModuleId::new("bshuler/avada-image").expect("a valid id");
        // The module hands over finished RGBA: a 2×2 all-red frame.
        crate::module_ui::image::set(
            &module,
            ImageData {
                surface: "image".into(),
                name: "cat.png".into(),
                width: 2,
                height: 2,
                format: "PNG".into(),
                bytes: 64,
                rgba: [255, 0, 0, 255].repeat(4),
                error: String::new(),
            },
        );

        let mut st = fresh();
        st.adopt_pane(&mgr(), module_image_pane("mod-img", "image"));
        let ps = &st.active_tab().panes[0];
        let kind = ps.kind.clone();
        let item = pane_item(ps, true, false, false, false, 14.0, &kind, 0, 0);

        assert!(item.has_image, "a module that shipped pixels is a texture pane");
        assert_eq!(
            item.kind,
            kind.ui_kind(),
            "it is still a module pane (kind 10), not the built-in image kind"
        );
        assert_eq!(
            item.view_rows.row_count(),
            0,
            "a picture is not a row list, and a module image has no grid rows either"
        );
        let row = crate::imagepane::row("mod-img").expect("the module image projected a texture");
        assert!(row.ok, "{}", row.error);
        assert_eq!((row.w, row.h), (2, 2));

        // A surface that never shipped a picture is a grid pane, not a texture pane — the
        // discriminator is the store's generation, exactly as documented in `pane_item`.
        st.adopt_pane(&mgr(), module_image_pane("mod-grid", "blank"));
        let ps2 = &st.active_tab().panes[1];
        let kind2 = ps2.kind.clone();
        let grid_item = pane_item(ps2, true, false, false, false, 14.0, &kind2, 0, 0);
        assert!(
            !grid_item.has_image,
            "a module surface with no picture is a grid pane, never a texture pane"
        );

        crate::module_ui::image::forget(&module);
        assert!(crate::imagepane::forget("mod-img"));
    }
}

#[cfg(test)]
mod grid_resize_tests {
    //! A tier-5 surface is told its cell size on the same clock the pty gets, and by a
    //! function that deliberately remembers nothing.
    use super::*;
    use crate::state::DetachedPane;
    use avada_core::tools::PaneKind;

    fn fresh() -> State {
        State::new(crate::theme::load_font(1.0))
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str, kind: PaneKind) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: "t".into(),
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
        }
    }

    fn module_pane(surface: &str) -> PaneKind {
        PaneKind::Module(
            ModulePaneRef::new("bshuler/avada-editor", surface, None).expect("a valid pane ref"),
        )
    }

    /// Sets pane 0's layout size and restarts its settle clock.
    fn lay_out(st: &mut State, cols: usize, rows: usize, at: Instant) {
        let p = &mut st.active_tab_mut().panes[0];
        p.applied = (cols, rows);
        p.pty_since = at;
    }

    fn flush(st: &State, now: Instant) -> Vec<(String, u16, u16)> {
        let mut sent = Vec::new();
        flush_grid_resizes(st, now, |m, c, r| sent.push((m.surface.clone(), c, r)));
        sent
    }

    /// The size a surface is still growing into is not a size, and (0, 0) is not one either.
    #[test]
    fn a_surface_hears_only_a_settled_size_and_never_a_degenerate_one() {
        let mut st = fresh();
        st.adopt_pane(&mgr(), det("a", module_pane("editor")));
        let start = Instant::now();
        lay_out(&mut st, 100, 30, start);

        assert!(
            flush(&st, start + PTY_RESIZE_SETTLE - Duration::from_millis(1)).is_empty(),
            "a size still moving is withheld for the same reason the pty withholds it"
        );
        assert_eq!(
            flush(&st, start + PTY_RESIZE_SETTLE),
            vec![("editor".to_string(), 100, 30)],
            "the instant it settles it goes out"
        );

        for degenerate in [(0, 0), (1, 30), (100, 0)] {
            lay_out(&mut st, degenerate.0, degenerate.1, start);
            assert!(
                flush(&st, start + Duration::from_secs(10)).is_empty(),
                "{degenerate:?} is 'not laid out yet', not a grid"
            );
        }
    }

    /// It re-offers rather than deduplicating, because the memory lives in the runtime.
    ///
    /// If this ever starts firing once, `ModuleRuntime::grid_resize`'s repeat-dropping is
    /// being duplicated here — and the two copies would disagree the moment a pane is closed
    /// and reopened at the same size, where the runtime deliberately stays quiet.
    #[test]
    fn the_same_settled_size_is_offered_again_on_every_tick() {
        let mut st = fresh();
        st.adopt_pane(&mgr(), det("a", module_pane("editor")));
        let start = Instant::now();
        lay_out(&mut st, 80, 24, start);
        let settled = start + PTY_RESIZE_SETTLE;

        assert_eq!(flush(&st, settled), vec![("editor".to_string(), 80, 24)]);
        assert_eq!(
            flush(&st, settled + Duration::from_secs(1)),
            vec![("editor".to_string(), 80, 24)],
            "stateless by design — the runtime is what remembers"
        );
    }

    /// Only tier-5 panes are offered anything; a terminal beside one is not a surface.
    #[test]
    fn every_module_pane_is_offered_its_own_size_and_no_other_pane_is() {
        let mut st = fresh();
        let start = Instant::now();
        for (uid, kind) in [
            ("a", module_pane("editor")),
            ("b", PaneKind::default()),
            ("c", module_pane("diff")),
            ("d", PaneKind::Markdown),
        ] {
            st.adopt_pane(&mgr(), det(uid, kind));
        }
        for (i, size) in [(90, 40), (91, 41), (92, 42), (93, 43)]
            .into_iter()
            .enumerate()
        {
            let p = &mut st.active_tab_mut().panes[i];
            p.applied = size;
            p.pty_since = start;
        }

        assert_eq!(
            flush(&st, start + Duration::from_secs(1)),
            vec![("editor".to_string(), 90, 40), ("diff".to_string(), 92, 42)],
            "the terminal and the markdown pane between them contribute nothing"
        );
    }
}
