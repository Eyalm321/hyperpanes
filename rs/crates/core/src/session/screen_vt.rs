//! Serialize a live [`Screen`](super::screen::Screen) back into a VT byte stream that rebuilds
//! it: fed into a fresh emulator of the same size, the stream reproduces the visible grid cell
//! for cell (glyphs, colours, attributes, soft wraps, wide and combining characters, hyperlinks),
//! the terminal modes a program set, the saved cursor, the cursor and its pen.
//!
//! This is what a re-attaching GUI primes a pane from (issue #140). A raw replay tail can't do
//! it: it starts mid-state, a diff renderer only rewrites the cells it believes changed, and the
//! mode switches that set the screen up may have scrolled out of the tail.
//!
//! The stream is meant to be fed AFTER whatever history the grid was primed with (the replay,
//! for scrollback): it cancels a half-parsed sequence, resets the pen and the scroll region,
//! erases every visible row without scrolling anything into history, then paints.
//!
//! Not carried (the emulator keeps no public handle on them, or they don't survive a re-attach
//! anyway): the scroll region (reset to the full screen), the primary screen under an active
//! alternate screen, charsets, the kitty keyboard stack, the title, palette changes (OSC 4), a
//! soft wrap on the bottom row, and a leading wide-char spacer at the end of a row that is not
//! soft-wrapped.

use std::fmt::Write as _;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, CursorStyle, NamedColor};

/// Cell flags that are layout, not attributes: they never go into an SGR.
const LAYOUT: Flags = Flags::WRAPLINE
    .union(Flags::WIDE_CHAR)
    .union(Flags::WIDE_CHAR_SPACER)
    .union(Flags::LEADING_WIDE_CHAR_SPACER);

/// The graphic rendition a cell is drawn with (what an SGR + OSC 8 set).
#[derive(Clone, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
    link: Option<Hyperlink>,
}

impl Pen {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags - LAYOUT,
            underline: cell.underline_color(),
            link: cell.hyperlink(),
        }
    }

    fn reset() -> Self {
        Self::of(&Cell::default())
    }
}

/// Serialize `term` (see the module docs). `default_style` is the cursor style the emulator
/// falls back to when no program set one: a style equal to it is written as "reset to the
/// default" (`CSI 0 SP q`), so the receiving terminal keeps its own default.
pub(crate) fn serialize<T: EventListener>(term: &Term<T>, default_style: CursorStyle) -> String {
    let grid = term.grid();
    let (cols, rows) = (grid.columns(), grid.screen_lines());
    let mode = *term.mode();
    let mut out = String::with_capacity(cols * rows * 2 + 256);

    // Prelude: abort any half-parsed sequence the history ended in (CAN), reset the pen and
    // any hyperlink, pick the screen, reset the scroll region / origin / insert, enable autowrap
    // (soft-wrapped rows are painted by wrapping).
    out.push('\x18');
    out.push_str("\x1b[0m\x1b]8;;\x1b\\");
    out.push_str(if mode.contains(TermMode::ALT_SCREEN) {
        "\x1b[?1049h"
    } else {
        "\x1b[?1049l"
    });
    out.push_str("\x1b[r\x1b[?6l\x1b[?7h\x1b[4l\x1b[20l");
    // Erase every visible row in place (an ED 2 would push the primary screen into history).
    for r in 0..rows {
        let _ = write!(out, "\x1b[{};1H\x1b[2K", r + 1);
    }

    let mut pen = Pen::reset();
    let mut r = 0;
    while r < rows {
        let _ = write!(out, "\x1b[{};1H", r + 1);
        let mut continuation = false;
        loop {
            let row = &grid[Line(r as i32)];
            let wrapped = r + 1 < rows && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            let mut end = if wrapped {
                cols
            } else {
                (0..cols)
                    .rev()
                    .find(|&c| !is_blank(&row[Column(c)]))
                    .map_or(0, |c| c + 1)
            };
            if continuation && end == 0 {
                end = 1; // a printed cell is what makes the previous row wrap onto this one
            }
            for c in 0..end {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) && wrapped {
                    continue; // the wide char that opens the next row puts it back
                }
                put_cell(&mut out, &mut pen, cell);
            }
            r += 1;
            if !wrapped {
                break;
            }
            continuation = true;
        }
    }

    // Modes. Mouse protocols and encodings are exclusive: clear them all, then set the one.
    flag(&mut out, mode.contains(TermMode::SHOW_CURSOR), "?25");
    flag(&mut out, mode.contains(TermMode::APP_CURSOR), "?1");
    out.push_str(if mode.contains(TermMode::APP_KEYPAD) {
        "\x1b="
    } else {
        "\x1b>"
    });
    flag(&mut out, mode.contains(TermMode::BRACKETED_PASTE), "?2004");
    out.push_str("\x1b[?1000l\x1b[?1002l\x1b[?1003l");
    if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
        out.push_str("\x1b[?1000h");
    } else if mode.contains(TermMode::MOUSE_DRAG) {
        out.push_str("\x1b[?1002h");
    } else if mode.contains(TermMode::MOUSE_MOTION) {
        out.push_str("\x1b[?1003h");
    }
    out.push_str("\x1b[?1006l\x1b[?1005l");
    if mode.contains(TermMode::SGR_MOUSE) {
        out.push_str("\x1b[?1006h");
    } else if mode.contains(TermMode::UTF8_MOUSE) {
        out.push_str("\x1b[?1005h");
    }
    flag(&mut out, mode.contains(TermMode::FOCUS_IN_OUT), "?1004");
    flag(&mut out, mode.contains(TermMode::ALTERNATE_SCROLL), "?1007");
    flag(&mut out, mode.contains(TermMode::URGENCY_HINTS), "?1042");
    flag(&mut out, mode.contains(TermMode::LINE_FEED_NEW_LINE), "20");
    if mode.contains(TermMode::ORIGIN) {
        out.push_str("\x1b[?6h"); // homes the cursor: before every positioning below
    }

    // The saved cursor (DECSC): its position and pen, saved with ESC 7.
    let saved = &grid.saved_cursor;
    goto(&mut out, saved.point);
    set_pen(&mut out, &mut pen, &Pen::of(&saved.template));
    out.push_str("\x1b7");

    // The cursor. A cursor parked past the last column (the next glyph wraps first) is rebuilt
    // by re-printing the glyph under it, which leaves the same pending wrap.
    let cursor = &grid.cursor;
    let under = &grid[cursor.point];
    if cursor.input_needs_wrap
        && !under
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        goto(&mut out, cursor.point);
        put_cell(&mut out, &mut pen, under);
    } else {
        goto(&mut out, cursor.point);
    }
    // Modes that change how the paint above behaves, set only now.
    if !mode.contains(TermMode::LINE_WRAP) {
        out.push_str("\x1b[?7l");
    }
    if mode.contains(TermMode::INSERT) {
        out.push_str("\x1b[4h");
    }
    let style = term.cursor_style();
    if style == default_style {
        out.push_str("\x1b[0 q");
    } else if let Some(n) = decscusr(style) {
        let _ = write!(out, "\x1b[{n} q");
    }
    // Leave the pen where the program had it.
    set_pen(&mut out, &mut pen, &Pen::of(&cursor.template));
    out
}

