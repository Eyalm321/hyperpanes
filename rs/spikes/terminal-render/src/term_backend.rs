//! Terminal model + PTY plumbing, shared by both renderers.
//!
//! Owns an `alacritty_terminal::Term` (parsed via `vte::ansi::Processor`) fed by a real
//! conpty shell (`portable-pty`). A reader thread pushes raw bytes over a channel; `pump()`
//! drains them on the UI thread and advances the parser. `snapshot()` produces a flat,
//! renderer-agnostic `GridSnapshot` (resolved RGBA per cell) plus a `dirty` flag derived
//! from alacritty's own damage tracking.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor, Rgb};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::sync::mpsc::{Receiver, TryRecvError};

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

/// Forwards terminal-originated writes (DSR/DA query replies, etc.) back toward the PTY.
/// Without this, conpty issues `ESC[6n` at startup and blocks waiting for our reply —
/// the whole shell hangs. Replies are queued and drained by `pump()` on the UI thread.
struct ProxyListener {
    tx: std::sync::mpsc::Sender<Vec<u8>>,
}
impl EventListener for ProxyListener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let _ = self.tx.send(text.into_bytes());
        }
    }
}

pub struct TermBackend {
    term: Term<ProxyListener>,
    parser: Processor,
    rx: Receiver<Vec<u8>>,
    resp_rx: Receiver<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    _child: Box<dyn Child + Send + Sync>,
    palette: [Rgb; 256],
    size: TermSize,
    dirty: bool,
    pub bytes_in: u64,
}

impl TermBackend {
    pub fn new(cols: usize, rows: usize) -> anyhow::Result<Self> {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // Default shell: PowerShell on Windows feels native; fall back to COMSPEC.
        // Overridable via SPIKE_SHELL for debugging.
        let shell = std::env::var("SPIKE_SHELL").ok();
        let mut cmd = match shell.as_deref() {
            Some(s) => CommandBuilder::new(s),
            None if cfg!(windows) => CommandBuilder::new("powershell.exe"),
            None => CommandBuilder::new_default_prog(),
        };
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut first = true;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        eprintln!("[pty] reader EOF");
                        break;
                    }
                    Err(e) => {
                        eprintln!("[pty] reader error: {e}");
                        break;
                    }
                    Ok(n) => {
                        if first || std::env::var("SPIKE_DUMP").is_ok() {
                            let preview: String = buf[..n.min(48)]
                                .iter()
                                .map(|b| {
                                    if b.is_ascii_graphic() || *b == b' ' {
                                        (*b as char).to_string()
                                    } else {
                                        format!("\\x{b:02x}")
                                    }
                                })
                                .collect();
                            eprintln!("[pty] read {n} bytes: {preview}");
                            first = false;
                        }
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let size = TermSize { cols, rows };
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let term = Term::new(Config::default(), &size, ProxyListener { tx: resp_tx });

        Ok(TermBackend {
            term,
            parser: Processor::new(),
            rx,
            resp_rx,
            master: pair.master,
            writer,
            _child: child,
            palette: default_palette(),
            size,
            dirty: true,
            bytes_in: 0,
        })
    }

    /// Drain pending PTY bytes into the parser. Returns true if anything changed.
    pub fn pump(&mut self) -> bool {
        let mut got = false;
        loop {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    self.bytes_in += chunk.len() as u64;
                    self.parser.advance(&mut self.term, &chunk);
                    got = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        // Forward any terminal-originated replies (DSR/DA/etc.) back to the shell.
        while let Ok(resp) = self.resp_rx.try_recv() {
            let _ = self.writer.write_all(&resp);
            let _ = self.writer.flush();
        }
        if got {
            self.dirty = true;
        }
        got
    }

    pub fn write_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.size.cols && rows == self.size.rows {
            return;
        }
        self.size = TermSize { cols, rows };
        self.term.resize(self.size);
        let _ = self.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.dirty = true;
    }

    pub fn take_dirty(&mut self) -> bool {
        let d = self.dirty;
        self.dirty = false;
        // Clear accumulated damage so it doesn't grow unbounded. (A real impl would
        // consume `term.damage()` to repaint only changed line ranges; the spike repaints
        // the whole pane texture per dirty frame — see RESULTS.md.)
        self.term.reset_damage();
        d
    }

    pub fn size(&self) -> TermSize {
        self.size
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

    pub fn snapshot(&self) -> GridSnapshot {
        let cols = self.size.cols;
        let rows = self.size.rows;
        let mut cells = vec![RenderCell::default(); cols * rows];
        let default_fg = [
            self.palette[7].r,
            self.palette[7].g,
            self.palette[7].b,
            0xff,
        ];
        let default_bg = [
            self.palette[0].r,
            self.palette[0].g,
            self.palette[0].b,
            0xff,
        ];

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
                if bg[3] == 0 {
                    bg = default_fg;
                }
            }
            let rc = RenderCell {
                ch: cell.c,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline: flags.contains(Flags::UNDERLINE)
                    || flags.contains(Flags::DOUBLE_UNDERLINE),
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
        p[i] = Rgb {
            r: c[0],
            g: c[1],
            b: c[2],
        };
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
