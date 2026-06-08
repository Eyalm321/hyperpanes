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
use crate::selection::{self, Selection};
use hyperpanes_core::paths::{self, OpenResult, ResolveResult};
use slint::Image;
use std::collections::HashMap;
use std::time::Instant;

/// How long a copy/paste indicator ("toast") stays up, in ms — matches the Electron pane's
/// 1.6s auto-dismiss in `Terminal.tsx`.
const TOAST_MS: u128 = 1600;

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
    /// System clipboard handle for copy-on-select / right-click paste (kept open for the pane's
    /// life — see [`crate::clipboard`]).
    clipboard: Clipboard,
    /// The transient copy/paste indicator ("toast") + when it was raised; auto-expires after
    /// [`TOAST_MS`]. Drained by [`toast_text`](Self::toast_text).
    toast: Option<(String, Instant)>,
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
            clipboard: Clipboard::new(),
            toast: None,
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
    }

    /// Extend the active selection's head to the (logical-px) cursor point during a drag.
    pub fn selection_update(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) {
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
        Some(text)
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
