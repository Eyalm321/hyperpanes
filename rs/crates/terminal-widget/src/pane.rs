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
use crate::links::extract_path_candidates;
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
}

/// A resolved, on-disk-verified link under the cursor: where to draw the hover underline (in the
/// pane's *logical* pixel space) plus the absolute target. Returned by [`TerminalPane::link_at`].
#[derive(Debug, Clone, PartialEq)]
pub struct LinkHit {
    /// Underline rect in logical px within the pane surface.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Absolute, verified path the link points at.
    pub abs_path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Tooltip label (`abs_path` with any `:line[:col]` suffix appended).
    pub tip: String,
}

/// The outcome of activating (clicking) a link. Mirrors the Electron split: a plain click opens
/// (editor / OS default), Ctrl/Cmd-click copies the absolute path.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkAction {
    /// Ctrl/Cmd-click — the caller should copy this absolute path to the clipboard.
    Copy(String),
    /// Plain click — the file/dir was opened (the result carries blocked/err detail).
    Opened(OpenResult),
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

    /// Hit-test a (logical-px) hover point against the rendered grid. Returns the underline rect +
    /// target when the point is over a path that exists on disk, else `None`. The candidate's
    /// `:line[:col]` is carried through (and shown in the tooltip), but only the resolved path is
    /// verified — mirroring the Electron link provider.
    pub fn link_at(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        let (r, start, end, row, cell_w, cell_h) = self.locate(x, y, surf_w, surf_h)?;
        // Recover the candidate's line/col by re-extracting (cheap; same row). The resolved
        // record only carries the path, so pull location off the candidate that covered the cell.
        let snap = self.grid.snapshot();
        let text = Self::row_text(&snap, row);
        let cand = extract_path_candidates(&text)
            .into_iter()
            .find(|c| c.start == start && c.end == end);
        let (line, col) = cand.as_ref().map(|c| (c.line, c.col)).unwrap_or((None, None));

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
        let hit = self.link_at(x, y, surf_w, surf_h)?;
        if ctrl {
            return Some(LinkAction::Copy(hit.abs_path));
        }
        let res = paths::open_resolved_path(&hit.abs_path, hit.line, hit.col, editor_command);
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
        Some(selection::Cell { col, row })
    }

    /// Begin a drag-selection anchored at the (logical-px) press point. Replaces any prior
    /// selection. The selection only starts *rendering* once the drag leaves the anchor cell, so
    /// a click that doesn't move still falls through to a link activation.
    pub fn selection_begin(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) {
        self.selection = self.cell_at_clamped(x, y, surf_w, surf_h).map(Selection::new);
        self.select_origin = Some((x, y));
    }

    /// Extend the active selection's head to the (logical-px) cursor point during a drag.
    ///
    /// Gated on [`DRAG_THRESHOLD_PX`]: until the pointer has moved that far from the press point
    /// the head stays pinned to the anchor (so the selection never becomes `dragged` and a click
    /// can't copy-on-select). This dead zone is what stops a stray click twitch — especially one
    /// that straddles a cell boundary — from clobbering the clipboard ahead of a paste.
    pub fn selection_update(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) {
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
        let mut sel = Selection::new(selection::Cell { col: 0, row: 0 });
        sel.update(selection::Cell { col: cols - 1, row: rows - 1 });
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
        self.selection.map_or(false, |s| s.dragged)
    }

    /// Highlight rectangles (logical px) for the active *dragged* selection over a surface of
    /// `surf_w`×`surf_h`. Empty for no selection or a non-dragged click — so a plain click never
    /// leaves a stray one-cell highlight.
    pub fn selection_rects(&self, surf_w: f32, surf_h: f32) -> Vec<(f32, f32, f32, f32)> {
        let sel = match &self.selection {
            Some(s) if s.dragged => s,
            _ => return Vec::new(),
        };
        let (cell_w, cell_h, cols, _rows) = match self.cell_logical(surf_w, surf_h) {
            Some(t) => t,
            None => return Vec::new(),
        };
        selection::selection_rects(sel, cols, cell_w, cell_h)
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
        let snap = self.grid.snapshot();
        if snap.cols == 0 {
            return None;
        }
        let last_col = snap.cols - 1;
        let mut lines = Vec::new();
        for row in start.row..=end.row.min(snap.rows.saturating_sub(1)) {
            let col_start = if row == start.row { start.col } else { 0 };
            let col_end = if row == end.row { end.col } else { last_col };
            let mut line = String::new();
            for col in col_start..=col_end.min(last_col) {
                let ch = snap.cell(col, row).ch;
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

    /// Read the system clipboard for a right-click / Ctrl+V paste, raising a "Pasted …"
    /// indicator. Returns the text the caller should write to this pane's session (the controller
    /// doesn't own the session transport), or `None` when the clipboard is empty/unavailable.
    pub fn paste_from_clipboard(&mut self) -> Option<String> {
        let text = self.clipboard.paste()?;
        let n = text.chars().count();
        self.set_toast(format!("Pasted {} char{}", n, if n == 1 { "" } else { "s" }));
        Some(prepare_paste(&text, self.grid.bracketed_paste()))
    }

    /// Pin the viewport back to the live edge (display offset 0) so the cursor is visible at the
    /// end of whatever was just written — e.g. after a paste, regardless of scrollback position.
    pub fn scroll_to_bottom(&mut self) {
        self.grid.scroll_to_bottom();
    }

    /// Scroll the scrollback viewport by `delta_lines` (positive = up into history, negative =
    /// toward the live edge), clamped to the history bounds. Drives mouse-wheel scrolling (a few
    /// lines per notch — see the widget's `scroll-requested` callback).
    pub fn scroll_by(&mut self, delta_lines: i32) {
        self.grid.scroll_by(delta_lines);
    }

    /// Scroll the scrollback viewport by one page (`up` = into history, else toward the live
    /// edge). A page is the visible row count less one row of overlap, so successive pages keep a
    /// line of context. Drives Shift+PageUp / Shift+PageDown.
    pub fn scroll_page(&mut self, up: bool) {
        let (_, rows) = self.grid_size();
        let page = (rows as i32 - 1).max(1);
        self.grid.scroll_by(if up { page } else { -page });
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
        if let Some(m) = self.search_index.and_then(|i| self.search_matches.get(i).copied()) {
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
        let hit = p.link_at(6.5, 0.5, w, h).expect("hover over note.txt should hit");
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
        assert!(!p.selection_is_drag(), "a sub-threshold twitch must not be a drag");
        assert!(p.selection_text().is_none(), "a click must not yield copyable text");
        assert!(p.selection_rects(100.0, 100.0).is_empty(), "a click leaves no highlight");
    }

    #[test]
    fn a_real_drag_past_threshold_selects_and_copies() {
        let mut p = unit_pane(10, 10);
        p.feed("abcdefghij"); // row 0 cells 0..10
        p.selection_begin(5.0, 5.0, 100.0, 100.0); // cell 0
        p.selection_update(55.0, 5.0, 100.0, 100.0); // 50px move → well past threshold, cell 5
        assert!(p.selection_is_drag(), "a real drag past the threshold selects");
        assert_eq!(p.selection_text().as_deref(), Some("abcdef"));
    }

    #[test]
    fn changing_cwd_clears_the_verify_cache() {
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some("/a".to_string()));
        // Prime the cache with a fake entry, then a cwd change must drop it.
        p.verified.insert("x".to_string(), paths::ResolveResult {
            token: "t".into(),
            abs_path: "/a/t".into(),
            exists: true,
            is_dir: false,
            is_exe: false,
        });
        assert!(!p.verified.is_empty());
        p.set_cwd(Some("/b".to_string()));
        assert!(p.verified.is_empty(), "a cwd change must clear stale resolutions");
    }
}
