//! [`TerminalPane`] — the reusable Rust controller for one terminal pane.
//!
//! It owns the renderer-agnostic grid model ([`TermGrid`]) and a chosen [`PaneRenderer`]
//! (software or GPU), and exposes a small, Wave-2-friendly API. It is deliberately
//! **decoupled from the session transport**: the caller pumps it.
//!
//! ## Lifecycle (how the app-shell drives N of these)
//! 1. Spawn/attach a session in `avada_core::session_manager` sized to your initial
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
use crate::links::{
    extract_commit_candidates, extract_path_candidates, extract_ref_candidates,
    extract_url_candidates, is_path_root, leading_path_fragment, trailing_path_fragment,
    trim_trailing_punct, PathCandidate, UrlCandidate,
};
use crate::render::{PaneRenderer, RenderOpts};
use crate::search::{self, Match};
use crate::selection::{self, Selection};
use avada_core::git;
use avada_core::paths::{self, ResolveResult};
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

/// Largest **OSC 52** clipboard store we will honour, in bytes. Anything a human would
/// plausibly want on the clipboard from a terminal program — a URL, a selection, a hash — fits
/// in 100 KiB with room to spare; past that it is far likelier to be a runaway program or a
/// hostile file being `cat`ed than an intent.
const MAX_OSC52_BYTES: usize = 100 * 1024;

/// Which of the **OSC 52** stores a single feed produced, if any, belongs on the system
/// clipboard. Split out from [`TerminalPane::honor_clipboard_writes`] so the policy can be
/// tested without an OS clipboard under it.
fn osc52_store_to_honor(stores: Vec<String>) -> Option<String> {
    let text = stores.into_iter().next_back()?;
    if text.is_empty() || text.len() > MAX_OSC52_BYTES {
        tracing::debug!("OSC 52 store ignored: {} bytes", text.len());
        return None;
    }
    Some(text)
}

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
    /// Commit hashes asked about in this pane, keyed by `cwd\x1ftoken`; the value is the full
    /// object name, or `None` for a hex-shaped word that names nothing. Unlike `verified`, the
    /// misses ARE cached: history is append-only, so a word that is not an object now will not
    /// become one, and every uncached lookup is a `git rev-parse` subprocess per hover.
    commits: HashMap<String, Option<String>>,
    /// Names this pane looked for across the whole repository, after the pane's own cwd could
    /// not place them, keyed by `cwd\x1ftoken`. Both answers are cached, misses included: the
    /// lookup is a `git ls-files` subprocess, and a hover that crosses a paragraph of prose
    /// would otherwise fire one per word that happens to end in `.py`. The price is a file
    /// created *after* it was first mentioned staying dark until the pane's cwd changes —
    /// paid only by this fallback, never by a path the cwd can resolve on its own.
    found: HashMap<String, Option<ResolveResult>>,
    /// The app's remembered project roots (absolute, normalized), tried as bases for a
    /// relative token after the pane's cwd and its own repository could not place it and
    /// before the screen is read. Pushed by the app whenever its project list changes;
    /// the widget never reads the store itself.
    project_roots: Vec<String>,
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

/// A link under the cursor — a path, an http/https URL, or a commit hash: where to draw the hover
/// underline (in the pane's *logical* pixel space) plus the target. Returned by
/// [`TerminalPane::link_at`], which only ever hands back paths that exist, and by
/// [`TerminalPane::link_target_at`], which does not.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkHit {
    /// Underline rect in logical px within the pane surface.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    /// Absolute path the link points at — the URL itself when [`is_url`](Self::is_url), or the
    /// full 40-character object name when [`is_commit`](Self::is_commit).
    pub abs_path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Tooltip label (`abs_path` with any `:line[:col]` suffix appended; the URL verbatim).
    pub tip: String,
    /// `true` for an http/https URL (routed by the app, never disk-verified).
    pub is_url: bool,
    /// `true` for a commit hash git confirmed exists in this pane's repository. `abs_path` then
    /// holds the full object name and `commit_cwd` the directory the lookup ran in — the app
    /// needs both to load the commit back out of the right repository.
    pub is_commit: bool,
    /// The directory the commit was resolved against, for an [`is_commit`](Self::is_commit) hit.
    pub commit_cwd: String,
    /// Whether the path was found on disk. Always `true` for a hover hit and for a URL; only a
    /// [`link_target_at`](TerminalPane::link_target_at) lookup can report `false`. Copying a path
    /// works either way — a build log naming a file that failed to generate is exactly when the
    /// path is worth having — but revealing one that isn't there has nothing to show.
    pub exists: bool,
    /// `true` when the running program *declared* this link with an **OSC 8** escape, rather
    /// than the terminal sniffing a URL out of rendered text. The distinction is what lets a
    /// plain click open it even while the program holds the mouse grab: a program that both
    /// grabs the mouse and marks a cell as a hyperlink has asked for the click to mean
    /// "follow me", so honouring it is obeying the program, not overriding it.
    pub is_osc8: bool,
}

