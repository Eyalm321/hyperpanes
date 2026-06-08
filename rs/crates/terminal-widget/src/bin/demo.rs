//! Standalone demo for `hyperpanes-terminal-widget`: a window with two live
//! `TerminalPane`s, each bound to a real shell spawned through
//! `hyperpanes_core::session_manager`. This is how the widget is developed/verified in
//! isolation (like Spike A), and the reference wiring Wave-2's `app-shell` mirrors.
//!
//! What it demonstrates end-to-end:
//!   * a `SessionManager` session per pane (real conpty shell, NOT a private pty);
//!   * its batched `SessionEvent::Data` fed into the pane's grid, DSR/DA replies forwarded
//!     back via `SessionManager::write`;
//!   * Slint key events → `keys::encode_key` → `SessionManager::write` (type into a pane);
//!   * geometry changes → grid + session resize;
//!   * software (CPU `SharedPixelBuffer`) **and** GPU (`wgpu::Texture`) renderers behind
//!     the `PaneRenderer` trait — pane 0 GPU, pane 1 software by default.
//!
//! Flags: `--software` (both panes software) · `--gpu` (both GPU).

use hyperpanes_core::session_manager::{SessionEvent, SessionManager, SpawnOptions};
use hyperpanes_terminal_widget::ui::{DemoWindow, HiRect, KeyMsg, PaneVisual};
use hyperpanes_terminal_widget::{
    cells_for_px, encode_key, Font, GpuRenderer, LinkAction, PaneRenderer, RenderOpts,
    SoftwareRenderer, TerminalPane,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::unbounded_channel;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Gpu,
    Software,
}

struct PaneCtl {
    pane: TerminalPane,
    kind: Kind,
    started: bool,
    /// The command written once the shell first produces output (so it isn't dropped
    /// before conpty is ready). `None` once sent.
    startup: Option<String>,
}

