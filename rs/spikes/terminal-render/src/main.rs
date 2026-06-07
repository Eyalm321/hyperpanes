//! Spike A — GPU terminal-in-Slint compositing seam (Phase 0, go/no-go).
//! Throwaway harness owned entirely by track `spike-terminal-render`.
//!
//! Goal: prove `alacritty_terminal` grid → swash glyph atlas → per-pane wgpu texture
//! (`slint::Image::try_from(wgpu::Texture)`, `unstable-wgpu-*`) composited inside Slint
//! as a rounded, border-radiused `Image` at ≥60 FPS on an iGPU maximized pane, plus a
//! software `SharedPixelBuffer` fallback. Full go/no-go criteria in FANOUT-HANDOFF.md.

fn main() {
    println!("spike-terminal-render: not yet implemented — see FANOUT-HANDOFF.md");
}
