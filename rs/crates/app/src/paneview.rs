//! The view bridge: turns the central [`State`] into the Slint models and drives
//! the per-frame pump. This is the *resync* half of the Seam #1 contract — it
//! only ever reads `State` (plus the relayout it performs on the active tab) and
//! writes the UI models; it never decides workspace policy.

use std::time::{Duration, Instant};

use hyperpanes_core::layout::presets::{
    compute_tiles, effective_layout, DividerKind, Orientation,
};
use hyperpanes_core::session_manager::SessionManager;
use hyperpanes_terminal_widget::{cells_for_px, RenderOpts};

use slint::{Model, ModelRc, VecModel};
use std::rc::Rc;

use crate::state::{Overlay, PaneState, State};
use crate::theme;
use crate::{
    AppWindow, DividerItem, FramePaletteOption, LayoutOption, PaletteItem, PaneItem, PrefOption,
    ProjectItem, TabItem,
};
use crate::prefs;

/// Thickness (logical px) of the draggable divider hit-area.
const DIVIDER_THICK: f32 = 10.0;

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

/// Build a model row for pane `i`.
fn pane_item(ps: &PaneState, focused: bool) -> PaneItem {
    let (x, y, w, h) = ps.rect;
    // Project the clickable-path hover overlay (if any) into the model row.
    let (lx, ly) = ps.link_cursor;
    let link = ps.link.as_ref();
    PaneItem {
        surface: ps.surface.clone(),
        title: ps.title.clone(),
        accent: ps.accent,
        x,
        y,
        w,
        h,
        visible: ps.visible,
        focused,
        link_visible: link.is_some(),
        link_x: link.map(|l| l.x).unwrap_or(0.0),
        link_y: link.map(|l| l.y).unwrap_or(0.0),
        link_w: link.map(|l| l.w).unwrap_or(0.0),
        link_tip: link.map(|l| l.tip.clone()).unwrap_or_default().into(),
        link_tip_x: lx + 12.0,
        link_tip_y: ly + 16.0,
    }
}

/// Recompute the active tab's pane rects (and reflow any pane whose pixel size
/// changed). Honors zoom (solo the zoomed pane full-area).
fn relayout_active(state: &mut State, area: (f32, f32), scale: f32, mgr: &SessionManager) {
    let (aw, ah) = area;
    let cw = state.font.cell_w;
    let ch = state.font.cell_h;
    let active = state.active;
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
        // size the grid to the terminal body (frame chrome removed) so cells match.
        let tw = (gw - PANE_CHROME_W).max(1.0);
        let th = (gh - PANE_CHROME_H).max(1.0);
        let (cols, rows) = cells_for_px(tw * scale, th * scale, cw, ch);
        if (cols, rows) != p.applied {
            if p.pane.resize(cols, rows) {
                mgr.resize(&p.uid, cols as u16, rows as u16);
            }
            p.applied = (cols, rows);
        }
    };

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
            label: theme::layout_name(*l).into(),
            glyph: theme::layout_glyph(*l).into(),
            active: *l == cur,
        })
        .collect();
    sync_model(&ui.layouts, layouts);

    // panes
    let t = state.active_tab();
    let focused = t.focused;
    let items: Vec<PaneItem> = t
        .panes
        .iter()
        .enumerate()
        .map(|(i, p)| pane_item(p, i == focused))
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

    // ---- Wave-2 overlay projection ----
    let kind = match state.overlay {
        Overlay::None => 0,
        Overlay::Palette => 1,
        Overlay::Prefs => 2,
    };
    app.set_overlay_kind(kind);

    // command palette rows + selection
    let palette: Vec<PaletteItem> = state
        .palette_rows()
        .into_iter()
        .map(|(title, subtitle)| PaletteItem { title, subtitle })
        .collect();
    sync_model(&ui.palette, palette);
    app.set_palette_sel(state.palette_sel as i32);

    // preferences scalars + the installed font-family options (active = the resolved
    // current font path). The picker offers each font by index; selection maps back to its
    // path in the controller.
    let current_font = state.settings.font_path();
    let avail = prefs::available_families();
    let mut font_label = String::from("Default");
    let families: Vec<PrefOption> = avail
        .iter()
        .enumerate()
        .map(|(id, (label, path))| {
            let active = *path == current_font;
            if active {
                font_label = label.clone();
            }
            PrefOption { id: id as i32, label: label.clone().into(), active }
        })
        .collect();
    sync_model(&ui.families, families);
    app.set_pref_font_label(font_label.into());

    // frame-palette options (label + 8 slot color chips), active = current
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
                active: id == state.settings.frame_palette,
                colors: ModelRc::from(Rc::new(VecModel::from(colors))),
            }
        })
        .collect();
    sync_model(&ui.palettes, palettes);

    // default-shell options; active = the one whose token matches the saved setting.
    let shells: Vec<PrefOption> = prefs::SHELL_OPTIONS
        .iter()
        .enumerate()
        .map(|(id, (label, value))| PrefOption {
            id: id as i32,
            label: (*label).into(),
            active: *value == state.settings.default_shell,
        })
        .collect();
    sync_model(&ui.shells, shells);

    app.set_pref_fontpx(state.settings.font_px.round() as i32);
    app.set_show_frame(state.settings.show_frame);
    app.set_show_dot(state.settings.show_dot);
    app.set_pref_clickable(state.settings.clickable_paths);
    app.set_pref_editor(state.settings.editor_command.clone().into());

    // sidebar / projects: the rail gating + flyout state + rows
    app.set_show_sidebar(state.settings.show_sidebar);
    app.set_sidebar_open(state.sidebar_open);
    let projects: Vec<ProjectItem> = state
        .project_rows()
        .into_iter()
        .map(|(name, color)| ProjectItem { name, color })
        .collect();
    sync_model(&ui.projects, projects);
}

/// One UI-thread render tick for a **single window**: resync if dirty, blink the
/// cursor, render dirty visible panes, refresh the HUD. Session output is drained
/// centrally (see [`crate::app::App::tick`]) and fed into panes before this runs, so the
/// pump no longer touches the event channel — that's what lets one engine feed N windows.
pub fn pump(
    app: &AppWindow,
    state: &mut State,
    ui: &Ui,
    area: (f32, f32),
    scale: f32,
    mgr: &SessionManager,
) {
    // ---- expire a held-Esc once auto-repeat stops (no key-release event) ----
    state.tick_esc();

    // ---- apply a pending font reload (we own the DPI scale here) ----
    if state.font_reload {
        state.reload_font(scale);
    }

    // ---- resync models when state changed ----
    if state.dirty {
        resync(state, app, ui, area, scale, mgr);
        state.dirty = false;
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

    // ---- render dirty (visible) panes of the active tab → model ----
    let active = state.active;
    let focused = state.tabs[active].focused;
    let font = &mut state.font;
    let tab = &mut state.tabs[active];
    let n = tab.panes.len();
    let mut rendered = false;
    for i in 0..n {
        let ps = &mut tab.panes[i];
        if !ps.visible {
            let _ = ps.pane.take_dirty();
            continue;
        }
        let focus_blink = i == focused && blink_changed;
        if !ps.pane.take_dirty() && !focus_blink {
            continue;
        }
        ps.surface = ps.pane.render(font, &opts);
        if i < ui.panes.row_count() {
            ui.panes.set_row_data(i, pane_item(ps, i == focused));
        }
        rendered = true;
    }
    if rendered {
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
}