/// The outcome of activating (clicking) a link: a plain click reveals the target (the left file
/// tree for a path, the app's browser preference for a URL), Ctrl/Cmd-click copies it.
///
/// Every variant hands the target *back* rather than acting on it. The widget cannot see the
/// left panel or the browser preference, and a widget that shelled out to the OS default would
/// silently bypass both.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkAction {
    /// Ctrl/Cmd-click — the caller should copy this absolute path (or URL) to the clipboard.
    Copy(String),
    /// Plain click on a path — the caller should reveal it in the left file tree, from where the
    /// human picks what to do with it (open in an editor pane, a terminal, a viewer). Opening it
    /// straight into whatever the OS thinks owns the extension takes that choice away.
    Reveal {
        path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Plain click on a URL — the caller should route it through Preferences → Browser (the OS
    /// default, one chosen browser, or the "ask each time" chooser).
    OpenUrl(String),
    /// Plain click on a commit hash — the caller should show that commit in the left git panel,
    /// from where the human can read the message, see the diff, or act on any file it touched.
    /// `cwd` is the directory the hash resolved in, which is what says *which* repository.
    ShowCommit { cwd: String, hash: String },
}

/// What [`TerminalPane::locate`] found under the pointer: the resolved token, its `line` /
/// `col` suffixes, the grid `(line, start col, end col)` it spans, and the surface x/y it was
/// hit at.
type Located = (
    ResolveResult,
    Option<u32>,
    Option<u32>,
    usize,
    usize,
    usize,
    f32,
    f32,
);

/// One wrap-run of the grid contributing a slice to a path its printing program broke apart:
/// the run's first row, how many rows it spans, and the char span within that run's joined text
/// — the coordinates [`TerminalPane::row_segment`] takes.
#[derive(Debug, Clone, Copy)]
struct Piece {
    first: usize,
    rows: usize,
    start: usize,
    end: usize,
}

/// A path reassembled out of several [`Piece`]s, with whatever `:line:col` suffix the joined
/// text turned out to be carrying.
#[derive(Debug)]
struct Rejoined {
    path: String,
    line: Option<u32>,
    col: Option<u32>,
    pieces: Vec<Piece>,
}

impl TerminalPane {
    /// Create a pane of `cols`×`rows` cells driving the given renderer. Use
    /// [`crate::render::SoftwareRenderer`] (always available) or
    /// [`crate::render::GpuRenderer`] (when a wgpu device is in hand).
    #[tracing::instrument(level = "debug", skip(renderer))]
    pub fn new(cols: usize, rows: usize, renderer: Box<dyn PaneRenderer>) -> Self {
        Self::with_scrollback(cols, rows, TermGrid::DEFAULT_SCROLLBACK, renderer)
    }

    /// Change how many lines of history the pane keeps (the app's `scrollback` preference
    /// applied live). Shrinking drops the oldest lines; a no-op when unchanged.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_scrollback(&mut self, scrollback: usize) {
        self.grid.set_scrollback(scrollback);
    }

    /// The scrollback depth the pane is configured with.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn scrollback_capacity(&self) -> usize {
        self.grid.scrollback_capacity()
    }

    /// [`Self::new`] with an explicit scrollback depth (lines of history kept above the
    /// viewport) — the hook for the app's `scrollback` preference. Use
    /// [`Self::set_scrollback`] to change it on a live pane.
    #[tracing::instrument(level = "debug", skip(renderer))]
    pub fn with_scrollback(
        cols: usize,
        rows: usize,
        scrollback: usize,
        renderer: Box<dyn PaneRenderer>,
    ) -> Self {
        Self {
            grid: TermGrid::with_scrollback(cols, rows, scrollback),
            renderer,
            cwd: None,
            verified: HashMap::new(),
            commits: HashMap::new(),
            found: HashMap::new(),
            project_roots: Vec::new(),
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn feed(&mut self, data: &str) {
        self.grid.feed(data.as_bytes());
        self.honor_clipboard_writes();
    }

    /// Feed raw output bytes (when you have bytes rather than a decoded `String`).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.grid.feed(bytes);
        self.honor_clipboard_writes();
    }

    /// Put any **OSC 52** clipboard store the last feed decoded onto the system clipboard, and
    /// toast it exactly like an explicit copy. Claude Code's `(c to copy)`, tmux's
    /// `set-clipboard`, and `vim --clipboard=unnamed` over ssh all arrive here.
    ///
    /// Two limits, both deliberate:
    ///
    /// * **Last write wins.** A program that emits several stores in one chunk meant the final
    ///   one; replaying the earlier ones would just flicker the clipboard.
    /// * **Size cap.** An unbounded store lets a single `cat` of a hostile file push megabytes
    ///   into the clipboard. Real uses (a URL, a selection, a commit hash) are far below the cap.
    ///
    /// The *read* half of OSC 52 stays off — see [`TermGrid::take_clipboard_writes`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn honor_clipboard_writes(&mut self) {
        let Some(text) = osc52_store_to_honor(self.grid.take_clipboard_writes()) else {
            return;
        };
        if self.clipboard.copy(&text) {
            self.set_toast("Copied to clipboard");
        }
    }

    /// Drain terminal-originated replies (DSR/DA/etc.) that must be written back to the
    /// session's pty. Empty when there is nothing to forward. **Must** be forwarded or a
    /// real conpty blocks at startup (see [`TermGrid::take_replies`]).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.grid.take_replies()
    }

    /// Resize the pane's grid. Returns `true` if the cell dimensions changed — in which
    /// case the caller should also `SessionManager::resize` the bound session.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        self.grid.resize(cols, rows)
    }

    /// Take-and-clear the repaint flag. `true` means the grid changed since the last call.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn take_dirty(&mut self) -> bool {
        self.grid.take_dirty()
    }

    /// Render the current grid to a `slint::Image` at the pane's *physical* pixel
    /// resolution (`cols*cell_w × rows*cell_h`). Cheap to call repeatedly — the renderer
    /// caches its buffers/atlas — but gate it on [`take_dirty`](Self::take_dirty) plus the
    /// cursor blink for minimal CPU.
    #[tracing::instrument(level = "debug", ret, skip(self, font, opts))]
    pub fn render(&mut self, font: &mut Font, opts: &RenderOpts) -> Image {
        let snap = self.grid.snapshot();
        self.renderer.render(&snap, font, opts)
    }

    /// Current grid size in `(cols, rows)`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn grid_size(&self) -> (usize, usize) {
        let s = self.grid.size();
        (s.cols, s.rows)
    }

    /// The visible screen as plain text — one line per viewport row (blank cells as spaces),
    /// with trailing whitespace and trailing blank lines trimmed. Lets a host feed an ambient
    /// summariser the *rendered* screen (what the user actually sees) instead of the raw redraw
    /// byte stream, so a continuously-repainting TUI (e.g. an agent CLI) is captured cleanly
    /// rather than as redraw noise.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn renderer_name(&self) -> &'static str {
        self.renderer.name()
    }

    /// Apply a colour theme: override the 16 base ANSI colours (index 0 = default background,
    /// 7 = default foreground). See [`crate::grid::TermGrid::set_base16`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_palette(&mut self, base: [[u8; 3]; 16]) {
        self.grid.set_base16(base);
    }

    /// Swap the renderer at runtime (e.g. GPU↔software on a device-lost / RDP transition).
    /// The next [`render`](Self::render) rebuilds from the live grid, so the swap is seamless.
    #[tracing::instrument(level = "debug", ret, skip(self, renderer))]
    pub fn set_renderer(&mut self, renderer: Box<dyn PaneRenderer>) {
        self.renderer = renderer;
    }

    // ---- Clickable file paths --------------------------------------------------------------
    //
    // Plain click opens the file (editor / OS default); Ctrl/Cmd-click copies the resolved
    // absolute path. Paths are verified on disk (against this pane's cwd) before they linkify,
    // so prose tokens don't light up. The grid extraction lives in [`crate::links`]; resolve +
    // open live in [`avada_core::paths`]. This is the renderer-side glue (Spike's
    // `Terminal.tsx` link provider, ported to the cell grid).

    /// Set this pane's working directory (the base for resolving relative path tokens). Clearing
    /// or changing it drops the verify cache, since the same token can resolve elsewhere.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        if cwd != self.cwd {
            self.cwd = cwd;
            self.verified.clear();
            self.commits.clear();
            self.found.clear();
        }
    }

    /// Replace the project roots. A changed set drops the verify cache: a token the screen
    /// answered earlier may now have a project answer, and the policy says the project
    /// wins. An unchanged set (the app reloads its list on every `cd`) keeps the cache.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_project_roots(&mut self, roots: Vec<String>) {
        if roots != self.project_roots {
            self.project_roots = roots;
            self.verified.clear();
        }
    }

    /// The logical-px cell size for a surface of `surf_w`×`surf_h`, or `None` for a degenerate
    /// pane.
    ///
    /// `surf_w`/`surf_h` must be the size of the RENDERED IMAGE (`cols*cell_w × rows*cell_h`,
    /// converted to logical px), not of the widget body it sits in. The widget once stretched
    /// the image over the body with `image-fit: fill`, when the two were interchangeable; it
    /// now pins the image 1:1 at its source resolution for crisp glyphs, so the body is up to
    /// a cell wider and a row taller. Passing the body divides that slack back into every cell,
    /// and because the error is multiplied by the column index it reaches a whole cell by the
    /// right-hand edge — the pointer selects one glyph left of the one under it.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn cell_logical(&self, surf_w: f32, surf_h: f32) -> Option<(f32, f32, usize, usize)> {
        let (cols, rows) = self.grid_size();
        if cols == 0 || rows == 0 || surf_w <= 0.0 || surf_h <= 0.0 {
            return None;
        }
        Some((surf_w / cols as f32, surf_h / rows as f32, cols, rows))
    }

    /// Reconstruct one viewport row's text (one char per column; blanks as spaces) so the
    /// `links` extractor's column indices line up with cells. Exact for ASCII paths.
    #[tracing::instrument(level = "debug", ret, skip(snap))]
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

    /// The hovered cell's *logical* line: viewport rows joined across soft wraps, the hovered
    /// cell's index into that text, and the first row of the run.
    ///
    /// A URL or path wider than the pane is one logical token that alacritty stores across two
    /// viewport rows, with `WRAPLINE` on the first. Extracting per visual row cuts it in half
    /// and the half under the cursor is what gets opened -- a link reading
    /// `http://127.0.0.1:51551/row18` opened `http://127.0.0.1:51`. The extractor in
    /// [`crate::links`] was always written against a wrap-joined line (see `cell_from_index`);
    /// this is the join it was waiting for.
    #[tracing::instrument(level = "debug", ret, skip(self, snap))]
    fn logical_line(
        &self,
        snap: &crate::grid::GridSnapshot,
        row: usize,
        col: usize,
    ) -> Option<(String, usize, usize)> {
        if row >= snap.rows || col >= snap.cols {
            return None;
        }
        let mut first = row;
        while first > 0 && self.grid.row_wraps(first - 1) {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < snap.rows && self.grid.row_wraps(last) {
            last += 1;
        }
        let mut text = String::with_capacity((last - first + 1) * snap.cols);
        for r in first..=last {
            text.push_str(&Self::row_text(snap, r));
        }
        Some((text, (row - first) * snap.cols + col, first))
    }

    /// The part of a logical-line span `[start, end)` that falls on visual `row` of a wrap run
    /// beginning at `first`, as columns of that row. The underline is one rect per [`LinkHit`],
    /// so a token spanning two rows underlines the row the cursor is actually on.
    #[tracing::instrument(level = "debug", ret)]
    fn row_segment(
        start: usize,
        end: usize,
        row: usize,
        first: usize,
        cols: usize,
    ) -> (usize, usize) {
        let off = (row - first) * cols;
        (
            start.saturating_sub(off).min(cols),
            (end.min(off + cols)).saturating_sub(off),
        )
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn cache_key(&self, token: &str) -> String {
        format!("{}\u{1f}{}", self.cwd.as_deref().unwrap_or(""), token)
    }

    /// Find a path token under the (logical-px) point, returning the resolved record, the
    /// candidate's column span, and the cell metrics. Resolution is cached per (cwd, token);
    /// only existing paths are cached, so freshly-created files linkify on a later hover.
    ///
    /// `require_exists` is what separates the two callers. Hovering underlines a path only when
    /// it is really there, because an underline is a promise. Copying makes no such promise, and
    /// the token a human most wants off their screen is often one that does *not* exist yet — a
    /// compiler naming the output it failed to write, a traceback from another machine.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn locate(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
        require_exists: bool,
    ) -> Option<Located> {
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
        let (text, idx, first) = self.logical_line(&snap, row, col)?;
        let mut cand = extract_path_candidates(&text)
            .into_iter()
            .find(|c| idx >= c.start && idx < c.end)?;

        // A path with a space in it reaches here in pieces, because whitespace is the only
        // token boundary an unquoted line offers. Only disk can say where such a path ends, so
        // the repair happens here and not in `links`, which is deliberately diskless.
        if let Some(wide) = self.widen_across_spaces(&text, &cand) {
            cand = wide;
        }

        // A candidate that runs to the last column of a run that does NOT wrap was cut off by
        // the program that printed it: it chose its own line width and emitted a hard newline
        // mid-path, so alacritty recorded no `WRAPLINE` and `logical_line` had nothing to join
        // on. The rest of the path is still on screen — it is simply on a line the grid says is
        // unrelated. `join_hard_wraps` goes and gets it.
        //
        // What must not happen is the tempting halfway house of linking the surviving prefix:
        // `/System/Volumes/Data` broken at the edge would offer to open `/System/Volumes`, which
        // exists, and a link that opens the wrong thing is worse than no link at all. So when the
        // rejoin fails to find something real, the old refusal stands.
        let text_len = text.chars().count();
        let run_rows = text.chars().count() / snap.cols.max(1);
        let cut_at_end =
            cand.end >= text_len && run_rows > 0 && !self.grid.row_wraps(first + run_rows - 1);
        // The mirror case: the pointer is on a continuation row, whose fragment opens the run
        // because the indent is all that precedes it. The head is above.
        let cut_before = text.chars().take(cand.start).all(char::is_whitespace)
            && first > 0
            && !self.grid.row_wraps(first - 1)
            && self
                .logical_line(&snap, first - 1, 0)
                .is_some_and(|(above, _, _)| trailing_path_fragment(&above).is_some());
        if cut_at_end || cut_before {
            if let Some(j) = self.join_hard_wraps(&snap, &text, &cand, first, run_rows, cut_at_end)
            {
                return self.located(j, row, snap.cols, cell_w, cell_h);
            }
            if cut_at_end {
                return None;
            }
        }

        let key = self.cache_key(&cand.path);
        let resolved = match self.verified.get(&key) {
            Some(hit) => hit.clone(),
            None => {
                let r = paths::resolve_path(self.cwd.as_deref(), &cand.path);
                if r.exists {
                    self.verified.insert(key, r.clone());
                    r
                } else if let Some(found) = self.elsewhere(&cand.path) {
                    // Not where this pane is standing, but somewhere in its repository — and
                    // only one such file, or `elsewhere` would have declined to guess.
                    self.verified.insert(key, found.clone());
                    found
                } else if let Some(found) = self.relative_to_project(&cand.path) {
                    // Inside exactly one remembered project. Tried before the screen on
                    // purpose: a project the app knows is stronger evidence than a directory
                    // that happened to be printed above, and it holds even after that line
                    // scrolls away.
                    self.verified.insert(key, found.clone());
                    found
                } else if let Some(found) = self.relative_to_screen(row, &cand.path) {
                    // Relative to a directory the screen itself named above this row — the
                    // `cd /some/where && git status` shape, where the pane's own cwd never
                    // moved but every path that follows belongs to `/some/where`.
                    self.verified.insert(key, found.clone());
                    found
                } else if require_exists {
                    return None;
                } else {
                    // A miss is never cached: a file that appears a second later has to
                    // linkify on the next hover rather than stay dark for the life of the pane.
                    r
                }
            }
        };
        // The candidate's line/col rides out with it: re-extracting to recover them would have
        // to rebuild the same joined line, and the span it matched on is a logical index now.
        let (start, end) = Self::row_segment(cand.start, cand.end, row, first, snap.cols);
        Some((
            resolved, cand.line, cand.col, start, end, row, cell_w, cell_h,
        ))
    }

    /// A backstop, not the limiter. [`Self::viable_prefix`] ends nearly every hopeless search
    /// after a single `stat`; this only bounds the pathological screen where every row keeps a
    /// live directory prefix alive.
    const MAX_JOIN_ROWS: usize = 32;

    /// Is it still worth appending another row to `partial`?
    ///
    /// Everything up to the last separator is committed — no continuation can change it — so if
    /// that directory does not exist, nothing appended to `partial` will ever name a real file.
    /// One `stat` per row buys the right to abandon the search instead of walking the screen.
    fn viable_prefix(&self, partial: &str) -> bool {
        match partial.rfind(['/', '\\']) {
            None | Some(0) => true,
            Some(i) => paths::resolve_path(self.cwd.as_deref(), &partial[..i]).exists,
        }
    }

    /// Does `joined` name something that exists? Hands back the normalized candidate along with
    /// how many characters [`extract_path_candidates`] trimmed off each end, so the underline can
    /// be pulled back off punctuation that was never part of the path.
    fn accept_joined(&self, joined: &str) -> Option<(PathCandidate, usize, usize)> {
        let cand = extract_path_candidates(joined).into_iter().next()?;
        if !paths::resolve_path(self.cwd.as_deref(), &cand.path).exists {
            return None;
        }
        let back = joined.chars().count().saturating_sub(cand.end);
        let front = cand.start;
        Some((cand, front, back))
    }

    /// Reassemble a path the printing program cut across several rows with hard newlines.
    ///
    /// Walks up while the fragment in hand opens its run and the run above ends flush, then down
    /// while each run keeps ending flush — as many rows as it takes. The search is bounded from
    /// the front rather than by a row count: the moment the committed directory prefix fails to
    /// exist, no continuation can rescue it and the walk stops.
    ///
    /// Existence is only *claimed* once, at the end, when the text stops being severed. Accepting
    /// the first joined prefix that happens to exist would put `/System/Volumes` back on screen.
    #[tracing::instrument(level = "debug", ret, skip(self, snap, text))]
    fn join_hard_wraps(
        &self,
        snap: &crate::grid::GridSnapshot,
        text: &str,
        cand: &PathCandidate,
        first: usize,
        run_rows: usize,
        cut_at_end: bool,
    ) -> Option<Rejoined> {
        let chars: Vec<char> = text.chars().collect();
        let mut joined: String = chars.get(cand.start..cand.end)?.iter().collect();
        let mut pieces = vec![Piece {
            first,
            rows: run_rows.max(1),
            start: cand.start,
            end: cand.end,
        }];

        let mut opens = chars[..cand.start].iter().all(|c| c.is_whitespace());
        while opens && pieces[0].first > 0 && pieces.len() < Self::MAX_JOIN_ROWS {
            let above = pieces[0].first - 1;
            if self.grid.row_wraps(above) {
                break; // a soft wrap — `logical_line` would already have joined it
            }
            let Some((atext, _, afirst)) = self.logical_line(snap, above, 0) else {
                break;
            };
            let Some((s, e)) = trailing_path_fragment(&atext) else {
                break;
            };
            let ac: Vec<char> = atext.chars().collect();
            joined.insert_str(0, &ac[s..e].iter().collect::<String>());
            pieces.insert(
                0,
                Piece {
                    first: afirst,
                    rows: above - afirst + 1,
                    start: s,
                    end: e,
                },
            );
            opens = ac[..s].iter().all(|c| c.is_whitespace());
        }

        let mut cut = cut_at_end;
        loop {
            if !cut {
                let (c, front, back) = self.accept_joined(&joined)?;
                return Some(Self::trimmed(c, pieces, front, back));
            }
            if pieces.len() >= Self::MAX_JOIN_ROWS || !self.viable_prefix(&joined) {
                return None;
            }
            let last = pieces.last()?;
            let next = last.first + last.rows;
            if next >= snap.rows {
                return None;
            }
            let (ntext, _, nfirst) = self.logical_line(snap, next, 0)?;
            let (s, e) = leading_path_fragment(&ntext)?;
            let nc: Vec<char> = ntext.chars().collect();
            joined.push_str(&nc[s..e].iter().collect::<String>());
            pieces.push(Piece {
                first: nfirst,
                rows: (nc.len() / snap.cols.max(1)).max(1),
                start: s,
                end: e,
            });
            cut = e >= nc.len();
        }
    }

    /// Pull the underline in off whatever punctuation `links` trimmed, spending each trim against
    /// the pieces from the outside in, and drop any piece left with nothing to draw.
    fn trimmed(cand: PathCandidate, mut pieces: Vec<Piece>, front: usize, back: usize) -> Rejoined {
        let mut front = front;
        for p in pieces.iter_mut() {
            let take = front.min(p.end - p.start);
            p.start += take;
            front -= take;
            if front == 0 {
                break;
            }
        }
        let mut back = back;
        for p in pieces.iter_mut().rev() {
            let take = back.min(p.end - p.start);
            p.end -= take;
            back -= take;
            if back == 0 {
                break;
            }
        }
        pieces.retain(|p| p.end > p.start);
        Rejoined {
            path: cand.path,
            line: cand.line,
            col: cand.col,
            pieces,
        }
    }

    /// Turn a rejoined path into the hit for the row actually under the pointer: every piece
    /// carries the same destination, only the underline differs.
    fn located(
        &mut self,
        j: Rejoined,
        row: usize,
        cols: usize,
        cell_w: f32,
        cell_h: f32,
    ) -> Option<Located> {
        let p = *j
            .pieces
            .iter()
            .find(|p| row >= p.first && row < p.first + p.rows)?;
        let key = self.cache_key(&j.path);
        let resolved = match self.verified.get(&key) {
            Some(hit) => hit.clone(),
            None => {
                // Only reached once the join has proved the path exists, so this is a hit and
                // caching it is safe.
                let r = paths::resolve_path(self.cwd.as_deref(), &j.path);
                self.verified.insert(key, r.clone());
                r
            }
        };
        let (start, end) = Self::row_segment(p.start, p.end, row, p.first, cols);
        Some((resolved, j.line, j.col, start, end, row, cell_w, cell_h))
    }

    /// Grow `cand` across single spaces, in both directions, while the result keeps naming a
    /// real file.
    ///
    /// `Artifact(/Users/bshuler/Library/Application Support/avada/lane-watch.html)` is one
    /// path, but nothing in the *text* says so: the space between `Application` and `Support` is
    /// indistinguishable from the space between two words of prose. Where such a path starts and
    /// ends is not a question the characters can answer — only `stat` can — which is why this
    /// lives beside the grid and not in [`crate::links`], whose whole suite runs without
    /// touching a disk.
    ///
    /// Both directions, because the human hovers wherever the interesting part is: over the
    /// directory at the front on one line, over the filename at the back on the next. A widening
    /// that only grew rightwards would light up the first half of that path and leave the half
    /// with the filename in it dark, which is the harder failure to explain.
    ///
    /// So: collect the word boundaries reachable on either side, and keep the LONGEST span that
    /// exists. Longest rather than first, because `…/Application Support` may itself be a real
    /// directory on the way to the file that was actually meant.
    ///
    /// Three things bound it. The seed must already have failed to resolve, so a path that
    /// stands on its own is never touched. A grown *start* must land on a path root (`/`, `~/`,
    /// `./`, `C:\`) — the shape a program prints when it announces a file, and one prose does
    /// not have. And a grown span is only ever accepted if it stats true, so the failure mode is
    /// "no link", never "wrong link".
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn widen_across_spaces(&self, text: &str, cand: &PathCandidate) -> Option<PathCandidate> {
        /// Four words each way covers `Application Support` and the deepest spaced name seen in
        /// the wild; past that the odds tilt towards swallowing the prose around the path.
        const MAX_WORDS: usize = 4;
        /// A hard stop for a pathological line of single-character words.
        const MAX_CHARS: usize = 200;

        if paths::resolve_path(self.cwd.as_deref(), &cand.path).exists {
            return None;
        }
        let chars: Vec<char> = text.chars().collect();

        // Where the path could START: here, or at the root marker inside any of the few words
        // to the left. Leftmost root within each word, not rightmost — the leading `/` of
        // `Artifact(/Users/bshuler/Library/Application`, not the one before `Application`.
        let mut starts = vec![cand.start];
        let mut word = cand.start;
        for _ in 0..MAX_WORDS {
            if word == 0 || chars[word - 1] != ' ' {
                break;
            }
            let mut b = word - 1;
            while b > 0 && chars[b - 1] != ' ' {
                b -= 1;
            }
            if let Some(k) = (b..word - 1).find(|&i| is_path_root(&chars[i..])) {
                starts.push(k);
            }
            word = b;
        }

        // Where it could END: here, or at the end of any of the few words to the right, with
        // that word's sentence punctuation trimmed — the `)` closing `Artifact(`, the full stop
        // ending the line — so the span stops where the name does.
        let mut ends = vec![cand.end];
        let mut e = cand.end;
        for _ in 0..MAX_WORDS {
            if chars.get(e) != Some(&' ') {
                break;
            }
            let ws = e + 1;
            let mut we = ws;
            while we < chars.len() && chars[we] != ' ' {
                we += 1;
            }
            if we == ws {
                break;
            }
            let w: String = chars[ws..we].iter().collect();
            ends.push(we - (w.chars().count() - trim_trailing_punct(&w).chars().count()));
            e = we;
        }

        let mut best: Option<PathCandidate> = None;
        for &st in &starts {
            for &en in &ends {
                // The seed itself is the one pair already known to miss.
                if (st, en) == (cand.start, cand.end) || en <= st || en - st > MAX_CHARS {
                    continue;
                }
                if best.as_ref().is_some_and(|b| en - st <= b.end - b.start) {
                    continue;
                }
                let path: String = chars[st..en].iter().collect();
                if paths::resolve_path(self.cwd.as_deref(), &path).exists {
                    best = Some(PathCandidate {
                        path,
                        line: cand.line,
                        col: cand.col,
                        start: st,
                        end: en,
                    });
                }
            }
        }
        best
    }

    /// The repository-wide fallback for a name the pane's own cwd could not place.
    ///
    /// A coding session says `b99_price.py` and means a file it is not standing next to. The
    /// repository is the only corpus that makes that name an answer rather than a guess, and
    /// [`git::find_in_repo`] declines outright when more than one file could be meant.
    #[tracing::instrument(level = "debug", skip_all)]
    fn elsewhere(&mut self, token: &str) -> Option<ResolveResult> {
        let cwd = self.cwd.clone()?;
        let key = self.cache_key(token);
        if let Some(hit) = self.found.get(&key) {
            return hit.clone();
        }
        let r = git::find_in_repo(std::path::Path::new(&cwd), token).and_then(|abs| {
            let abs_path = abs.to_string_lossy().into_owned();
            // Listed by git is not the same as present on disk — an index entry outlives a
            // `rm`. The link means "open this", so it has to be there.
            let md = std::fs::metadata(&abs).ok()?;
            Some(ResolveResult {
                token: token.to_string(),
                is_exe: paths::is_executable_ext(&abs_path),
                is_dir: md.is_dir(),
                exists: true,
                abs_path,
            })
        });
        self.found.insert(key, r.clone());
        r
    }

    /// How far above the hovered row the screen is read for a directory, in grid lines. A
    /// `cd` that scrolled further off than this is not what the human is looking at.
    const CONTEXT_LINES: usize = 200;
    /// The stat budget for one screen-context lookup. Every rooted token above the row, and
    /// each of its ancestors, is one candidate base; a screen full of absolute paths must not
    /// turn a hover into a disk walk.
    const CONTEXT_STATS: usize = 32;
    /// The stat budget for the project-root step: one per root, so a pathological project
    /// list cannot turn a hover into a disk walk.
    const PROJECT_ROOT_STATS: usize = 64;

    /// The project-root fallback for a relative path that neither the pane's cwd nor its
    /// repository could place: every remembered project is tried as a base, and the answer
    /// counts only when exactly one project holds the file. `README.md` is in all of them,
    /// and a link that opens the wrong project's copy is worse than no link, so a tie is
    /// handed on to the screen, where a printed `cd` can settle it. Nothing is cached here:
    /// a hit lands in `verified` by the caller, and a miss must stay a miss only until a
    /// project holding the file is added.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn relative_to_project(&self, token: &str) -> Option<ResolveResult> {
        let chars: Vec<char> = token.chars().collect();
        if is_path_root(&chars) {
            return None;
        }
        let mut hit: Option<ResolveResult> = None;
        for root in self.project_roots.iter().take(Self::PROJECT_ROOT_STATS) {
            let r = paths::resolve_path(Some(root), token);
            if r.exists {
                if hit.is_some() {
                    return None; // two projects answer: not ours to guess
                }
                hit = Some(r);
            }
        }
        hit
    }

    /// The screen-context fallback for a relative path that neither the pane's cwd, its
    /// repository, nor a remembered project could place.
    ///
    /// A tool that `cd`s somewhere and prints paths relative to *that* directory leaves the
    /// pane's cwd untouched — an agent's shell does exactly this — so `docs/x.yaml` is dark
    /// even though `/abs/project` is printed three lines up. The rooted tokens above the row
    /// are the only directories the screen vouches for, and they are tried nearest-first: the
    /// last `cd` wins the way it would in the shell. Each rooted token is tried as a base, then
    /// its ancestors (a path *inside* the project names the project too), until something on
    /// disk answers. Nothing about this is cached here — a hit lands in `verified` by the
    /// caller, and a miss must stay a miss only until the `cd` line scrolls into view.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn relative_to_screen(&self, row: usize, token: &str) -> Option<ResolveResult> {
        let chars: Vec<char> = token.chars().collect();
        if is_path_root(&chars) {
            return None;
        }
        let line = row as i32 - self.grid.display_offset() as i32;
        let mut stats = 0usize;
        let mut cursor = line - 1;
        let floor = line - Self::CONTEXT_LINES as i32;
        while cursor >= floor {
            let Some(mut text) = self.grid.line_text(cursor) else {
                break;
            };
            // Walk back to the head of a soft-wrapped run so a long `cd` target is whole.
            let mut first = cursor;
            while first > floor && self.grid.line_wraps(first - 1) {
                first -= 1;
                match self.grid.line_text(first) {
                    Some(t) => text.insert_str(0, &t),
                    None => break,
                }
            }
            for base in Self::rooted_bases(&text) {
                let mut dir: Option<&std::path::Path> = Some(std::path::Path::new(&base));
                while let Some(d) = dir {
                    if d.as_os_str().is_empty() || d.parent().is_none() {
                        break; // the root itself is not a project
                    }
                    if stats >= Self::CONTEXT_STATS {
                        return None;
                    }
                    stats += 1;
                    let r = paths::resolve_path(Some(&d.to_string_lossy()), token);
                    if r.exists {
                        return Some(r);
                    }
                    dir = d.parent();
                }
            }
            cursor = first - 1;
        }
        None
    }

    /// The rooted (absolute, `~/`, drive-lettered) path tokens on one logical line, rightmost
    /// first — on a `cd a && cd b` line the later one is where the shell ended up. `~` is
    /// expanded so the result can serve as a base directory verbatim.
    #[tracing::instrument(level = "debug", ret)]
    fn rooted_bases(text: &str) -> Vec<String> {
        let mut out: Vec<String> = extract_path_candidates(text)
            .into_iter()
            .filter(|c| {
                let cs: Vec<char> = c.path.chars().collect();
                is_path_root(&cs) && !c.path.starts_with('.')
            })
            .map(|c| paths::resolve_path(None, &c.path).abs_path)
            .collect();
        out.reverse();
        out.dedup();
        out
    }

    /// Find an http/https URL under the (logical-px) point, returning the candidate, its row,
    /// and the cell metrics. URLs linkify on shape alone — no disk/network verification (so no
    /// cache either; extraction per hover is cheap, same as the path re-extract in `link_at`).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn url_under(
        &self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<(UrlCandidate, usize, usize, usize, f32, f32)> {
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
        let (text, idx, first) = self.logical_line(&snap, row, col)?;
        let cand = extract_url_candidates(&text)
            .into_iter()
            .find(|c| idx >= c.start && idx < c.end)?;
        let (start, end) = Self::row_segment(cand.start, cand.end, row, first, snap.cols);
        Some((cand, start, end, row, cell_w, cell_h))
    }

    /// Find an **OSC 8 hyperlink** under the (logical-px) point, returning its URI, that row's
    /// column extent, the row, and the cell metrics.
    ///
    /// Unlike [`url_under`](Self::url_under) this reads no rendered text at all — the target
    /// comes from the escape the program emitted, so it survives a visible label that is
    /// truncated, renamed, or split across rows.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn osc8_under(
        &self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<(String, usize, usize, usize, f32, f32)> {
        let (cell_w, cell_h, cols, rows) = self.cell_logical(surf_w, surf_h)?;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let col = (x / cell_w) as usize;
        let row = (y / cell_h) as usize;
        if col >= cols || row >= rows {
            return None;
        }
        let (uri, start, end) = self.grid.hyperlink_at(col, row)?;
        Some((uri, start, end, row, cell_w, cell_h))
    }

    /// Build the [`LinkHit`] for an OSC 8 span. Shared by the hover and the click hit-tests,
    /// which differ only in what they fall back to.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn osc8_hit(&self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        let (uri, start, end, row, cell_w, cell_h) = self.osc8_under(x, y, surf_w, surf_h)?;
        // Only web links are ours to open. A program is free to point OSC 8 at `file:`,
        // `mailto:` or anything else, and handing an arbitrary scheme to the OS opener is how
        // a terminal turns a hostile `cat` into code execution.
        if !uri.starts_with("http://") && !uri.starts_with("https://") {
            return None;
        }
        Some(LinkHit {
            x: start as f32 * cell_w,
            y: (row as f32 + 1.0) * cell_h - 1.0,
            w: (end - start) as f32 * cell_w,
            tip: uri.clone(),
            abs_path: uri,
            line: None,
            col: None,
            is_url: true,
            is_commit: false,
            commit_cwd: String::new(),
            exists: true,
            is_osc8: true,
        })
    }

    /// Find a commit hash under the (logical-px) point, returning the full object name git
    /// resolved it to, the candidate's column span, and the cell metrics.
    ///
    /// The shape gate in [`crate::links`] is deliberately loose, so git is the real one: a
    /// hex-shaped word only linkifies once `rev-parse` confirms this repository holds a commit by
    /// that name. That costs one subprocess the first time a given word is hovered in a given
    /// cwd, and nothing afterwards — both hits and misses are cached, because history does not
    /// un-write itself.
    // The tuple predates this track; naming it is a refactor for another day.
    #[allow(clippy::type_complexity)]
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn commit_under(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
    ) -> Option<(
        String,
        String,
        usize,
        usize,
        usize,
        f32,
        f32,
        Option<String>,
    )> {
        // No cwd means no repository to ask. Unlike a path, a commit has no sensible fallback:
        // resolving it against the home directory would answer for whatever repo happens to be
        // there, which is never the one the text came from.
        let cwd = self.cwd.clone()?;
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
        let (text, idx, first) = self.logical_line(&snap, row, col)?;
        // A hex hash first; failing that, a `word/word` token — `origin/main` — that git can
        // name a commit for. Both go through the same cache and the same resolver, and a ref
        // carries its name out so the tooltip can say which branch the commit is the tip of.
        let (rev, cand_start, cand_end, label) = match extract_commit_candidates(&text)
            .into_iter()
            .find(|c| idx >= c.start && idx < c.end)
        {
            Some(c) => (c.hash, c.start, c.end, None),
            None => {
                let c = extract_ref_candidates(&text)
                    .into_iter()
                    .find(|c| idx >= c.start && idx < c.end)?;
                (c.name.clone(), c.start, c.end, Some(c.name))
            }
        };

        let key = self.cache_key(&rev);
        let full = match self.commits.get(&key) {
            Some(hit) => hit.clone(),
            None => {
                let r = git::resolve_commit(std::path::Path::new(&cwd), &rev);
                self.commits.insert(key, r.clone());
                r
            }
        }?;
        let (start, end) = Self::row_segment(cand_start, cand_end, row, first, snap.cols);
        Some((full, cwd, start, end, row, cell_w, cell_h, label))
    }

    /// Hit-test a (logical-px) hover point against the rendered grid. Returns the underline rect +
    /// target when the point is over an http/https URL or a path that exists on disk, else `None`.
    /// The candidate's `:line[:col]` is carried through (and shown in the tooltip), but only the
    /// resolved path is verified — mirroring the Electron link provider. URLs win when a token is
    /// both (a URL is path-shaped but never disk-verifies anyway).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn link_at(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        // A declared link beats a sniffed one: the program named its own target.
        if let Some(hit) = self.osc8_hit(x, y, surf_w, surf_h) {
            return Some(hit);
        }
        if let Some((cand, start, end, row, cell_w, cell_h)) = self.url_under(x, y, surf_w, surf_h)
        {
            return Some(LinkHit {
                x: start as f32 * cell_w,
                y: (row as f32 + 1.0) * cell_h - 1.0, // a hairline along the cell's baseline
                w: (end - start) as f32 * cell_w,
                tip: cand.url.clone(),
                abs_path: cand.url,
                line: None,
                col: None,
                is_url: true,
                is_commit: false,
                commit_cwd: String::new(),
                exists: true,
                is_osc8: false,
            });
        }
        self.path_hit(x, y, surf_w, surf_h, true)
            .or_else(|| self.commit_hit(x, y, surf_w, surf_h))
    }

    /// Hit-test for a link the human has *asked about* — a Ctrl/Cmd-click or a context menu —
    /// rather than merely hovered. Same geometry as [`link_at`](Self::link_at), minus the
    /// on-disk requirement: the returned [`LinkHit::exists`] says which kind it is, so a caller
    /// can copy any path-shaped token while still refusing to reveal one that isn't there.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn link_target_at(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        // A declared link beats a sniffed one: the program named its own target.
        if let Some(hit) = self.osc8_hit(x, y, surf_w, surf_h) {
            return Some(hit);
        }
        if let Some((cand, start, end, row, cell_w, cell_h)) = self.url_under(x, y, surf_w, surf_h)
        {
            return Some(LinkHit {
                x: start as f32 * cell_w,
                y: (row as f32 + 1.0) * cell_h - 1.0,
                w: (end - start) as f32 * cell_w,
                tip: cand.url.clone(),
                abs_path: cand.url,
                line: None,
                col: None,
                is_url: true,
                is_commit: false,
                commit_cwd: String::new(),
                exists: true,
                is_osc8: false,
            });
        }
        self.path_hit(x, y, surf_w, surf_h, false)
            .or_else(|| self.commit_hit(x, y, surf_w, surf_h))
    }

    /// The commit half of both hit-tests. It is the same for either caller: a hash git cannot
    /// resolve is not a target anybody can copy or open, so there is no "missing but wanted"
    /// case the way there is for a path.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn commit_hit(&mut self, x: f32, y: f32, surf_w: f32, surf_h: f32) -> Option<LinkHit> {
        let (full, cwd, start, end, row, cell_w, cell_h, label) =
            self.commit_under(x, y, surf_w, surf_h)?;
        let short = &full[..full.len().min(12)];
        Some(LinkHit {
            x: start as f32 * cell_w,
            y: (row as f32 + 1.0) * cell_h - 1.0,
            w: (end - start) as f32 * cell_w,
            tip: match &label {
                Some(name) => format!("{name} \u{b7} commit {short}"),
                None => format!("commit {short}"),
            },
            abs_path: full,
            line: None,
            col: None,
            is_url: false,
            is_commit: true,
            commit_cwd: cwd,
            exists: true,
            is_osc8: false,
        })
    }

    /// The path half of both hit-tests, differing only in whether a missing file still counts.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn path_hit(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
        require_exists: bool,
    ) -> Option<LinkHit> {
        let (r, line, col, start, end, row, cell_w, cell_h) =
            self.locate(x, y, surf_w, surf_h, require_exists)?;

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
            is_commit: false,
            commit_cwd: String::new(),
            exists: r.exists,
            is_osc8: false,
        })
    }

    /// Activate the link under a (logical-px) click. `ctrl` (Ctrl or Cmd held) copies the
    /// absolute path — whether or not it exists — while a plain click asks the caller to reveal
    /// it, which only a path that is really there can satisfy. `None` when the click wasn't over
    /// a link at all.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn activate_link(
        &mut self,
        x: f32,
        y: f32,
        surf_w: f32,
        surf_h: f32,
        ctrl: bool,
    ) -> Option<LinkAction> {
        // Suppress link open/copy at the end of a drag-selection. The widget fires
        // `selection-end` then `link-activated` on the same left-button release, and a dragged
        // selection is kept alive (copied, not cleared) by the shell — so a release that just
        // finished selecting text must NOT also open/copy the link under the release point. A
        // plain click begins a fresh, non-dragged selection, so this never blocks real clicks.
        if self.selection_is_drag() {
            return None;
        }
        // Ctrl/Cmd-click looks up the ungated way: a path that failed to exist never underlined,
        // but the human aimed at it deliberately and wants the text.
        let hit = if ctrl {
            self.link_target_at(x, y, surf_w, surf_h)?
        } else {
            self.link_at(x, y, surf_w, surf_h)?
        };
        if ctrl {
            // Copy here with the pane's own (arboard) clipboard handle — the proven path the
            // selection copy uses — instead of relying on the caller: the app shell's `clip`
            // shell-out failed silently in live testing. The action is still returned so the
            // caller can do its (now best-effort, redundant) copy and any follow-up UX.
            if self.clipboard.copy(&hit.abs_path) {
                self.set_toast(format!(
                    "Copied {} to clipboard",
                    if hit.is_url {
                        "link"
                    } else if hit.is_commit {
                        "commit"
                    } else {
                        "path"
                    }
                ));
            }
            return Some(LinkAction::Copy(hit.abs_path));
        }
        if hit.is_url {
            // Handed back, not opened — see `LinkAction::OpenUrl`.
            return Some(LinkAction::OpenUrl(hit.abs_path));
        }
        if hit.is_commit {
            return Some(LinkAction::ShowCommit {
                cwd: hit.commit_cwd,
                hash: hit.abs_path,
            });
        }
        Some(LinkAction::Reveal {
            path: hit.abs_path,
            line: hit.line,
            col: hit.col,
        })
    }

    // ---- Text selection ---------------------------------------------------------------------
    //
    // Drag to select a cell range; the controller turns it into highlight rects (for the Slint
    // overlay) and, on release, the selected text (copy-on-select — see `copy_selection`). The
    // model is our own (`crate::selection`) rather than alacritty's, kept in viewport cells.

    /// The (clamped) viewport cell under a logical-px point. Unlike [`locate`](Self::locate),
    /// this never returns `None` for an in-pane drag that strays past an edge — it clamps to the
    /// nearest cell so a selection can run to the grid border.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn selection_clear(&mut self) {
        self.selection = None;
        self.select_origin = None;
        self.drag_pointer = None;
    }

    /// End the drag (button released) — stops edge-autoscroll while KEEPING any selection (the
    /// controller may still copy it). Call from the `selection-end` handler.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn end_selection_drag(&mut self) {
        self.drag_pointer = None;
        self.select_origin = None;
    }

    /// One edge-autoscroll step while a selection drag is held in the top/bottom edge band: scroll
    /// the viewport one line toward that edge and extend the selection head to the just-revealed
    /// edge row, so the selection grows into off-screen scrollback (the vim/iTerm/Claude drag
    /// behavior). Returns `true` if it scrolled — the pump uses that to keep ticking + repainting.
    /// No-op unless a real (dragged) selection is in flight with the pointer at an edge.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn clear(&mut self) {
        self.selection = None;
        self.grid.feed(b"\x1b[H\x1b[2J\x1b[3J");
        self.grid.scroll_to_bottom();
    }

    /// True once the active selection has actually been dragged across cells (i.e. it's a real
    /// selection, not a stationary click). The caller uses this to choose copy-vs-click on release.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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

    /// A plain click on the shell's input line → the arrow keys that walk the caret to the
    /// clicked cell, or `None` when the click cannot safely be read as "put the caret here".
    ///
    /// Every other terminal-embedded line editor (readline, PSReadLine, Claude Code's prompt)
    /// already lets you move the caret with the arrow keys; the only thing missing was a way to
    /// say where you want it without counting the characters yourself. A terminal cannot ask the
    /// application to move its caret — there is no such escape sequence — so it does the only
    /// thing it can: it presses the arrow key for you, the right number of times.
    ///
    /// Safety model, deliberately the same one [`type_over_selection`](Self::type_over_selection)
    /// already relies on:
    ///
    ///  * **Left/right arrows only**, and every line editor **clamps them at the input-region
    ///    boundaries**. So a click on the prompt decoration, on a box border, or past the end of
    ///    what you typed lands the caret at the nearest end of the editable text rather than
    ///    doing something destructive. Nothing outside the input can be touched, and no vertical
    ///    motion is emitted — up/down is history recall in most editors, which would be exactly
    ///    the surprise this is trying to avoid.
    ///  * **Alternate screen excluded**: there these bytes are application commands (vim
    ///    motions), not caret movement.
    ///  * **Mouse-grabbing apps excluded**: they asked for the click themselves and get it
    ///    verbatim, as before.
    ///  * **The click must land on the cursor's own wrapped logical line.** A click on
    ///    scrollback, on command output, or on a different row of a drawn box is not an edit
    ///    position, and declining is free — the click keeps its old meaning.
    ///
    /// Cell↔char caveat: counts are in grid cells, exact for ASCII (same wide-glyph caveat as
    /// [`selection_text`](Self::selection_text)).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn click_move_cursor(&self) -> Option<Vec<u8>> {
        if self.grid.alt_screen() || self.grid.mouse_mode() {
            return None;
        }
        // The press anchor IS the clicked cell; a dragged selection is a selection, not a click.
        let target = match &self.selection {
            Some(s) if !s.dragged => s.anchor,
            _ => return None,
        };
        let crow = self.grid.cursor_row()?;
        let cline = crow as i32 - self.grid.display_offset() as i32;
        let (cols, _) = self.grid_size();
        if cols == 0 {
            return None;
        }
        let (lo, hi) = (target.line.min(cline), target.line.max(cline));
        // Soft-wrapped input (the shell's own line wrapped by the terminal): every grid line
        // from the upper of the two to (exclusive) the lower carries the WRAPLINE continuation
        // flag, so the whole thing is one editable line `cols` cells wide per row and the
        // editor's arrows walk straight through the wrap. Same test as `type_over_selection`.
        let steps = if (lo..hi).all(|r| self.grid.line_wraps(r)) {
            let lin = |line: i32, col: usize| line * cols as i32 + col as i32;
            lin(target.line, target.col) - lin(cline, self.grid.cursor_col())
        } else {
            // Hard rows: a TUI input box (Claude Code's `│ > … │` prompt, a `>`-marked
            // continuation) laid out over several rows by the application itself — a
            // Shift+Enter newline, or the box wrapping a long line on its own. The arrows still
            // walk the caret through the text as one buffer, so the walk is counted row by row
            // from the rows' visible text rather than assuming `cols` cells each.
            let rows = self.hard_input_rows(lo, hi)?;
            hard_row_offset(&rows, lo, target.line, target.col)
                - hard_row_offset(&rows, lo, cline, self.grid.cursor_col())
        };
        if steps == 0 {
            return None; // clicked the caret's own cell: nothing to send
        }
        const LEFT: &[u8] = b"\x1b[D";
        const RIGHT: &[u8] = b"\x1b[C";
        let seq = if steps > 0 { RIGHT } else { LEFT };
        let mut bytes = Vec::with_capacity(steps.unsigned_abs() as usize * seq.len());
        for _ in 0..steps.abs() {
            bytes.extend_from_slice(seq);
        }
        Some(bytes)
    }

    /// The per-row text extents of a multi-row TUI input spanning grid rows `lo..=hi`, or
    /// `None` when those rows do not look like one input box.
    ///
    /// The rows qualify when every one of them is framed by a box border (`│` / `║`) — the
    /// Claude Code prompt — or when at least one of them carries a `>` / `❯` prompt marker (a
    /// `>`-continued multi-line input without a frame). Plain output rows above a bare shell
    /// prompt match neither, so a click there keeps declining as before.
    ///
    /// Text on every row starts at the block's shared margin: the leftmost text column after
    /// the decoration, which is the marker row's own text column (continuation rows are indented
    /// under it). That margin, not each row's first non-blank cell, is the row's start, so a
    /// deliberately indented line keeps its leading spaces in the count.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn hard_input_rows(&self, lo: i32, hi: i32) -> Option<Vec<HardRow>> {
        let (cols, _) = self.grid_size();
        let mut rows = Vec::with_capacity((hi - lo + 1) as usize);
        let mut any_marker = false;
        let mut all_bordered = true;
        for r in lo..=hi {
            let text: Vec<char> = self.grid.line_text(r)?.chars().collect();
            let shape = HardRowShape::of(&text, cols);
            any_marker |= shape.marker;
            all_bordered &= shape.bordered;
            rows.push(HardRow {
                shape,
                wraps: self.grid.line_wraps(r),
                start: 0,
                end: 0,
            });
        }
        if !all_bordered && !any_marker {
            return None;
        }
        let margin = rows.iter().map(|r| r.shape.text_start).min().unwrap_or(0);
        for r in &mut rows {
            r.start = margin;
            r.end = r.shape.text_end.max(margin);
        }
        Some(rows)
    }

    /// Highlight rectangles (logical px) for the active *dragged* selection over a surface of
    /// `surf_w`×`surf_h`. Empty for no selection or a non-dragged click — so a plain click never
    /// leaves a stray one-cell highlight.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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

    /// Wrap `text` for insertion exactly as a paste would — bracketed-paste markers when
    /// the program in this pane asked for them, CR-normalized newlines otherwise.
    ///
    /// For text the app supplies itself rather than reading from the clipboard (the OS file
    /// drop). Sharing `prepare_paste` is the point: a TUI that distinguishes pasted content
    /// from typing must see a drop the same way it sees a paste, or a dropped path arrives
    /// as if it had been hand-typed one key at a time.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn prepare_insert(&self, text: &str) -> String {
        prepare_paste(text, self.grid.bracketed_paste())
    }

    /// Whether the OS clipboard holds an image (vs text). The controller uses this to decide
    /// whether a Ctrl+V with no clipboard text should forward a literal 0x16 to an in-pane TUI
    /// (Claude Code) that reads the clipboard image itself — see [`Clipboard::has_image`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn clipboard_has_image(&mut self) -> bool {
        self.clipboard.has_image()
    }

    /// Pin the viewport back to the live edge (display offset 0) so the cursor is visible at the
    /// end of whatever was just written — e.g. after a paste, regardless of scrollback position.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn scroll_to_bottom(&mut self) {
        self.grid.scroll_to_bottom();
    }

    /// Scroll the scrollback viewport by `delta_lines` (positive = up into history, negative =
    /// toward the live edge), clamped to the history bounds. Stamps the scrollbar's show timer.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn scroll_by(&mut self, delta_lines: i32) {
        self.grid.scroll_by(delta_lines);
        self.scroll_activity = Some(Instant::now());
    }

    /// Scroll the scrollback viewport by one page (`up` = into history, else toward the live
    /// edge). A page is the visible row count less one row of overlap, so successive pages keep a
    /// line of context. Drives Shift+PageUp / Shift+PageDown.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn scroll_page(&mut self, up: bool) {
        let (_, rows) = self.grid_size();
        let page = (rows as i32 - 1).max(1);
        self.scroll_by(if up { page } else { -page });
    }

    /// Jump the viewport to the very top of scrollback (Shift+Home). Stamps the scrollbar timer.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    /// * a **mouse-grabbing app** (DECSET 1000/1002/1003 — vim, htop) → a mouse-wheel report at
    ///   the pointer cell, so the app scrolls its own view;
    /// * the **alternate screen** with no mouse mode (less, man, a pager) → up/down arrow keys
    ///   (xterm's "alternate scroll"), so the pager scrolls a line at a time.
    ///
    /// Otherwise it scrolls our own scrollback viewport and returns `None`. Do not read the two
    /// forwarding cases as covering every full-screen program: an agent CLI that repaints in the
    /// primary screen without grabbing the mouse takes this last path, and then the wheel can only
    /// move history the emulator actually has. That in turn needs the pty to be the size of the
    /// grid, so the program's cursor reaches the last row and pushes lines into scrollback — see
    /// `flush_pty_resizes` in the app crate, where a silently dropped resize once left such panes
    /// unscrollable for their whole life.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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

    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn app_grabs_mouse(&self) -> bool {
        self.grid.mouse_mode()
    }

    /// Bytes to forward to the pty for a pointer event over a mouse-grabbing app, or `None` to
    /// suppress it. `kind`: 0 = button press, 1 = motion, 2 = release. `button`: 0 = left, 1 =
    /// middle, 2 = right, 3 = none (a bare move). Motion is only reported when the app asked for it
    /// (1002 = while a button is held, 1003 = always); press/release always report. Returns `None`
    /// if the app isn't grabbing the mouse.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn scroll_offset(&self) -> usize {
        self.grid.scroll_metrics().2
    }

    /// The vim-style scrollbar to draw right now, or `None` when there is no scrollback or the bar
    /// has fully faded. Returns `(thumb_y, thumb_h, opacity)` in logical px over a `surf_h`-tall
    /// pane: the thumb height is proportional to the visible fraction of the buffer, its position to
    /// how far down the buffer the viewport sits, and the opacity ramps from 1 down to 0 over the
    /// show-then-fade window since the last scroll gesture (so the bar is invisible while idle).
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn set_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// The indicator text to display right now, or `None` once it has expired (which also clears
    /// it). Call this every frame and push the result to the pane's `toast` property.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn search_open(&mut self) {
        self.search_shown = true;
    }

    /// Close the search box, dropping the query/matches and pinning the viewport back to the
    /// bottom (the live prompt).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn search_close(&mut self) {
        self.search_shown = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_index = None;
        self.grid.scroll_to_bottom();
    }

    /// Whether the search box is open.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn search_is_open(&self) -> bool {
        self.search_shown
    }

    /// The current query text.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Set the query (find-as-you-type): recompute matches across the grid + scrollback, pick the
    /// match nearest the current viewport, and scroll it into view.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn search_set_query(&mut self, query: &str) {
        self.search_query = query.to_string();
        self.search_recompute();
        self.search_reveal_active();
    }

    /// Step to the next (`forward`) / previous match, wrapping around, and reveal it.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn search_step(&mut self, forward: bool) {
        if self.search_matches.is_empty() {
            return;
        }
        let i = self.search_index.unwrap_or(0);
        self.search_index = Some(search::step(i, self.search_matches.len(), forward));
        self.search_reveal_active();
    }

    /// `(current_1_based, total)` for the match counter — `(0, 0)` when there are no matches.
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
    #[tracing::instrument(level = "debug", ret, skip(self))]
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
#[tracing::instrument(level = "debug", ret)]
pub fn cells_for_px(width_px: f32, height_px: f32, cell_w: u32, cell_h: u32) -> (usize, usize) {
    let cols = ((width_px as u32) / cell_w.max(1)).max(2) as usize;
    let rows = ((height_px as u32) / cell_h.max(1)).max(1) as usize;
    (cols, rows)
}