/// A cell that EL leaves behind: a blank with the default rendition.
fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.flags.difference(Flags::WRAPLINE).is_empty()
        && cell.fg == Color::Named(NamedColor::Foreground)
        && cell.bg == Color::Named(NamedColor::Background)
        && cell.extra.is_none()
}

fn put_cell(out: &mut String, pen: &mut Pen, cell: &Cell) {
    set_pen(out, pen, &Pen::of(cell));
    out.push(cell.c);
    if let Some(zw) = cell.zerowidth() {
        out.extend(zw.iter());
    }
}

fn flag(out: &mut String, on: bool, mode: &str) {
    let _ = write!(out, "\x1b[{mode}{}", if on { 'h' } else { 'l' });
}

fn goto(out: &mut String, p: Point) {
    let _ = write!(out, "\x1b[{};{}H", p.line.0 + 1, p.column.0 + 1);
}

fn set_pen(out: &mut String, pen: &mut Pen, want: &Pen) {
    if pen.link != want.link {
        match &want.link {
            Some(l) => {
                let _ = write!(out, "\x1b]8;id={};{}\x1b\\", l.id(), l.uri());
            }
            None => out.push_str("\x1b]8;;\x1b\\"),
        }
    }
    if pen.fg != want.fg
        || pen.bg != want.bg
        || pen.flags != want.flags
        || pen.underline != want.underline
    {
        out.push_str("\x1b[0");
        let f = want.flags;
        for (bit, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if f.contains(bit) {
                out.push(';');
                out.push_str(code);
            }
        }
        colour(out, want.fg, 30, 90, 38);
        colour(out, want.bg, 40, 100, 48);
        match want.underline {
            Some(Color::Indexed(i)) => {
                let _ = write!(out, ";58;5;{i}");
            }
            Some(Color::Spec(c)) => {
                let _ = write!(out, ";58;2;{};{};{}", c.r, c.g, c.b);
            }
            _ => {}
        }
        out.push('m');
    }
    *pen = want.clone();
}

