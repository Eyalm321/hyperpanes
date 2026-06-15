//! Port of `src/renderer/layout/presets.ts` — the 5 layout presets (single / columns / rows /
//! grid / main-stack) + auto-resolution (1→single, 2-3→columns, 4+→grid). Computes tile rects
//! as fractions 0..1 from (preset, pane count, sizes, mainFraction):
//! `compute_tiles(...) -> Vec<Tile>` (each carries a `Rect { x, y, w, h }`). Mirror `presets.test.ts`.

use serde::{Deserialize, Serialize};

use super::sizes::{clamp_fraction, equal_sizes, normalize};

/// `'auto'` tiles by pane count (see [`effective_layout`]); the rest are concrete presets.
/// Serializes to the same kebab strings as the TS `Layout` union (`"main-stack"` etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    Auto,
    Single,
    Columns,
    Rows,
    Grid,
    MainStack,
}

/// The columns→grid boundary for auto: 2..=AUTO_COLUMNS_MAX panes tile as columns,
/// more tile as a grid. A single tunable knob (Q2).
pub const AUTO_COLUMNS_MAX: usize = 3;

/// Resolve a layout to a concrete preset for a given pane count. `'auto'` maps
/// 1 → single, 2..=AUTO_COLUMNS_MAX → columns, more → grid; `'main-stack'` and
/// `'rows'` are manual-only and never produced here. Concrete layouts pass through
/// unchanged, so compute_tiles/compute_dividers/neighbor_index always see one of
/// the 5 real presets — never `'auto'`.
pub fn effective_layout(layout: Layout, n: usize) -> Layout {
    if layout != Layout::Auto {
        return layout;
    }
    if n <= 1 {
        return Layout::Single;
    }
    if n <= AUTO_COLUMNS_MAX {
        return Layout::Columns;
    }
    Layout::Grid
}

/// A rectangle in fractions of the container (0..1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tile {
    /// index into the pane order
    pub index: usize,
    pub rect: Rect,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DividerKind {
    Size,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DividerDesc {
    pub id: String,
    pub kind: DividerKind,
    pub orientation: Orientation,
    /// boundary after pane `index` (for kind `Size`); -1 for `Main`
    pub index: i32,
    /// position along the axis, fraction 0..1
    pub at: f64,
}

const FULL: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1.0,
    h: 1.0,
};

/// Maps (layout, pane count, sizes) to a rectangle per pane. Every pane gets a
/// tile every time (panes stay mounted); `visible: false` just hides it (used by
/// the `single` preset) so terminal sessions and scrollback are never destroyed.
pub fn compute_tiles(
    layout: Layout,
    n: usize,
    sizes: &[f64],
    main_fraction: f64,
    focused_index: i32,
) -> Vec<Tile> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![Tile {
            index: 0,
            rect: FULL,
            visible: true,
        }];
    }

    let fallback = equal_sizes(n);
    let norm = normalize(if sizes.len() == n { sizes } else { &fallback });
    let mut tiles: Vec<Tile> = Vec::new();

    match layout {
        Layout::Single => {
            let shown = if focused_index >= 0 && (focused_index as usize) < n {
                focused_index as usize
            } else {
                0
            };
            for i in 0..n {
                tiles.push(Tile {
                    index: i,
                    rect: FULL,
                    visible: i == shown,
                });
            }
            tiles
        }
        Layout::Columns => {
            let mut x = 0.0;
            for (i, &frac) in norm.iter().enumerate() {
                tiles.push(Tile {
                    index: i,
                    rect: Rect {
                        x,
                        y: 0.0,
                        w: frac,
                        h: 1.0,
                    },
                    visible: true,
                });
                x += frac;
            }
            tiles
        }
        Layout::Rows => {
            let mut y = 0.0;
            for (i, &frac) in norm.iter().enumerate() {
                tiles.push(Tile {
                    index: i,
                    rect: Rect {
                        x: 0.0,
                        y,
                        w: 1.0,
                        h: frac,
                    },
                    visible: true,
                });
                y += frac;
            }
            tiles
        }
        Layout::Grid => {
            let cols = (n as f64).sqrt().ceil() as usize;
            let rows = ((n as f64) / (cols as f64)).ceil() as usize;
            for i in 0..n {
                let r = i / cols;
                let items_in_row = if r < rows - 1 {
                    cols
                } else {
                    n - cols * (rows - 1)
                };
                let c = i - r * cols;
                tiles.push(Tile {
                    index: i,
                    rect: Rect {
                        x: c as f64 / items_in_row as f64,
                        y: r as f64 / rows as f64,
                        w: 1.0 / items_in_row as f64,
                        h: 1.0 / rows as f64,
                    },
                    visible: true,
                });
            }
            tiles
        }
        Layout::MainStack => {
            let mf = clamp_fraction(main_fraction);
            tiles.push(Tile {
                index: 0,
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: mf,
                    h: 1.0,
                },
                visible: true,
            });
            let stack_n = n - 1;
            let h = 1.0 / stack_n as f64;
            for i in 1..n {
                tiles.push(Tile {
                    index: i,
                    rect: Rect {
                        x: mf,
                        y: (i - 1) as f64 * h,
                        w: 1.0 - mf,
                        h,
                    },
                    visible: true,
                });
            }
            tiles
        }
        // 'auto' never reaches here (resolved via effective_layout first).
        Layout::Auto => tiles,
    }
}

