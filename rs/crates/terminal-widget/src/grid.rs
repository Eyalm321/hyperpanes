//! The terminal grid model — an `alacritty_terminal::Term` fed by a `vte` parser,
//! producing a renderer-agnostic [`GridSnapshot`] for the [`crate::render`] backends.
//!
//! Lifted from Spike A's `term_backend.rs`, but with the **PTY removed**: in the real
//! app the live shell is owned by `hyperpanes_core::session_manager`, which hands us
//! already-batched UTF-8 output via `SessionEvent::Data`. So this type is a pure model:
//!
//!   * [`TermGrid::feed`] — advance the parser with raw session bytes.
//!   * [`TermGrid::take_replies`] — drain terminal-originated writes (DSR/DA query
//!     replies). The conpty issues `ESC[6n` at startup and **blocks until answered**, so
//!     the controller must forward these back to the session's pty (via
//!     `SessionManager::write`). Without it the shell hangs with no output.
//!   * [`TermGrid::resize`] — reflow the grid (the pty resize is the session's job).
//!   * [`TermGrid::snapshot`] — a flat, resolved-RGBA grid for any `PaneRenderer`.
//!   * [`TermGrid::take_dirty`] — damage-gated repaint flag.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor, Rgb};
use std::sync::mpsc::{channel, Receiver, Sender};

/// A single resolved cell ready for rasterization.
#[derive(Clone, Copy)]
pub struct RenderCell {
    pub ch: char,
    pub fg: [u8; 4],
    pub bg: [u8; 4],
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Left half of a wide (CJK) glyph — render glyph, occupies 2 columns.
    pub wide: bool,
    /// Right-half spacer of a wide glyph — skip glyph, keep bg.
    pub wide_spacer: bool,
}

impl Default for RenderCell {
    fn default() -> Self {
        RenderCell {
            ch: ' ',
            fg: [0xc0, 0xca, 0xf5, 0xff],
            bg: [0, 0, 0, 0],
            bold: false,
            italic: false,
            underline: false,
            wide: false,
            wide_spacer: false,
        }
    }
}

/// A flat, renderer-agnostic snapshot of the grid: resolved RGBA per cell so renderers
/// never touch alacritty/vte types.
pub struct GridSnapshot {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<RenderCell>,
    pub cursor: (usize, usize), // (col, row) in viewport
    pub cursor_visible: bool,
    pub default_bg: [u8; 4],
    pub default_fg: [u8; 4],
}

impl GridSnapshot {
    #[inline]
    pub fn cell(&self, col: usize, row: usize) -> &RenderCell {
        &self.cells[row * self.cols + col]
    }
}

/// Implements `alacritty_terminal::grid::Dimensions` so `Term::new`/`resize` accept our size.
#[derive(Clone, Copy)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}
impl alacritty_terminal::grid::Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Captures terminal-originated writes (DSR/DA query replies, etc.) so the controller can
/// forward them back to the session's pty. Without this, conpty's startup `ESC[6n` blocks
/// the whole shell. Replies are queued and drained by [`TermGrid::take_replies`].
struct ProxyListener {
    tx: Sender<Vec<u8>>,
}
impl EventListener for ProxyListener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let _ = self.tx.send(text.into_bytes());
        }
    }
}

/// The terminal grid: a parser-fed `Term` plus a resolved 256-colour palette. Owns no
/// PTY — see the module docs.
pub struct TermGrid {
    term: Term<ProxyListener>,
    parser: Processor,
    resp_rx: Receiver<Vec<u8>>,
    palette: [Rgb; 256],
    size: TermSize,
    dirty: bool,
}

impl TermGrid {
    /// A fresh grid sized to `cols`×`rows` (each clamped to ≥1).
    pub fn new(cols: usize, rows: usize) -> Self {
        let size = TermSize {
            cols: cols.max(1),
            rows: rows.max(1),
        };
        let (resp_tx, resp_rx) = channel::<Vec<u8>>();
        let term = Term::new(Config::default(), &size, ProxyListener { tx: resp_tx });
        TermGrid {
            term,
            parser: Processor::new(),
            resp_rx,
            palette: default_palette(),
            size,
            dirty: true,
        }
    }

