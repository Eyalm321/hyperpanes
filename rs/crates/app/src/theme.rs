//! Palette, layout metadata, and font loading — the small presentation helpers the
//! controller reaches for. No state here; pure look-up tables + `load_font`.

use hyperpanes_core::layout::presets::Layout;
use hyperpanes_terminal_widget::Font;
use slint::Color;

/// Accent palette assigned to panes in creation order (Tokyo-Night-ish).
pub const PALETTE: [(u8, u8, u8); 6] = [
    (0x7a, 0xa2, 0xf7), // blue
    (0x9e, 0xce, 0x6a), // green
    (0xbb, 0x9a, 0xf7), // purple
    (0xe0, 0xaf, 0x68), // amber
    (0xf7, 0x76, 0x8e), // red
    (0x7d, 0xcf, 0xff), // cyan
];

/// The pane accent for creation index `i`.
pub fn accent_for(i: usize) -> Color {
    let (r, g, b) = PALETTE[i % PALETTE.len()];
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
    let px = (14.0 * scale).round().max(8.0);
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
    Font::from_path(path, px).expect("load monospace font")
}
