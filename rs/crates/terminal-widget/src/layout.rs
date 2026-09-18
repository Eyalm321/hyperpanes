//! Testable text-**layout** layer for the renderers: glyph ink metrics, pen placement and
//! wide-glyph alignment.
//!
//! The grid already classifies each cell as narrow / wide / wide-spacer via alacritty's
//! `Flags` (see [`crate::grid::RenderCell::wide`]). This module turns a cell's origin +
//! a rasterized glyph's ink box into the **pen X** where the glyph should be drawn, so a
//! wide (CJK full-width) glyph is visually **centered in its 2-cell span** instead of being
//! left-beam-clamped — the alignment guarantee terminals rely on for CJK text.
//!
//! The three pieces the task asks for map to:
//!   * **字形度量** — [`ink_rect`]: the coverage rectangle a glyph occupies (from its ink
//!     width + left bearing), so callers can reason about the drawn box independently.
//!   * **布局**     — [`pen_x`]: where the glyph's pen goes for a cell, in buffer px.
//!   * **对齐**     — the 2-cell centering inside [`pen_x`] (the `wide` branch).
//!
//! Pure functions over `(u32, u32, i32)` — no rendering state — so every rule is unit-tested
//! in isolation.

use crate::font::CachedGlyph;

/// The ink (coverage) rectangle occupied by a glyph drawn at pen X, in buffer px:
/// `(left_edge, width)`. `left_edge` is clamped to `0` (a negative bearing can't draw off
/// the buffer's left). This is the "度量" half — what actually gets painted.
#[inline]
pub fn ink_rect(pen_x: u32, glyph: &CachedGlyph) -> (u32, u32) {
    let x = (pen_x as i32 + glyph.left).max(0) as u32;
    (x, glyph.w)
}

/// Pen X (buffer px) to draw a cell's glyph at, given the cell's left edge, the cell width
/// and the glyph's ink box. For a **narrow** cell the pen is the cell's left edge (the
/// caller's blit adds `glyph.left` internally — unchanged behaviour). For a **wide** cell the
/// ink is centered within the 2-cell span: `passthrough` when the glyph already fills the
/// span, otherwise shifted so the ink is equidistant from the span's edges.
#[inline]
pub fn pen_x(cell_x: u32, cell_w: u32, wide: bool, glyph_w: u32, glyph_left: i32) -> u32 {
    if !wide {
        return cell_x;
    }
    let span = (cell_w * 2).max(glyph_w);
    let ink_x0 = cell_x + (span - glyph_w) / 2;
    // blit adds `glyph_left` back, so the pen = ink_x0 - left. Never push the ink left of the
    // cell's own edge (a large positive bearing would otherwise over-shoot).
    (ink_x0 as i32 - glyph_left).max(cell_x as i32) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(w: u32, left: i32) -> CachedGlyph {
        CachedGlyph {
            mask: vec![0u8; w as usize],
            w,
            h: 1,
            left,
            top: 0,
        }
    }

    #[test]
    fn narrow_glyph_pen_is_the_cell_edge() {
        // A narrow glyph just starts at the cell's left edge — no centering.
        assert_eq!(pen_x(16, 8, false, 8, 0), 16);
        assert_eq!(pen_x(16, 8, false, 8, 2), 16); // left bearing kept as-is
    }

    #[test]
    fn wide_glyph_filling_span_is_unchanged() {
        // A CJK glyph whose ink is exactly 2 cells wide has nowhere to move.
        let p = pen_x(0, 8, true, 16, 0);
        assert_eq!(p, 0);
        assert_eq!(ink_rect(p, &glyph(16, 0)), (0, 16));
    }

    #[test]
    fn wide_glyph_narrower_than_span_is_centered() {
        // Ink 12px in a 16px span → ink starts at 2, pen backs off the (0) left bearing.
        let g = glyph(12, 0);
        let p = pen_x(0, 8, true, g.w, g.left);
        assert_eq!(p, 2);
        assert_eq!(ink_rect(p, &g), (2, 12));
    }

    #[test]
    fn wide_glyph_with_left_bearing_is_centered() {
        // Ink 12px with a 1px left bearing → desired ink_x0 = 2, pen = 2 - 1 = 1, so the
        // blit (which adds `left`) lands the ink at 2 — centered.
        let g = glyph(12, 1);
        let p = pen_x(0, 8, true, g.w, g.left);
        assert_eq!(p, 1);
        assert_eq!(ink_rect(p, &g), (2, 12));
    }

    #[test]
    fn wide_glyph_ink_never_shifted_left_of_cell() {
        // A large positive bearing would want the pen before the cell; clamp to the cell edge.
        let g = glyph(10, 5);
        let p = pen_x(0, 8, true, g.w, g.left);
        assert_eq!(p, 0);
        assert_eq!(ink_rect(p, &g), (5, 10));
    }

    #[test]
    fn wide_glyph_wider_than_span_stays_left_aligned() {
        // Ink 20px in a 16px span → no room; keep it at the cell edge, not clipped mid-glyph.
        let g = glyph(20, 0);
        let p = pen_x(0, 8, true, g.w, g.left);
        assert_eq!(p, 0);
    }
}
