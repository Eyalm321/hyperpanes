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
use serde::{Deserialize, Serialize};

/// What sits in a Claude-Code-style prompt box (`❯` under a `─` rule) on a screen read.
///
/// Claude Code draws its own next-prompt **suggestion** (ghost text) faint (SGR 2) in the
/// input box, and text a person typed in a normal colour. A plain-text screen read cannot tell
/// the two apart, so a watcher reading the box would report Claude's suggestion as unsent
/// owner input. The grid keeps the cell flags, so the read can say which it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputLine {
    /// The box's text (rows after the `❯`, joined with `\n`, trimmed). For a suggestion this
    /// is the ghost text, which nobody typed.
    pub text: String,
    pub kind: InputLineKind,
}

/// Who put the text in the prompt box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputLineKind {
    /// Nothing in the box.
    Empty,
    /// At least one visible character is drawn normally: someone typed it (or pasted it).
    Typed,
    /// Every visible character is drawn faint: the agent's own suggested next prompt (or a
    /// placeholder hint). Never treat it as input someone authored.
    Suggestion,
}

/// A screen read: the clean text plus the prompt box, when the screen has one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRead {
    pub text: String,
    pub input_line: Option<InputLine>,
}

/// The prompt glyph Claude Code draws at the start of its input box.
const PROMPT: char = '\u{276F}';

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\u{a0}' || c == '\0'
}