/// Alternate-scroll arrow keys for a wheel notch in the alternate screen (no mouse mode): one
/// Up/Down per scrollback line in `delta_lines` (positive = up). Encoded as application cursor keys
/// (`ESC O A/B`) when DECCKM is set, else normal (`ESC [ A/B`) — what xterm sends so pagers (less,
/// man) scroll on the wheel. This is the no-mouse-grab leg of the "can't scroll Claude" fix.
#[tracing::instrument(level = "debug", ret)]
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
///
/// The rules themselves live in [`avada_core::session::paste::prepare_paste`], because
/// the GUI is not the only thing that pastes into a pane: dictation delivers a transcript
/// through the session layer with no widget in the picture, and it was the one bulk-text
/// path that had its own idea of how to do it — a minute of speech reached the pane split
/// across tty reads, and all but the last read was dropped by the receiving TUI. One
/// definition, so there is nowhere for a second one to drift.
fn prepare_paste(text: &str, bracketed: bool) -> String {
    avada_core::session::paste::prepare_paste(text, bracketed)
}

/// One row of a multi-row TUI input, see [`TerminalPane::hard_input_rows`].
#[derive(Debug, Clone, PartialEq)]
struct HardRow {
    shape: HardRowShape,
    /// The terminal soft-wrapped this row into the next (WRAPLINE).
    wraps: bool,
    /// Block margin: the column the row's text starts at.
    start: usize,
    /// One past the row's last text cell (never below `start`).
    end: usize,
}

