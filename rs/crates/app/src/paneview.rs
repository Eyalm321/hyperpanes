//! The view bridge: turns the central [`State`] into the Slint models and drives
//! the per-frame pump. This is the *resync* half of the Seam #1 contract — it
//! only ever reads `State` (plus the relayout it performs on the active tab) and
//! writes the UI models; it never decides workspace policy.

use std::time::{Duration, Instant};

use hyperpanes_core::layout::presets::{
    compute_tiles, effective_layout, DividerKind, Layout, Orientation,
};
use hyperpanes_core::session_manager::SessionManager;
use hyperpanes_terminal_widget::{cells_for_px, RenderOpts};

use slint::{Color, Model, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::contextmenu::CtxKind;
use crate::state::{Overlay, PaneState, State};
use crate::theme;
use crate::{
    AppWindow, CtxTab, DividerItem, FramePaletteOption, HiRect, KeybindingItem, LayoutOption,
    MenuEntry, PaletteItem, PaneItem, PrefOption, ProjectItem, TabItem, WorktreeRow,
};
use crate::prefs;

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
    pub families: Rc<VecModel<PrefOption>>,
    pub palettes: Rc<VecModel<FramePaletteOption>>,
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
    pub ctx_layouts: Rc<VecModel<LayoutOption>>,
    // ---- sidebar worktree subtrees ----
    /// Per-project worktree models, keyed by repo path and reused across ticks so each
    /// `ProjectItem.worktrees` keeps a STABLE model identity. Without this the projection's
    /// per-tick rebuild would recreate the worktree rows every frame, dropping in-flight
    /// clicks on the trash icons. Pruned to the live project set each resync.
    pub wt_models: RefCell<HashMap<String, Rc<VecModel<WorktreeRow>>>>,
}

impl Ui {
    /// A fresh, empty model set for one window.
    pub fn new() -> Rc<Ui> {
        Rc::new(Ui {
            panes: Rc::new(VecModel::default()),
            tabs: Rc::new(VecModel::default()),
            dividers: Rc::new(VecModel::default()),
            layouts: Rc::new(VecModel::default()),
            palette: Rc::new(VecModel::default()),
            projects: Rc::new(VecModel::default()),
            families: Rc::new(VecModel::default()),
            palettes: Rc::new(VecModel::default()),
            shells: Rc::new(VecModel::default()),
            themes: Rc::new(VecModel::default()),
            idle_effects: Rc::new(VecModel::default()),
            keybindings: Rc::new(VecModel::default()),
            np_swatches: Rc::new(VecModel::default()),
            np_shells: Rc::new(VecModel::default()),
            ctx_entries: Rc::new(VecModel::default()),
            ctx_swatches: Rc::new(VecModel::default()),
            ctx_tabs: Rc::new(VecModel::default()),
            ctx_layouts: Rc::new(VecModel::default()),
            wt_models: RefCell::new(HashMap::new()),
        })
    }

    /// Bind this window's models to its `AppWindow` instance.
    pub fn attach(&self, app: &AppWindow) {
        app.set_panes(ModelRc::from(self.panes.clone()));
        app.set_tabs(ModelRc::from(self.tabs.clone()));
        app.set_dividers(ModelRc::from(self.dividers.clone()));
        app.set_layouts(ModelRc::from(self.layouts.clone()));
        app.set_palette(ModelRc::from(self.palette.clone()));
        app.set_projects(ModelRc::from(self.projects.clone()));
        app.set_pref_families(ModelRc::from(self.families.clone()));
        app.set_pref_palettes(ModelRc::from(self.palettes.clone()));
        app.set_pref_shells(ModelRc::from(self.shells.clone()));
        app.set_pref_themes(ModelRc::from(self.themes.clone()));
        app.set_pref_idle_effects(ModelRc::from(self.idle_effects.clone()));
        app.set_pref_keybindings(ModelRc::from(self.keybindings.clone()));
        app.set_np_swatches(ModelRc::from(self.np_swatches.clone()));
        app.set_np_shells(ModelRc::from(self.np_shells.clone()));
        app.set_ctx_entries(ModelRc::from(self.ctx_entries.clone()));
        app.set_ctx_swatches(ModelRc::from(self.ctx_swatches.clone()));
        app.set_ctx_tabs(ModelRc::from(self.ctx_tabs.clone()));
        app.set_ctx_layouts(ModelRc::from(self.ctx_layouts.clone()));
    }
}

/// Push `items` into `model`, reusing the existing elements when the row count is
/// unchanged (`set_row_data`) and only rebuilding (`set_vec`) when it differs.
/// Reuse is essential: `set_vec` destroys + recreates the repeated Slint elements,
/// which would drop a divider's pointer grab mid-drag and reset pane focus.
fn sync_model<T: Clone + 'static>(model: &VecModel<T>, items: Vec<T>) {
    if model.row_count() == items.len() {
        for (i, it) in items.into_iter().enumerate() {
            model.set_row_data(i, it);
        }
    } else {
        model.set_vec(items);
    }
}