struct State {
    font: Font,
    panes: Vec<PaneCtl>,
    /// Cell dims currently applied per pane (to detect a real reflow).
    applied: Vec<(usize, usize)>,
    last_blink: Instant,
    cursor_on: bool,
    last_hud: Instant,
    frames: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- flags ----
    let mut force: Option<Kind> = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--gpu" => force = Some(Kind::Gpu),
            "--software" | "--sw" => force = Some(Kind::Software),
            _ => {}
        }
    }

    // ---- Slint backend: try to get a shared wgpu 28 device (for the GPU renderer). ----
    // Falls back to software-only if wgpu isn't available on this host.
    let want_gpu_backend = force != Some(Kind::Software);
    let wgpu_selected = if want_gpu_backend {
        slint::BackendSelector::new()
            .require_wgpu_28(slint::wgpu_28::WGPUConfiguration::default())
            .select()
            .map_err(|e| eprintln!("[demo] wgpu-28 backend unavailable ({e}); software-only"))
            .is_ok()
    } else {
        false
    };

    // Per-pane renderer kind. Default proves BOTH paths (pane 0 GPU, pane 1 software).
    let kinds: Vec<Kind> = match force {
        Some(k) => vec![k, k],
        None if wgpu_selected => vec![Kind::Gpu, Kind::Software],
        None => vec![Kind::Software, Kind::Software],
    };
    let titles = ["pane 0", "pane 1"];
    let accents = [
        slint::Color::from_rgb_u8(0x7a, 0xa2, 0xf7),
        slint::Color::from_rgb_u8(0xbb, 0x9a, 0xf7),
    ];
    let uids: Vec<String> = vec!["pane-0".to_string(), "pane-1".to_string()];
    let n = uids.len();

    // ---- tokio runtime that drives the SessionManager's per-session tasks ----
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    // ---- the session manager + its event stream ----
    let (etx, erx) = unbounded_channel::<SessionEvent>();
    let mgr = Rc::new(SessionManager::new(etx));
    let erx = Rc::new(RefCell::new(erx));

    // ---- window + model ----
    let app = DemoWindow::new()?;
    let model: Rc<VecModel<PaneVisual>> = Rc::new(VecModel::default());
    // Seed one (empty) row per pane up front so the `for` instantiates the panes and their
    // geometry-changed callbacks fire.
    for i in 0..n {
        model.push(PaneVisual {
            surface: slint::Image::default(),
            title: titles[i].into(),
            accent: accents[i],
            link_visible: false,
            link_x: 0.0,
            link_y: 0.0,
            link_w: 0.0,
            link_tip: Default::default(),
            link_tip_x: 0.0,
            link_tip_y: 0.0,
            selection_rects: ModelRc::new(VecModel::default()),
        });
    }
    app.set_panes(ModelRc::from(model.clone()));

    // Build a Slint `[HiRect]` model from controller-reported (x,y,w,h) rects.
    fn to_hirects(rects: Vec<(f32, f32, f32, f32)>) -> ModelRc<HiRect> {
        let v: Vec<HiRect> = rects
            .into_iter()
            .map(|(x, y, w, h)| HiRect { x, y, w, h })
            .collect();
        ModelRc::new(VecModel::from(v))
    }

    // Capture Slint's wgpu Device/Queue once rendering is set up.
    let gpu_slot: Rc<RefCell<Option<(wgpu::Device, wgpu::Queue)>>> = Rc::new(RefCell::new(None));
    {
        let slot = gpu_slot.clone();
        app.window()
            .set_rendering_notifier(move |state, api| {
                if let slint::RenderingState::RenderingSetup = state {
                    if let slint::GraphicsAPI::WGPU28 { device, queue, .. } = api {
                        *slot.borrow_mut() = Some((device.clone(), queue.clone()));
                    }
                }
            })
            .ok();
    }

    // Latest reported geometry per pane (logical px); updated by geometry-changed.
    let geom: Rc<RefCell<Vec<(f32, f32)>>> = Rc::new(RefCell::new(vec![(0.0, 0.0); n]));
    {
        let geom = geom.clone();
        app.on_geometry_changed(move |idx, w, h| {
            let idx = idx as usize;
            if let Some(slot) = geom.borrow_mut().get_mut(idx) {
                *slot = (w, h);
            }
        });
    }

    // Focus signal: any mouse-down in a pane fires `focus-requested` (the frozen contract the
    // real app wires to focus the pane). The demo just logs it to prove it fires.
    {
        app.on_focus_requested(move |idx| {
            eprintln!("[demo] focus-requested → pane {idx}");
        });
    }

    // Key input: encode and write to the focused pane's session.
    {
        let mgr = mgr.clone();
        let uids = uids.clone();
        app.on_key(move |idx, msg: KeyMsg| {
            let idx = idx as usize;
            if let Some(bytes) = encode_key(&msg.text, msg.control, msg.alt, msg.shift) {
                if let Some(uid) = uids.get(idx) {
                    mgr.write(uid, &String::from_utf8_lossy(&bytes));
                }
            }
        });
    }

    let state: Rc<RefCell<Option<State>>> = Rc::new(RefCell::new(None));

    // ---- clickable file paths: hover hit-testing + click open/copy ----
    // The widget reports pointer moves/clicks (logical px within the surface); we hit-test the
    // pane's grid against its on-screen size (`geom`), then drive the per-pane hover overlay in
    // the model. The surface Image fills the pane, so the reported coords ARE surface coords.
    {
        let state = state.clone();
        let geom = geom.clone();
        let model = model.clone();
        app.on_link_moved(move |idx, x, y| {
            let idx = idx as usize;
            let mut guard = state.borrow_mut();
            let st = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            if idx >= st.panes.len() {
                return;
            }
            let (w, h) = geom.borrow().get(idx).copied().unwrap_or((0.0, 0.0));
            let hit = st.panes[idx].pane.link_at(x, y, w, h);
            if let Some(mut row) = model.row_data(idx) {
                match hit {
                    Some(lh) => {
                        row.link_visible = true;
                        row.link_x = lh.x;
                        row.link_y = lh.y;
                        row.link_w = lh.w;
                        row.link_tip = lh.tip.into();
                        row.link_tip_x = x + 12.0;
                        row.link_tip_y = y + 16.0;
                    }
                    None => {
                        row.link_visible = false;
                        row.link_tip = Default::default();
                    }
                }
                model.set_row_data(idx, row);
            }
        });
    }
    {
        let model = model.clone();
        app.on_link_exited(move |idx| {
            let idx = idx as usize;
            if let Some(mut row) = model.row_data(idx) {
                row.link_visible = false;
                row.link_tip = Default::default();
                model.set_row_data(idx, row);
            }
        });
    }
    {
        let state = state.clone();
        let geom = geom.clone();
        app.on_link_activated(move |idx, x, y, ctrl| {
            let idx = idx as usize;
            let mut guard = state.borrow_mut();
            let st = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            if idx >= st.panes.len() {
                return;
            }
            // A release that ended a drag-selection isn't a link click — let it pass.
            if st.panes[idx].pane.selection_is_drag() {
                return;
            }
            let (w, h) = geom.borrow().get(idx).copied().unwrap_or((0.0, 0.0));
            // Empty editor command → core picks VS Code (if on PATH) else the guarded OS default.
            match st.panes[idx].pane.activate_link(x, y, w, h, ctrl, "") {
                Some(LinkAction::Copy(p)) => eprintln!("[demo] Ctrl+click → copy: {p}"),
                Some(LinkAction::Opened(res)) => {
                    if res.ok {
                        eprintln!("[demo] click → opened");
                    } else if res.blocked {
                        eprintln!("[demo] click → refused to auto-open ({:?})", res.error);
                    } else {
                        eprintln!("[demo] click → open failed ({:?})", res.error);
                    }
                }
                None => {}
            }
        });
    }

    // ---- text selection: drag to select, copy on release (copy added with the indicator) ----
    {
        let state = state.clone();
        let geom = geom.clone();
        app.on_selection_begin(move |idx, x, y| {
            let idx = idx as usize;
            let mut guard = state.borrow_mut();
            let st = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            if idx >= st.panes.len() {
                return;
            }
            let (w, h) = geom.borrow().get(idx).copied().unwrap_or((0.0, 0.0));
            st.panes[idx].pane.selection_begin(x, y, w, h);
        });
    }
    {
        let state = state.clone();
        let geom = geom.clone();
        let model = model.clone();
        app.on_selection_update(move |idx, x, y| {
            let idx = idx as usize;
            let mut guard = state.borrow_mut();
            let st = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            if idx >= st.panes.len() {
                return;
            }
            let (w, h) = geom.borrow().get(idx).copied().unwrap_or((0.0, 0.0));
            st.panes[idx].pane.selection_update(x, y, w, h);
            let rects = st.panes[idx].pane.selection_rects(w, h);
            if let Some(mut row) = model.row_data(idx) {
                row.selection_rects = to_hirects(rects);
                model.set_row_data(idx, row);
            }
        });
    }
    {
        let state = state.clone();
        let model = model.clone();
        app.on_selection_end(move |idx| {
            let idx = idx as usize;
            let mut guard = state.borrow_mut();
            let st = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            if idx >= st.panes.len() {
                return;
            }
            // A real drag keeps its highlight (and, once wired, copies). A stationary click
            // clears the zero-size selection so it doesn't linger or block the next click.
            if !st.panes[idx].pane.selection_is_drag() {
                st.panes[idx].pane.selection_clear();
                if let Some(mut row) = model.row_data(idx) {
                    row.selection_rects = to_hirects(Vec::new());
                    model.set_row_data(idx, row);
                }
            }
        });
    }

    // ---- the render/pump loop (Slint timer on the UI thread) ----
    let timer = slint::Timer::default();
    let app_weak = app.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(8), {
        let state = state.clone();
        let gpu_slot = gpu_slot.clone();
        let geom = geom.clone();
        let model = model.clone();
        let mgr = mgr.clone();
        let erx = erx.clone();
        let uids = uids.clone();
        let kinds = kinds.clone();
        move || {
            let app = match app_weak.upgrade() {
                Some(a) => a,
                None => return,
            };
            let scale = app.window().scale_factor().max(1.0);

            // ---------- lazy init (needs geometry for every pane + a wgpu device if any
            //            pane is GPU) ----------
            if state.borrow().is_none() {
                let want_gpu = kinds.iter().any(|k| *k == Kind::Gpu);
                if want_gpu && gpu_slot.borrow().is_none() {
                    return; // wait for RenderingSetup
                }
                let g = geom.borrow();
                if g.iter().any(|(w, h)| *w <= 1.0 || *h <= 1.0) {
                    return; // wait for the first real layout
                }

                let px = (14.0 * scale).round().max(8.0);
                let font_path = if std::path::Path::new("C:/Windows/Fonts/CascadiaMono.ttf").exists()
                {
                    "C:/Windows/Fonts/CascadiaMono.ttf"
                } else {
                    "C:/Windows/Fonts/consola.ttf"
                };
                let font = Font::from_path(font_path, px).expect("font load");
                let (cw, ch) = (font.cell_w, font.cell_h);

                let mut panes = Vec::new();
                let mut applied = Vec::new();
                for i in 0..uids.len() {
                    let (w, h) = g[i];
                    let (cols, rows) = cells_for_px(w * scale, h * scale, cw, ch);
                    // Spawn the bound session FIRST, sized to this grid.
                    mgr.create(SpawnOptions {
                        uid: uids[i].clone(),
                        cols: Some(cols as u16),
                        rows: Some(rows as u16),
                        pane_id: Some(uids[i].clone()),
                        ..Default::default()
                    })
                    .expect("spawn session");

                    let renderer: Box<dyn PaneRenderer> = match kinds[i] {
                        Kind::Gpu => {
                            let (d, q) = gpu_slot.borrow().clone().unwrap();
                            Box::new(GpuRenderer::new(d, q))
                        }
                        Kind::Software => Box::new(SoftwareRenderer::new()),
                    };
                    let pane = TerminalPane::new(cols, rows, renderer);
                    eprintln!(
                        "[demo] {} — {}x{} cells ({}x{} px) [{}]",
                        uids[i],
                        cols,
                        rows,
                        cols as u32 * cw,
                        rows as u32 * ch,
                        pane.renderer_name()
                    );
                    panes.push(PaneCtl {
                        pane,
                        kind: kinds[i],
                        started: false,
                        startup: Some(format!(
                            "echo hyperpanes terminal-widget [{}]\r",
                            if kinds[i] == Kind::Gpu { "GPU" } else { "software" }
                        )),
                    });
                    applied.push((cols, rows));
                }

                *state.borrow_mut() = Some(State {
                    font,
                    panes,
                    applied,
                    last_blink: Instant::now(),
                    cursor_on: true,
                    last_hud: Instant::now(),
                    frames: 0,
                });
            }

            let mut guard = state.borrow_mut();
            let st = guard.as_mut().unwrap();

            // ---------- drain session events into the panes ----------
            {
                let mut rx = erx.borrow_mut();
                while let Ok(ev) = rx.try_recv() {
                    match ev {
                        SessionEvent::Data { uid, data } => {
                            if let Some(i) = uids.iter().position(|u| *u == uid) {
                                let pc = &mut st.panes[i];
                                pc.pane.feed(&data);
                                let replies = pc.pane.take_replies();
                                if !replies.is_empty() {
                                    mgr.write(&uid, &String::from_utf8_lossy(&replies));
                                }
                                // First output → the shell is alive; send the demo command.
                                if !pc.started {
                                    pc.started = true;
                                    if let Some(cmd) = pc.startup.take() {
                                        mgr.write(&uid, &cmd);
                                    }
                                }
                            }
                        }
                        SessionEvent::Cwd { uid, cwd } => {
                            // Resolve clickable paths relative to the shell's live directory.
                            if let Some(i) = uids.iter().position(|u| *u == uid) {
                                st.panes[i].pane.set_cwd(Some(cwd.clone()));
                            }
                            eprintln!("[demo] {uid} cwd → {cwd}");
                        }
                        SessionEvent::Exit { uid, code } => {
                            eprintln!("[demo] {uid} exited ({code})");
                        }
                    }
                }
            }

            // ---------- apply geometry changes (reflow grid + session) ----------
            {
                let g = geom.borrow();
                let cw = st.font.cell_w;
                let ch = st.font.cell_h;
                for i in 0..st.panes.len() {
                    let (w, h) = g[i];
                    if w <= 1.0 || h <= 1.0 {
                        continue;
                    }
                    let (cols, rows) = cells_for_px(w * scale, h * scale, cw, ch);
                    if (cols, rows) != st.applied[i] {
                        if st.panes[i].pane.resize(cols, rows) {
                            mgr.resize(&uids[i], cols as u16, rows as u16);
                        }
                        st.applied[i] = (cols, rows);
                    }
                }
            }

            // ---------- cursor blink (~530ms) ----------
            let blink_changed = if st.last_blink.elapsed() >= Duration::from_millis(530) {
                st.cursor_on = !st.cursor_on;
                st.last_blink = Instant::now();
                true
            } else {
                false
            };
            let opts = RenderOpts {
                cursor_on: st.cursor_on,
            };

            // ---------- render dirty panes → model ----------
            let mut rendered = false;
            let State { font, panes, .. } = &mut *st;
            for (i, pc) in panes.iter_mut().enumerate() {
                if !pc.pane.take_dirty() && !blink_changed {
                    continue;
                }
                let img = pc.pane.render(font, &opts);
                // Read-modify-write so the live hover-overlay fields (link_*) set by the link
                // callbacks survive a surface repaint.
                if let Some(mut row) = model.row_data(i) {
                    row.surface = img;
                    model.set_row_data(i, row);
                }
                rendered = true;
            }
            if rendered {
                st.frames += 1;
            }

            // ---------- HUD ----------
            if st.last_hud.elapsed() >= Duration::from_millis(500) {
                let fps = st.frames as f32 / st.last_hud.elapsed().as_secs_f32();
                let names: Vec<&str> = st.panes.iter().map(|p| match p.kind {
                    Kind::Gpu => "GPU",
                    Kind::Software => "SW",
                }).collect();
                app.set_hud(format!("{:.0} fps · {}", fps, names.join(" + ")).into());
                st.frames = 0;
                st.last_hud = Instant::now();
            }
        }
    });

    app.run()?;
    // Tidy up the shells on exit.
    mgr.kill_all();
    Ok(())
}
