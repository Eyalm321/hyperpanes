//! Grid reference tests — feed a known byte sequence to [`TermGrid`], then assert the
//! resolved visible grid text/flags from its [`snapshot`](TermGrid::snapshot).
//!
//! Modeled on Alacritty's `ref`-test approach (a known input → a known grid), but kept
//! deterministic and headless: we feed bytes straight to the model and never touch a PTY,
//! a renderer, or a window.
//!
//! SCOPE: these intentionally cover only **stable** grid semantics — cursor placement,
//! autowrap, CR/LF, erase-in-line / erase-in-display, and wide (CJK) cells. Scrollback and
//! scroll-region (DECSTBM) behavior is deliberately *not* asserted here: it is in flight on
//! a sibling track, and these reference tests must not depend on those changes.

use hyperpanes_terminal_widget::{GridSnapshot, TermGrid};

/// The visible text of one grid row (`'\0'`/blank cells render as spaces), trailing
/// blanks trimmed so equality reads naturally.
fn row_text(snap: &GridSnapshot, row: usize) -> String {
    let mut s = String::with_capacity(snap.cols);
    for col in 0..snap.cols {
        let ch = snap.cell(col, row).ch;
        s.push(if ch == '\0' { ' ' } else { ch });
    }
    s.trim_end().to_string()
}

#[test]
fn plain_text_lands_at_expected_cells_and_advances_cursor() {
    let mut g = TermGrid::new(20, 4);
    g.feed(b"hello");
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "hello");
    // Cursor sits just past the last printed glyph, still on row 0.
    assert!(snap.cursor_visible);
    assert_eq!(snap.cursor, (5, 0));
}

#[test]
fn autowrap_continues_on_the_next_row() {
    // 5 columns; 7 printable chars must wrap "abcde" | "fg".
    let mut g = TermGrid::new(5, 4);
    g.feed(b"abcdefg");
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "abcde");
    assert_eq!(row_text(&snap, 1), "fg");
}

#[test]
fn carriage_return_and_linefeed_position_the_cursor() {
    // \r returns to column 0; \n moves down one row (LNM off → no implicit CR).
    let mut g = TermGrid::new(20, 4);
    g.feed(b"ab\r\ncd");
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "ab");
    assert_eq!(row_text(&snap, 1), "cd");
}

#[test]
fn erase_in_line_clears_from_cursor_to_end() {
    // CHA to column 3 (1-based), then EL(0) erases the cursor cell through end-of-line.
    let mut g = TermGrid::new(10, 2);
    g.feed(b"ABCDE");
    g.feed(b"\x1b[3G"); // cursor -> column index 2 ('C')
    g.feed(b"\x1b[K"); // erase cursor..EOL
    let snap = g.snapshot();
    assert_eq!(snap.cell(0, 0).ch, 'A');
    assert_eq!(snap.cell(1, 0).ch, 'B');
    assert_eq!(snap.cell(2, 0).ch, ' ');
    assert_eq!(snap.cell(4, 0).ch, ' ');
}

#[test]
fn erase_in_display_clears_the_whole_screen() {
    let mut g = TermGrid::new(20, 4);
    g.feed(b"line one\r\nline two");
    assert_eq!(row_text(&g.snapshot(), 0), "line one");
    g.feed(b"\x1b[2J"); // ED(2): clear entire display
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "");
    assert_eq!(row_text(&snap, 1), "");
}

#[test]
fn wide_cjk_glyph_occupies_two_columns() {
    // A double-width glyph marks its left cell `wide` and the following cell `wide_spacer`.
    let mut g = TermGrid::new(20, 2);
    g.feed("世".as_bytes());
    let snap = g.snapshot();
    assert_eq!(snap.cell(0, 0).ch, '世');
    assert!(snap.cell(0, 0).wide, "left half is the wide glyph");
    assert!(snap.cell(1, 0).wide_spacer, "right half is a spacer");
}

#[test]
fn fresh_grid_is_blank() {
    let g = TermGrid::new(8, 3);
    let snap = g.snapshot();
    assert_eq!(snap.cols, 8);
    assert_eq!(snap.rows, 3);
    for row in 0..snap.rows {
        assert_eq!(row_text(&snap, row), "", "row {row} should start blank");
    }
}
