//! Spike A — GPU terminal-in-Slint compositing seam (Phase 0, go/no-go).
//! Throwaway harness owned entirely by track `spike-terminal-render`.
//!
//! Proves: `alacritty_terminal` grid (fed by a real conpty shell) → swash glyph atlas →
//! per-pane `wgpu::Texture` rendered on Slint's *own* wgpu device → `slint::Image::try_from`
//! → composited as a rounded, border-radiused, shadowed Slint `Image`. Plus a software
//! `SharedPixelBuffer` fallback. Both behind the `PaneRenderer` trait.
//!
//! Run modes:
//!   (default)      two panes side-by-side: pane 0 = GPU, pane 1 = software (visual compare)
//!   --gpu          all panes GPU            --software   all panes software
//!   --max          one maximized pane (~200x55 cells) for the go/no-go FPS test
//!   --flood        send a flood generator to each shell (vtebench-style stress)
//!   --bench        run a timed render-throughput burst once initialised, print FPS

slint::include_modules!();

mod font;
mod render;
mod term_backend;

use font::Font;
use render::{GpuRenderer, PaneRenderer, RenderOpts, SoftwareRenderer};
use slint::{Model, ModelRc, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use term_backend::TermBackend;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Gpu,
    Software,
}

#[derive(Clone, Copy)]
struct PaneCfg {
    // logical rect in the window
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    kind: Kind,
    title: &'static str,
    accent: slint::Color,
}

struct Pane {
    cfg: PaneCfg,
    backend: TermBackend,
    sw: Option<SoftwareRenderer>,
    gpu: Option<GpuRenderer>,
    cols: usize,
    rows: usize,
    flood_sent: bool,
}

struct Flags {
    force: Option<Kind>,
    maximized: bool,
    flood: bool,
    bench: bool,
}

fn parse_flags() -> Flags {
    let mut f = Flags {
        force: None,
        maximized: false,
        flood: false,
        bench: false,
    };
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--gpu" => f.force = Some(Kind::Gpu),
            "--software" => f.force = Some(Kind::Software),
            "--max" => f.maximized = true,
            "--flood" => f.flood = true,
            "--bench" => f.bench = true,
            _ => {}
        }
    }
    f
}

