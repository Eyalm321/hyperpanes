//! Rendered-screen serializer: drive an `alacritty_terminal` `Term` from the pty byte
//! stream and serialize its grid to clean text for control `mode:"screen"` reads.
//! This replaces the renderer's xterm.js serialize — a capability GAIN: screen reads
//! need no GUI.
//!
//! Exact text is **best-effort parity** with xterm. Divergences to expect:
//!   * line wrapping at the right margin may break at a different column,
//!   * wide (CJK/emoji) chars: the trailing spacer cell is dropped so the glyph
//!     appears once (xterm serialize does the same, but column counts can differ),
//!   * trailing whitespace on each line and trailing blank lines are trimmed.
//!
//! The "is this pane awaiting input?" heuristic (`control::output::detectAwaitingInput`)
//! is meant to run on THIS rendered text, not the raw stream — keep it a separate
//! concern (owned by `core-io`); this module only produces the clean screen.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

/// A live VTE screen: an `alacritty_terminal` `Term` fed incrementally from the pty
/// byte stream, plus its ANSI parser. One per session, mirroring how the renderer
/// kept a live xterm per pane. `render()` produces the clean text for a screen read.
pub struct Screen {
    term: Term<VoidListener>,
    parser: Processor,
    cols: usize,
    rows: usize,
}

impl Screen {
    /// A blank screen of `cols`×`rows`. Scrollback is disabled: screen reads only ever
    /// serialize the visible viewport, so per-session history would be wasted memory.
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = (cols as usize).max(1);
        let rows = (rows as usize).max(1);
        // Scrollback disabled: screen reads only serialize the visible viewport.
        let config = Config { scrolling_history: 0, ..Config::default() };
        let term = Term::new(config, &TermSize::new(cols, rows), VoidListener);
        Self { term, parser: Processor::new(), cols, rows }
    }

    /// Feed a raw output chunk (same bytes the renderer's terminal would receive).
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the screen grid. No-op if unchanged dimensions are passed.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = (cols as usize).max(1);
        let rows = (rows as usize).max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.term.resize(TermSize::new(cols, rows));
        self.cols = cols;
        self.rows = rows;
    }

    /// Serialize the visible grid to clean text: one line per screen row, trailing
    /// whitespace trimmed per line, trailing blank lines dropped. ANSI styling is
    /// already consumed by the parser, so the output is plain characters only.
    pub fn render(&self) -> String {
        let grid = self.term.grid();
        let mut lines: Vec<String> = Vec::with_capacity(self.rows);
        for l in 0..self.rows as i32 {
            let row = &grid[Line(l)];
            let mut s = String::with_capacity(self.cols);
            for c in 0..self.cols {
                let cell = &row[Column(c)];
                // The placeholder cell after a wide char carries a blank; skip it so
                // the wide glyph is emitted exactly once.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(cell.c);
            }
            // Empty alacritty cells hold a space; trim them off the right edge.
            while s.ends_with(' ') {
                s.pop();
            }
            lines.push(s);
        }
        while matches!(lines.last(), Some(l) if l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

/// One-shot convenience: render `bytes` onto a fresh `cols`×`rows` screen and return
/// the serialized text. Equivalent to `new` + `advance` + `render`; handy for tests
/// and for rendering a captured replay buffer without keeping a live `Screen`.
pub fn render_bytes(cols: u16, rows: u16, bytes: &[u8]) -> String {
    let mut screen = Screen::new(cols, rows);
    screen.advance(bytes);
    screen.render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_text_on_the_first_line() {
        assert_eq!(render_bytes(20, 5, b"hello world"), "hello world");
    }

    #[test]
    fn handles_crlf_line_breaks() {
        assert_eq!(render_bytes(20, 5, b"line one\r\nline two"), "line one\nline two");
    }

    #[test]
    fn trims_trailing_blank_lines_and_trailing_spaces() {
        // Cursor writes a short line then a few blank rows follow.
        assert_eq!(render_bytes(20, 6, b"top\r\n\r\n   \r\n"), "top");
    }

    #[test]
    fn clear_screen_and_home_resets_then_writes() {
        // SGR color + clear + home, then text — styling must not leak into the text.
        let bytes = b"junk\x1b[2J\x1b[H\x1b[31mRED\x1b[0m";
        assert_eq!(render_bytes(20, 5, bytes), "RED");
    }

    #[test]
    fn absolute_cursor_positioning_places_text() {
        // Move to row 3, col 5 (1-based) and write — rows 1-2 stay blank but precede
        // content, so they're kept; row 3 has content.
        let out = render_bytes(20, 5, b"\x1b[3;5HX");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "    X"); // 4 leading spaces → column 5
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        assert_eq!(render_bytes(20, 3, b"aaaa\rbb"), "bbaa");
    }

    #[test]
    fn strips_sgr_styling_sequences() {
        assert_eq!(render_bytes(40, 3, b"\x1b[1;32mgreen bold\x1b[0m text"), "green bold text");
    }

    #[test]
    fn resize_reflows_and_keeps_rendering() {
        let mut s = Screen::new(10, 4);
        s.advance(b"hello");
        assert_eq!(s.render(), "hello");
        s.resize(20, 6);
        s.advance(b" again");
        assert!(s.render().contains("hello"));
        assert!(s.render().contains("again"));
    }
}
