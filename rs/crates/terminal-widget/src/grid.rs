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

    /// The cursor's column (0-based grid cell). Pairs with [`cursor_row`](Self::cursor_row) to
    /// compute the type-over-selection edit sequence (how far the selection sits from the caret).
    pub fn cursor_col(&self) -> usize {
        self.term.grid().cursor.point.column.0
    }

    /// Whether the application has switched to the **alternate screen** (vim, htop, full-screen
    /// TUIs). Type-over-selection must not fire there: its arrow/backspace bytes would be app
    /// commands (vim motions), not line edits.
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    /// The cursor's row within the current viewport (display offset applied), or `None` when the
    /// cursor is scrolled out of view. Cheap — reads the grid cursor directly, no snapshot. Used
    /// to scope type-over-selection to the live prompt line (the cursor's own row).
    pub fn cursor_row(&self) -> Option<usize> {
        let grid = self.term.grid();
        let row = grid.cursor.point.line.0 + grid.display_offset() as i32;
        if row >= 0 && (row as usize) < self.size.rows {
            Some(row as usize)
        } else {
            None
        }
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

    /// Scroll the viewport by `delta_lines`: **positive scrolls up into history**, negative
    /// scrolls back toward the live edge. alacritty clamps the resulting display offset to the
    /// scrollback bounds, so an over-scroll past the top/bottom is a no-op. Marks the grid dirty
    /// only when the offset actually moved (so a clamped no-op doesn't force a repaint). Drives
    /// mouse-wheel + Shift-PageUp/Down scrollback.
    pub fn scroll_by(&mut self, delta_lines: i32) {
        if delta_lines == 0 {
            return;
        }
        let before = self.term.grid().display_offset();
        self.term.scroll_display(Scroll::Delta(delta_lines));
        if self.term.grid().display_offset() != before {
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
            // Faint/dim (SGR 2): blend the glyph colour ~55% toward the cell's effective
            // background. PSReadLine's inline prediction is the headline user of this.
            // Applied after INVERSE so dim acts on the colours actually drawn.
            if flags.contains(Flags::DIM) {
                let towards = if bg[3] > 0 { bg } else { default_bg };
                fg = dim_blend(fg, towards);
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

/// SGR 2 (faint/dim) colour: blend `fg` 55% toward `bg` — the common terminal approach
/// (a heavier blend than 50% so dim text reads clearly quieter than normal text while
/// staying legible). Alpha is kept opaque; only the hue moves toward the background.
#[inline]
fn dim_blend(fg: [u8; 4], bg: [u8; 4]) -> [u8; 4] {
    const KEEP_NUM: u32 = 45; // keep 45% of fg → 55% toward bg
    const DEN: u32 = 100;
    let mix = |f: u8, b: u8| -> u8 {
        ((f as u32 * KEEP_NUM + b as u32 * (DEN - KEEP_NUM)) / DEN) as u8
    };
    [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2]), 0xff]
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

    // ---- scroll-region correctness + throughput diagnosis (Task 17) ----
    //
    // The bench flagged native `scrolling-region` at 0.4 MB/s vs `scrolling` at 7.5 (~18x slower)
    // and hypothesized an O(n) full-grid memmove per scrolled line in the grid's DECSTBM path.
    // That hypothesis does NOT hold for this implementation: the grid is `alacritty_terminal`,
    // whose region scroll is an O(1) ring rotate (`Grid::scroll_up`) plus a few O(1) row-pointer
    // swaps for the lines fixed outside the region — no whole-grid copy to replace.
    //
    // The `#[ignore]`d diagnosis below measures it directly. Findings on this machine (4 MiB feed,
    // 120x50): feed-only full=49 region=41 MB/s (region ~= full), +snapshot full=25 region=24
    // MB/s (parity), +software-render full=1.6 region=2.4 MB/s — region is actually *faster* when
    // rendering, because rows below the 20-line region stay blank (fewer glyphs to rasterize).
    // So at the terminal-widget level scroll-region is never slower than full-screen scrolling;
    // the bench's catastrophe originates ABOVE this crate (session batching / render-pump cadence
    // / GPU upload in the app + core), which is outside this track's file scope. No grid fix is
    // warranted — these tests instead lock in scroll-within-margins correctness and feed parity.
    #[test]
    #[ignore]
    fn scroll_region_throughput_diagnosis() {
        fn mbps(bytes: usize, d: std::time::Duration) -> f64 {
            (bytes as f64 / (1024.0 * 1024.0)) / d.as_secs_f64()
        }
        // Feed `budget` bytes of `line` in pty-sized chunks after `prologue`; snapshot every
        // `snap_every` lines (0 = never) to mimic the render gate without a font dependency.
        fn run(cols: usize, rows: usize, prologue: &[u8], line: &str, budget: usize, snap_every: usize) -> std::time::Duration {
            let mut g = TermGrid::new(cols, rows);
            g.feed(prologue);
            let lb = line.as_bytes();
            let mut payload = Vec::with_capacity(budget);
            while payload.len() < budget {
                payload.extend_from_slice(lb);
            }
            let t = std::time::Instant::now();
            let mut since = 0usize;
            for chunk in payload.chunks(4096) {
                g.feed(chunk);
                if snap_every > 0 {
                    since += chunk.len() / lb.len().max(1);
                    if since >= snap_every {
                        std::hint::black_box(g.snapshot());
                        since = 0;
                    }
                }
            }
            t.elapsed()
        }
        let line = "region line 12345 — lorem ipsum dolor sit amet consectetur\r\n";
        let budget = 4 * 1024 * 1024; // 4 MiB, well past the 10k scrollback-growth ramp
        for &(cols, rows) in &[(120usize, 24usize), (120, 50), (200, 80)] {
            let full = run(cols, rows, b"", line, budget, 0);
            let region = run(cols, rows, b"\x1b[1;20r\x1b[H", line, budget, 0);
            let full_s = run(cols, rows, b"", line, budget, 16);
            let region_s = run(cols, rows, b"\x1b[1;20r\x1b[H", line, budget, 16);
            eprintln!(
                "[{cols}x{rows}] feed-only full={:.1} region={:.1} ({:.2}x) | +snapshot full={:.1} region={:.1} ({:.2}x)  MB/s",
                mbps(budget, full), mbps(budget, region), full.as_secs_f64() / region.as_secs_f64(),
                mbps(budget, full_s), mbps(budget, region_s), full_s.as_secs_f64() / region_s.as_secs_f64(),
            );
        }
    }

    /// First char of each viewport row (blanks as space) — a compact way to assert scroll layout.
    fn col0_chars(g: &TermGrid) -> Vec<char> {
        let snap = g.snapshot();
        (0..snap.rows).map(|r| snap.cell(0, r).ch).collect()
    }

    #[test]
    fn scroll_within_midscreen_region_keeps_lines_outside_it() {
        // DECSTBM region NOT anchored at the top exercises alacritty's swap-based region rotate.
        // Region = 1-based rows 2..=4 (0-based 1,2,3). Rows 0 and 4,5 are fixed.
        let mut g = TermGrid::new(8, 6);
        // Seed each row with a distinct first char via absolute cursor moves.
        g.feed(b"\x1b[1;1Ha\x1b[2;1Hb\x1b[3;1Hc\x1b[4;1Hd\x1b[5;1He\x1b[6;1Hf");
        assert_eq!(col0_chars(&g), vec!['a', 'b', 'c', 'd', 'e', 'f']);
        g.feed(b"\x1b[2;4r"); // set scroll region rows 2..=4
        g.feed(b"\x1b[4;1H"); // park the cursor on the region's bottom line (1-based row 4)
        g.feed(b"\n"); // line-feed there scrolls the region up by one
        // Region rows scroll (b drops out, blank appears at the bottom of the region); the lines
        // ABOVE (a) and BELOW (e, f) the region must be untouched.
        assert_eq!(col0_chars(&g), vec!['a', 'c', 'd', ' ', 'e', 'f']);
    }

    #[test]
    fn scroll_within_top_anchored_region_keeps_lines_below_it() {
        // The bench's case: region anchored at the top (1-based 1..=3 → 0-based 0,1,2), which
        // takes alacritty's ring-rotate + fixed-bottom swap path. Rows 3,4,5 stay put; the
        // scrolled-out top line goes to scrollback.
        let mut g = TermGrid::new(8, 6);
        g.feed(b"\x1b[1;1Ha\x1b[2;1Hb\x1b[3;1Hc\x1b[4;1Hd\x1b[5;1He\x1b[6;1Hf");
        g.feed(b"\x1b[1;3r"); // region rows 1..=3
        g.feed(b"\x1b[3;1H"); // bottom line of the region (1-based row 3)
        g.feed(b"\n");
        assert_eq!(col0_chars(&g), vec!['b', 'c', ' ', 'd', 'e', 'f']);
        // The scrolled-out 'a' went into scrollback (history grew, viewport still pinned).
        assert!(g.display_offset() == 0);
    }

    // ---- SGR attribute → RenderCell style mapping (feedback #7: PSReadLine predictions) ----

    #[test]
    fn sgr_bold_and_italic_set_style_flags() {
        let mut g = TermGrid::new(20, 4);
        g.feed(b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[1;3mZ\x1b[0m");
        let snap = g.snapshot();
        assert!(snap.cell(0, 0).bold && !snap.cell(0, 0).italic);
        assert!(snap.cell(1, 0).italic && !snap.cell(1, 0).bold);
        assert!(snap.cell(2, 0).bold && snap.cell(2, 0).italic, "SGR 1;3 sets both");
    }

    #[test]
    fn sgr_dim_blends_fg_toward_background() {
        // SGR 2 over the (transparent) default bg: the glyph colour must move toward the
        // default background — strictly darker than normal text on this dark theme, but
        // not equal to the bg (still legible).
        let mut g = TermGrid::new(20, 4);
        g.feed(b"N \x1b[2mD\x1b[0m");
        let snap = g.snapshot();
        let normal = snap.cell(0, 0).fg;
        let dim = snap.cell(2, 0).fg;
        assert_eq!(snap.cell(2, 0).ch, 'D');
        assert_ne!(dim, normal, "dim must change the drawn colour");
        for i in 0..3 {
            assert!(
                dim[i] < normal[i] && dim[i] > snap.default_bg[i],
                "dim channel {i} must sit between bg and fg (bg={} dim={} fg={})",
                snap.default_bg[i], dim[i], normal[i]
            );
        }
        // Exactly the documented 45/55 blend.
        let expect = dim_blend(normal, snap.default_bg);
        assert_eq!(dim, expect);
    }

    #[test]
    fn sgr_dim_blends_toward_explicit_cell_bg() {
        // Dim over an explicit (non-default) background blends toward THAT bg, not the
        // pane default. SGR 44 = blue bg, SGR 2 = dim.
        let mut g = TermGrid::new(20, 4);
        g.feed(b"\x1b[44;2mX\x1b[0m");
        let snap = g.snapshot();
        let c = snap.cell(0, 0);
        assert!(c.bg[3] > 0, "explicit bg must be opaque");
        assert_eq!(c.fg, dim_blend(snap.default_fg, c.bg));
    }

    #[test]
    fn sgr_dim_and_bold_combine() {
        // SGR 1;2 — alacritty tracks DIM and BOLD independently; both must survive into
        // the cell (bold face, dimmed colour).
        let mut g = TermGrid::new(20, 4);
        g.feed(b"\x1b[1;2mY\x1b[0m");
        let snap = g.snapshot();
        let c = snap.cell(0, 0);
        assert!(c.bold, "bold flag survives alongside dim");
        assert_eq!(c.fg, dim_blend(snap.default_fg, snap.default_bg));
    }

    #[test]
    fn indexed_256_and_truecolor_foregrounds_resolve() {
        // PSReadLine's default prediction colour is an indexed dim gray (e.g. 238);
        // truecolor must pass through verbatim.
        let mut g = TermGrid::new(20, 4);
        g.feed(b"\x1b[38;5;238mA\x1b[0m\x1b[38;2;10;200;30mB\x1b[0m");
        let snap = g.snapshot();
        // Index 238 is on the grayscale ramp: 8 + (238-232)*10 = 68.
        assert_eq!(snap.cell(0, 0).fg, [68, 68, 68, 0xff]);
        assert_eq!(snap.cell(1, 0).fg, [10, 200, 30, 0xff]);
    }

    #[test]
    fn psreadline_prediction_style_dim_italic_256gray() {
        // The exact shape PSReadLine emits for inline predictions: faint + italic + a dim
        // 256-colour gray, ended with SGR 0.
        let mut g = TermGrid::new(40, 4);
        g.feed(b"\x1b[2;3;38;5;238mGet-ChildItem\x1b[0m");
        let snap = g.snapshot();
        let c = snap.cell(0, 0);
        assert_eq!(c.ch, 'G');
        assert!(c.italic, "prediction renders italic");
        assert!(!c.bold);
        let gray = [68u8, 68, 68, 0xff]; // index 238
        assert_eq!(c.fg, dim_blend(gray, snap.default_bg), "dim applied on top of the 256-colour gray");
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