    /// Apply a colour theme by overriding the 16 base ANSI colours (indices 0–15; index 0 is
    /// the default background, index 7 the default foreground). The 6×6×6 colour cube and the
    /// grayscale ramp (indices 16–255) are kept. Marks the grid dirty so it repaints.
    pub fn set_base16(&mut self, base: [[u8; 3]; 16]) {
        for (i, c) in base.iter().enumerate() {
            self.palette[i] = Rgb { r: c[0], g: c[1], b: c[2] };
        }
        self.dirty = true;
    }

    /// Advance the VTE parser with a chunk of raw session output. Marks the grid dirty.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, bytes);
        self.dirty = true;
    }

    /// Drain any terminal-originated replies (DSR/DA/etc.) the parser queued while
    /// processing the last [`feed`](Self::feed). The controller must write these back to
    /// the session's pty (`SessionManager::write`) or the shell can hang at startup.
    /// Returns an empty vec when there is nothing to forward.
    pub fn take_replies(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(chunk) = self.resp_rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        out
    }

    /// Reflow the grid to `cols`×`rows`. Returns `true` if the size actually changed (the
    /// caller should then also resize the *session* via `SessionManager::resize`).
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.size.cols && rows == self.size.rows {
            return false;
        }
        self.size = TermSize { cols, rows };
        self.term.resize(self.size);
        self.dirty = true;
        true
    }

    /// Take-and-clear the dirty flag. Also clears alacritty's accumulated damage so it
    /// doesn't grow unbounded (the renderers repaint the whole pane texture per dirty
    /// frame — partial-damage repaint is a noted future refinement).
    pub fn take_dirty(&mut self) -> bool {
        let d = self.dirty;
        self.dirty = false;
        self.term.reset_damage();
        d
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    /// Whether the application has enabled **bracketed paste** mode (DECSET 2004). When on, a
    /// paste must be wrapped in `ESC[200~ … ESC[201~` so the shell/line-editor (e.g. PSReadLine)
    /// treats it as one literal insertion instead of replaying each embedded newline as Enter —
    /// which is what fragments a multi-line paste across `>>` continuation prompts and strands the
    /// caret. See [`TerminalPane::paste_from_clipboard`](crate::pane::TerminalPane).
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
    }

    // ---- Scrollback access + scrolling (for in-pane search) ---------------------------------

    /// How far the viewport is scrolled up into history, in lines (0 = pinned to the bottom /
    /// live prompt). Viewport row `r` shows absolute grid line `r - display_offset`.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Every grid line — scrollback history included — as `(absolute_line, text)`, top to bottom.
    /// Absolute lines are negative for scrollback and `0..rows` for the live viewport (the same
    /// numbering the snapshot/cursor use). One char per cell, blanks as spaces. Used by the
    /// search model; O(history) so call it on query change, not per frame.
    pub fn history_lines(&self) -> Vec<(i32, String)> {
        let grid = self.term.grid();
        let cols = grid.columns();
        let top = grid.topmost_line().0;
        let bottom = grid.bottommost_line().0;
        let mut out = Vec::with_capacity((bottom - top + 1).max(0) as usize);
        for line_i in top..=bottom {
            let row = &grid[Line(line_i)];
            let mut s = String::with_capacity(cols);
            for c in 0..cols {
                let ch = row[Column(c)].c;
                s.push(if ch == '\0' { ' ' } else { ch });
            }
            out.push((line_i, s));
        }
        out
    }

    /// Scroll so absolute grid `line` is visible (roughly centered), unless it already is. Marks
    /// the grid dirty when the offset actually moves. Used to reveal the active search match.
    pub fn scroll_to_visible(&mut self, line: i32) {
        let (cur, hist, rows) = {
            let g = self.term.grid();
            (
                g.display_offset() as i32,
                g.history_size() as i32,
                self.size.rows as i32,
            )
        };
        let row = line + cur;
        if row >= 0 && row < rows {
            return; // already on screen
        }
        let desired = (rows / 2 - line).clamp(0, hist);
        let delta = desired - cur;
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
            self.dirty = true;
        }
    }

    /// Pin the viewport back to the bottom (the live prompt), e.g. when search closes.
    pub fn scroll_to_bottom(&mut self) {
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
            self.dirty = true;
        }
    }

    fn resolve(&self, c: AnsiColor, default_fg: bool) -> [u8; 4] {
        let rgb = match c {
            AnsiColor::Spec(rgb) => rgb,
            AnsiColor::Indexed(i) => self.palette[i as usize],
            AnsiColor::Named(n) => match n {
                NamedColor::Foreground => self.palette[7],
                NamedColor::Background => self.palette[0],
                other => {
                    let idx = other as usize;
                    if idx < 256 {
                        self.palette[idx.min(15)]
                    } else if default_fg {
                        self.palette[7]
                    } else {
                        self.palette[0]
                    }
                }
            },
        };
        [rgb.r, rgb.g, rgb.b, 0xff]
    }

    /// Produce a flat, resolved-RGBA snapshot of the current viewport for a renderer.
    pub fn snapshot(&self) -> GridSnapshot {
        let cols = self.size.cols;
        let rows = self.size.rows;
        let mut cells = vec![RenderCell::default(); cols * rows];
        let default_fg = [self.palette[7].r, self.palette[7].g, self.palette[7].b, 0xff];
        let default_bg = [self.palette[0].r, self.palette[0].g, self.palette[0].b, 0xff];

        let content = self.term.renderable_content();
        let display_offset = content.display_offset as i32;
        for indexed in content.display_iter {
            let point: Point = indexed.point;
            let cell = indexed.cell;
            // Map absolute line to viewport row.
            let row = point.line.0 + display_offset;
            if row < 0 || row as usize >= rows {
                continue;
            }
            let row = row as usize;
            let col = point.column.0;
            if col >= cols {
                continue;
            }
            let flags = cell.flags;
            let mut fg = self.resolve(cell.fg, true);
            let mut bg = self.resolve(cell.bg, false);
            // Background defaults to transparent so the pane bg shows through.
            if matches!(cell.bg, AnsiColor::Named(NamedColor::Background)) {
                bg = [0, 0, 0, 0];
            }
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
                // After the swap a default-background cell yields a transparent bg (and,
                // for the glyph, a transparent fg). Reverse video must stay legible, so
                // fall back to the resolved defaults: default-fg as the block colour and
                // default-bg as the (now inverted) text colour. Without the fg fallback the
                // glyph paints with alpha 0 → an empty colour block (e.g. pwsh's
                // reverse-video update notice rendered as a solid bar).
                if bg[3] == 0 {
                    bg = default_fg;
                }
                if fg[3] == 0 {
                    fg = default_bg;
                }
            }
            let rc = RenderCell {
                ch: cell.c,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.contains(Flags::UNDERLINE) || flags.contains(Flags::DOUBLE_UNDERLINE),
                wide: flags.contains(Flags::WIDE_CHAR),
                wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
            };
            cells[row * cols + col] = rc;
        }

        // Cursor in viewport coordinates.
        let cpoint = content.cursor.point;
        let crow = cpoint.line.0 + display_offset;
        let cursor_visible = crow >= 0 && (crow as usize) < rows && (cpoint.column.0) < cols;
        let cursor = if cursor_visible {
            (cpoint.column.0, crow as usize)
        } else {
            (0, 0)
        };

        GridSnapshot {
            cols,
            rows,
            cells,
            cursor,
            cursor_visible,
            default_bg,
            default_fg,
        }
    }
}