/// Build a model row for pane `i`. `editing` flags the pane whose label is being renamed
/// inline; `show_frame`/`show_dot` are the GLOBAL Appearance prefs, folded here over each
/// pane's per-pane override (a clean new pane resolves OFF, a git-project pane ON).
fn pane_item(
    ps: &PaneState,
    focused: bool,
    editing: bool,
    show_frame: bool,
    show_dot: bool,
    font_px: f32,
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
        None => (false, HiRect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }),
    };
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
        show_frame: ps.frame_on(show_frame),
        show_dot: ps.dot_on(show_dot),
        editing,
        accent: ps.accent,
        x,
        y,
        w,
        h,
        visible: ps.visible,
        focused,
        glow: ps.glow.alpha,
        link_visible: link.is_some(),
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
        search_rects: ModelRc::from(Rc::new(VecModel::from(search_rects))),
        search_active_on,
        search_active_rect,
        toast: ps.last_toast.clone().into(),
        // The live terminal font px (logical) — drives the widget's indicator scaling.
        font_px,
        // The native app drops a pane the moment its session exits, so a live pane is never
        // "exited"; the field exists for the taskbar's Electron-parity badge.
        exited: false,
    }
}

/// Recompute the active tab's pane rects (and reflow any pane whose pixel size
/// changed). Honors zoom (solo the zoomed pane full-area).
fn relayout_active(state: &mut State, area: (f32, f32), scale: f32, mgr: &SessionManager) {
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

    let place = |p: &mut PaneState, x: f32, y: f32, w: f32, h: f32| {
        // Inset each pane within its tile → the inter-pane gap + edge margin.
        let gx = x + PANE_GAP;
        let gy = y + PANE_GAP;
        let gw = (w - 2.0 * PANE_GAP).max(1.0);
        let gh = (h - 2.0 * PANE_GAP).max(1.0);
        p.rect = (gx, gy, gw, gh);
        p.visible = true;
        // The selection / link / search-highlight hit-test surface, set authoritatively here
        // every tick. Slint's `geometry-changed` is unreliable: it doesn't fire for a pane
        // created already at its final size (a freshly *launched* pane stays at surf (0,0), which
        // breaks hit-testing), and when adding a pane reflows its neighbours their surf can go
        // stale (highlights then drift against the old size). The pump always knows the exact
        // size, so we own it here. The widget's surface = the frame minus its insets: a 4px
        // x-inset and 30px vertically (26px header + 2px top + 2px bottom) — see paneview.slint
        // `tp` (matches the size `geometry-changed` reports, so this never fights it).
        p.surf = ((gw - 4.0).max(1.0), (gh - 30.0).max(1.0));
        // size the grid to the terminal body (frame chrome removed) so cells match. Each pane
        // uses its OWN font cell metrics (per-pane zoom), so panes can differ in cols/rows.
        let tw = (gw - PANE_CHROME_W).max(1.0);
        let th = (gh - PANE_CHROME_H).max(1.0);
        let (cols, rows) = cells_for_px(tw * scale, th * scale, p.font.cell_w, p.font.cell_h);
        if (cols, rows) != p.applied {
            if p.pane.resize(cols, rows) {
                mgr.resize(&p.uid, cols as u16, rows as u16);
            }
            p.applied = (cols, rows);
            // The grid rewrapped — recompute any open search so its highlights track the
            // reflowed text instead of drifting against stale match coordinates.
            p.pane.search_reflow();
        }
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

/// Pixel rects for the active tab's draggable dividers.
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
pub fn resync(state: &mut State, app: &AppWindow, ui: &Ui, area: (f32, f32), scale: f32, mgr: &SessionManager) {
    relayout_active(state, area, scale, mgr);

    // tab strip
    let active = state.active;
    let tabs: Vec<TabItem> = state
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabItem {
            title: t.title.clone(),
            active: i == active,
        })
        .collect();
    sync_model(&ui.tabs, tabs);

    // layout picker
    let cur = state.active_tab().layout;
    let layouts: Vec<LayoutOption> = theme::LAYOUT_MENU
        .iter()
        .map(|l| LayoutOption {
            id: theme::layout_id(*l),
            label: theme::layout_label(*l).into(),
            glyph: theme::layout_icon(*l).into(),
            active: *l == cur,
            hint: SharedString::new(),
        })
        .collect();
    sync_model(&ui.layouts, layouts);

    // panes
    let show_frame = state.settings.show_frame;
    let show_dot = state.settings.show_dot;
    let editing_pane = state.editing_pane;
    let t = state.active_tab();
    let focused = t.focused;
    let items: Vec<PaneItem> = t
        .panes
        .iter()
        .enumerate()
        .map(|(i, p)| pane_item(p, i == focused, i as i32 == editing_pane, show_frame, show_dot, p.font_px))
        .collect();
    sync_model(&ui.panes, items);

    // dividers
    let divs = build_dividers(state, area);
    crate::dbg_log(&format!(
        "resync: active={} layout={:?} panes={} dividers={} {:?}",
        state.active,
        state.active_tab().layout,
        state.active_tab().panes.len(),
        divs.len(),
        divs.iter()
            .map(|d| format!("(x={:.0},y={:.0},w={:.0},h={:.0},vert={})", d.x, d.y, d.w, d.h, d.vertical))
            .collect::<Vec<_>>()
    ));
    sync_model(&ui.dividers, divs);

    // scalars
    app.set_layout_glyph(theme::layout_glyph(cur).into());
    app.set_editing_tab(state.editing_tab);
    app.set_zoomed(state.active_tab().zoomed.is_some());
    app.set_fullscreen(state.fullscreen);
    app.set_esc_holding(state.esc_holding);
    // Terminal zoom factor of the FOCUSED pane (its font px / base) → scales the fullscreen
    // hint (fullscreen solos the focused pane), tracking that pane's per-pane zoom.
    let focused_px = {
        let t = state.active_tab();
        t.panes.get(t.focused).map(|p| p.font_px).unwrap_or(prefs::DEFAULT_FONT_PX)
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
    };
    app.set_overlay_kind(kind);

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
            label: if id == 0 { "Use default shell".into() } else { (*label).into() },
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

    // Appearance controls reflect the DRAFT while Preferences is open (so edits preview
    // without touching the live panes), else the committed settings.
    let (_view_font, view_palette, view_theme, view_px, view_frame, view_dot) =
        state.appearance_view();

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
            PrefOption { id: id as i32, label: (*label).into(), active }
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
            PrefOption { id: id as i32, label: (*label).into(), active }
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
            PrefOption { id: id as i32, label: (*label).into(), active }
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
            PrefOption { id: id as i32, label: (*label).into(), active }
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
    sync_model(&ui.keybindings, keybindings);
    app.set_pref_keybinds_overridden(state.keymap.any_overridden());
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
            // Reuse this project's worktree model (stable identity) and update its contents
            // in place, so the inner repeater isn't rebuilt every frame (see `wt_models`).
            let model = ui
                .wt_models
                .borrow_mut()
                .entry(proj.path.clone())
                .or_insert_with(|| Rc::new(VecModel::default()))
                .clone();
            sync_model(&model, rows);
            ProjectItem { name, color, worktrees: ModelRc::from(model) }
        })
        .collect();
    // Drop cached worktree models for projects no longer present.
    {
        let live: std::collections::HashSet<&str> =
            state.projects.iter().map(|p| p.path.as_str()).collect();
        ui.wt_models.borrow_mut().retain(|k, _| live.contains(k.as_str()));
    }
    sync_model(&ui.projects, projects);

    // ---- context menu (pane header / tab strip) ----
    let ctx_kind = match state.ctx.as_ref() {
        None => 0,
        Some(c) => match c.kind {
            CtxKind::Pane => 1,
            CtxKind::Tab => 2,
            CtxKind::App => 3,
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
                glyph: e.glyph.clone(),
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
                        label: theme::layout_label(*l).into(),
                        glyph: theme::layout_icon(*l).into(),
                        active: *l == cur,
                        hint: SharedString::new(),
                    })
                    .collect();
                sync_model(&ui.ctx_layouts, layouts);
                sync_model(&ui.ctx_swatches, Vec::new());
                sync_model(&ui.ctx_tabs, Vec::new());
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
                        label: theme::layout_label(*l).into(),
                        glyph: theme::layout_icon(*l).into(),
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
pub fn pump(
    app: &AppWindow,
    state: &mut State,
    ui: &Ui,
    area: (f32, f32),
    scale: f32,
    mgr: &SessionManager,
) -> PumpResult {
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

    // ---- resync models when state changed ----
    if state.dirty {
        resync(state, app, ui, area, scale, mgr);
        state.dirty = false;
        active = true;
    }

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
    let tab = &mut state.tabs[active_idx];
    let n = tab.panes.len();
    let mut rendered = 0usize;
    for i in 0..n {
        let ps = &mut tab.panes[i];
        // Advance this pane's idle glow every tick. A pane is "idle" once it's been
        // output-quiet past the threshold (the agent finished + is waiting); the alpha
        // animates while idle and resets to 0 otherwise.
        let prev_glow = ps.glow.alpha;
        // Only AGENT panes glow (an agent CLI sets the shell title) — a plain quiet shell
        // never does, matching the Electron `isAiPane && idle` gate.
        let idle = idle_on
            && ps.visible
            && crate::glow::is_ai_pane(&ps.shell_title)
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
        } else if !glow_changed && !toast_changed && !ai_changed {
            continue;
        }
        // An advancing glow/toast/typewriter animation also warrants the fast cadence.
        if glow_changed || toast_changed || ai_changed {
            active = true;
        }
        if i < ui.panes.row_count() {
            ui.panes.set_row_data(
                i,
                pane_item(ps, i == focused, i as i32 == editing_pane, show_frame, show_dot, ps.font_px),
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
