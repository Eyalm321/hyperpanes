# hyperpanes-terminal-widget

The reusable **live-terminal pane** for the native (Slint) hyperpanes app — a clean
`TerminalPane` Slint component + a Rust controller, lifted from the proven Spike A
renderer and bound to `hyperpanes-core`'s `session_manager` (real shells, not private PTYs).

Wave-2's `app-shell` drops N of these into layout rects.

## What's inside

| Piece | What it is |
|---|---|
| `ui/widget.slint` → `TerminalPane` | The reusable Slint component: shows the terminal `surface` image, composites chrome (rounded corners, drop-shadow, accent border, title chip) over it, captures keys via a `FocusScope`, and reports geometry changes. **Import this into your `.slint`.** |
| `pane::TerminalPane` | The Rust controller: owns the grid model + a renderer, pumped by the app. |
| `render::PaneRenderer` | The renderer trait + `SoftwareRenderer` (CPU `SharedPixelBuffer`) and `GpuRenderer` (per-pane `wgpu::Texture` on Slint's shared device). Software-first; GPU is a one-line swap. |
| `grid::TermGrid` | The `alacritty_terminal` model — fed raw session bytes, emits a renderer-agnostic `GridSnapshot`. Owns no PTY. |
| `font::Font` | Shared `swash` glyph cache + integer cell metrics (share one across all panes). |
| `keys::encode_key` | Slint key event → PTY bytes (arrows, Home/End, Ctrl-/Alt- combos, …). |

## Controller lifecycle (how the app-shell drives one pane)

```rust
use hyperpanes_terminal_widget::{TerminalPane, SoftwareRenderer, Font, RenderOpts, encode_key, cells_for_px};
use hyperpanes_core::session_manager::{SessionManager, SpawnOptions, SessionEvent};

// 1. Spawn/attach a session sized to your initial grid, and a matching controller.
let (cols, rows) = cells_for_px(width_px, height_px, font.cell_w, font.cell_h);
mgr.create(SpawnOptions { uid: uid.clone(), cols: Some(cols as u16), rows: Some(rows as u16),
                          pane_id: Some(uid.clone()), ..Default::default() })?;
let mut pane = TerminalPane::new(cols, rows, Box::new(SoftwareRenderer::new()));

// 2. On each SessionEvent::Data for this pane → feed it, forward DSR/DA replies back.
pane.feed(&data);
let replies = pane.take_replies();           // conpty's ESC[6n etc. — MUST be forwarded
if !replies.is_empty() { mgr.write(&uid, &String::from_utf8_lossy(&replies)); }

// 3. On a Slint key event (from the component's `key` callback) → write to the session.
if let Some(bytes) = encode_key(&msg.text, msg.control, msg.alt, msg.shift) {
    mgr.write(&uid, &String::from_utf8_lossy(&bytes));
}

// 4. On a geometry change (the component's `geometry-changed` callback) → reflow both.
let (cols, rows) = cells_for_px(w * scale, h * scale, font.cell_w, font.cell_h);
if pane.resize(cols, rows) { mgr.resize(&uid, cols as u16, rows as u16); }

// 5. Each frame (a Slint Timer): repaint if dirty (or the cursor blink flipped).
if pane.take_dirty() || blink_flipped {
    let img: slint::Image = pane.render(&mut font, &RenderOpts { cursor_on });
    // → assign `img` to the TerminalPane component's `surface` property.
}
```

The `Font` is passed at render time so a whole fleet of panes shares one glyph cache.
Pick `GpuRenderer::new(device, queue)` instead of `SoftwareRenderer` when you hold Slint's
shared wgpu device (capture it from `set_rendering_notifier` /
`GraphicsAPI::WGPU28`); swap at runtime with `TerminalPane::set_renderer`.

## Demo

```
cargo run --bin demo            # two live panes: pane 0 GPU, pane 1 software
cargo run --bin demo -- --software   # both panes software (RDP / weak-iGPU fallback)
cargo run --bin demo -- --gpu        # both panes GPU
```

`src/bin/demo.rs` is the full reference wiring (tokio runtime + `SessionManager` + the
render/pump loop). See `screenshot_demo.png` for the GPU-vs-software side-by-side.

## Status

`cargo build` / `cargo test` green (13 unit tests: grid, keys, controller, inverse-video).
Bound to `core::session_manager`; key input + resize + DSR-reply forwarding work; both
render paths behind `PaneRenderer`.
