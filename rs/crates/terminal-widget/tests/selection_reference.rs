//! Selection-extraction reference tests — drive [`TerminalPane`]'s drag-selection API with
//! logical-pixel points (the way the Slint shell does) and assert the extracted text.
//!
//! Headless: panes use the [`SoftwareRenderer`] but never render — `selection_text` works
//! straight off the grid snapshot. Geometry: a pane of C×R cells over a `surf_w`×`surf_h`
//! surface has cells of `surf_w/C` × `surf_h/R` logical px (see `TerminalPane::cell_logical`),
//! so with a 10×20 px cell, the point (5, 10) lands in cell (0, 0).
//!
//! The drag threshold (4 px) is part of the contract: a press + sub-threshold twitch must
//! NOT become a selection (that's the clipboard-clobber guard), so tests drag well past it.

use hyperpanes_terminal_widget::{SoftwareRenderer, TerminalPane};

/// A 20×4 pane over a 200×80 surface → exact 10×20 px cells.
const SURF_W: f32 = 200.0;
const SURF_H: f32 = 80.0;

fn pane_with(content: &str) -> TerminalPane {
    let mut p = TerminalPane::new(20, 4, Box::new(SoftwareRenderer::new()));
    p.feed(content);
    p
}

/// Center of cell (col, row) in logical px for the 10×20 cell geometry.
fn at(col: usize, row: usize) -> (f32, f32) {
    (col as f32 * 10.0 + 5.0, row as f32 * 20.0 + 10.0)
}

fn drag(p: &mut TerminalPane, from: (usize, usize), to: (usize, usize)) {
    let (x0, y0) = at(from.0, from.1);
    let (x1, y1) = at(to.0, to.1);
    p.selection_begin(x0, y0, SURF_W, SURF_H);
    p.selection_update(x1, y1, SURF_W, SURF_H);
}

#[test]
fn single_row_drag_extracts_the_inclusive_cell_range() {
    let mut p = pane_with("hello world");
    drag(&mut p, (0, 0), (7, 0));
    assert!(p.selection_is_drag());
    assert_eq!(p.selection_text().as_deref(), Some("hello wo"));
}

#[test]
fn multi_row_drag_joins_rows_with_newlines_and_trims_trailing_blanks() {
    let mut p = pane_with("hello world\r\nsecond");
    drag(&mut p, (0, 0), (3, 1));
    // Row 0 runs from the anchor to the line end (trailing blanks trimmed),
    // row 1 from column 0 to the head cell, inclusive.
    assert_eq!(p.selection_text().as_deref(), Some("hello world\nseco"));
}

#[test]
fn reverse_drag_reads_in_document_order() {
    let mut p = pane_with("hello world\r\nsecond");
    drag(&mut p, (3, 1), (0, 0)); // drag up-left: head before anchor
    assert_eq!(p.selection_text().as_deref(), Some("hello world\nseco"));
}

#[test]
fn a_click_without_a_real_drag_selects_nothing() {
    let mut p = pane_with("hello");
    let (x, y) = at(2, 0);
    p.selection_begin(x, y, SURF_W, SURF_H);
    // A 2-px twitch is inside the drag threshold — even across a cell boundary it must
    // not become a selection (the copy-on-select clipboard-clobber guard).
    p.selection_update(x + 2.0, y, SURF_W, SURF_H);
    assert!(!p.selection_is_drag());
    assert_eq!(p.selection_text(), None);
}

#[test]
fn drag_past_the_grid_edge_clamps_to_the_border_cell() {
    let mut p = pane_with("0123456789abcdefghij");
    let (x0, y0) = at(15, 0);
    p.selection_begin(x0, y0, SURF_W, SURF_H);
    // Stray well past the right/bottom edge — the head clamps to the last cell of row 0
    // when y stays on row 0's band... and to the grid corner when both overshoot.
    p.selection_update(SURF_W + 50.0, y0, SURF_W, SURF_H);
    assert_eq!(p.selection_text().as_deref(), Some("fghij"));
}

#[test]
fn select_all_covers_the_visible_screen() {
    let mut p = pane_with("first\r\nsecond\r\nthird");
    p.select_all();
    // The 4-row screen has one trailing blank row — select-all keeps it as a final
    // newline (each row is right-trimmed, rows joined by `\n`).
    assert_eq!(
        p.selection_text().as_deref(),
        Some("first\nsecond\nthird\n")
    );
}

#[test]
fn selection_over_blank_cells_yields_none() {
    let mut p = pane_with(""); // empty grid
    drag(&mut p, (0, 0), (5, 0));
    assert_eq!(
        p.selection_text(),
        None,
        "an all-blank row extracts no text"
    );
}

#[test]
fn clearing_the_selection_drops_the_text() {
    let mut p = pane_with("hello");
    drag(&mut p, (0, 0), (4, 0));
    assert_eq!(p.selection_text().as_deref(), Some("hello"));
    p.selection_clear();
    assert_eq!(p.selection_text(), None);
}
