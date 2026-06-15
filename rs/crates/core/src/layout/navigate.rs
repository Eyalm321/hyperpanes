//! Port of `src/renderer/layout/navigate.ts` — arrow-key focus direction (left/right/up/down)
//! across the current layout's tile geometry (pick the neighbor whose rect is in that
//! direction). Mirror `navigate.test.ts`.

use serde::{Deserialize, Serialize};

use super::presets::Tile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

fn center(t: &Tile) -> (f64, f64) {
    (t.rect.x + t.rect.w / 2.0, t.rect.y + t.rect.h / 2.0)
}

/// Picks the nearest tile in `dir` from the tile at `from_index`, scoring by the
/// distance along the travel axis plus a penalty for perpendicular drift (so
/// focus moves to the best-aligned neighbour). Returns its index or `None`.
pub fn neighbor_index(tiles: &[Tile], from_index: usize, dir: Direction) -> Option<usize> {
    let from = tiles.iter().find(|t| t.index == from_index)?;
    let (fcx, fcy) = center(from);

    let mut best: Option<(usize, f64)> = None;
    for t in tiles {
        if t.index == from_index {
            continue;
        }
        let (cx, cy) = center(t);
        let dx = cx - fcx;
        let dy = cy - fcy;

        let (ok, score) = match dir {
            Direction::Right => (dx > 0.001, dx + dy.abs() * 2.0),
            Direction::Left => (dx < -0.001, -dx + dy.abs() * 2.0),
            Direction::Down => (dy > 0.001, dy + dx.abs() * 2.0),
            Direction::Up => (dy < -0.001, -dy + dx.abs() * 2.0),
        };

        if ok && best.is_none_or(|(_, bs)| score < bs) {
            best = Some((t.index, score));
        }
    }

    best.map(|(idx, _)| idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::presets::{compute_tiles, Layout};
    use crate::layout::sizes::equal_sizes;

    // 2x2 grid order: 0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right
    fn grid() -> Vec<Tile> {
        compute_tiles(Layout::Grid, 4, &equal_sizes(4), 0.6, 0)
    }

    #[test]
    fn moves_across_a_2x2_grid() {
        let t = grid();
        assert_eq!(neighbor_index(&t, 0, Direction::Right), Some(1));
        assert_eq!(neighbor_index(&t, 0, Direction::Down), Some(2));
        assert_eq!(neighbor_index(&t, 1, Direction::Left), Some(0));
        assert_eq!(neighbor_index(&t, 3, Direction::Up), Some(1));
    }

    #[test]
    fn returns_none_when_there_is_no_neighbour_that_way() {
        let t = grid();
        assert_eq!(neighbor_index(&t, 0, Direction::Left), None);
        assert_eq!(neighbor_index(&t, 0, Direction::Up), None);
    }
}
