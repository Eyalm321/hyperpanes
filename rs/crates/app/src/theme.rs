//! Palette, layout metadata, and font loading — the small presentation helpers the
//! controller reaches for. No state here; pure look-up tables + `load_font`.

use hyperpanes_core::layout::presets::Layout;
use hyperpanes_terminal_widget::Font;
use slint::Color;

/// The selectable frame palettes (pane dot + frame-border colors), the native port of
/// the renderer's `theme.ts` `PALETTES`. Every palette shares the same 8 slots in the
/// same order — red · amber · green · blue · purple · pink · teal · yellow — so a pane
/// created at index `i` keeps its logical hue when the active palette changes (the accent
/// is recomputed by index against the new palette). Which one is active is the
/// `frame_palette` appearance setting; index 0 (Muted) is the default.
pub const FRAME_PALETTES: [(&str, [(u8, u8, u8); 8]); 4] = [
    // "Muted" — the original saturated set (renderer `dark`), kept as the default.
    (
        "Muted",
        [
            (0xe5, 0x48, 0x4d), // red
            (0xf5, 0xa6, 0x23), // amber
            (0x30, 0xa4, 0x6c), // green
            (0x3b, 0x82, 0xf6), // blue
            (0xa8, 0x55, 0xf7), // purple
            (0xec, 0x48, 0x99), // pink
            (0x14, 0xb8, 0xa6), // teal
            (0xea, 0xb3, 0x08), // yellow
        ],
    ),
    // "Vivid" — bold, fully-saturated hues (renderer `medium`).
    (
        "Vivid",
        [
            (0xff, 0x40, 0x40),
            (0xff, 0xa1, 0x2e),
            (0x21, 0xc2, 0x5c),
            (0x35, 0x73, 0xf0),
            (0xad, 0x44, 0xf2),
            (0xf7, 0x3d, 0x92),
            (0x14, 0xc8, 0xb6),
            (0xf7, 0xcb, 0x24),
        ],
    ),
    // "Neon" — brightest, near-pure colors (renderer `light`).
    (
        "Neon",
        [
            (0xff, 0x1a, 0x1a),
            (0xff, 0x88, 0x00),
            (0x00, 0xdd, 0x33),
            (0x2e, 0x8b, 0xff),
            (0xc0, 0x26, 0xff),
            (0xff, 0x1f, 0x8c),
            (0x00, 0xe6, 0xcf),
            (0xff, 0xe0, 0x00),
        ],
    ),
    // "Grayscale" — 8 distinct grays, all readable against the dark UI.
    (
        "Grayscale",
        [
            (0xe0, 0xe0, 0xe0),
            (0xc8, 0xc8, 0xc8),
            (0xb0, 0xb0, 0xb0),
            (0x98, 0x98, 0x98),
            (0x80, 0x80, 0x80),
            (0x6a, 0x6a, 0x6a),
            (0x56, 0x56, 0x56),
            (0x44, 0x44, 0x44),
        ],
    ),
];

/// Clamp a (possibly stale) palette index to a real palette, returning its 8 slots
/// (defaults to index 0 = Muted).
pub fn frame_palette(idx: usize) -> &'static [(u8, u8, u8); 8] {
    &FRAME_PALETTES[idx.min(FRAME_PALETTES.len() - 1)].1
}

/// The pane accent for creation index `i` under frame-palette `palette`.
pub fn accent_for(i: usize, palette: usize) -> Color {
    let slots = frame_palette(palette);
    let (r, g, b) = slots[i % slots.len()];
    Color::from_rgb_u8(r, g, b)
}

/// The full set of user-selectable layouts, in menu order. `Auto` leads (the
/// smart default); the four concrete presets follow. `Single` is reachable via
/// this menu too so every preset is selectable per the Wave-1 spec.
pub const LAYOUT_MENU: [Layout; 6] = [
    Layout::Auto,
    Layout::Single,
    Layout::Columns,
    Layout::Rows,
    Layout::Grid,
    Layout::MainStack,
];

/// Stable order index used to round-trip a `Layout` through the Slint menu (which
/// passes an `int` id). Matches [`LAYOUT_MENU`].
pub fn layout_id(l: Layout) -> i32 {
    LAYOUT_MENU.iter().position(|x| *x == l).unwrap_or(0) as i32
}

/// Resolve a menu id back to a `Layout` (defaults to `Auto` on an out-of-range id).
pub fn layout_from_id(id: i32) -> Layout {
    LAYOUT_MENU
        .get(id as usize)
        .copied()
        .unwrap_or(Layout::Auto)
}

pub fn layout_name(l: Layout) -> &'static str {
    match l {
        Layout::Auto => "auto",
        Layout::Single => "single",
        Layout::Columns => "columns",
        Layout::Rows => "rows",
        Layout::Grid => "grid",
        Layout::MainStack => "main-stack",
    }
}

/// A Segoe MDL2 Assets glyph evoking each layout, for the picker + top-bar button.
pub fn layout_glyph(l: Layout) -> &'static str {
    match l {
        Layout::Auto => "\u{E80A}",      // GridView
        Layout::Single => "\u{E737}",    // Checkbox/single
        Layout::Columns => "\u{E76F}",   // DockLeft-ish (columns)
        Layout::Rows => "\u{E78A}",      // rows
        Layout::Grid => "\u{E80A}",      // GridView
        Layout::MainStack => "\u{E8A4}", // main + stack
    }
}

/// Load a monospace font at the given UI scale (best-available Cascadia/Consolas).
pub fn load_font(scale: f32) -> Font {
    let candidates = [
        "C:/Windows/Fonts/CascadiaMono.ttf",
        "C:/Windows/Fonts/CascadiaCode.ttf",
        "C:/Windows/Fonts/consola.ttf",
    ];
    let path = candidates
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .copied()
        .unwrap_or("C:/Windows/Fonts/consola.ttf");
    load_font_at(path, 14.0, scale)
}

/// Load a monospace font from `path` at `base_px` logical points, scaled for DPI.
/// The Wave-2 preferences feature uses this to re-load the terminal font when the user
/// changes family/size (the new font flows through `relayout`'s cell-metric reflow).
pub fn load_font_at(path: &str, base_px: f32, scale: f32) -> Font {
    let px = (base_px * scale).round().max(8.0);
    let real = if std::path::Path::new(path).exists() {
        path
    } else {
        "C:/Windows/Fonts/consola.ttf"
    };
    Font::from_path(real, px).expect("load monospace font")
}
