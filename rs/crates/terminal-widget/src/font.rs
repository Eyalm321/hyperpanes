//! Shared glyph rasterization via swash. Loads a monospace **fallback chain** — the
//! user's selected font (primary) plus a sequence of fallbacks — derives integer cell
//! metrics from the *primary* font, and rasterizes coverage (R8 alpha) masks on demand
//! with a cache. Both the software and GPU renderers consume `CachedGlyph`s from here (the
//! GPU path packs them into an atlas; the software path blends them directly).
//!
//! ## Why a chain
//! A single font rarely covers everything a TUI draws. When a font lacks a glyph,
//! `charmap().map(ch)` returns 0 and swash rasterizes `.notdef` → the `□` tofu box. So,
//! per character, we walk a chain and rasterize from the **first** font that actually maps
//! it: primary → bundled JetBrains Mono (broad coverage) → the platform's symbol + emoji
//! fonts (Segoe on Windows, Apple Symbols/Emoji on macOS, Noto/Symbola via fontconfig on
//! Linux — see [`fallback_specs`]; emoji are monochrome here, colour is a follow-up) →
//! last-resort the primary's `.notdef` so truly-unknown codepoints still draw *something*.
//!
//! Cell metrics stay driven by the primary font so the grid stays monospace; an oversized
//! fallback glyph (e.g. an emoji) is uniformly shrunk by a per-face `fit` factor so it
//! snaps into the cell box instead of resizing the row.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Angle, Format, Transform};
use swash::{FontRef, GlyphId};

/// The bundled JetBrains Mono — the universal fallback. Embedded so the widget needs no
/// asset path at runtime (the file lives in the app crate's `assets/fonts`).
const JETBRAINS_MONO: &[u8] =
    include_bytes!("../../app/assets/fonts/JetBrainsMono-Regular.ttf");

/// Symbols Nerd Font Mono — covers the private-use icon ranges (powerline, devicons,
/// font-awesome, etc., roughly U+E000–U+F8FF + supplementary) that neither the primary nor
/// JetBrains Mono / Segoe map. Bundled in this crate's own `assets/fonts` (it's the
/// canonical symbols-only Nerd Font; see assets/fonts/SymbolsNerdFont-LICENSE).
const SYMBOLS_NERD: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

#[derive(Clone)]
pub struct CachedGlyph {
    pub mask: Vec<u8>, // coverage, row-major, w*h bytes (Content::Mask). Empty if blank.
    pub w: u32,
    pub h: u32,
    pub left: i32, // bearing from pen origin
    pub top: i32,  // bearing above baseline
}

/// Glyph cache / atlas key. `font_id` is the index into the fallback chain that the glyph
/// was resolved from — it MUST be part of the key so a gid from one font can't collide with
/// the same gid from another (different fonts number their glyphs independently).
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct GlyphKey {
    pub font_id: u16,
    pub gid: u16,
    pub bold: bool,
    pub italic: bool,
}

/// One font in the fallback chain: its raw bytes plus a uniform `fit` scale applied when
/// rasterizing. `fit == 1.0` for the primary (so its output is byte-identical to the
/// pre-fallback renderer) and `<= 1.0` for a fallback whose natural line-height exceeds the
/// primary cell, so its glyphs shrink into the cell box and never change the row height.
struct FontFace {
    data: Vec<u8>,
    fit: f32,
}

impl FontFace {
    #[inline]
    fn font(&self) -> FontRef<'_> {
        // Cheap: parses only the table directory. Re-derived per use so `Font` needn't hold
        // a self-referential `FontRef` borrowing `data`.
        FontRef::from_index(&self.data, 0).unwrap()
    }
}

/// Monotonic source of per-`Font` generation ids. Every `Font` constructed gets a fresh,
/// unique id so consumers that cache by glyph (e.g. the GPU atlas) can detect a font/size
/// reload — the new `Font` carries a different generation — and invalidate stale entries.
static FONT_GEN: AtomicU64 = AtomicU64::new(1);

pub struct Font {
    /// The fallback chain. `faces[0]` is the primary (selected) font; `1..` are fallbacks.
    faces: Vec<FontFace>,
    scale: ScaleContext,
    cache: HashMap<GlyphKey, CachedGlyph>,
    /// Memoized `resolve(ch)` results so the fallback chain (each face parses its table
    /// directory + charmap lookup) is walked at most once per distinct char, not per char
    /// per frame.
    resolve_cache: HashMap<char, (u16, u16)>,
    /// Unique identity of this `Font` instance — see [`FONT_GEN`]. Bumps on every load.
    generation: u64,
    px: f32,
    /// Integer cell metrics in physical px — driven entirely by the primary font.
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
        let _ = font; // release the borrow of `data` before moving it into `FontFace`