/// Tokyo-Night-ish default 16 + the standard xterm 256-colour cube/grayscale.
fn default_palette() -> [Rgb; 256] {
    let mut p = [Rgb { r: 0, g: 0, b: 0 }; 256];
    let base: [[u8; 3]; 16] = [
        [0x16, 0x16, 0x1e], // 0 black (used as default bg)
        [0xf7, 0x76, 0x8e], // 1 red
        [0x9e, 0xce, 0x6a], // 2 green
        [0xe0, 0xaf, 0x68], // 3 yellow
        [0x7a, 0xa2, 0xf7], // 4 blue
        [0xbb, 0x9a, 0xf7], // 5 magenta
        [0x7d, 0xcf, 0xff], // 6 cyan
        [0xc0, 0xca, 0xf5], // 7 white (used as default fg)
        [0x41, 0x48, 0x68], // 8 bright black
        [0xf7, 0x76, 0x8e], // 9 bright red
        [0x9e, 0xce, 0x6a], // 10 bright green
        [0xe0, 0xaf, 0x68], // 11 bright yellow
        [0x7a, 0xa2, 0xf7], // 12 bright blue
        [0xbb, 0x9a, 0xf7], // 13 bright magenta
        [0x7d, 0xcf, 0xff], // 14 bright cyan
        [0xff, 0xff, 0xff], // 15 bright white
    ];
    for (i, c) in base.iter().enumerate() {
        p[i] = Rgb { r: c[0], g: c[1], b: c[2] };
    }
    // 6x6x6 colour cube (indices 16..=231).
    let levels = [0u8, 95, 135, 175, 215, 255];
    let mut idx = 16;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                p[idx] = Rgb {
                    r: levels[r],
                    g: levels[g],
                    b: levels[b],
                };
                idx += 1;
            }
        }
    }
    // Grayscale ramp (indices 232..=255).
    for i in 0..24 {
        let v = 8 + i as u8 * 10;
        p[232 + i] = Rgb { r: v, g: v, b: v };
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_advances_grid_and_sets_dirty() {
        let mut g = TermGrid::new(20, 4);
        assert!(g.take_dirty()); // starts dirty
        assert!(!g.take_dirty()); // cleared
        g.feed(b"hi");
        assert!(g.take_dirty());
        let snap = g.snapshot();
        assert_eq!(snap.cell(0, 0).ch, 'h');
        assert_eq!(snap.cell(1, 0).ch, 'i');
    }

    #[test]
    fn resize_reports_change_only_when_size_differs() {
        let mut g = TermGrid::new(20, 4);
        assert!(!g.resize(20, 4));
        assert!(g.resize(40, 10));
        assert_eq!(g.size().cols, 40);
        assert_eq!(g.size().rows, 10);
    }

    #[test]
    fn inverse_default_cell_keeps_a_visible_glyph() {
        // ESC[7m = reverse video. Over the default bg, the swap must NOT leave the glyph
        // colour transparent, else reverse-video text vanishes into a solid colour block.
        let mut g = TermGrid::new(20, 4);
        g.feed(b"\x1b[7mX\x1b[0m");
        let snap = g.snapshot();
        let c = snap.cell(0, 0);
        assert_eq!(c.ch, 'X');
        assert_eq!(c.fg[3], 0xff, "inverse glyph colour must be opaque (visible)");
        assert_eq!(c.bg[3], 0xff, "inverse background must be opaque");
        // The block paints in the default fg; the glyph in the default bg.
        assert_eq!([c.bg[0], c.bg[1], c.bg[2]], [snap.default_fg[0], snap.default_fg[1], snap.default_fg[2]]);
        assert_eq!([c.fg[0], c.fg[1], c.fg[2]], [snap.default_bg[0], snap.default_bg[1], snap.default_bg[2]]);
    }

    #[test]
    fn bracketed_paste_mode_tracks_decset_2004() {
        let mut g = TermGrid::new(20, 4);
        assert!(!g.bracketed_paste(), "off by default");
        g.feed(b"\x1b[?2004h"); // DECSET 2004 — app turns bracketed paste ON (PSReadLine does)
        assert!(g.bracketed_paste(), "enabled after DECSET 2004h");
        g.feed(b"\x1b[?2004l"); // DECRST — back off
        assert!(!g.bracketed_paste(), "disabled after DECRST 2004l");
    }

    #[test]
    fn dsr_query_produces_a_reply_to_forward() {
        let mut g = TermGrid::new(20, 4);
        // ESC[6n — Device Status Report (cursor position). The terminal must answer or a
        // real conpty blocks; we forward the queued reply to the pty.
        g.feed(b"\x1b[6n");
        let reply = g.take_replies();
        assert!(!reply.is_empty(), "DSR must produce a reply to forward");
        // A CPR reply looks like ESC [ <row> ; <col> R.
        assert_eq!(reply[0], 0x1b);
        assert_eq!(*reply.last().unwrap(), b'R');
    }
}
