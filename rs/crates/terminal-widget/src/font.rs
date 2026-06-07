//! Shared glyph rasterization via swash. Loads a monospace font, derives integer cell
//! metrics, and rasterizes coverage (R8 alpha) masks on demand with a cache. Both the
//! software and GPU renderers consume `CachedGlyph`s from here (the GPU path packs them
//! into an atlas; the software path blends them directly).

use std::collections::HashMap;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::scale::image::Content;
use swash::zeno::{Angle, Format, Transform};
use swash::{FontRef, GlyphId};

#[derive(Clone)]
pub struct CachedGlyph {
    pub mask: Vec<u8>, // coverage, row-major, w*h bytes (Content::Mask). Empty if blank.
    pub w: u32,
    pub h: u32,
    pub left: i32, // bearing from pen origin
    pub top: i32,  // bearing above baseline
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct GlyphKey {
    pub gid: u16,
    pub bold: bool,
    pub italic: bool,
}

pub struct Font {
    data: Vec<u8>,
    scale: ScaleContext,
    cache: HashMap<GlyphKey, CachedGlyph>,
    px: f32,
    /// Integer cell metrics in physical px.
    pub cell_w: u32,
    pub cell_h: u32,
    pub ascent: i32, // baseline offset from cell top
}

impl Font {
    pub fn from_path(path: &str, px: f32) -> anyhow::Result<Self> {
        let data = std::fs::read(path)?;
        let font = FontRef::from_index(&data, 0)
            .ok_or_else(|| anyhow::anyhow!("not a valid font: {path}"))?;

        let m = font.metrics(&[]).scale(px);
        let cell_h = (m.ascent + m.descent + m.leading).round().max(1.0) as u32;
        let ascent = m.ascent.round() as i32;
        // Monospace advance: measure a representative glyph.
        let gm = font.glyph_metrics(&[]).scale(px);
        let gid = font.charmap().map('M');
        let adv = gm.advance_width(gid);
        let cell_w = if adv > 0.5 {
            adv.round().max(1.0) as u32
        } else {
            (px * 0.6).round() as u32
        };
        let _ = font; // release the borrow of `data` before moving it into `Font`

        Ok(Font {
            data,
            scale: ScaleContext::new(),
            cache: HashMap::new(),
            px,
            cell_w,
            cell_h,
            ascent,
        })
    }

    #[inline]
    pub fn glyph_id(&self, ch: char) -> u16 {
        let font = FontRef::from_index(&self.data, 0).unwrap();
        font.charmap().map(ch)
    }

    pub fn rasterize(&mut self, key: GlyphKey) -> &CachedGlyph {
        if !self.cache.contains_key(&key) {
            let glyph = self.render_glyph(key);
            self.cache.insert(key, glyph);
        }
        self.cache.get(&key).unwrap()
    }

    fn render_glyph(&mut self, key: GlyphKey) -> CachedGlyph {
        // Disjoint field borrows: `data` (immut) + `scale` (mut).
        let font = FontRef::from_index(&self.data, 0).unwrap();
        let mut scaler = self
            .scale
            .builder(font)
            .size(self.px)
            .hint(true)
            .build();

        let mut render = Render::new(&[
            Source::Outline,
            Source::Bitmap(StrikeWith::BestFit),
        ]);
        render.format(Format::Alpha);
        if key.italic {
            // ~12-degree synthetic slant.
            render.transform(Some(Transform::skew(
                Angle::from_degrees(-12.0),
                Angle::ZERO,
            )));
        }
        if key.bold {
            render.embolden(self.px * 0.03);
        }

        match render.render(&mut scaler, key.gid as GlyphId) {
            Some(img) if img.placement.width > 0 && img.placement.height > 0 => {
                // We only ever request Alpha → Content::Mask (1 byte/px).
                let (w, h) = (img.placement.width, img.placement.height);
                let mask = match img.content {
                    Content::Mask => img.data,
                    // Defensive: collapse any color result to luminance-ish coverage.
                    _ => img
                        .data
                        .chunks_exact(4)
                        .map(|p| p[3])
                        .collect::<Vec<u8>>(),
                };
                CachedGlyph {
                    mask,
                    w,
                    h,
                    left: img.placement.left,
                    top: img.placement.top,
                }
            }
            _ => CachedGlyph {
                mask: Vec::new(),
                w: 0,
                h: 0,
                left: 0,
                top: 0,
            },
        }
    }
}