        // Primary first (fit forced to 1.0 → identical output to the single-font path),
        // then the fallback chain. Each fallback is appended only if it loads.
        let mut faces = vec![FontFace { data, fit: 1.0 }];
        for fb in fallback_specs() {
            if let Some(bytes) = fb.load() {
                if let Some(fit) = compute_fit(&bytes, px, cell_h) {
                    faces.push(FontFace { data: bytes, fit });
                }
            }
        }

        Ok(Font {
            faces,
            scale: ScaleContext::new(),
            cache: HashMap::new(),
            resolve_cache: HashMap::new(),
            generation: FONT_GEN.fetch_add(1, Ordering::Relaxed),
            px,
            cell_w,
            cell_h,
            ascent,
        })
    }

    /// This `Font`'s unique generation id. It changes whenever a new `Font` is loaded (a
    /// font/size reload constructs a fresh one), so a renderer that caches glyphs by key can
    /// compare against it and drop stale entries on reload. See [`crate::render::GpuRenderer`].
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Walk the fallback chain and return `(font_id, gid)` for the **first** font that maps
    /// `ch`. Falls back to `(0, 0)` — the primary font's `.notdef` — so an unmapped char
    /// still draws something (and stays cached under the primary, like before).
    #[inline]
    pub fn resolve(&mut self, ch: char) -> (u16, u16) {
        if let Some(&hit) = self.resolve_cache.get(&ch) {
            return hit;
        }
        // Cold path: walk the chain once (each `face.font()` parses the table directory),
        // then memoize so subsequent frames are a single HashMap lookup.
        let mut result = (0, 0);
        for (i, face) in self.faces.iter().enumerate() {
            let gid = face.font().charmap().map(ch);
            if gid != 0 {
                result = (i as u16, gid);
                break;
            }
        }
        self.resolve_cache.insert(ch, result);
        result
    }

    pub fn rasterize(&mut self, key: GlyphKey) -> &CachedGlyph {
        if !self.cache.contains_key(&key) {
            let glyph = self.render_glyph(key);
            self.cache.insert(key, glyph);
        }
        self.cache.get(&key).unwrap()
    }

    fn render_glyph(&mut self, key: GlyphKey) -> CachedGlyph {
        let face = match self.faces.get(key.font_id as usize) {
            Some(f) => f,
            None => &self.faces[0], // defensive: stale font_id → primary
        };
        let fit = face.fit;
        let font = face.font();
        // A fallback glyph is rasterized at a reduced size (`px * fit`) so it fits the cell
        // box; the primary's fit is 1.0, so the primary path is unchanged.
        let mut scaler = self
            .scale
            .builder(font)
            .size(self.px * fit)
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
            render.embolden(self.px * fit * 0.03);
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

/// A lazily-resolved fallback font source.
enum FallbackSpec {
    /// Bytes already in the binary (the bundled JetBrains Mono / Symbols Nerd Font).
    Embedded(&'static [u8]),
    /// A file under `%WINDIR%\Fonts` (the Segoe fonts).
    #[cfg_attr(not(windows), allow(dead_code))]
    WindowsFont(&'static str),
    /// An absolute path probed directly (the macOS system fonts).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    SystemPath(&'static str),
    /// A fontconfig family name resolved via `fc-list` (Linux — font files have no
    /// stable paths across distros, but family names are universal).
    #[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
    Family(&'static str),
}

impl FallbackSpec {
    fn load(&self) -> Option<Vec<u8>> {
        match self {
            FallbackSpec::Embedded(bytes) => Some(bytes.to_vec()),
            FallbackSpec::WindowsFont(name) => {
                let dir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
                let path = std::path::Path::new(&dir).join("Fonts").join(name);
                std::fs::read(path).ok()
            }
            FallbackSpec::SystemPath(path) => std::fs::read(path).ok(),
            FallbackSpec::Family(family) => std::fs::read(fontconfig_family_file(family)?).ok(),
        }
    }
}

/// The installed font file for a fontconfig family, or `None` when the family isn't
/// installed. Uses `fc-list` (which lists only real installs — `fc-match` substitutes the
/// closest font and would silently pull in something wrong). Prefers the Regular face over
/// alphabetically-earlier Bold/Italic siblings. Called once per fallback per font load (a
/// rare event: startup + preference changes), so a shell-out is fine.
fn fontconfig_family_file(family: &str) -> Option<String> {
    let out = std::process::Command::new("fc-list")
        .args(["--format", "%{file}\\n", family])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let files: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    files
        .iter()
        .find(|f| f.contains("Regular") || f.contains("regular"))
        .or_else(|| files.first())
        .map(|f| f.to_string())
}

/// The fallback chain after the primary, in priority order, per platform. Always: the
/// bundled JetBrains Mono (broad Unicode coverage) first and the bundled Symbols Nerd Font
/// (private-use icon ranges) in the middle; around those, the OS's own symbol + emoji
/// fonts. Missing fonts are simply skipped. Order matters only for codepoints more than
/// one font maps — the nerd PUA ranges are unique to the Symbols font.
///
/// Emoji render monochrome everywhere (the pipeline is alpha-mask only — a colour bitmap
/// strike collapses to its alpha channel, i.e. a silhouette); colour is a follow-up.
fn fallback_specs() -> Vec<FallbackSpec> {
    #[cfg(windows)]
    return vec![
        FallbackSpec::Embedded(JETBRAINS_MONO),
        FallbackSpec::WindowsFont("seguisym.ttf"),
        FallbackSpec::Embedded(SYMBOLS_NERD),
        FallbackSpec::WindowsFont("seguiemj.ttf"),
    ];
    #[cfg(target_os = "macos")]
    return vec![
        FallbackSpec::Embedded(JETBRAINS_MONO),
        FallbackSpec::SystemPath("/System/Library/Fonts/Apple Symbols.ttf"),
        FallbackSpec::Embedded(SYMBOLS_NERD),
        FallbackSpec::SystemPath("/System/Library/Fonts/Apple Color Emoji.ttc"),
    ];
    // Linux: the Noto symbol pair (Fedora/openSUSE/Ubuntu default coverage) + Symbola
    // (very broad single-file symbol font, gdouros-symbola on Fedora), then the Nerd
    // icons, then Noto Emoji (monochrome) with Noto Color Emoji as the silhouette-only
    // last resort, and DejaVu Sans as a broad catch-all tail.
    #[cfg(not(any(windows, target_os = "macos")))]
    return vec![
        FallbackSpec::Embedded(JETBRAINS_MONO),
        FallbackSpec::Family("Noto Sans Symbols 2"),
        FallbackSpec::Family("Noto Sans Symbols"),
        FallbackSpec::Family("Symbola"),
        FallbackSpec::Embedded(SYMBOLS_NERD),
        FallbackSpec::Family("Noto Emoji"),
        FallbackSpec::Family("Noto Color Emoji"),
        FallbackSpec::Family("DejaVu Sans"),
    ];
}

/// Uniform shrink factor so this fallback's glyphs fit the primary cell height. Returns
/// `None` if the bytes aren't a valid font (so the face is dropped from the chain).
fn compute_fit(data: &[u8], px: f32, cell_h: u32) -> Option<f32> {
    let font = FontRef::from_index(data, 0)?;
    let m = font.metrics(&[]).scale(px);
    let lh = m.ascent + m.descent + m.leading;
    let fit = if lh > cell_h as f32 && lh > 0.0 {
        cell_h as f32 / lh
    } else {
        1.0
    };
    Some(fit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Font` whose primary is the bundled JetBrains Mono (written to a temp
    /// file — `from_path` is the only constructor), with the platform fallback chain.
    fn jetbrains_font() -> Font {
        let p = std::env::temp_dir().join("hp-font-test-jbmono.ttf");
        std::fs::write(&p, JETBRAINS_MONO).unwrap();
        Font::from_path(p.to_str().unwrap(), 14.0).unwrap()
    }

    #[test]
    fn nerd_icons_resolve_past_the_primary() {
        // U+F121 (font-awesome "code") lives only in the bundled Symbols Nerd Font
        // (JetBrains Mono carries the powerline E0B0s itself, but not this range) —
        // it must resolve to a non-primary face with a real gid on every platform.
        let mut f = jetbrains_font();
        let (font_id, gid) = f.resolve('\u{f121}');
        assert!(font_id > 0 && gid != 0, "nerd icon fell to ({font_id}, {gid})");
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn emoji_resolve_via_a_system_fallback_when_installed() {
        // 😀 is in no bundled font; with a fontconfig emoji font installed the chain
        // must pick it up (the original Linux bug: the chain probed only Segoe paths,
        // so emoji were tofu regardless of the selected family). Skipped quietly on a
        // box without any Noto Emoji.
        if fontconfig_family_file("Noto Emoji").is_none()
            && fontconfig_family_file("Noto Color Emoji").is_none()
        {
            return;
        }
        let mut f = jetbrains_font();
        let (font_id, gid) = f.resolve('😀');
        assert!(font_id > 0 && gid != 0, "emoji fell to ({font_id}, {gid})");
    }
}
