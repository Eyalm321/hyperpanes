//! [`TerminalPane`] — the reusable Rust controller for one terminal pane.
//!
//! It owns the renderer-agnostic grid model ([`TermGrid`]) and a chosen [`PaneRenderer`]
//! (software or GPU), and exposes a small, Wave-2-friendly API. It is deliberately
//! **decoupled from the session transport**: the caller pumps it.
//!
//! ## Lifecycle (how the app-shell drives N of these)
//! 1. Spawn/attach a session in `hyperpanes_core::session_manager` sized to your initial
//!    `cols`×`rows`, and construct a `TerminalPane` of the same size.
//! 2. On each `SessionEvent::Data { data, .. }` for this pane → [`feed`](Self::feed), then
//!    drain [`take_replies`](Self::take_replies) and `SessionManager::write` them back
//!    (DSR/DA answers — the shell hangs without them).
//! 3. On a Slint key event → [`crate::keys::encode_key`] → `SessionManager::write`.
//! 4. On a geometry change → compute new `cols`×`rows` from the pixel size and the font
//!    cell metrics, [`resize`](Self::resize) the pane, and `SessionManager::resize` the
//!    session if it returns `true`.
//! 5. Each frame (a Slint `Timer`): if [`take_dirty`](Self::take_dirty) (or the cursor
//!    blink flipped) → [`render`](Self::render) and hand the `slint::Image` to your model.
//!
//! The `Font` is passed in at render time so a whole fleet of panes can share one glyph
//! cache (it is `&mut` because rasterization is lazy/cached).

use crate::clipboard::Clipboard;
use crate::font::Font;
use crate::grid::TermGrid;
use crate::links::{extract_path_candidates, extract_url_candidates, UrlCandidate};
use crate::render::{PaneRenderer, RenderOpts};
use crate::search::{self, Match};
use crate::selection::{self, Selection};
use hyperpanes_core::paths::{self, OpenResult, ResolveResult};
use slint::Image;
use std::collections::HashMap;
use std::time::Instant;

/// How long a copy/paste indicator ("toast") stays up, in ms — matches the Electron pane's
/// 1.6s auto-dismiss in `Terminal.tsx`.
const TOAST_MS: u128 = 1600;

/// Minimum pointer travel from the press point (logical px) before a left-press is treated as a
/// drag-select rather than a click. A click frequently twitches a pixel or two; if that twitch
/// straddles a cell boundary the selection would otherwise flip to `dragged` and copy-on-select,
/// clobbering the clipboard right before a paste (the `$c.Dispose()`-rides-along bug). Below this
/// slop the head never tracks, so a click can never copy — matching the few-px dead zone xterm /
/// the Electron pane allow before a drag begins.
const DRAG_THRESHOLD_PX: f32 = 4.0;

/// Controller for a single terminal pane: grid model + a pluggable renderer.
pub struct TerminalPane {
    grid: TermGrid,
    renderer: Box<dyn PaneRenderer>,
    /// This pane's working directory, used to resolve relative path tokens (the renderer-side
    /// half of `core::paths`). `None` falls back to the home dir, matching the pty start dir.
    cwd: Option<String>,
    /// Verified paths cached for this pane's lifetime, keyed by `cwd\x1ftoken`. Only *existing*
    /// paths are cached (negatives aren't), so a file the shell creates becomes clickable on the
    /// next hover — mirroring the Electron renderer's `verified` map.
    verified: HashMap<String, ResolveResult>,
    /// The live drag-selection, if any (our own cell-range model — see [`crate::selection`]).
    /// `None` until a press starts one; a non-dragged selection (a plain click) is held but
    /// renders nothing, so the same press can still resolve to a link click.
    selection: Option<Selection>,
    /// The (logical-px) point of the active selection press, used to gate a real drag from a click
    /// twitch: the head only starts tracking once the pointer moves past [`DRAG_THRESHOLD_PX`] from
    /// here. `None` when no press is in flight.
    select_origin: Option<(f32, f32)>,
    /// System clipboard handle for copy-on-select / right-click paste (kept open for the pane's
    /// life — see [`crate::clipboard`]).
    clipboard: Clipboard,
    /// The transient copy/paste indicator ("toast") + when it was raised; auto-expires after
    /// [`TOAST_MS`]. Drained by [`toast_text`](Self::toast_text).
    toast: Option<(String, Instant)>,
    /// Whether the in-pane search box (Ctrl+F) is open.
    search_shown: bool,
    /// The current search query (the search box text).
    search_query: String,
    /// All matches for `search_query` across the grid + scrollback, top to bottom.
    search_matches: Vec<Match>,
    /// Index into `search_matches` of the active (highlighted/revealed) match, if any.
    search_index: Option<usize>,
    /// When the scrollback viewport was last moved by an explicit scroll gesture (wheel /
    /// Shift+PageUp/Down / scroll-to-edge). Drives the vim-style scrollbar's show-then-fade: the
    /// bar is opaque for [`SCROLLBAR_SHOW_MS`], fades over [`SCROLLBAR_FADE_MS`], then is hidden.
    /// `None` once it has fully faded. NOT stamped by the keystroke-driven snap-to-bottom, so the
    /// bar never flashes while you type.
    scroll_activity: Option<Instant>,
    /// The live drag-selection pointer `(x, y, surf_w, surf_h)` in logical px while the button is
    /// held, for edge-autoscroll: when it sits in the top/bottom edge band, the pump's
    /// [`selection_autoscroll_tick`](Self::selection_autoscroll_tick) scrolls the viewport and
    /// grows the selection into off-screen scrollback. `None` when no drag is in flight.
    drag_pointer: Option<(f32, f32, f32, f32)>,
}

/// How long the scrollbar stays fully opaque after a scroll gesture before it begins to fade.
const SCROLLBAR_SHOW_MS: u128 = 900;
/// How long the scrollbar takes to fade out once the show window elapses.
const SCROLLBAR_FADE_MS: u128 = 350;
/// Minimum scrollbar thumb height (logical px) so it stays grabbable/visible on a huge buffer.
const SCROLLBAR_MIN_THUMB_PX: f32 = 24.0;
/// Lines scrolled per wheel notch (mirrors the widget's `scroll-requested(±3)` magnitude) — used
/// to collapse a notch back to one mouse-wheel report when forwarding to a mouse-grabbing app.
const WHEEL_LINES_PER_NOTCH: i32 = 3;

/// A link under the cursor — an on-disk-verified path or an http/https URL: where to draw the
/// hover underline (in the pane's *logical* pixel space) plus the target. Returned by
/// [`TerminalPane::link_at`].
#[derive(Debug, Clone, PartialEq)]
pub struct LinkHit {
    /// Underline rect in logical px within the pane surface.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Absolute, verified path the link points at — or the URL itself when [`is_url`](Self::is_url).
    pub abs_path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Tooltip label (`abs_path` with any `:line[:col]` suffix appended; the URL verbatim).
    pub tip: String,
    /// `true` for an http/https URL (opens in the default browser, never disk-verified).
    pub is_url: bool,
}

/// The outcome of activating (clicking) a link. Mirrors the Electron split: a plain click opens
/// (editor / OS default / browser for URLs), Ctrl/Cmd-click copies the target.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkAction {
    /// Ctrl/Cmd-click — the caller should copy this absolute path (or URL) to the clipboard.
    Copy(String),
    /// Plain click — the file/dir/URL was opened (the result carries blocked/err detail).
    Opened(OpenResult),
}

/// Open an http/https URL in the default browser, detached. Same launch mechanism as
/// `core::paths`' `os_open` (`cmd /C start "" <target>` on Windows, `open`/`xdg-open` elsewhere),
/// but the URL is passed pre-quoted via `raw_arg` so cmd metacharacters in query strings
/// (`?a=1&b=2`) can't split the command — the extractor guarantees a URL never contains `"`.
fn open_url(url: &str) -> OpenResult {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    let spawn = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000; // don't flash a console (see core's NoWindow)
        Command::new("cmd")
            .raw_arg("/C")
            .raw_arg("start")
            .raw_arg("\"\"") // window-title arg, so the quoted URL isn't consumed as the title
            .raw_arg(format!("\"{url}\""))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    };
    #[cfg(target_os = "macos")]
    let spawn = Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawn = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawn {
        Ok(_) => OpenResult {
            ok: true,
            blocked: false,
            error: None,
        },
        Err(e) => OpenResult {
            ok: false,
            blocked: false,
            error: Some(e.to_string()),
        },
    }
}

impl TerminalPane {
    /// Create a pane of `cols`×`rows` cells driving the given renderer. Use
    /// [`crate::render::SoftwareRenderer`] (always available) or
    /// [`crate::render::GpuRenderer`] (when a wgpu device is in hand).
    pub fn new(cols: usize, rows: usize, renderer: Box<dyn PaneRenderer>) -> Self {
        Self {
            grid: TermGrid::new(cols, rows),
            renderer,
            cwd: None,
            verified: HashMap::new(),
            selection: None,
            select_origin: None,
            clipboard: Clipboard::new(),
            toast: None,
            search_shown: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_index: None,
            scroll_activity: None,
            drag_pointer: None,
        }
    }

    /// Feed a chunk of session output (the `data` of a `SessionEvent::Data`) into the grid.
    pub fn feed(&mut self, data: &str) {
        self.grid.feed(data.as_bytes());
    }

