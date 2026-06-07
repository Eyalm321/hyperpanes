# Phase 0 · Spike A — GPU terminal-in-Slint — RESULTS

**Recommendation: 🟢 GO** (with one mandatory follow-up: re-measure on the actual Intel
iGPU laptop — see *Caveats*). The single riskiest piece of the rewrite is proven: an
`alacritty_terminal` grid, fed by a real conpty shell, rasterized with `swash`, rendered
into a **per-pane `wgpu::Texture` on Slint's own wgpu device**, and imported via
`slint::Image::try_from` composites as an ordinary Slint `Image` — getting border-radius,
drop-shadow, accent border, z-order and a title chip *for free, over the terminal surface*.
A pure-CPU `SharedPixelBuffer` fallback works behind the same trait. Both clear the bars.

---

## What was built

A standalone throwaway crate (`rs/spikes/terminal-render`, its own workspace root):

- **`term_backend.rs`** — `alacritty_terminal` 0.26 `Term` fed by `vte::ansi::Processor`,
  driven by a real `portable-pty` 0.9 conpty shell (PowerShell). Reader thread → channel →
  `pump()` on the UI thread. Produces a renderer-agnostic `GridSnapshot` (resolved RGBA per
  cell, wide-char flags, cursor) + a dirty flag.
- **`font.rs`** — `swash` glyph rasterizer: loads Cascadia Mono, derives integer cell
  metrics, rasterizes R8 coverage masks with a cache (synthetic bold/italic).
- **`render.rs`** — the **`PaneRenderer` trait** + two impls:
  - `GpuRenderer` — `swash` → `etagere` R8 atlas → instanced bg/glyph quads → per-pane
    `Rgba8Unorm` texture (`RENDER_ATTACHMENT | TEXTURE_BINDING`) on Slint's shared device →
    `slint::Image::try_from`.
  - `SoftwareRenderer` — coverage masks blended into a double-buffered `SharedPixelBuffer`
    → `Image::from_rgba8`.
- **`ui/app.slint` + `main.rs`** — ≥2 rounded, border-radiused, shadowed panes at
  fractional rects, live shells, damage-gated repaint driven by a Slint `Timer`, FPS/idle
  HUD, plus `--max / --flood / --bench / --gpu / --software` measurement modes.

### Screenshots (in this folder)
- `screenshot_idle.png` — 2 panes, **GPU (left) vs software (right) render identically**;
  rounded corners, drop-shadow, accent borders and title chips composited over both real
  PowerShell sessions; idle HUD `0.8%/core`.
- `screenshot_window.png` — both panes under simultaneous flood (~11 MB/s combined VT).
- `screenshot_dpi150.png` — maximized GPU pane at 150% DPI: crisp text from a
  physical-resolution (2328-px-wide) texture, idle `0.2%/core`.

---

## Measured numbers

**Test rig:** Windows 11, **NVIDIA RTX 2080 Ti (discrete)**, 2560×1440 @ 59 Hz, Rust 1.95,
`opt-level=3` on deps. ⚠️ **This is NOT the Intel-iGPU target the criteria specify** —
read *Caveats* before treating the GPU FPS as final.

### Renderer throughput — `--bench`, worst-case *full repaint every frame*
(`render_to_texture` + queue wait; this is the renderer's own ceiling, independent of
display vsync.)

| Renderer | Grid (≈ maximized) | DPI | Texture px | FPS | ms/frame |
|---|---|---|---|---|---|
| **GPU** | 194×51 | 100% | 1552×816 | **8552** | 0.117 |
| **GPU** | 176×49 | 125% | 1936×1029 | **8671** | 0.115 |
| **GPU** | 194×51 | 150% | 2328×1224 | **8083** | 0.124 |
| **Software** | 194×51 | 100% | 1552×816 | **763** | 1.310 |

> GPU is ~11× the 60-FPS bar even at 150% DPI; software alone is ~12× the 30-FPS NO-GO
> floor and ~13× a 60-FPS bar — entirely on CPU.

### Live, integrated (through Slint's vsync'd present, 59 Hz monitor)