/// A PowerShell one-liner that floods stdout as fast as conpty will carry it (vtebench/
/// cmatrix-style). Builds ~12 KiB of coloured rows once, then blasts them via the raw
/// console writer in a tight loop — this is producer-fast, so the live frame rate reflects
/// the *renderer's* ability to keep up rather than PowerShell's slow `Write-Host`.
const FLOOD_CMD: &str = "$e=[char]27; $sb=[Text.StringBuilder]::new(); for($i=0;$i -lt 60;$i++){ $c=$i%7+31; [void]$sb.Append(\"$e[1;3${c}m\"); for($j=0;$j -lt 200;$j++){ [void]$sb.Append([char](33 + (($i*7+$j*13)%93))) }; [void]$sb.Append(\"`r`n\") }; $s=$sb.ToString(); while($true){ [Console]::Out.Write($s) }\r";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let flags = parse_flags();

    // Make Slint create (and own) a wgpu 28 device/queue we can share. Automatic config.
    slint::BackendSelector::new()
        .require_wgpu_28(slint::wgpu_28::WGPUConfiguration::default())
        .select()?;

    let app = MainWindow::new()?;

    // Capture Slint's wgpu Device/Queue when rendering is set up.
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
            .expect("this build has no wgpu backend; rebuild with unstable-wgpu-28");
    }

    // Pane layout.
    let pane_cfgs: Vec<PaneCfg> = if flags.maximized {
        vec![PaneCfg {
            x: 24.0,
            y: 48.0,
            w: 1552.0,
            h: 828.0,
            kind: flags.force.unwrap_or(Kind::Gpu),
            title: "maximized",
            accent: slint::Color::from_rgb_u8(0x7a, 0xa2, 0xf7),
        }]
    } else {
        vec![
            PaneCfg {
                x: 24.0,
                y: 48.0,
                w: 760.0,
                h: 828.0,
                kind: flags.force.unwrap_or(Kind::Gpu),
                title: "GPU (wgpu texture)",
                accent: slint::Color::from_rgb_u8(0x7a, 0xa2, 0xf7),
            },
            PaneCfg {
                x: 808.0,
                y: 48.0,
                w: 760.0,
                h: 828.0,
                kind: flags.force.unwrap_or(Kind::Software),
                title: "software (pixel buffer)",
                accent: slint::Color::from_rgb_u8(0xbb, 0x9a, 0xf7),
            },
        ]
    };

    let model: Rc<VecModel<PaneVisual>> = Rc::new(VecModel::default());
    app.set_panes(ModelRc::from(model.clone()));

    // Lazily initialised on the first timer tick (so scale_factor + wgpu device are ready).
    let state: Rc<RefCell<Option<State>>> = Rc::new(RefCell::new(None));

    struct State {
        font: Font,
        panes: Vec<Pane>,
        // FPS / blink bookkeeping
        last_fps_t: Instant,
        frames: u32,
        fps: f32,
        last_blink: Instant,
        cursor_on: bool,
        busy_accum: Duration,
        busy_pct: f32,
        benched: bool,
    }

    let app_weak = app.as_weak();
    let timer = slint::Timer::default();
    let flags_rc = Rc::new(flags);

    timer.start(slint::TimerMode::Repeated, Duration::from_millis(4), {
        let state = state.clone();
        let gpu_slot = gpu_slot.clone();
        let model = model.clone();
        let flags = flags_rc.clone();
        let pane_cfgs = Rc::new(pane_cfgs);
        let ticks = std::cell::Cell::new(0u64);
        move || {
            let tc = ticks.get() + 1;
            ticks.set(tc);
            if tc % 200 == 0 {
                eprintln!("[tick] {tc}");
            }
            let app = match app_weak.upgrade() {
                Some(a) => a,
                None => return,
            };

            // ---- lazy init ----
            if state.borrow().is_none() {
                // Need the wgpu device before we can build GPU panes.
                let want_gpu = pane_cfgs.iter().any(|c| c.kind == Kind::Gpu);
                if want_gpu && gpu_slot.borrow().is_none() {
                    return; // wait for RenderingSetup
                }
                let scale = app.window().scale_factor().max(1.0);
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
                for cfg in pane_cfgs.iter() {
                    let cols = ((cfg.w * scale) as u32 / cw).max(8) as usize;
                    let rows = ((cfg.h * scale) as u32 / ch).max(4) as usize;
                    let backend = TermBackend::new(cols, rows).expect("pty");
                    let (sw, gpu) = match cfg.kind {
                        Kind::Software => (Some(SoftwareRenderer::new()), None),
                        Kind::Gpu => {
                            let (d, q) = gpu_slot.borrow().clone().unwrap();
                            (None, Some(GpuRenderer::new(d, q)))
                        }
                    };
                    panes.push(Pane {
                        cfg: PaneCfg { ..*cfg },
                        backend,
                        sw,
                        gpu,
                        cols,
                        rows,
                        flood_sent: false,
                    });
                }

                eprintln!(
                    "[init] scale={scale} font_px={px} cell={cw}x{ch}  panes:",
                );
                for p in &panes {
                    eprintln!(
                        "       {} — {}x{} cells ({}x{} px) [{}]",
                        p.cfg.title,
                        p.cols,
                        p.rows,
                        p.cols as u32 * cw,
                        p.rows as u32 * ch,
                        if p.cfg.kind == Kind::Gpu { "GPU" } else { "SW" }
                    );
                }

                *state.borrow_mut() = Some(State {
                    font,
                    panes,
                    last_fps_t: Instant::now(),
                    frames: 0,
                    fps: 0.0,
                    last_blink: Instant::now(),
                    cursor_on: true,
                    busy_accum: Duration::ZERO,
                    busy_pct: 0.0,
                    benched: false,
                });
            }

            let mut guard = state.borrow_mut();
            let st = guard.as_mut().unwrap();

            // Cursor blink (~530ms).
            if st.last_blink.elapsed() >= Duration::from_millis(530) {
                st.cursor_on = !st.cursor_on;
                st.last_blink = Instant::now();
            }
            let opts = RenderOpts {
                cursor_on: st.cursor_on,
            };

            // ---- one-shot benchmark: worst-case full repaint throughput ----
            if flags.bench && !st.benched {
                st.benched = true;
                const N: u32 = 600;
                let State { font, panes, .. } = &mut *st;
                eprintln!("\n[bench] {N} full repaints per renderer (worst case):");
                for p in panes.iter_mut() {
                    let snap = p.backend.snapshot();
                    if let Some(g) = p.gpu.as_mut() {
                        g.render_to_texture(&snap, font, &opts); // warm
                        g.wait_idle();
                        let t0 = Instant::now();
                        for _ in 0..N {
                            g.render_to_texture(&snap, font, &opts);
                        }
                        g.wait_idle();
                        let dt = t0.elapsed().as_secs_f32();
                        eprintln!(
                            "  [GPU]  {}x{} cells  {:.0} FPS  ({:.3} ms/frame)",
                            p.cols,
                            p.rows,
                            N as f32 / dt,
                            dt * 1000.0 / N as f32
                        );
                    } else if let Some(s) = p.sw.as_mut() {
                        let _ = s.render(&snap, font, &opts); // warm
                        let t0 = Instant::now();
                        for _ in 0..N {
                            let _ = s.render(&snap, font, &opts);
                        }
                        let dt = t0.elapsed().as_secs_f32();
                        eprintln!(
                            "  [SW]   {}x{} cells  {:.0} FPS  ({:.3} ms/frame)",
                            p.cols,
                            p.rows,
                            N as f32 / dt,
                            dt * 1000.0 / N as f32
                        );
                    }
                }
                eprintln!();
            }

            let tick_start = Instant::now();
            // Pump PTYs and repaint dirty panes.
            let mut any_render = false;
            // Ensure model has a row per pane.
            while (model.row_count() as usize) < st.panes.len() {
                model.push(PaneVisual::default());
            }
            let blink_changed = st.last_blink.elapsed() < Duration::from_millis(5);
            let n = st.panes.len();
            let State { font, panes, .. } = st;
            for i in 0..n {
                let p = &mut panes[i];
                p.backend.pump();
                // Send the flood generator once the shell is alive (has produced output).
                if flags.flood && !p.flood_sent && p.backend.bytes_in > 0 {
                    p.backend.write_input(FLOOD_CMD.as_bytes());
                    p.flood_sent = true;
                }
                let dirty = p.backend.take_dirty() || blink_changed;
                if !dirty {
                    continue;
                }
                let snap = p.backend.snapshot();
                let img = if let Some(g) = p.gpu.as_mut() {
                    g.render(&snap, font, &opts)
                } else {
                    p.sw.as_mut().unwrap().render(&snap, font, &opts)
                };
                any_render = true;
                let cw = font.cell_w;
                let ch = font.cell_h;
                let scale = app.window().scale_factor().max(1.0);
                let row = PaneVisual {
                    surface: img,
                    x: p.cfg.x,
                    y: p.cfg.y,
                    width: (p.cols as u32 * cw) as f32 / scale,
                    height: (p.rows as u32 * ch) as f32 / scale,
                    title: p.cfg.title.into(),
                    accent: p.cfg.accent,
                };
                model.set_row_data(i, row);
            }

            // FPS / busy accounting.
            st.busy_accum += tick_start.elapsed();
            if any_render {
                st.frames += 1;
            }
            let el = st.last_fps_t.elapsed();
            if el >= Duration::from_millis(500) {
                st.fps = st.frames as f32 / el.as_secs_f32();
                st.busy_pct = st.busy_accum.as_secs_f32() / el.as_secs_f32() * 100.0;
                st.frames = 0;
                st.busy_accum = Duration::ZERO;
                st.last_fps_t = Instant::now();
                let total_bytes: u64 = st.panes.iter().map(|p| p.backend.bytes_in).sum();
                let hud = format!(
                    "{:.0} FPS · render-thread {:.1}%/core · {} KiB in",
                    st.fps,
                    st.busy_pct,
                    total_bytes / 1024
                );
                eprintln!("[live] {hud}");
                app.set_hud(hud.into());
            }
        }
    });

    app.run()?;
    Ok(())
}
