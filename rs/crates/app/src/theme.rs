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

/// The selectable terminal colour themes (the terminal's own bg/fg + 16 ANSI colours),
/// the native port of the renderer's `TERMINAL_THEMES`. Each is the 16 base colours the
/// glyph grid uses: index 0 = background, 7 = foreground, 1–6 the ANSI colours, 8–15 the
/// bright variants (see `terminal-widget`'s `set_base16`). Index 0 (Dark) is the default.
pub const TERMINAL_THEMES: [(&str, [[u8; 3]; 16]); 4] = [
    // "Dark" — Catppuccin Mocha (the original look).
    (
        "Dark",
        [
            [0x11, 0x11, 0x1b], // bg
            [0xf3, 0x8b, 0xa8], // red
            [0xa6, 0xe3, 0xa1], // green
            [0xf9, 0xe2, 0xaf], // yellow
            [0x89, 0xb4, 0xfa], // blue
            [0xf5, 0xc2, 0xe7], // magenta
            [0x94, 0xe2, 0xd5], // cyan
            [0xcd, 0xd6, 0xf4], // fg
            [0x58, 0x5b, 0x70], // bright black
            [0xf3, 0x8b, 0xa8],
            [0xa6, 0xe3, 0xa1],
            [0xf9, 0xe2, 0xaf],
            [0x89, 0xb4, 0xfa],
            [0xf5, 0xc2, 0xe7],
            [0x94, 0xe2, 0xd5],
            [0xa6, 0xad, 0xc8],
        ],
    ),
    // "Black" — pure-black background (OLED-friendly).
    (
        "Black",
        [
            [0x00, 0x00, 0x00],
            [0xff, 0x5c, 0x57],
            [0x5a, 0xf7, 0x8e],
            [0xf3, 0xf9, 0x9d],
            [0x57, 0xc7, 0xff],
            [0xff, 0x6a, 0xc1],
            [0x9a, 0xed, 0xfe],
            [0xe6, 0xe6, 0xe6],
            [0x68, 0x68, 0x68],
            [0xff, 0x5c, 0x57],
            [0x5a, 0xf7, 0x8e],
            [0xf3, 0xf9, 0x9d],
            [0x57, 0xc7, 0xff],
            [0xff, 0x6a, 0xc1],
            [0x9a, 0xed, 0xfe],
            [0xff, 0xff, 0xff],
        ],
    ),
    // "Light" — Catppuccin Latte (light background, light-tuned ANSI).
    (
        "Light",
        [
            [0xef, 0xf1, 0xf5],
            [0xd2, 0x0f, 0x39],
            [0x40, 0xa0, 0x2b],
            [0xdf, 0x8e, 0x1d],
            [0x1e, 0x66, 0xf5],
            [0xea, 0x76, 0xcb],
            [0x17, 0x92, 0x99],
            [0x4c, 0x4f, 0x69],
            [0x6c, 0x6f, 0x85],
            [0xd2, 0x0f, 0x39],
            [0x40, 0xa0, 0x2b],
            [0xdf, 0x8e, 0x1d],
            [0x1e, 0x66, 0xf5],
            [0xea, 0x76, 0xcb],
            [0x17, 0x92, 0x99],
            [0xbc, 0xc0, 0xcc],
        ],
    ),
    // "High contrast" — white-on-black with vivid ANSI colours.
    (
        "High contrast",
        [
            [0x00, 0x00, 0x00],
            [0xff, 0x55, 0x55],
            [0x00, 0xff, 0x00],
            [0xff, 0xff, 0x00],
            [0x5c, 0x5c, 0xff],
            [0xff, 0x55, 0xff],
            [0x00, 0xff, 0xff],
            [0xff, 0xff, 0xff],
            [0x88, 0x88, 0x88],
            [0xff, 0x55, 0x55],
            [0x55, 0xff, 0x55],
            [0xff, 0xff, 0x55],
            [0x7c, 0x7c, 0xff],
            [0xff, 0x7c, 0xff],
            [0x55, 0xff, 0xff],
            [0xff, 0xff, 0xff],
        ],
    ),
];

/// Clamp a (possibly stale) theme index to a real theme, returning its 16 base colours
/// (defaults to index 0 = Dark).
pub fn terminal_theme(idx: usize) -> [[u8; 3]; 16] {
    TERMINAL_THEMES[idx.min(TERMINAL_THEMES.len() - 1)].1
}

/// A colour from a theme's base-16 slot, as a Slint `Color` (used by the preview).
pub fn theme_color(idx: usize, slot: usize) -> Color {
    let c = terminal_theme(idx)[slot.min(15)];
    Color::from_rgb_u8(c[0], c[1], c[2])
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

/// The geometric Unicode icon for each layout — the exact glyphs the Electron app uses
/// (`presets.ts` `LAYOUTS[].icon` + `AUTO_LAYOUT.icon`). Unlike [`layout_glyph`] (Segoe
/// MDL2 private-use codepoints, which need the icon font) these render in any UI font, so
/// the menus show them reliably. Used by the application + tab Layout submenus.
pub fn layout_icon(l: Layout) -> &'static str {
    match l {
        Layout::Auto => "⊞",
        Layout::Single => "□",
        Layout::Columns => "▥",
        Layout::Rows => "▤",
        Layout::Grid => "▦",
        Layout::MainStack => "▧",
    }
}

/// The human display label for each layout, matching Electron's `LAYOUTS[].label` /
/// `AUTO_LAYOUT.label` (Title Case). Used in the menus; the HUD/serialization keep the
/// lowercase token from [`layout_name`].
pub fn layout_label(l: Layout) -> &'static str {
    match l {
        Layout::Auto => "Automatic",
        Layout::Single => "Single",
        Layout::Columns => "Columns",
        Layout::Rows => "Rows",
        Layout::Grid => "Grid",
        Layout::MainStack => "Main + Stack",
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