    /// Feed raw output bytes (when you have bytes rather than a decoded `String`).
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.grid.feed(bytes);
    }

    /// Drain terminal-originated replies (DSR/DA/etc.) that must be written back to the
    /// session's pty. Empty when there is nothing to forward. **Must** be forwarded or a
    /// real conpty blocks at startup (see [`TermGrid::take_replies`]).
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.grid.take_replies()
    }

    /// Resize the pane's grid. Returns `true` if the cell dimensions changed — in which
    /// case the caller should also `SessionManager::resize` the bound session.
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        self.grid.resize(cols, rows)
    }

    /// Take-and-clear the repaint flag. `true` means the grid changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        self.grid.take_dirty()
    }

    /// Render the current grid to a `slint::Image` at the pane's *physical* pixel
    /// resolution (`cols*cell_w × rows*cell_h`). Cheap to call repeatedly — the renderer
    /// caches its buffers/atlas — but gate it on [`take_dirty`](Self::take_dirty) plus the
    /// cursor blink for minimal CPU.
    pub fn render(&mut self, font: &mut Font, opts: &RenderOpts) -> Image {
        let snap = self.grid.snapshot();
        self.renderer.render(&snap, font, opts)
    }

    /// Current grid size in `(cols, rows)`.
    pub fn grid_size(&self) -> (usize, usize) {
        let s = self.grid.size();
        (s.cols, s.rows)
    }

    /// The visible screen as plain text — one line per viewport row (blank cells as spaces),
    /// with trailing whitespace and trailing blank lines trimmed. Lets a host feed an ambient
    /// summariser the *rendered* screen (what the user actually sees) instead of the raw redraw
    /// byte stream, so a continuously-repainting TUI (e.g. an agent CLI) is captured cleanly
    /// rather than as redraw noise.
    pub fn screen_text(&self) -> String {
        let snap = self.grid.snapshot();
        let mut lines: Vec<String> = (0..snap.rows)
            .map(|r| Self::row_text(&snap, r).trim_end().to_string())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// A human-readable name for the active renderer (e.g. for a HUD/debug overlay).
    pub fn renderer_name(&self) -> &'static str {
        self.renderer.name()
    }

    /// Apply a colour theme: override the 16 base ANSI colours (index 0 = default background,
    /// 7 = default foreground). See [`crate::grid::TermGrid::set_base16`].
    pub fn set_palette(&mut self, base: [[u8; 3]; 16]) {
        self.grid.set_base16(base);
    }

    /// Swap the renderer at runtime (e.g. GPU↔software on a device-lost / RDP transition).
    /// The next [`render`](Self::render) rebuilds from the live grid, so the swap is seamless.
    pub fn set_renderer(&mut self, renderer: Box<dyn PaneRenderer>) {
        self.renderer = renderer;
    }

    // ---- Clickable file paths --------------------------------------------------------------
    //
    // Plain click opens the file (editor / OS default); Ctrl/Cmd-click copies the resolved
    // absolute path. Paths are verified on disk (against this pane's cwd) before they linkify,
    // so prose tokens don't light up. The grid extraction lives in [`crate::links`]; resolve +
    // open live in [`hyperpanes_core::paths`]. This is the renderer-side glue (Spike's
    // `Terminal.tsx` link provider, ported to the cell grid).

    /// Set this pane's working directory (the base for resolving relative path tokens). Clearing
    /// or changing it drops the verify cache, since the same token can resolve elsewhere.
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        if cwd != self.cwd {
            self.cwd = cwd;
            self.verified.clear();
        }
    }

    /// The logical-px cell size for a surface of `surf_w`×`surf_h` (the terminal image is
    /// stretched `image-fit: fill` over the pane body), or `None` for a degenerate pane.
    fn cell_logical(&self, surf_w: f32, surf_h: f32) -> Option<(f32, f32, usize, usize)> {
        let (cols, rows) = self.grid_size();
        if cols == 0 || rows == 0 || surf_w <= 0.0 || surf_h <= 0.0 {
            return None;
        }
        Some((surf_w / cols as f32, surf_h / rows as f32, cols, rows))
    }

    /// Reconstruct one viewport row's text (one char per column; blanks as spaces) so the
    /// `links` extractor's column indices line up with cells. Exact for ASCII paths.
    fn row_text(snap: &crate::grid::GridSnapshot, row: usize) -> String {
        (0..snap.cols)
            .map(|col| {
                let ch = snap.cell(col, row).ch;
                if ch == '\0' {
                    ' '
                } else {
                    ch
                }
            })
            .collect()
    }

    fn cache_key(&self, token: &str) -> String {
        format!("{}\u{1f}{}", self.cwd.as_deref().unwrap_or(""), token)
    }

    /// Find a verified path token under the (logical-px) point, returning the resolved record,
    /// the candidate's column span, and the cell metrics. Resolution is cached per (cwd, token);
    /// only existing paths are cached, so freshly-created files linkify on a later hover.
    fn locate(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<(ResolveResult, usize, usize, usize, f32, f32)> {
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as usize;
        if col >= cols || row >= rows {
            return None;
        }

        let snap = self.grid.snapshot();
        let text = Self::row_text(&snap, row);
        let cand = extract_path_candidates(&text)
            .into_iter()
            .find(|c| col >= c.start && col < c.end)?;

        let key = self.cache_key(&cand.path);
        let resolved = if let Some(hit) = self.verified.get(&key) {
            hit.clone()
        } else {
            let r = paths::resolve_path(self.cwd.as_deref(), &cand.path);
            if !r.exists {
                return None; // negatives aren't cached (a later hover re-checks)
            }
            self.verified.insert(key, r.clone());
            r
        };
        Some((resolved, cand.start, cand.end, row, cell_w, cell_h))
    }

    /// Find an http/https URL under the (logical-px) point, returning the candidate, its row,
    /// and the cell metrics. URLs linkify on shape alone — no disk/network verification (so no
    /// cache either; extraction per hover is cheap, same as the path re-extract in `link_at`).
    fn url_under(
        &self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<(UrlCandidate, usize, f32, f32)> {
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as usize;
        if col >= cols || row >= rows {
            return None;
        }
        let snap = self.grid.snapshot();
        let text = Self::row_text(&snap, row);
        let cand = extract_url_candidates(&text)
            .into_iter()
            .find(|c| col >= c.start && col < c.end)?;
        Some((cand, row, cell_w, cell_h))
    }

    /// Hit-test a (logical-px) hover point against the rendered grid. Returns the underline rect +
    /// target when the point is over an http/https URL or a path that exists on disk, else `None`.
    /// The candidate's `:line[:col]` is carried through (and shown in the tooltip), but only the
    /// resolved path is verified — mirroring the Electron link provider. URLs win when a token is
    /// both (a URL is path-shaped but never disk-verifies anyway).
    pub fn link_at(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        if let Some((cand, row, cell_w, cell_h)) = self.url_under(x, y, surf_w, surf_h) {
            return Some(LinkHit {
                x: cand.start as f32 * cell_w,
                y: (row as f32 + 1.0) * cell_h - 1.0, // a hairline along the cell's baseline
                w: (cand.end - cand.start) as f32 * cell_w,
                tip: cand.url.clone(),
                abs_path: cand.url,
                line: None,
                col: None,
                is_url: true,
            });
        }
        let (r, start, end, row, cell_w, cell_h) = self.locate(x, y, surf_w, surf_h)?;
        // Recover the candidate's line/col by re-extracting (cheap; same row). The resolved
        // record only carries the path, so pull location off the candidate that covered the cell.
        let snap = self.grid.snapshot();
        let text = Self::row_text(&snap, row);
        let cand = extract_path_candidates(&text)
            .into_iter()
            .find(|c| c.start == start && c.end == end);
        let (line, col) = cand
            .as_ref()
            .map(|c| (c.line, c.col))
            .unwrap_or((None, None));

        let tip = match (line, col) {
            (Some(l), Some(c)) => format!("{}:{}:{}", r.abs_path, l, c),
            (Some(l), None) => format!("{}:{}", r.abs_path, l),
            _ => r.abs_path.clone(),
        };
        Some(LinkHit {
            x: start as f32 * cell_w,
            y: (row as f32 + 1.0) * cell_h - 1.0, // a hairline along the cell's baseline
            w: (end - start) as f32 * cell_w,
            abs_path: r.abs_path,
            line,
            col,
            tip,
            is_url: false,
        })
    }

    /// Activate the link under a (logical-px) click. `ctrl` (Ctrl or Cmd held) copies the
    /// absolute path; otherwise the file/dir is opened via [`hyperpanes_core::paths`] using
    /// `editor_command` (empty → VS Code if present, else the guarded OS default). `None` when
    /// the click wasn't over a verified path.
    pub fn activate_link(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
        ctrl: bool,
        editor_command: &str,
    ) -> Option<LinkAction> {
        // Suppress link open/copy at the end of a drag-selection. The widget fires
        // `selection-end` then `link-activated` on the same left-button release, and a dragged
        // selection is kept alive (copied, not cleared) by the shell — so a release that just
        // finished selecting text must NOT also open/copy the link under the release point. A
        // plain click begins a fresh, non-dragged selection, so this never blocks real clicks.
        if self.selection_is_drag() {
            return None;
        }
        let hit = self.link_at(x, y, surf_w, surf_h)?;
        if ctrl {
            // Copy here with the pane's own (arboard) clipboard handle — the proven path the
            // selection copy uses — instead of relying on the caller: the app shell's `clip`
            // shell-out failed silently in live testing. The action is still returned so the
            // caller can do its (now best-effort, redundant) copy and any follow-up UX.
            if self.clipboard.copy(&hit.abs_path) {
                self.set_toast(format!(
                    "Copied {} to clipboard",
                    if hit.is_url { "link" } else { "path" }
                ));
            }
            return Some(LinkAction::Copy(hit.abs_path));
        }
        let res = if hit.is_url {
            open_url(&hit.abs_path)
        } else {
            paths::open_resolved_path(&hit.abs_path, hit.line, hit.col, editor_command)
        };
        Some(LinkAction::Opened(res))
    }

    // ---- Text selection ---------------------------------------------------------------------
    //
    // Drag to select a cell range; the controller turns it into highlight rects (for the Slint
    // overlay) and, on release, the selected text (copy-on-select — see `copy_selection`). The
    // model is our own (`crate::selection`) rather than alacritty's, kept in viewport cells.

    /// The (clamped) viewport cell under a logical-px point. Unlike [`locate`](Self::locate),
    /// this never returns `None` for an in-pane drag that strays past an edge — it clamps to the
    /// nearest cell so a selection can run to the grid border.
    fn cell_at_clamped(&self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<selection::Cell> {
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        let col = (x / cell_w).floor().clamp(0.0, (cols - 1) as f32) as usize;
        let row = (y / cell_h).floor().clamp(0.0, (rows - 1) as f32) as usize;
        // Anchor to the ABSOLUTE grid line so the selection stays glued to its text as the viewport
        // scrolls: a viewport row `r` shows absolute line `r - display_offset`.
        let line = row as i32 - self.grid.display_offset() as i32;
        Some(selection::Cell { col, line })
    }

    /// Begin a drag-selection anchored at the (logical-px) press point. Replaces any prior
    /// selection. The selection only starts *rendering* once the drag leaves the anchor cell, so
    /// a click that doesn't move still falls through to a link activation.
    pub fn selection_begin(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) {
        self.selection = self
            .cell_at_clamped(x, y, surf_w, surf_h)
            .map(Selection::new);
        self.select_origin = Some((x, y));
        self.drag_pointer = Some((x, y, surf_w, surf_h));
    }

    /// Extend the active selection's head to the (logical-px) cursor point during a drag.
    ///
    /// Gated on [`DRAG_THRESHOLD_PX`]: until the pointer has moved that far from the press point
    /// the head stays pinned to the anchor (so the selection never becomes `dragged` and a click
    /// can't copy-on-select). This dead zone is what stops a stray click twitch — especially one
    /// that straddles a cell boundary — from clobbering the clipboard ahead of a paste.
    pub fn selection_update(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) {
        // Track the live pointer for edge-autoscroll even below the drag threshold (autoscroll
        // itself only kicks in once the selection is actually dragged).
        self.drag_pointer = Some((x, y, surf_w, surf_h));
        if let Some((ox, oy)) = self.select_origin {
            if (x - ox).hypot(y - oy) < DRAG_THRESHOLD_PX {
                return;
            }
        }
        let c = match self.cell_at_clamped(x, y, surf_w, surf_h) {
            Some(c) => c,
            None => return,
        };
        if let Some(sel) = self.selection.as_mut() {
            sel.update(c);
        }
    }

    /// Drop the current selection (and its highlight).
    pub fn selection_clear(&mut self) {
        self.selection = None;
        self.select_origin = None;
        self.drag_pointer = None;
    }

    /// End the drag (button released) — stops edge-autoscroll while KEEPING any selection (the
    /// controller may still copy it). Call from the `selection-end` handler.
    pub fn end_selection_drag(&mut self) {
        self.drag_pointer = None;
        self.select_origin = None;
    }

    /// One edge-autoscroll step while a selection drag is held in the top/bottom edge band: scroll
    /// the viewport one line toward that edge and extend the selection head to the just-revealed
    /// edge row, so the selection grows into off-screen scrollback (the vim/iTerm/Claude drag
    /// behavior). Returns `true` if it scrolled — the pump uses that to keep ticking + repainting.
    /// No-op unless a real (dragged) selection is in flight with the pointer at an edge.
    pub fn selection_autoscroll_tick(&mut self) -> bool {
        let (x, y, sw, sh) = match self.drag_pointer {
            Some(d) => d,
            None => return false,
        };
        if !self.selection.is_some_and(|s| s.dragged) {
            return false;
        }
        let (_, cell_h, _, _) = match self.cell_logical(sw, sh) {
            Some(t) => t,
            None => return false,
        };
        let edge = cell_h.max(8.0); // a one-row band (min 8px) at each edge
        let dir = if y < edge {
            1 // top edge → scroll up into history
        } else if y > sh - edge {
            -1 // bottom edge → scroll down toward the live edge
        } else {
            return false;
        };
        let before = self.grid.display_offset();
        self.scroll_by(dir);
        if self.grid.display_offset() == before {
            return false; // clamped at the top/bottom of the buffer
        }
        // Re-map the head to the edge row at the NEW offset → the line just scrolled into view.
        let edge_y = if dir > 0 { 0.0 } else { sh };
        if let Some(c) = self.cell_at_clamped(x, edge_y, sw, sh) {
            if let Some(sel) = self.selection.as_mut() {
                sel.update(c);
            }
        }
        true
    }

    /// Select the entire viewport (every visible cell), marked `dragged` so it renders and is
    /// copyable. This is the context menu's "Select All" — viewport-scoped (the region
    /// [`selection_text`](Self::selection_text) can reconstruct), mirroring xterm's `selectAll`
    /// over the on-screen buffer. A subsequent [`copy_selection`](Self::copy_selection) copies it.
    pub fn select_all(&mut self) {
        let (cols, rows) = self.grid_size();
        if cols == 0 || rows == 0 {
            self.selection = None;
            return;
        }
        // The visible viewport, in absolute lines: top viewport row 0 is line `-offset`.
        let off = self.grid.display_offset() as i32;
        let mut sel = Selection::new(selection::Cell { col: 0, line: -off });
        sel.update(selection::Cell {
            col: cols - 1,
            line: rows as i32 - 1 - off,
        });
        self.selection = Some(sel);
    }

    /// Clear the screen **and** scrollback (the context menu's "Clear"), dropping any selection
    /// and pinning the viewport to the bottom. Feeds the ED escapes (erase display + erase
    /// scrollback) so it runs through the same parser path as live output — the native analog of
    /// xterm's `term.clear()`.
    pub fn clear(&mut self) {
        self.selection = None;
        self.grid.feed(b"\x1b[H\x1b[2J\x1b[3J");
        self.grid.scroll_to_bottom();
    }

    /// True once the active selection has actually been dragged across cells (i.e. it's a real
    /// selection, not a stationary click). The caller uses this to choose copy-vs-click on release.
    pub fn selection_is_drag(&self) -> bool {
        self.selection.is_some_and(|s| s.dragged)
    }

    /// True when there's an active *dragged* selection lying entirely on the cursor's own viewport
    /// row — i.e. over the live shell input line, the only row a terminal can safely treat as
    /// editable text. Scopes type-over-selection to the prompt line: typing over a selection here
    /// drops the highlight (you're replacing your own input), whereas a selection on any other row
    /// (scrollback / command output) isn't in the shell's buffer and is left untouched, so no
    /// speculative deletes are ever sent (no PTY corruption). False for no selection, a non-dragged
    /// click, a multi-row span, or when the cursor is scrolled out of view.
    pub fn selection_on_cursor_row(&self) -> bool {
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return false,
        };
        let (start, end) = sel.ordered();
        if start.line != end.line {
            return false; // a multi-row selection is never a single prompt line
        }
        // Compare in absolute lines: the cursor's viewport row maps to absolute `row - offset`.
        match self.grid.cursor_row() {
            Some(crow) => start.line == crow as i32 - self.grid.display_offset() as i32,
            None => false,
        }
    }

    /// Type-over selection: the byte sequence that ERASES the selected prompt-line text, to be
    /// written to the pty *before* a printable keystroke so the key replaces the selection.
    /// `None` (and no state change) unless the dragged selection lies on the cursor's own
    /// logical line — its visual row, or any rows soft-WRAPPED into it (a long shell input
    /// spanning several visual rows is still one editable line) — and the main screen is
    /// active; on `Some` the selection is also cleared.
    ///
    /// Safety model — the sequence is built ONLY from edit keys the line editor **clamps at the
    /// input-region boundaries** (left/right arrows, backspace, forward-delete are no-ops at the
    /// edges in PSReadLine/readline): if the selection overlaps the prompt decoration itself, the
    /// surplus moves/deletes simply do nothing — only the selected chars inside the editable
    /// input are removed, and nothing left of the input start can ever be touched. The
    /// alternate screen is excluded ([`TermGrid::alt_screen`](crate::grid::TermGrid::alt_screen)):
    /// there these bytes would be app commands (vim motions), not edits.
    ///
    /// Cell↔char caveat: counts are in grid cells, exact for ASCII (same wide-glyph caveat as
    /// [`selection_text`](Self::selection_text)).
    pub fn type_over_selection(&mut self) -> Option<Vec<u8>> {
        if self.grid.alt_screen() {
            return None;
        }
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return None,
        };
        let (start, end) = sel.ordered();
        let crow = self.grid.cursor_row()?;
        let cline = crow as i32 - self.grid.display_offset() as i32;
        let (cols, _) = self.grid_size();
        if cols == 0 {
            return None;
        }
        // The selection must lie on the SAME WRAPPED LOGICAL LINE as the cursor: every grid line
        // between the selection and the cursor (inclusive bounds, exclusive of the last) must carry
        // the WRAPLINE continuation flag. A single-line selection on the cursor's own line is the
        // degenerate case (empty range). This is what makes a long soft-wrapped shell input
        // editable across its visual rows, while a selection on a genuinely different line
        // (scrollback, command output) still declines.
        let lo = start.line.min(cline);
        let hi = end.line.max(cline);
        if (lo..hi).any(|r| !self.grid.line_wraps(r)) {
            return None;
        }
        // Linear cell offsets within the wrapped line: a wrapped row is always `cols` cells wide,
        // and the line editor's arrows/backspace walk straight through the wrap, so the single-row
        // arithmetic below holds verbatim in linear space. Absolute line numbers are fine — only
        // the differences between these offsets matter.
        let lin = |line: i32, col: usize| line * cols as i32 + col as i32;
        let s = lin(start.line, start.col);
        let e = lin(end.line, end.col);
        let c = lin(cline, self.grid.cursor_col());
        const LEFT: &[u8] = b"\x1b[D";
        const RIGHT: &[u8] = b"\x1b[C";
        const BS: &[u8] = &[0x7f];
        const FDEL: &[u8] = b"\x1b[3~"; // forward delete (DeleteChar)
        let mut bytes = Vec::new();
        let mut rep = |seq: &[u8], n: i32| {
            for _ in 0..n.max(0) {
                bytes.extend_from_slice(seq);
            }
        };
        if c > e {
            // Caret right of the selection (the common case — you selected text you just
            // typed): step left to the selection end, then backspace it away.
            rep(LEFT, c - (e + 1));
            rep(BS, e - s + 1);
        } else if c <= s {
            // Caret left of (or at) the selection: step right to its start, forward-delete it.
            rep(RIGHT, s - c);
            rep(FDEL, e - s + 1);
        } else {
            // Caret inside the selection: backspace the left part, forward-delete the rest.
            rep(BS, c - s);
            rep(FDEL, e - c + 1);
        }
        self.selection_clear();
        Some(bytes)
    }

    /// Highlight rectangles (logical px) for the active *dragged* selection over a surface of
    /// `surf_w`×`surf_h`. Empty for no selection or a non-dragged click — so a plain click never
    /// leaves a stray one-cell highlight.
    pub fn selection_rects(&self, surf_w: f32, surf_h: f32) -> Vec<(f32, f32, f32, f32)> {
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return Vec::new(),
        };
        let (cell_w, cell_h, cols, rows) = match self.cell_logical(surf_w, surf_h) {
            Some(t) => t,
            None => return Vec::new(),
        };
        // Project the absolute-line selection back through the current scroll position so the
        // highlight rides the content (and clips at the viewport edges) as the user scrolls.
        let off = self.grid.display_offset() as i32;
        selection::selection_rects(sel, cols, cell_w, cell_h, off, rows)
    }

    /// The text covered by the active *dragged* selection, reconstructed from the grid snapshot
    /// (one char per cell, blanks as spaces, each line right-trimmed, rows joined by `\n`).
    /// `None` when there's no real selection. Exact for ASCII (the same wide-glyph caveat as the
    /// link extractor).
    pub fn selection_text(&self) -> Option<String> {
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return None,
        };
        let (start, end) = sel.ordered();
        let (cols, _) = self.grid_size();
        if cols == 0 {
            return None;
        }
        let last_col = cols - 1;
        // Read straight from the grid by ABSOLUTE line, so a selection anchored in scrollback (or
        // straddling the history/viewport boundary) reconstructs correctly regardless of scroll.
        let mut lines = Vec::new();
        for line_i in start.line..=end.line {
            let row_text = match self.grid.line_text(line_i) {
                Some(t) => t,
                None => continue, // line outside the buffer (shouldn't happen for a live selection)
            };
            let chars: Vec<char> = row_text.chars().collect();
            let col_start = if line_i == start.line { start.col } else { 0 };
            let col_end = if line_i == end.line {
                end.col
            } else {
                last_col
            };
            let mut line = String::new();
            for col in col_start..=col_end.min(last_col) {
                let ch = chars.get(col).copied().unwrap_or(' ');
                line.push(if ch == '\0' { ' ' } else { ch });
            }
            lines.push(line.trim_end().to_string());
        }
        let text = lines.join("\n");
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Copy the current selection to the system clipboard and raise a "Copied …" indicator
    /// (the copy-on-select behavior, also bound to Ctrl+C / Ctrl+Shift+C). Returns the number of
    /// characters copied, or `None` if there was no selection or the clipboard was unavailable.
    pub fn copy_selection(&mut self) -> Option<usize> {
        let text = self.selection_text()?;
        let n = text.chars().count();
        if self.clipboard.copy(&text) {
            self.set_toast(format!(
                "Copied {} char{} to clipboard",
                n,
                if n == 1 { "" } else { "s" }
            ));
            Some(n)
        } else {
            None
        }
    }

    /// Copy arbitrary `text` (a Ctrl+clicked link/path) to the system clipboard and raise the
    /// "Copied …" indicator — the same arboard instance + toast as
    /// [`copy_selection`](Self::copy_selection). Replaces the app's `clip.exe` shell-out, which
    /// blocked the UI thread on `child.wait()` for every Ctrl+click (a visible freeze) and
    /// showed no indicator.
    pub fn copy_text(&mut self, text: &str) -> bool {
        if self.clipboard.copy(text) {
            self.set_toast("Copied to clipboard");
            true
        } else {
            false
        }
    }

    /// Read the system clipboard for a right-click / Ctrl+V paste, raising a "Pasted …"
    /// indicator. Returns the text the caller should write to this pane's session (the controller
    /// doesn't own the session transport), or `None` when the clipboard is empty/unavailable.
    pub fn paste_from_clipboard(&mut self) -> Option<String> {
        let text = self.clipboard.paste()?;
        let n = text.chars().count();
        self.set_toast(format!(
            "Pasted {} char{}",
            n,
            if n == 1 { "" } else { "s" }
        ));
        Some(prepare_paste(&text, self.grid.bracketed_paste()))
    }

    /// Whether the OS clipboard holds an image (vs text). The controller uses this to decide
    /// whether a Ctrl+V with no clipboard text should forward a literal 0x16 to an in-pane TUI
    /// (Claude Code) that reads the clipboard image itself — see [`Clipboard::has_image`].
    pub fn clipboard_has_image(&mut self) -> bool {
        self.clipboard.has_image()
    }

    /// Pin the viewport back to the live edge (display offset 0) so the cursor is visible at the
    /// end of whatever was just written — e.g. after a paste, regardless of scrollback position.
    pub fn scroll_to_bottom(&mut self) {
        self.grid.scroll_to_bottom();
    }

    /// Scroll the scrollback viewport by `delta_lines` (positive = up into history, negative =
    /// toward the live edge), clamped to the history bounds. Stamps the scrollbar's show timer.
    pub fn scroll_by(&mut self, delta_lines: i32) {
        self.grid.scroll_by(delta_lines);
        self.scroll_activity = Some(Instant::now());
    }

    /// Scroll the scrollback viewport by one page (`up` = into history, else toward the live
    /// edge). A page is the visible row count less one row of overlap, so successive pages keep a
    /// line of context. Drives Shift+PageUp / Shift+PageDown.
    pub fn scroll_page(&mut self, up: bool) {
        let (_, rows) = self.grid_size();
        let page = (rows as i32 - 1).max(1);
        self.scroll_by(if up { page } else { -page });
    }

    /// Jump the viewport to the very top of scrollback (Shift+Home). Stamps the scrollbar timer.
    pub fn scroll_to_top(&mut self) {
        let (hist, _, off) = self.grid.scroll_metrics();
        if hist > off {
            self.scroll_by((hist - off) as i32);
        }
    }

    /// Handle a mouse-wheel notch over the pane. `delta_lines` is positive for wheel-up (into
    /// history) / negative for wheel-down, in scrollback lines (the widget sends ±3). Returns the
    /// bytes the controller should write to the pty when the wheel belongs to the **application**
    /// instead of our scrollback:
    ///
    /// * a **mouse-grabbing app** (DECSET 1000/1002/1003 — vim, htop, Claude Code) → a mouse-wheel
    ///   report at the pointer cell, so the app scrolls its own view;
    /// * the **alternate screen** with no mouse mode (less, man, a pager) → up/down arrow keys
    ///   (xterm's "alternate scroll"), so the pager scrolls a line at a time.
    ///
    /// Otherwise it scrolls our own scrollback viewport and returns `None`. This is the fix for
    /// "can't scroll Claude": in the alt screen there is no scrollback for `scroll_by` to move, so
    /// the wheel must be forwarded to the app.
    pub fn wheel(
        &mut self,
        delta_lines: i32,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<Vec<u8>> {
        if delta_lines == 0 {
            return None;
        }
        if self.grid.mouse_mode() {
            return Some(self.mouse_wheel_report(delta_lines, x, y, surf_w, surf_h));
        }
        if self.grid.alt_screen() {
            return Some(alt_scroll_arrows(delta_lines, self.grid.app_cursor()));
        }
        self.scroll_by(delta_lines);
        None
    }

    /// Build mouse-wheel report bytes for a mouse-grabbing app: one report per wheel notch (a notch
    /// is [`WHEEL_LINES_PER_NOTCH`] of `delta_lines`), encoded SGR (`ESC[<Cb;Cx;Cy M`) when the app
    /// asked for it (DECSET 1006), else legacy X10 (`ESC[M` + 3 bytes). Button 64 = wheel up, 65 =
    /// down; the position is the 1-based cell under the pointer.
    /// 1-based `(col, row)` of the pointer at logical px `(x, y)` over a `surf_w`×`surf_h` surface,
    /// clamped into the grid. Shared by every mouse report.
    fn cell_1based(&self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> (usize, usize) {
        match self.cell_logical(surf_w, surf_h) {
            Some((cw, ch, cols, rows)) => {
                let c = (x / cw).floor().clamp(0.0, (cols - 1) as f32) as usize + 1;
                let r = (y / ch).floor().clamp(0.0, (rows - 1) as f32) as usize + 1;
                (c, r)
            }
            None => (1, 1),
        }
    }

    /// Encode ONE mouse report: `cb` is the button/event code (motion already includes the +32
    /// motion bit; wheel is 64/65), `release` picks SGR final `m`/X10 button-3. SGR (`ESC[<cb;c;r
    /// M|m`) when the app asked for it (DECSET 1006), else legacy X10 (`ESC[M` + 3 bytes, each +32).
    fn fmt_mouse(&self, cb: u32, col: usize, row: usize, release: bool) -> Vec<u8> {
        if self.grid.sgr_mouse() {
            let term = if release { 'm' } else { 'M' };
            format!("\x1b[<{cb};{col};{row}{term}").into_bytes()
        } else {
            let b = if release { 3 } else { cb }; // X10 can't say which button released
            vec![
                0x1b,
                b'[',
                b'M',
                (b + 32).min(255) as u8,
                (col as u32 + 32).min(255) as u8,
                (row as u32 + 32).min(255) as u8,
            ]
        }
    }

    fn mouse_wheel_report(
        &self,
        delta_lines: i32,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Vec<u8> {
        let (col, row) = self.cell_1based(x, y, surf_w, surf_h);
        let cb: u32 = if delta_lines > 0 { 64 } else { 65 };
        let notches = (delta_lines.unsigned_abs() / WHEEL_LINES_PER_NOTCH as u32).max(1);
        let mut out = Vec::new();
        for _ in 0..notches {
            out.extend_from_slice(&self.fmt_mouse(cb, col, row, false));
        }
        out
    }

    /// Whether the application has grabbed the mouse (DECSET 1000/1002/1003). When true the
    /// controller forwards button/drag/release events to the app (its own selection/clicks) instead
    /// of doing a local terminal selection — unless Shift is held (see [`mouse_report`]).
    ///
    /// [`mouse_report`]: Self::mouse_report
    pub fn app_grabs_mouse(&self) -> bool {
        self.grid.mouse_mode()
    }

    /// Bytes to forward to the pty for a pointer event over a mouse-grabbing app, or `None` to
    /// suppress it. `kind`: 0 = button press, 1 = motion, 2 = release. `button`: 0 = left, 1 =
    /// middle, 2 = right, 3 = none (a bare move). Motion is only reported when the app asked for it
    /// (1002 = while a button is held, 1003 = always); press/release always report. Returns `None`
    /// if the app isn't grabbing the mouse.
    pub fn mouse_report(
        &self,
        kind: i32,
        button: i32,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<Vec<u8>> {
        if !self.grid.mouse_mode() {
            return None;
        }
        let (col, row) = self.cell_1based(x, y, surf_w, surf_h);
        let btn = button.clamp(0, 3) as u32;
        match kind {
            0 => Some(self.fmt_mouse(btn, col, row, false)), // press
            2 => Some(self.fmt_mouse(btn.min(2), col, row, true)), // release
            1 => {
                // Motion: forward on any-motion (1003), or on drag (1002) only while a button is held.
                let held = button != 3;
                let want = self.grid.mouse_any_motion() || (self.grid.mouse_drag() && held);
                if !want {
                    return None;
                }
                let mb = if held { btn } else { 3 };
                Some(self.fmt_mouse(32 + mb, col, row, false))
            }
            _ => None,
        }
    }

    // ---- scroll overlays (vim scrollbar + jump-to-bottom HUD) --------------------------------

    /// How far the viewport is scrolled up from the live edge, in lines (0 = pinned to the bottom).
    /// Drives the jump-to-bottom HUD: shown whenever this is non-zero.
    pub fn scroll_offset(&self) -> usize {
        self.grid.scroll_metrics().2
    }

    /// The vim-style scrollbar to draw right now, or `None` when there is no scrollback or the bar
    /// has fully faded. Returns `(thumb_y, thumb_h, opacity)` in logical px over a `surf_h`-tall
    /// pane: the thumb height is proportional to the visible fraction of the buffer, its position to
    /// how far down the buffer the viewport sits, and the opacity ramps from 1 down to 0 over the
    /// show-then-fade window since the last scroll gesture (so the bar is invisible while idle).
    pub fn scrollbar(&self, surf_h: f32) -> Option<(f32, f32, f32)> {
        let opacity = self.scrollbar_opacity()?;
        let (hist, rows, off) = self.grid.scroll_metrics();
        if hist == 0 || rows == 0 || surf_h <= 0.0 {
            return None;
        }
        let total = (hist + rows) as f32;
        let thumb_h = (surf_h * rows as f32 / total)
            .max(SCROLLBAR_MIN_THUMB_PX)
            .min(surf_h);
        // Fraction of the way down the buffer the viewport TOP sits: 0 at the very top of history,
        // 1 at the live edge. `hist - off` lines sit above the viewport top, out of `hist` total.
        let frac = if hist == 0 {
            1.0
        } else {
            (hist - off) as f32 / hist as f32
        };
        let thumb_y = (surf_h - thumb_h) * frac;
        Some((thumb_y, thumb_h, opacity))
    }

    /// Current scrollbar opacity from the show-then-fade timer, or `None` once it has fully faded
    /// (so the projection can drop the bar entirely while idle).
    fn scrollbar_opacity(&self) -> Option<f32> {
        let e = self.scroll_activity?.elapsed().as_millis();
        if e < SCROLLBAR_SHOW_MS {
            Some(1.0)
        } else if e < SCROLLBAR_SHOW_MS + SCROLLBAR_FADE_MS {
            Some(1.0 - (e - SCROLLBAR_SHOW_MS) as f32 / SCROLLBAR_FADE_MS as f32)
        } else {
            None
        }
    }

    // ---- Copy/paste indicator ("toast") -----------------------------------------------------

    /// Raise a transient indicator over the pane (e.g. "Copied 12 chars to clipboard"). It
    /// auto-expires after [`TOAST_MS`]; poll it each frame with [`toast_text`](Self::toast_text).
    pub fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// The indicator text to display right now, or `None` once it has expired (which also clears
    /// it). Call this every frame and push the result to the pane's `toast` property.
    pub fn toast_text(&mut self) -> Option<String> {
        let expired = match &self.toast {
            Some((_, at)) => at.elapsed().as_millis() >= TOAST_MS,
            None => return None,
        };
        if expired {
            self.toast = None;
            return None;
        }
        self.toast.as_ref().map(|(m, _)| m.clone())
    }

    // ---- In-pane search (Ctrl+F) ------------------------------------------------------------
    //
    // Open a search box, type to find/highlight matches across the grid + scrollback, and step
    // through them (Enter / Shift+Enter), revealing each by scrolling it into view. Mirrors the
    // xterm `@xterm/addon-search` wiring in the Electron `Terminal.tsx` / `SearchBox.tsx`.

    /// Open the search box (Ctrl+F). The query starts empty; type to search.
    pub fn search_open(&mut self) {
        self.search_shown = true;
    }

    /// Close the search box, dropping the query/matches and pinning the viewport back to the
    /// bottom (the live prompt).
    pub fn search_close(&mut self) {
        self.search_shown = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_index = None;
        self.grid.scroll_to_bottom();
    }

    /// Whether the search box is open.
    pub fn search_is_open(&self) -> bool {
        self.search_shown
    }

    /// The current query text.
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Set the query (find-as-you-type): recompute matches across the grid + scrollback, pick the
    /// match nearest the current viewport, and scroll it into view.
    pub fn search_set_query(&mut self, query: &str) {
        self.search_query = query.to_string();
        self.search_recompute();
        self.search_reveal_active();
    }

    /// Step to the next (`forward`) / previous match, wrapping around, and reveal it.
    pub fn search_step(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            return;
        }
        let i = self.search_index.unwrap_or(0);
        self.search_index = Some(search::step(i, self.search_matches.len(), forward));
        self.search_reveal_active();
    }

    /// `(current_1_based, total)` for the match counter — `(0, 0)` when there are no matches.
    pub fn search_count(&self) -> (usize, usize) {
        let total = self.search_matches.len();
        let cur = self.search_index.map(|i| i + 1).unwrap_or(0);
        (cur, total)
    }

    /// Highlight rectangles (logical px) for every match currently in the viewport, plus the
    /// active match's rect on its own (so the pane can draw it distinctly). Matches scrolled out
    /// of view are omitted.
    // pre-existing; deferred per repo lint policy (test.yml)
    #[allow(clippy::type_complexity)]
    pub fn search_view_rects(
        &self,
        surf_w: f32,
        surf_h: f32,
    ) -> (Vec<(f32, f32, f32, f32)>, Option<(f32, f32, f32, f32)>) {
        let (cell_w, cell_h, _cols, rows) = match self.cell_logical(surf_w, surf_h) {
            Some(t) => t,
            None => return (Vec::new(), None),
        };
        let off = self.grid.display_offset() as i32;
        let mut rects = Vec::new();
        let mut active = None;
        for (i, m) in self.search_matches.iter().enumerate() {
            let row = m.line + off;
            if row < 0 || row >= rows as i32 {
                continue;
            }
            let rect = (
                m.start as f32 * cell_w,
                row as f32 * cell_h,
                (m.end.saturating_sub(m.start)) as f32 * cell_w,
                cell_h,
            );
            if Some(i) == self.search_index {
                active = Some(rect);
            } else {
                rects.push(rect);
            }
        }
        (rects, active)
    }

    /// Recompute `search_matches` for the current query, choosing an initial active match nearest
    /// the viewport top. Clears everything for an empty query.
    fn search_recompute(&mut self) {
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.search_index = None;
            return;
        }
        let lines = self.grid.history_lines();
        self.search_matches = search::find_matches(&lines, &self.search_query);
        self.search_index = if self.search_matches.is_empty() {
            None
        } else {
            let prefer_line = -(self.grid.display_offset() as i32);
            search::initial_index(&self.search_matches, prefer_line)
        };
    }

    /// Scroll the active match into view (no-op if already visible or there's none).
    fn search_reveal_active(&mut self) {
        if let Some(m) = self
            .search_index
            .and_then(|i| self.search_matches.get(i).copied())
        {
            self.grid.scroll_to_visible(m.line);
        }
    }

    /// Recompute matches against the (possibly reflowed) grid — call after a [`resize`](Self::resize)
    /// so the highlight rects keep tracking the rewrapped text while the search box stays open. A
    /// no-op when search is closed; doesn't force-scroll (the viewport stays where the user left it).
    pub fn search_reflow(&mut self) {
        if self.search_shown {
            self.search_recompute();
        }
    }
}

/// Compute the cell grid that fits a pane of `width_px`×`height_px` *physical* pixels for
/// a font with `cell_w`×`cell_h` cells. Clamped to a sane minimum so a collapsed pane
/// never produces a 0-sized grid. A small free helper the app-shell can reuse for the
/// geometry→resize step.
pub fn cells_for_px(width_px: f32, height_px: f32, cell_w: u32, cell_h: u32) -> (usize, usize) {
    let cols = ((width_px as u32) / cell_w.max(1)).max(2) as usize;
    let rows = ((height_px as u32) / cell_h.max(1)).max(1) as usize;
    (cols, rows)
}

/// Alternate-scroll arrow keys for a wheel notch in the alternate screen (no mouse mode): one
/// Up/Down per scrollback line in `delta_lines` (positive = up). Encoded as application cursor keys
/// (`ESC O A/B`) when DECCKM is set, else normal (`ESC [ A/B`) — what xterm sends so pagers (less,
/// man) scroll on the wheel. This is the no-mouse-grab leg of the "can't scroll Claude" fix.
fn alt_scroll_arrows(delta_lines: i32, app_cursor: bool) -> Vec<u8> {
    let seq: &[u8] = match (delta_lines > 0, app_cursor) {
        (true, false) => b"\x1b[A",
        (false, false) => b"\x1b[B",
        (true, true) => b"\x1bOA",
        (false, true) => b"\x1bOB",
    };
    seq.repeat(delta_lines.unsigned_abs().max(1) as usize)
}

/// Turn raw clipboard text into the exact bytes to write to the pty for a paste.
///
/// Two transforms, both matching how Windows Terminal feeds a paste to conpty:
/// 1. **Normalize line endings to CR (`\r`).** Windows console input treats CR as Enter; a bare
///    LF (`\n`) is mishandled by conpty/PSReadLine, which strands the caret and fragments a
///    multi-line paste across `>>` continuation prompts. Our selection text joins rows with `\n`,
///    and external clipboards carry `\r\n`/`\n` — all collapse to `\r` here.
/// 2. **Bracket** the payload in `ESC[200~ … ESC[201~` *only* when the app enabled bracketed-paste
///    mode (DECSET 2004 — modern PSReadLine / PowerShell 7). Then the shell inserts it as one
///    literal paste (caret at the end, no premature execution). Old shells (Windows PowerShell 5.1)
///    don't set the mode, so the CR-normalized text is sent bare — still the correct Enter handling.
fn prepare_paste(text: &str, bracketed: bool) -> String {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\u{1b}[200~{normalized}\u{1b}[201~")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::SoftwareRenderer;

    #[test]
    fn feed_then_dirty_then_render_roundtrips() {
        let mut p = TerminalPane::new(20, 4, Box::new(SoftwareRenderer::new()));
        assert!(p.take_dirty()); // starts dirty
        assert!(!p.take_dirty());
        p.feed("hi");
        assert!(p.take_dirty());
        assert_eq!(p.grid_size(), (20, 4));
    }

    #[test]
    fn resize_signals_session_resize_need() {
        let mut p = TerminalPane::new(20, 4, Box::new(SoftwareRenderer::new()));
        assert!(!p.resize(20, 4));
        assert!(p.resize(30, 8));
        assert_eq!(p.grid_size(), (30, 8));
    }

    #[test]
    fn cells_for_px_clamps_and_divides() {
        assert_eq!(cells_for_px(800.0, 400.0, 8, 16), (100, 25));
        // Collapsed pane never yields a zero grid.
        assert_eq!(cells_for_px(0.0, 0.0, 8, 16), (2, 1));
    }

    // A pane whose surface is `cols`×`rows` logical px → exactly 1px per cell, so a hover at
    // `(col + 0.5, row + 0.5)` lands squarely on cell `(col, row)`.
    fn unit_pane(cols: usize, rows: usize) -> TerminalPane {
        TerminalPane::new(cols, rows, Box::new(SoftwareRenderer::new()))
    }

    #[test]
    fn link_at_hits_a_verified_path_and_misses_prose() {
        let dir = std::env::temp_dir().join(format!("hp_pane_link_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), b"hi").unwrap();

        let mut p = unit_pane(40, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("see note.txt"); // "see " = cols 0..4, "note.txt" = cols 4..12
        let (w, h) = (40.0, 3.0); // 1px per cell

        // Over the path token → a hit pointing at the absolute file.
        let hit = p
            .link_at(6.5, 0.5, w, h)
            .expect("hover over note.txt should hit");
        assert!(hit.abs_path.replace('\\', "/").ends_with("note.txt"));
        // Underline spans exactly the token's columns (4..12) at 1px/col.
        assert_eq!(hit.x, 4.0);
        assert_eq!(hit.w, 8.0);

        // Over the bare word "see" (not path-shaped) → nothing.
        assert!(p.link_at(1.5, 0.5, w, h).is_none());
        // Over a blank cell past the text → nothing.
        assert!(p.link_at(30.5, 0.5, w, h).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_at_ignores_a_nonexistent_path() {
        let dir = std::env::temp_dir().join(format!("hp_pane_nolink_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = unit_pane(40, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("ghost.txt is gone"); // shape-passes but doesn't exist → no link
        assert!(p.link_at(2.5, 0.5, 40.0, 2.0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_click_copies_the_absolute_path() {
        let dir = std::env::temp_dir().join(format!("hp_pane_copy_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("a.txt"); // cols 0..5
        match p.activate_link(2.5, 0.5, 20.0, 2.0, true, "") {
            Some(LinkAction::Copy(path)) => {
                assert!(path.replace('\\', "/").ends_with("a.txt"));
            }
            other => panic!("ctrl+click should copy, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drag_selection_suppresses_link_activation() {
        // After a drag-select, the same left-release must NOT also open/copy the link under it
        // (the widget fires selection-end then link-activated on one release).
        let dir = std::env::temp_dir().join(format!("hp_pane_seldrag_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("a.txt"); // cols 0..5, 1px/cell on a 20x2 surface
                         // A real drag past the threshold marks the selection dragged.
        p.selection_begin(0.5, 0.5, 20.0, 2.0);
        p.selection_update(5.5, 0.5, 20.0, 2.0); // 5px move > DRAG_THRESHOLD_PX
        assert!(p.selection_is_drag());
        // Activation over the path is suppressed while the drag selection stands — both a plain
        // click (would open) and Ctrl+click (would re-copy and clobber the just-copied selection).
        assert!(p.activate_link(2.5, 0.5, 20.0, 2.0, false, "").is_none());
        assert!(p.activate_link(2.5, 0.5, 20.0, 2.0, true, "").is_none());
        // Once the selection is cleared, a click activates the link normally again.
        p.selection_clear();
        assert!(matches!(
            p.activate_link(2.5, 0.5, 20.0, 2.0, true, ""),
            Some(LinkAction::Copy(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn link_at_hits_a_url_without_disk_verification() {
        let mut p = unit_pane(40, 3);
        // No cwd set, nothing on disk — a URL must still linkify on shape alone.
        p.feed("go https://a.com/x?q=1 now"); // "go " = cols 0..3, URL = cols 3..22
        let (w, h) = (40.0, 3.0); // 1px per cell

        let hit = p
            .link_at(10.5, 0.5, w, h)
            .expect("hover over the URL should hit");
        assert!(hit.is_url);
        assert_eq!(hit.abs_path, "https://a.com/x?q=1");
        assert_eq!(hit.tip, "https://a.com/x?q=1");
        assert_eq!((hit.line, hit.col), (None, None));
        // Underline spans exactly the URL's columns (3..22) at 1px/col.
        assert_eq!(hit.x, 3.0);
        assert_eq!(hit.w, 19.0);

        // Over the bare word "go" → nothing; past the URL → nothing.
        assert!(p.link_at(1.5, 0.5, w, h).is_none());
        assert!(p.link_at(30.5, 0.5, w, h).is_none());
    }

    #[test]
    fn ctrl_click_copies_the_url() {
        let mut p = unit_pane(30, 2);
        p.feed("https://a.com/x"); // cols 0..15
        match p.activate_link(5.5, 0.5, 30.0, 2.0, true, "") {
            Some(LinkAction::Copy(url)) => assert_eq!(url, "https://a.com/x"),
            other => panic!("ctrl+click on a URL should copy it, got {other:?}"),
        }
    }

    #[test]
    fn a_drag_selection_suppresses_url_activation_too() {
        // Same one-release rule as paths: a drag-select release must not also open/copy the URL.
        let mut p = unit_pane(30, 2);
        p.feed("https://a.com/x");
        p.selection_begin(0.5, 0.5, 30.0, 2.0);
        p.selection_update(8.5, 0.5, 30.0, 2.0); // 8px move > DRAG_THRESHOLD_PX
        assert!(p.selection_is_drag());
        assert!(p.activate_link(5.5, 0.5, 30.0, 2.0, false, "").is_none());
        assert!(p.activate_link(5.5, 0.5, 30.0, 2.0, true, "").is_none());
        p.selection_clear();
        assert!(matches!(
            p.activate_link(5.5, 0.5, 30.0, 2.0, true, ""),
            Some(LinkAction::Copy(_))
        ));
    }

    #[test]
    fn prepare_paste_normalizes_newlines_to_cr() {
        // LF and CRLF both collapse to CR (the Enter the Windows console expects); a paste with
        // no bracketed-paste mode is sent bare.
        assert_eq!(prepare_paste("a\nb\r\nc", false), "a\rb\rc");
        // Trailing blank lines (e.g. a multi-row selection over empty rows) become trailing CRs.
        assert_eq!(prepare_paste("x\n\n", false), "x\r\r");
        // No newlines → unchanged.
        assert_eq!(prepare_paste("echo hello", false), "echo hello");
    }

    #[test]
    fn prepare_paste_wraps_in_brackets_only_when_mode_on() {
        assert_eq!(prepare_paste("a\nb", true), "\u{1b}[200~a\rb\u{1b}[201~");
        assert!(!prepare_paste("a\nb", false).contains("200~"));
    }

    #[test]
    fn a_click_twitch_within_threshold_is_not_a_drag() {
        // A press with a sub-threshold wobble (even one that crosses a cell boundary) must NOT
        // promote to a drag — otherwise it copies-on-select and clobbers the clipboard before a
        // paste. A 10×10 pane driven by a 100px surface → each cell is 10px wide: a 2px move from
        // x=19 (cell 1) to x=21 (cell 2) straddles a boundary but stays inside the 4px dead zone.
        let mut p = unit_pane(10, 10);
        p.selection_begin(19.0, 5.0, 100.0, 100.0);
        p.selection_update(21.0, 5.0, 100.0, 100.0); // 2px move, crosses cell 1→2
        assert!(
            !p.selection_is_drag(),
            "a sub-threshold twitch must not be a drag"
        );
        assert!(
            p.selection_text().is_none(),
            "a click must not yield copyable text"
        );
        assert!(
            p.selection_rects(100.0, 100.0).is_empty(),
            "a click leaves no highlight"
        );
    }

    #[test]
    fn a_real_drag_past_threshold_selects_and_copies() {
        let mut p = unit_pane(10, 10);
        p.feed("abcdefghij"); // row 0 cells 0..10
        p.selection_begin(5.0, 5.0, 100.0, 100.0); // cell 0
        p.selection_update(55.0, 5.0, 100.0, 100.0); // 50px move → well past threshold, cell 5
        assert!(
            p.selection_is_drag(),
            "a real drag past the threshold selects"
        );
        assert_eq!(p.selection_text().as_deref(), Some("abcdef"));
    }

    #[test]
    fn wheel_scrolls_scrollback_on_the_main_screen() {
        // Grow scrollback past the viewport, then wheel up: the main screen has no app mouse grab,
        // so the wheel moves OUR viewport (no bytes forwarded) and the display offset advances.
        let mut p = unit_pane(20, 3);
        for _ in 0..50 {
            p.feed("line\r\n");
        }
        assert_eq!(p.scroll_offset(), 0, "pinned to the live edge initially");
        assert!(
            p.wheel(3, 5.0, 5.0, 20.0, 3.0).is_none(),
            "no pty forward on the main screen"
        );
        assert!(
            p.scroll_offset() > 0,
            "wheel moved the scrollback viewport up"
        );
    }

    #[test]
    fn wheel_in_alt_screen_without_mouse_forwards_arrow_keys() {
        // Alt screen, no mouse mode → alternate scroll: wheel becomes Up/Down arrows so a pager
        // scrolls. This is the "can't scroll Claude" fix's no-mouse-grab leg.
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1049h"); // enter alternate screen
        let up = p
            .wheel(3, 5.0, 5.0, 20.0, 3.0)
            .expect("alt screen forwards to the pty");
        assert_eq!(
            up, b"\x1b[A\x1b[A\x1b[A",
            "3 lines up → three normal Up arrows"
        );
        let down = p.wheel(-3, 5.0, 5.0, 20.0, 3.0).unwrap();
        assert_eq!(down, b"\x1b[B\x1b[B\x1b[B");
        // The viewport never moved (alt screen has no scrollback to scroll).
        assert_eq!(p.scroll_offset(), 0);
    }

    #[test]
    fn wheel_with_app_cursor_keys_uses_ss3_arrows() {
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1049h\x1b[?1h"); // alt screen + DECCKM (application cursor keys)
        let up = p.wheel(3, 5.0, 5.0, 20.0, 3.0).unwrap();
        assert_eq!(up, b"\x1bOA\x1bOA\x1bOA", "DECCKM → SS3 Up arrows");
    }

    #[test]
    fn wheel_with_mouse_grab_forwards_sgr_wheel_report() {
        // A mouse-grabbing app (DECSET 1000) with SGR encoding (1006) gets a wheel report at the
        // pointer cell — one per notch — so it (Claude Code / vim / htop) scrolls its own view.
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1000h\x1b[?1006h");
        // Pointer over cell (col 4, row 1) → 1-based (5, 2). One notch (3 lines) → one report.
        let up = p.wheel(3, 4.5, 1.5, 20.0, 3.0).unwrap();
        assert_eq!(up, b"\x1b[<64;5;2M", "wheel-up SGR report, button 64");
        let down = p.wheel(-3, 4.5, 1.5, 20.0, 3.0).unwrap();
        assert_eq!(down, b"\x1b[<65;5;2M", "wheel-down SGR report, button 65");
    }

    #[test]
    fn mouse_report_forwards_press_drag_release_to_a_grabbing_app() {
        // DECSET 1002 (button-event/drag tracking) + 1006 (SGR): forward the mouse so the app
        // (Claude) does its own selection. Pointer at cell (4,1) → 1-based (5,2).
        let mut p = unit_pane(20, 3);
        p.feed("\x1b[?1002h\x1b[?1006h");
        assert!(p.app_grabs_mouse());
        // Left press → button 0.
        assert_eq!(
            p.mouse_report(0, 0, 4.5, 1.5, 20.0, 3.0).unwrap(),
            b"\x1b[<0;5;2M"
        );
        // Drag (motion, left held) → +32 motion bit.
        assert_eq!(
            p.mouse_report(1, 0, 4.5, 1.5, 20.0, 3.0).unwrap(),
            b"\x1b[<32;5;2M"
        );
        // Release → SGR final 'm'.
        assert_eq!(
            p.mouse_report(2, 0, 4.5, 1.5, 20.0, 3.0).unwrap(),
            b"\x1b[<0;5;2m"
        );
        // A bare move (no button) under drag-only tracking (1002) is NOT reported.
        assert!(p.mouse_report(1, 3, 4.5, 1.5, 20.0, 3.0).is_none());
    }

    #[test]
    fn mouse_report_is_none_when_app_has_no_mouse_mode() {
        let mut p = unit_pane(20, 3);
        p.feed("hi"); // plain shell, no mouse mode
        assert!(!p.app_grabs_mouse());
        assert!(p.mouse_report(0, 0, 4.5, 1.5, 20.0, 3.0).is_none());
    }

    #[test]
    fn selection_text_follows_content_after_scrolling() {
        // Select a line, then scroll up: the selection is anchored to the absolute line, so its
        // text is unchanged even though the viewport now shows different rows. (Task: keep the
        // selection while scrolling.)
        let mut p = unit_pane(10, 3);
        p.feed("AAAAA\r\n");
        for _ in 0..20 {
            p.feed("xxxxx\r\n");
        }
        // Scroll up so the "AAAAA" line is back on screen, then select its first 5 cells.
        p.scroll_by(100); // clamps to the top of history
                          // Find AAAAA isn't necessary; just select viewport row 0 cols 0..5 and remember its text.
        p.selection_begin(0.5, 0.5, 10.0, 3.0);
        p.selection_update(4.5, 0.5, 10.0, 3.0);
        let before = p.selection_text();
        assert!(before.is_some());
        // Scroll down one line: the same content moves, but selection_text must be identical.
        p.scroll_by(-1);
        assert_eq!(
            p.selection_text(),
            before,
            "selection text is glued to its line"
        );
    }

    #[test]
    fn drag_at_top_edge_autoscrolls_into_history_and_grows_selection() {
        // 60 lines of history in a 20-row pane (so the 8px edge band leaves a middle zone).
        let mut p = unit_pane(10, 20);
        for i in 0..60 {
            p.feed(&format!("L{i}\r\n"));
        }
        // At the live edge; begin a drag at the bottom and pull it to the top row.
        p.selection_begin(0.5, 19.5, 10.0, 20.0);
        p.selection_update(0.5, 0.5, 10.0, 20.0); // y < edge(8) → top band
        assert!(p.selection_is_drag());
        let off0 = p.scroll_offset();
        let grew_before = p.selection_text();
        assert!(p.selection_autoscroll_tick(), "top-edge drag autoscrolls");
        assert!(p.scroll_offset() > off0, "scrolled up into history");
        // The head was re-mapped to the newly revealed top line → the selection text changed.
        assert_ne!(
            p.selection_text(),
            grew_before,
            "selection grew into scrollback"
        );
    }

    #[test]
    fn drag_at_bottom_edge_autoscrolls_toward_live_edge() {
        let mut p = unit_pane(10, 20);
        for i in 0..60 {
            p.feed(&format!("L{i}\r\n"));
        }
        p.scroll_by(30); // scroll up into history first
        let off0 = p.scroll_offset();
        assert!(off0 > 0);
        p.selection_begin(0.5, 0.5, 10.0, 20.0);
        p.selection_update(8.5, 19.5, 10.0, 20.0); // y > sh-edge → bottom band
        assert!(p.selection_is_drag());
        assert!(
            p.selection_autoscroll_tick(),
            "bottom-edge drag autoscrolls"
        );
        assert!(p.scroll_offset() < off0, "scrolled toward the live edge");
        // Releasing the button stops autoscroll even though the selection is kept.
        p.end_selection_drag();
        assert!(
            !p.selection_autoscroll_tick(),
            "no drag in flight → no autoscroll"
        );
    }

    #[test]
    fn no_autoscroll_when_pointer_is_in_the_middle() {
        let mut p = unit_pane(10, 20);
        for i in 0..60 {
            p.feed(&format!("L{i}\r\n"));
        }
        p.scroll_by(15);
        let off0 = p.scroll_offset();
        p.selection_begin(0.5, 9.5, 10.0, 20.0);
        p.selection_update(8.5, 10.5, 10.0, 20.0); // middle band (8..12) — no edge
        assert!(!p.selection_autoscroll_tick(), "middle drag doesn't scroll");
        assert_eq!(p.scroll_offset(), off0);
    }

    #[test]
    fn scrollbar_appears_after_a_scroll_and_is_hidden_at_the_bottom_with_no_history() {
        let mut p = unit_pane(20, 3);
        // No history yet → no scrollbar even right after a (clamped) scroll.
        assert!(p.scrollbar(60.0).is_none(), "no scrollback → no bar");
        for _ in 0..50 {
            p.feed("line\r\n");
        }
        // A fresh scroll shows the bar at full opacity with a thumb shorter than the track.
        p.scroll_by(5);
        let (thumb_y, thumb_h, op) = p.scrollbar(60.0).expect("bar shows right after a scroll");
        assert!(
            (op - 1.0).abs() < 1e-3,
            "fully opaque immediately after scrolling"
        );
        assert!((SCROLLBAR_MIN_THUMB_PX..60.0).contains(&thumb_h));
        assert!(thumb_y >= 0.0);
    }

    #[test]
    fn selection_on_cursor_row_is_prompt_line_only() {
        // 1px/cell on a 20x5 surface. Put the cursor (the "prompt") on row 2.
        let mut p = unit_pane(20, 5);
        p.feed("a\r\nb\r\nprompt"); // rows 0,1,2; cursor ends on row 2
        let (w, h) = (20.0, 5.0);
        assert!(!p.selection_on_cursor_row(), "no selection → false");
        // A single-row drag ON the cursor row is the editable prompt line.
        p.selection_begin(0.5, 2.5, w, h);
        p.selection_update(5.5, 2.5, w, h);
        assert!(p.selection_is_drag());
        assert!(
            p.selection_on_cursor_row(),
            "selection on the cursor row is the prompt line"
        );
        // A drag on a different row is not the prompt line.
        p.selection_begin(0.5, 0.5, w, h);
        p.selection_update(3.5, 0.5, w, h);
        assert!(
            !p.selection_on_cursor_row(),
            "an off-row selection is not the prompt line"
        );
        // A multi-row selection (even one touching the cursor row) is not a single prompt line.
        p.selection_begin(0.5, 0.5, w, h);
        p.selection_update(3.5, 2.5, w, h);
        assert!(
            !p.selection_on_cursor_row(),
            "a multi-row selection is not a prompt line"
        );
        // A stationary click (not dragged) is not a selection.
        p.selection_begin(0.5, 2.5, w, h);
        assert!(
            !p.selection_on_cursor_row(),
            "a click is not a drag-selection"
        );
    }

    /// 10px/cell on a 20x5 grid: drags between exact cells while clearing the 4px threshold.
    fn prompt_pane() -> (TerminalPane, f32, f32) {
        let mut p = unit_pane(20, 5);
        p.feed("a\r\nb\r\nprompt"); // cursor ends at row 2, col 6
        (p, 200.0, 50.0)
    }

    fn drag(p: &mut TerminalPane, c1: usize, c2: usize, row: usize, w: f32, h: f32) {
        let y = row as f32 * 10.0 + 5.0;
        p.selection_begin(c1 as f32 * 10.0 + 5.0, y, w, h);
        p.selection_update(c2 as f32 * 10.0 + 5.0, y, w, h);
        assert!(p.selection_is_drag());
    }

    #[test]
    fn type_over_backspaces_a_selection_left_of_the_caret() {
        // Select all of "prompt" (cols 0..=5); caret at col 6, right after it → 6 backspaces.
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 0, 5, 2, w, h);
        assert_eq!(p.type_over_selection(), Some(vec![0x7f; 6]));
        assert!(!p.selection_is_drag(), "type-over consumes the selection");
    }

    #[test]
    fn type_over_steps_left_across_a_gap_before_backspacing() {
        // Select "ro" (cols 1..=2); caret at col 6 → 3 lefts to land after the selection, 2 BS.
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 1, 2, 2, w, h);
        assert_eq!(
            p.type_over_selection(),
            Some(b"\x1b[D\x1b[D\x1b[D\x7f\x7f".to_vec())
        );
    }

    #[test]
    fn type_over_forward_deletes_a_selection_right_of_the_caret() {
        // Select cols 8..=9 (right of the caret at col 6) → 2 rights + 2 forward-deletes.
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 8, 9, 2, w, h);
        assert_eq!(
            p.type_over_selection(),
            Some(b"\x1b[C\x1b[C\x1b[3~\x1b[3~".to_vec())
        );
    }

    #[test]
    fn type_over_splits_around_a_caret_inside_the_selection() {
        // Select cols 4..=8 with the caret at col 6 → 2 BS (cols 4-5) + 3 FDEL (cols 6-8).
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 4, 8, 2, w, h);
        assert_eq!(
            p.type_over_selection(),
            Some(b"\x7f\x7f\x1b[3~\x1b[3~\x1b[3~".to_vec())
        );
    }

    #[test]
    fn type_over_declines_off_row_and_keeps_the_selection() {
        // A selection on a non-cursor row is not editable text — no bytes, selection intact
        // (the caller just clears the highlight).
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 0, 3, 0, w, h);
        assert_eq!(p.type_over_selection(), None);
        assert!(
            p.selection_is_drag(),
            "declining must not consume the selection"
        );
    }

    #[test]
    fn type_over_spans_a_soft_wrapped_input_line() {
        // 30 chars on a 20-col grid soft-wrap onto row 1 (cursor row 1, col 10). A selection
        // up on row 0 is STILL the same editable line — erase distances go linear through the
        // wrap: caret lin=30, selection cols 5..=8 lin → 21 lefts + 4 backspaces.
        let mut p = unit_pane(20, 5);
        p.feed("abcdefghijklmnopqrstuvwxyz0123");
        let (w, h) = (200.0, 50.0);
        drag(&mut p, 5, 8, 0, w, h);
        let mut expect = b"\x1b[D".repeat(21);
        expect.extend_from_slice(&[0x7f; 4]);
        assert_eq!(p.type_over_selection(), Some(expect));
    }

    #[test]
    fn type_over_declines_on_the_alternate_screen() {
        // In a TUI (vim/htop) the erase bytes would be app commands, not line edits.
        let (mut p, w, h) = prompt_pane();
        p.feed("\x1b[?1049h"); // enter the alternate screen
        drag(&mut p, 0, 5, 2, w, h);
        assert_eq!(p.type_over_selection(), None);
    }

    #[test]
    fn changing_cwd_clears_the_verify_cache() {
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some("/a".to_string()));
        // Prime the cache with a fake entry, then a cwd change must drop it.
        p.verified.insert(
            "x".to_string(),
            paths::ResolveResult {
                token: "t".into(),
                abs_path: "/a/t".into(),
                exists: true,
                is_dir: false,
                is_exe: false,
            },
        );
        assert!(!p.verified.is_empty());
        p.set_cwd(Some("/b".to_string()));
        assert!(
            p.verified.is_empty(),
            "a cwd change must clear stale resolutions"
        );
    }
}