/// What a grid row looks like once its decoration is stripped: box border, prompt marker, and
/// where the text itself begins and ends.
#[derive(Debug, Clone, PartialEq)]
struct HardRowShape {
    bordered: bool,
    marker: bool,
    text_start: usize,
    text_end: usize,
    /// The column the text may run up to: the closing border, or the grid width.
    right_limit: usize,
}

impl HardRowShape {
    #[tracing::instrument(level = "debug", ret)]
    fn of(text: &[char], cols: usize) -> Self {
        const BORDERS: [char; 2] = ['│', '║'];
        const MARKERS: [char; 2] = ['>', '❯'];
        let mut i = text.iter().position(|c| *c != ' ').unwrap_or(text.len());
        let bordered = text.get(i).is_some_and(|c| BORDERS.contains(c));
        if bordered {
            i += 1;
            while text.get(i) == Some(&' ') {
                i += 1;
            }
        }
        let marker = text.get(i).is_some_and(|c| MARKERS.contains(c))
            && matches!(text.get(i + 1), Some(' ') | None);
        if marker {
            i += 1;
            while text.get(i) == Some(&' ') {
                i += 1;
            }
        }
        // Trailing decoration: spaces, then an optional closing border, then spaces again.
        let mut end = text.len();
        while end > i && text[end - 1] == ' ' {
            end -= 1;
        }
        let mut right_limit = cols;
        if bordered && end > i && BORDERS.contains(&text[end - 1]) {
            right_limit = end - 1;
            end -= 1;
            while end > i && text[end - 1] == ' ' {
                end -= 1;
            }
        }
        HardRowShape {
            bordered,
            marker,
            text_start: i.min(text.len()),
            text_end: end.max(i.min(text.len())),
            right_limit,
        }
    }
}