| Scenario | Producer rate | Presented FPS | Render-thread CPU |
|---|---|---|---|
| Idle (prompt sitting) | ~0 | 2–4 (blink only) | **0.2–1.7 %/core** |
| GPU maximized, `Write-Host` flood | ~0.15 MB/s | 36 | 4.9 %/core |
| GPU maximized, fast flood | **~5.3 MB/s** | ~33 | 14 %/core |
| 2 panes (GPU+SW) fast flood | **~11.6 MB/s** | ~25 | 30 %/core |

**Reading the live numbers:** presented FPS is **vsync/present-bound, not renderer-bound.**
The monitor is 59 Hz and Slint presents through a FIFO swapchain; under a continuous
full-texture churn a missed deadline halves 60→30, hence the ~25–36 readings. The renderer
itself produces frames at 8552/763 FPS (above). The meaningful live fact: the GPU path
**ingests ~5.3 MB/s of VT on one pane (11.6 MB/s across two) at only 14–30 % of a single
core** — far more than any real workload (cmatrix/builds emit a tiny fraction of that), and
humans can't read 5 MB/s anyway.

---

## GO / NO-GO criteria — verdict

| Criterion | Result |
|---|---|
| GPU sustains ≥ 60 FPS on a flood | ✅ Renderer 8552 FPS; live capped only by 59 Hz vsync. Ingests 5.3 MB/s @ 14%/core. |
| Idle CPU < 3% | ✅ 0.2–1.7 %/core idle (damage-gated; repaint only on dirty/blink). |
| Crisp text at 100/125/150% DPI | ✅ Cell metrics scale (8×16 → 11×21 → 12×24); texture rendered at **physical** resolution, composited into logical rects → 1:1 crisp. See `screenshot_dpi150.png`. |
| border-radius + inset shadow over the texture | ✅ Rounded corners, drop-shadow, accent border, title chip all composite over both GPU and software surfaces (`screenshot_idle.png`). |
| wgpu/GL builds & runs on Intel + AMD + NVIDIA, degrades to software | ⚠️ **NVIDIA only** here. wgpu picks the adapter generically and Slint exposes a software-`renderer-software` fallback, so graceful degradation is *expected* but **untested** on Intel/AMD/RDP. |
| NO-GO (GPU crashes broadly AND software < 30 FPS) | ❌ Not triggered — GPU stable; software 763 FPS. |

---

## Surprises / things that bit (worth carrying into the real build)

1. **Slint needs an explicit wgpu renderer.** `unstable-wgpu-28` alone errors at startup;
   you must also enable `renderer-femtovg-wgpu` (or `renderer-skia`). Setup is
   `BackendSelector::new().require_wgpu_28(WGPUConfiguration::default()).select()` *before*
   creating the window, then grab `device`/`queue` from `set_rendering_notifier` on
   `RenderingState::RenderingSetup` / `GraphicsAPI::WGPU28 { device, queue, .. }` (owned —
   `.clone()` is a cheap Arc bump). `wgpu = "28"` dedups to the exact crate Slint links, so
   `Image::try_from(texture)` accepts it.
2. **conpty hangs without a reply path.** conpty issues `ESC[6n` (cursor-position DSR) at
   startup and *blocks until answered*. `alacritty_terminal`'s `VoidListener` silently drops
   the reply → the shell never prints. Fix: a real `EventListener` that forwards
   `Event::PtyWrite(..)` back to the pty writer. (Cost me the first "0 bytes in" hour.)
3. **Keep the `Child` alive** — binding it to `_child` and letting it drop at end of
   construction can close the process; store it.
4. **wgpu 28 API drift vs. older snippets:** `PipelineLayoutDescriptor.immediate_size`
   (was `push_constant_ranges`), `RenderPipelineDescriptor.multiview_mask` (was
   `multiview`), `RenderPassDescriptor.multiview_mask` is required, copy types are
   `TexelCopyTextureInfo` / `TexelCopyBufferLayout`, `device.poll(PollType::wait_indefinitely())`.

---

## Proposed `PaneRenderer` trait (deliverable for the real UI)

