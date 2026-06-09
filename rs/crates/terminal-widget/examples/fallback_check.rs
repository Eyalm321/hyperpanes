//! Headless before/after proof for the font-fallback chain (no GUI needed).
//!
//! Loads **FiraCode** as the primary font — it lacks the box-drawing / powerline / nerd /
//! block / arrow glyphs a TUI like Claude Code draws — and rasterizes a line of those
//! glyphs two ways into one PNG:
//!   * **BEFORE** (top row): primary font ONLY → missing glyphs render as `.notdef` (□ tofu).
//!   * **AFTER**  (bottom row): the fallback chain (`Font::resolve`) → each glyph comes from
//!     the first font that maps it (JetBrains Mono / Segoe), so the boxes disappear.
//!
//! Run: `cargo run --example fallback_check`  → writes `target/fallback_check.png`.

use hyperpanes_terminal_widget::font::{Font, GlyphKey};

const FG: [u8; 3] = [0xc0, 0xca, 0xf5];
const BG: [u8; 3] = [0x16, 0x16, 0x1e];
const GAP: u32 = 6;

fn main() -> anyhow::Result<()> {
    // FiraCode is bundled in the app crate's assets — relative to this example file.
    let fira = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../app/assets/fonts/FiraCode-Regular.ttf"
    );
    let px = 30.0_f32;
    let mut font = Font::from_path(fira, px)?;

    let fira_bytes = std::fs::read(fira)?;
    let primary = swash::FontRef::from_index(&fira_bytes, 0).unwrap();

    // A wide candidate set of glyphs a TUI like Claude Code draws. We auto-select the ones
    // FiraCode actually LACKS so the before/after is striking rather than mostly-identical.
    let candidates: Vec<char> = "╭╮╰╯╱╲╳⎿⎸⎹✻✶✦✧✩●○◉◌◍◎⏺⏸⏵⏴⠋⠙⠿⣿⚠⚡ℹ★☆»«›‹•·‣⁃↵⏎➜➤➔⌘⌥⇧⌃✅❌🚀🔥"
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();

    // Keep only what FiraCode misses (primary gid == 0). Those are the glyphs that tofu today.
    let line: Vec<char> = candidates
        .iter()
        .copied()
        .filter(|&c| primary.charmap().map(c) == 0)
        .take(28)
        .collect();
    let line: Vec<char> = if line.is_empty() {
        candidates // (shouldn't happen) — fall back to showing everything
    } else {
        line
    };
    let primary_gids: Vec<u16> = line.iter().map(|&c| primary.charmap().map(c)).collect();

    // Diagnostic table: char, codepoint, primary gid, resolved (font_id, gid).
    let face_name = |id: u16| match id {
        0 => "primary(FiraCode)",
        1 => "JetBrainsMono",
        2 => "SegoeUISymbol",
        3 => "SegoeUIEmoji",
        _ => "?",
    };
    println!("char  codepoint  primaryGid  ->  (font_id, gid)  face");
    for &c in &line {
        let (fid, gid) = font.resolve(c);
        println!(
            "  {c}   U+{:04X}      {:>4}       ->  ({fid}, {gid})   {}",
            c as u32,
            primary.charmap().map(c),
            face_name(fid)
        );
    }

    let cw = font.cell_w;
    let ch = font.cell_h;
    let cols = line.len() as u32;
    let w = cw * cols;
    let h = ch * 2 + GAP;
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for px4 in buf.chunks_exact_mut(4) {
        px4.copy_from_slice(&[BG[0], BG[1], BG[2], 0xff]);
    }

    // BEFORE: primary-only (font_id 0, primary gid → .notdef where unmapped).
    for (i, &gid) in primary_gids.iter().enumerate() {
        let key = GlyphKey { font_id: 0, gid, bold: false, italic: false };
        blit(&mut font, key, &mut buf, w, h, i as u32 * cw, 0);
    }
    // AFTER: the real fallback chain.
    for (i, &c) in line.iter().enumerate() {
        let (font_id, gid) = font.resolve(c);
        let key = GlyphKey { font_id, gid, bold: false, italic: false };
        blit(&mut font, key, &mut buf, w, h, i as u32 * cw, ch + GAP);
    }

    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/target/fallback_check.png");
    let file = std::fs::File::create(out)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&buf)?;

    // Report how many glyphs the primary lacked but the chain recovered.
    let missing = primary_gids.iter().filter(|&&g| g == 0).count();
    let recovered = line
        .iter()
        .zip(&primary_gids)
        .filter(|(&c, &pg)| pg == 0 && font.resolve(c).0 != 0)
        .count();
    println!("wrote {out}");
    println!(
        "primary (FiraCode) lacked {missing}/{} glyphs; fallback chain recovered {recovered}",
        line.len()
    );
    Ok(())
}

/// Blend one rasterized glyph's coverage mask into the RGBA buffer at cell origin
/// (`x0`, `y0`), baseline at `y0 + font.ascent`. Mirrors `SoftwareRenderer`'s blit.
fn blit(font: &mut Font, key: GlyphKey, buf: &mut [u8], w: u32, h: u32, x0: u32, y0: u32) {
    let ascent = font.ascent;
    let g = font.rasterize(key).clone();
    if g.w == 0 {
        return;
    }
    let pen_x = x0 as i32 + g.left;
    let base_y = y0 as i32 + ascent;
    let gy0 = base_y - g.top;
    for gy in 0..g.h as i32 {
        let dy = gy0 + gy;
        if dy < 0 || dy >= h as i32 {
            continue;
        }
        for gx in 0..g.w as i32 {
            let dx = pen_x + gx;
            if dx < 0 || dx >= w as i32 {
                continue;
            }
            let cov = g.mask[(gy * g.w as i32 + gx) as usize] as u32;
            if cov == 0 {
                continue;
            }
            let off = ((dy as u32 * w + dx as u32) * 4) as usize;
            for k in 0..3 {
                let b = buf[off + k] as u32;
                let f = FG[k] as u32;
                buf[off + k] = (b + (f - b) * cov / 255) as u8;
            }
        }
    }
}