/// Append one colour's SGR parameters; the default colour (and the named colours no SGR can
/// select) need none after the leading reset.
fn colour(out: &mut String, c: Color, base: u8, bright: u8, ext: u8) {
    match c {
        Color::Named(n) if (n as usize) < 8 => {
            let _ = write!(out, ";{}", base as usize + n as usize);
        }
        Color::Named(n) if (n as usize) < 16 => {
            let _ = write!(out, ";{}", bright as usize + n as usize - 8);
        }
        Color::Named(_) => {}
        Color::Indexed(i) => {
            let _ = write!(out, ";{ext};5;{i}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

/// DECSCUSR parameter for a style, if one selects it (a hollow block or a hidden cursor has none).
fn decscusr(style: CursorStyle) -> Option<u8> {
    let base = match style.shape {
        CursorShape::Block => 1,
        CursorShape::Underline => 3,
        CursorShape::Beam => 5,
        CursorShape::HollowBlock | CursorShape::Hidden => return None,
    };
    Some(if style.blinking { base } else { base + 1 })
}

#[cfg(test)]
mod tests {
    use super::super::screen::Screen;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::TermMode;

    /// A scripted full-screen diff renderer: alternate screen, app cursor keys, bracketed
    /// paste, SGR mouse with drag reporting, focus events, a hidden cursor, a beam cursor, a
    /// saved cursor, then frames drawn by absolute and relative moves that rewrite only parts
    /// of rows (EL 0, ECH, overwrites), in every SGR the serializer carries: 16/256/truecolour,
    /// bold/dim/italic, all underline styles + underline colour, inverse, hidden, strike, wide
    /// and combining glyphs, a hyperlink, a soft-wrapped line, and a scroll region it uses and
    /// drops again.
    fn fake_tui() -> Vec<u8> {
        let mut b = String::new();
        b.push_str(
            "\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?25l\x1b[5 q",
        );
        b.push_str("\x1b[H\x1b[2J");
        // frame 1
        b.push_str("\x1b[1;1H\x1b[1;38;5;208mTitle bar\x1b[0m \x1b[48;2;10;20;30m status \x1b[0m");
        b.push_str("\x1b[3;2H\x1b[3;4:3;58;2;255;0;0mcurly\x1b[0m \x1b[4:2mdouble\x1b[24m \x1b[9;2mstrike-dim\x1b[0m");
        b.push_str(
            "\x1b[4;1H\x1b[7minverse\x1b[27m \x1b[8mhidden\x1b[28m \x1b[94;101mbright\x1b[0m",
        );
        b.push_str("\x1b[5;1H宽字符 emoji 😀 e\u{301} combining");
        b.push_str(
            "\x1b[6;1H\x1b]8;id=lnk;https://example.com/x\x1b\\link text\x1b]8;;\x1b\\ after",
        );
        // a soft-wrapped line (40 cols wide screen in the tests)
        b.push_str("\x1b[8;1Hthis line is long enough to wrap onto the next row of the grid");
        // a scroll region used and reset
        b.push_str("\x1b[12;15r\x1b[15;1Hregion-a\n\nregion-b\x1b[r");
        // a saved cursor with its own pen
        b.push_str("\x1b[18;7H\x1b[32;4m\x1b7\x1b[0m");
        // frame 2: a diff — only some cells change
        b.push_str("\x1b[1;7H\x1b[1;38;5;208mBAR\x1b[0m");
        b.push_str("\x1b[3;2H\x1b[1C\x1b[3X\x1b[4;5H\x1b[K");
        b.push_str("\x1b[9;10H\x1b[2D\x1b[33mxx\x1b[0m");
        b.push_str("\x1b[20;3H\x1b[45mprompt> \x1b[0m\x1b[2mghost suggestion\x1b[0m");
        // final pen + cursor
        b.push_str("\x1b[36;1m\x1b[20;11H");
        b.into_bytes()
    }

    /// Garbage the receiving grid holds before the seed: other modes, a region, origin and
    /// insert mode, a hyperlink pen, and a sequence cut off mid-way.
    fn dirty_history() -> Vec<u8> {
        let mut b = String::new();
        b.push_str("shell history line 1\r\nline 2\r\n\x1b[?1003h\x1b[?1005h\x1b[?2004l\x1b[3;9r\x1b[?6h\x1b[4h");
        b.push_str("\x1b]8;;https://old\x1b\\\x1b[41;5mjunk junk junk\x1b[?7l\x1b[38;5");
        b.into_bytes()
    }

    fn assert_same(want: &Screen, got: &Screen) {
        let (tw, tg) = (want.term(), got.term());
        let (gw, gg) = (tw.grid(), tg.grid());
        assert_eq!(
            (gw.columns(), gw.screen_lines()),
            (gg.columns(), gg.screen_lines())
        );
        for l in 0..gw.screen_lines() as i32 {
            for c in 0..gw.columns() {
                let (a, b) = (&gw[Line(l)][Column(c)], &gg[Line(l)][Column(c)]);
                assert_eq!(a, b, "cell row {l} col {c}");
            }
        }
        let mask = !(TermMode::VI | TermMode::KITTY_KEYBOARD_PROTOCOL);
        assert_eq!(*tw.mode() & mask, *tg.mode() & mask, "modes");
        assert_eq!(gw.cursor.point, gg.cursor.point, "cursor");
        assert_eq!(
            gw.cursor.input_needs_wrap, gg.cursor.input_needs_wrap,
            "pending wrap"
        );
        assert_eq!(gw.cursor.template, gg.cursor.template, "pen");
        assert_eq!(gw.saved_cursor.point, gg.saved_cursor.point, "saved cursor");
        assert_eq!(
            gw.saved_cursor.template, gg.saved_cursor.template,
            "saved pen"
        );
        assert_eq!(tw.cursor_style(), tg.cursor_style(), "cursor style");
    }

    fn round_trip(cols: u16, rows: u16, program: &[u8]) {
        let mut src = Screen::new(cols, rows);
        src.advance(program);
        let vt = src.snapshot_vt();
        let mut fresh = Screen::new(cols, rows);
        fresh.advance(vt.as_bytes());
        assert_same(&src, &fresh);
        let mut dirty = Screen::new(cols, rows);
        dirty.advance(&dirty_history());
        dirty.advance(vt.as_bytes());
        assert_same(&src, &dirty);
        // Idempotent: serializing the rebuilt screen gives the same stream.
        assert_eq!(fresh.snapshot_vt(), vt);
    }

    #[test]
    fn a_diff_rendered_alt_screen_round_trips_cell_for_cell() {
        round_trip(40, 24, &fake_tui());
    }

    #[test]
    fn the_primary_screen_after_scrolling_round_trips() {
        let mut b = String::new();
        for i in 0..60 {
            b.push_str(&format!("\x1b[3{}mline {i}\x1b[0m output\r\n", i % 8));
        }
        b.push_str("\x1b]8;;https://x\x1b\\$ \x1b]8;;\x1b\\typed");
        round_trip(30, 10, b.as_bytes());
    }

    #[test]
    fn leaving_the_alt_screen_is_carried() {
        let mut b = fake_tui();
        b.extend_from_slice(b"\x1b[?1049l\x1b[?25h\x1b[0 qback on the primary");
        round_trip(40, 24, &b);
    }

    #[test]
    fn a_cursor_parked_past_the_last_column_keeps_its_pending_wrap() {
        round_trip(10, 4, b"\x1b[2;1H0123456789");
        let mut s = Screen::new(10, 4);
        s.advance(b"\x1b[2;1H0123456789");
        assert!(s.term().grid().cursor.input_needs_wrap);
    }

    #[test]
    fn a_wide_glyph_wrapping_at_the_edge_and_modes_off_round_trip() {
        // A wide glyph that doesn't fit leaves a leading spacer and wraps; autowrap off, insert
        // mode and origin mode set last.
        round_trip(
            9,
            5,
            "\x1b[1;1H12345678宽\x1b[?7l\x1b[4h\x1b[20h\x1b[?1000h\x1b[?6h".as_bytes(),
        );
    }

    #[test]
    fn a_blank_screen_round_trips() {
        round_trip(20, 5, b"");
    }

    /// The seed is taken under a session lock (see `SessionRegistry::screen_seed`): a big, fully
    /// styled screen must serialize fast.
    #[test]
    fn a_full_300x100_truecolour_screen_serializes_fast() {
        let mut b = String::new();
        for r in 0..100u32 {
            b.push_str(&format!("\x1b[{};1H", r + 1));
            for c in 0..300u32 {
                b.push_str(&format!(
                    "\x1b[{};38;2;{};{};{};48;5;{}m{}",
                    1 + (c % 4),
                    (r * 3) % 256,
                    (c * 7) % 256,
                    (r + c) % 256,
                    c % 256,
                    char::from(b'a' + (c % 26) as u8)
                ));
            }
        }
        let mut s = Screen::new(300, 100);
        s.advance(b.as_bytes());
        let started = std::time::Instant::now();
        let vt = s.snapshot_vt();
        let took = started.elapsed();
        eprintln!("300x100 serialize: {took:?}, {} bytes", vt.len());
        // Worst case: every cell a different truecolour SGR (~1 MB of output). Unoptimized test
        // builds are ~10x slower than release and share the host with other builds, so their
        // bound only catches a pathological regression. The seed holds no output-path lock
        // while it serializes (see `Shared::screen_seed`).
        let bound = if cfg!(debug_assertions) { 400 } else { 10 };
        assert!(took.as_millis() < bound, "took {took:?}");
        let mut back = Screen::new(300, 100);
        back.advance(vt.as_bytes());
        assert_same(&s, &back);
    }
}