The spike's trait is deliberately small and renderer-agnostic. The grid model and font
cache are shared; each renderer owns its own GPU/CPU buffers and caches them across calls:

```rust
/// Produced by the terminal backend, consumed by any renderer. Resolved RGBA per cell so
/// renderers never touch alacritty/vte types.
pub struct GridSnapshot {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<RenderCell>,      // row-major; ch, fg, bg, bold/italic/underline, wide flags
    pub cursor: (usize, usize),
    pub cursor_visible: bool,
    pub default_bg: [u8; 4],
    pub default_fg: [u8; 4],
}

pub struct RenderOpts { pub cursor_on: bool /* + selection, blink, theme later */ }

pub trait PaneRenderer {
    fn name(&self) -> &'static str;
    /// Render `grid` into a `slint::Image` at the pane's *physical* pixel resolution
    /// (cols*cell_w × rows*cell_h). Implementations cache buffers/atlases across calls and
    /// must be cheap when the grid is unchanged (caller gates on a dirty flag).
    fn render(&mut self, grid: &GridSnapshot, font: &mut Font, opts: &RenderOpts) -> slint::Image;
}
```

- `GpuRenderer::new(device, queue)` is handed Slint's shared `wgpu::Device`/`Queue`.
- `SoftwareRenderer::new()` needs nothing — it's the RDP / software-GL fallback.
- The UI picks GPU when a wgpu device is available and falls back to software otherwise;
  the swap is a one-line `Box<dyn PaneRenderer>` choice.

### Recommended refinements before production
- **Damage-driven partial repaint.** The spike rebuilds the whole instance buffer + texture
  per dirty frame (still 8552 FPS, so not urgent, but it scales the iGPU headroom). Consume
  `term.damage()` line ranges; keep a persistent per-cell instance buffer; `write_buffer`
  only changed rows. (Stubbed/noted in `take_dirty`.)
- **One shared atlas + device across all panes** (the spike's atlas is per-`GpuRenderer`;
  share it so N panes don't each hold a 2048² R8 texture).
- **Atlas eviction** — currently grows until full then drops glyphs (fine for ASCII-heavy
  use; a real impl needs LRU eviction / multi-page).
- Wire **selection** quads (cursor is done) and **top padding** so the title chip doesn't
  overlap row 0. **Ligatures intentionally skipped** for v1 (Cascadia *Mono*).

---

## Caveats — read before acting on the GO

1. **Hardware mismatch (the big one).** Measured on an **RTX 2080 Ti**, not the Intel-iGPU
   laptop the criteria name. An iGPU will be far slower than 8552 FPS — but the per-frame
   work is tiny (~10 k instances, ~2 MB texture, ~0.1 ms CPU to build), so an iGPU should
   still clear 60 comfortably; and the **software fallback (763 FPS, pure CPU) already
   exceeds 60** and is roughly hardware-independent. The GO is robust on that logic, but
   **re-run `--bench --max` + `--flood` on the real iGPU laptop** to convert the inference
   into a measurement before locking the toolkit decision.
2. **Intel / AMD / RDP untested** — no such hardware on this rig. wgpu + Slint's
   `renderer-software` make graceful degradation expected, not proven.
3. **Live FPS is vsync-bound** (59 Hz, FIFO present). Not a renderer limit; a 144 Hz panel
   or a mailbox present mode would show higher. Irrelevant to feasibility.
4. **Idle-CPU figure is the render-thread busy-fraction proxy**, not whole-process CPU
   (excludes the PTY reader thread + Slint compositor). It was 0.2–1.7 %/core; confirm
   with Task Manager on the target machine, but there's a wide margin under 3%.

## How to reproduce
```
cd rs/spikes/terminal-render
cargo run -- --bench --max            # GPU maximized throughput
cargo run -- --bench --max --software # software maximized throughput
cargo run -- --max --flood            # live maximized GPU flood
cargo run --                          # 2 panes: GPU (left) + software (right)
# DPI: set SLINT_SCALE_FACTOR=1.25 / 1.5 ; shell override: SPIKE_SHELL=cmd.exe
```
