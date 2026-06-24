//! Cell-range text selection for a terminal pane — the model behind drag-to-select.
//!
//! This is a deliberately small, renderer-agnostic model (our own cell-range rather than
//! `alacritty_terminal`'s `Selection`, per the Wave-5 handoff): a drag has an **anchor**
//! (the cell the press landed on) and a **head** (the cell under the cursor now), both
//! *inclusive*. Each endpoint is an **absolute grid line** (alacritty's line index: `0..rows`
//! is the live viewport, negatives are scrollback) plus a column — NOT a viewport row. Anchoring
//! to absolute lines is what keeps a selection glued to its text while the viewport scrolls
//! underneath it (and lets a drag keep extending while the wheel scrolls). [`TerminalPane`]
//! owns one of these and turns it into highlight rectangles (projected back through the current
//! display offset, for the Slint overlay) and the selected text (copy-on-select). The text
//! extraction lives in `pane.rs` because it needs the grid; this module is the pure geometry.
//!
//! [`TerminalPane`]: crate::pane::TerminalPane

/// A cell coordinate: a 0-based `col` on an **absolute grid line** (`line`; `0..rows` is the live
/// viewport, negatives are scrollback). A given `line` always refers to the same buffer content,
/// regardless of where the viewport is scrolled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub col: usize,
    pub line: i32,
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

    /// `(start, end)` in reading order (top→bottom by line, then left→right), both inclusive.
    pub fn ordered(&self) -> (Cell, Cell) {
        let (a, b) = (self.anchor, self.head);
        if (a.line, a.col) <= (b.line, b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }
}

/// Highlight rectangles (logical px: `x`,`y`,`w`,`h`) covering `sel`, projected into the **current
/// viewport** via `display_offset` (a line's viewport row = `line + display_offset`) and clipped to
/// the `rows`-tall viewport — so a selection that runs off the top or bottom of the screen paints
/// only its visible part, and one entirely off-screen paints nothing. The selection is
/// row-inclusive and the head cell is included, so a single-cell selection still paints one cell
/// wide. Multi-line selections split into: the first line from its start column to the line end, a
/// full-width block for any whole middle lines, and the last line from column 0 to its end column.
pub fn selection_rects(
    sel: &Selection,
    cols: usize,
    cell_w: f32,
    cell_h: f32,
    display_offset: i32,
    rows: usize,
) -> Vec<(f32, f32, f32, f32)> {
    let (s, e) = sel.ordered();
    let rows_i = rows as i32;
    let mut rects = Vec::new();
    if cols == 0 || rows == 0 {
        return rects;
    }
    // Push one clipped row rect for absolute `line`, columns `c0..=c1` — a no-op if that line is
    // scrolled out of the viewport.
    let push_row = |rects: &mut Vec<(f32, f32, f32, f32)>, line: i32, c0: usize, c1: usize| {
        let vr = line + display_offset;
        if vr < 0 || vr >= rows_i {
            return;
        }
        let x = c0 as f32 * cell_w;
        let w = (c1 + 1 - c0) as f32 * cell_w;
        rects.push((x, vr as f32 * cell_h, w, cell_h));
    };
    if s.line == e.line {
        push_row(&mut rects, s.line, s.col, e.col);
        return rects;
    }
    // First (partial) line: from the start column to the end of the line.
    push_row(&mut rects, s.line, s.col, cols.saturating_sub(1));
    // Whole middle lines as one block, clipped to the visible viewport rows.
    if e.line > s.line + 1 {
        let top_vr = (s.line + 1 + display_offset).max(0);
        let bot_vr = (e.line - 1 + display_offset).min(rows_i - 1);
        if bot_vr >= top_vr {
            rects.push((
                0.0,
                top_vr as f32 * cell_h,
                cols as f32 * cell_w,
                (bot_vr - top_vr + 1) as f32 * cell_h,
            ));
        }
    }
    // Last (partial) line: from column 0 to the end column (inclusive).
    push_row(&mut rects, e.line, 0, e.col);
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(col: usize, line: i32) -> Cell {
        Cell { col, line }
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
    fn single_row_selection_is_one_inclusive_rect_at_zero_offset() {
        let mut s = Selection::new(cell(2, 1));
        s.update(cell(5, 1));
        let r = selection_rects(&s, 80, 10.0, 20.0, 0, 24);
        assert_eq!(r.len(), 1);
        // cols 2..=5 on line 1 → x=20, y=20, w=(5+1-2)*10=40
        assert_eq!(r[0], (20.0, 20.0, 40.0, 20.0));
    }

    #[test]
    fn multi_row_selection_splits_into_three_rects() {
        let mut s = Selection::new(cell(70, 0));
        s.update(cell(4, 2));
        let r = selection_rects(&s, 80, 10.0, 20.0, 0, 24);
        assert_eq!(r.len(), 3);
        // first line: from col 70 to line end (80-70=10 cols) at y=0
        assert_eq!(r[0], (700.0, 0.0, 100.0, 20.0));
        // middle block: full width, one line tall, at y=20
        assert_eq!(r[1], (0.0, 20.0, 800.0, 20.0));
        // last line: col 0..=4 (5 cols) at y=40
        assert_eq!(r[2], (0.0, 40.0, 50.0, 20.0));
    }

    #[test]
    fn two_adjacent_rows_have_no_middle_block() {
        let mut s = Selection::new(cell(10, 0));
        s.update(cell(3, 1));
        let r = selection_rects(&s, 80, 10.0, 20.0, 0, 24);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn display_offset_shifts_rects_down_so_selection_follows_content() {
        // A selection on absolute line 0. Scroll up by 3 (offset 3): line 0's content now sits on
        // viewport row 3, so the highlight must move down with it.
        let mut s = Selection::new(cell(1, 0));
        s.update(cell(4, 0));
        let r = selection_rects(&s, 80, 10.0, 20.0, 3, 24);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, 60.0, "viewport row 3 → y = 3*20");
    }

    #[test]
    fn selection_above_the_viewport_is_clipped_away() {
        // Line -5 with no offset is above the top of the screen → nothing painted.
        let mut s = Selection::new(cell(0, -5));
        s.update(cell(4, -5));
        let r = selection_rects(&s, 80, 10.0, 20.0, 0, 24);
        assert!(r.is_empty());
    }

    #[test]
    fn partial_visibility_keeps_only_on_screen_rows() {
        // Spans lines -2..=2 at offset 0: lines -2,-1 are off-screen, lines 0,1,2 visible.
        let mut s = Selection::new(cell(0, -2));
        s.update(cell(79, 2));
        let r = selection_rects(&s, 80, 10.0, 20.0, 0, 24);
        // first line (-2) clipped out; middle block clipped to rows 0..=1; last line (2) visible.
        // → middle block + last line = 2 rects, both starting at y>=0.
        assert!(r.iter().all(|rc| rc.1 >= 0.0));
        assert!(!r.is_empty());
    }
}