/// Character offset of `(line, col)` within the input made of `rows` (whose first row is grid
/// row `lo`), counting each earlier row's text plus one newline — unless that row is a wrap:
/// the terminal's own (WRAPLINE), or the box's, which shows as text running to the row's
/// right limit. `col` is clamped into the row's text so a click on the decoration or past the
/// end lands at the nearest end of that row.
#[tracing::instrument(level = "debug", ret)]
fn hard_row_offset(rows: &[HardRow], lo: i32, line: i32, col: usize) -> i32 {
    let idx = (line - lo) as usize;
    let mut acc: i32 = 0;
    for (k, r) in rows.iter().enumerate().take(idx) {
        // A terminal soft-wrap continues at column 0 of the next row.
        let start = if k > 0 && rows[k - 1].wraps {
            0
        } else {
            r.start
        };
        if r.wraps {
            acc += (r.shape.right_limit.max(start) - start) as i32;
        } else {
            acc += (r.end.max(start) - start) as i32;
            let box_wrapped = r.end + 1 >= r.shape.right_limit;
            if !box_wrapped {
                acc += 1;
            }
        }
    }
    let r = &rows[idx];
    let start = if idx > 0 && rows[idx - 1].wraps {
        0
    } else {
        r.start
    };
    acc + (col.clamp(start, r.end.max(start)) - start) as i32
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

    /// The reported failure, end to end: an agent announcing an artifact under
    /// `~/Library/Application Support`. The space is not a token boundary here, and only the
    /// disk can say so.
    #[test]
    fn a_path_with_a_space_in_it_links_as_one_path() {
        let dir = std::env::temp_dir().join(format!("hp_pane_space_{}", std::process::id()));
        let sub = dir.join("Application Support");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("lane-watch.html");
        std::fs::write(&file, b"x").unwrap();
        let abs = file.to_string_lossy().into_owned();

        let line = format!("Artifact({abs})");
        let cols = line.chars().count() + 8;
        let mut p = unit_pane(cols, 3);
        p.feed(&line);
        let (w, h) = (cols as f32, 3.0);

        // Hover just inside the '(' — over the half of the path the tokenizer kept.
        let hit = p
            .link_at(10.5, 0.5, w, h)
            .expect("the spaced path should be one link");
        assert_eq!(hit.abs_path.replace('\\', "/"), abs.replace('\\', "/"));
        // The underline covers the whole name: from just past the '(' to just before the ')'.
        assert_eq!(hit.x, 9.0);
        assert_eq!(hit.w, abs.chars().count() as f32);

        // ...and the second half of it is the same link, not a separate one.
        let tail = p
            .link_at((line.chars().count() - 3) as f32 + 0.5, 0.5, w, h)
            .expect("the far side of the space belongs to the same link");
        assert_eq!(tail.abs_path.replace('\\', "/"), abs.replace('\\', "/"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path the printing program broke across lines itself leaves a prefix that can be a real
    /// directory. The rest of it is still on screen, one row down, so go and get it — and when
    /// that fails, the prefix must never stand in for the whole.
    #[test]
    fn a_path_cut_off_by_a_hard_wrap_is_rejoined_or_refused_never_truncated() {
        let dir = std::env::temp_dir().join(format!("hp_pane_cut_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Volumes").join("Data")).unwrap();

        // Room to spare: `./Volumes` ends mid-row, so it is plainly whole and links.
        let mut wide = unit_pane(20, 3);
        wide.set_cwd(Some(dir.to_string_lossy().into_owned()));
        wide.feed("xxx ./Volumes");
        assert!(wide.link_at(6.5, 0.5, 20.0, 3.0).is_some());

        // Same text in a pane exactly its width, with the rest on a line of its own. The run
        // does not wrap, so the grid says the halves are unrelated -- but `/Data` is right there
        // and `./Volumes/Data` exists, so the link is the whole path, from either half.
        let mut tight = unit_pane(13, 3);
        tight.set_cwd(Some(dir.to_string_lossy().into_owned()));
        tight.feed("xxx ./Volumes\r\n/Data");
        let head = tight
            .link_at(6.5, 0.5, 13.0, 3.0)
            .expect("the severed head should link");
        // Windows reports the join with backslashes; the shape is what matters here.
        let head_slashed = head.abs_path.replace('\\', "/");
        assert!(head_slashed.ends_with("Volumes/Data"), "{}", head.abs_path);
        assert_eq!((head.x, head.w), (4.0, 9.0), "underline covers `./Volumes`");
        let tail = tight
            .link_at(2.5, 1.5, 13.0, 3.0)
            .expect("the continuation should link to the same file");
        assert_eq!(tail.abs_path, head.abs_path);
        assert_eq!((tail.x, tail.w), (0.0, 5.0), "underline covers `/Data`");

        // The `/System/Volumes` hazard, which is the whole reason this is delicate: when the
        // rejoin does not land on something real, the answer stays no link. Never the prefix.
        let mut wrong = unit_pane(13, 3);
        wrong.set_cwd(Some(dir.to_string_lossy().into_owned()));
        wrong.feed("xxx ./Volumes\r\n/Nope");
        assert!(wrong.link_at(6.5, 0.5, 13.0, 3.0).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The case from the field: Claude Code hard-wraps its own transcript and indents the
    /// continuation, so a long relative path arrives in three unrelated-looking pieces.
    #[test]
    fn a_path_hard_wrapped_across_three_rows_links_from_any_of_them() {
        let dir = std::env::temp_dir().join(format!("hp_pane_rejoin3_{}", std::process::id()));
        let rel = "scratchpad/reports/G2-misc-second-half.md";
        std::fs::create_dir_all(dir.join("scratchpad/reports")).unwrap();
        std::fs::write(dir.join(rel), b"hi").unwrap();

        let cols = 20;
        let mut p = unit_pane(cols, 5);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        // Row 0 fills the last column and breaks; rows 1 and 2 are indented continuations.
        p.feed(&format!(
            "Read {}\r\n  {}\r\n  {}",
            &rel[..15],
            &rel[15..33],
            &rel[33..]
        ));
        let (w, h) = (cols as f32, 5.0);
        // `dir` arrives in the platform's separators and `rel` in forward slashes; compare
        // both sides slashed so Windows judges the same thing macOS and Linux do.
        let want = dir.join(rel).to_string_lossy().replace('\\', "/");

        for (label, x, y, ux, uw) in [
            ("head", 7.5, 0.5, 5.0, 15.0),
            ("middle", 5.5, 1.5, 2.0, 18.0),
            ("tail", 4.5, 2.5, 2.0, 8.0),
        ] {
            let hit = p
                .link_at(x, y, w, h)
                .unwrap_or_else(|| panic!("{label} row should link"));
            assert_eq!(hit.abs_path.replace('\\', "/"), want, "{label}");
            assert!(hit.exists, "{label}");
            assert_eq!((hit.x, hit.w), (ux, uw), "{label} underline");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A row that opens with a TUI's own box-drawing is announcing a new line, not finishing the
    /// one above. Gluing the two together would invent a path nobody printed.
    #[test]
    fn a_continuation_row_that_starts_with_chrome_is_not_joined() {
        let dir = std::env::temp_dir().join(format!("hp_pane_chrome_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Volumes").join("Data")).unwrap();

        let mut p = unit_pane(13, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("xxx ./Volumes\r\n\u{23bf} /Data");
        assert!(p.link_at(6.5, 0.5, 13.0, 3.0).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stopping rule: once the committed directory prefix is known not to exist, no
    /// continuation can rescue it, so the walk gives up instead of reading the rest of the pane.
    #[test]
    fn a_cut_path_under_a_missing_directory_stops_looking() {
        let dir = std::env::temp_dir().join(format!("hp_pane_prune_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut p = unit_pane(13, 4);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        // `./ghostdir` does not exist, so `./ghostdir/x/y.txt` cannot either.
        p.feed("xxx ./ghostdir\r\n/x\r\n/y.txt");
        assert!(p.link_at(6.5, 0.5, 13.0, 4.0).is_none());

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
        match p.activate_link(2.5, 0.5, 20.0, 2.0, true) {
            Some(LinkAction::Copy(path)) => {
                assert!(path.replace('\\', "/").ends_with("a.txt"));
            }
            other => panic!("ctrl+click should copy, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plain click hands the path back for the left file tree rather than opening it: the
    /// human picked the file, not what should happen to it.
    #[test]
    fn plain_click_reveals_the_path_rather_than_opening_it() {
        let dir = std::env::temp_dir().join(format!("hp_pane_reveal_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("a.txt:12:4"); // cols 0..10
        match p.activate_link(2.5, 0.5, 20.0, 2.0, false) {
            Some(LinkAction::Reveal { path, line, col }) => {
                assert!(path.replace('\\', "/").ends_with("a.txt"));
                // The location rides along so whichever tool opens it lands on the right line.
                assert_eq!((line, col), (Some(12), Some(4)));
            }
            other => panic!("a plain click should reveal, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The asymmetry D14 asks for: a path that isn't on disk still copies, but has nothing to
    /// reveal — and it never underlines, so it is only ever reached deliberately.
    #[test]
    fn a_missing_path_copies_but_neither_underlines_nor_reveals() {
        let dir = std::env::temp_dir().join(format!("hp_pane_missing_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = unit_pane(20, 2);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("gone.txt"); // never created

        assert!(
            p.link_at(2.5, 0.5, 20.0, 2.0).is_none(),
            "no hover underline"
        );
        let hit = p
            .link_target_at(2.5, 0.5, 20.0, 2.0)
            .expect("a deliberate lookup still finds it");
        assert!(!hit.exists);
        assert!(hit.abs_path.replace('\\', "/").ends_with("gone.txt"));

        match p.activate_link(2.5, 0.5, 20.0, 2.0, true) {
            Some(LinkAction::Copy(path)) => {
                assert!(path.replace('\\', "/").ends_with("gone.txt"));
            }
            other => panic!("ctrl+click should copy a missing path, got {other:?}"),
        }
        assert!(
            p.activate_link(2.5, 0.5, 20.0, 2.0, false).is_none(),
            "there is nothing to reveal"
        );
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
        // click (would reveal) and Ctrl+click (would re-copy, clobbering the just-copied selection).
        assert!(p.activate_link(2.5, 0.5, 20.0, 2.0, false).is_none());
        assert!(p.activate_link(2.5, 0.5, 20.0, 2.0, true).is_none());
        // Once the selection is cleared, a click activates the link normally again.
        p.selection_clear();
        assert!(matches!(
            p.activate_link(2.5, 0.5, 20.0, 2.0, true),
            Some(LinkAction::Copy(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A throwaway repo with one commit, or `None` when this machine has no usable git — a
    /// skip, not a failure: this test is about the pane's plumbing, not about git being present.
    fn commit_fixture() -> Option<(std::path::PathBuf, String)> {
        let dir = std::env::temp_dir().join(format!(
            "hp_pane_commit_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                // Both dates pinned, so the commit — and therefore its object name — is the
                // same on every run. Without this the fixture mints a *random* hash, and
                // `extract_commit_candidates` deliberately declines an all-digit token as a
                // count or a timestamp rather than an object name. A 7-character
                // abbreviation is all digits (10/16)^7 of the time, so the hash test failed
                // roughly one run in twenty-seven, on nothing the product did.
                .env("GIT_AUTHOR_DATE", FIXTURE_DATE)
                .env("GIT_COMMITTER_DATE", FIXTURE_DATE)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return None;
        }
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("f.txt"), "one\n").ok()?;
        if !run(&["add", "-A"]) || !run(&["commit", "-q", "-m", "s"]) {
            return None;
        }
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()?;
        let short = String::from_utf8_lossy(&out.stdout).trim().to_string();
        // The property the pinned dates buy, stated where it is relied on: if git ever
        // hands back an all-digit abbreviation this fails the same way on every machine and
        // every run, instead of one run in twenty-seven on one of them.
        assert!(
            short.bytes().any(|b| b.is_ascii_lowercase()),
            "fixture hash {short} is all digits, which the extractor declines by design"
        );
        Some((dir, short))
    }

    /// The commit date the fixture pins, in both roles. Any fixed instant does; this one is
    /// recognisable in a failure message as a value nobody's clock produced.
    const FIXTURE_DATE: &str = "2020-01-01T00:00:00Z";

    #[test]
    fn a_hash_this_repository_knows_becomes_a_link_to_the_commit() {
        let Some((dir, short)) = commit_fixture() else {
            return;
        };
        let mut p = unit_pane(60, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        let line = format!("pushed as {short}, signed");
        p.feed(&line);
        let (w, h) = (60.0, 3.0); // 1px per cell
        let start = "pushed as ".len();

        let hit = p
            .link_at(start as f32 + 0.5, 0.5, w, h)
            .expect("hover over the hash should hit");
        assert!(hit.is_commit);
        assert!(!hit.is_url);
        // The full object name rides out, not the abbreviation the screen showed.
        assert_eq!(hit.abs_path.len(), 40);
        assert!(hit.abs_path.starts_with(&short));
        assert_eq!(hit.commit_cwd, dir.to_string_lossy());
        // Underline spans exactly the hash's columns at 1px/col.
        assert_eq!(hit.x, start as f32);
        assert_eq!(hit.w, short.len() as f32);

        // A plain click asks the app to show it, in the repository it was resolved against.
        match p.activate_link(start as f32 + 0.5, 0.5, w, h, false) {
            Some(LinkAction::ShowCommit { cwd, hash }) => {
                assert_eq!(cwd, dir.to_string_lossy());
                assert_eq!(hash, hit.abs_path);
            }
            other => panic!("expected ShowCommit, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_branch_this_repository_has_becomes_a_link_to_its_tip() {
        let Some((dir, short)) = commit_fixture() else {
            return;
        };
        let made = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["branch", "feature/x"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            return;
        }
        let mut p = unit_pane(60, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("Working tree is clean, feature/x is current, and/or not.");
        let (w, h) = (60.0, 3.0); // 1px per cell

        let start = "Working tree is clean, ".len();
        let hit = p
            .link_at(start as f32 + 0.5, 0.5, w, h)
            .expect("the branch name should light up");
        assert!(hit.is_commit);
        assert!(hit.abs_path.starts_with(&short), "{}", hit.abs_path);
        assert_eq!(
            hit.tip,
            format!("feature/x \u{b7} commit {}", &hit.abs_path[..12])
        );
        assert_eq!(hit.x, start as f32);
        assert_eq!(hit.w, "feature/x".len() as f32);
        // A click opens the tip commit in the panel, exactly as a hash would.
        match p.activate_link(start as f32 + 0.5, 0.5, w, h, false) {
            Some(LinkAction::ShowCommit { cwd, hash }) => {
                assert_eq!(cwd, dir.to_string_lossy());
                assert_eq!(hash, hit.abs_path);
            }
            other => panic!("expected ShowCommit, got {other:?}"),
        }

        // `and/or` has the shape but names nothing, and stays dark.
        let at = "Working tree is clean, feature/x is current, ".len() as f32 + 0.5;
        assert!(p.link_at(at, 0.5, w, h).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hex_word_naming_no_commit_stays_dark() {
        let Some((dir, _)) = commit_fixture() else {
            return;
        };
        let mut p = unit_pane(60, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("saw deadbeef today"); // hex-shaped, but this repo has no such object
        assert!(p.link_at(6.5, 0.5, 60.0, 3.0).is_none());
        // Ctrl/Cmd-click has nothing to copy either: an unresolvable hash is not a target.
        assert!(p.link_target_at(6.5, 0.5, 60.0, 3.0).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_name_from_elsewhere_in_the_repository_still_linkifies() {
        let Some((dir, _)) = commit_fixture() else {
            return;
        };
        // The file the agent is talking about is nowhere near the pane's cwd — which is the
        // ordinary case when a session narrates its own work.
        std::fs::create_dir_all(dir.join("deep/er")).unwrap();
        std::fs::write(dir.join("deep/er/b99_price.py"), "x = 1\n").unwrap();
        // And two files that share a name, which is a question with no answer.
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a/dup.rs"), "").unwrap();
        std::fs::write(dir.join("b/dup.rs"), "").unwrap();

        let mut p = unit_pane(60, 3);
        p.set_cwd(Some(dir.to_string_lossy().into_owned()));
        p.feed("in the b99_price.py shape, and dup.rs");
        let (w, h) = (60.0, 3.0); // 1px per cell

        let at = "in the ".len() as f32 + 0.5;
        let hit = p.link_at(at, 0.5, w, h).expect("the name should light up");
        assert!(hit.exists);
        assert!(!hit.is_commit);
        assert!(
            hit.abs_path.ends_with("deep/er/b99_price.py"),
            "{}",
            hit.abs_path
        );
        // Underline spans the name and nothing else.
        assert_eq!(hit.x, "in the ".len() as f32);
        assert_eq!(hit.w, "b99_price.py".len() as f32);

        let dup = "in the b99_price.py shape, and ".len() as f32 + 0.5;
        assert!(
            p.link_at(dup, 0.5, w, h).is_none(),
            "two files of that name: opening one of them would be a guess"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A project directory that is NOT a git repository, so the repository fallback has
    /// nothing to say and the screen is the only remaining witness.
    fn context_fixture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hp_pane_ctx_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj/docs/language/sql")).unwrap();
        std::fs::write(dir.join("proj/docs/language/sql/35-gaps.yaml"), "a: 1\n").unwrap();
        std::fs::write(dir.join("proj/README.md"), "# proj\n").unwrap();
        // A second project holding the same relative paths: the tie the project step
        // must refuse to break on its own.
        std::fs::create_dir_all(dir.join("proj2/docs/language/sql")).unwrap();
        std::fs::write(dir.join("proj2/docs/language/sql/35-gaps.yaml"), "a: 2\n").unwrap();
        std::fs::write(dir.join("proj2/README.md"), "# proj2\n").unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        dir
    }

    #[test]
    fn a_path_relative_to_a_directory_printed_above_it_linkifies() {
        let dir = context_fixture("above");
        let proj = dir.join("proj");
        let proj_s = proj.to_string_lossy().into_owned();

        // The pane is standing somewhere else entirely — an agent's shell `cd`s for one
        // command and prints paths relative to where it went, not where the pane is.
        let mut p = unit_pane(200, 6);
        p.set_cwd(Some(dir.join("elsewhere").to_string_lossy().into_owned()));
        p.feed(&format!(
            "cd {proj_s} && git status --porcelain\r\n M docs/language/sql/35-gaps.yaml\r\n"
        ));
        let (w, h) = (200.0, 6.0); // 1px per cell

        let at = " M ".len() as f32 + 0.5;
        let hit = p
            .link_at(at, 1.5, w, h)
            .expect("the relative path should resolve against the cd target above it");
        assert!(hit.exists);
        assert_eq!(
            std::path::Path::new(&hit.abs_path),
            proj.join("docs/language/sql/35-gaps.yaml")
        );
        assert_eq!(hit.x, " M ".len() as f32);
        assert_eq!(hit.w, "docs/language/sql/35-gaps.yaml".len() as f32);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_printed_below_the_path_does_not_count() {
        let dir = context_fixture("below");
        let proj_s = dir.join("proj").to_string_lossy().into_owned();

        let mut p = unit_pane(200, 6);
        p.set_cwd(Some(dir.join("elsewhere").to_string_lossy().into_owned()));
        // The only directory on screen comes AFTER the path: the shell had not gone there
        // yet when the path was printed, so it is no witness for it.
        p.feed(&format!(
            " M docs/language/sql/35-gaps.yaml\r\ncd {proj_s}\r\n"
        ));
        let (w, h) = (200.0, 6.0);

        let at = " M ".len() as f32 + 0.5;
        assert!(
            p.link_at(at, 0.5, w, h).is_none(),
            "a directory named later on screen must not vouch for an earlier path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_soft_wrapped_directory_above_is_read_whole() {
        let dir = context_fixture("wrap");
        let proj = dir.join("proj");
        let proj_s = proj.to_string_lossy().into_owned();

        // Narrow enough that the `cd` line soft-wraps mid-path: the base has to be stitched
        // back together from two visual rows before it can be tried.
        let cols = ("cd ".len() + proj_s.len()) * 2 / 3;
        let mut p = unit_pane(cols, 8);
        p.set_cwd(Some(dir.join("elsewhere").to_string_lossy().into_owned()));
        p.feed(&format!("cd {proj_s}\r\nM 35-gaps.yaml\r\n"));
        let (w, h) = (cols as f32, 8.0);
        // Row 0..2 hold the wrapped cd line; the relative path is on the row after.
        let row = ("cd ".len() + proj_s.len()) / cols + 1;

        // `35-gaps.yaml` lives under docs/language/sql — relative to the wrong base this
        // stays dark, so the ancestor walk must not be what makes this pass.
        std::fs::write(proj.join("35-gaps.yaml"), "b: 2\n").unwrap();
        let at = "M ".len() as f32 + 0.5;
        let hit = p
            .link_at(at, row as f32 + 0.5, w, h)
            .expect("the wrapped cd target should still be found");
        assert_eq!(
            std::path::Path::new(&hit.abs_path),
            proj.join("35-gaps.yaml")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_inside_the_project_names_the_project_too() {
        let dir = context_fixture("ancestor");
        let proj = dir.join("proj");
        let inner = proj.join("docs/language/sql/35-gaps.yaml");
        let inner_s = inner.to_string_lossy().into_owned();

        let mut p = unit_pane(240, 6);
        p.set_cwd(Some(dir.join("elsewhere").to_string_lossy().into_owned()));
        // No `cd` anywhere — only a full path to one file in the project, which is enough
        // to place a sibling named relative to the project root.
        p.feed(&format!(
            "wrote {inner_s}\r\nsee docs/language/sql/35-gaps.yaml\r\n"
        ));
        let (w, h) = (240.0, 6.0);

        let at = "see ".len() as f32 + 0.5;
        let hit = p
            .link_at(at, 1.5, w, h)
            .expect("an ancestor of a printed path should serve as the base");
        assert_eq!(std::path::Path::new(&hit.abs_path), inner);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- remembered project roots (the step between the repository and the screen) ----

    fn str_of(p: &std::path::Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn a_remembered_project_places_a_path_the_pane_cannot() {
        let dir = context_fixture("root_places");
        let proj = dir.join("proj");

        // Standing elsewhere, nothing printed above: only the app's memory of the project
        // can say where `docs/...` lives.
        let mut p = unit_pane(200, 6);
        p.set_cwd(Some(str_of(&dir.join("elsewhere"))));
        p.set_project_roots(vec![str_of(&proj)]);
        p.feed(" M docs/language/sql/35-gaps.yaml\r\n");
        let (w, h) = (200.0, 6.0);

        let at = " M ".len() as f32 + 0.5;
        let hit = p
            .link_at(at, 0.5, w, h)
            .expect("a remembered project root should place the relative path");
        assert!(hit.exists);
        assert_eq!(
            std::path::Path::new(&hit.abs_path),
            proj.join("docs/language/sql/35-gaps.yaml")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remembered_project_wins_over_a_directory_printed_above() {
        let dir = context_fixture("root_beats_screen");
        let proj = dir.join("proj");
        let proj2_s = str_of(&dir.join("proj2"));

        // Both hold the file. The screen names proj2; the app remembers proj. The stated
        // policy is "prefer the project root, then screen text" — pinned here.
        let mut p = unit_pane(200, 6);
        p.set_cwd(Some(str_of(&dir.join("elsewhere"))));
        p.set_project_roots(vec![str_of(&proj)]);
        p.feed(&format!(
            "cd {proj2_s}\r\n M docs/language/sql/35-gaps.yaml\r\n"
        ));
        let (w, h) = (200.0, 6.0);

        let at = " M ".len() as f32 + 0.5;
        let hit = p.link_at(at, 1.5, w, h).expect("the path should resolve");
        assert_eq!(
            std::path::Path::new(&hit.abs_path),
            proj.join("docs/language/sql/35-gaps.yaml"),
            "the remembered project must win over the directory printed above"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_projects_holding_the_path_leave_it_to_the_screen() {
        let dir = context_fixture("root_tie");
        let proj = dir.join("proj");
        let proj2 = dir.join("proj2");
        let proj2_s = str_of(&proj2);
        let roots = vec![str_of(&proj), str_of(&proj2)];

        // `README.md` is in both projects: the project step must decline, and the `cd`
        // printed above is what breaks the tie.
        let mut p = unit_pane(200, 6);
        p.set_cwd(Some(str_of(&dir.join("elsewhere"))));
        p.set_project_roots(roots.clone());
        p.feed(&format!("cd {proj2_s}\r\nsee README.md\r\n"));
        let (w, h) = (200.0, 6.0);

        let at = "see ".len() as f32 + 0.5;
        let hit = p
            .link_at(at, 1.5, w, h)
            .expect("the screen should break the tie");
        assert_eq!(std::path::Path::new(&hit.abs_path), proj2.join("README.md"));

        // With nothing on screen to break it, the tie stays dark: a wrong link is worse
        // than none.
        let mut q = unit_pane(200, 6);
        q.set_cwd(Some(str_of(&dir.join("elsewhere"))));
        q.set_project_roots(roots);
        q.feed("see README.md\r\n");
        assert!(
            q.link_at(at, 0.5, w, h).is_none(),
            "two projects holding the file must not light it on their own"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_added_later_lights_a_path_that_was_dark() {
        let dir = context_fixture("root_added_later");
        let proj = dir.join("proj");

        let mut p = unit_pane(200, 6);
        p.set_cwd(Some(str_of(&dir.join("elsewhere"))));
        p.feed(" M docs/language/sql/35-gaps.yaml\r\n");
        let (w, h) = (200.0, 6.0);
        let at = " M ".len() as f32 + 0.5;

        // No roots, nothing above: dark.
        assert!(p.link_at(at, 0.5, w, h).is_none());
        // The app learns about the project; the very next hover must light — which is
        // only true because the miss was never cached.
        p.set_project_roots(vec![str_of(&proj)]);
        let hit = p
            .link_at(at, 0.5, w, h)
            .expect("a project added after the miss should answer on the next hover");
        assert_eq!(
            std::path::Path::new(&hit.abs_path),
            proj.join("docs/language/sql/35-gaps.yaml")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn changing_the_project_roots_forgets_a_screen_answer() {
        let dir = context_fixture("root_forgets");
        let proj = dir.join("proj");
        let proj2 = dir.join("proj2");
        let proj2_s = str_of(&proj2);

        let mut p = unit_pane(200, 6);
        p.set_cwd(Some(str_of(&dir.join("elsewhere"))));
        p.feed(&format!(
            "cd {proj2_s}\r\n M docs/language/sql/35-gaps.yaml\r\n"
        ));
        let (w, h) = (200.0, 6.0);
        let at = " M ".len() as f32 + 0.5;

        // No roots yet: the screen answers, and the answer is cached in `verified`.
        let hit = p
            .link_at(at, 1.5, w, h)
            .expect("the cd above should answer");
        assert_eq!(
            std::path::Path::new(&hit.abs_path),
            proj2.join("docs/language/sql/35-gaps.yaml")
        );
        assert!(!p.verified.is_empty(), "the screen answer should be cached");

        // The roots change: the cache is dropped and the project wins on the next hover.
        p.set_project_roots(vec![str_of(&proj)]);
        let hit = p
            .link_at(at, 1.5, w, h)
            .expect("the project should now answer");
        assert_eq!(
            std::path::Path::new(&hit.abs_path),
            proj.join("docs/language/sql/35-gaps.yaml"),
            "a changed root set must forget the screen's answer"
        );

        // The same set again is not a change: the cache survives the reload.
        p.set_project_roots(vec![str_of(&proj)]);
        assert!(
            !p.verified.is_empty(),
            "an unchanged root set must keep the verify cache"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rooted_token_never_asks_the_projects() {
        let dir = context_fixture("root_rooted");
        let mut p = unit_pane(40, 3);
        p.set_project_roots(vec![str_of(&dir.join("proj"))]);
        // An absolute path is not relative to anything; the project step has no say.
        assert_eq!(p.relative_to_project("/usr/bin/env"), None);
        assert_eq!(p.relative_to_project("~/x"), None);
        // While a relative one inside the single root is exactly its business.
        let r = p
            .relative_to_project("docs/language/sql/35-gaps.yaml")
            .expect("the relative path should be placed");
        assert_eq!(
            std::path::Path::new(&r.abs_path),
            dir.join("proj/docs/language/sql/35-gaps.yaml")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_project_root_step_stops_at_its_stat_budget() {
        let dir = context_fixture("root_budget");
        let proj = dir.join("proj");
        // Sixty-four roots that hold nothing, then the one that does: past the budget, so
        // it must never be tried.
        let mut roots: Vec<String> = (0..TerminalPane::PROJECT_ROOT_STATS)
            .map(|i| str_of(&dir.join(format!("nowhere{i}"))))
            .collect();
        roots.push(str_of(&proj));
        let mut p = unit_pane(40, 3);
        p.set_project_roots(roots);
        assert_eq!(
            p.relative_to_project("docs/language/sql/35-gaps.yaml"),
            None,
            "the root past the stat budget must not be consulted"
        );
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
    fn a_url_wrapped_across_two_rows_stays_one_link() {
        // The pane is 20 cols; the URL is 29 chars, so the terminal soft-wraps it. Hit-testing
        // per visual row used to hand back only the half under the cursor -- a link reading
        // `http://127.0.0.1:51551/row18` opened `http://127.0.0.1:51` and hit nothing.
        let mut p = unit_pane(20, 4);
        let url = "https://a.com/abcdefghijklmno"; // 29 chars
        p.feed(url);
        let (w, h) = (20.0, 4.0); // 1px per cell

        // Hovering the first row: the whole URL, underlined across that row's 20 columns.
        let top = p.link_at(5.5, 0.5, w, h).expect("row 0 should hit");
        assert_eq!(top.abs_path, url);
        assert_eq!((top.x, top.w), (0.0, 20.0));

        // Hovering the continuation row: the same whole URL, underlined over its 9 columns.
        let cont = p.link_at(3.5, 1.5, w, h).expect("row 1 should hit");
        assert_eq!(cont.abs_path, url);
        assert_eq!((cont.x, cont.w), (0.0, 9.0));

        // Past the wrapped tail there is nothing to click.
        assert!(p.link_at(15.5, 1.5, w, h).is_none());
    }

    #[test]
    fn a_path_wrapped_across_two_rows_stays_one_link() {
        let dir = std::env::temp_dir().join(format!("hp_pane_wrap_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a-rather-long-file-name.txt");
        std::fs::write(&file, b"hi").unwrap();
        let abs = file.to_string_lossy().to_string();

        // Narrow enough that the absolute path cannot fit on one row.
        let cols = 24;
        let rows = abs.len().div_ceil(cols) + 2;
        let mut p = unit_pane(cols, rows);
        p.feed(&abs);
        let (w, h) = (cols as f32, rows as f32);

        // The last row of the wrap: still the full path, still verified on disk.
        let last = (abs.len() - 1) / cols;
        let hit = p
            .link_at(1.5, last as f32 + 0.5, w, h)
            .expect("the wrapped tail should hit");
        assert_eq!(hit.abs_path, abs);
        assert!(!hit.is_url);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_click_copies_the_url() {
        let mut p = unit_pane(30, 2);
        p.feed("https://a.com/x"); // cols 0..15
        match p.activate_link(5.5, 0.5, 30.0, 2.0, true) {
            Some(LinkAction::Copy(url)) => assert_eq!(url, "https://a.com/x"),
            other => panic!("ctrl+click on a URL should copy it, got {other:?}"),
        }
    }

    /// A plain click on a URL hands it back rather than launching it: which browser gets the
    /// link is a Avada preference this crate can't see, and a widget that opened the OS
    /// default itself would silently bypass "ask each time".
    #[test]
    fn plain_click_hands_the_url_back_unopened() {
        let mut p = unit_pane(30, 2);
        p.feed("https://a.com/x");
        match p.activate_link(5.5, 0.5, 30.0, 2.0, false) {
            Some(LinkAction::OpenUrl(url)) => assert_eq!(url, "https://a.com/x"),
            other => panic!("a clicked URL should come back as OpenUrl, got {other:?}"),
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
        assert!(p.activate_link(5.5, 0.5, 30.0, 2.0, false).is_none());
        assert!(p.activate_link(5.5, 0.5, 30.0, 2.0, true).is_none());
        p.selection_clear();
        assert!(matches!(
            p.activate_link(5.5, 0.5, 30.0, 2.0, true),
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

    /// A plain click: press and release on one cell, never crossing the drag threshold.
    fn click(p: &mut TerminalPane, col: usize, row: usize, w: f32, h: f32) {
        p.selection_begin(col as f32 * 10.0 + 5.0, row as f32 * 10.0 + 5.0, w, h);
        assert!(
            !p.selection_is_drag(),
            "a press with no motion is not a drag"
        );
    }

    #[test]
    fn a_click_left_of_the_caret_walks_it_back_with_left_arrows() {
        // "prompt" with the caret at col 6; click col 2 → 4 lefts.
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 2, 2, w, h);
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[D".repeat(4)));
    }

    #[test]
    fn a_click_right_of_the_caret_walks_it_forward_with_right_arrows() {
        // Past the end of the typed text: the editor clamps, so this is safe and still useful
        // (it lands the caret at the end of the input rather than in the blank cells).
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 9, 2, w, h);
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[C".repeat(3)));
    }

    #[test]
    fn a_click_on_the_caret_sends_nothing() {
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 6, 2, w, h);
        assert_eq!(p.click_move_cursor(), None);
    }

    #[test]
    fn a_click_on_another_line_is_not_an_edit_position() {
        // Row 0 holds "a" — output, not the input line. Moving the caret there would be
        // fabricating an edit the human never asked for.
        let (mut p, w, h) = prompt_pane();
        click(&mut p, 0, 0, w, h);
        assert_eq!(p.click_move_cursor(), None);
    }

    #[test]
    fn a_click_walks_through_a_soft_wrap() {
        // One input longer than the 20-col grid: row 0 wraps into row 1, so the whole thing is
        // a single editable line and the arrows walk straight across the wrap.
        let mut p = unit_pane(20, 5);
        p.feed(&"x".repeat(25)); // caret lands on row 1, col 5
        click(&mut p, 3, 0, 200.0, 50.0);
        // 20 - 3 cells to the end of row 0, plus 5 more back across row 1 = 22 lefts.
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[D".repeat(22)));
    }

    #[test]
    fn a_click_in_the_alternate_screen_moves_nothing() {
        // There the arrows are vim motions, not caret movement.
        let (mut p, w, h) = prompt_pane();
        p.feed("\x1b[?1049h");
        click(&mut p, 2, 2, w, h);
        assert_eq!(p.click_move_cursor(), None);
    }

    /// A Claude Code prompt box, 20 cols: the caret sits after "world" on the second text row.
    fn boxed_prompt_pane() -> (TerminalPane, f32, f32) {
        let mut p = unit_pane(20, 5);
        p.feed("╭──────────────────╮\r\n│ > hello          │\r\n│   world          │\r\n╰──────────────────╯");
        p.feed("\x1b[3;10H"); // row 2, col 9 (after "world")
        assert_eq!(p.grid.cursor_row(), Some(2));
        assert_eq!(p.grid.cursor_col(), 9);
        (p, 200.0, 50.0)
    }

    #[test]
    fn a_click_on_the_row_above_a_shift_enter_newline_walks_back_through_it() {
        // "hello⏎world" with the caret at the end (offset 11); the "e" of hello is offset 1.
        let (mut p, w, h) = boxed_prompt_pane();
        click(&mut p, 5, 1, w, h);
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[D".repeat(10)));
    }

    #[test]
    fn a_click_on_the_row_below_walks_forward_through_the_newline() {
        let (mut p, w, h) = boxed_prompt_pane();
        p.feed("\x1b[2;5H"); // caret on "h" of hello (offset 0)
        click(&mut p, 6, 2, w, h); // "r" of world = offset 8
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[C".repeat(8)));
    }

    #[test]
    fn a_click_on_the_box_decoration_lands_at_the_nearest_end_of_that_row() {
        // Past the end of "hello" (still on its row) → the end of hello, offset 5: 6 lefts.
        let (mut p, w, h) = boxed_prompt_pane();
        click(&mut p, 15, 1, w, h);
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[D".repeat(6)));
    }

    #[test]
    fn a_marked_multi_line_input_without_a_frame_still_walks() {
        let mut p = unit_pane(20, 5);
        p.feed("> first\r\n  second");
        p.feed("\x1b[2;9H"); // after "second"
        click(&mut p, 2, 0, 200.0, 50.0); // "f" of first = offset 0; caret = 12
        assert_eq!(p.click_move_cursor(), Some(b"\x1b[D".repeat(12)));
    }

    #[test]
    fn hard_row_shape_strips_border_and_marker() {
        let s = HardRowShape::of(&"│ > hello   │".chars().collect::<Vec<_>>(), 13);
        assert_eq!(
            s,
            HardRowShape {
                bordered: true,
                marker: true,
                text_start: 4,
                text_end: 9,
                right_limit: 12
            }
        );
        let s = HardRowShape::of(&"  plain".chars().collect::<Vec<_>>(), 7);
        assert!(!s.bordered && !s.marker);
        assert_eq!((s.text_start, s.text_end, s.right_limit), (2, 7, 7));
    }

    #[test]
    fn a_dragged_selection_is_not_a_click() {
        let (mut p, w, h) = prompt_pane();
        drag(&mut p, 1, 3, 2, w, h);
        assert_eq!(p.click_move_cursor(), None);
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

    /// The reported bug, at the pane's own seam: Claude Code's OAuth screen declares its login
    /// URL with OSC 8 while holding the mouse grab, and hard-positions each row with an explicit
    /// cursor move — so there is no soft-wrap for the text-sniffing path to follow and the label
    /// need not look like a URL at all. The declared layer has to carry it.
    #[test]
    fn a_declared_link_hits_even_when_the_label_is_not_a_url() {
        let mut p = unit_pane(40, 3);
        p.feed("go \u{1b}]8;;https://a.com/x\u{7}click here\u{1b}]8;;\u{7}");
        let (w, h) = (40.0, 3.0); // 1px per cell

        let hit = p
            .link_at(6.5, 0.5, w, h)
            .expect("a cell inside the declared span must hit");
        assert!(hit.is_osc8, "the hit must be marked as declared");
        assert!(hit.is_url);
        assert_eq!(hit.abs_path, "https://a.com/x");
        // Underline spans exactly the wrapped cells: "click here" is cols 3..13.
        assert_eq!((hit.x, hit.w), (3.0, 10.0));
        // Text outside the span is not the link, and never was URL-shaped.
        assert!(p.link_at(1.5, 0.5, w, h).is_none());
        assert!(p.link_at(20.5, 0.5, w, h).is_none());
    }

    /// A declared link may point somewhere its label does not name. That is the feature (elided
    /// and relabelled URLs) and also the risk, so the tooltip shows the DECLARED target: what
    /// opens is what the human was shown, not the prettier text under the cursor.
    #[test]
    fn a_relabelled_link_opens_and_advertises_its_declared_target() {
        let mut p = unit_pane(40, 3);
        p.feed("\u{1b}]8;;https://real.example/verify?c=9\u{7}https://a.com\u{1b}]8;;\u{7}");
        let hit = p.link_at(4.5, 0.5, 40.0, 3.0).expect("should hit");
        assert_eq!(hit.abs_path, "https://real.example/verify?c=9");
        assert_eq!(
            hit.tip, "https://real.example/verify?c=9",
            "the tooltip must name where the click actually goes"
        );
    }

    /// Only `http`/`https` are ours to open. A program is free to point OSC 8 at anything, and
    /// handing an arbitrary scheme to the OS opener is how a terminal turns a hostile `cat` into
    /// code execution. A refused scheme falls through to the ordinary sniffing path, which finds
    /// nothing URL-shaped here either.
    #[test]
    fn a_declared_link_to_a_non_web_scheme_is_refused() {
        for uri in [
            "file:///etc/passwd",
            "mailto:a@b.c",
            "javascript:alert(1)",
            "ssh://box/x",
        ] {
            let mut p = unit_pane(40, 3);
            p.feed(&format!("\u{1b}]8;;{uri}\u{7}open me\u{1b}]8;;\u{7}"));
            assert!(
                p.link_at(3.5, 0.5, 40.0, 3.0).is_none(),
                "{uri} must not become a clickable link"
            );
        }
    }

    /// A declared link is still a link while the running program holds the mouse grab — that is
    /// precisely the case the plain-click exception in `widget.slint` exists for. The pane's job
    /// is to hand back `OpenUrl` without demanding a modifier; which gesture reaches it is the
    /// widget's decision.
    #[test]
    fn a_declared_link_survives_the_mouse_grab_and_opens_on_a_plain_click() {
        let mut p = unit_pane(40, 3);
        // DECSET 1002+1006: button-event tracking with SGR encoding, exactly Claude Code's grab.
        p.feed("\u{1b}[?1002h\u{1b}[?1006h\u{1b}]8;;https://a.com/x\u{7}sign in\u{1b}]8;;\u{7}");
        assert!(p.app_grabs_mouse(), "the program must hold the mouse");
        match p.activate_link(3.5, 0.5, 40.0, 3.0, false) {
            Some(LinkAction::OpenUrl(url)) => assert_eq!(url, "https://a.com/x"),
            other => panic!("a plain click on a declared link should open it, got {other:?}"),
        }
    }

    /// OSC 52 policy, without an OS clipboard under it. The rejections are not "be careful with
    /// user data" hand-waving: anything that writes to a pty can emit OSC 52, so an uncapped
    /// store lets one `cat` of a hostile file push megabytes onto the clipboard.
    #[test]
    fn osc52_honours_the_last_store_within_the_cap() {
        assert_eq!(osc52_store_to_honor(vec![]), None);
        assert_eq!(osc52_store_to_honor(vec!["only".into()]).as_deref(), Some("only"));
        // Last write wins: a program emitting several stores in one chunk meant the final one.
        assert_eq!(
            osc52_store_to_honor(vec!["one".into(), "two".into()]).as_deref(),
            Some("two")
        );
        // An empty store is a clear, not a copy; we leave the clipboard alone.
        assert_eq!(osc52_store_to_honor(vec![String::new()]), None);
        // Exactly at the cap is fine; one byte over is not.
        assert!(osc52_store_to_honor(vec!["x".repeat(MAX_OSC52_BYTES)]).is_some());
        assert_eq!(osc52_store_to_honor(vec!["x".repeat(MAX_OSC52_BYTES + 1)]), None);
        // A rejected last store does NOT fall back to an earlier one — the program has moved on
        // from that text, and quietly copying something stale is worse than copying nothing.
        assert_eq!(
            osc52_store_to_honor(vec!["fine".into(), "x".repeat(MAX_OSC52_BYTES + 1)]),
            None
        );
    }

    /// End to end through `feed`: a program's own "copy this for me" reaches the pane's policy.
    /// The clipboard write itself is the OS's and is not asserted here.
    #[test]
    fn an_osc52_store_arriving_in_output_reaches_the_clipboard_policy() {
        let mut p = unit_pane(20, 2);
        p.feed("\u{1b}]52;c;aGVsbG8=\u{7}"); // base64("hello")
        assert!(
            p.grid.take_clipboard_writes().is_empty(),
            "feed must have drained the store rather than leaving it queued"
        );
    }
}