/// Draggable seams for the current layout. Phase 2 resizes columns, rows, and
/// the main divider of main-stack; grid and the stack interior use fixed splits.
pub fn compute_dividers(
    layout: Layout,
    n: usize,
    sizes: &[f64],
    main_fraction: f64,
) -> Vec<DividerDesc> {
    if n < 2 {
        return Vec::new();
    }
    let fallback = equal_sizes(n);
    let norm = normalize(if sizes.len() == n { sizes } else { &fallback });
    let mut out: Vec<DividerDesc> = Vec::new();

    match layout {
        Layout::Columns => {
            let mut x = 0.0;
            for (i, &frac) in norm.iter().enumerate().take(n - 1) {
                x += frac;
                out.push(DividerDesc {
                    id: format!("v-{i}"),
                    kind: DividerKind::Size,
                    orientation: Orientation::Vertical,
                    index: i as i32,
                    at: x,
                });
            }
        }
        Layout::Rows => {
            let mut y = 0.0;
            for (i, &frac) in norm.iter().enumerate().take(n - 1) {
                y += frac;
                out.push(DividerDesc {
                    id: format!("h-{i}"),
                    kind: DividerKind::Size,
                    orientation: Orientation::Horizontal,
                    index: i as i32,
                    at: y,
                });
            }
        }
        Layout::MainStack => {
            out.push(DividerDesc {
                id: "main".to_string(),
                kind: DividerKind::Main,
                orientation: Orientation::Vertical,
                index: -1,
                at: clamp_fraction(main_fraction),
            });
        }
        _ => {}
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.005
    }

    // ---- compute_tiles ----

    #[test]
    fn a_single_pane_fills_the_area_for_any_layout() {
        let t = compute_tiles(Layout::Grid, 1, &[1.0], 0.6, 0);
        assert_eq!(t.len(), 1);
        assert_eq!(
            t[0].rect,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0
            }
        );
        assert!(t[0].visible);
    }

    #[test]
    fn columns_are_full_height_and_widths_sum_to_1() {
        let t = compute_tiles(Layout::Columns, 3, &equal_sizes(3), 0.6, 0);
        assert_eq!(t.len(), 3);
        assert!(t.iter().all(|x| x.visible && x.rect.h == 1.0));
        assert!(close(t.iter().map(|x| x.rect.w).sum(), 1.0));
    }

    #[test]
    fn single_layout_shows_only_the_focused_pane() {
        let t = compute_tiles(Layout::Single, 3, &equal_sizes(3), 0.6, 1);
        assert_eq!(t.iter().filter(|x| x.visible).count(), 1);
        assert!(t[1].visible);
    }

    #[test]
    fn grid_keeps_every_tile_within_bounds() {
        let t = compute_tiles(Layout::Grid, 4, &equal_sizes(4), 0.6, 0);
        assert_eq!(t.len(), 4);
        assert!(t
            .iter()
            .all(|x| x.rect.x >= 0.0 && x.rect.x + x.rect.w <= 1.0001));
    }

    #[test]
    fn main_stack_gives_pane_0_the_main_width_and_stacks_the_rest() {
        let t = compute_tiles(Layout::MainStack, 3, &equal_sizes(3), 0.6, 0);
        assert!(close(t[0].rect.w, 0.6));
        assert!(close(t[1].rect.x, 0.6));
        assert!(close(t[2].rect.x, 0.6));
    }

    // ---- effective_layout ----

    #[test]
    fn maps_1_pane_to_single() {
        assert_eq!(effective_layout(Layout::Auto, 1), Layout::Single);
    }

    #[test]
    fn maps_2_and_3_panes_to_columns() {
        assert_eq!(effective_layout(Layout::Auto, 2), Layout::Columns);
        assert_eq!(effective_layout(Layout::Auto, 3), Layout::Columns);
    }

    #[test]
    fn maps_4_plus_panes_to_grid() {
        assert_eq!(effective_layout(Layout::Auto, 4), Layout::Grid);
        assert_eq!(effective_layout(Layout::Auto, 9), Layout::Grid);
        assert_eq!(effective_layout(Layout::Auto, 25), Layout::Grid);
    }

    #[test]
    fn treats_an_empty_group_as_single() {
        assert_eq!(effective_layout(Layout::Auto, 0), Layout::Single);
    }

    #[test]
    fn never_auto_selects_rows_or_main_stack_at_any_count() {
        for n in 0..=30 {
            let eff = effective_layout(Layout::Auto, n);
            assert_ne!(eff, Layout::Rows);
            assert_ne!(eff, Layout::MainStack);
            assert_ne!(eff, Layout::Auto);
        }
    }

    #[test]
    fn passes_concrete_layouts_through_unchanged_regardless_of_count() {
        assert_eq!(effective_layout(Layout::Rows, 5), Layout::Rows);
        assert_eq!(effective_layout(Layout::MainStack, 9), Layout::MainStack);
        assert_eq!(effective_layout(Layout::Single, 4), Layout::Single);
        assert_eq!(effective_layout(Layout::Columns, 1), Layout::Columns);
        assert_eq!(effective_layout(Layout::Grid, 2), Layout::Grid);
    }

    // ---- compute_dividers ----

    #[test]
    fn columns_produce_n_minus_1_vertical_dividers() {
        assert_eq!(
            compute_dividers(Layout::Columns, 3, &equal_sizes(3), 0.6).len(),
            2
        );
    }

    #[test]
    fn grid_has_no_draggable_dividers() {
        assert_eq!(
            compute_dividers(Layout::Grid, 4, &equal_sizes(4), 0.6).len(),
            0
        );
    }

    #[test]
    fn main_stack_has_a_single_main_divider() {
        let d = compute_dividers(Layout::MainStack, 3, &equal_sizes(3), 0.6);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DividerKind::Main);
    }

    // serde parity: the kebab strings match the TS `Layout` union.
    #[test]
    fn layout_serializes_to_ts_strings() {
        assert_eq!(
            serde_json::to_string(&Layout::MainStack).unwrap(),
            "\"main-stack\""
        );
        assert_eq!(serde_json::to_string(&Layout::Auto).unwrap(), "\"auto\"");
    }
}