fn is_rule(c: char) -> bool {
    matches!(c, '\u{2500}' | '\u{256D}' | '\u{2570}')
}

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
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        let term = Term::new(config, &TermSize::new(cols, rows), VoidListener);
        Self {
            term,
            parser: Processor::new(),
            cols,
            rows,
        }
    }

    /// Feed a raw output chunk (same bytes the renderer's terminal would receive).
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Current grid dimensions `(cols, rows)` — what remote clients must emulate at.
    pub fn dims(&self) -> (u16, u16) {
        (self.cols as u16, self.rows as u16)
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
        self.render_text()
    }

    /// [`render`](Self::render) plus the prompt box classification ([`InputLine`]).
    pub fn render_read(&self) -> ScreenRead {
        ScreenRead {
            text: self.render_text(),
            input_line: self.input_line(),
        }
    }

    /// The Claude-Code-style prompt box: the LAST row whose first visible glyph is `❯` and
    /// whose row above starts with a `─` rule (a `❯` in a selection menu has no rule above
    /// it), plus its continuation rows down to the next rule. `None` when there is no box.
    pub fn input_line(&self) -> Option<InputLine> {
        let grid = self.term.grid();
        let first_visible = |l: usize| -> Option<(usize, char)> {
            let row = &grid[Line(l as i32)];
            (0..self.cols)
                .map(|c| (c, row[Column(c)].c))
                .find(|&(_, ch)| !is_blank(ch))
        };
        let start = (1..self.rows).rev().find(|&l| {
            matches!(first_visible(l), Some((_, PROMPT)))
                && matches!(first_visible(l - 1), Some((_, ch)) if is_rule(ch))
        })?;
        let mut rows_text: Vec<String> = Vec::new();
        let (mut visible, mut faint) = (0usize, 0usize);
        for l in start..self.rows {
            let from = if l == start {
                first_visible(l).map_or(0, |(c, _)| c + 1)
            } else {
                match first_visible(l) {
                    Some((_, ch)) if is_rule(ch) => break,
                    _ => 0,
                }
            };
            let row = &grid[Line(l as i32)];
            let mut s = String::new();
            for c in from..self.cols {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let ch = if is_blank(cell.c) { ' ' } else { cell.c };
                if ch != ' ' {
                    visible += 1;
                    if cell.flags.contains(Flags::DIM) {
                        faint += 1;
                    }
                }
                s.push(ch);
            }
            rows_text.push(s.trim().to_string());
        }
        while matches!(rows_text.last(), Some(r) if r.is_empty()) {
            rows_text.pop();
        }
        let kind = if visible == 0 {
            InputLineKind::Empty
        } else if faint == visible {
            InputLineKind::Suggestion
        } else {
            InputLineKind::Typed
        };
        Some(InputLine {
            text: rows_text.join("\n").trim().to_string(),
            kind,
        })
    }

    fn render_text(&self) -> String {
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
        assert_eq!(
            render_bytes(20, 5, b"line one\r\nline two"),
            "line one\nline two"
        );
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
        assert_eq!(
            render_bytes(40, 3, b"\x1b[1;32mgreen bold\x1b[0m text"),
            "green bold text"
        );
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

    // ---- prompt box (InputLine) -------------------------------------------------------------
    // Byte sequences captured from a live Claude Code pane (hyperpanes t36): the suggestion is
    // `❯` NBSP + SGR 2 … SGR 22; typed text is `❯ ` + a truecolor foreground.

    /// A 60x6 screen with a rule, the prompt row bytes, and a closing rule below.
    fn boxed(prompt_row: &[u8]) -> Screen {
        let mut s = Screen::new(60, 6);
        s.advance(b"some agent output\r\n");
        s.advance("──────────── goals-hyperpanes ─\r\n".as_bytes());
        s.advance(prompt_row);
        s.advance("\r\n────────────────────────────────\r\n".as_bytes());
        s.advance(b"  status line");
        s
    }

    #[test]
    fn faint_text_in_the_box_is_a_suggestion() {
        let s = boxed("❯\u{a0}\x1b[2mok let me know what it finds\x1b[22m".as_bytes());
        let line = s.input_line().expect("a prompt box");
        assert_eq!(line.kind, InputLineKind::Suggestion);
        assert_eq!(line.text, "ok let me know what it finds");
        // The plain text read is unchanged: the two kinds look the same there.
        assert!(s.render().contains("ok let me know what it finds"));
    }

    #[test]
    fn coloured_text_in_the_box_is_typed() {
        let s = boxed("❯ \x1b[38;2;255;255;255myes write it up for the lead\x1b[39m".as_bytes());
        let line = s.input_line().expect("a prompt box");
        assert_eq!(line.kind, InputLineKind::Typed);
        assert_eq!(line.text, "yes write it up for the lead");
    }

    #[test]
    fn plain_default_colour_text_is_typed() {
        let s = boxed("❯ give me the rpm command".as_bytes());
        assert_eq!(s.input_line().unwrap().kind, InputLineKind::Typed);
    }

    #[test]
    fn an_empty_box_is_empty() {
        let s = boxed("❯\u{a0}".as_bytes());
        let line = s.input_line().unwrap();
        assert_eq!(line.kind, InputLineKind::Empty);
        assert_eq!(line.text, "");
    }

    #[test]
    fn mixed_typed_and_faint_counts_as_typed() {
        let s = boxed("❯ ship it\x1b[2m in 0.0.38\x1b[22m".as_bytes());
        assert_eq!(s.input_line().unwrap().kind, InputLineKind::Typed);
    }

    #[test]
    fn a_typed_line_that_wraps_keeps_its_continuation_rows() {
        let mut s = Screen::new(30, 8);
        s.advance("──────────────────────────────\r\n".as_bytes());
        s.advance("❯ first part of a long\r\n  second part\r\n".as_bytes());
        s.advance("──────────────────────────────".as_bytes());
        let line = s.input_line().unwrap();
        assert_eq!(line.kind, InputLineKind::Typed);
        assert_eq!(line.text, "first part of a long\nsecond part");
    }

    #[test]
    fn a_menu_cursor_without_a_rule_above_is_not_a_prompt_box() {
        let s = render_read_of(40, 5, "Proceed?\r\n❯ 1. Yes\r\n  2. No".as_bytes());
        assert_eq!(s.input_line, None);
        assert!(s.text.contains("❯ 1. Yes"));
    }

    #[test]
    fn no_prompt_glyph_means_no_box() {
        assert_eq!(render_read_of(20, 3, b"$ ls").input_line, None);
    }

    #[test]
    fn the_last_box_on_screen_wins() {
        let mut s = boxed("❯ \x1b[38;2;255;255;255mold typed\x1b[39m".as_bytes());
        s.advance("\r\n──────────\r\n❯\u{a0}\x1b[2mnew ghost\x1b[22m".as_bytes());
        let line = s.input_line().unwrap();
        assert_eq!(line.kind, InputLineKind::Suggestion);
        assert_eq!(line.text, "new ghost");
    }

    #[test]
    fn input_line_serializes_with_a_lowercase_kind() {
        let v = serde_json::to_value(InputLine {
            text: "x".into(),
            kind: InputLineKind::Suggestion,
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "text": "x", "kind": "suggestion" }));
    }

    fn render_read_of(cols: u16, rows: u16, bytes: &[u8]) -> ScreenRead {
        let mut s = Screen::new(cols, rows);
        s.advance(bytes);
        s.render_read()
    }
}
