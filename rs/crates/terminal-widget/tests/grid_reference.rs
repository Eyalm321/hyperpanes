//! Grid reference tests — feed a known byte sequence to [`TermGrid`], then assert the
//! resolved visible grid text/flags from its [`snapshot`](TermGrid::snapshot).
//!
//! Modeled on Alacritty's `ref`-test approach (a known input → a known grid), but kept
//! deterministic and headless: we feed bytes straight to the model and never touch a PTY,
//! a renderer, or a window.
//!
//! SCOPE (wave 3, #15): stable grid semantics — cursor placement, autowrap, CR/LF,
//! erase-in-line / erase-in-display, wide (CJK) cells — plus the behaviors mature
//! terminals gate on: scroll region (DECSTBM driven by IND/RI), alt-screen enter/exit
//! (DECSET 1049), wide-char wrap at the right edge, and wrap/reflow across a resize.
//! (The earlier "scroll-region is in flight on a sibling track" carve-out is over — the
//! wave-2 DECSTBM work landed, so the region contract is locked in here too.)

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

/// First char of each viewport row (blank cells as space) — compact scroll-layout probe.
fn col0_chars(g: &TermGrid) -> Vec<char> {
    let snap = g.snapshot();
    (0..snap.rows).map(|r| snap.cell(0, r).ch).collect()
}

/// Seed a 6-row grid so each row's first char names it: a b c d e f.
fn seed_six_rows(g: &mut TermGrid) {
    g.feed(b"\x1b[1;1Ha\x1b[2;1Hb\x1b[3;1Hc\x1b[4;1Hd\x1b[5;1He\x1b[6;1Hf");
    assert_eq!(col0_chars(g), vec!['a', 'b', 'c', 'd', 'e', 'f']);
}

// ---- Scroll region: DECSTBM driven by IND (ESC D) and RI (ESC M) -------------------------
//
// The grid.rs unit tests cover LF-at-region-bottom; these lock in the *index* control
// functions, which are what full-screen apps (vim, less, tmux panes) actually scroll with.

#[test]
fn ind_at_region_bottom_scrolls_only_the_region() {
    // Region = 1-based rows 2..=4. IND (ESC D) on the region's bottom line scrolls the
    // region up by one; the fixed lines above (a) and below (e, f) must not move.
    let mut g = TermGrid::new(8, 6);
    seed_six_rows(&mut g);
    g.feed(b"\x1b[2;4r"); // DECSTBM rows 2..=4
    g.feed(b"\x1b[4;1H"); // cursor on the region's bottom line
    g.feed(b"\x1bD"); // IND — line feed without the LNM ambiguity
    assert_eq!(col0_chars(&g), vec!['a', 'c', 'd', ' ', 'e', 'f']);
}

#[test]
fn ri_at_region_top_scrolls_the_region_down() {
    // RI (ESC M) on the region's TOP line scrolls the region down: a blank line enters at
    // the top, the region's old bottom line (d) falls off, and the fixed lines stay.
    let mut g = TermGrid::new(8, 6);
    seed_six_rows(&mut g);
    g.feed(b"\x1b[2;4r");
    g.feed(b"\x1b[2;1H"); // cursor on the region's top line
    g.feed(b"\x1bM"); // RI
    assert_eq!(col0_chars(&g), vec!['a', ' ', 'b', 'c', 'e', 'f']);
}

#[test]
fn ind_and_ri_inside_region_just_move_the_cursor() {
    // Off the region's edges, IND/RI are plain cursor moves — no scroll.
    let mut g = TermGrid::new(8, 6);
    seed_six_rows(&mut g);
    g.feed(b"\x1b[2;4r");
    g.feed(b"\x1b[3;1H"); // mid-region
    g.feed(b"\x1bD"); // IND → row down, no scroll
    let snap = g.snapshot();
    assert_eq!(snap.cursor, (0, 3));
    assert_eq!(col0_chars(&g), vec!['a', 'b', 'c', 'd', 'e', 'f']);
    g.feed(b"\x1bM\x1bM"); // RI ×2 → back up to the region top, still no scroll
    let snap = g.snapshot();
    assert_eq!(snap.cursor, (0, 1));
    assert_eq!(col0_chars(&g), vec!['a', 'b', 'c', 'd', 'e', 'f']);
}

#[test]
fn decstbm_homes_the_cursor_and_reset_restores_full_screen_scrolling() {
    let mut g = TermGrid::new(8, 6);
    seed_six_rows(&mut g);
    g.feed(b"\x1b[2;4r");
    // Setting a margin homes the cursor (DECOM off → absolute home).
    assert_eq!(g.snapshot().cursor, (0, 0));
    g.feed(b"\x1b[r"); // DECSTBM with no params = whole screen
    g.feed(b"\x1b[6;1H\x1bD"); // IND at the screen's last row now scrolls EVERYTHING
    assert_eq!(col0_chars(&g), vec!['b', 'c', 'd', 'e', 'f', ' ']);
}

// ---- Alt screen (DECSET/DECRST 1049) ------------------------------------------------------

