//! Cell-range text selection for a terminal pane — the model behind drag-to-select.
//!
//! This is a deliberately small, renderer-agnostic model (our own cell-range rather than
//! `alacritty_terminal`'s `Selection`, per the Wave-5 handoff): a drag has an **anchor**
//! (the cell the press landed on) and a **head** (the cell under the cursor now), both
//! *inclusive* viewport cells. [`TerminalPane`](crate::pane::TerminalPane) owns one of these
//! and turns it into highlight rectangles (for the Slint overlay) and the selected text (for
//! copy-on-select). The text extraction itself lives in `pane.rs` because it needs the grid
//! snapshot; this module is the pure geometry + ordering.

/// A cell coordinate in the pane's *viewport* (0-based `col`,`row`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub col: usize,
    pub row: usize,
}

/// A live drag selection. `anchor` is where the press started; `head` follows the cursor.
/// Both are inclusive. `dragged` flips true once the head leaves the anchor cell, which lets
/// the caller tell a real selection apart from a plain click (so a click still opens a link).
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor: Cell,
    pub head: Cell,
    pub dragged: bool,
}

impl Selection {
    /// Begin a selection anchored at `anchor` (head starts coincident, not yet dragged).
    pub fn new(anchor: Cell) -> Self {
        Self {
            anchor,
            head: anchor,
            dragged: false,
        }
    }

    /// Move the head to `head`; marks the selection `dragged` once it leaves the anchor cell.
    pub fn update(&mut self, head: Cell) {
        self.head = head;
        if head != self.anchor {
            self.dragged = true;
        }
    }

    /// `(start, end)` in reading order (top→bottom, then left→right), both inclusive.
    pub fn ordered(&self) -> (Cell, Cell) {
        let (a, b) = (self.anchor, self.head);
        if (a.row, a.col) <= (b.row, b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }
}

/// Up to three highlight rectangles (logical px: `x`,`y`,`w`,`h`) covering `sel` over a grid of
/// `cols` columns, given the logical cell size. The selection is row-inclusive and the head cell
/// is included, so a single-cell selection still paints one cell wide. Multi-row selections split
/// into: the first row from its start column to the line end, a full-width block for any whole
/// middle rows, and the last row from column 0 to its end column.
pub fn selection_rects(
    sel: &Selection,
    cols: usize,
    cell_w: f32,
    cell_h: f32,
) -> Vec<(f32, f32, f32, f32)> {
    let (s, e) = sel.ordered();
    let mut rects = Vec::new();
    if s.row == e.row {
        let x = s.col as f32 * cell_w;
        let w = (e.col + 1 - s.col) as f32 * cell_w;
        rects.push((x, s.row as f32 * cell_h, w, cell_h));
        return rects;
    }
    // First (partial) row: from the start column to the end of the line.
    rects.push((
        s.col as f32 * cell_w,
        s.row as f32 * cell_h,
        (cols.saturating_sub(s.col)) as f32 * cell_w,
        cell_h,
    ));
    // Whole middle rows as one block.
    if e.row > s.row + 1 {
        rects.push((
            0.0,
            (s.row + 1) as f32 * cell_h,
            cols as f32 * cell_w,
            (e.row - s.row - 1) as f32 * cell_h,
        ));
    }
    // Last (partial) row: from column 0 to the end column (inclusive).
    rects.push((
        0.0,
        e.row as f32 * cell_h,
        (e.col + 1) as f32 * cell_w,
        cell_h,
    ));
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(col: usize, row: usize) -> Cell {
        Cell { col, row }
    }

    #[test]
    fn ordering_is_reading_order_regardless_of_drag_direction() {
        // Drag up-left: head before anchor → ordered() swaps them.
        let mut s = Selection::new(cell(5, 2));
        s.update(cell(1, 0));
        let (a, b) = s.ordered();
        assert_eq!(a, cell(1, 0));
        assert_eq!(b, cell(5, 2));
        assert!(s.dragged);
    }

    #[test]
    fn a_press_without_movement_is_not_dragged() {
        let s = Selection::new(cell(3, 1));
        assert!(!s.dragged);
        let (a, b) = s.ordered();
        assert_eq!(a, b);
    }

    #[test]
    fn single_row_selection_is_one_inclusive_rect() {
        let mut s = Selection::new(cell(2, 1));
        s.update(cell(5, 1));
        let r = selection_rects(&s, 80, 10.0, 20.0);
        assert_eq!(r.len(), 1);
        // cols 2..=5 → x=20, w=(5+1-2)*10=40
        assert_eq!(r[0], (20.0, 20.0, 40.0, 20.0));
    }

    #[test]
    fn multi_row_selection_splits_into_three_rects() {
        let mut s = Selection::new(cell(70, 0));
        s.update(cell(4, 2));
        let r = selection_rects(&s, 80, 10.0, 20.0);
        assert_eq!(r.len(), 3);
        // first row: from col 70 to line end (80-70=10 cols) at y=0
        assert_eq!(r[0], (700.0, 0.0, 100.0, 20.0));
        // middle block: full width, one row tall, at y=20
        assert_eq!(r[1], (0.0, 20.0, 800.0, 20.0));
        // last row: col 0..=4 (5 cols) at y=40
        assert_eq!(r[2], (0.0, 40.0, 50.0, 20.0));
    }

    #[test]
    fn two_adjacent_rows_have_no_middle_block() {
        let mut s = Selection::new(cell(10, 0));
        s.update(cell(3, 1));
        let r = selection_rects(&s, 80, 10.0, 20.0);
        assert_eq!(r.len(), 2);
    }
}
