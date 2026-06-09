//! Headless before/after proof for the **software** renderer review-fixes (the path the
//! app ships): the block cursor's true-invert (no longer erases the char under it) and the
//! underline staying inside its cell.
//!
//! It feeds a line with an underlined word ("Under") and the cursor parked over a printed
//! glyph ('A'), renders it with the *real* `SoftwareRenderer`, and writes one PNG with two
//! rows:
//!   * **BEFORE** (top): the old cursor behaviour re-created by painting a solid fg block
//!     over the cursor cell → the 'A' vanishes.
//!   * **AFTER**  (bottom): the real renderer with the cursor on → the 'A' is redrawn in the
//!     background colour over the block (true invert), so it stays readable.
//!
//! It also asserts the cursor cell in the AFTER frame still contains glyph pixels (proof the
//! char wasn't erased) and prints the count.
//!
//! Run: `cargo run --example cursor_underline_check` → writes `target/cursor_underline_check.png`.

use hyperpanes_terminal_widget::{
    PaneRenderer, RenderOpts, SoftwareRenderer, TermGrid,
};

const GAP: u32 = 8;

fn render_bytes(
    r: &mut SoftwareRenderer,
    grid: &TermGrid,
    font: &mut hyperpanes_terminal_widget::Font,
    cursor_on: bool,
) -> (Vec<u8>, u32, u32) {
    let snap = grid.snapshot();
    let img = r.render(&snap, font, &RenderOpts { cursor_on });
    let pb = img.to_rgba8().expect("software renderer must yield an rgba8 image");
    (pb.as_bytes().to_vec(), pb.width(), pb.height())
}

fn main() -> anyhow::Result<()> {
    let font_path = if std::path::Path::new("C:/Windows/Fonts/CascadiaMono.ttf").exists() {
        "C:/Windows/Fonts/CascadiaMono.ttf"
    } else {
        "C:/Windows/Fonts/consola.ttf"
    };
    let px = 28.0_f32;
    let mut font = hyperpanes_terminal_widget::Font::from_path(font_path, px)?;
    let (cw, ch) = (font.cell_w, font.cell_h);

    // One row: "Under" underlined, then " AB", with the cursor moved back over the 'A'
    // (col index 6, i.e. CHA column 7).
    let mut grid = TermGrid::new(12, 1);
    grid.feed(b"\x1b[4mUnder\x1b[0m AB\x1b[7G");
    let snap = grid.snapshot();
    let (ccol, crow) = snap.cursor;
    let cell_fg = snap.cell(ccol, crow).fg;
    eprintln!(
        "cursor over '{}' at cell ({ccol},{crow}); cell {cw}x{ch}px",
        snap.cell(ccol, crow).ch
    );

    let mut renderer = SoftwareRenderer::new();

    // AFTER — the real, fixed renderer with the cursor on.
    let (after, w, h) = render_bytes(&mut renderer, &grid, &mut font, true);

    // BEFORE — render with no cursor, then re-create the old bug: a solid fg block over the
    // cursor cell, with no glyph redraw.
    let (mut before, _, _) = render_bytes(&mut renderer, &grid, &mut font, false);
    let x0 = ccol as u32 * cw;
    let y0 = crow as u32 * ch;
    for yy in 0..ch {
        for xx in 0..cw {
            let (xpix, ypix) = (x0 + xx, y0 + yy);
            if xpix >= w || ypix >= h {
                continue;
            }
            let off = ((ypix * w + xpix) * 4) as usize;
            before[off] = cell_fg[0];
            before[off + 1] = cell_fg[1];
            before[off + 2] = cell_fg[2];
            before[off + 3] = 0xff;
        }
    }

    // Proof: count glyph pixels (≠ the solid fg block colour) inside the cursor cell.
    let glyph_px = |buf: &[u8]| -> u32 {
        let mut n = 0;
        for yy in 0..ch {
            for xx in 0..cw {
                let (xpix, ypix) = (x0 + xx, y0 + yy);
                if xpix >= w || ypix >= h {
                    continue;
                }
                let off = ((ypix * w + xpix) * 4) as usize;
                if buf[off] != cell_fg[0] || buf[off + 1] != cell_fg[1] || buf[off + 2] != cell_fg[2]
                {
                    n += 1;
                }
            }
        }
        n
    };
    let after_glyph = glyph_px(&after);
    let before_glyph = glyph_px(&before);
    eprintln!("cursor-cell glyph pixels — BEFORE: {before_glyph}, AFTER: {after_glyph}");
    assert!(
        after_glyph > 0,
        "true-invert must keep glyph pixels under the cursor (got {after_glyph})"
    );

    // Compose BEFORE over AFTER into one PNG.
    let oh = h * 2 + GAP;
    let mut out = vec![0u8; (w * oh * 4) as usize];
    for p in out.chunks_exact_mut(4) {
        p.copy_from_slice(&[0x16, 0x16, 0x1e, 0xff]);
    }
    let blit = |out: &mut [u8], src: &[u8], y_off: u32| {
        for y in 0..h {
            let dst = (((y + y_off) * w) * 4) as usize;
            let s = ((y * w) * 4) as usize;
            out[dst..dst + (w * 4) as usize].copy_from_slice(&src[s..s + (w * 4) as usize]);
        }
    };
    blit(&mut out, &before, 0);
    blit(&mut out, &after, h + GAP);

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/cursor_underline_check.png");
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, oh);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&out)?;
    eprintln!("wrote {path}  (top=BEFORE: char erased · bottom=AFTER: char visible + underline in-cell)");
    Ok(())
}