#[test]
fn alt_screen_starts_blank_and_exit_restores_primary_content_and_cursor() {
    let mut g = TermGrid::new(20, 4);
    g.feed(b"primary");
    g.feed(b"\x1b[?1049h"); // enter alt screen (save cursor + clear — it does NOT home)
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "", "alt screen must start blank");
    // The cursor carries over from the primary screen (col 7), so home before writing —
    // exactly what a full-screen app does on entry.
    g.feed(b"\x1b[HALT CONTENT");
    assert_eq!(row_text(&g.snapshot(), 0), "ALT CONTENT");
    g.feed(b"\x1b[?1049l"); // leave alt screen
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "primary", "primary content restored");
    assert_eq!(row_text(&snap, 1), "", "alt content must not leak back");
    // 1049 also saves/restores the cursor: it returns to just past "primary".
    assert_eq!(snap.cursor, (7, 0));
}

#[test]
fn alt_screen_round_trip_is_repeatable() {
    // A second enter must again present a CLEAN alt screen (1049 clears on entry), and a
    // second exit must still restore the same primary content.
    let mut g = TermGrid::new(20, 4);
    g.feed(b"keep me");
    for _ in 0..2 {
        g.feed(b"\x1b[?1049h");
        assert_eq!(row_text(&g.snapshot(), 0), "");
        g.feed(b"scratch");
        g.feed(b"\x1b[?1049l");
        assert_eq!(row_text(&g.snapshot(), 0), "keep me");
    }
}

// ---- Wide (CJK) glyphs at the margins ------------------------------------------------------

#[test]
fn consecutive_wide_glyphs_advance_two_columns_each() {
    let mut g = TermGrid::new(20, 2);
    g.feed("世界".as_bytes());
    let snap = g.snapshot();
    assert_eq!(snap.cell(0, 0).ch, '世');
    assert!(snap.cell(0, 0).wide);
    assert!(snap.cell(1, 0).wide_spacer);
    assert_eq!(snap.cell(2, 0).ch, '界');
    assert!(snap.cell(2, 0).wide);
    assert!(snap.cell(3, 0).wide_spacer);
    assert_eq!(snap.cursor, (4, 0), "each wide glyph advances the cursor 2 columns");
}

#[test]
fn wide_glyph_that_does_not_fit_wraps_whole_to_the_next_row() {
    // 5 columns, "abcd" leaves one free cell — a 2-cell glyph can't split, so the whole
    // glyph wraps to row 1 and the orphan cell stays a non-glyph filler.
    let mut g = TermGrid::new(5, 3);
    g.feed("abcd世".as_bytes());
    let snap = g.snapshot();
    assert_eq!(snap.cell(0, 0).ch, 'a');
    assert_eq!(snap.cell(3, 0).ch, 'd');
    assert_eq!(snap.cell(0, 1).ch, '世', "wide glyph wraps whole onto row 1");
    assert!(snap.cell(0, 1).wide);
    assert!(snap.cell(1, 1).wide_spacer);
    assert_ne!(snap.cell(4, 0).ch, '世', "the orphan margin cell must not hold the glyph");
}

// ---- Wrap + reflow across resize -----------------------------------------------------------

/// All viewport text concatenated (rows trimmed, joined bare) — reflow-shape-agnostic probe.
fn visible_text(g: &TermGrid) -> String {
    let snap = g.snapshot();
    (0..snap.rows).map(|r| row_text(&snap, r)).collect::<Vec<_>>().concat()
}

#[test]
fn shrink_reflows_a_soft_wrapped_line_without_losing_text() {
    // 15 chars on 10 cols soft-wrap as "abcdefghij" | "klmno".
    let mut g = TermGrid::new(10, 4);
    g.feed(b"abcdefghijklmno");
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "abcdefghij");
    assert_eq!(row_text(&snap, 1), "klmno");
    // Shrink to 5 cols: the text re-wraps at the new width; nothing is lost (rows that
    // scroll past the top land in history, so probe history + viewport together).
    g.resize(5, 4);
    let all: String = g
        .history_lines()
        .iter()
        .map(|(_, s)| s.trim_end().to_string())
        .collect::<Vec<_>>()
        .concat();
    assert!(
        all.contains("abcdefghijklmno"),
        "soft-wrapped text must survive a shrink reflow intact: {all:?}"
    );
    // Every visible row now fits the 5-col width.
    let snap = g.snapshot();
    assert_eq!(snap.cols, 5);
}

#[test]
fn grow_reflows_a_soft_wrapped_line_back_together() {
    let mut g = TermGrid::new(10, 4);
    g.feed(b"abcdefghijklmno");
    g.resize(20, 4);
    // A soft-wrapped (not hard-newline) line rejoins when the grid widens.
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "abcdefghijklmno");
    assert_eq!(row_text(&snap, 1), "");
}

#[test]
fn hard_newlines_do_not_rejoin_on_grow() {
    // CRLF-separated lines are hard breaks: widening must NOT splice them together.
    let mut g = TermGrid::new(10, 4);
    g.feed(b"first\r\nsecond");
    g.resize(30, 4);
    let snap = g.snapshot();
    assert_eq!(row_text(&snap, 0), "first");
    assert_eq!(row_text(&snap, 1), "second");
}

#[test]
fn resize_keeps_text_when_rows_change() {
    let mut g = TermGrid::new(10, 4);
    g.feed(b"top\r\nmid\r\nbot");
    g.resize(10, 8);
    let after = visible_text(&g);
    assert!(after.contains("top") && after.contains("mid") && after.contains("bot"),
        "growing rows must keep all lines: {after:?}");
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
