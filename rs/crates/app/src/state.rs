//! The central app state — Wave-2 **Seam #1**.
//!
//! `State` owns every tab (workspace group), each with its own panes, layout,
//! split sizes, main-fraction, focus and zoom. All mutation flows through the
//! methods here; each one leaves the data consistent and flips `dirty` so the
//! next pump cycle *resyncs* the Slint models (see [`crate::paneview::resync`]).
//! That **mutate → set-dirty → resync** contract is the single seam Wave-2
//! features (palette, keybindings, prefs) extend: they only ever call these
//! methods (usually via a [`crate::command::Command`]) and never touch the UI
//! models directly.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use avada_core::ai::service::AiProjectRef;
use avada_core::layout::navigate::{neighbor_index, Direction};
use avada_core::layout::presets::{
    compute_dividers, compute_tiles, effective_layout, DividerKind, Layout,
};
use avada_core::layout::sizes::{clamp_fraction, equal_sizes, insert_size, remove_size, resize_at};
use avada_core::persistence::{paths, projects};
use avada_core::session_manager::{AgentLiveness, SessionManager, SpawnOptions};
use avada_core::tools::{PaneKind, ToolSessionMark};
use avada_core::workspace::io::{read_workspace, windows_of, write_workspace};
use avada_core::workspace::model::{GroupSpec, PaneSpec, WorkspaceFile};
use avada_core::workspace::sets;
use avada_terminal_widget::{Font, RenderOpts, SoftwareRenderer, TerminalPane};

use slint::{Color, Image, SharedString};

use crate::command::Command;
use crate::glow::Glow;
use crate::leftpanel::{RailGesture, RailRequest};
use crate::palette::{self, Entry};
use crate::prefs::{self, Settings};
use crate::sidebar::{self, Project};
use crate::theme;

/// Which Wave-2 overlay panel (if any) is mounted in the overlay slot (**Seam #3**).
/// Exactly one is shown at a time; opening one replaces the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Palette,
    Prefs,
    /// The "New pane" options dialog (Shift+＋ / the menus' "New pane…"). Configures a pane
    /// before it spawns; submitting routes through [`State::add_pane_opts`].
    NewPane,
    /// The "Add project" dialog (the ＋ on the sidebar's PROJECTS header): type a directory
    /// path to add it as a project explicitly; submitting routes through
    /// [`State::submit_add_project`].
    AddProject,
    /// The "New goal" dialog (command palette → "New goal…"): pick a project + type a goal;
    /// submitting routes through [`State::submit_new_goal`].
    NewGoal,
    /// The "Open link with…" chooser — Preferences → Browser → "Ask each time". Holds the
    /// URL in [`State::ask_url`] until a human picks one of [`State::ask_browsers`];
    /// picking routes through [`State::pick_browser`]. Dismissing drops the URL on the
    /// floor, which is the point: "ask" means the human may also answer "not this one".
    AskBrowser,
    /// The close-confirmation card. Holds the close the user asked for in
    /// [`State::pending_close`] until they answer; answering routes through
    /// [`State::confirm_close_go`] / the ordinary dismiss path (which cancels).
    ConfirmClose,
}

// Pane/session uid minting moved to the backend: `SessionManager::fresh_uid` picks the
// scheme (in-process `pane-N` from a process-global counter — same cross-window uniqueness
// the old `state.rs` counter guaranteed — vs daemon UUID for cross-RUN uniqueness, which
// re-attach needs). See `session_manager::fresh_uid` and the plan's "uid stability".
//
// EXCEPT for non-pty view panes (file browser / viewer / markdown — `PaneKind::is_pty()`
// false), which mint `view-N` here instead. A view pane has no pty, so handing it a
// backend uid would put a phantom in front of `pane_load`, `has`, and the multi-window
// `claim_session`/`release_session` arbitration — a uid the daemon would be asked about
// forever and could never answer for. The `view-` prefix cannot alias either backend
// scheme (`pane-N` / `pane-<uuid>`), which is what makes the gate a total function rather
// than a convention. See the plan's D3.

/// Process-global counter for `view-N`, matching `next_inproc_uid`'s rationale: two windows
/// sharing one process must never mint the same view uid.
static NEXT_VIEW_UID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `view-0`, `view-1`, … — pane identity for a pane with no session behind it.
#[tracing::instrument(level = "debug", ret)]
fn fresh_view_uid() -> String {
    format!(
        "view-{}",
        NEXT_VIEW_UID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// `mgr.kill(uid)` for a pane that actually has a session. A view pane's uid was never
/// registered with the backend, so killing it would ask the daemon about a session it has
/// never heard of. Every close/evict path routes through here so the gate is in ONE place.
#[tracing::instrument(level = "debug", skip_all)]
fn kill_session_of(mgr: &SessionManager, uid: &str, kind: &PaneKind) {
    if kind.is_pty() {
        tracing::info!(uid, "killing pane session");
        mgr.kill(uid);
    }
}

/// What [`State::pane_exited`] did about a session that ended on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneExit {
    /// Whether the window still has panes. `false` means the workspace is empty and the
    /// caller quits — the pre-existing contract, unchanged.
    pub alive: bool,
    /// `(old uid, new uid)` when the pane fell back to a shell instead of closing. The pane
    /// is the same pane, but its session is a new one, so callers holding uid-keyed state
    /// (the control plane's stable pane-id alias) must move it across.
    pub rebound: Option<(String, String)>,
}

/// The one line a pane prints when its program exits and the shell takes over.
///
/// It has to answer the two questions the old behaviour destroyed the pane before it could
/// answer: what just ended, and how to get it back. Dim so it reads as chrome rather than
/// as output the program produced, and CRLF-terminated because this is written into a
/// terminal grid, not to a file.
#[tracing::instrument(level = "debug", ret)]
fn exit_banner(program: &str, code: i32) -> String {
    let how = if code == 0 {
        format!("{program} exited")
    } else {
        format!("{program} exited (code {code})")
    };
    format!("\r\n\x1b[2m{how} — this pane is a shell now; run `{program}` to start it again.\x1b[0m\r\n")
}

/// A session detached from its window for re-hosting in another (Wave-1 multi-window
/// plumbing). Carries only the session `uid` + chrome; the PTY stays alive centrally in
/// the [`SessionManager`], so re-hosting is a replay-into-a-fresh-grid, never a restart.
#[derive(Debug, Clone)]
pub struct DetachedPane {
    pub uid: String,
    pub title: SharedString,
    pub subtitle: Option<SharedString>,
    pub pinned_accent: Option<Color>,
    /// Per-pane frame/dot overrides (carried across a re-host so a tinted project pane
    /// stays tinted, and a clean pane stays clean). `None` = inherit the global pref.
    pub show_frame: Option<bool>,
    pub show_dot: Option<bool>,
    /// The pane's per-pane zoom (terminal font px), carried across a re-host so a zoomed pane
    /// keeps its size when torn off / moved.
    pub font_px: f32,
    /// The pane's original spawn spec (command/args/shell), carried across a re-host so a
    /// torn-off / moved pane still records its program in a later relaunch snapshot.
    pub spawn_command: Option<String>,
    pub spawn_args: Option<Vec<String>>,
    pub spawn_shell: Option<String>,
    /// What the pane *is* — a plain terminal, a specific CLI tool's pane, or one of the
    /// non-pty views. Carried across a re-host so a Claude pane torn off into another
    /// window arrives as a Claude pane rather than reverting to a bare terminal.
    pub kind: PaneKind,
    /// The conversation this pane was resumed into, carried across a re-host for the same
    /// reason as `kind`: a pane moved to another window has to relaunch into the same chat.
    pub tool_session: Option<ToolSessionMark>,
    /// The pane's working directory — and, for the non-pty views, its *target*: the file the
    /// viewer/markdown pane renders, the directory the browser lists (see [`crate::viewpane`]).
    /// That makes it content, not decoration: a re-host that dropped it turned a README
    /// preview into "No path set for this pane". For a terminal it is what the left panel's
    /// file tree follows, so dropping it there un-anchored the tree instead.
    pub cwd: Option<String>,
}

/// A whole tab detached for re-hosting (the tab menu's "Move to New Window") or parked on the
/// recently-closed history (for "Reopen Closed"). Like [`DetachedPane`] the sessions stay alive
/// centrally, so re-hosting is a replay-into-fresh-grids, never a restart.
#[derive(Debug, Clone)]
pub struct DetachedTab {
    pub title: SharedString,
    pub layout: Layout,
    pub sizes: Vec<f64>,
    pub main_fraction: f64,
    /// The focused pane index, carried so a reopened/moved tab restores focus instead of
    /// snapping back to pane 0.
    pub focused: usize,
    /// The zoomed (maximised-in-tab) pane, carried so a reopened/moved tab keeps its
    /// maximized pane instead of dropping the zoom.
    pub zoomed: Option<usize>,
    pub panes: Vec<DetachedPane>,
}

/// What one entry of the recently-closed history holds — a closed *pane* or a closed *tab*.
///
/// Both keep their sessions ALIVE centrally: a close parks through the detach machinery, not
/// the kill path, which is what makes reopening a re-dock of the running shell rather than a
/// respawn of a fresh one. That aliveness is also the obligation: an entry evicted by the cap,
/// and every entry still parked when the window closes, must have its sessions killed — see
/// [`State::push_closed`] and [`State::session_uids`].
#[derive(Debug, Clone)]
pub enum ClosedWhat {
    Pane(Box<DetachedPane>),
    Tab(Box<DetachedTab>),
}

/// One row of the recently-closed history: what was closed, plus what the list needs to draw
/// it and address it.
#[derive(Debug, Clone)]
pub struct ClosedItem {
    pub what: ClosedWhat,
    /// Identity for a row click. Index-free for the same reason the reminder rows key on
    /// `uid`: the list can shift between the frame the human clicked and the click arriving
    /// (another close pushes, the cap evicts), and an index would then restore the wrong row.
    pub id: i32,
    /// When it was closed, in epoch ms — the list's relative-age column.
    pub at_ms: u64,
}

impl ClosedItem {
    /// The live panes this entry is keeping alive (one for a pane, all of them for a tab).
    /// Borrowed as a slice so the kill loops read the same for both variants.
    #[tracing::instrument(level = "debug", ret)]
    pub fn panes(&self) -> &[DetachedPane] {
        match &self.what {
            ClosedWhat::Pane(p) => std::slice::from_ref(&**p),
            ClosedWhat::Tab(t) => &t.panes,
        }
    }

    /// The history row's title.
    #[tracing::instrument(level = "debug", ret)]
    pub fn label(&self) -> SharedString {
        match &self.what {
            ClosedWhat::Pane(p) => p.title.clone(),
            ClosedWhat::Tab(t) => t.title.clone(),
        }
    }

    /// Whether this entry is a whole tab (drives the row's icon + "N panes" line).
    #[tracing::instrument(level = "debug", ret)]
    pub fn is_tab(&self) -> bool {
        matches!(self.what, ClosedWhat::Tab(_))
    }

    /// The row's tint — the first pane's pinned accent, if it has one.
    #[tracing::instrument(level = "debug", ret)]
    pub fn accent(&self) -> Option<Color> {
        self.panes().first().and_then(|p| p.pinned_accent)
    }
}

/// How many closed panes/tabs stay reopenable. Everything past this is evicted *and killed*,
/// so the number is a live-session budget, not just a list length.
pub const CLOSED_STACK_CAP: usize = 20;

/// The rail entry id a file-tree module is expected to register. The app used to *be* that
/// tree; it now only knows the entry's NAME, so anything that reveals a path can find the
/// module currently providing one without either side naming the other.
pub const FILES_ENTRY: &str = "files";

/// The rail entry id a git module is expected to register. Named for the same reason
/// [`FILES_ENTRY`] is: the app used to draw the working tree itself, and now only knows
/// what to ask for. Anything that wants to show a commit finds whoever answers to `git`.
pub const GIT_ENTRY: &str = "git";

/// Which revision of which repository a module's row was listed from, as the row's own
/// `data.git` object reported it.
///
/// The host cannot work this out for itself: a row is a path and a label, and whether that
/// path is being shown as "changed since HEAD" or as "touched by abc1234" is a fact only
/// the module projecting it knows. It says so, and the host's row menu then offers the diff
/// the row actually means. Absent or malformed is simply "no diff verb" — a module that
/// says nothing gets the ordinary file menu, which is the right outcome for every module
/// that is not about git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOrigin {
    /// The repository the row belongs to.
    pub root: PathBuf,
    /// The commit, or `None` for the working tree (diffed against HEAD).
    pub rev: Option<String>,
    /// How that commit is spelled in the menu ("abc1234"). Empty for the working tree.
    pub short: String,
}

impl GitOrigin {
    /// Read one from a row's `data.git`, or `None` when there isn't one.
    ///
    /// A missing `root` is the only hard requirement: without it there is no repository to
    /// run anything in. Everything else degrades to the working tree, because a module that
    /// sent half an object should get the conservative verb rather than a menu row that
    /// spawns `git show` on an empty revision.
    pub fn from_row(data: Option<&serde_json::Value>) -> Option<Self> {
        let v = data?;
        let root = v.get("root")?.as_str().filter(|r| !r.is_empty())?;
        let rev = v
            .get("rev")
            .and_then(|r| r.as_str())
            .filter(|r| !r.is_empty())
            .map(|r| r.to_string());
        let short = v
            .get("short")
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_string();
        // A commit with no short form still needs a name in the menu; git's own default is
        // the first seven characters, so use that rather than printing the whole hash.
        let short = match (&rev, short.is_empty()) {
            (Some(r), true) => r.chars().take(7).collect(),
            _ => short,
        };
        Some(Self {
            root: PathBuf::from(root),
            rev,
            short,
        })
    }
}

/// How many consecutive pump ticks may attempt to hand the keyboard to a newly selected pane
/// before giving up (see [`State::sync_pane_keyboard_focus`]). The hand-off normally lands on
/// the first or second: the first can arrive before Slint has instantiated the pane's element,
/// the second is after the repeater has caught up. The rest is headroom — and a hard stop, so
/// a pane that can never take focus doesn't re-dirty the state forever.
///
/// Sized in ticks (~8 ms each), so this is a little over half a second. Eight was not enough:
/// a tab switch that changes the pane COUNT re-creates the whole repeater, and on a cold start
/// the whole budget was spent before the pane's element existed — the hand-off then never
/// happened at all, which is the "I can't type in it until I click in it" report. The loop
/// stops the moment the widget confirms, so the extra headroom only ever runs while the
/// hand-off is genuinely still failing.
const KBD_FOCUS_TRIES: u8 = 64;

/// How long ago a history entry was closed, for its row's second line. Coarse on purpose:
/// the first minute is one bucket ("just now") so a fresh close doesn't re-push the whole
/// list into Slint once a second (the pump's signature includes this string).
#[tracing::instrument(level = "debug", ret)]
pub fn rel_age(now_ms: u64, at_ms: u64) -> String {
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Process-global counter for [`ClosedItem::id`] — same rationale as `NEXT_VIEW_UID`: two
/// windows in one process must never mint the same row id.
static NEXT_CLOSED_ID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

#[tracing::instrument(level = "debug", ret)]
fn fresh_closed_id() -> i32 {
    NEXT_CLOSED_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Which pane/tab a [`PendingClose`] is about, named by a **session uid** rather than an
/// index. The confirm card is modal but the world underneath is not — a background shell can
/// exit and renumber the tabs while the question is on screen — so the answer is re-resolved
/// against the uid at confirm time and simply does nothing if that pane is already gone. A
/// tab is named by its first pane's uid, which is as stable an identity as a tab has.
#[derive(Debug, Clone, PartialEq)]
pub enum CloseTarget {
    Pane(String),
    Tab(String),
}

/// A close the human asked for, held while the confirm card asks whether they meant it.
#[derive(Debug, Clone)]
pub struct PendingClose {
    pub target: CloseTarget,
    /// What is about to close, in the card's headline ("claude", "Tab 2 — 3 panes").
    pub title: SharedString,
    /// True when this close ENDS THE WINDOW: the last pane of the last tab has nowhere to be
    /// parked (the history dies with the window), so it is a real kill and the card says so.
    /// This is also why such a close is confirmed even with the confirm preference off.
    pub final_close: bool,
}

/// A single preferences edit, carried by `Command::ApplySetting`. Keeps the `Command`
/// enum flat (one variant) while still typing each field of [`Settings`].
#[derive(Debug, Clone)]
pub enum Setting {
    /// Select the terminal font by its file path (see `prefs::available_families`).
    FontFamily(String),
    /// Select the frame palette by index into `theme::FRAME_PALETTES` (remaps pane accents).
    FramePalette(usize),
    /// Select the terminal colour theme by index into `theme::TERMINAL_THEMES`.
    TerminalTheme(usize),
    /// Select the app *shell* colour palette by index into `theme::UI_PALETTES`. Unlike the
    /// two above it changes nothing inside a pane — it repaints the window around them.
    UiPalette(usize),
    /// Set the default shell token for new panes ("" = system default).
    DefaultShell(String),
    /// Nudge the base font size by ±N points.
    FontDelta(i32),
    ShowFrame(bool),
    ShowDot(bool),
    /// Toggle whether terminal paths are clickable.
    ClickablePaths(bool),
    /// Toggle copy-on-select (finishing a drag copies it; right-click then always pastes).
    CopyOnSelect(bool),
    /// Set the editor-command template used to open clicked paths ("" = auto).
    EditorCommand(String),
    /// Toggle the idle-glow (AI-pane quiescence glow).
    IdleAlert(bool),
    /// Select the glow style by index into `glow::IdleEffect::OPTIONS`.
    IdleEffect(usize),
    /// Nudge the idle threshold (seconds) by ±N, clamped to the supported range.
    IdleSeconds(i32),
    /// Toggle the startup auto-update check (Task 8).
    AutoUpdate(bool),
    /// Toggle keep-terminals-running-in-the-background on quit (session-daemon M3).
    KeepAlive(bool),
    /// Star/unstar a registry tool id (Preferences → Tools). Order is the user's, so this
    /// toggles in place rather than re-sorting; an id this build doesn't know is still
    /// storable, because a favourite must survive a downgrade.
    ToggleFavoriteTool(String),
    /// Override where a tool's binary lives: `(registry id, path)`. An EMPTY path clears the
    /// override and returns the tool to `PATH` detection — that is the only way to undo one,
    /// so it is deliberately not a separate variant.
    ToolPath(String, String),
    /// Which browser opens a URL: one of `prefs::BROWSER_MODE_{DEFAULT,APP,ASK}`.
    BrowserMode(String),
    /// The `core::open::BrowserApp::id` used when the mode is `app`.
    BrowserApp(String),
}

/// A goal awaiting robust delivery into its orchestrator pane's Claude TUI. Rather than the
/// fragile first-output `startup` write (which fires mid-boot, when Claude's bracketed-paste
/// input box swallows the CR), a goal is queued here and drained by the app tick
/// (`deliver_pending_goals`) once the pane's Claude is ready — signalled by its SessionStart
/// marker (aged a few seconds), or after a fallback timeout when no hook/marker exists. Delivery
/// reuses the proven resume-queue cadence (type text, gap, CR, insurance CR).
#[derive(Debug, Clone)]
pub struct PendingGoal {
    /// The orchestrator pane's session uid (also its `AVADA_PANE_ID` / marker filename).
    pub uid: String,
    /// The goal prompt (intent + image path references + model hint), sans trailing CR.
    pub text: String,
    /// Attached image files, re-pasted via the OS clipboard on delivery (best-effort; the paths
    /// are already in `text`, which is the guaranteed delivery).
    pub images: Vec<std::path::PathBuf>,
    /// When the goal was queued — drives the no-marker fallback timeout.
    pub queued_at: std::time::Instant,
}

/// One remembered "New goal" submission — the ↓-history of the New-goal box. Persisted (most
/// recent first, capped) in `goal_history.json` next to `projects.json`, so past goals survive
/// relaunch and can be recalled/edited/resubmitted from the free-text field.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalHistoryEntry {
    /// The goal intent as typed (no image/model suffixes).
    pub text: String,
    /// Canonical path of the project it was submitted against.
    pub project: String,
}

/// Fields of the New-goal box, in ←/→ navigation order (`State::goal_field` indexes this):
/// 0 = the free-text goal, then the project + the three model-tier chips.
pub const GOAL_FIELDS: usize = 5;

/// Most-recent-first cap on the persisted goal history.
const GOAL_HISTORY_CAP: usize = 50;

/// Where the goal history persists — sibling of `projects.json` in the app data dir.
#[tracing::instrument(level = "debug", ret)]
fn goal_history_path() -> std::path::PathBuf {
    avada_core::persistence::paths::data_dir().join("goal_history.json")
}

/// Where the New-goal dialog's last-used model tiers persist — sibling of
/// `goal_history.json`. Remembering the picks means re-opening the box doesn't reset the
/// orchestrator/spec/impl tiers to the built-in defaults every time.
#[tracing::instrument(level = "debug", ret)]
fn goal_defaults_path() -> std::path::PathBuf {
    avada_core::persistence::paths::data_dir().join("goal_defaults.json")
}

/// Built-in model tiers when nothing is remembered yet: orchestrator/spec = opus (idx 0),
/// implementation = sonnet (idx 1). Keep in sync with [`State::open_new_goal`].
const GOAL_MODEL_SEL_DEFAULT: [usize; 3] = [0, 0, 1];

/// Clamp each remembered tier index into [`crate::command::GOAL_MODELS`] so a stale file
/// (e.g. written before the model list shrank) can never point past the end.
#[tracing::instrument(level = "debug", ret)]
fn clamp_goal_model_sel(sel: [usize; 3]) -> [usize; 3] {
    let max = crate::command::GOAL_MODELS.len() - 1;
    [sel[0].min(max), sel[1].min(max), sel[2].min(max)]
}

/// Load the remembered [orchestrator, spec, impl] model-tier indices for the New-goal box
/// (missing/corrupt file → built-in defaults).
#[tracing::instrument(level = "debug", ret)]
fn load_goal_model_defaults() -> [usize; 3] {
    std::fs::read_to_string(goal_defaults_path())
        .ok()
        .and_then(|s| serde_json::from_str::<[usize; 3]>(&s).ok())
        .map(clamp_goal_model_sel)
        .unwrap_or(GOAL_MODEL_SEL_DEFAULT)
}

/// Persist the New-goal box's model tiers (best-effort — a write failure only loses the
/// remembered defaults, never load-bearing).
#[tracing::instrument(level = "debug", ret)]
fn save_goal_model_defaults(sel: &[usize; 3]) {
    if let Ok(json) = serde_json::to_string(sel) {
        let path = goal_defaults_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(path, json);
        }
    }
}

/// Load the persisted goal history (missing/corrupt file → empty; history is a convenience,
/// never load-bearing).
#[tracing::instrument(level = "debug", ret)]
fn load_goal_history() -> Vec<GoalHistoryEntry> {
    std::fs::read_to_string(goal_history_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// A goal intent squeezed into a pane subtitle: first line only, elided past 60 chars (the
/// header has one secondary line; the full task is in the pane's conversation).
#[tracing::instrument(level = "debug", ret)]
fn goal_subtitle(intent: &str) -> String {
    let line = intent.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 60 {
        line.to_string()
    } else {
        let cut: String = line.chars().take(59).collect();
        format!("{cut}…")
    }
}

/// Index into `projects` of the project containing `cwd` — the project whose non-empty root is
/// `cwd` itself or a `<root>/…` prefix of it, longest (most specific) winning when roots nest.
/// `None` when `cwd` sits outside every project. Pure so the New-goal default is unit-testable.
#[tracing::instrument(level = "debug", ret)]
fn goal_project_for_cwd(projects: &[sidebar::Project], cwd: &str) -> Option<usize> {
    projects
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            !p.path.is_empty() && (cwd == p.path || cwd.starts_with(&format!("{}/", p.path)))
        })
        .max_by_key(|(_, p)| p.path.len())
        .map(|(i, _)| i)
}

/// Persist the goal history (best-effort — a write failure only loses recall convenience).
#[tracing::instrument(level = "debug", ret)]
fn save_goal_history(history: &[GoalHistoryEntry]) {
    if let Ok(json) = serde_json::to_string_pretty(history) {
        let path = goal_history_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, json);
    }
}

/// Build the contents of `goals-mcp.json`: registers the avada MCP server so a spawned
/// `claude --mcp-config <this file>` sees `mcp__avada__*` tools. Needed because the goals
/// system rotates `CLAUDE_CONFIG_DIR` across per-account dirs (see `claude_accounts`) whose
/// `.claude.json` has no user-scoped MCP registrations — the avada server only lives in the
/// default `~/.claude.json`, which `claude` ignores once `CLAUDE_CONFIG_DIR` is set.
#[tracing::instrument(level = "debug", ret)]
fn goals_mcp_config_json(control_json_path: &str) -> String {
    serde_json::json!({
        "mcpServers": {
            "avada": {
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "avada-mcp"],
                "env": {
                    "AVADA_ALLOW_INPUT": "1",
                    "AVADA_CONTROL_FILE": control_json_path,
                }
            }
        }
    })
    .to_string()
}

/// Write `goals-mcp.json` into the state dir, returning its path on success. Best-effort — a
/// write failure must not block a goal spawn; the caller just omits `--mcp-config`.
#[tracing::instrument(level = "debug", ret)]
fn write_goals_mcp_config() -> Option<std::path::PathBuf> {
    let control_path = avada_core::persistence::paths::control_json();
    let json = goals_mcp_config_json(&control_path.to_string_lossy());
    let path = avada_core::persistence::paths::state_dir().join("goals-mcp.json");
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(
                "failed to create state dir for goals-mcp.json: {e}; spawning without --mcp-config"
            );
            return None;
        }
    }
    match std::fs::write(&path, json) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("failed to write goals-mcp.json: {e}; spawning without --mcp-config");
            None
        }
    }
}

/// Build the contents of `goals-settings.json`: a minimal Claude settings blob carrying just
/// the user's `statusLine`. Spawned agents rotate `CLAUDE_CONFIG_DIR` across per-account dirs
/// whose `settings.json` has no `statusLine` (it lives only in the default `~/.claude`), so
/// without this they show Claude's built-in default statusline instead of the user's.
#[tracing::instrument(level = "debug", ret)]
fn goals_settings_json(status_line: &serde_json::Value) -> String {
    serde_json::json!({ "statusLine": status_line }).to_string()
}

/// Write `goals-settings.json` for a spawned `claude --settings <this file>`, mirroring the
/// user's `statusLine` from `~/.claude/settings.json` (the config a manual, no-`CLAUDE_CONFIG_DIR`
/// launch reads). Returns `None` — and the caller omits `--settings` — when there's no
/// statusLine to carry or the copy fails; a settings hiccup must never block a goal spawn.
#[tracing::instrument(level = "debug", ret)]
fn write_goals_settings_config() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let src = std::path::Path::new(&home)
        .join(".claude")
        .join("settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(src).ok()?).ok()?;
    let status_line = parsed.get("statusLine").filter(|v| !v.is_null())?;
    let json = goals_settings_json(status_line);
    let path = avada_core::persistence::paths::state_dir().join("goals-settings.json");
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("failed to create state dir for goals-settings.json: {e}; spawning without --settings");
            return None;
        }
    }
    match std::fs::write(&path, json) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("failed to write goals-settings.json: {e}; spawning without --settings");
            None
        }
    }
}

#[cfg(test)]
mod goals_mcp_config_tests {
    #[test]
    fn json_registers_avada_server_with_control_path() {
        let control_path = "/tmp/example-state/control.json";
        let json = super::goals_mcp_config_json(control_path);
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("goals-mcp.json contents must parse as JSON");
        let avada = &parsed["mcpServers"]["avada"];
        assert_eq!(avada["command"], "npx");
        assert_eq!(avada["env"]["AVADA_CONTROL_FILE"], control_path);
        assert!(json.contains(control_path));
        assert!(json.contains("avada"));
    }

    #[test]
    fn settings_json_wraps_status_line_only() {
        let status_line = serde_json::json!({
            "type": "command",
            "command": "~/.claude/statusline-tee.sh",
        });
        let json = super::goals_settings_json(&status_line);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Only statusLine is carried — no behavior keys (model/effort/outputStyle) leak in.
        assert_eq!(
            parsed.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["statusLine"]
        );
        assert_eq!(parsed["statusLine"], status_line);
    }
}

#[cfg(test)]
mod goal_defaults_tests {
    use super::{clamp_goal_model_sel, GOAL_MODEL_SEL_DEFAULT};

    #[test]
    fn valid_indices_pass_through() {
        // A saved selection within range survives a round-trip unchanged.
        assert_eq!(clamp_goal_model_sel([2, 0, 1]), [2, 0, 1]);
        assert_eq!(
            clamp_goal_model_sel(GOAL_MODEL_SEL_DEFAULT),
            GOAL_MODEL_SEL_DEFAULT
        );
    }

    #[test]
    fn stale_out_of_range_indices_are_clamped() {
        // A file written when GOAL_MODELS was longer must not point past the end.
        let max = crate::command::GOAL_MODELS.len() - 1;
        assert_eq!(clamp_goal_model_sel([99, 0, 42]), [max, 0, max]);
    }

    #[test]
    fn serde_roundtrip_matches_saved_shape() {
        // save_goal_model_defaults writes a bare 3-element array; load parses the same.
        let sel = [1usize, 2, 3];
        let json = serde_json::to_string(&sel).unwrap();
        assert_eq!(json, "[1,2,3]");
        let back: [usize; 3] = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sel);
    }
}

/// The "New pane" dialog's payload — full spawn options for a configured pane. The simple
/// [`State::add_pane`] / [`State::add_pane_cwd`] paths build a default of this. The native
/// port of the Electron `addPane({ label, color, showFrame, showDot, command, cwd, shell })`.
#[derive(Debug, Clone, Default)]
pub struct NewPaneOpts {
    /// Label override (empty → the slot default, e.g. "pane 3").
    pub label: Option<String>,
    pub cwd: Option<String>,
    /// A command to run instead of an interactive shell (empty → interactive).
    pub command: Option<String>,
    /// Argv for `command`, passed straight to the pty with no shell parsing. `Some` turns the
    /// spawn into a direct exec (`command` + these args, no `sh -c`); `None`/empty keeps the
    /// shell-wrapped behaviour. The path a module's `panes.spawn { kind: "terminal" }` takes,
    /// so an untrusted argument can never be read as a shell metacharacter.
    pub args: Option<Vec<String>>,
    /// Shell token override ("" / `None` → the default-shell preference).
    pub shell: Option<String>,
    /// The chosen accent (the swatch). `None` = the by-slot palette color (a plain new pane).
    pub accent: Option<Color>,
    /// Explicit frame/dot. `None` = the default (tinted when `accent` is pinned, else off).
    pub show_frame: Option<bool>,
    pub show_dot: Option<bool>,
    /// Per-pane env overrides layered over the fresh spawn base (#27 linked terminal).
    pub env: Option<avada_core::session::spawn::EnvMap>,
    /// Text typed into the pane once it produces its first output (the boot-safe inject the
    /// resume path uses). Used to hand a freshly-spawned agent its opening prompt without a
    /// PTY timing race. `None` ⇒ nothing typed.
    pub startup: Option<String>,
    /// An explicit pane kind. `None` ⇒ derived from `command` (so "New Claude Pane" and
    /// "run `claude`" land in the same place without the caller having to say so twice).
    pub kind: Option<PaneKind>,
    /// The tool conversation id this pane is being opened into, when the caller knows it —
    /// the left panel's session list does, because it just picked one row. Paired with
    /// `cwd` (which the same caller sets) into the pane's [`ToolSessionMark`], so a relaunch
    /// re-resumes this exact chat instead of starting a fresh one.
    pub session: Option<String>,
}

/// Where the process was launched from, pinned once at startup by [`pin_launch_dir`].
///
/// `std::env::current_dir()` is not a stable answer later in the run: anything in-process may
/// `set_current_dir`, and on macOS a bundle launched from Finder/launchd reports `/`. Pinning
/// it at `main` entry keeps "the directory the app was started in" meaning exactly that.
static LAUNCH_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Record the process's launch directory. Called once from `main` before anything can `cd`.
///
/// The filesystem ROOT is deliberately rejected rather than stored: a double-clicked macOS
/// bundle (and a launchd-started process on Linux) reports `/`, which *is* a directory and so
/// would silently win over `$HOME` in [`State::resolve_new_pane_cwd`] — every new pane would
/// open on the whole disk. A `None` here simply moves the decision on to the next candidate.
#[tracing::instrument(level = "debug", ret)]
pub fn pin_launch_dir() {
    let _ = LAUNCH_DIR.get_or_init(|| {
        std::env::current_dir()
            .ok()
            .filter(|d| d.parent().is_some())
            .filter(|d| d.is_dir())
    });
}

/// The pinned launch directory, or `None` when it was the filesystem root / unreadable / the
/// pin never ran (tests, headless tools — the caller then falls through to `$HOME`).
#[tracing::instrument(level = "debug", ret)]
fn launch_dir() -> Option<String> {
    LAUNCH_DIR
        .get()
        .and_then(|d| d.as_ref())
        .map(|d| d.to_string_lossy().into_owned())
}

/// The first candidate that names a directory that actually exists.
///
/// Ordering alone is not enough: a pane's remembered cwd, a workspace anchor and a launch
/// directory all outlive the directory they point at (a deleted worktree, an unmounted
/// volume, a project moved on disk). Handing such a path to a pty does not fail loudly — the
/// shell dies during `chdir` with no output, and the pane just sits there empty. So each
/// candidate is stat'd and a dead one is SKIPPED, not used and not fatal.
///
/// Empty strings are treated as "not set" because that is how they arrive: Slint string
/// properties and the workspace JSON both default to `""` rather than to null.
#[tracing::instrument(level = "debug", ret, skip(candidates))]
fn first_existing_dir<I, S>(candidates: I) -> Option<String>
where
    I: IntoIterator<Item = Option<S>>,
    S: AsRef<str>,
{
    candidates.into_iter().flatten().find_map(|c| {
        let c = c.as_ref();
        (!c.is_empty() && Path::new(c).is_dir()).then(|| c.to_string())
    })
}

#[cfg(test)]
mod new_pane_cwd_tests {
    use super::{first_existing_dir, PaneKind, State};

    /// A scratch tree of real directories — the resolver stats every candidate, so the test
    /// has to hand it paths that exist (and one that provably does not).
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hp-pane-cwd-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn explicit_cwd_wins_over_every_fallback() {
        let a = scratch("explicit-a");
        let b = scratch("explicit-b");
        let got = first_existing_dir(vec![
            Some(a.to_string_lossy().into_owned()),
            Some(b.to_string_lossy().into_owned()),
        ]);
        assert_eq!(got.as_deref(), Some(a.to_string_lossy().as_ref()));
    }

    #[test]
    fn empty_and_none_candidates_are_not_set() {
        let a = scratch("empty-skip");
        let got = first_existing_dir(vec![
            None,
            Some(String::new()),
            Some(a.to_string_lossy().into_owned()),
        ]);
        assert_eq!(got.as_deref(), Some(a.to_string_lossy().as_ref()));
    }

    /// The whole point of stat'ing: a stale anchor must fall THROUGH to the next candidate
    /// instead of being handed to the pty, where it would kill the shell silently.
    #[test]
    fn a_candidate_that_does_not_exist_is_skipped() {
        let gone = scratch("gone").join("no-such-subdir");
        let a = scratch("skip-to");
        let got = first_existing_dir(vec![
            Some(gone.to_string_lossy().into_owned()),
            Some(a.to_string_lossy().into_owned()),
        ]);
        assert_eq!(got.as_deref(), Some(a.to_string_lossy().as_ref()));
    }

    /// A file is not a directory — `chdir` fails on it exactly like a missing path.
    #[test]
    fn a_file_is_not_a_usable_cwd() {
        let dir = scratch("file-candidate");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").expect("scratch file");
        let got = first_existing_dir(vec![
            Some(file.to_string_lossy().into_owned()),
            Some(dir.to_string_lossy().into_owned()),
        ]);
        assert_eq!(got.as_deref(), Some(dir.to_string_lossy().as_ref()));
    }

    #[test]
    fn no_usable_candidate_yields_none() {
        let gone = scratch("all-dead").join("nope");
        let got = first_existing_dir(vec![Some(gone.to_string_lossy().into_owned()), None]);
        assert_eq!(got, None);
    }

    /// Why [`State::resolve_new_pane_cwd`] takes a kind at all. A view pane's `cwd` is the
    /// FILE it is pointed at, not a directory to start a shell in, and it is what the
    /// workspace snapshot round-trips as that pane's target. Stat'ing it as a directory
    /// fails, and the pane silently falls through to the focused terminal's directory — a
    /// restored workspace comes back showing the wrong file (or the containing folder).
    #[test]
    fn a_view_panes_file_target_is_not_treated_as_a_directory() {
        let dir = scratch("view-target");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"x").expect("scratch file");
        let target = file.to_string_lossy().into_owned();
        let st = State::new(crate::theme::load_font(1.0));
        for kind in [
            PaneKind::FileViewer,
            PaneKind::Markdown,
            PaneKind::FileBrowser,
        ] {
            assert_eq!(
                st.resolve_new_pane_cwd(&kind, Some(target.clone()))
                    .as_deref(),
                Some(target.as_str()),
                "{kind:?} lost its target"
            );
        }
        // The pty side is unchanged by the gate: nothing can `chdir` into a file, so a
        // terminal handed the same path still falls through rather than spawning into it.
        assert_ne!(
            st.resolve_new_pane_cwd(&PaneKind::Terminal, Some(target.clone()))
                .as_deref(),
            Some(target.as_str())
        );
    }
}

/// The in-dialog draft of the **appearance** settings. While Preferences is open these edit
/// the draft only — the live panes don't change until Done (mirrors the renderer's
/// `AppearanceDraft`). General/Terminal settings (shell, clickable paths, editor) are not
/// drafted; they apply immediately, exactly like the Electron dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefsDraft {
    pub font_family: String,
    pub frame_palette: usize,
    pub terminal_theme: usize,
    /// Index into `theme::UI_PALETTES`. Drafted like the rest so Cancel restores it — but
    /// unlike the rest the *shell* follows the draft live (there is nowhere to preview a
    /// window's own chrome except in the window), so picking a palette recolours
    /// immediately and Cancel puts it back.
    pub ui_palette: usize,
    pub font_px: f32,
    pub show_frame: bool,
    pub show_dot: bool,
}

impl PrefsDraft {
    /// Snapshot the appearance subset of `s`.
    #[tracing::instrument(level = "debug", ret)]
    fn from_settings(s: &Settings) -> Self {
        PrefsDraft {
            font_family: s.font_family.clone(),
            frame_palette: s.frame_palette,
            terminal_theme: s.terminal_theme,
            ui_palette: s.ui_palette,
            font_px: s.font_px,
            show_frame: s.show_frame,
            show_dot: s.show_dot,
        }
    }
}

/// The ambient-AI subtitle state for one pane: the latest engine-produced summary line
/// (`full`, already redacted by `core::ai`) and a typewriter `reveal` cursor (chars shown
/// so far, as a float so the pump can advance it a sub-character per tick for a smooth
/// reveal). Empty `full` = no AI line for this pane. A manual subtitle and the per-pane
/// Mute flag both suppress the AI line at render time (manual always wins).
#[derive(Debug, Clone, Default)]
pub struct AiLine {
    pub full: String,
    pub reveal: f32,
    /// Cached `full.chars().count()` — the typewriter reveal needs this every frame for every
    /// AI pane, so it's computed once here (on `set_target`) rather than re-walking `full` each
    /// tick. Kept in lock-step with `full`: only `set_target` mutates either.
    pub len: usize,
}

impl AiLine {
    /// Set the target summary text; restart the typewriter when it actually changed.
    #[tracing::instrument(level = "debug", ret)]
    pub fn set_target(&mut self, text: &str) {
        if self.full != text {
            self.full = text.to_string();
            self.len = self.full.chars().count();
            self.reveal = 0.0;
        }
    }
}

/// One pane's controller-side state (terminal grid + placement + chrome).
pub struct PaneState {
    pub uid: String,
    /// The pane's editable label (the header title): "shell"/"pane N" by default, the
    /// first word of a launched command, or — once tinted to a git project — the repo
    /// name. Double-click the header to rename (see [`State::begin_rename_pane`]).
    pub title: SharedString,
    /// An optional secondary line under the label (user-set subtitle). `None` = none.
    pub subtitle: Option<SharedString>,
    /// Per-pane override of the global `show_frame`/`show_dot` Appearance prefs (mirrors
    /// the renderer's `pane.showFrame ?? globalShowFrame`). NEW panes default both to
    /// `Some(false)` (clean: no colored border/tint/dot); a git-project tint flips them to
    /// `Some(true)`; `None` would inherit the global pref. The pane still carries a `color`
    /// VALUE (its `accent`) even while clean.
    pub show_frame: Option<bool>,
    pub show_dot: Option<bool>,
    pub accent: Color,
    pub pane: TerminalPane,
    /// Cell dims currently applied to the LOCAL grid (to detect a real reflow).
    pub applied: (usize, usize),
    /// Cell dims last reported to the session backend (the pty's own idea of its size).
    ///
    /// Split from [`applied`](Self::applied) because the two must not move together. Telling
    /// the pty is a SIGWINCH, and a shell answers one by reclaiming the rows it thinks its
    /// prompt occupies and erasing to the end of the display — zsh emits a literal
    /// `\r\e[A\e[A…\e[J`. Do that while the layout is still settling and it eats the last
    /// command's output. Reflowing the local grid, by contrast, is lossless.
    pub pty: (usize, usize),
    /// When [`applied`](Self::applied) last moved — the debounce clock for pushing `pty`.
    pub pty_since: std::time::Instant,
    /// The latest rendered terminal image.
    pub surface: Image,
    /// Placement in logical px, recomputed on relayout.
    pub rect: (f32, f32, f32, f32),
    pub visible: bool,
    /// Whether the shell has produced its first output yet (gate the startup write).
    pub started: bool,
    pub startup: Option<String>,
    /// A fixed accent (e.g. a project color) that survives relabel; `None` = by-index.
    pub pinned_accent: Option<Color>,
    /// The terminal image's on-screen logical size — `cols x rows` cells, NOT the widget body
    /// it is centred in (the body's sub-cell remainder shows through as empty terminal space).
    /// Every hit-test divides by cols/rows to recover the cell size, so this has to be the
    /// image or the pointer drifts off the glyph. Owned by `relayout_active`; `(0,0)` until the
    /// first layout.
    pub surf: (f32, f32),
    /// The current clickable-path hover hit (drives the link overlay), plus the cursor
    /// position (logical px within the surface) for tooltip placement. `None` = no link.
    pub link: Option<avada_terminal_widget::LinkHit>,
    pub link_cursor: (f32, f32),
    /// Idle-glow animation state — its `alpha` (0 when not glowing) is projected into the
    /// pane model each tick once the pane has been output-quiet past the idle threshold.
    pub glow: Glow,
    /// The pane's latest OSC window title (sniffed from pty output), used to detect an
    /// AI/agent CLI so the idle glow only arms on agent panes (mirrors `isAiPane`). "" until
    /// the shell sets a title.
    pub shell_title: String,
    /// Whether the pane's ambient-AI summary line is muted (the pane menu's "Mute AI Summary"
    /// toggle; mirrors the renderer's `ui.aiMuted` set). New panes default unmuted.
    pub ai_muted: bool,
    /// Per-pane "talk": speak NEW assistant replies aloud via local TTS (the pane menu's
    /// "Talk" toggle). New panes default off; mirrors the control read-model's `PaneInfo::talk`.
    pub talk: bool,
    /// Whether this pane is recording dictation right now (the header microphone). NOT owned
    /// here and never persisted: the recorder lives on the control server's `DictationService`,
    /// and this is the projection stamped back each sync tick so the header can light up.
    pub recording: bool,
    /// Ambient-AI subtitle + typewriter reveal state (the local projection of this pane's
    /// `meta['ai.subtitle']`; produced by the `core::ai` engine when enabled).
    pub ai: AiLine,
    /// The last polled value of the widget's transient bottom-right indicator ("toast" —
    /// copy/paste confirmations + the Ctrl-zoom font %), cached so the pump can detect a
    /// change and update/clear the row even when the surface itself isn't dirty.
    pub last_toast: String,
    /// Whether the vim scrollbar was drawn (opacity > 0) on the previous pump tick. Lets the pump
    /// keep re-pushing the row (and stay at the fast cadence) while the bar fades, then push one
    /// final frame when it disappears — without it the bar would freeze mid-fade once the pane goes
    /// otherwise-idle.
    pub scrollbar_on: bool,
    /// Bumped each time Ctrl+F (re)invokes the in-pane search box on this pane, even while it's
    /// already open. Projected into `PaneItem::search_focus_seq` so the widget can (re)focus the
    /// query input on a reliable change signal rather than a one-shot `init`.
    pub search_focus_seq: i32,
    /// Bumped when the New-goal box CLOSES, to hand keyboard focus back to this pane's terminal
    /// `FocusScope` (the box owns its own scope while open — see [`State::goal_focus_seq`]).
    /// Projected into `PaneItem::refocus_seq`; the widget's `changed refocus-tick` calls
    /// `fs.focus()`.
    pub refocus_seq: i32,
    /// The pane's OWN terminal font size (logical px) — per-pane zoom. Ctrl+= / − / 0 adjust
    /// the FOCUSED pane only (Electron parity); a new pane starts at the configured base
    /// (`Settings::font_px`). Drives both the rendered glyphs and the indicator scaling.
    pub font_px: f32,
    /// The font loaded at `font_px × DPI-scale` (its own glyph cache + cell metrics), so each
    /// pane renders — and reflows — at its own zoom level independently of its neighbours.
    pub font: avada_terminal_widget::Font,
    /// Set when `font_px` changed → the pump reloads `font` at the current DPI scale and
    /// forces a repaint on the next tick (a DPI / family / base-size change reloads via
    /// [`State::reload_font`] instead).
    pub font_dirty: bool,
    /// The pane's latest reported working directory (OSC-7 / OSC-9;9 sniff), mirrored from
    /// the session's `Cwd` events. Surfaced into the control read-model's `/state` so agents
    /// (and `list_panes`) see each pane's live cwd. `None` until the shell reports one.
    pub cwd: Option<String>,
    /// The env overrides this pane was spawned with (`None` = none), kept so "Open Linked
    /// Terminal" / "Refresh Env" can re-spawn with the same per-pane context (#27/#28).
    pub env: Option<avada_core::session::spawn::EnvMap>,
    /// A SHORT shell-type label (e.g. "pwsh", "cmd", "bash") derived once at pane creation
    /// from the resolved spawn shell program (see [`shell_label`]) and projected dim after
    /// the title in the header. "" = unknown (e.g. a re-hosted pane whose original shell
    /// isn't tracked across the detach).
    pub shell_label: String,
    /// What this pane was spawned with — the original `command`/`args`/`shell` handed to
    /// [`SpawnOptions`] at creation, kept so the relaunch snapshot ([`State::to_session_file`])
    /// can record them. That lets restore re-run the *original program* (e.g. `claude`) instead
    /// of a default shell — and is the spawn-spec half of the session-daemon M2 re-attach (the
    /// uid half is `self.uid`). `None` = an interactive shell at the default (nothing to record).
    pub spawn_command: Option<String>,
    pub spawn_args: Option<Vec<String>>,
    pub spawn_shell: Option<String>,
    /// What this pane *is*: a plain terminal, one specific CLI tool's pane, or a non-pty
    /// view (file browser / viewer / markdown / browser). Set at creation from the spawn
    /// command, upgraded later when a plain terminal is seen running a known tool, and
    /// persisted through `PaneSpec`'s `meta["pane.kind"]` so it survives a relaunch.
    ///
    /// Only [`PaneKind::is_pty`] kinds have a live session behind them; the views render
    /// into the same `surface` waist without a pty.
    pub kind: PaneKind,
    /// Which tool conversation this pane is in, when it is known — set when the pane was
    /// opened from the left panel's session list, and persisted through `PaneSpec`'s
    /// `meta["tool.session"]` / `meta["tool.cwd"]` so a relaunch re-resumes the same chat.
    ///
    /// `kind` usually already says *which tool*, so the mark does not normally repeat it.
    /// The exception is a pane the user turned into a tool pane **by hand**: the runtime
    /// sniff re-brands such a pane for display but never writes its persisted kind, so the
    /// kind stays `Terminal` and answers nothing. A mark adopted for one of those carries
    /// the tool id itself (`meta["tool.id"]`) so the restore path can still name the
    /// binary to resume.
    pub tool_session: Option<ToolSessionMark>,
}

impl PaneState {
    /// Effective frame visibility: the per-pane override if set, else the global pref.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn frame_on(&self, global: bool) -> bool {
        self.show_frame.unwrap_or(global)
    }
    /// Effective dot visibility: the per-pane override if set, else the global pref.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn dot_on(&self, global: bool) -> bool {
        self.show_dot.unwrap_or(global)
    }
}

/// Whether `label` is still a default auto-name ("shell" / "pane N"), so a git-project tint
/// may overwrite it — never a name the user chose. Mirrors the renderer's `/^(shell|pane \d+)$/i`
/// test. A bare number (e.g. "42") is NOT treated as default: it's a valid user rename, and
/// silently overwriting it with the repo name on a cwd change would clobber that choice.
#[tracing::instrument(level = "debug", ret)]
fn is_default_label(label: &str) -> bool {
    let l = label.trim();
    if l.eq_ignore_ascii_case("shell") {
        return true;
    }
    if let Some(rest) = l.strip_prefix("pane ").or_else(|| l.strip_prefix("Pane ")) {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// Derive a SHORT shell-type label from a resolved spawn program (the shell the pane is
/// running on, e.g. `pwsh.exe`, `C:\Windows\system32\cmd.exe`, `/bin/bash`, `wsl.exe`).
/// Computed once at pane creation and cached on [`PaneState::shell_label`] (never per-frame),
/// then rendered dim after the pane title. Kept app-side (mirrors core's `is_posix_shell`
/// basename matching) so the badge needs no `core` change.
///
/// Uids whose ASYNC pty spawn just completed (see [`State::spawn_session_async`]).
/// `App::tick` drains this each tick: it forces a geometry re-apply for the pane (the
/// pump may have resized it while the session didn't exist yet — a resize on a missing
/// uid is a silent no-op, so without the re-apply the pty would stay at its spawn size),
/// and kills the session if the pane was closed mid-spawn.
#[tracing::instrument(level = "debug", ret)]
pub fn spawn_done() -> &'static std::sync::Mutex<Vec<String>> {
    static Q: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    Q.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Uids with a currently-live "last write to this pane failed" flag, set by
/// [`note_pane_write`]. A dead backend fails every keystroke after the first, so this
/// is a dedup registry, not a log: it exists to answer "is this failure NEW" so callers
/// log and surface it exactly once per outage instead of once per keypress. Cleared by a
/// later successful write to the same uid, so a pane that recovers (or is torn down and
/// its uid retired — see [`State::forget_pane_runtime`]) can fail loudly again later.
#[tracing::instrument(level = "debug", ret)]
fn pane_write_failures() -> &'static std::sync::Mutex<BTreeSet<String>> {
    static Q: std::sync::OnceLock<std::sync::Mutex<BTreeSet<String>>> = std::sync::OnceLock::new();
    Q.get_or_init(|| std::sync::Mutex::new(BTreeSet::new()))
}

/// Records the outcome of a write to `uid` against the dedup registry in
/// [`pane_write_failures`] and reports whether THIS call is the one that should be
/// acted on: a fresh failure (log it, surface it) or a fresh recovery (nothing to do,
/// but the registry needed clearing so the next failure isn't suppressed). Repeats of
/// the same outcome are deliberately silent — see [`pane_write_failures`] for why.
///
/// Split out from [`pane_write`] so the decision logic is testable without a real
/// session or pty: every case here is exercised directly against a bare uid string in
/// `mod pane_write_tests` below.
#[tracing::instrument(level = "debug", ret)]
fn note_pane_write(uid: &str, ok: bool) -> bool {
    let mut failed = pane_write_failures()
        .lock()
        .expect("pane_write_failures lock");
    if ok {
        // `remove` reports whether the uid was present, i.e. whether this write is the
        // one that ends an outage. A success on a uid with no prior failure is the
        // overwhelmingly common case and is not newsworthy.
        failed.remove(uid)
    } else {
        // `insert` reports whether the uid is NEW to the set, i.e. whether this is the
        // first failure of the current outage rather than the 40th repeat of it.
        failed.insert(uid.to_string())
    }
}

/// Uids whose most recent write just failed FOR THE FIRST TIME (per [`note_pane_write`]'s
/// dedup). `App::tick` drains this each tick, on the UI thread, and marks each pane
/// [`AgentLiveness::Error`] via `State::note_agent_state` — the same mechanism a stalled
/// or crashed agent already uses to put a red dot on a pane. Mirrors [`spawn_done`]:
/// callers of [`pane_write`] run on both the UI thread and background threads (queued
/// prompt/goal delivery, status polling), and `State` is `!Send`, so a write failure
/// discovered off the UI thread has no `State` to mark directly — it drops its uid here
/// instead and lets the next tick do the marking.
#[tracing::instrument(level = "debug", ret)]
pub fn pane_write_failed_uids() -> &'static std::sync::Mutex<Vec<String>> {
    static Q: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    Q.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// The one path every GUI-side pane write should go through, replacing a bare
/// `let _ = mgr.write(...)`. Forwards to [`SessionManager::write`] and, on failure,
/// dedups (see [`pane_write_failures`]), logs, and queues the pane for a visible
/// `AgentLiveness::Error` mark (see [`pane_write_failed_uids`]) — but only on the
/// FIRST failure of an outage. A dead session daemon fails every subsequent keystroke
/// too; logging or re-marking each one would bury the one useful signal (that the
/// backend died) under noise proportional to how much the user kept typing into a
/// pane that looked alive. `op` is a short static tag (`"keystroke"`, `"paste"`, ...)
/// naming what kind of write this was, for the warn log — never the data itself.
///
/// `skip_all` and no `ret`: `data` is exactly what the user typed or pasted, up to and
/// including secrets like `ANTHROPIC_API_KEY`. Neither the entry log nor a return value
/// may ever carry it — see the same treatment on `persist_last_session` and
/// `embed_claude_sessions` in `app.rs`.
#[tracing::instrument(level = "debug", skip_all)]
pub fn pane_write(mgr: &SessionManager, uid: &str, data: &str, op: &str) {
    let ok = mgr.write(uid, data).is_ok();
    if note_pane_write(uid, ok) && !ok {
        tracing::warn!(uid = %uid, op, "pane write failed; backend is gone, further writes to this pane will be suppressed until it recovers or is closed");
        pane_write_failed_uids()
            .lock()
            .expect("pane_write_failed_uids lock")
            .push(uid.to_string());
    }
}

#[cfg(test)]
mod pane_write_tests {
    use super::*;

    /// A bare uid with no session ever created behind it: `SessionManager::write`
    /// returns `Err(NotFound)` synchronously for it, no pty or tokio runtime involved
    /// (unlike `State::add_pane`, which does need `#[tokio::test]`  — see
    /// `hyperpane_uniqueness_tests::mgr` below).
    fn dead_mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    /// Each test claims its own uid so the shared, process-global registries in
    /// `pane_write_failures`/`pane_write_failed_uids` can't leak state between tests
    /// run concurrently by the same test binary.
    fn unique_uid(tag: &str) -> String {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("pane-write-test-{tag}-{n}")
    }

    #[test]
    fn a_first_failed_write_is_reported_and_queued() {
        let uid = unique_uid("first-failure");
        let m = dead_mgr();
        pane_write(&m, &uid, "hello", "keystroke");
        assert!(
            pane_write_failed_uids().lock().unwrap().contains(&uid),
            "pane_write must queue the uid for a UI mark on its first failure"
        );
    }

    #[test]
    fn a_repeat_failure_on_the_same_pane_is_suppressed() {
        let uid = unique_uid("repeat-failure");
        assert!(note_pane_write(&uid, false), "first failure is new");
        assert!(
            !note_pane_write(&uid, false),
            "a second failure on the same uid must be suppressed, or a dead backend \
             would log/queue once per keystroke instead of once per outage"
        );

        pane_write_failed_uids().lock().unwrap().clear();
        let m = dead_mgr();
        pane_write(&m, &uid, "a", "keystroke");
        pane_write(&m, &uid, "b", "keystroke");
        let queued: Vec<_> = pane_write_failed_uids()
            .lock()
            .unwrap()
            .iter()
            .filter(|q| **q == uid)
            .cloned()
            .collect();
        assert_eq!(
            queued.len(),
            0,
            "an already-failed uid must not be re-queued"
        );
    }

    #[test]
    fn a_success_after_failure_clears_the_dedup_state_so_the_next_failure_logs_again() {
        let uid = unique_uid("recovers");
        assert!(note_pane_write(&uid, false), "first failure is new");
        assert!(!note_pane_write(&uid, false), "repeat is suppressed");

        assert!(
            note_pane_write(&uid, true),
            "a success must clear the dedup entry (reported as 'this ended an outage')"
        );
        assert!(
            note_pane_write(&uid, false),
            "after a recovery, a new failure must be treated as new again"
        );
    }

    #[test]
    fn a_success_with_no_prior_failure_is_not_newsworthy() {
        let uid = unique_uid("always-fine");
        assert!(
            !note_pane_write(&uid, true),
            "a success on a uid that never failed must not be reported"
        );
    }
}

/// Maps the common shells to a canonical lowercase label (`pwsh.exe`→`pwsh`,
/// `powershell.exe`→`powershell`, `cmd.exe`→`cmd`, `bash(.exe)`→`bash`, `wsl.exe`→`wsl`,
/// plus `nu`/`zsh`/`fish`/`sh`/`dash`/`ash`); anything else falls back to the bare basename
/// with a trailing `.exe` stripped. An empty/whitespace program yields "".
#[tracing::instrument(level = "debug", ret)]
fn shell_label(program: &str) -> String {
    // basename after the last path separator (handles full paths like COMSPEC's cmd.exe).
    let base = program
        .trim()
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .trim();
    // strip a trailing ".exe" (case-insensitive) for a tidy badge. The lowercased `ends_with`
    // keeps the byte index a valid char boundary even for a non-ASCII basename.
    let stem = if base.to_ascii_lowercase().ends_with(".exe") {
        &base[..base.len() - 4]
    } else {
        base
    };
    if stem.is_empty() {
        return String::new();
    }
    match stem.to_ascii_lowercase().as_str() {
        "pwsh" => "pwsh".to_string(),
        "powershell" => "powershell".to_string(),
        "cmd" => "cmd".to_string(),
        "bash" => "bash".to_string(),
        "wsl" => "wsl".to_string(),
        "nu" => "nu".to_string(),
        "zsh" => "zsh".to_string(),
        "fish" => "fish".to_string(),
        "sh" => "sh".to_string(),
        "dash" => "dash".to_string(),
        "ash" => "ash".to_string(),
        // an unrecognised program → the bare basename (.exe already stripped).
        _ => stem.to_string(),
    }
}

/// One tab = a self-contained workspace group (the Rust port of `useWorkspace`'s
/// `Group`). Background tabs keep their `PaneState`s — and thus their live
/// sessions — alive; only the active tab is mounted in the UI models.
pub struct Tab {
    pub title: SharedString,
    pub panes: Vec<PaneState>,
    pub layout: Layout,
    pub sizes: Vec<f64>,
    pub main_fraction: f64,
    pub focused: usize,
    /// Index of the zoomed (maximised-in-tab) pane, if any.
    pub zoomed: Option<usize>,
    /// Whether this is a SYSTEM tab — the app owns it, the human does not get to close it.
    /// Exactly one exists today (the always-on "Hyperpane" tab, see [`State::ensure_hyperpane_tab`]),
    /// but nothing here assumes that. A system tab is ordinary in every other respect: it can be
    /// renamed, reordered, split, and its panes closed like any other. Only the close is refused,
    /// and it is refused at the state layer rather than only in the UI, because the control plane
    /// and the keyboard both reach `close_tab` without going through a × button.
    pub system: bool,
}

impl Tab {
    #[tracing::instrument(level = "debug")]
    fn empty(title: SharedString) -> Self {
        Tab {
            title,
            panes: Vec::new(),
            layout: Layout::Auto,
            sizes: Vec::new(),
            main_fraction: 0.6,
            focused: 0,
            zoomed: None,
            system: false,
        }
    }

    /// Recolor panes so each unpinned pane's accent tracks its creation slot in the
    /// current palette. A pinned accent (a manual recolor or a git-project color) is
    /// preserved. Pane LABELS are stable and user-owned, so they're left untouched —
    /// unlike the old build, panes are no longer renumbered 1..N (that was the bare-number
    /// "title" bug). The color VALUE stays assigned even on a clean (frame-off) pane.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn relabel(&mut self, palette: usize) {
        for (i, p) in self.panes.iter_mut().enumerate() {
            p.accent = p
                .pinned_accent
                .unwrap_or_else(|| theme::accent_for(i, palette));
        }
    }

    /// The concrete preset this tab currently tiles with (auto resolved).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn effective(&self) -> Layout {
        effective_layout(self.layout, self.panes.len())
    }
}

/// The whole window's workspace state.
pub struct State {
    /// The base font (loaded at the configured `font_px`) — the template a new pane copies its
    /// size from; per-pane rendering uses each pane's own [`PaneState::font`].
    pub font: avada_terminal_widget::Font,
    /// The DPI scale of the last pump tick, so pane fonts (created/zoomed outside the pump,
    /// where the scale isn't known) can be loaded at the right physical size.
    pub last_scale: f32,
    pub tabs: Vec<Tab>,
    pub active: usize,
    tab_seq: usize,
    /// Set when preparing the Hyperpane directory failed; creation/reseed is not retried
    /// before this instant. See [`Self::materialize_hyperpane_dir`].
    hyperpane_retry_at: Option<std::time::Instant>,
    pub fullscreen: bool,
    /// Index of the tab whose title is being edited inline (-1 = none).
    pub editing_tab: i32,
    /// Index (within the active tab) of the pane whose label is being edited inline
    /// (-1 = none). Double-clicking a pane header sets this; it drives the inline editor.
    pub editing_pane: i32,
    /// Which pane last had the keyboard handed to it, as `(tab index, pane uid)`. The
    /// per-tick [`Self::sync_pane_keyboard_focus`] compares the current selection against
    /// this and only then bumps a `refocus_seq`.
    pub kbd_focus_on: Option<(usize, String)>,
    /// The uid of the pane whose terminal `FocusScope` last reported `has-focus` — the
    /// CONFIRMATION half of the hand-off above, fed by the widget's `focus-changed`.
    ///
    /// Without it the hand-off was fire-and-forget, and it silently missed whenever Slint had
    /// just re-created the pane's repeated element (any tab switch between tabs with different
    /// pane counts, and every app start): a `changed refocus-tick` never fires for a property's
    /// INITIAL value, so the focus call was never made and the keystrokes stayed on the
    /// previous pane until the user clicked. Also published on `/state` as the window's
    /// `keyboardFocusPaneId`, which is how the fix is testable from outside the GUI.
    pub kbd_focus_live: Option<String>,
    /// Hand-offs left to attempt for the current selection. Set when the selection moves,
    /// spent one per tick until the pane confirms focus. Bounded so a pane that can never take
    /// focus (an unfocused window, a Family-B view pane with no terminal) cannot spin.
    pub kbd_focus_tries: u8,
    /// Monotonic source for every `refocus_seq` bump. A per-pane counter is not enough:
    /// the pane model is rebuilt per active tab, so the value Slint compares against is
    /// whatever the OTHER tab's pane at that repeater slot last showed. Two independent
    /// per-pane counters collide (both sit at 0 on a fresh workspace, and drift back into
    /// step later), and a collision reads as "unchanged" — no `changed` handler, no focus.
    /// One clock for the whole app can never repeat a value.
    pub refocus_clock: i32,
    pub last_blink: Instant,
    pub cursor_on: bool,
    pub frames: u32,
    pub last_hud: Instant,
    /// The UI models (tabs / panes / dividers) need a full rebuild.
    pub dirty: bool,
    // ---- Wave-2: overlay panels (Seam #3) ----
    /// Which overlay panel is mounted (palette / prefs / sidebar / none).
    pub overlay: Overlay,
    /// Inline validation error shown in the Add-Project dialog (`""` = none). Set when a
    /// submitted path doesn't exist / isn't a directory; cleared on open and on close.
    pub add_project_error: String,
    /// The URL the [`Overlay::AskBrowser`] chooser is holding (`""` when it isn't open).
    /// Already validated by [`avada_core::open::is_openable_url`] before the overlay
    /// mounts — the chooser never displays a URL it would refuse to open.
    pub ask_url: String,
    /// The browsers offered by the [`Overlay::AskBrowser`] chooser. Snapshotted at open so
    /// the row a human clicks is the row they saw, even if an install finishes mid-choice.
    pub ask_browsers: Vec<avada_core::open::BrowserApp>,
    /// Persisted appearance preferences (font, frame/dot).
    pub settings: Settings,
    /// The user's keybinding overrides — consulted (override-first) by the key router. Edited
    /// live from the Preferences → Keybindings panel.
    pub keymap: crate::keybindings::Keymap,
    /// The binding id currently capturing a new chord in the editor (`None` = not capturing).
    /// While set, that editor row grabs focus and the next key combo rebinds it.
    pub capturing_binding: Option<String>,
    /// While capturing, the label of the binding the last-pressed chord clashes with (`None` =
    /// no clash yet). Drives the editor's "Used by <label>" message; cleared on a clean bind.
    pub capture_conflict: Option<String>,
    /// Set when the font family/size changed — the pump reloads the font (it owns the
    /// DPI scale) then clears this.
    pub font_reload: bool,
    /// The in-dialog appearance draft (Some while Preferences is open). Appearance edits go
    /// here and only commit to `settings` (and the panes) on Done.
    pub prefs_draft: Option<PrefsDraft>,
    /// Whether the "unsaved appearance changes" save/discard prompt is showing.
    pub prefs_confirm: bool,
    /// Whether the font picker is in "Custom…" mode (showing the free-text font path field).
    pub font_custom: bool,
    // ---- appearance preview: a real, locked (no-pty) terminal showing sample output ----
    /// The preview pane (fed canned sample output once; never bound to a session).
    preview_pane: TerminalPane,
    /// The font the preview renders with, reloaded when the drafted family/size/scale change.
    preview_font: Option<Font>,
    /// Cache key for `preview_font`: `(font_path, px, scale)`.
    preview_key: (String, f32, f32),
    /// Last terminal-theme index applied to the preview pane (-1 = none yet).
    preview_theme: i32,
    /// Last cursor on/off state rendered into the preview (so the caret blinks).
    preview_cursor: bool,
    /// The latest rendered preview image (shown in the Appearance preview).
    pub preview_surface: Image,
    /// Animates the idle-glow demo on the AI-features preview (always "idle" so the chosen
    /// effect plays continuously while Preferences is open).
    pub preview_glow: Glow,
    /// The self-playing Tetris shown in the preview pane (ambient animation).
    pub preview_tetris: crate::tetris::Tetris,
    /// When the Tetris last advanced a frame (it steps on a fixed cadence, not every tick).
    pub preview_tetris_last: Instant,
    /// Cached, newest-first git-project list for the sidebar rail.
    pub projects: Vec<Project>,
    /// Whether the projects flyout (behind the 📁 icon) is currently expanded. The rail
    /// itself is gated by `settings.show_sidebar`; this is just the flyout panel state.
    pub sidebar_open: bool,
    /// Whether the LEFT slide-out panel (workspace tree / detached sessions / a module's
    /// rows) is
    /// open. Like `sidebar_open` this is pure window UI state — not persisted — and the
    /// panel is a sibling of the pane area, so opening it shrinks the panes rather than
    /// covering them. See `crate::leftpanel` + `ui/leftpanel.slint`.
    pub left_panel_open: bool,
    /// When this window last aged the panel's liveness dots. Also per window, for the same
    /// reason — a shared stamp is consumed by whichever window is pumped first, freezing
    /// every other window's dots. See `leftpanel::heartbeat_due`.
    pub left_panel_beat: Option<std::time::Instant>,
    /// The workspace file this window was last saved to / opened from, if any (M6). Set by
    /// "Save workspace as…" and "Open workspace…", used by "Save workspace" to write back
    /// silently instead of re-prompting. `None` ⇒ "Save workspace" prompts.
    pub workspace_path: Option<std::path::PathBuf>,
    // ---- command palette working state ----
    /// The registry snapshot built when the palette opened.
    palette_entries: Vec<Entry>,
    /// Indices into `palette_entries` that survive the current query, best-first.
    pub palette_view: Vec<usize>,
    /// The highlighted row within `palette_view`.
    pub palette_sel: usize,
    /// The live search query.
    pub palette_query: String,
    // ---- hold-Esc-to-exit-fullscreen tracking (no key-release events, so we
    // infer a held key from rapid auto-repeat) ----
    esc_last: Option<Instant>,
    esc_hold_start: Option<Instant>,
    /// True while Esc is being held — drives the hint + its progress fill.
    pub esc_holding: bool,
    esc_fired: bool,
    // ---- context menus (Phase-5 parity) ----
    /// The open cursor-anchored context menu (pane header / tab strip), if any. Built fresh on
    /// each right-click so its gating + checkmarks reflect the moment it opened.
    pub ctx: Option<crate::contextmenu::CtxMenu>,
    // ---- the window's project anchor ----
    //
    // Named for what it is rather than for the panel that used to own it: the built-in file
    // explorer became the `bshuler/avada-files` module, but the *project* it was rooted at is
    // still what a git module reads and what a new pane inherits as its cwd.
    /// The project the human is looking at, not the whole disk. `None` until something needs
    /// it, when it is derived from the focused pane's cwd.
    pub project_root: Option<PathBuf>,
    /// The focused pane's cwd that [`State::project_root`] was derived from. The panel is
    /// anchored to the SELECTED pane (K), so this is what tells it the anchor has moved:
    /// while it still matches the focused pane, an explicitly chosen root is the human's and
    /// is left alone.
    pub project_root_from: Option<String>,
    /// What every running module has put on the left panel's mode strip, and which module
    /// entry (if any) the panel is showing (track H4). Fed by
    /// [`State::apply_rail_event`] from the module host's rail channel.
    pub rail: crate::leftpanel::ModuleRail,
    /// Work for the module host, queued by a rail click and drained by whoever owns the
    /// host (see [`State::take_rail_requests`]). A queue rather than a direct call because
    /// `State` is behind a `RefCell` borrow for the whole of a command dispatch: calling
    /// into the host from here would let a host callback re-enter it.
    pub rail_requests: Vec<RailRequest>,
    /// While set, every tick re-asserts the rail's scroll target for
    /// [`crate::paneview::RAIL_SCROLL_HOLD`]. A reveal asks for a scroll on a frame where
    /// the list cannot yet honour it — the module has not answered, the panel is being
    /// instantiated, the `ListView` has not measured the new rows — so the ask is repeated
    /// for a few frames rather than fired once into a view that is not there.
    pub rail_scroll_hold: Option<std::time::Instant>,
    /// Host events queued for `avada_core::module::Host::emit`, as `(kind, payload)`.
    /// Queued rather than emitted for the same re-entrancy reason as
    /// [`State::rail_requests`], and drained beside it.
    pub module_events: Vec<(String, serde_json::Value)>,
    /// Opener-routed module panes awaiting registration with the host's per-module pane
    /// guard, as `(module, surface)`. When the app opens a file into a module pane (see
    /// [`Command::FilesOpen`](crate::command)) the surface never went through
    /// `host.panes.spawn`, so `avada_core::module::Host`'s dispatcher has no record of it and
    /// would reject the module's first `host.doc.set` / `host.rows.set`. This queue is drained
    /// beside [`State::module_events`] — but strictly *before* it — so the surface is
    /// registered before the `DOC_OPEN` event that provokes the module to push its document.
    pub opener_panes: Vec<(avada_core::rights::ModuleId, String)>,
    /// The file types installed modules claim to open, refreshed from
    /// `avada_core::module::Host::openers` each module tick. A file-open consults this before
    /// it falls back to a built-in viewer, so a module that claimed the extension gets the
    /// pane. Sorted by the host (extension, then module), so the first match on an extension
    /// is the tie-break winner — see [`State::opener_for_path`].
    pub module_openers: Vec<avada_core::module::Opener>,
    /// Capability rights for every installed module — the truth the Preferences rights
    /// page projects and the ask toast answers against (track H2). Constructed empty and
    /// rooted at the real app-support dir; nothing is read or written until a module is
    /// registered, so a build with no modules installed never touches the disk.
    /// Shared, not owned: `State` is per window and the control plane edits the same
    /// service (see [`crate::prefs::rights::shared`]).
    pub rights: std::sync::Arc<std::sync::Mutex<avada_core::rights::RightsService>>,
    /// The module the rights page is showing (`None` = its first row). Page selection, not
    /// rights truth, so it lives here rather than in the service.
    pub rights_selected: Option<avada_core::rights::ModuleId>,
    /// Answered asks and accepted held updates waiting for whoever owns the module host and
    /// the install store: an `Answered` is the decision the host relays to the module that
    /// is blocked on it, an `Accepted` the capability set the install store must re-sign
    /// into the record. Queued for the same reason as [`State::rail_requests`] — `State` is
    /// borrowed for the whole dispatch.
    pub rights_effects: Vec<crate::prefs::rights::Applied>,
    /// The recently-closed history — panes AND tabs in ONE list, newest last, sessions kept
    /// alive centrally so reopening re-docks the running shell. One list rather than two
    /// because "reopen the last thing I closed" is only answerable if both kinds share an
    /// order: with separate stacks, closing a pane after a tab makes the tab unreachable by
    /// the keyboard command. Capped at [`CLOSED_STACK_CAP`] — evicted entries are killed.
    /// NOT persisted: sessions don't survive a relaunch, so neither can their history.
    pub closed: Vec<ClosedItem>,
    /// Whether the rail's RECENTLY CLOSED section is expanded (mirrors `reminders_open`;
    /// the two are mutually exclusive sections of the same widened rail).
    pub closed_open: bool,
    /// The close waiting on the confirm card's answer (`None` when it isn't up). Held as a
    /// target rather than a closure so the answer runs back through the ordinary command path.
    pub pending_close: Option<PendingClose>,
    // ---- reminder panes (Track F) ----
    /// Panes parked "until a chosen time": removed from the layout but their sessions stay
    /// alive centrally (the same detach machinery as the `closed` history). The app tick marks
    /// entries `fired` when due; clicking a bell-list row re-docks the pane into the active
    /// tab and removes its entry. NOT persisted — sessions don't survive a relaunch, so
    /// reminders die with them (matching the session lifecycle).
    pub reminders: Vec<Reminder>,
    /// Goals system: project canonical path → the session uid of that project's live
    /// goals-orchestrator pane. Lets "New goal" route to the existing orchestrator (inject the
    /// goal) instead of spawning a duplicate. NOT persisted — orchestrator panes don't survive
    /// a relaunch; the entry is re-established on the next "New goal" for the project.
    pub goal_orchestrators: std::collections::HashMap<String, String>,
    /// Goals system: images attached to the in-progress New-goal dialog, held as file
    /// paths (clipboard images are captured to temp PNGs) until submit, when their paths are
    /// written into the goal prompt. Cleared on open and on submit.
    pub goal_draft_images: Vec<std::path::PathBuf>,
    /// New-goal box: the goal text — a controller-owned mirror the key router edits while the
    /// overlay is open (same pattern as `palette_query`; no focused Slint `TextInput`).
    pub goal_text: String,
    /// New-goal box: the ←/→-focused field (0 goal text · 1 project · 2 orch · 3 spec · 4 impl).
    pub goal_field: usize,
    /// New-goal box: whether the focused field's ↓-option list is open.
    pub goal_menu_open: bool,
    /// New-goal box: selected row in the open option list.
    pub goal_menu_sel: usize,
    /// New-goal box: selected project (index into `projects`).
    pub goal_proj_sel: usize,
    /// New-goal box: selected model per tier (orch/spec/impl; indices into
    /// [`crate::command::GOAL_MODELS`]). Defaults opus/opus/sonnet.
    pub goal_model_sel: [usize; 3],
    /// Past goal submissions (most recent first) — the goal field's ↓-options. Loaded from
    /// disk on open, appended + saved on submit.
    pub goal_history: Vec<GoalHistoryEntry>,
    /// Bumped when the New-goal box opens, to (re)focus the box's OWN `FocusScope` (which
    /// captures every key and forwards it to `goal_key`). Projected into `goal-focus-tick`; the
    /// dialog's `changed focus-tick` calls `keyscope.focus()`. This is what makes the box
    /// keyboard-live regardless of how it was opened (palette, menu, mouse, boot scaffold) — the
    /// terminal `FocusScope` can't be grabbed reliably on demand while the overlay covers it.
    pub goal_focus_seq: i32,
    /// Bumped when the controller sets the goal text itself (a history pick) so the box's
    /// TextInput — which is otherwise the source of truth for typing — takes the new value.
    /// Projected into `goal-settext-tick` (with `goal_text` as `goal-settext-value`); the
    /// dialog's `changed settext-tick` assigns `gi.text`.
    pub goal_settext_seq: i32,
    /// New-goal box: whether the option chips (project + model tiers) are revealed. Hidden by
    /// default — the box is just the text field — and toggled by Ctrl+O (or Tab from the text
    /// field). While open, the focused chip's dropdown is always shown and Up/Down apply live.
    pub goal_options_open: bool,
    /// Goals system: goals queued for robust delivery into their orchestrator panes (drained by
    /// the app tick once each pane's Claude is ready). See [`PendingGoal`].
    pub pending_goals: Vec<PendingGoal>,
    /// Goals system: round-robin cursor over the Claude-account registry, advanced each time a
    /// fresh orchestrator is spawned so consecutive goal orgs start on different accounts.
    pub goal_account_cursor: usize,
    /// Whether the sidebar bell's reminder-list panel is expanded.
    pub reminders_open: bool,
    /// uid → the registry id of a tool a **plain terminal** pane was caught running, from
    /// the OSC-title sniff (`glow::sniff_osc_title`). Deliberately NOT `PaneState.kind`.
    ///
    /// A sniff is an inference, and the plan's detection precedence (D5) says an inference
    /// may upgrade a pane's *chrome* but must never touch what the pane relaunches as. Kept
    /// out here, that rule holds by construction rather than by everyone remembering it:
    /// `kind` stays the explicit/derived answer that persistence writes, and the only place
    /// the two are combined is the projection into the UI (`paneview::effective_kind`).
    /// Runtime-only — never serialized, re-learned the moment the tool prints its title again.
    pub sniffed_tool: std::collections::HashMap<String, String>,
    /// uid → the program's own last self-reported liveness (`OSC 9;hp;state=…`). Cleared
    /// when the command ends or the shell returns to a prompt, because a stale "busy" badge
    /// is worse than none. Runtime-only, same as `sniffed_tool`.
    pub agent_live: std::collections::HashMap<String, AgentLiveness>,
}

/// A pane parked by "Remind at…" — the detached pane (its session alive centrally) plus
/// when it's due back. `fired` flips once the due time passes (set by the app tick) and
/// drives the bell/list highlight; v1 never auto-restores.
#[derive(Debug, Clone)]
pub struct Reminder {
    pub pane: DetachedPane,
    /// Due time in UNIX-epoch ms (compared against `glow::now_epoch_ms` by the tick).
    pub due_ms: u64,
    /// Human due label computed at set time from the LOCAL clock ("14:32" / "tomorrow 09:00").
    pub due_label: SharedString,
    pub fired: bool,
    /// When the reminder fired, in epoch ms (0 = not fired yet). Anchors the alert toast's
    /// auto-expiry window — the toast shows from fire until `REMINDER_TOAST_MS` later.
    pub fired_at_ms: u64,
    /// The alert toast was clicked away (or aged out). The bell badge/list highlight is
    /// untouched by this — only the transient toast honours it.
    pub toast_dismissed: bool,
}

/// How long a fired reminder's alert toast stays up unclicked, in ms. Deliberately much
/// longer than the in-pane copy/paste toast (~1.3s) — a fired reminder is an alert, not a
/// confirmation — but it still ages out so a wall of stale toasts can't pile up overnight.
pub const REMINDER_TOAST_MS: u64 = 10_000;

/// The pane menu's reminder offsets — the four quick picks plus the flyout's Custom input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderOffset {
    Min15,
    Hour1,
    Hour3,
    Tomorrow9,
    /// Minutes from now (1..=1440), parsed by `contextmenu::parse_custom_duration`.
    Custom(u32),
}

/// What the key router should do with an Escape press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscOutcome {
    /// A tap — send Escape to the focused shell.
    Forward,
    /// Held in fullscreen — leave fullscreen (and don't forward).
    Exit,
    /// An auto-repeat tail — swallow (so a hold doesn't spam the shell).
    Ignore,
}

impl State {
    /// Fresh state with a single empty tab; the caller seeds pane 0 via [`Self::add_pane`].
    #[tracing::instrument(level = "debug", skip(font))]
    pub fn new(font: avada_terminal_widget::Font) -> Self {
        // Module keymap overrides live in their own file and their own store, so they are
        // loaded here rather than inside `Keymap::load` — a module surface can be rebound
        // long before (or long after) the module that owns it is ever spawned.
        crate::module_ui::grid::load_prefs();
        let mut s = State {
            font,
            last_scale: 1.0,
            tabs: Vec::new(),
            active: 0,
            tab_seq: 0,
            hyperpane_retry_at: None,
            fullscreen: false,
            editing_tab: -1,
            editing_pane: -1,
            kbd_focus_on: None,
            kbd_focus_live: None,
            kbd_focus_tries: 0,
            refocus_clock: 0,
            last_blink: Instant::now(),
            cursor_on: true,
            frames: 0,
            last_hud: Instant::now(),
            dirty: true,
            overlay: Overlay::None,
            add_project_error: String::new(),
            ask_url: String::new(),
            ask_browsers: Vec::new(),
            settings: prefs::load(),
            keymap: crate::keybindings::Keymap::load(),
            capturing_binding: None,
            capture_conflict: None,
            // Apply the saved font family/size on the first pump (it owns the scale).
            font_reload: true,
            prefs_draft: None,
            prefs_confirm: false,
            font_custom: false,
            // Sized to the preview frame: the Tetris board (20 cols × 26 rows) plus a 2-row
            // HUD above and below (the font preview). See `preview_frame`.
            preview_pane: TerminalPane::new(20, 30, Box::new(SoftwareRenderer::new())),
            preview_font: None,
            preview_key: (String::new(), 0.0, 0.0),
            preview_theme: -1,
            preview_cursor: false,
            preview_surface: Image::default(),
            preview_glow: Glow::new(0x9E37_79B9_7F4A_7C15),
            preview_tetris: crate::tetris::Tetris::new(0x5DEE_CE2F_1234_ABCD),
            preview_tetris_last: Instant::now(),
            // Seed the rail's badge with the remembered projects up front (so the count
            // is right before any pane reports a cwd).
            projects: sidebar::list(),
            goal_orchestrators: std::collections::HashMap::new(),
            goal_draft_images: Vec::new(),
            goal_text: String::new(),
            goal_field: 0,
            goal_menu_open: false,
            goal_menu_sel: 0,
            goal_proj_sel: 0,
            goal_model_sel: GOAL_MODEL_SEL_DEFAULT,
            goal_history: Vec::new(),
            goal_focus_seq: 0,
            goal_settext_seq: 0,
            goal_options_open: false,
            pending_goals: Vec::new(),
            goal_account_cursor: 0,
            sidebar_open: false,
            left_panel_open: false,
            left_panel_beat: None,
            workspace_path: None,
            palette_entries: Vec::new(),
            palette_view: Vec::new(),
            palette_sel: 0,
            palette_query: String::new(),
            esc_last: None,
            esc_hold_start: None,
            esc_holding: false,
            esc_fired: false,
            ctx: None,
            project_root: None,
            project_root_from: None,
            rail: Default::default(),
            rail_requests: Vec::new(),
            rail_scroll_hold: None,
            module_events: Vec::new(),
            opener_panes: Vec::new(),
            module_openers: Vec::new(),
            rights: crate::prefs::rights::shared().clone(),
            rights_selected: None,
            rights_effects: Vec::new(),
            closed: Vec::new(),
            closed_open: false,
            pending_close: None,
            reminders: Vec::new(),
            reminders_open: false,
            sniffed_tool: std::collections::HashMap::new(),
            agent_live: std::collections::HashMap::new(),
        };
        let tab = s.fresh_tab();
        s.tabs.push(tab);
        // Seed the preview with the first composed frame so it's never blank before the
        // first animation tick (the pump advances + re-feeds it while Preferences is open).
        let frame = s.preview_frame();
        s.preview_pane.feed(&frame);
        s
    }

    /// Render the appearance preview (a real, locked terminal) with the drafted font + theme,
    /// returning the freshly-rendered image when anything changed (else `None`). Called by the
    /// pump while Preferences is open; `scale` is the window DPI scale.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn render_preview(&mut self, scale: f32, cursor_on: bool) -> Option<Image> {
        let (font_path, px, theme_idx) = match &self.prefs_draft {
            Some(d) => (
                prefs::resolve_or_default(&d.font_family),
                d.font_px,
                d.terminal_theme,
            ),
            None => (
                self.settings.font_path(),
                self.settings.font_px,
                self.settings.terminal_theme,
            ),
        };
        let key = (font_path.clone(), px, scale);
        let mut changed = false;
        if self.preview_font.is_none() || self.preview_key != key {
            self.preview_font = Some(theme::load_font_at(&font_path, px, scale));
            self.preview_key = key;
            changed = true;
        }
        if self.preview_theme != theme_idx as i32 {
            self.preview_pane
                .set_palette(theme::terminal_theme(theme_idx));
            self.preview_theme = theme_idx as i32;
            changed = true;
        }
        // Locked (no pty), but the caret still blinks like a real terminal.
        if self.preview_cursor != cursor_on {
            self.preview_cursor = cursor_on;
            changed = true;
        }
        if changed || self.preview_pane.take_dirty() {
            let font = self.preview_font.as_mut().unwrap();
            self.preview_surface = self.preview_pane.render(font, &RenderOpts { cursor_on });
            Some(self.preview_surface.clone())
        } else {
            None
        }
    }

    /// Advance the preview's ambient Tetris on its fixed cadence, feeding the new composed
    /// frame into the locked preview terminal so it animates. Called by the pump while
    /// Preferences is open; cheap no-op between frames.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn animate_preview_tetris(&mut self) {
        if self.preview_tetris_last.elapsed() >= std::time::Duration::from_millis(90) {
            self.preview_tetris.step();
            let frame = self.preview_frame();
            self.preview_pane.feed(&frame);
            self.preview_tetris_last = Instant::now();
        }
    }

    /// Compose the full preview frame: a 2-row Tetris HUD (score / level / lines), the board
    /// coloured from the drafted frame palette, then a 2-row HUD (NEXT swatch + the font name
    /// at the drafted size — so the font family/size still preview live). Reflects the
    /// appearance DRAFT while the dialog is open (else the committed settings).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn preview_frame(&self) -> String {
        let (palette_idx, px, font_value) = match &self.prefs_draft {
            Some(d) => (d.frame_palette, d.font_px, d.font_family.as_str()),
            None => (
                self.settings.frame_palette,
                self.settings.font_px,
                self.settings.font_family.as_str(),
            ),
        };
        let colors = theme::frame_palette(palette_idx);
        let (ar, ag, ab) = colors[0]; // accent = the palette's first slot
        let t = &self.preview_tetris;
        let board = t.render(colors);
        let (nr, ng, nb) = colors[t.next_kind() % colors.len()];
        let label = prefs::font_label(font_value);

        let accent = format!("\x1b[38;2;{};{};{}m", ar, ag, ab);
        let dim = "\x1b[2m";
        let reset = "\x1b[0m";
        let trunc = |s: &str, n: usize| s.chars().take(n).collect::<String>();

        let mut s = String::with_capacity(2400);
        s.push_str("\x1b[H\x1b[?25l"); // home + hide cursor (it's an animation, not a prompt)
                                       // top HUD: score (accent) + level/lines
        s.push_str(&accent);
        s.push_str(&format!("SCORE {:06}", t.score()));
        s.push_str(reset);
        s.push_str("\x1b[K\r\n");
        s.push_str(dim);
        s.push_str(&format!("LEVEL {}  LINES {}", t.level(), t.lines()));
        s.push_str(reset);
        s.push_str("\x1b[K\r\n");
        // the palette-coloured board (H rows)
        s.push_str(&board);
        s.push_str("\r\n");
        // bottom HUD: the NEXT piece (letter + swatch, in its colour), then font name + size
        s.push_str(dim);
        s.push_str("NEXT ");
        s.push_str(reset);
        s.push_str(&format!(
            "\x1b[38;2;{};{};{}m{} \u{2588}\u{2588}\x1b[0m",
            nr,
            ng,
            nb,
            t.next_letter()
        ));
        s.push_str("\x1b[K\r\n");
        s.push_str(dim);
        s.push_str(&format!("{}  {}px", trunc(label, 13), px as i32));
        s.push_str(reset);
        s.push_str("\x1b[K"); // last line: no trailing newline → never scrolls the grid
        s
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn fresh_tab(&mut self) -> Tab {
        let title = self.fresh_tab_title();
        Tab::empty(title)
    }

    /// The next unused default tab name.
    ///
    /// `tab_seq` alone is not enough and never was: it is a per-`State` counter that starts
    /// at 0 and is not persisted, so a window restored with a `term 2` tab in it hands out
    /// `term 1`, then `term 2` — a second tab with a name already on the strip. Skipping the
    /// names that are actually taken makes the counter a hint rather than the answer, which
    /// is all it can be once titles survive a restart and the user can rename freely.
    ///
    /// Only the *generated* name is deduped. A title the user typed, or one a workspace file
    /// names explicitly, is left exactly as written — silently renaming what someone asked
    /// for is worse than letting two tabs share a name on purpose.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn fresh_tab_title(&mut self) -> SharedString {
        loop {
            self.tab_seq += 1;
            let name = format!("term {}", self.tab_seq);
            if !self.tabs.iter().any(|t| t.title.as_str() == name) {
                return name.into();
            }
        }
    }

    #[tracing::instrument(level = "debug", skip(self))]
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Locate the (tab, pane) holding session `uid` across *all* tabs (events for
    /// background tabs still need to reach their pane).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn find_pane(&mut self, uid: &str) -> Option<(usize, usize)> {
        for (ti, t) in self.tabs.iter().enumerate() {
            if let Some(pi) = t.panes.iter().position(|p| p.uid == uid) {
                return Some((ti, pi));
            }
        }
        None
    }

    /// Spawn the pane's pty session on a worker thread. On Windows, CreateProcessW into a
    /// ConPTY blocks ~1s for some shells (pwsh 7 measured 1.0–1.1s EVERY spawn; see
    /// docs/conpty-passthrough-investigation.md) — running it inline froze startup and
    /// every split/new-pane/restore for that long. The pane's grid is built immediately
    /// and output starts flowing whenever the shell is ready (exactly how other terminals
    /// behave: window first, prompt when the shell is up). Completion is reported via
    /// [`spawn_done`], which `App::tick` drains to (a) force a geometry re-apply — the
    /// pump may have resized the pane while its session didn't exist, and a resize on a
    /// missing uid is a silent no-op — and (b) kill the session if its pane was closed
    /// mid-spawn (otherwise it would leak). A spawn error just logs: the pane shows a
    /// dead shell, and a restart re-tries.
    #[tracing::instrument(level = "debug", skip_all)]
    fn spawn_session_async(mgr: &SessionManager, opts: SpawnOptions) {
        let mgr = mgr.clone();
        let uid = opts.uid.clone();
        // Captured on the UI thread (inside the app's tokio runtime guard); the worker
        // re-enters it so `create`'s tokio::spawn of the driver task has a runtime.
        let rt = tokio::runtime::Handle::current();
        let res = std::thread::Builder::new()
            .name("hp-pane-spawn".into())
            .spawn(move || {
                let _guard = rt.enter();
                if let Err(e) = mgr.create(opts) {
                    tracing::error!("failed to spawn {uid}: {e}");
                }
                spawn_done().lock().unwrap().push(uid);
            });
        if let Err(e) = res {
            tracing::error!("failed to start spawn thread: {e}");
        }
    }

    /// Best-known cell size for spawning a NEW pane's pty (ConPTY Option C: keep the grid at
    /// the pane's actual visible cells, never larger — conhost's scroll repaint cost is
    /// proportional to rows, and every `ResizePseudoConsole` triggers a full-grid re-render
    /// on the in-box host, so spawning close to the final size skips one of those plus the
    /// 80-col prompt rewrap). The focused pane's laid-out size is the best predictor of a
    /// sibling's tile (exact for `restart_pane`, ≤1 pump tick off after a split re-tiles);
    /// before any layout exists (eager first-pane seed, workspace restore into a fresh
    /// window) there is nothing to predict from, so fall back to the classic 80×24 — rows
    /// only ever grow from there, and the first render pump corrects it.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn spawn_cells(&self) -> (u16, u16) {
        let tab = self.active_tab();
        match tab
            .panes
            .get(tab.focused.min(tab.panes.len().saturating_sub(1)))
        {
            // `applied` starts as the spawn default and only becomes meaningful once the
            // pump has laid the pane out (rect set) — don't propagate a pre-layout value.
            Some(p) if p.rect.2 > 0.0 => (
                (p.applied.0.clamp(2, 500)) as u16,
                (p.applied.1.clamp(1, 300)) as u16,
            ),
            _ => (80, 24),
        }
    }

    /// Decide where a pane's shell starts, in one place, in a fixed order.
    ///
    /// A terminal that opens in `$HOME` when the app was started in a project is the single
    /// most-reported papercut here, and it happened because nobody DECIDED: `SpawnOptions.cwd`
    /// was left `None` and [`avada_core::session_manager`]'s pty-layer safety net picked
    /// `$HOME` — which is the right last resort but the wrong first answer. The order below is
    /// the intent, most-specific first:
    ///
    /// 1. `explicit` — a cwd the caller already knows (the New-pane dialog, a workspace spec, a
    ///    restored pane). It is never second-guessed: a restored pane must come back where it
    ///    was, not where the window happens to be pointed now.
    /// 2. the focused pane's live cwd in this tab — "open another pane HERE" is what splitting
    ///    a terminal means, and the focused pane is the one the human is looking at.
    /// 3. the project the left panel is anchored to ([`State::project_root`] — the same anchor
    ///    the GIT view draws, deliberately reused so there is ONE notion of "this window's
    ///    project" rather than a second one that can disagree with what is on screen).
    /// 4. the directory the process was launched from ([`launch_dir`]), so `cd project &&
    ///    avada` opens in `project`.
    /// 5. `$HOME`, and finally `None` — at which point the pty layer's own fallback runs.
    ///
    /// Every candidate is stat'd by [`first_existing_dir`]; a stale one is skipped rather than
    /// handed to the pty, where a failed `chdir` kills the shell with no visible error.
    ///
    /// `kind` is not decoration. A NON-pty pane's `cwd` is not a working directory at all — it
    /// is the FILE the viewer or the markdown renderer is pointed at (`/tmp/notes.txt`), and it
    /// is what the snapshot round-trips as that pane's target. Running it through the directory
    /// stat below would reject it and silently retarget the pane at whatever directory the
    /// focused terminal happens to be sitting in, so a restored workspace came back showing the
    /// wrong file. Nothing chdirs for a view pane, so there is no pty to protect: its target is
    /// taken exactly as given.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn resolve_new_pane_cwd(&self, kind: &PaneKind, explicit: Option<String>) -> Option<String> {
        let explicit = explicit.filter(|c| !c.is_empty());
        if !kind.is_pty() {
            return explicit;
        }
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok());
        first_existing_dir(vec![
            explicit,
            self.focused_cwd(),
            self.project_root
                .as_ref()
                .map(|r| r.to_string_lossy().into_owned()),
            launch_dir(),
            home,
        ])
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn make_pane(
        &mut self,
        mgr: &SessionManager,
        idx: usize,
        opts: NewPaneOpts,
    ) -> Option<PaneState> {
        let palette = self.settings.frame_palette;
        let accent = opts.accent;
        // A command to run instead of an interactive shell ("" → interactive).
        let command = opts.command.filter(|c| !c.is_empty());
        // What this pane IS is decided before anything else, because it decides three
        // questions below: which uid scheme, whether a pty is spawned at all (D3), and
        // whether `cwd` is a directory to start a shell in or a file to point a viewer at.
        // An explicit kind wins; otherwise the program we were asked to run names it.
        let kind = opts.kind.clone().unwrap_or_else(|| {
            command
                .as_deref()
                .map(PaneKind::for_command)
                .unwrap_or_default()
        });
        // Resolved once, here, so the pane's own record and the pty spawn below agree on
        // where it started (the relaunch snapshot reads it back off the pane).
        let cwd = self.resolve_new_pane_cwd(&kind, opts.cwd);
        // The conversation this pane is being opened into, when the caller knew one. Built
        // here, before `cwd` is moved into the pane, because the mark is the id AND the
        // directory it belongs to — every provider-backed tool keys resume off both.
        let tool_session = opts
            .session
            .as_deref()
            .zip(cwd.as_deref())
            .and_then(|(id, dir)| ToolSessionMark::new(id, dir));
        // Mint via the backend so a daemon-backed pane gets a cross-run-unique uid (it may
        // outlive this GUI run and be re-attached by uid next launch); in-process keeps the
        // `pane-N` form. See `SessionManager::fresh_uid` / the plan's "uid stability".
        // A view pane has no session to identify, so it mints locally instead.
        let uid = if kind.is_pty() {
            mgr.fresh_uid()
        } else {
            fresh_view_uid()
        };
        // Shell: an explicit pick from the dialog is used verbatim; otherwise honour the
        // default-shell preference ("" = prefer pwsh when available, else core's default).
        let shell = match opts.shell {
            Some(s) if !s.is_empty() => Some(s),
            _ => prefs::effective_shell(&self.settings.default_shell),
        };
        // Inject shell integration so the shell reports its cwd (OSC-7 for pwsh/bash, OSC
        // 9;9 for cmd). That's what lets a pane's cwd → git-project tint (and clickable-path
        // resolution) actually fire — without it pwsh never emits a cwd OSC. Additive: the
        // resolved shell is classified, and the init script must be deployed next to the
        // binary (build.rs in dev, packaging for release), else this is simply `None`.
        let shell_path = shell
            .clone()
            .unwrap_or_else(avada_core::session::spawn::default_shell);
        // Integration applies to the interactive branch only; a one-off `command` pane is
        // not an interactive shell, so skip it there (core would ignore it anyway).
        let integration = command
            .is_none()
            .then(|| {
                avada_core::shell_integration::integration_for(
                    &shell_path,
                    &avada_core::shell_integration::shell_integration_dir(),
                )
                .map(|si| avada_core::session_manager::Integration {
                    args: si.args,
                    env: si.env.into_iter().collect(),
                })
            })
            .flatten();
        let (cols, rows) = self.spawn_cells();
        // A view pane renders into the same `surface` waist with nothing behind it — no pty,
        // no backend registration, so no phantom session for the daemon to answer for (D3).
        if kind.is_pty() {
            Self::spawn_session_async(
                mgr,
                SpawnOptions {
                    uid: uid.clone(),
                    cols: Some(cols),
                    rows: Some(rows),
                    pane_id: Some(uid.clone()),
                    // Cloned for the same reason as `shell` below: a view pane keeps its own
                    // copy on the PaneState.
                    cwd: cwd.clone(),
                    // Cloned so the resolved spawn spec is also kept on the PaneState (below) for
                    // the relaunch snapshot.
                    shell: shell.clone(),
                    command: command.clone(),
                    // Argv for a direct exec: a caller (a module's terminal pane) that gave an
                    // arg vector runs `command` + these with no shell in between. Cloned so the
                    // same spec is kept on the PaneState below for the relaunch snapshot.
                    args: opts.args.clone(),
                    // `BROWSER` points at our shim so a link a tool in this pane wants to
                    // show goes back through `App::route_event` → `Command::OpenLink` (and
                    // thus Preferences → Browser) instead of straight to the OS. Wrapped
                    // here and not where the env is recorded on the pane: the shim path is
                    // regenerated every run, so it must not be baked into a snapshot.
                    env: avada_core::open::with_browser_shim(opts.env.clone()),
                    integration,
                    ..Default::default()
                },
            );
        }
        let mut pane = TerminalPane::with_scrollback(
            cols as usize,
            rows as usize,
            self.settings.scrollback as usize,
            Box::new(SoftwareRenderer::new()),
        );
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        pane.set_project_roots(self.project_roots());
        let glow = Glow::new(crate::glow::seed_from(&uid));
        // A pane spawned WITH an accent is a project/dialog pane: by default tint it on. A
        // plain new pane is clean — it still gets a palette color VALUE by slot, but its
        // frame/dot overrides default OFF (mirrors `addPane`). The New Pane dialog passes
        // explicit `show_frame`/`show_dot` (both default off — a fresh pane is clean) which
        // win over this default.
        let project = accent.is_some();
        let show_frame = opts.show_frame.unwrap_or(project);
        let show_dot = opts.show_dot.unwrap_or(project);
        let label = match opts.label {
            Some(l) if !l.trim().is_empty() => l,
            _ if idx == 0 => "shell".to_string(),
            _ => format!("pane {}", idx + 1),
        };
        // Each pane owns its font (per-pane zoom); start at the configured base size.
        let font_px = self.settings.font_px;
        let font = theme::load_font_at(&self.settings.font_path(), font_px, self.last_scale);
        tracing::info!(uid = %uid, label = %label, "pane spawned");
        Some(PaneState {
            uid,
            title: label.into(),
            subtitle: None,
            show_frame: Some(show_frame),
            show_dot: Some(show_dot),
            accent: accent.unwrap_or_else(|| theme::accent_for(idx, palette)),
            pane,
            applied: (cols as usize, rows as usize),
            pty: (cols as usize, rows as usize),
            pty_since: std::time::Instant::now(),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: false,
            startup: opts.startup.clone(),
            pinned_accent: accent,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            talk: false,
            recording: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            scrollbar_on: false,
            search_focus_seq: 0,
            refocus_seq: 0,
            font_px,
            font,
            font_dirty: false,
            // A pty pane starts with an unknown cwd and learns its live one from the shell's
            // OSC 7. A view pane has no shell to ask, and its target IS its cwd (see
            // `State::view_navigate`) — so it has to be seeded here, or the browser opens on
            // "No path set for this pane" no matter what the caller passed.
            cwd: (!kind.is_pty()).then_some(cwd).flatten(),
            env: opts.env,
            // The resolved shell program → its short header badge (computed once here).
            // Only a pty pane gets one: a file browser never spawned a shell, and a header
            // reading "notes.md  zsh" claims a process that does not exist.
            shell_label: if kind.is_pty() {
                shell_label(&shell_path)
            } else {
                String::new()
            },
            // Remember the spawn spec so the relaunch snapshot can re-run this program. A New
            // Pane dialog carries no argv, so `spawn_args` stays None there; a module's terminal
            // pane does carry one, and it round-trips so a relaunch re-execs the same argv. A
            // view pane never ran a program, so it records none — the snapshot restores it from
            // its `kind` alone.
            spawn_command: kind.is_pty().then_some(command).flatten(),
            spawn_args: kind.is_pty().then_some(opts.args).flatten(),
            spawn_shell: kind.is_pty().then_some(shell).flatten(),
            kind,
            tool_session,
        })
    }

    // ---- pane mutations (act on the active tab) ----

    /// Spawn a new pane + shell in the active tab and focus it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn add_pane(&mut self, mgr: &SessionManager) {
        self.add_pane_cwd(mgr, None, None);
    }

    /// Spawn a new pane in the active tab with an optional working directory + accent
    /// (used to open a sidebar project cd'd into its repo), and focus it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn add_pane_cwd(
        &mut self,
        mgr: &SessionManager,
        cwd: Option<String>,
        accent: Option<Color>,
    ) {
        self.add_pane_opts(
            mgr,
            NewPaneOpts {
                cwd,
                accent,
                ..Default::default()
            },
        );
    }

    /// Spawn a new pane in the active tab from the full [`NewPaneOpts`] (the New Pane
    /// dialog's payload), and focus it.
    /// Spawn a configured pane in the active tab. Returns the new pane's session `uid`
    /// (`None` if the spawn failed), so callers that need to address the pane later — e.g.
    /// the goals launcher registering its orchestrator — can record it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn add_pane_opts(&mut self, mgr: &SessionManager, opts: NewPaneOpts) -> Option<String> {
        let idx = self.active_tab().panes.len();
        let ps = self.make_pane(mgr, idx, opts)?;
        let uid = ps.uid.clone();
        let auto = self.active_tab().layout == Layout::Auto;
        let t = self.active_tab_mut();
        t.sizes = if auto {
            equal_sizes(idx + 1)
        } else {
            insert_size(&t.sizes, idx)
        };
        t.panes.push(ps);
        t.focused = idx;
        t.zoomed = None;
        self.dirty = true;
        Some(uid)
    }

    /// Remove pane `idx` of tab `ti` **without** killing its session, returning the
    /// removed [`PaneState`] and whether the window still has panes (`false` = the
    /// workspace emptied → the caller should close the window). An emptied non-last tab
    /// is dropped. Shared by [`Self::close_pane_in`] (which then kills the session) and
    /// pane re-host (which keeps the session alive to rebind it in another window).
    #[tracing::instrument(level = "debug", skip(self))]
    fn take_pane_in(&mut self, ti: usize, idx: usize) -> Option<(PaneState, bool)> {
        let taken = self.take_pane_inner(ti, idx);
        if let Some((ps, _)) = taken.as_ref() {
            // The pane is leaving this window (closed, or detached for re-host). Its
            // runtime-only facts are re-learned from the next title/state marker wherever
            // it lands, so dropping them here is what keeps the maps bounded.
            self.forget_pane_runtime(&ps.uid);
        }
        taken
    }

    /// The removal itself — see [`Self::take_pane_in`], which wraps this to drop the
    /// pane's runtime-only side-map entries on every exit path.
    #[tracing::instrument(level = "debug", skip(self))]
    fn take_pane_inner(&mut self, ti: usize, idx: usize) -> Option<(PaneState, bool)> {
        if ti >= self.tabs.len() {
            return None;
        }
        let palette = self.settings.frame_palette;
        let t = &mut self.tabs[ti];
        if idx >= t.panes.len() {
            return None;
        }
        let ps = t.panes.remove(idx);
        let auto = t.layout == Layout::Auto;
        t.sizes = if auto {
            equal_sizes(t.panes.len())
        } else {
            remove_size(&t.sizes, idx)
        };
        self.dirty = true;
        if t.panes.is_empty() {
            if t.system {
                // The system tab (the always-on "Hyperpane") outlives its panes: it is never
                // dropped and never quits the window. It is left EMPTY here — this path has
                // no `SessionManager` to spawn with — and refilled by
                // [`Self::reseed_system_tab`], which every caller that owns a manager runs
                // right after the take, and which the app's tick runs as a backstop for the
                // callers that don't (cross-window drag, reminders).
                t.focused = 0;
                t.zoomed = None;
                t.sizes.clear();
                self.editing_tab = -1;
                return Some((ps, true));
            }
            if self.tabs.len() <= 1 {
                // Last pane of the last tab → workspace emptied. Leave the empty tab in
                // place (the window is about to close).
                return Some((ps, false));
            }
            // Drop the now-empty tab and fix the active index.
            self.tabs.remove(ti);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if ti < self.active {
                self.active -= 1;
            }
            self.editing_tab = -1;
            return Some((ps, true));
        }
        let t = &mut self.tabs[ti];
        if t.focused >= t.panes.len() {
            t.focused = t.panes.len() - 1;
        } else if idx < t.focused {
            t.focused -= 1;
        }
        t.zoomed = match t.zoomed {
            Some(z) if z == idx => None,
            Some(z) if z > idx => Some(z - 1),
            other => other,
        };
        t.relabel(palette);
        Some((ps, true))
    }

    /// Close pane `idx` of tab `ti`, killing its session. An emptied tab is
    /// dropped; closing the last pane of the last tab returns `false` (caller
    /// quits). Works for background tabs too (used by self-exiting shells).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn close_pane_in(&mut self, ti: usize, idx: usize, mgr: &SessionManager) -> bool {
        match self.take_pane_in(ti, idx) {
            Some((ps, alive)) => {
                tracing::info!(uid = %ps.uid, tab = ti, "pane closed");
                kill_session_of(mgr, &ps.uid, &ps.kind);
                // Closing the system tab's last pane leaves the tab, not the window: it gets a
                // fresh shell in the Hyperpane directory (the user closed the agent on purpose,
                // so it is not relaunched behind their back — they can start it by hand).
                self.reseed_system_tab(mgr);
                alive
            }
            None => true,
        }
    }

    /// Close pane `idx` of tab `ti` **reopenably**: park it on the recently-closed history with
    /// its session alive, so the × is undoable instead of fatal. Returns whether the window
    /// still has panes.
    ///
    /// The last pane of the last tab falls through to the real [`Self::close_pane_in`]: there
    /// is nowhere to park it: the history lives in this `State` and dies with the window, so
    /// parking would leak the PTY rather than preserve it. That close is the one genuinely
    /// irreversible one, which is why it is also the one always confirmed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn close_pane_menu(&mut self, ti: usize, idx: usize, mgr: &SessionManager) -> bool {
        if self.ends_window_pane(ti, idx) {
            return self.close_pane_in(ti, idx, mgr);
        }
        match self.detach_pane_in(ti, idx) {
            Some(det) => {
                self.push_closed(ClosedWhat::Pane(Box::new(det)), mgr);
                // Parking the system tab's last pane keeps the tab (see `take_pane_inner`);
                // it is refilled so the strip never shows an empty Hyperpane.
                self.reseed_system_tab(mgr);
                true
            }
            None => true,
        }
    }

    /// Whether closing pane `idx` of tab `ti` empties the whole window (the only pane of the
    /// only tab) — the close that quits, and cannot be parked.
    ///
    /// Never true for the system tab: its last pane is replaced, not fatal (see
    /// [`Self::take_pane_inner`]), so the window does not end and the close is parkable.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn ends_window_pane(&self, ti: usize, idx: usize) -> bool {
        self.tabs.len() <= 1
            && self
                .tabs
                .get(ti)
                .is_some_and(|t| !t.system && t.panes.len() <= 1 && idx < t.panes.len())
    }

    /// Detach the focused pane of the active tab for re-hosting in another window:
    /// remove it **without** killing its session (the PTY stays alive centrally),
    /// returning the rebind info + whether this window still has panes. `None` when the
    /// active tab has no panes.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn detach_focused(&mut self, mgr: &SessionManager) -> Option<(DetachedPane, bool)> {
        // Sessions are NOT touched here — that's the whole point of detach. The manager is
        // only for the reseed: a pane torn out of the system tab leaves the tab behind, and
        // the tab gets a fresh shell so it is never an empty Hyperpane.
        let ti = self.active;
        let idx = self.tabs.get(ti)?.focused;
        let (ps, alive) = self.take_pane_in(ti, idx)?;
        self.reseed_system_tab(mgr);
        Some((
            DetachedPane {
                uid: ps.uid,
                title: ps.title,
                subtitle: ps.subtitle,
                pinned_accent: ps.pinned_accent,
                show_frame: ps.show_frame,
                show_dot: ps.show_dot,
                font_px: ps.font_px,
                spawn_command: ps.spawn_command,
                spawn_args: ps.spawn_args,
                spawn_shell: ps.spawn_shell,
                kind: ps.kind,
                tool_session: ps.tool_session,
                cwd: ps.cwd,
            },
            alive,
        ))
    }

    /// Re-host a detached session at the end of the active tab (see [`Self::adopt_pane_at`]).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn adopt_pane(&mut self, mgr: &SessionManager, det: DetachedPane) {
        let at = self.active_tab().panes.len();
        self.adopt_pane_at(mgr, det, at);
    }

    /// Re-host a detached session in the active tab at insertion index `at`: build a fresh
    /// terminal grid, prime it from the session's **replay buffer** (recent scrollback — so
    /// no blank pane and no PTY restart), rebind it to the existing `uid`, and focus it.
    /// `at` is clamped to `0..=len`, so a stitch can insert the pane at a hovered slot.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn adopt_pane_at(&mut self, mgr: &SessionManager, det: DetachedPane, at: usize) {
        let palette = self.settings.frame_palette;
        let (cols, rows) = (80u16, 24u16);
        let mut pane = TerminalPane::with_scrollback(
            cols as usize,
            rows as usize,
            self.settings.scrollback as usize,
            Box::new(SoftwareRenderer::new()),
        );
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        pane.set_project_roots(self.project_roots());
        // Replay the rolling buffer so the re-hosted pane shows recent output instantly.
        if let Some(replay) = mgr.replay(&det.uid) {
            pane.feed(&replay);
        }
        let glow = Glow::new(crate::glow::seed_from(&det.uid));
        let font = theme::load_font_at(&self.settings.font_path(), det.font_px, self.last_scale);
        let ps = PaneState {
            uid: det.uid,
            title: det.title,
            subtitle: det.subtitle,
            show_frame: det.show_frame,
            show_dot: det.show_dot,
            accent: det
                .pinned_accent
                .unwrap_or_else(|| theme::accent_for(at, palette)),
            pane,
            applied: (cols as usize, rows as usize),
            pty: (cols as usize, rows as usize),
            pty_since: std::time::Instant::now(),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: true, // the session is already running — don't re-send any startup.
            startup: None,
            pinned_accent: det.pinned_accent,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            talk: false,
            recording: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            scrollbar_on: false,
            search_focus_seq: 0,
            refocus_seq: 0,
            font_px: det.font_px,
            font,
            font_dirty: false,
            // Carried, not reset: for a view pane this IS the file/directory it renders, and
            // for a terminal it is what the file tree follows.
            cwd: det.cwd,
            env: None,
            // A re-hosted session: its original spawn shell isn't tracked across the detach,
            // so the badge stays hidden ("") rather than guessing.
            shell_label: String::new(),
            // The spawn spec IS carried across the detach, so a relaunch snapshot of a
            // re-hosted pane still records its program.
            spawn_command: det.spawn_command,
            spawn_args: det.spawn_args,
            spawn_shell: det.spawn_shell,
            kind: det.kind,
            tool_session: det.tool_session,
        };
        let auto = self.active_tab().layout == Layout::Auto;
        let t = self.active_tab_mut();
        let at = at.min(t.panes.len());
        t.sizes = if auto {
            equal_sizes(t.panes.len() + 1)
        } else {
            insert_size(&t.sizes, at)
        };
        t.panes.insert(at, ps);
        t.focused = at;
        t.zoomed = None;
        t.relabel(palette);
        self.dirty = true;
    }

    /// Re-host a detached session as a **brand-new tab** (dock-as-tab on a tear-off drop):
    /// append a fresh tab, switch to it, and adopt the pane into it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn adopt_pane_as_tab(&mut self, mgr: &SessionManager, det: DetachedPane) {
        let tab = self.fresh_tab();
        tracing::info!(title = %tab.title, "tab created (pane adopted)");
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        self.adopt_pane(mgr, det);
    }

    /// Detach a **specific** pane (by `uid`) from wherever it lives (any tab) for re-hosting
    /// elsewhere — like [`Self::detach_focused`] but targets the dragged pane. Searching all
    /// tabs (not just the active one) keeps a drop correct even after the active tab changed
    /// mid-drag (e.g. a spring-load switched tabs). Returns the rebind info + whether this
    /// window still has panes; `None` if the uid isn't here. `take_pane_in` keeps the active
    /// tab pointing at the same tab across the removal.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn detach_uid(&mut self, uid: &str) -> Option<(DetachedPane, bool)> {
        let (ti, idx) = self.find_pane(uid)?;
        let (ps, alive) = self.take_pane_in(ti, idx)?;
        Some((
            DetachedPane {
                uid: ps.uid,
                title: ps.title,
                subtitle: ps.subtitle,
                pinned_accent: ps.pinned_accent,
                show_frame: ps.show_frame,
                show_dot: ps.show_dot,
                font_px: ps.font_px,
                spawn_command: ps.spawn_command,
                spawn_args: ps.spawn_args,
                spawn_shell: ps.spawn_shell,
                kind: ps.kind,
                tool_session: ps.tool_session,
                cwd: ps.cwd,
            },
            alive,
        ))
    }

    /// Whether the active tab currently hosts pane `uid` (used to choose reorder-in-place
    /// vs cross-tab move when a pane is dropped in the pane area).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn active_has_uid(&self, uid: &str) -> bool {
        self.active_tab().panes.iter().any(|p| p.uid == uid)
    }

    /// Move pane `from` to insertion index `to` within the active tab (in-window reorder),
    /// carrying its split size with it so the layout stays stable. Focus follows the moved
    /// pane. No-op when the move is a no-op or the indices are out of range.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reorder_pane(&mut self, from: usize, to: usize) {
        self.reorder_pane_in(self.active, from, to);
    }

    /// The general form of [`Self::reorder_pane`]: reorder within tab `ti`, which need not be
    /// the active one. The left panel's tree shows every tab at once, so a row can be dragged
    /// into a new position inside a group the window is not currently showing.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reorder_pane_in(&mut self, ti: usize, from: usize, to: usize) {
        if ti >= self.tabs.len() {
            return;
        }
        let palette = self.settings.frame_palette;
        let t = &mut self.tabs[ti];
        let n = t.panes.len();
        if from >= n || to > n {
            return;
        }
        // Translate the insertion index into the post-removal slot.
        let dest = if to > from { to - 1 } else { to };
        if dest == from {
            return;
        }
        let pane = t.panes.remove(from);
        t.panes.insert(dest, pane);
        if t.sizes.len() == n {
            let s = t.sizes.remove(from);
            t.sizes.insert(dest, s);
        }
        t.focused = dest;
        t.zoomed = match t.zoomed {
            Some(z) if z == from => Some(dest),
            _ => t.zoomed,
        };
        t.relabel(palette);
        self.dirty = true;
    }

    /// Keep the app-owned tab (the always-on "Hyperpane") pinned as tab #1.
    ///
    /// It is the tab every other tab is driven from, so it has a fixed home the same way a
    /// browser's pinned tab does: you should never have to hunt for it. Called after the tab
    /// is created and after a restore, because a workspace written before the pin existed can
    /// carry it anywhere in the strip.
    ///
    /// The active tab travels with the shuffle, so pinning never changes which tab you are
    /// looking at.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pin_system_tab_first(&mut self) {
        let Some(idx) = self.tabs.iter().position(|t| t.system) else {
            return;
        };
        if idx == 0 {
            return;
        }
        let tab = self.tabs.remove(idx);
        self.tabs.insert(0, tab);
        // Everything the system tab jumped over shifted one to the right.
        self.active = if self.active == idx {
            0
        } else if self.active < idx {
            self.active + 1
        } else {
            self.active
        };
        self.editing_tab = -1;
        self.dirty = true;
    }

    /// Move tab `from` to index `to` (in-strip tab reorder), keeping the same tab active.
    ///
    /// The app-owned tab is pinned first (see [`Self::pin_system_tab_first`]): it cannot be
    /// dragged out of slot 0, and nothing can be dropped in front of it. Both are refused
    /// silently — the drag simply doesn't take — rather than reordering and snapping back.
    ///
    /// Returns where the tab actually LANDED, which is not always `to`: a refused or clamped
    /// move ends somewhere else, and the live tab-drag tracks the dragged tab by index. Take
    /// the answer from here rather than assuming, or the rest of the drag moves the wrong tab.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reorder_tab(&mut self, from: usize, to: usize) -> usize {
        let n = self.tabs.len();
        if from >= n || to > n {
            return from;
        }
        if self.tabs[from].system {
            return from;
        }
        let mut dest = if to > from { to - 1 } else { to };
        if dest == 0 && self.tabs.first().is_some_and(|t| t.system) {
            dest = 1;
        }
        if dest == from {
            return from;
        }
        let active_title_idx = self.active;
        let tab = self.tabs.remove(from);
        self.tabs.insert(dest, tab);
        // Keep the previously-active tab active across the shuffle.
        self.active = if active_title_idx == from {
            dest
        } else {
            // recompute where the old active landed
            let mut a = active_title_idx;
            if from < a {
                a -= 1;
            }
            if dest <= a {
                a += 1;
            }
            a.min(self.tabs.len() - 1)
        };
        self.editing_tab = -1;
        self.dirty = true;
        dest
    }

    /// Every live session uid this window hosts (used to kill them when the window
    /// closes — in Wave 1 each session is referenced by exactly one window).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn session_uids(&self) -> Vec<String> {
        self.tabs
            .iter()
            .flat_map(|t| t.panes.iter().map(|p| p.uid.clone()))
            // Panes/tabs parked on the recently-closed history keep their sessions alive (for
            // reopen), so they must be killed when the window closes too — else they'd leak.
            .chain(
                self.closed
                    .iter()
                    .flat_map(|c| c.panes().iter().map(|p| p.uid.clone())),
            )
            // Parked reminder panes also keep their sessions alive — kill them on close too.
            .chain(self.reminders.iter().map(|r| r.pane.uid.clone()))
            .collect()
    }

    /// A session exited on its own. A pane that was *running a program* falls back to a
    /// shell in place; a plain shell's exit closes the pane as it always has.
    ///
    /// The distinction is the whole point. Quitting a program is not the same act as
    /// closing a terminal: `Ctrl-C` out of `claude` in a one-pane tab used to take the pane,
    /// the tab, and everything the user could have typed to bring it back — the recovery
    /// affordance died with the thing it would have recovered. Leaving a shell behind, in
    /// the same slot and the same cwd, makes the exit undoable by typing the program's name.
    ///
    /// A plain shell keeps closing: there `exit` *is* "close this terminal", and respawning
    /// a shell the user just quit would make the pane impossible to get rid of.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn pane_exited(&mut self, uid: &str, mgr: &SessionManager, code: i32) -> PaneExit {
        tracing::info!(uid, code, "pane exited");
        match self.find_pane(uid) {
            Some((ti, pi)) => {
                if let Some(program) = self.exited_program(ti, pi) {
                    let (cwd, env) = {
                        let p = &self.tabs[ti].panes[pi];
                        (p.cwd.clone(), p.env.clone())
                    };
                    // Keep the cwd: the point is to land where the program was, so retyping
                    // its name resumes the same work.
                    if let Some((old, new)) = self.restart_pane_at(ti, pi, mgr, cwd, env) {
                        tracing::info!(old = %old, new = %new, program = %program, "pane respawned as a shell");
                        let p = &mut self.tabs[ti].panes[pi];
                        // The pane runs a shell now, and saying so is not cosmetic: only a
                        // `Terminal` pane may be re-upgraded by the title/foreground sniffs
                        // (`note_pane_title`), so a pane left claiming `Tool` would never
                        // notice the program being started again.
                        p.kind = PaneKind::Terminal;
                        p.pane.feed(&exit_banner(&program, code));
                        return PaneExit {
                            alive: true,
                            rebound: Some((old, new)),
                        };
                    }
                }
                PaneExit {
                    alive: self.close_pane_in(ti, pi, mgr),
                    rebound: None,
                }
            }
            None => {
                // A PARKED (reminder) pane's shell exited on its own — its session is gone,
                // so drop the reminder rather than leave a dead row in the bell list.
                if let Some(i) = self.reminders.iter().position(|r| r.pane.uid == uid) {
                    self.reminders.remove(i);
                    self.dirty = true;
                }
                PaneExit {
                    alive: true,
                    rebound: None,
                }
            }
        }
    }

    /// The display name of the program pane `(ti, pi)` was spawned to run, or `None` if it
    /// was only ever a shell.
    ///
    /// Read from what the pane was *spawned* with — its `PaneKind` and `spawn_command` —
    /// never from a sniff. A sniff says what the pane happened to be running a moment ago;
    /// falling back to a shell is a decision about what the pane *is for*, and a shell pane
    /// that ran `claude` once must not start refusing to close.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn exited_program(&self, ti: usize, pi: usize) -> Option<String> {
        let p = self.tabs.get(ti)?.panes.get(pi)?;
        if !p.kind.is_pty() {
            return None;
        }
        if let PaneKind::Tool(id) = &p.kind {
            return Some(
                avada_core::tools::registry::by_id(id)
                    .map(|t| t.bin.to_string())
                    .unwrap_or_else(|| id.clone()),
            );
        }
        // A `Terminal` pane spawned with a command (`htop`, a script) is a program pane too —
        // the kind only records that we could not name the tool, not that there wasn't one.
        let cmd = p.spawn_command.as_deref()?.trim();
        let head = cmd.split_whitespace().next()?;
        Some(head.rsplit(['/', '\\']).next().unwrap_or(head).to_string())
    }

    // ---- runtime tool identity (D5: inference upgrades chrome, never the relaunch) ----

    /// Fold an OSC window title into the sniffed-tool map.
    ///
    /// Only a pane whose recorded [`PaneKind`] is `Terminal` can be upgraded: a pane that
    /// was *spawned* as a tool already has the authoritative answer, and a title that
    /// happens to name a different tool must not overwrite it. A title that names no tool
    /// leaves the previous sniff alone — `claude` prints plenty of transient titles, and
    /// the honest downgrade signal is the shell returning to a prompt, not a quiet frame.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn note_pane_title(&mut self, uid: &str, title: &str) {
        let Some((ti, pi)) = self.find_pane(uid) else {
            return;
        };
        if !matches!(self.tabs[ti].panes[pi].kind, PaneKind::Terminal) {
            return;
        }
        if let Some(t) = avada_core::tools::registry::by_title(title) {
            let changed = self.sniffed_tool.get(uid).map(String::as_str) != Some(t.id);
            if changed {
                self.sniffed_tool.insert(uid.to_string(), t.id.to_string());
                self.dirty = true;
            }
        }
    }

    /// Fold the kernel's answer to "what is running in this pane's foreground *right now*"
    /// into the sniffed-tool map (D5's deterministic signal — see
    /// `docs/tool-panes-plan.md` §D5).
    ///
    /// `name` is a normalised process name (`Some("claude")`, `Some("zsh")`) or `None`.
    /// The two are **not** alike:
    ///
    /// * `Some(n)` is a definite answer. If it names a tool the pane wears that tool's
    ///   chrome; if it names anything else the shell is back at a prompt, so this is the
    ///   honest **downgrade** — the same one [`note_agent_idle`](Self::note_agent_idle)
    ///   performs, and for the same reason: a pane that ran `claude` once must not wear
    ///   its mark forever.
    /// * `None` is *no answer* — a platform with no foreground process group, a daemon
    ///   that predates the field, a pty that exposes no descriptor. It must leave every
    ///   previous belief alone: treating it as a downgrade would erase every title sniff
    ///   on the platforms that can never answer.
    ///
    /// Like every other sniff this may only upgrade a pane whose recorded [`PaneKind`] is
    /// `Terminal`, and it never touches `spawn_command` / `spawn_args` / the persisted
    /// kind — what the pane *relaunches* is not inferred.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn note_pane_foreground(&mut self, uid: &str, name: Option<&str>) {
        let Some(name) = name else {
            return; // no answer — not "nothing is running"
        };
        let Some((ti, pi)) = self.find_pane(uid) else {
            return;
        };
        if !matches!(self.tabs[ti].panes[pi].kind, PaneKind::Terminal) {
            return;
        }
        match avada_core::tools::foreground::tool_for_foreground_name(name) {
            Some(t) => {
                if self.sniffed_tool.get(uid).map(String::as_str) != Some(t.id) {
                    self.sniffed_tool.insert(uid.to_string(), t.id.to_string());
                    self.dirty = true;
                }
            }
            // Nothing tool-shaped is in the foreground: the shell owns the terminal again.
            None => self.note_agent_idle(uid),
        }
    }

    /// Record the program's own liveness report (`OSC 9;hp;state=…`).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn note_agent_state(&mut self, uid: &str, state: AgentLiveness) {
        if self.agent_live.get(uid) != Some(&state) {
            self.agent_live.insert(uid.to_string(), state);
            self.dirty = true;
        }
    }

    /// The shell is back at a prompt (or the foreground command ended): whatever was
    /// running is not running any more. This is D5's **downgrade** — the sniffed identity
    /// and the liveness badge both go, so a pane that ran `claude` once does not wear its
    /// mark forever. A pane spawned as a tool is untouched (nothing was ever sniffed for
    /// it), which is exactly right: it relaunches as that tool.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn note_agent_idle(&mut self, uid: &str) {
        if self.agent_live.remove(uid).is_some() | self.sniffed_tool.remove(uid).is_some() {
            self.dirty = true;
        }
    }

    /// Record `mark` as the conversation running in pane `uid` — **only if the pane has
    /// no conversation recorded yet**.
    ///
    /// # Why this fills an empty slot instead of taking the best answer
    ///
    /// Three things can name a pane's conversation, and they do not deserve equal trust:
    ///
    /// 1. the **spawn** out of the left panel's session list, which handed the pane one
    ///    exact conversation and therefore cannot be wrong;
    /// 2. a **hook** the tool itself fired, which reports the id the tool is actually in;
    /// 3. an **inference** from watching new conversations appear in the tool's history
    ///    store (`tools::session_infer`), which is a deduction, not a report.
    ///
    /// Ranking them at the moment of adoption would need a stored provenance per pane. It
    /// is not needed, because the three cannot tie: (1) is set when the pane is born, and
    /// (2) lands within a second of the tool starting, whereas (3) cannot adopt before its
    /// second look — a full scan interval later — and only after a baseline taken at the
    /// first. So "first writer wins" *is* "the most authoritative writer wins", and it has
    /// the property that matters more than either: an id already on a pane is never
    /// silently swapped for a different one, which is the failure that would resume the
    /// user into somebody else's conversation.
    ///
    /// Returns whether the mark was taken, so a caller can stop looking.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn adopt_tool_session(&mut self, uid: &str, mark: ToolSessionMark) -> bool {
        let Some((ti, pi)) = self.find_pane(uid) else {
            return false;
        };
        let p = &mut self.tabs[ti].panes[pi];
        if p.tool_session.is_some() {
            return false;
        }
        p.tool_session = Some(mark);
        self.dirty = true;
        true
    }

    /// Whether pane `uid` still needs a conversation found for it, and — once the pane has
    /// said where it is — the directory to look in. `None` means "do not look": the pane is
    /// gone, is not a tool pane, or already knows its conversation.
    ///
    /// Reads `effective_kind`, not `kind`, on purpose — the hand-started pane this exists
    /// for is precisely the one whose persisted kind is still `Terminal`.
    ///
    /// The directory is optional because a pty pane is born without one and learns its live
    /// cwd from the shell's OSC 7 (see the `cwd` seed in `add_pane_opts`). That is not a
    /// reason to stop looking: a hook marker names its own directory, so it can be read the
    /// moment the tool starts. Only the scan-and-diff fallback needs a directory to scope
    /// by, and it would rather wait for a real one than search every project on the disk.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn tool_session_wanted(&self, uid: &str) -> Option<(String, Option<String>)> {
        let p = self
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter())
            .find(|p| p.uid == uid)?;
        if p.tool_session.is_some() {
            return None;
        }
        let tool = self.effective_kind(p).tool_id()?.to_string();
        Some((tool, p.cwd.clone()))
    }

    /// Drop every runtime-only fact keyed by `uid`. Called wherever a pane leaves this
    /// window (closed, detached, restarted under a new uid) so the maps cannot grow
    /// without bound. Everything here is re-learned from the next title or state marker.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn forget_pane_runtime(&mut self, uid: &str) {
        self.sniffed_tool.remove(uid);
        self.agent_live.remove(uid);
        // A Family B view pane also parks a projected row list keyed by this uid. It
        // lives outside `State` (it is a Slint model, not app state), so it has to be
        // released on the same path — otherwise every browser pane ever opened stays
        // resident for the life of the window.
        crate::viewpane::forget(uid);
        // The other three per-uid side stores live outside `State` for the same reason and
        // have to be released on the same path: a decoded image is megabytes, a data tree
        // holds the whole parsed document, and a tier-5 module grid holds a frame's worth
        // of runs. All three are no-ops for a pane that never had one.
        crate::imagepane::forget(uid);
        crate::datatree::forget(uid);
        crate::gridpane::forget(uid);
    }

    /// The identity a pane is **drawn** with: its own `kind` when it has one, else
    /// whatever the title sniff caught it running. `PaneState.kind` — the thing
    /// persistence writes and a relaunch replays — is never written from a sniff.
    #[tracing::instrument(level = "debug", ret, skip(self, ps))]
    pub fn effective_kind(&self, ps: &PaneState) -> PaneKind {
        if !matches!(ps.kind, PaneKind::Terminal) {
            return ps.kind.clone();
        }
        match self.sniffed_tool.get(&ps.uid) {
            Some(id) => PaneKind::Tool(id.clone()),
            None => PaneKind::Terminal,
        }
    }

    /// The liveness badge code for the UI: 0 none, 1 busy, 2 awaiting input, 3 done,
    /// 4 error. An opaque int like every other code crossing the Slint seam.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn liveness_ui(&self, uid: &str) -> i32 {
        match self.agent_live.get(uid) {
            None => 0,
            Some(AgentLiveness::Busy) => 1,
            Some(AgentLiveness::AwaitingInput) => 2,
            Some(AgentLiveness::Done) => 3,
            Some(AgentLiveness::Error) => 4,
        }
    }

    /// Whether this window hosts session `uid` anywhere — laid out in a tab OR parked as a
    /// reminder. Used by the app's event routing so a parked pane's events still reach the
    /// window that owns it (e.g. its shell exiting drops the reminder).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn hosts_session(&mut self, uid: &str) -> bool {
        self.find_pane(uid).is_some() || self.reminders.iter().any(|r| r.pane.uid == uid)
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn focus_pane(&mut self, idx: usize) {
        // Clicking into a pane cancels any in-progress tab rename.
        if self.editing_tab != -1 {
            self.editing_tab = -1;
            self.dirty = true;
        }
        let t = self.active_tab_mut();
        if idx < t.panes.len() && t.focused != idx {
            t.focused = idx;
            if t.zoomed.is_some() {
                t.zoomed = Some(idx); // zoom follows focus
            }
            self.dirty = true;
        }
    }

    /// Point Family B pane `idx` at `target` — the file browser descending into a
    /// directory, or climbing to `..`.
    ///
    /// This is a *retarget*, not a new pane: the uid, the kind and the pane's own
    /// label all stay put, and the breadcrumb the view draws is what tells the user
    /// where they are. A view pane's target lives in `cwd`, which already round-trips
    /// through `PaneSpec`, so navigating is persisted for free.
    ///
    /// Refuses anything that is not a file browser. A viewer's rows are inert, so the
    /// only way to reach this with the wrong kind is a stale click racing a retarget —
    /// and the honest answer to that is to do nothing.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn view_navigate(&mut self, idx: usize, target: String) {
        let t = self.active_tab_mut();
        let Some(p) = t.panes.get_mut(idx) else {
            return;
        };
        if !matches!(p.kind, PaneKind::FileBrowser) || p.cwd.as_deref() == Some(target.as_str()) {
            return;
        }
        p.cwd = Some(target);
        self.dirty = true;
    }

    /// Move focus in `dir`. When soloed (zoom, fullscreen, or single), cycle the pane order.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn focus_dir(&mut self, dir: Direction) {
        let fullscreen = self.fullscreen;
        let t = self.active_tab_mut();
        let n = t.panes.len();
        if n < 2 {
            return;
        }
        let eff = t.effective();
        let next = if t.zoomed.is_some() || fullscreen || eff == Layout::Single {
            let delta = matches!(dir, Direction::Right | Direction::Down);
            Some(if delta {
                (t.focused + 1) % n
            } else {
                (t.focused + n - 1) % n
            })
        } else {
            let tiles = compute_tiles(eff, n, &t.sizes, t.main_fraction, t.focused as i32);
            neighbor_index(&tiles, t.focused, dir)
        };
        if let Some(next) = next {
            t.focused = next;
            if t.zoomed.is_some() {
                t.zoomed = Some(next);
            }
            self.dirty = true;
        }
    }

    // ---- layout / zoom ----

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_layout(&mut self, layout: Layout) {
        let t = self.active_tab_mut();
        if t.layout != layout {
            t.layout = layout;
            self.dirty = true;
        }
    }

    /// Toggle zoom (maximise-in-tab) of the focused pane.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_zoom(&mut self) {
        let t = self.active_tab_mut();
        if t.panes.is_empty() {
            return;
        }
        let f = t.focused;
        t.zoomed = if t.zoomed == Some(f) { None } else { Some(f) };
        self.dirty = true;
    }

    /// Drag a divider: move the boundary by `delta` (a fraction of the area).
    /// Resizing an `auto` tab promotes it to the concrete preset it was showing,
    /// so the dragged sizes stick (mirrors the React Divider, Q7).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn resize_divider(&mut self, kind: DividerKind, index: i32, delta: f64) {
        let n = self.active_tab().panes.len();
        let eff = self.active_tab().effective();
        let t = self.active_tab_mut();
        if t.layout == Layout::Auto {
            t.layout = eff;
            if t.sizes.len() != n {
                t.sizes = equal_sizes(n);
            }
        }
        match kind {
            DividerKind::Main => {
                let before = t.main_fraction;
                t.main_fraction = clamp_fraction(t.main_fraction + delta);
                tracing::debug!(
                    "    resize main: {before:.3} + {delta:.4} -> {:.3} (layout={:?})",
                    t.main_fraction,
                    t.layout
                );
            }
            DividerKind::Size => {
                if index >= 0 {
                    let before = t.sizes.clone();
                    t.sizes = resize_at(&t.sizes, index as usize, delta);
                    tracing::debug!("    resize sizes[{index}] delta={delta:.4}: {before:?} -> {:?} (layout={:?})",
                        t.sizes, t.layout);
                }
            }
        }
        self.dirty = true;
    }

    /// Whether the active tab tiles as rows (so a stitch edge band runs along the
    /// vertical axis → top/bottom rather than left/right). Used by the drag hit-test.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn active_is_rows(&self) -> bool {
        self.active_tab().effective() == Layout::Rows
    }

    /// The current active tab's draggable dividers (empty when zoomed or fullscreen — both
    /// solo a single pane, so there are no seams to drag).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn dividers(&self) -> Vec<avada_core::layout::presets::DividerDesc> {
        let t = self.active_tab();
        if t.zoomed.is_some() || self.fullscreen {
            return Vec::new();
        }
        compute_dividers(t.effective(), t.panes.len(), &t.sizes, t.main_fraction)
    }

    // ---- tabs ----

    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new_tab(&mut self, mgr: &SessionManager) {
        self.new_tab_with(mgr, None, false, NewPaneOpts::default());
    }

    /// Append a tab and seed it with one pane, with everything about it spelled out: an
    /// explicit `title` (`None` ⇒ the "term N" default), whether the app owns it
    /// ([`Tab::system`]), and the full [`NewPaneOpts`] for its first pane. Activates it and
    /// returns its index.
    ///
    /// This is the one place a tab is born, so the control plane's `newTab` (which carries a
    /// title and a cwd) and [`Self::ensure_hyperpane_tab`] (which carries a command) don't
    /// each have to re-do the push/activate/seed dance — and can't get it subtly different.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn new_tab_with(
        &mut self,
        mgr: &SessionManager,
        title: Option<&str>,
        system: bool,
        opts: NewPaneOpts,
    ) -> usize {
        let mut tab = self.fresh_tab();
        if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
            tab.title = t.into();
        }
        tab.system = system;
        tracing::info!(title = %tab.title, system, "tab created");
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.add_pane_opts(mgr, opts); // seed one pane so the tab is usable
        self.editing_tab = -1;
        self.dirty = true;
        self.active
    }

    /// Create the always-on **Hyperpane** tab, unless this window already has one.
    ///
    /// The tab runs the user's coding CLI in [`hyperpane_dir`], the app-managed directory
    /// holding the skills that teach it to drive this workspace over the control API. Returns
    /// `true` only when a tab was actually created — the caller uses that to enable the
    /// control API exactly once, at creation, and never again.
    ///
    /// The CLI is picked the way the rest of the app picks one: the first favourite that
    /// resolves, then Claude, then the first resolving agent in the registry. Editors are
    /// excluded — `vim` is a tool pane like any other, but it cannot read a skill. No agent
    /// installed ⇒ a plain shell in the same directory, so the tab and its files still exist
    /// and the user can start one by hand.
    ///
    /// [`hyperpane_dir`]: avada_core::persistence::paths::hyperpane_dir
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn ensure_hyperpane_tab(&mut self, mgr: &SessionManager) -> bool {
        if self.tabs.iter().any(|t| t.system) {
            return false;
        }
        let Some(dir) = self.materialize_hyperpane_dir() else {
            return false;
        };
        let opts = self.hyperpane_pane_opts(&dir, true);
        self.new_tab_with(mgr, Some("Hyperpane"), true, opts);
        // `new_tab_with` appends; the Hyperpane tab lives at the front. Pinning AFTER the
        // seed keeps `add_pane_opts`' "active tab" contract intact.
        self.pin_system_tab_first();
        tracing::info!(dir = %dir.display(), "Hyperpane tab created");
        true
    }

    /// How long a failed [`avada_core::hyperpane::materialize`] is held before the
    /// tick's retry is allowed to try again — long enough that a read-only data dir does
    /// not turn the 8 ms pump into a filesystem hammer, short enough that fixing it (or a
    /// transient failure passing) is noticed in the same sitting.
    const HYPERPANE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

    /// Refresh the shipped files from the bundle and return the Hyperpane directory.
    ///
    /// Failing that (a read-only or missing data dir) there is nowhere to put the tab, so
    /// `None` — the caller leaves it uncreated rather than open an agent into some arbitrary
    /// cwd where its skill would be absent. The failure is logged at warn and remembered:
    /// until [`Self::HYPERPANE_RETRY`] has passed the call returns `None` immediately, so
    /// the app's per-tick retry (which keeps `hyperpane_done` unlatched on failure) is
    /// bounded rather than every 8 ms.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn materialize_hyperpane_dir(&mut self) -> Option<std::path::PathBuf> {
        let now = std::time::Instant::now();
        if self.hyperpane_retry_at.is_some_and(|at| now < at) {
            return None;
        }
        match avada_core::hyperpane::materialize() {
            Ok(dir) => {
                self.hyperpane_retry_at = None;
                Some(dir)
            }
            Err(e) => {
                tracing::warn!(
                    "could not prepare the Hyperpane directory ({e}); tab not created, \
                     retrying in {}s",
                    Self::HYPERPANE_RETRY.as_secs()
                );
                self.hyperpane_retry_at = Some(now + Self::HYPERPANE_RETRY);
                None
            }
        }
    }

    /// Refill an EMPTIED system tab with a plain shell in the Hyperpane directory.
    ///
    /// [`Self::take_pane_inner`] never drops the system tab, so the last pane leaving it —
    /// closed, moved to another tab, torn off to another window, parked — leaves it with no
    /// panes. This is the other half of that contract: every caller that owns a manager
    /// runs it right after the take, and the app's tick runs it as a backstop for the
    /// callers that don't. Deliberately a SHELL, not the agent: the pane left because the
    /// user sent it away, and an agent that comes straight back would read as ignoring
    /// them. The shell sits in the same directory with the same control-API environment,
    /// so `claude` typed into it is the Hyperpane again.
    ///
    /// The directory is re-materialized each time (the same call the creation uses); if
    /// that fails the shell starts wherever the default cwd lands, because an empty tab is
    /// the one outcome this exists to prevent. Returns whether a pane was added.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn reseed_system_tab(&mut self, mgr: &SessionManager) -> bool {
        let Some(ti) = self
            .tabs
            .iter()
            .position(|t| t.system && t.panes.is_empty())
        else {
            return false;
        };
        let dir = self.materialize_hyperpane_dir();
        let opts = match &dir {
            Some(dir) => self.hyperpane_pane_opts(dir, false),
            None => NewPaneOpts {
                label: Some("hyperpane".to_string()),
                ..Default::default()
            },
        };
        // `add_pane_opts` seeds the ACTIVE tab; the system tab is not necessarily it.
        let was = self.active;
        self.active = ti;
        let added = self.add_pane_opts(mgr, opts).is_some();
        self.active = was;
        if added {
            tracing::info!(tab = ti, "Hyperpane tab reseeded with a shell");
        } else {
            tracing::warn!(tab = ti, "Hyperpane tab could not be reseeded");
        }
        self.dirty = true;
        added
    }

    /// The pane the Hyperpane tab runs: the user's coding CLI (`agent`) or a plain shell,
    /// either way in `dir` with the environment the control-API skills need. Shared by
    /// creation ([`Self::ensure_hyperpane_tab`]) and refill ([`Self::reseed_system_tab`]).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn hyperpane_pane_opts(&self, dir: &std::path::Path, agent: bool) -> NewPaneOpts {
        use avada_core::tools::{detect, registry};
        let picked = self
            .settings
            .tool_favorites
            .iter()
            .filter_map(|id| registry::by_id(id))
            .chain(registry::by_id("claude"))
            .chain(registry::TOOLS.iter())
            .filter(|t| !registry::EDITOR_IDS.contains(&t.id))
            .filter(|_| agent)
            .find_map(|t| detect::resolve(t, &self.settings.tool_paths).map(|r| (t, r)));

        let (kind, command) = match &picked {
            // The resolved absolute path, not the bare name: the pane's shell may not carry
            // the same PATH the app was launched with, and a tab that opens on
            // "command not found" is worse than no tab.
            Some((def, res)) => (
                Some(PaneKind::Tool(def.id.to_string())),
                Some(res.path.to_string_lossy().into_owned()),
            ),
            None => (None, None),
        };

        let mut env: avada_core::session::spawn::EnvMap = std::collections::HashMap::new();
        // Where the control API's token and port are published. The agent reads it through
        // `avada ctl`, which does the same lookup — set explicitly so a pane that
        // inherited an empty value from somewhere else still points at the right file.
        env.insert(
            "AVADA_CONTROL_FILE".to_string(),
            avada_core::persistence::paths::control_json()
                .to_string_lossy()
                .into_owned(),
        );
        env.insert(
            "HP_HYPERPANE_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        );
        // Same Claude Code >= 2.1.x tool-search deferral `submit_new_goal` pins off: with the
        // default mode every MCP tool needs a ToolSearch round-trip before it can be called.
        env.insert("ENABLE_TOOL_SEARCH".to_string(), "false".to_string());
        if let Ok(exe) = std::env::current_exe() {
            env.insert("HP_CTL".to_string(), exe.to_string_lossy().into_owned());
            // Put the app's own directory on PATH so the skills' `avada ctl …` resolves
            // verbatim — which is what lets the shipped `.claude/settings.json` pre-approve
            // exactly that command and nothing else. An absolute path could not be written
            // into a permission rule, and a permission prompt per read would make the tab
            // useless.
            if let Some(exe_dir) = exe.parent() {
                let base = std::env::var("PATH").unwrap_or_default();
                let sep = if cfg!(windows) { ";" } else { ":" };
                env.insert(
                    "PATH".to_string(),
                    format!("{}{sep}{base}", exe_dir.to_string_lossy()),
                );
            }
        }

        NewPaneOpts {
            label: Some("hyperpane".to_string()),
            cwd: Some(dir.to_string_lossy().into_owned()),
            command,
            kind,
            env: Some(env),
            ..Default::default()
        }
    }

    /// Close tab `idx`, killing its sessions. Returns `false` if nothing remains
    /// (caller quits the window).
    ///
    /// A system tab (the always-on "Hyperpane") is never closed — every close path funnels
    /// through here or [`Self::close_tab_menu`], so guarding both covers the × button, the
    /// keybinding, the context menu, "close others"/"close to the right", and the control API
    /// alike. The return is `true` ("something remains"), because refusing to close is not a
    /// reason to quit the window.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn close_tab(&mut self, idx: usize, mgr: &SessionManager) -> bool {
        if idx >= self.tabs.len() {
            return true;
        }
        if self.tabs[idx].system {
            return true;
        }
        tracing::info!(tab = idx, title = %self.tabs[idx].title, "tab closed");
        if self.tabs.len() <= 1 {
            // Last tab: kill its sessions and signal quit.
            for p in &self.tabs[idx].panes {
                kill_session_of(mgr, &p.uid, &p.kind);
            }
            return false;
        }
        let tab = self.tabs.remove(idx);
        for p in &tab.panes {
            kill_session_of(mgr, &p.uid, &p.kind);
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        self.editing_tab = -1;
        self.dirty = true;
        true
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() && idx != self.active {
            self.active = idx;
            self.editing_tab = -1;
            self.dirty = true;
        }
    }

    /// Move the active tab by `delta` (±1), wrapping around — the Ctrl+Tab / Ctrl+Shift+Tab
    /// keybindings. No-op with a single tab.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn cycle_tab(&mut self, delta: i32) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let next = (self.active as i32 + delta).rem_euclid(n as i32) as usize;
        self.switch_tab(next);
    }

    /// Nudge the FOCUSED pane's terminal font size by `delta` px (clamped) — the Ctrl+= /
    /// Ctrl+- font-zoom keybindings (and Ctrl+wheel). Zoom is per-pane (Electron parity): only
    /// the focused pane re-grids; its neighbours keep their own size. The pump reloads the
    /// pane's font at the current DPI scale (via `font_dirty`) and re-flows it.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn font_zoom(&mut self, delta: i32) {
        let f = self.active_tab().focused;
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            let next = Settings::clamp_font(p.font_px + delta as f32);
            if next != p.font_px {
                p.font_px = next;
                p.font_dirty = true;
            }
        }
        self.show_zoom_toast();
    }

    /// Reset the FOCUSED pane's terminal font size to the configured base — the Ctrl+0
    /// keybinding. Only the focused pane is affected (per-pane zoom).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn font_reset(&mut self) {
        let base = self.settings.font_px;
        let f = self.active_tab().focused;
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            if p.font_px != base {
                p.font_px = base;
                p.font_dirty = true;
            }
        }
        self.show_zoom_toast();
    }

    /// Flash the focused pane's current zoom as a `%` indicator in its bottom-right — the same
    /// transient "toast" the widget uses for copy/paste confirmations. Percentage is relative
    /// to the default font size, matching the Electron zoom badge.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn show_zoom_toast(&mut self) {
        let f = self.active_tab().focused;
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            let pct = (p.font_px / prefs::DEFAULT_FONT_PX * 100.0).round() as i32;
            p.pane.set_toast(format!("{pct}%"));
        }
        self.dirty = true;
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn begin_rename(&mut self, idx: i32) {
        if idx >= 0 && (idx as usize) < self.tabs.len() {
            self.editing_tab = idx;
            self.dirty = true;
        }
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn rename_tab(&mut self, idx: i32, title: &str) {
        if idx >= 0 && (idx as usize) < self.tabs.len() {
            let title = title.trim();
            if !title.is_empty() {
                self.tabs[idx as usize].title = title.into();
            }
        }
        self.editing_tab = -1;
        self.dirty = true;
    }

    /// Begin editing pane `idx`'s label inline (double-click on its header). Cancels any
    /// in-progress tab rename first.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn begin_rename_pane(&mut self, idx: i32) {
        self.editing_tab = -1;
        if idx >= 0 && (idx as usize) < self.active_tab().panes.len() {
            self.editing_pane = idx;
            self.dirty = true;
        }
    }

    /// Commit a pane label rename (blank keeps the prior label, mirroring the renderer).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn rename_pane(&mut self, idx: i32, title: &str) {
        if idx >= 0 && (idx as usize) < self.active_tab().panes.len() {
            let title = title.trim();
            if !title.is_empty() {
                self.active_tab_mut().panes[idx as usize].title = title.into();
            }
        }
        self.editing_pane = -1;
        self.dirty = true;
    }

    /// A pane reported a cwd: refresh the remembered-projects list (the sidebar), AND — if
    /// that cwd sits inside a git repo — TINT this specific pane to the project (the native
    /// port of `applyProjectToPane`): adopt the project color, turn the per-pane frame + dot
    /// ON, and rename the pane to the repo name **only if its label is still a default**.
    /// A clean pane outside any repo is left untouched (stays frame/dot OFF).
    /// Returns the resolved git project (root path + name) so the caller can feed the
    /// ambient-AI engine's `on_cwd`; `None` when the cwd isn't inside a git repo.
    /// Record the directory a pane is **in**, reporting whether it actually moved.
    ///
    /// Split out of [`note_pane_cwd`](Self::note_pane_cwd) because the two answer different
    /// questions and only one of them is cheap. This one writes two fields; that one walks
    /// the tree looking for a `.git`, upserts the project store and rebuilds the project
    /// list. The kernel sniff runs on a timer over every pane, so it asks this first and
    /// only pays for the rest on a real `cd` — which is also what keeps a pane's label from
    /// being re-derived on every tick.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_pane_cwd(&mut self, uid: &str, cwd: &str) -> bool {
        let Some((ti, pi)) = self.find_pane(uid) else {
            return false;
        };
        let p = &mut self.tabs[ti].panes[pi];
        if p.cwd.as_deref() == Some(cwd) {
            return false;
        }
        // Both, as the OSC 7 path does: the widget resolves clickable paths against its
        // copy, and the control read-model's `/state` reads the pane's.
        p.pane.set_cwd(Some(cwd.to_string()));
        p.cwd = Some(cwd.to_string());
        self.dirty = true;
        true
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn note_pane_cwd(&mut self, uid: &str, cwd: &str) -> Option<AiProjectRef> {
        let root = sidebar::git_root_of(cwd)?;
        let project = projects::upsert_project_by_root(&root.to_string_lossy());
        let color = parse_hex(&project.color);
        if let Some((ti, pi)) = self.find_pane(uid) {
            let p = &mut self.tabs[ti].panes[pi];
            p.accent = color;
            p.pinned_accent = Some(color);
            p.show_frame = Some(true);
            p.show_dot = Some(true);
            if is_default_label(&p.title) {
                p.title = project.name.clone().into();
            }
            // Drop a subtitle that merely duplicated the repo name (it's now the label).
            if p.subtitle.as_deref() == Some(project.name.as_str()) {
                p.subtitle = None;
            }
        }
        // Refresh the cached, newest-first project list (rail badge + flyout).
        self.reload_projects();
        self.dirty = true;
        Some(AiProjectRef {
            path: root.to_string_lossy().to_string(),
            name: project.name,
        })
    }

    /// Apply an ambient-AI subtitle produced by the engine to the pane with session `uid`
    /// (the typewriter reveal restarts when the text changes). No-op for an unknown uid.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_ai_subtitle(&mut self, uid: &str, text: &str) {
        if let Some((ti, pi)) = self.find_pane(uid) {
            self.tabs[ti].panes[pi].ai.set_target(text);
        }
    }

    /// The ambient-AI watch list for this window: one entry per pane across all tabs, keyed
    /// by its session uid (used as the stable pane id), carrying the current label + Mute
    /// flag so the engine can summarise unmuted panes and clear muted ones.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn ai_pane_publish(&self) -> Vec<avada_core::ai::service::AiPanePublish> {
        let mut out = Vec::new();
        for tab in &self.tabs {
            for p in &tab.panes {
                out.push(avada_core::ai::service::AiPanePublish {
                    pane_id: p.uid.clone(),
                    session_uid: p.uid.clone(),
                    label: p.title.to_string(),
                    muted: p.ai_muted,
                });
            }
        }
        out
    }

    /// A cheap signature of the AI watch list (uid + label + mute), so the controller only
    /// re-publishes the pane context when something the engine cares about changed.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn ai_context_sig(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        };
        for tab in &self.tabs {
            for p in &tab.panes {
                mix(p.uid.as_bytes());
                mix(b"\x1f");
                mix(p.title.as_bytes());
                mix(if p.ai_muted { b"\x01" } else { b"\x00" });
                mix(b"\x1e");
            }
        }
        h
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_fullscreen(&mut self, on: bool) {
        if self.fullscreen != on {
            self.fullscreen = on;
            self.dirty = true;
        }
    }

    // ---- Wave-2: overlay panels (Seam #3) ----

    /// Whether any overlay panel is currently mounted.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn overlay_open(&self) -> bool {
        self.overlay != Overlay::None
    }

    /// Close whatever overlay is open. Preferences routes through the appearance
    /// save/discard guard (Esc / scrim click); every other overlay closes immediately.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn close_overlay(&mut self) {
        if self.overlay == Overlay::Prefs {
            self.prefs_request_close();
            return;
        }
        self.close_overlay_now();
    }

    /// Actually tear down the overlay (clears any appearance draft + confirm prompt).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn close_overlay_now(&mut self) {
        if self.overlay != Overlay::None {
            // The New-goal box held keyboard focus in its own scope — hand it back to the
            // terminal so the shell is typeable again the instant the box closes.
            let was_goal = self.overlay == Overlay::NewGoal;
            self.overlay = Overlay::None;
            self.prefs_draft = None;
            self.prefs_confirm = false;
            self.font_custom = false;
            self.capturing_binding = None;
            self.capture_conflict = None;
            self.add_project_error.clear();
            self.ask_url.clear();
            self.ask_browsers.clear();
            // Dismissing the confirm card IS the cancel: the close it was holding is dropped
            // on the floor, exactly like the browser chooser's URL.
            self.pending_close = None;
            if was_goal {
                self.refocus_active_pane_scope();
            }
            self.dirty = true;
        }
    }

    // ---- New Pane dialog ----

    /// Open the "New pane" options dialog (Shift+＋ / the menus' "New pane…").
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_new_pane(&mut self) {
        self.overlay = Overlay::NewPane;
        self.dirty = true;
    }

    /// Open the "New goal" box (command palette → "New goal…"). Refreshes the project list
    /// first so the picker reflects any just-added projects, clears any leftover draft
    /// images/text, and loads the ↓-history from disk.
    /// The index into `self.projects` for the FOCUSED pane's project — the project whose root is
    /// the longest path prefix of the focused pane's live cwd. `0` (recency-top) when the focused
    /// pane has no cwd yet or sits outside every remembered project. Used to seed the New-goal
    /// box so a goal defaults to the project the user is actually in, not whatever's most-recent.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn default_goal_project_sel(&self) -> usize {
        let cwd = self
            .active_tab()
            .panes
            .get(self.active_tab().focused)
            .and_then(|p| p.cwd.as_deref());
        cwd.and_then(|c| goal_project_for_cwd(&self.projects, c))
            .unwrap_or(0)
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_new_goal(&mut self) {
        self.reload_projects();
        self.goal_draft_images.clear();
        self.goal_text.clear();
        self.goal_field = 0;
        self.goal_menu_open = false;
        self.goal_menu_sel = 0;
        self.goal_options_open = false;
        // Default the target project to the one the FOCUSED pane sits in — what the user is
        // looking at — not the recency-top project (index 0). Otherwise a goal typed while a
        // DIFFERENT project's orchestrator is the most-recently-touched silently routes to that
        // project's org: the "sent the prompt to the wrong orchestrator" bug. Falls back to 0.
        self.goal_proj_sel = self.default_goal_project_sel();
        self.goal_model_sel = load_goal_model_defaults();
        self.goal_history = load_goal_history();
        self.overlay = Overlay::NewGoal;
        // Focus the box's OWN key scope (it captures every key and forwards to `goal_key`), so
        // the box is keyboard-live however it was opened — the terminal FocusScope can't be
        // grabbed reliably on demand while the overlay covers it.
        self.goal_focus_seq = self.goal_focus_seq.wrapping_add(1);
        self.dirty = true;
    }

    /// Route a URL a pane asked to open, honouring Preferences → Browser.
    ///
    /// Three outcomes, in the order the settings name them:
    /// * `"ask"` and at least one browser was found → mount [`Overlay::AskBrowser`] and
    ///   return `Ok(())`. Nothing opens until a human picks. With *no* browser found there
    ///   is nothing to ask about, so it degrades to the OS handler rather than putting an
    ///   empty card on screen.
    /// * `"app"` and the chosen browser is still installed → open in that browser.
    /// * anything else → the OS default handler, which is where `BROWSER` is honoured.
    ///
    /// A refused URL reports `Err` here rather than silently doing nothing, so the caller
    /// can say why. Validation happens before the overlay mounts, so the chooser is never
    /// holding a URL it would then refuse to open.
    /// Decide what a link clicked inside a rendered markdown preview should do.
    ///
    /// A README's links are mostly `http(s)`, and those keep going through [`open_link`] so
    /// the Preferences browser choice governs a preview exactly as it governs a shell. But a
    /// README also links its *neighbours* — `see [the design](docs/design.md)`, `../README.md`
    /// — and those are the majority of the links a human actually clicks while reading one.
    /// They are not URLs, so `open_link` refused them and the click did nothing at all: no
    /// error, no motion, just a dead link in the middle of a document.
    ///
    /// Resolved against the previewed file's own directory and revealed in the files rail,
    /// the same place a terminal's clicked path lands. Only paths that EXIST resolve; anything
    /// else falls through to `open_link` and gets its refusal, which is what keeps a
    /// `javascript:` or `file:///etc/…` href in a downloaded document inert.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn view_link_command(&self, idx: usize, href: &str) -> crate::command::Command {
        use crate::command::Command;
        let open = Command::OpenLink(href.to_string());
        if avada_core::open::is_openable_url(href) {
            return open;
        }
        let Some(p) = self.active_tab().panes.get(idx) else {
            return open;
        };
        let Some(target) = p.cwd.as_deref().filter(|t| !t.is_empty()) else {
            return open;
        };
        match crate::viewpane::resolve_local_href(Path::new(target), href) {
            Some(abs) => Command::RevealInFiles {
                path: abs.display().to_string(),
                line: None,
                col: None,
            },
            None => open,
        }
    }

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_link(&mut self, url: &str) -> Result<(), String> {
        if !avada_core::open::is_openable_url(url) {
            return Err(format!(
                "refusing to open {url:?}: not an http/https/mailto URL"
            ));
        }
        if self.settings.browser_asks() {
            let found = avada_core::open::list_browsers();
            if !found.is_empty() {
                self.ask_url = url.to_string();
                self.ask_browsers = found;
                self.overlay = Overlay::AskBrowser;
                self.dirty = true;
                return Ok(());
            }
        }
        match self.settings.browser_launcher() {
            Some(l) => avada_core::open::open_url_with(&l, url),
            None => avada_core::open::open_url(url),
        }
    }

    /// Answer the [`Overlay::AskBrowser`] chooser: open the held URL in row `idx`, then
    /// close. An out-of-range row closes without opening — the card comes down either way,
    /// so a stale click can never strand it on screen.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pick_browser(&mut self, idx: usize) -> Result<(), String> {
        let url = std::mem::take(&mut self.ask_url);
        let launcher = self.ask_browsers.get(idx).map(|b| b.launcher.clone());
        self.close_overlay_now();
        match launcher {
            Some(l) if !url.is_empty() => avada_core::open::open_url_with(&l, &url),
            _ => Ok(()),
        }
    }

    /// Hand keyboard focus back to the active pane's terminal `FocusScope` (bumps its
    /// `refocus_seq`). Called when the New-goal box closes so the shell regains the keyboard.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn refocus_active_pane_scope(&mut self) {
        let f = self.active_tab().focused;
        let seq = self.next_refocus();
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            p.refocus_seq = seq;
        }
    }

    /// The next value for a `refocus_seq`, from the app-wide [`Self::refocus_clock`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn next_refocus(&mut self) -> i32 {
        self.refocus_clock = self.refocus_clock.wrapping_add(1);
        self.refocus_clock
    }

    /// Hand the keyboard to the selected pane whenever the selection has moved (L).
    ///
    /// A pane is selected from a dozen places — clicking its header, clicking a tab,
    /// Ctrl+Tab, closing a pane or a tab, a drag that rearranges the grid, restoring a
    /// workspace — but only the two header `TouchArea`s call `tp.focus()`. Everywhere
    /// else the pane drew its focus ring while the keystrokes still went to whatever
    /// held focus before, so the user had to click the pane to type into it.
    ///
    /// One guarded check per tick, for the same reason `sync_left_root` is one: the list
    /// of ways focus moves goes stale, a compare against the last-handed-to pane does not.
    /// The quiet tick costs a string compare.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn sync_pane_keyboard_focus(&mut self) {
        // Something else legitimately owns the keyboard: an overlay's own scope, a tab
        // title editor, a pane label editor. Each focuses its own field, and a double
        // click on an UNFOCUSED pane header moves the selection and opens the label
        // editor in the same gesture — so stealing focus back here would cancel the
        // rename the user just started.
        if self.overlay_open() || self.editing_tab != -1 || self.editing_pane != -1 {
            return;
        }
        let t = self.active_tab();
        let Some(p) = t.panes.get(t.focused) else {
            // No pane to hand it to (an empty tab). Forget the last one, so returning to
            // a populated tab counts as a move again.
            self.kbd_focus_on = None;
            self.kbd_focus_tries = 0;
            return;
        };
        let uid = p.uid.clone();
        let now = (self.active, uid.clone());
        if self.kbd_focus_on.as_ref() != Some(&now) {
            self.kbd_focus_on = Some(now);
            self.kbd_focus_tries = KBD_FOCUS_TRIES;
        }
        if self.kbd_focus_tries == 0 {
            return;
        }
        // Confirmed by the widget — stop, and leave focus alone from here so a click into the
        // left panel or a dialog keeps what it took.
        if self.kbd_focus_live.as_deref() == Some(uid.as_str()) {
            self.kbd_focus_tries = 0;
            return;
        }
        // Not confirmed yet: bump again. One bump is not enough — the pane's element may not
        // exist yet on the tick the selection moved (`sync_model` re-creates the whole
        // repeater when the pane COUNT changes, which is exactly what a tab switch usually
        // does), and an element created carrying the new seq never fires `changed`.
        self.kbd_focus_tries -= 1;
        let seq = self.next_refocus();
        let f = self.active_tab().focused;
        self.active_tab_mut().panes[f].refocus_seq = seq;
        self.dirty = true;
    }

    /// The widget for pane `uid` reported that it gained (`on`) or lost the keyboard.
    /// Confirms — or invalidates — the hand-off `sync_pane_keyboard_focus` asked for.
    ///
    /// Keyed by uid rather than row index because the report is queued by the window and
    /// applied on the next pump (see `Window::focus_acks`): by then the row order may have
    /// moved under it, and a stale index would confirm the hand-off against the wrong pane.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn note_pane_focus(&mut self, uid: &str, on: bool) {
        if on {
            self.kbd_focus_live = Some(uid.to_string());
        } else if self.kbd_focus_live.as_deref() == Some(uid) {
            // Only the pane that currently holds it can give it up: on a move Slint may
            // report the gain before the loss, and clearing unconditionally would erase the
            // confirmation we just recorded for the new pane.
            self.kbd_focus_live = None;
        }
        self.dirty = true;
    }

    /// New-goal box: set the goal text (the key router's controller-owned mirror). Typing
    /// pulls focus back to the text field and closes any open option list.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_set_text(&mut self, text: String) {
        // The TextInput is the source of truth for editing; this just mirrors it (for submit +
        // history). Editing only happens on the text field, so leave field/options state alone.
        self.goal_text = text;
        self.dirty = true;
    }

    /// New-goal box: reveal (or, if already open, hide) the option chips — the Ctrl+O affordance.
    /// Opening focuses the first chip and shows its dropdown; closing returns to the text field.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_toggle_options(&mut self) {
        if self.goal_options_open {
            self.goal_collapse();
        } else {
            self.goal_options_open = true;
            self.goal_field = 1;
            self.goal_menu_toggle(true);
        }
        self.dirty = true;
    }

    /// New-goal box: hide the option chips and return focus to the text field. Bumps the focus
    /// tick so the `gi` TextInput regains the keyboard — without it, collapsing back from a chip
    /// (e.g. Enter to confirm a model tier) leaves the text field visually active but unfocused,
    /// so typing is dropped.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_collapse(&mut self) {
        self.goal_options_open = false;
        self.goal_field = 0;
        self.goal_menu_open = false;
        self.goal_focus_seq = self.goal_focus_seq.wrapping_add(1);
        self.dirty = true;
    }

    /// New-goal box: Tab / Shift+Tab. From the text field it reveals the options (like Ctrl+O);
    /// with the options open it cycles focus among the chips, each auto-showing its dropdown.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_nav(&mut self, delta: i32) {
        if !self.goal_options_open {
            self.goal_toggle_options();
            return;
        }
        let count = (GOAL_FIELDS - 1) as i32; // the 4 option chips (fields 1..=4)
        let cur = self.goal_field.saturating_sub(1) as i32;
        self.goal_field = (cur + delta).rem_euclid(count) as usize + 1;
        self.goal_menu_toggle(true); // show the newly-focused chip's dropdown, seed its selection
        self.dirty = true;
    }

    /// The option rows for the focused field: goal text → history (title = past goal,
    /// subtitle = its project name), project → the project list, model tiers → the model
    /// labels. Empty when there's nothing to offer (no history yet / no projects).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_menu_rows(&self) -> Vec<(String, String)> {
        match self.goal_field {
            0 => self
                .goal_history
                .iter()
                .map(|h| {
                    let proj = std::path::Path::new(&h.project)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (h.text.replace('\n', " "), proj)
                })
                .collect(),
            1 => self
                .projects
                .iter()
                .map(|p| (p.name.clone(), p.path.clone()))
                .collect(),
            _ => crate::command::GOAL_MODEL_LABELS
                .iter()
                .zip(crate::command::GOAL_MODELS.iter())
                .map(|(label, id)| (label.to_string(), id.to_string()))
                .collect(),
        }
    }

    /// New-goal box: open (↓) / close (Esc) the focused field's option list. Opening seeds the
    /// selection on the field's current value; a field with no options stays closed.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_menu_toggle(&mut self, open: bool) {
        if !open {
            self.goal_menu_open = false;
            self.dirty = true;
            return;
        }
        let len = self.goal_menu_rows().len();
        if len == 0 {
            return;
        }
        self.goal_menu_sel = match self.goal_field {
            0 => 0,
            1 => self.goal_proj_sel.min(len - 1),
            f => self.goal_model_sel[f - 2].min(len - 1),
        };
        self.goal_menu_open = true;
        self.dirty = true;
    }

    /// New-goal box: move the open option list's selection by `delta`, clamped. For a chip field
    /// the new selection is applied LIVE (project / model tier); the history list (text field)
    /// applies only on pick (Enter).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_menu_nav(&mut self, delta: i32) {
        if !self.goal_menu_open {
            return;
        }
        let len = self.goal_menu_rows().len();
        if len == 0 {
            return;
        }
        let max = (len - 1) as i32;
        self.goal_menu_sel = (self.goal_menu_sel as i32 + delta).clamp(0, max) as usize;
        if self.goal_field != 0 {
            self.apply_goal_menu_sel();
        }
        self.dirty = true;
    }

    /// Apply the current option-list selection to the focused CHIP field (project / model tier).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn apply_goal_menu_sel(&mut self) {
        let sel = self.goal_menu_sel;
        match self.goal_field {
            1 => self.goal_proj_sel = sel.min(self.projects.len().saturating_sub(1)),
            f if f >= 2 => {
                self.goal_model_sel[f - 2] = sel.min(crate::command::GOAL_MODELS.len() - 1)
            }
            _ => {}
        }
    }

    /// New-goal box: apply the option list's selected row. A chip selection applies and leaves the
    /// options open (the dropdown stays visible); a history row (text field) fills the goal text,
    /// re-selects that goal's project, and closes the history list.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_menu_pick(&mut self) {
        if !self.goal_menu_open {
            return;
        }
        let sel = self.goal_menu_sel;
        if self.goal_field == 0 {
            if let Some(h) = self.goal_history.get(sel) {
                self.goal_text = h.text.clone();
                // Force the picked text into the box's TextInput (it's the source of truth for
                // typing, so a plain mirror write wouldn't reach it).
                self.goal_settext_seq = self.goal_settext_seq.wrapping_add(1);
                if let Some(i) = self.projects.iter().position(|p| p.path == h.project) {
                    self.goal_proj_sel = i;
                }
            }
            self.goal_menu_open = false;
        } else {
            self.apply_goal_menu_sel();
        }
        self.dirty = true;
    }

    /// New-goal box (mouse): focus a field. Clicking the text field (`i == 0`) returns to it;
    /// clicking a chip reveals the options, focuses it, and shows its dropdown.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_field_click(&mut self, i: usize) {
        if i == 0 {
            self.goal_field = 0;
            self.goal_menu_open = false;
        } else {
            self.goal_options_open = true;
            self.goal_field = i.min(GOAL_FIELDS - 1);
            self.goal_menu_toggle(true);
        }
        self.dirty = true;
    }

    /// New-goal box (mouse): apply option row `i` of the open list.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_menu_click(&mut self, i: usize) {
        if !self.goal_menu_open {
            return;
        }
        self.goal_menu_sel = i;
        self.goal_menu_pick();
    }

    /// The four chip labels under the goal box (project + the three model tiers), reflecting
    /// the current selections.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_chip_labels(&self) -> Vec<String> {
        let proj = self
            .projects
            .get(self.goal_proj_sel)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "no projects".to_string());
        let m = |i: usize| {
            crate::command::GOAL_MODEL_LABELS
                .get(i)
                .copied()
                .unwrap_or("?")
                .to_string()
        };
        vec![
            proj,
            m(self.goal_model_sel[0]),
            m(self.goal_model_sel[1]),
            m(self.goal_model_sel[2]),
        ]
    }

    /// Submit the New-goal box (Enter / Start): resolve the selected project + models, remember
    /// the goal in the ↓-history, and hand it to the project's orchestrator via
    /// [`State::submit_new_goal`] (which closes the overlay). No-op while the option list is
    /// open, on empty text, or with no projects.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn goal_submit(&mut self, mgr: &SessionManager) {
        let text = self.goal_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(path) = self
            .projects
            .get(self.goal_proj_sel)
            .map(|p| p.path.clone())
        else {
            return;
        };
        let m = |i: usize| {
            *crate::command::GOAL_MODELS
                .get(i)
                .unwrap_or(&crate::command::GOAL_MODELS[0])
        };
        let (orch, spec, implm) = (
            m(self.goal_model_sel[0]),
            m(self.goal_model_sel[1]),
            m(self.goal_model_sel[2]),
        );
        if self.submit_new_goal(mgr, &path, &text, orch, spec, implm) {
            // Remember the chosen tiers so the next New-goal box opens pre-filled.
            save_goal_model_defaults(&self.goal_model_sel);
            self.goal_history.retain(|h| h.text != text);
            self.goal_history.insert(
                0,
                GoalHistoryEntry {
                    text,
                    project: path,
                },
            );
            self.goal_history.truncate(GOAL_HISTORY_CAP);
            save_goal_history(&self.goal_history);
        }
    }

    /// New-goal box Ctrl+V: an image on the clipboard becomes an attachment; otherwise
    /// clipboard text is appended to the goal text.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_paste_clipboard(&mut self) {
        if self.goal_paste_image() {
            return;
        }
        if let Ok(mut cb) = arboard::Clipboard::new() {
            if let Ok(text) = cb.get_text() {
                let t = text.replace('\r', "");
                if !t.is_empty() {
                    self.goal_text.push_str(&t);
                    // Push the updated text into the TextInput (it's the source of truth).
                    self.goal_settext_seq = self.goal_settext_seq.wrapping_add(1);
                    self.goal_field = 0;
                    self.goal_menu_open = false;
                    self.dirty = true;
                }
            }
        }
    }

    /// The active frame palette's 8 swatches (the New Pane dialog's color row). The default
    /// swatch index the dialog seeds is the next palette-rotation slot (mirrors the
    /// renderer's `nextColor(seq)`), computed in the resync from `panes.len()`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn frame_swatches(&self) -> Vec<Color> {
        theme::frame_palette(self.settings.frame_palette)
            .iter()
            .map(|(r, g, b)| Color::from_rgb_u8(*r, *g, *b))
            .collect()
    }

    // ---- command palette ----

    /// Open the palette: snapshot the command registry from current state, reset the
    /// query + selection. Rebuilt every open so pane/layout entries stay fresh.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_palette(&mut self) {
        self.palette_entries = palette::build(self);
        self.palette_query.clear();
        self.palette_view = (0..self.palette_entries.len()).collect();
        self.palette_sel = 0;
        self.overlay = Overlay::Palette;
        self.dirty = true;
    }

    /// Update the palette query → refilter + re-rank, keeping the selection in range.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn palette_set_query(&mut self, query: &str) {
        self.palette_query = query.to_string();
        self.palette_view = palette::filter(&self.palette_entries, query);
        self.palette_sel = 0;
        self.dirty = true;
    }

    /// Move the palette selection by `delta` rows, clamped to the visible results.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn palette_nav(&mut self, delta: i32) {
        let n = self.palette_view.len();
        if n == 0 {
            return;
        }
        let cur = self.palette_sel as i32;
        let next = (cur + delta).clamp(0, n as i32 - 1);
        if next as usize != self.palette_sel {
            self.palette_sel = next as usize;
            self.dirty = true;
        }
    }

    /// Set the palette selection to a specific visible row (e.g. a mouse click).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn palette_select(&mut self, idx: usize) {
        if idx < self.palette_view.len() && idx != self.palette_sel {
            self.palette_sel = idx;
            self.dirty = true;
        }
    }

    /// The command for the currently-highlighted palette row (consumed on activate).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn palette_command(&self) -> Option<Command> {
        let entry = self.palette_view.get(self.palette_sel)?;
        self.palette_entries.get(*entry).map(|e| e.command.clone())
    }

    /// The visible palette rows as `(title, subtitle)` pairs, in display order.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn palette_rows(&self) -> Vec<(SharedString, SharedString)> {
        self.palette_view
            .iter()
            .filter_map(|i| self.palette_entries.get(*i))
            .map(|e| (e.title.as_str().into(), e.subtitle.as_str().into()))
            .collect()
    }

    // ---- preferences ----

    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_prefs(&mut self) {
        self.overlay = Overlay::Prefs;
        // Snapshot the appearance settings into a draft so edits preview without touching
        // the live panes until Done.
        self.prefs_draft = Some(PrefsDraft::from_settings(&self.settings));
        self.prefs_confirm = false;
        self.font_custom = prefs::is_custom_font(&self.settings.font_family);
        self.dirty = true;
    }

    /// Font picker: select option `idx` from `prefs::FONT_OPTIONS`, or enter "Custom…" mode
    /// when `idx` is the trailing Custom entry (== `FONT_OPTIONS.len()`). Edits the draft.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn font_select(&mut self, idx: usize) {
        let Some(d) = self.prefs_draft.as_mut() else {
            return;
        };
        if let Some((_, value)) = prefs::FONT_OPTIONS.get(idx) {
            d.font_family = value.to_string();
            self.font_custom = false;
        } else {
            // Custom… — start from an empty field unless the current value is already custom.
            if !prefs::is_custom_font(&d.font_family) {
                d.font_family.clear();
            }
            self.font_custom = true;
        }
        self.dirty = true;
    }

    /// Font picker: set the custom font path typed in the "Custom…" field (edits the draft).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn font_custom_value(&mut self, value: String) {
        if let Some(d) = self.prefs_draft.as_mut() {
            d.font_family = value;
            self.font_custom = true;
            self.dirty = true;
        }
    }

    /// The appearance values the dialog should display: the draft while Preferences is open,
    /// else the committed settings. Returns `(resolved_font_path, frame_palette, terminal_theme,
    /// ui_palette, font_px, show_frame, show_dot)`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn appearance_view(&self) -> (String, usize, usize, usize, f32, bool, bool) {
        match &self.prefs_draft {
            Some(d) => (
                prefs::resolve_or_default(&d.font_family),
                d.frame_palette,
                d.terminal_theme,
                d.ui_palette,
                d.font_px,
                d.show_frame,
                d.show_dot,
            ),
            None => (
                self.settings.font_path(),
                self.settings.frame_palette,
                self.settings.terminal_theme,
                self.settings.ui_palette,
                self.settings.font_px,
                self.settings.show_frame,
                self.settings.show_dot,
            ),
        }
    }

    /// Edit the appearance **draft** (no live change). Used for the appearance settings while
    /// the dialog is open; a no-op if there's no draft or `s` isn't an appearance setting.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn draft_setting(&mut self, s: Setting) {
        let Some(d) = self.prefs_draft.as_mut() else {
            return;
        };
        match s {
            Setting::FontFamily(path) => d.font_family = path,
            Setting::FramePalette(idx) => d.frame_palette = idx,
            Setting::TerminalTheme(idx) => d.terminal_theme = idx,
            Setting::UiPalette(idx) => d.ui_palette = idx,
            Setting::FontDelta(delta) => d.font_px = Settings::clamp_font(d.font_px + delta as f32),
            Setting::ShowFrame(on) => d.show_frame = on,
            Setting::ShowDot(on) => d.show_dot = on,
            // Non-appearance settings never reach the draft.
            Setting::DefaultShell(_)
            | Setting::ClickablePaths(_)
            | Setting::CopyOnSelect(_)
            | Setting::EditorCommand(_)
            | Setting::IdleAlert(_)
            | Setting::IdleEffect(_)
            | Setting::IdleSeconds(_)
            | Setting::AutoUpdate(_)
            | Setting::KeepAlive(_)
            | Setting::ToggleFavoriteTool(_)
            | Setting::ToolPath(..)
            | Setting::BrowserMode(_)
            | Setting::BrowserApp(_) => {}
        }
        self.dirty = true;
    }

    /// Whether the appearance draft differs from the committed settings (un-applied edits).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn prefs_dirty(&self) -> bool {
        match &self.prefs_draft {
            Some(d) => *d != PrefsDraft::from_settings(&self.settings),
            None => false,
        }
    }

    /// Commit the appearance draft to the live settings (Done / Save): apply each changed
    /// field via [`Self::apply_setting`] so font reload + palette remap happen, then close.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn prefs_done(&mut self) {
        if let Some(d) = self.prefs_draft.take() {
            if d.font_family != self.settings.font_family {
                self.apply_setting(Setting::FontFamily(d.font_family.clone()));
            }
            if d.frame_palette != self.settings.frame_palette {
                self.apply_setting(Setting::FramePalette(d.frame_palette));
            }
            if d.terminal_theme != self.settings.terminal_theme {
                self.apply_setting(Setting::TerminalTheme(d.terminal_theme));
            }
            if d.ui_palette != self.settings.ui_palette {
                self.apply_setting(Setting::UiPalette(d.ui_palette));
            }
            if d.font_px != self.settings.font_px {
                // Apply the absolute drafted size (apply_setting takes a delta).
                self.apply_setting(Setting::FontDelta(
                    (d.font_px - self.settings.font_px).round() as i32,
                ));
            }
            if d.show_frame != self.settings.show_frame {
                self.apply_setting(Setting::ShowFrame(d.show_frame));
            }
            if d.show_dot != self.settings.show_dot {
                self.apply_setting(Setting::ShowDot(d.show_dot));
            }
        }
        self.close_overlay_now();
    }

    /// Esc / scrim click while Preferences is open: prompt to save/discard if there are
    /// un-applied appearance edits, otherwise just close (discarding the empty draft).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn prefs_request_close(&mut self) {
        if self.prefs_dirty() {
            self.prefs_confirm = true;
            self.dirty = true;
        } else {
            self.close_overlay_now();
        }
    }

    /// Resolve the save/discard prompt: 0 = keep editing · 1 = discard · 2 = save.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn prefs_confirm_resolve(&mut self, action: i32) {
        match action {
            0 => {
                self.prefs_confirm = false;
                self.dirty = true;
            }
            1 => self.close_overlay_now(), // discard the draft
            2 => self.prefs_done(),        // commit the draft
            _ => {}
        }
    }

    /// Apply a single preferences edit: mutate the settings, persist the blob, and flag
    /// a resync (font edits additionally request a font reload on the next pump).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn apply_setting(&mut self, s: Setting) {
        match s {
            Setting::FontFamily(path) => {
                if self.settings.font_family != path {
                    self.settings.font_family = path;
                    self.font_reload = true;
                }
            }
            Setting::FramePalette(idx) => {
                if self.settings.frame_palette != idx {
                    self.settings.frame_palette = idx;
                    // Recompute every pane's accent against the new palette (by creation
                    // slot); pinned project colors are preserved by `relabel`.
                    for t in &mut self.tabs {
                        t.relabel(idx);
                    }
                }
            }
            Setting::TerminalTheme(idx) => {
                if self.settings.terminal_theme != idx {
                    self.settings.terminal_theme = idx;
                    // Repaint every open pane with the new colour theme.
                    let theme = theme::terminal_theme(idx);
                    for t in &mut self.tabs {
                        for p in &mut t.panes {
                            p.pane.set_palette(theme);
                        }
                    }
                }
            }
            Setting::UiPalette(idx) => {
                // Nothing to recompute here: the shell's colours live in the Slint `Theme`
                // global, and the resync pushes them from `appearance_view` every frame the
                // index changes. This arm only has to make the choice stick.
                self.settings.ui_palette = idx;
            }
            Setting::FontDelta(d) => {
                let next = Settings::clamp_font(self.settings.font_px + d as f32);
                if next != self.settings.font_px {
                    self.settings.font_px = next;
                    // The Appearance font-size pref sets the base for everything: re-base every
                    // pane to it (resetting any per-pane zoom), then reload all fonts.
                    for t in &mut self.tabs {
                        for p in &mut t.panes {
                            p.font_px = next;
                        }
                    }
                    self.font_reload = true;
                }
            }
            Setting::DefaultShell(shell) => self.settings.default_shell = shell,
            Setting::ShowFrame(on) => self.settings.show_frame = on,
            Setting::ShowDot(on) => self.settings.show_dot = on,
            Setting::ClickablePaths(on) => self.settings.clickable_paths = on,
            Setting::CopyOnSelect(on) => self.settings.copy_on_select = on,
            Setting::EditorCommand(cmd) => self.settings.editor_command = cmd,
            Setting::IdleAlert(on) => self.settings.idle_alert = on,
            Setting::IdleEffect(idx) => {
                if let Some((token, _)) = crate::glow::IdleEffect::OPTIONS.get(idx) {
                    self.settings.idle_effect = (*token).to_string();
                }
            }
            Setting::IdleSeconds(d) => {
                // `d` is a ±1 step; the dial moves in whole IDLE_STEP_SECONDS jumps and
                // snaps any odd persisted value onto the grid.
                let step = prefs::IDLE_STEP_SECONDS as i32;
                let cur = self.settings.idle_alert_seconds as i32;
                let steps = (cur / step + d).clamp(
                    prefs::MIN_IDLE_SECONDS as i32 / step,
                    prefs::MAX_IDLE_SECONDS as i32 / step,
                );
                self.settings.idle_alert_seconds = (steps * step) as u32;
            }
            Setting::AutoUpdate(on) => self.settings.auto_update = on,
            Setting::KeepAlive(on) => self.settings.keep_alive = on,
            Setting::ToggleFavoriteTool(id) => self.settings.toggle_favorite_tool(&id),
            Setting::ToolPath(id, path) => {
                let path = path.trim().to_string();
                if path.is_empty() {
                    self.settings.tool_paths.remove(&id);
                } else {
                    self.settings.tool_paths.insert(id, path);
                }
            }
            Setting::BrowserMode(mode) => self.settings.browser_mode = mode,
            Setting::BrowserApp(id) => self.settings.browser_app = id,
        }
        prefs::save(&self.settings);
        self.dirty = true;
    }

    /// The live preferences as the camelCase JSON object the control plane serves from
    /// `GET /settings`. Serializing [`Settings`] rather than re-listing its fields means the
    /// published shape follows the struct: a field added there shows up here — and in the key
    /// validation `PATCH /settings` runs against this same blob — with no second edit.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn settings_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.settings).unwrap_or_else(|_| serde_json::json!({}))
    }

    /// Apply a settings patch from the control plane: a shallow merge of camelCase keys onto
    /// the live [`Settings`], persisted and with the same live side effects the Preferences
    /// panel triggers.
    ///
    /// This deliberately does NOT go through [`Setting`]: two of that enum's variants
    /// (`FontDelta`, `IdleSeconds`) are ±1 steps for a dial, and a caller saying
    /// `{"fontPx": 15}` means fifteen, not fifteen steps up. Merging through serde also keeps
    /// the accepted keys in step with the struct instead of a hand-written match that would
    /// silently miss each new field.
    ///
    /// Returns the keys actually applied, or `Err` if the patch is not an object, names a key
    /// [`Settings`] does not have, or gives one a value of the wrong type — all-or-nothing, so
    /// a typo in one key cannot leave the other half applied.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn patch_settings(&mut self, patch: &serde_json::Value) -> Result<Vec<String>, String> {
        let keys = self.apply_settings_patch(patch)?;
        prefs::save(&self.settings);
        tracing::info!(keys = ?keys, "settings patched");
        Ok(keys)
    }

    /// [`Self::patch_settings`] minus the write to disk: validation, the merge, and every
    /// live effect. Split out so it can be exercised without touching the user's real
    /// `native-settings.json` (there is no data-dir override for tests).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn apply_settings_patch(&mut self, patch: &serde_json::Value) -> Result<Vec<String>, String> {
        let obj = patch
            .as_object()
            .ok_or_else(|| "settings patch must be a JSON object".to_string())?;
        let mut merged = self.settings_json();
        let dst = merged
            .as_object_mut()
            .ok_or_else(|| "settings are not an object".to_string())?;
        let mut keys: Vec<String> = Vec::with_capacity(obj.len());
        for (k, v) in obj {
            if !dst.contains_key(k) {
                return Err(format!("unknown setting: {k}"));
            }
            dst.insert(k.clone(), v.clone());
            keys.push(k.clone());
        }
        let mut next: Settings =
            serde_json::from_value(merged).map_err(|e| format!("bad settings patch: {e}"))?;
        // `log_level` is a free string to serde; the logger only understands its own set,
        // and a typo stored here would silently fall back at the next launch.
        if !avada_core::logging::valid_level(&next.log_level) {
            return Err(format!(
                "bad settings patch: unknown log level {:?}",
                next.log_level
            ));
        }

        // The panel's widgets can only produce in-range values; a JSON caller can ask for a
        // 400px font or a 3-second idle alert, so clamp on the way in rather than storing
        // something the UI would then have to defend against.
        next.font_px = Settings::clamp_font(next.font_px);
        next.idle_alert_seconds = prefs::idle_seconds_on_grid(next.idle_alert_seconds);
        next.frame_palette = next.frame_palette.min(theme::FRAME_PALETTES.len() - 1);
        next.terminal_theme = next.terminal_theme.min(theme::TERMINAL_THEMES.len() - 1);
        // Everything the clamps above don't cover (palette indices, token-valued strings, the
        // loop cadences and prompt): one rule set, shared with the save path, so a value the
        // panel would refuse cannot arrive through the control plane instead.
        next.validate()
            .map_err(|e| format!("bad settings patch: {e}"))?;

        // Then the same live effects `apply_setting` runs, driven off what actually changed —
        // each is expensive enough (a font reload, a repaint of every open pane) to be worth
        // skipping when the patch didn't touch it.
        if next.font_family != self.settings.font_family {
            self.font_reload = true;
        }
        if next.font_px != self.settings.font_px {
            // The Appearance font size is the base for everything: re-base every pane to it,
            // resetting any per-pane zoom, exactly as the ± buttons do.
            for t in &mut self.tabs {
                for p in &mut t.panes {
                    p.font_px = next.font_px;
                }
            }
            self.font_reload = true;
        }
        if next.frame_palette != self.settings.frame_palette {
            for t in &mut self.tabs {
                t.relabel(next.frame_palette);
            }
        }
        if next.terminal_theme != self.settings.terminal_theme {
            let palette = theme::terminal_theme(next.terminal_theme);
            for t in &mut self.tabs {
                for p in &mut t.panes {
                    p.pane.set_palette(palette);
                }
            }
        }
        if next.scrollback != self.settings.scrollback {
            // Resizes every live grid's history in place; lines beyond the new depth are
            // dropped, a deeper buffer simply starts keeping more from here on.
            for t in &mut self.tabs {
                for p in &mut t.panes {
                    p.pane.set_scrollback(next.scrollback as usize);
                }
            }
        }

        // Turning the sidebar off must also collapse it, exactly as `toggle_sidebar` does:
        // `show_sidebar` is the preference, `sidebar_open` the live state, and the panel is
        // drawn from the latter.
        if !next.show_sidebar && self.settings.show_sidebar {
            self.sidebar_open = false;
        }

        self.settings = next;
        self.dirty = true;
        Ok(keys)
    }

    // ---- keybindings editor (Preferences → Keybindings) ----

    /// Begin capturing a new chord for binding `id`: the editor row grabs focus and shows
    /// "Press a chord…"; the next captured combo rebinds it (or Esc cancels).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn begin_rebind(&mut self, id: &str) {
        self.capturing_binding = Some(id.to_string());
        self.capture_conflict = None;
        self.dirty = true;
    }

    /// Cancel an in-progress chord capture (Esc, or clicking elsewhere).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn cancel_rebind(&mut self) {
        self.capture_conflict = None;
        if self.capturing_binding.take().is_some() {
            self.dirty = true;
        }
    }

    /// Handle a key captured while rebinding: Escape cancels, a bare modifier is ignored (keep
    /// waiting for a real key), and any other combo becomes the binding's override (persisted)
    /// and ends the capture. If the combo is already held by another binding it is **stolen** —
    /// that binding becomes unbound (its row then shows "Unbound"). No-op when not capturing.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn capture_chord(&mut self, ctrl: bool, alt: bool, shift: bool, text: &str) {
        let Some(id) = self.capturing_binding.clone() else {
            return;
        };
        if crate::is_key(text, slint::platform::Key::Escape) {
            self.cancel_rebind();
            return;
        }
        // A bare modifier press has no key token yet — keep the capture open.
        let Some(key) = crate::key_tok_from_text(text, ctrl) else {
            return;
        };
        let chord = crate::keybindings::Chord {
            ctrl,
            alt,
            shift,
            key,
        };
        // A module's action lives in a different keymap and a different file, so it is
        // rebound through the module store rather than `self.keymap`. No chord stealing:
        // a module's chords are scoped to its own surface, so two modules — or a module and
        // the app — binding the same chord is not a conflict to resolve.
        //
        // On macOS the captured `ctrl` is Control **or** Command (the editor cannot tell
        // them apart), while a module chord's `ctrl` is always the physical Control key
        // (see `crate::pty_ctrl`) — Command stays the app's. A Command capture therefore
        // lands on Control, which is the only one of the two a module can ever receive.
        if let Some((module, _surface, action)) = crate::module_ui::grid::parse_row_id(&id) {
            let spelled = crate::module_ui::grid::chord_of(ctrl, alt, shift, &key.token());
            crate::module_ui::grid::set_binding(&module, &action, Some(&spelled));
            crate::module_ui::grid::save_prefs();
            self.capturing_binding = None;
            self.capture_conflict = None;
            self.dirty = true;
            return;
        }
        // Steal the chord from its current owner (if any) — that binding becomes unbound.
        if let Some(other) = self.keymap.owner_of(chord, &id) {
            self.keymap.unbind(other);
        }
        self.keymap.set(&id, chord);
        self.capturing_binding = None;
        self.capture_conflict = None;
        self.dirty = true;
    }

    /// Unbind binding `id` (clear its chord — nothing fires it) and end any capture on it. The
    /// row then shows "Unbound" until rebound or reset to default.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn unbind_binding(&mut self, id: &str) {
        if let Some((module, _surface, action)) = crate::module_ui::grid::parse_row_id(id) {
            // An explicit unbind is stored, not dropped: it has to survive a restart, and
            // it has to out-rank whatever the preset says. Clearing it is `reset_binding`.
            crate::module_ui::grid::set_binding(&module, &action, None);
            crate::module_ui::grid::save_prefs();
        } else {
            self.keymap.unbind(id);
        }
        if self.capturing_binding.as_deref() == Some(id) {
            self.capturing_binding = None;
            self.capture_conflict = None;
        }
        self.dirty = true;
    }

    /// Reset binding `id` to its default chord (drop the override/unbind).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reset_binding(&mut self, id: &str) {
        if let Some((module, _surface, action)) = crate::module_ui::grid::parse_row_id(id) {
            crate::module_ui::grid::clear_binding(&module, &action);
            crate::module_ui::grid::save_prefs();
        } else {
            self.keymap.reset(id);
        }
        if self.capturing_binding.as_deref() == Some(id) {
            self.capturing_binding = None;
            self.capture_conflict = None;
        }
        self.dirty = true;
    }

    /// Reset every binding to its default (clear all overrides).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reset_all_bindings(&mut self) {
        self.keymap.reset_all();
        crate::module_ui::grid::clear_all_bindings();
        crate::module_ui::grid::save_prefs();
        self.capturing_binding = None;
        self.capture_conflict = None;
        self.dirty = true;
    }

    /// Reload the base font + EVERY pane's font (each at its own per-pane `font_px`) from the
    /// current settings at DPI `scale`, forcing every pane to re-grid at the new cell metrics
    /// (resets each pane's `applied`). Called by the pump (which owns the scale) on a DPI
    /// change or when `font_reload` is set (a font-family / base-size pref change).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reload_font(&mut self, scale: f32) {
        let path = self.settings.font_path();
        self.font = theme::load_font_at(&path, self.settings.font_px, scale);
        for t in &mut self.tabs {
            for p in &mut t.panes {
                p.font = theme::load_font_at(&path, p.font_px, scale);
                p.font_dirty = false;
                p.applied = (0, 0); // force a reflow at the new cell size
            }
        }
        self.font_reload = false;
        self.dirty = true;
    }

    // ---- clickable paths (terminal link hover / activation) ----

    /// Hover hit-test for a clickable path under the cursor (logical px within the pane
    /// surface). Updates the pane's link-overlay state. No-op (and clears any link) when
    /// clickable paths are disabled. `idx` is an active-tab pane.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_link_moved(&mut self, idx: usize, x: f32, y: f32) {
        let on = self.settings.clickable_paths;
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if !on {
                if p.link.is_some() {
                    p.link = None;
                    self.dirty = true;
                }
                return;
            }
            let (w, h) = p.surf;
            let hit = p.pane.link_at(x, y, w, h);
            // Only repaint when the hovered link actually changes.
            if hit != p.link {
                p.link = hit;
                p.link_cursor = (x, y);
                self.dirty = true;
            } else if p.link.is_some() {
                p.link_cursor = (x, y); // keep the tooltip tracking the cursor
            }
        }
    }

    /// Clear a pane's hover link when the pointer leaves its surface.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_link_exited(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if p.link.take().is_some() {
                self.dirty = true;
            }
        }
    }

    /// Activate the link under a click: reveal (plain) or copy (ctrl). Returns the action so
    /// the caller can dispatch it — the widget can see neither the left file tree nor the
    /// browser preference, so it hands the target back rather than acting on it. `None` when
    /// clickable paths are off, or when a plain click missed a path that exists (a ctrl-click
    /// is ungated: the token a human most wants off their screen is often one that does not
    /// exist yet). `idx` is an active-tab pane.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_link_activate(
        &mut self,
        idx: usize,
        x: f32,
        y: f32,
        ctrl: bool,
    ) -> Option<avada_terminal_widget::LinkAction> {
        if !self.settings.clickable_paths {
            return None;
        }
        let p = self.active_tab_mut().panes.get_mut(idx)?;
        let (w, h) = p.surf;
        p.pane.activate_link(x, y, w, h, ctrl)
    }

    /// Right-click on a link: open the menu the target deserves, anchored at window-logical
    /// `(ax, ay)`. A file gets the file row menu — the same rows a right-click on
    /// it in the left panel offers — a URL or a commit gets an open/copy menu. Returns `false`
    /// when nothing under the pointer is a link (or clickable paths are off), so the widget can
    /// let the click be the paste it has always been; only what is underlined earns a menu,
    /// because taking paste away from a token that was never a link would be a surprise.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_link_context(&mut self, idx: usize, x: f32, y: f32, ax: f32, ay: f32) -> bool {
        if !self.settings.clickable_paths {
            return false;
        }
        let Some(p) = self.active_tab_mut().panes.get_mut(idx) else {
            return false;
        };
        let (w, h) = p.surf;
        let Some(hit) = p.pane.link_at(x, y, w, h) else {
            return false;
        };
        if hit.is_url || hit.is_commit {
            self.ctx = Some(crate::contextmenu::link_menu(&hit, ax, ay));
            self.dirty = true;
        } else {
            self.open_file_context(Path::new(&hit.abs_path), ax, ay);
        }
        true
    }

    // ---- the window's project anchor ----
    //
    // What is left of the built-in explorer after it became `bshuler/avada-files`: the
    // project it was rooted at. The git view reads it, a new pane inherits it as its cwd,
    // and a files module is handed it as its workspace root.

    /// Where the window should root itself when it has not been given a root yet: the
    /// repository containing the focused pane's live cwd, that cwd if it is in no repo, and
    /// only then the home directory. A tree rooted at `/` is technically the whole disk and
    /// practically useless.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn default_project_root(&self) -> PathBuf {
        if let Some(cwd) = self.focused_cwd() {
            if let Some(root) = crate::sidebar::git_root_of(&cwd) {
                return root;
            }
            return PathBuf::from(cwd);
        }
        std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// The focused pane's live cwd, or `None` when it has none yet (a pane whose shell has
    /// not reported one — the panel must not root itself on an empty string).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn focused_cwd(&self) -> Option<String> {
        self.active_tab()
            .panes
            .get(self.active_tab().focused)
            .and_then(|p| p.cwd.clone())
            .filter(|c| !c.is_empty())
    }

    /// Re-anchor the project on the SELECTED pane (K).
    ///
    /// Every module surface on the rail describes the pane you are looking at, so focusing a
    /// pane in a different project moves them there. Three guards keep that from being
    /// expensive or rude, in order:
    ///
    /// * the anchor cwd is compared first, so the common tick — nothing moved — costs one
    ///   string compare and touches no filesystem;
    /// * a `cd` that stays inside the same repository derives the same root, and a root that
    ///   did not move is not re-applied, so the tree you had open does not collapse under you;
    /// * an explicit re-root (a module's "go up", a chosen directory) leaves the anchor
    ///   alone and therefore survives until the selected pane itself changes: it is the
    ///   human's.
    ///
    /// Nothing is re-read here beyond the root itself: a module hears the new root on its
    /// next activation and decides for itself what that costs. This used to spawn `git` when
    /// the working-tree mode was the one on screen, which is precisely the kind of decision
    /// the host stopped making when that view became a module.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn sync_left_root(&mut self) {
        let cwd = self.focused_cwd();
        if self.project_root_from == cwd && self.project_root.is_some() {
            return;
        }
        self.project_root_from = cwd;
        let want = self.default_project_root();
        if self.project_root.as_deref() == Some(want.as_path()) {
            return;
        }
        self.set_project_root(want);
    }

    // ---- the module rail (track H4) ----

    /// Fold one rail event from the module host into [`State::rail`]. Returns whether
    /// anything changed, so a tick that drains an idle channel costs no resync.
    ///
    /// A `Gone` (or a re-`Registered` set that no longer contains it) takes the active
    /// entry with it, and the resync ships `rail.active` verbatim, so the panel falls back
    /// to its own frame rather than showing a head with no module behind it.
    pub fn apply_rail_event(&mut self, event: avada_core::module::RailEvent) -> bool {
        // One host stream, two surfaces. The pane store is fed here rather than inside
        // `ModuleRail` because a module pane is not part of the left panel at all — it is a
        // pane in the workspace, and the panel has no business owning its rows.
        match &event {
            avada_core::module::RailEvent::Rows {
                module,
                entry,
                target: avada_core::module::RowTarget::Pane,
                rows,
            } => crate::module_ui::rows::set(module, entry, rows.clone()),
            avada_core::module::RailEvent::Gone { module } => {
                crate::module_ui::rows::forget(module);
                crate::module_ui::grid::forget(module);
                crate::module_ui::doc::forget(module);
                crate::module_ui::image::forget(module);
            }
            _ => {}
        }
        self.rail.apply(event);
        self.dirty = true;
        true
    }

    /// A module entry on the strip was clicked. Activates it and queues an `activate` for
    /// the host. A key no module registered (the entry was unregistered between the paint
    /// and the click) is a no-op, not a blank panel.
    pub fn rail_activate(&mut self, key: &str) {
        if !self.rail.activate(key) {
            return;
        }
        if let Some((module, entry)) = crate::leftpanel::split_key(key) {
            self.rail_requests.push(RailRequest::Activate {
                module,
                entry: entry.to_string(),
            });
        }
        self.left_panel_open = true;
        self.dirty = true;
    }

    /// Leave the module entry: the panel goes back to its own frame — the tab/pane tree
    /// and the detached list.
    ///
    /// Clearing `rail.active` is the whole of it. The panel's view is no longer a mode
    /// index anyone has to keep in step: `RailAdapter.active` IS the view, the resync
    /// writes it from here every frame, and an empty key draws the frame.
    pub fn rail_deactivate(&mut self) {
        self.rail.deactivate();
        self.dirty = true;
    }

    /// A row under the active module entry was clicked. The row's `data` payload travels
    /// back with the gesture — the module put it there precisely so it does not have to
    /// keep a row table of its own — and the module answers by re-projecting its rows, so
    /// nothing here mutates the list.
    pub fn rail_row(&mut self, key: &str, row: &str, gesture: RailGesture) {
        let Some((module, entry)) = crate::leftpanel::split_key(key) else {
            return;
        };
        let Some(r) = self.rail.rows(key).iter().find(|r| r.id == row) else {
            return;
        };
        self.rail_requests.push(RailRequest::Row {
            module,
            entry: entry.to_string(),
            target: avada_core::module::RowTarget::Rail,
            row: row.to_string(),
            data: r.data.clone(),
            gesture,
        });
        self.dirty = true;
    }

    /// A row inside a module's own pane was clicked. The pane twin of [`State::rail_row`]:
    /// same queue, same round trip back to the module, `target` the one difference.
    ///
    /// The row's `data` is looked up in the pane store rather than taken from the click,
    /// because the view carries only what it draws — the opaque payload never reaches Slint
    /// and must not, since it is the module's private business.
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn module_pane_row(
        &mut self,
        module: &avada_core::rights::ModuleId,
        surface: &str,
        row: &str,
        gesture: RailGesture,
    ) {
        let rows = crate::module_ui::rows::rows(module, surface);
        let Some(r) = rows.iter().find(|r| r.id == row) else {
            return;
        };
        self.rail_requests.push(RailRequest::Row {
            module: module.clone(),
            entry: surface.to_string(),
            target: avada_core::module::RowTarget::Pane,
            row: row.to_string(),
            data: r.data.clone(),
            gesture,
        });
        self.dirty = true;
    }

    /// A row under the active module entry was right-clicked, at window-logical `(x, y)`.
    ///
    /// Two destinations, which is why it is not `rail_row(.., Context)`. The module hears
    /// the gesture like any other — it may want to re-project, or to note which row the
    /// human is pointing at. And when the row's `data` carries a `path`, the HOST opens its
    /// own file menu over it, so a tier-1 module inherits the app's whole "Open in…" list
    /// (`docs/module-contract.md` §10.6) without shipping a single menu row. A row with no
    /// `path` still reaches the module; there is simply no host menu to draw for it.
    ///
    /// A row may also carry a `git` object ([`GitOrigin`]), which adds the diff verb to that
    /// menu. That is how the git module gets a verb it is not allowed to perform: spawning
    /// `git show` in a pane is running a command, and no module may do that — but the HOST
    /// may, on a row whose own module said which revision the row came from.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn rail_context(&mut self, key: &str, row: &str, x: f32, y: f32) {
        self.rail_row(key, row, RailGesture::Context);
        let found = self.rail.rows(key).iter().find(|r| r.id == row).map(|r| {
            (
                r.data
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(PathBuf::from),
                GitOrigin::from_row(r.data.get("git")),
            )
        });
        if let Some((Some(path), origin)) = found {
            self.open_file_context_git(&path, origin, x, y);
        }
    }

    /// Take everything queued for the module host since the last drain.
    pub fn take_rail_requests(&mut self) -> Vec<RailRequest> {
        std::mem::take(&mut self.rail_requests)
    }

    // ---- module capability rights (track H2) ----

    /// The opaque key the rights store files this window's workspace column under: the
    /// workspace file's path, or `None` when this window has never been saved to / opened
    /// from one. An unsaved window has no workspace column — the page hides it rather
    /// than writing overrides into a name that is about to change.
    pub fn rights_workspace(&self) -> Option<String> {
        self.workspace_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Remember the workspace file this window is bound to and pull that workspace's
    /// rights overrides in. The one place `workspace_path` is written, so the rights
    /// column can never be a workspace behind the tabs on screen.
    pub fn set_workspace_path(&mut self, path: std::path::PathBuf) {
        self.rights
            .lock()
            .unwrap()
            .load_workspace(&path.to_string_lossy());
        self.workspace_path = Some(path);
    }

    /// Apply one rights command from the Preferences page or the ask toast. Whatever the
    /// service could not finish alone — a held update to re-sign, an ask whose answer a
    /// blocked module is waiting for — is queued in [`State::rights_effects`] for the
    /// host/install-store owner; an I/O failure is logged and dropped, since a rights file
    /// that would not write must not take the window down with it.
    pub fn rights_apply(&mut self, cmd: &crate::prefs::rights::RightsCommand) {
        let ws = self.rights_workspace();
        let mut rights = self.rights.lock().unwrap();
        let applied = crate::prefs::rights::apply(&mut rights, cmd, ws.as_deref());
        match applied {
            Ok(crate::prefs::rights::Applied::Nothing) => return,
            Ok(crate::prefs::rights::Applied::Selected(i)) => {
                self.rights_selected = rights.modules().get(i).map(|r| r.module_id.clone());
            }
            Ok(crate::prefs::rights::Applied::Written) => {}
            Ok(effect) => self.rights_effects.push(effect),
            Err(e) => {
                tracing::warn!(error = %e, "rights: could not write the decision");
                return;
            }
        }
        drop(rights);
        self.dirty = true;
    }

    /// Take everything the rights page has queued for the module host and the install
    /// store since the last drain.
    pub fn take_rights_effects(&mut self) -> Vec<crate::prefs::rights::Applied> {
        std::mem::take(&mut self.rights_effects)
    }

    /// Point the window's project anchor at `dir`.
    ///
    /// Once the whole tree the explorer drew; now only the anchor, because the tree itself
    /// belongs to `bshuler/avada-files`. The module keeps its own root and hears about this
    /// one through `module.activate`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_project_root(&mut self, dir: PathBuf) {
        self.project_root = Some(dir);
        self.dirty = true;
    }

    /// The active entry's filter box was edited.
    ///
    /// Notified, never applied: `rail.query` tells the module what was typed and the module
    /// answers with the list it wants drawn. A host that filtered the rows it already has
    /// would be guessing at a vocabulary it does not own — the module's rows may be a tree
    /// whose matches are not even loaded, which is exactly the case for a file explorer.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn rail_query(&mut self, query: &str) {
        let Some(key) = self.rail.active.clone() else {
            return;
        };
        let Some((_, entry)) = key.split_once('#') else {
            return;
        };
        self.emit_module_event(
            avada_core::module::methods::events::RAIL_QUERY,
            serde_json::json!({ "entry": entry, "query": query }),
        );
    }

    /// The rail key of the entry a files module registered, if one is live.
    ///
    /// Matched on the ENTRY id rather than the module id, so a fork or a replacement that
    /// registers `files` inherits every reveal in the app without the host learning its
    /// name. The host names a contract, not a vendor.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn files_entry_key(&self) -> Option<String> {
        self.entry_key(FILES_ENTRY)
    }

    /// The rail key of the entry a git module registered, if one is live.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn git_entry_key(&self) -> Option<String> {
        self.entry_key(GIT_ENTRY)
    }

    /// The rail key of whichever live entry answers to `id`, by ENTRY id and not by module
    /// id — see [`State::files_entry_key`] for why that is the identity that matters.
    fn entry_key(&self, id: &str) -> Option<String> {
        self.rail
            .entries()
            .into_iter()
            .find(|v| v.entry.id == id)
            .map(|v| v.key)
    }

    /// Queue one host event for every module that subscribed to `kind`.
    ///
    /// Drained by whoever owns the `module::Host` alongside
    /// [`State::take_rail_requests`]; `avada_core::module::Host::emit` is the far end. The
    /// state layer deliberately does not know whether anybody is listening — that is a fact
    /// about live child processes, and it changes between the queue and the drain.
    pub fn emit_module_event(&mut self, kind: &str, payload: serde_json::Value) {
        self.module_events.push((kind.to_string(), payload));
        self.dirty = true;
    }

    /// Take everything queued for `Host::emit` since the last drain.
    pub fn take_module_events(&mut self) -> Vec<(String, serde_json::Value)> {
        std::mem::take(&mut self.module_events)
    }

    /// Record that `surface` (a pane of `module`) was opened by the app, not by
    /// `host.panes.spawn`, so the host must be told before the module tries to render into it.
    ///
    /// Queued for the same re-entrancy reason as [`State::emit_module_event`]; the drain
    /// hands it to `avada_core::module::Host::note_opener_pane`, which registers the surface
    /// with the per-module pane guard. Must be queued before the matching `DOC_OPEN` event.
    pub fn note_opener_pane(&mut self, module: avada_core::rights::ModuleId, surface: String) {
        self.opener_panes.push((module, surface));
        self.dirty = true;
    }

    /// Take the opener-routed pane registrations queued since the last drain.
    pub fn take_opener_panes(&mut self) -> Vec<(avada_core::rights::ModuleId, String)> {
        std::mem::take(&mut self.opener_panes)
    }

    /// Replace the file-open claim table with the host's current projection.
    ///
    /// Called each module tick from whoever owns the `Host` (see
    /// `ModuleRuntime::apply`). The table changes only when a module is installed or
    /// removed, so re-storing an identical `Vec` most ticks is cheap and keeps the state
    /// layer free of any dependency on the live host.
    pub fn set_module_openers(&mut self, openers: Vec<avada_core::module::Opener>) {
        self.module_openers = openers;
    }

    /// Which module pane, if any, an installed module claims for `path`'s extension.
    ///
    /// The extension is matched lowercase against [`State::module_openers`], which the host
    /// already sorted by (extension, module, surface). The first entry on the extension is
    /// therefore the tie-break winner — the lowest module id — so a linear scan for the
    /// first match is the whole resolution. `None` when the path has no extension, no module
    /// claims it, or the winning claim names a surface that no longer validates; every such
    /// case falls back to the caller's built-in viewer.
    pub fn opener_for_path(&self, path: &Path) -> Option<avada_core::tools::kind::ModulePaneRef> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        let hit = self.module_openers.iter().find(|o| o.ext == ext)?;
        avada_core::tools::kind::ModulePaneRef::new(hit.module.as_str(), &hit.surface, None)
    }

    /// Show `path` in whichever module owns the `files` rail entry: open the panel on that
    /// entry and emit `files.reveal`. The module expands the directories leading to the
    /// path, marks its row `selected`, and the host scrolls to the mark — see
    /// `docs/module-contract.md` §10.1 and §10.4.
    ///
    /// `line`/`col` come from a `file:line:col` hit in a pane's output and ride along in the
    /// payload for whichever tool opens the file next.
    ///
    /// With no files module installed there is nothing to reveal *into*, so the human is
    /// told that rather than being given a panel that silently does nothing.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reveal_in_files(&mut self, path: &Path, line: Option<u32>, col: Option<u32>) {
        let Some(key) = self.files_entry_key() else {
            self.toast_active("install the Files module to reveal paths");
            return;
        };
        // Re-anchor on the revealed path's own project. A click in a pane running somewhere
        // else is exactly when the project should follow, and the module reads this root
        // back on its next activation.
        let base = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent().map(|p| p.to_path_buf()).unwrap_or_default()
        };
        if !base.as_os_str().is_empty() {
            let root = crate::sidebar::git_root_of(&base.to_string_lossy()).unwrap_or(base);
            if self.project_root.as_deref() != Some(root.as_path()) {
                self.set_project_root(root);
            }
        }
        // Claim the follow-the-focused-pane anchor for the root just chosen. A reveal
        // normally OPENS the panel, and while it was closed `sync_left_root` never ran — so
        // the anchor still holds whatever cwd was current the last time it was open. Left
        // stale, the very next tick would decide the panel had fallen behind and re-root
        // onto the focused pane's project, undoing the reveal before the human's eye
        // reached the panel.
        self.project_root_from = self.focused_cwd();
        self.rail_activate(&key);
        let mut payload = serde_json::json!({ "path": path.display().to_string() });
        if let Some(l) = line {
            payload["line"] = serde_json::json!(l);
        }
        if let Some(c) = col {
            payload["col"] = serde_json::json!(c);
        }
        self.emit_module_event(avada_core::module::methods::events::FILES_REVEAL, payload);
        // Held open across the round trip: the module has to hear the event, walk its tree
        // and push rows back before there is a `selected` row to scroll to at all.
        self.rail_scroll_hold = Some(std::time::Instant::now());
    }

    /// Open the row menu for `path`, anchored at window-logical `(x, y)`.
    ///
    /// Reached from a clicked filename in a pane and from a `context` gesture on a module
    /// row: the menu is the HOST's, so a tier-1 module gets the app's own "Open in…" list
    /// without shipping one (`docs/module-contract.md` §10).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_file_context(&mut self, path: &Path, x: f32, y: f32) {
        self.open_file_context_git(path, None, x, y);
    }

    /// [`State::open_file_context`], plus the revision the path was listed from.
    ///
    /// Split rather than defaulted so the plain form stays the one every caller that has no
    /// opinion about git reaches for — a filename clicked in a pane's output is a filename,
    /// and nothing about it says which commit it came from.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_file_context_git(
        &mut self,
        path: &Path,
        origin: Option<GitOrigin>,
        x: f32,
        y: f32,
    ) {
        self.ctx = Some(crate::contextmenu::file_menu(
            self,
            path,
            origin.as_ref(),
            x,
            y,
        ));
        self.dirty = true;
    }

    // ---- git links (J) ----
    //
    // All that is left in the host of what used to be a whole GIT mode. The working tree is
    // `bshuler/avada-git`'s to draw now; what stays here is the one thing a module cannot do
    // for itself — turn a hash printed in a pane into a panel showing that commit.

    /// Show one commit in whichever module owns the `git` rail entry: open the panel on that
    /// entry and emit `git.commit`. The module loads the commit through `host.git.commit` and
    /// projects its files, exactly as [`State::reveal_in_files`] does for a path.
    ///
    /// Returns `false` when git cannot resolve `hash` — a hash that was printed an hour ago
    /// can be gone by the click (a fetch --prune, a rebase), and a panel that switched views
    /// for nothing is worse than one that stayed where it was. Resolved HERE rather than in
    /// the module because that answer decides whether the click did anything at all, and the
    /// module's reply would arrive long after the caller needed to know.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn show_commit(&mut self, cwd: &str, hash: &str) -> bool {
        let Some(commit) = avada_core::git::load_commit(std::path::Path::new(cwd), hash) else {
            return false;
        };
        let Some(key) = self.git_entry_key() else {
            self.toast_active("install the Git module to show commits");
            // The commit is real; there is simply nowhere to draw it. Reported as success so
            // the caller does not ALSO claim the hash was bad, which would be a second and
            // untrue explanation for the same nothing-happened.
            return true;
        };
        // Re-root on the commit's own repository first. A hash printed by a pane in another
        // project otherwise leaves the panel's header and its rows describing two places.
        if self.project_root.as_deref() != Some(commit.root.as_path()) {
            self.set_project_root(commit.root.clone());
        }
        // Claim the follow-the-focused-pane anchor, for the same reason `reveal_in_files`
        // does: the next tick would otherwise decide the panel had fallen behind and drag
        // the root back to the focused pane, undoing this before the human's eye arrived.
        self.project_root_from = self.focused_cwd();
        self.rail_activate(&key);
        self.emit_module_event(
            avada_core::module::rpc::GIT_COMMIT_EVENT,
            serde_json::json!({
                "root": commit.root.display().to_string(),
                "rev": commit.hash,
                "short": commit.short,
            }),
        );
        self.left_panel_open = true;
        // Held open across the round trip, like a reveal: the module has to hear the event
        // and push the commit's rows back before there is anything on screen to look at.
        self.rail_scroll_hold = Some(std::time::Instant::now());
        self.dirty = true;
        true
    }

    // ---- sidebar / projects ----

    /// Show/hide the whole right-edge rail (`Ctrl+Shift+B`, the ☰ menu, the palette).
    /// Persisted like the other appearance prefs; hiding it also collapses the flyout.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_sidebar(&mut self) {
        self.settings.show_sidebar = !self.settings.show_sidebar;
        if !self.settings.show_sidebar {
            self.sidebar_open = false;
        }
        prefs::save(&self.settings);
        self.dirty = true;
    }

    /// Toggle the projects flyout behind the 📁 icon; refresh the list when opening it.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_projects(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open {
            self.reload_projects();
            self.reminders_open = false;
            self.closed_open = false;
        }
        self.dirty = true;
    }

    // ---- the left slide-out panel (mux plan M5) ----

    /// Toggle the left panel; refresh the workspace library when opening it (the same
    /// closed→open refresh the projects flyout does, so a workspace saved from another
    /// window shows up without a restart).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_left_panel(&mut self) {
        self.left_panel_open = !self.left_panel_open;
        self.dirty = true;
    }

    /// Focus pane `idx` of tab `ti` from the panel's workspace tree: switch to that tab
    /// first (a tree click on a background tab's pane means "take me there"), then focus.
    /// Out-of-range indices are ignored — they arrive from a UI model snapshot.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn focus_pane_in_tab(&mut self, ti: usize, idx: usize) {
        if ti >= self.tabs.len() || idx >= self.tabs[ti].panes.len() {
            return;
        }
        self.switch_tab(ti);
        self.focus_pane(idx);
    }

    /// Every session uid THIS window is holding ALIVE — laid out in any tab, parked as a
    /// reminder, or sitting on the recently-closed history. The left panel subtracts these
    /// to decide which live sessions are detached.
    ///
    /// The set is [`Self::session_uids`]'s, deliberately: that is the list this window kills
    /// when it closes, i.e. the exact inventory of sessions it is responsible for. A closed
    /// tab's or pane's session belongs in it — its PTY is still running so "Reopen Closed" can
    /// bring them back — and leaving them out would offer them in the panel's DETACHED list,
    /// where one click would re-host a session the reopen stack still points at (reopening
    /// the tab afterwards would then duplicate the uid in two panes).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn claimed_uids(&self) -> std::collections::HashSet<String> {
        self.session_uids().into_iter().collect()
    }

    /// Save the active tab into the panel's workspace library (no file dialog — that's what
    /// the library is for). Named after the tab; a collision gets a numeric suffix rather
    /// than overwriting the earlier snapshot.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_workspace_to_library(&mut self) {
        self.save_workspace_to_library_as(None);
    }

    /// [`Self::save_workspace_to_library`], with the name supplied rather than taken from
    /// the active tab.
    ///
    /// `None` keeps the tab's own title, which is what the panel's Save button has always
    /// done; a module passes a name because it has no tab strip to name things after. A
    /// blank name is treated as no name at all rather than saved as an empty label.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_workspace_to_library_as(&mut self, name: Option<&str>) {
        if self.refuse_saving_system_tab() {
            return;
        }
        let file = self.to_library_workspace_file();
        let name = match name.map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => self.active_tab().title.to_string(),
        };
        if crate::leftpanel::save_to_library(&name, &file).is_none() {
            tracing::warn!("failed to save workspace into the library");
        }
        self.dirty = true;
    }

    /// Open a saved workspace by path.
    ///
    /// The path form is the one a module gets: `host.workspace.list` hands out paths, not
    /// row indices, and an index would be a race anyway — the drawer can be rescanned
    /// between the list and the open. The index form above resolves to this.
    #[tracing::instrument(level = "debug", skip(self, mgr))]
    pub fn open_workspace_path(&mut self, path: &std::path::Path, mgr: &SessionManager) {
        let Some(file) = read_workspace(path) else {
            tracing::warn!("{} is not a valid workspace", path.display());
            self.dirty = true;
            return;
        };
        self.load_workspace(file, mgr);
    }

    /// Save every non-empty tab in this window as a new set, named `name` or, failing that,
    /// after the active tab. The set-drawer twin of [`Self::save_workspace_to_library_as`].
    ///
    /// The member workspaces go to [`paths::set_members_dir`], NOT the library: a set of N
    /// tabs generates N files, and the library is for the workspaces the user saved by hand.
    ///
    /// A numeric suffix on collision rather than an overwrite — the same contract as
    /// [`Self::save_workspace_to_library_as`]. The suffix goes on the *display* name, not
    /// the slug, so the unique name flows through to the member filenames too (`save_set_to`
    /// stems those from it) and a second save of the same tab title cannot clobber the first
    /// set's members.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_set_to_library_as(&mut self, name: Option<&str>) {
        let dir = paths::sets_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            tracing::warn!("failed to create the sets directory");
            return;
        }
        let title = match name.map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => self.active_tab().title.trim().to_string(),
        };
        let base = if title.is_empty() {
            "set".to_string()
        } else {
            title
        };
        let mut name = base.clone();
        let mut n = 2;
        while dir.join(format!("{}.json", sets::slug(&name))).exists() {
            name = format!("{base} {n}");
            n += 1;
            if n > 999 {
                tracing::warn!("too many sets named {base:?}");
                return;
            }
        }
        let path = dir.join(format!("{}.json", sets::slug(&name)));
        if self
            .save_set_to(&path, &paths::set_members_dir(), &name)
            .is_none()
        {
            tracing::warn!("failed to save the set into the sets drawer");
        }
        self.dirty = true;
    }

    /// Open a saved set by path — the path-shaped sibling of
    /// [`Self::open_workspace_path`], for the same reason.
    #[tracing::instrument(level = "debug", skip(self, mgr))]
    pub fn open_set_path(&mut self, path: &std::path::Path, mgr: &SessionManager) {
        if self.open_set_from(path, mgr) == 0 {
            // Nothing loaded: the set is stale (deleted or corrupted since it was listed),
            // or every member reference is dead. Whoever listed it — the workspace module —
            // rescans on its own tick, so there is no host-side cache left to invalidate.
            tracing::warn!("{} loaded no member workspaces", path.display());
        }
    }

    /// Adopt detached session `uid` into the active tab: a re-attach, not a respawn — the
    /// spec carries the uid, so `make_pane_from_spec` re-hosts the live session and seeds
    /// the fresh grid from its replay buffer.
    ///
    /// Two guards, in order, because they answer different questions.
    ///
    /// **In-process** ([`Self::claimed_uids`]): ignored if this window is already holding
    /// the session anywhere — laid out, parked as a reminder, or on the reopen stack;
    /// adopting one of those would give the same uid two homes inside one process.
    ///
    /// **Cross-process (M7): claim first, adopt second.** The claim is a compare-and-set in
    /// the daemon's registry, so if two windows (in two processes) click adopt on the same
    /// orphan at the same moment, exactly one of them is granted it and the other returns
    /// here having changed nothing. Losing is silent by design: the winner's claim reaches
    /// this process on the next pushed snapshot and the row simply leaves the DETACHED list.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn adopt_detached_session(&mut self, uid: &str, mgr: &SessionManager) {
        if uid.is_empty() || self.claimed_uids().contains(uid) {
            return;
        }
        if !mgr.claim_session(uid) {
            return;
        }
        self.attach_panes_from_specs(
            mgr,
            &[PaneSpec {
                uid: Some(uid.to_string()),
                ..Default::default()
            }],
        );
        self.dirty = true;
    }

    /// Reload the cached project rail from core after the control plane changed
    /// `projects.json` off the UI thread (an MCP `add_project` / rename / recolor / remove,
    /// or a project-opening pane bumping recency). Same refresh seam the in-app project
    /// mutations use; the dirty signal is driven by [`crate::control_host::ControlHost::sync`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn refresh_projects(&mut self) {
        self.reload_projects();
        self.dirty = true;
    }

    /// Reload the remembered projects and hand every pane the roots, so a relative path a
    /// pane cannot place on its own can be placed inside a project the app knows. The single
    /// seam for the list: every site that used to assign `self.projects` directly goes
    /// through here so no pane is left with a stale set. The constructor is the one
    /// exception (no panes exist yet), and the appearance preview pane is never given roots
    /// (it shows canned output and linkifies nothing).
    #[tracing::instrument(level = "debug", skip(self))]
    fn reload_projects(&mut self) {
        self.projects = sidebar::list();
        self.push_project_roots();
    }

    /// The remembered projects' roots, in list order (newest first). Order does not matter
    /// to the pane — it answers only when exactly one root holds the file — so no sort.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn project_roots(&self) -> Vec<String> {
        Self::roots_of(&self.projects)
    }

    /// The pure half of [`Self::project_roots`]: a project's `path` is its normalized git
    /// root, which is exactly the base a relative token wants.
    fn roots_of(projects: &[Project]) -> Vec<String> {
        projects.iter().map(|p| p.path.clone()).collect()
    }

    /// Hand the current roots to every pane in every tab. Cheap when nothing changed: the
    /// pane compares before it drops its verify cache.
    #[tracing::instrument(level = "debug", skip(self))]
    fn push_project_roots(&mut self) {
        let roots = self.project_roots();
        for t in &mut self.tabs {
            for p in &mut t.panes {
                p.pane.set_project_roots(roots.clone());
            }
        }
    }

    /// The cached project rows as `(name, color)` for the flyout.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn project_rows(&self) -> Vec<(SharedString, Color)> {
        self.projects
            .iter()
            .map(|p| (p.name.as_str().into(), parse_hex(&p.color)))
            .collect()
    }

    /// Open project `idx` (from the flyout): as the windows the checkout describes for
    /// itself if it describes any, else in a new pane cd'd into its repo, focused.
    /// Collapses the flyout afterwards (mirrors the Electron click behaviour).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn open_project(&mut self, idx: usize, mgr: &SessionManager) {
        let Some(p) = self.projects.get(idx).cloned() else {
            return;
        };
        self.sidebar_open = false;
        if self.open_project_windows(&p.path, mgr) {
            return;
        }
        self.add_pane_cwd(mgr, Some(p.path.clone()), Some(parse_hex(&p.color)));
    }

    /// Open the windows `<root>/.avada/project.json` describes, and say whether it
    /// did. The layout as a property of the *checkout*: cloned with the repo, still there
    /// after a reboot or a six-month pause.
    ///
    /// Precedence is `project::resolve`'s, not a second opinion: a layout already on
    /// screen for this checkout wins, so re-opening a folder you are already working in
    /// can never replace the running panes with a months-old file. Only what is live
    /// *for this project* counts — the rest of the window is somebody else's work.
    #[tracing::instrument(level = "debug", skip_all)]
    fn open_project_windows(&mut self, root: &str, mgr: &SessionManager) -> bool {
        use avada_core::workspace::project;
        let repo = match project::discover_project(root) {
            Ok(Some(f)) => f,
            Ok(None) => return false,
            Err(e) => {
                tracing::debug!("project file for {root}: {e}");
                return false;
            }
        };
        let mut live = self.to_session_file();
        if let Some(groups) = live.groups.as_mut() {
            for g in groups.iter_mut() {
                g.panes
                    .retain(|pane| pane.cwd.as_deref().is_some_and(|c| cwd_in_project(c, root)));
            }
            groups.retain(|g| !g.panes.is_empty());
        }
        if project::resolve(Some(&live), Some(&repo.workspace)) != project::Source::Repo {
            return false;
        }
        self.load_workspace(repo.workspace, mgr).is_some()
    }

    /// "Save to this repo": write the active tab's layout into the checkout the focused
    /// pane sits in, as `.avada/project.json`.
    ///
    /// The root is found by walking up from the pane's cwd, so a `.git` checkout with no
    /// `.avada/` yet is where a first save lands — that is the whole point of the
    /// walk stopping at `.git`. Pane cwds under the root are stored relative to it, so
    /// the file still means something on a machine that keeps its checkouts elsewhere.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_project(&mut self) {
        use avada_core::workspace::project;
        if self.refuse_saving_system_tab() {
            return;
        }

        let f = self.active_tab().focused;
        // The focused pane's live cwd, else any pane in this tab that has reported one —
        // a tab whose focus happens to sit on a fresh pane still knows where it is.
        let Some(cwd) = self
            .active_tab()
            .panes
            .get(f)
            .and_then(|p| p.cwd.clone())
            .or_else(|| self.active_tab().panes.iter().find_map(|p| p.cwd.clone()))
        else {
            self.toast_active("no folder to save to");
            return;
        };
        let Some(root) = project::find_project_root(&cwd) else {
            self.toast_active("not inside a checkout");
            return;
        };
        let file = self.to_library_workspace_file();
        match project::write_project(&root.dir, &project::relativize_cwds(&file, &root.dir)) {
            Ok(_) => self.toast_active("saved to .avada"),
            Err(e) => {
                tracing::debug!("save to repo: {e}");
                self.toast_active("could not save to .avada");
            }
        }
    }

    /// Flash a line on the focused pane — the same transient the widget uses for
    /// copy/paste confirmations, for an action with no other visible result.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toast_active(&mut self, msg: &str) {
        let f = self.active_tab().focused;
        if let Some(p) = self.active_tab_mut().panes.get_mut(f) {
            p.pane.set_toast(msg.to_string());
        }
        self.dirty = true;
    }

    /// Recolor project at flyout row `idx` to palette swatch `swatch`, persist via core,
    /// and refresh the cache so the dot updates immediately. Already-open panes inside the
    /// project are retinted too (new panes pick the color up via the cwd tint; existing ones
    /// must not be left on the stale accent) — EXCEPT panes the user explicitly recolored
    /// to something other than the project tint, whose pin wins (see [`Self::recolor_pane`]).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_project_color(&mut self, idx: usize, swatch: usize) {
        let Some(p) = self.projects.get(idx) else {
            return;
        };
        let Some(color) = projects::PROJECT_COLORS.get(swatch) else {
            return;
        };
        let project_path = p.path.clone();
        let old = parse_hex(&p.color);
        let new = parse_hex(color);
        projects::set_project_color(&p.id, color);
        self.reload_projects();
        // Propagate to open panes across ALL tabs: same matcher as the cwd tint
        // (`note_pane_cwd`): pane cwd → enclosing git root → the store's path key.
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                if !pane
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| cwd_in_project(cwd, &project_path))
                {
                    continue;
                }
                if !follows_project_tint(pane.pinned_accent, old) {
                    continue;
                }
                pane.accent = new;
                pane.pinned_accent = Some(new);
            }
        }
        // The same mutate→set-dirty seam every pane-chrome change uses: the pump
        // republishes the pane models on the next tick, so the UI updates immediately.
        self.dirty = true;
    }

    /// Rename project at flyout row `idx` (no-op on an empty/unchanged name).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn rename_project(&mut self, idx: usize, name: &str) {
        let name = name.trim();
        let Some(p) = self.projects.get(idx) else {
            return;
        };
        if name.is_empty() || name == p.name {
            return;
        }
        let id = p.id.clone();
        projects::rename_project(&id, name);
        self.reload_projects();
        self.dirty = true;
    }

    /// Forget project at flyout row `idx`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn remove_project(&mut self, idx: usize) {
        let Some(p) = self.projects.get(idx) else {
            return;
        };
        projects::remove_project(&p.id);
        // The pane side matters most here: a root that left the list must stop
        // answering, and the smaller set is what makes the pane drop its cache.
        self.reload_projects();
        self.dirty = true;
    }

    /// Open the "Add project" dialog (the ＋ on the sidebar's PROJECTS header).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_add_project(&mut self) {
        self.add_project_error.clear();
        self.overlay = Overlay::AddProject;
        self.dirty = true;
    }

    /// Submit the Add-Project dialog: validate the typed path (must exist and be a
    /// directory), then add it explicitly via core (a git repo is NOT required; adding an
    /// already-known dir is a no-op) and refresh the cached list so the flyout picks it up.
    /// On a bad path the dialog stays open with an inline error instead of closing.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn submit_add_project(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            self.add_project_error = "Enter a directory path".to_string();
            self.dirty = true;
            return;
        }
        if !std::path::Path::new(path).is_dir() {
            self.add_project_error = "Path doesn't exist or isn't a directory".to_string();
            self.dirty = true;
            return;
        }
        let _ = projects::add_project_explicit(path);
        self.reload_projects();
        self.close_overlay();
        self.dirty = true;
    }

    /// Goals system: hand a free-text goal to the project's goals orchestrator, spawning one if
    /// it isn't already live. Either way the goal is not written to the pty immediately — it is
    /// queued as a [`PendingGoal`] and delivered by the app tick (`deliver_pending_goals`) once the
    /// pane's Claude TUI is ready (marker-gated, with a fallback timeout), reusing the resume
    /// queue's proven gap+CR cadence. For an existing orchestrator that's near-instant (its marker
    /// is already old); for a fresh spawn it waits out the boot. The orchestrator self-registers
    /// `role`/`project` meta and drives spec agents → impl agents (see the goal-orchestrator skill).
    ///
    /// Returns `false` (with an eprintln) only if the orchestrator persona file can't be found.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn submit_new_goal(
        &mut self,
        mgr: &SessionManager,
        project_path: &str,
        intent: &str,
        orch_model: &str,
        spec_model: &str,
        impl_model: &str,
    ) -> bool {
        let intent = intent.trim();
        if intent.is_empty() || project_path.is_empty() {
            return false;
        }

        // Build the goal payload: the intent + any attached image paths (Claude reads image
        // files referenced by path — the reliable delivery; a live clipboard-paste of the same
        // images is best-effort on top) + the per-goal spec/impl model tiers as a hint the
        // orchestrator honors when it spawns those agents.
        let mut payload = intent.to_string();
        for img in &self.goal_draft_images {
            payload.push_str(&format!("\n[image: {}]", img.display()));
        }
        payload.push_str(&format!(
            "\n(models — spec: {spec_model}, implementation: {impl_model})"
        ));

        // The pane carries the project's identity (title = project name, frame = project
        // color) and the current task as its subtitle.
        let project = self.projects.iter().find(|p| p.path == project_path);
        let (proj_name, proj_color) = match project {
            Some(p) => (p.name.clone(), p.color.clone()),
            None => (String::new(), String::new()),
        };
        let subtitle = goal_subtitle(intent);

        // Existing orchestrator for this project still live? Queue the goal for it (delivered on
        // the next tick — its Claude is already up, so the marker gate passes immediately) and
        // refresh its subtitle to the newest task.
        if let Some(uid) = self.goal_orchestrators.get(project_path).cloned() {
            let alive = self
                .tabs
                .iter()
                .flat_map(|t| &t.panes)
                .any(|p| p.uid == uid);
            if alive {
                if let Some((ti, pi)) = self.find_pane(&uid) {
                    self.tabs[ti].panes[pi].subtitle = Some(subtitle.clone().into());
                }
                self.queue_pending_goal(uid, payload);
                self.close_overlay();
                self.dirty = true;
                return true;
            }
            self.goal_orchestrators.remove(project_path); // stale (pane closed) — respawn below
        }

        // Locate the goal-orchestrator persona `SKILL.md`. Bundled with the app under
        // `resources/claude/goal-orchestrator/` — mirror the packaged layouts
        // `shell_integration_dir()` handles (exe-relative, macOS `.app` Resources, and FHS
        // deb/rpm `share`/`lib`), then fall back to a `~/.claude/skills` symlink and finally the
        // dev skills-repo checkout so it works uninstalled.
        let persona = {
            let rel = std::path::Path::new("resources")
                .join("claude")
                .join("goal-orchestrator")
                .join("SKILL.md");
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
            let mut candidates: Vec<std::path::PathBuf> = Vec::new();
            if let Some(dir) = &exe_dir {
                candidates.push(dir.join(&rel));
                if let Some(prefix) = dir.parent() {
                    candidates.push(
                        prefix
                            .join("Resources")
                            .join("claude")
                            .join("goal-orchestrator")
                            .join("SKILL.md"),
                    );
                    candidates.push(prefix.join("share").join("avada").join(&rel));
                    candidates.push(prefix.join("lib").join("avada").join(&rel));
                }
            }
            if let Some(home) = std::env::var_os("HOME") {
                let h = std::path::Path::new(&home);
                candidates.push(h.join(".claude/skills/goal-orchestrator/SKILL.md"));
                candidates.push(
                    h.join("dev/agent-orchestration-skills/skills/goal-orchestrator/SKILL.md"),
                );
            }
            candidates.into_iter().find(|p| p.is_file())
        };
        let Some(persona) = persona else {
            tracing::warn!(
                "orchestrator persona not found — bundle resources/claude/goal-orchestrator/ \
                 or symlink the skill into ~/.claude/skills; goal not started"
            );
            return false;
        };

        // Spawn a fresh orchestrator in the project cwd. `--model` = the orchestrator tier; the
        // spec/impl tiers ride in as env so the persona spawns those agents with the chosen
        // models. The goal is NOT set as `startup` (that fires mid-boot, when Claude swallows it);
        // it's queued for marker-gated delivery once the pane's Claude is ready.
        // `--dangerously-skip-permissions`: the whole goal org runs unattended 24/7 — a
        // permission prompt would wedge it (and swallow the delivered goal text).
        let mut command = format!(
            "claude --dangerously-skip-permissions --append-system-prompt-file {} --model {orch_model}",
            persona.display()
        );
        // Account rotation hides the user-scoped avada MCP registration (it only lives in
        // the default `~/.claude.json`), so hand every spawned claude an explicit config that
        // re-registers it. Best-effort: a write failure just drops the flag, not the spawn.
        if let Some(mcp_config_path) = write_goals_mcp_config() {
            // `--strict-mcp-config`: load ONLY the avada server from our config, never merge
            // whatever `.mcp.json` / user-scoped servers the goal's project cwd happens to carry.
            // Keeps the goal agent's tool pool small and deterministic (just `mcp__avada__*`).
            command.push_str(&format!(
                " --mcp-config {} --strict-mcp-config",
                mcp_config_path.display()
            ));
        }
        // The user's statusLine lives only in the default `~/.claude/settings.json`; account
        // rotation points CLAUDE_CONFIG_DIR at per-account dirs that don't carry it, so agents
        // fall back to Claude's built-in statusline. Inject it via `--settings` (same
        // rotation-blindness fix as `--mcp-config`) and hand the path down as HP_GOAL_SETTINGS so
        // the persona's spec/impl spawns inherit it too. Best-effort: no statusLine ⇒ no flag.
        let goals_settings = write_goals_settings_config();
        if let Some(ref settings_path) = goals_settings {
            command.push_str(&format!(" --settings {}", settings_path.display()));
        }
        let mut env: avada_core::session::spawn::EnvMap = std::collections::HashMap::new();
        env.insert("HP_GOAL_SPEC_MODEL".to_string(), spec_model.to_string());
        env.insert("HP_GOAL_IMPL_MODEL".to_string(), impl_model.to_string());
        // The orchestrator is handed the persona *content* via `--append-system-prompt-file`,
        // not its path — so it cannot resolve `<persona dir>/SPEC.md` / `IMPL.md` when it spawns
        // spec/impl agents, and those agents silently lose their persona. Hand it the on-disk dir
        // (all three personas ship together, see build.rs) so the SKILL/SPEC spawn commands can
        // pass `$HP_GOAL_PERSONA_DIR/{SPEC,IMPL}.md`.
        if let Some(persona_dir) = persona.parent() {
            env.insert(
                "HP_GOAL_PERSONA_DIR".to_string(),
                persona_dir.to_string_lossy().into_owned(),
            );
        }
        if let Some(ref settings_path) = goals_settings {
            env.insert(
                "HP_GOAL_SETTINGS".to_string(),
                settings_path.to_string_lossy().into_owned(),
            );
        }
        env.insert(
            "AVADA_CONTROL_FILE".to_string(),
            avada_core::persistence::paths::control_json()
                .to_string_lossy()
                .into_owned(),
        );
        // Force eager MCP-tool registration. Claude Code >= 2.1.x defaults to "tool-search" mode
        // (`ENABLE_TOOL_SEARCH` unset ⇒ mode "tst"), under which EVERY MCP tool is deferred and
        // only callable after a `ToolSearch` round-trip. In an unattended goal pane that deferral
        // is fatal: the `mcp__avada__*` tools show up but never surface (ToolSearch returns
        // nothing / can even orphan a `tool_search_tool_result` and 400-brick the session). Pin
        // the mode to "standard" so the pane's Claude — and every spec/impl agent it spawns, which
        // inherit this env — registers the avada tools up front and can call them directly.
        // (The user's own sessions dodge this because their launcher already sets it; unattended
        // spawns get the CLI default, so we set it explicitly here.)
        env.insert("ENABLE_TOOL_SEARCH".to_string(), "false".to_string());
        // Project identity, so the persona can stamp it onto the spec/impl panes it spawns
        // (title = project name, frame = project color; see SKILL.md "Pane identity").
        if !proj_name.is_empty() {
            env.insert("HP_GOAL_PROJECT_NAME".to_string(), proj_name.clone());
        }
        if !proj_color.is_empty() {
            env.insert("HP_GOAL_PROJECT_COLOR".to_string(), proj_color.clone());
        }
        // Account rotation: assign this orchestrator the next account (round-robin over the
        // registry) via CLAUDE_CONFIG_DIR, and hand it the full ordered list (HP_GOAL_ACCOUNTS,
        // newline-separated) so the persona can spread + rotate its spec/impl agents across
        // accounts. No registry / single account ⇒ nothing injected (Claude uses its default).
        let accounts = avada_core::claude_accounts::config_dirs();
        if !accounts.is_empty() {
            let chosen = &accounts[self.goal_account_cursor % accounts.len()];
            self.goal_account_cursor = self.goal_account_cursor.wrapping_add(1);
            env.insert(
                "CLAUDE_CONFIG_DIR".to_string(),
                chosen.to_string_lossy().into_owned(),
            );
            let list = accounts
                .iter()
                .map(|d| d.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n");
            env.insert("HP_GOAL_ACCOUNTS".to_string(), list);
        }
        // Env-inheritance backstop. A control-spawned pane gets the APP process env + its explicit
        // spawn env, NOT the parent pane's PTY env (see `session::spawn::build_env`) — so these
        // vars reach a spec agent / worker-runner pane only if the LLM threads them through EVERY
        // spawn. Miss one and `--append-system-prompt-file $HP_GOAL_PERSONA_DIR/IMPL.md` expands to
        // `/IMPL.md` and the worker dies instantly. Mirror the STABLE, non-goal-specific vars into
        // the app's own env so `fresh_env()` hands them to every pane unconditionally. The
        // goal/project-specific vars (models, project name/color, the rotated CLAUDE_CONFIG_DIR)
        // deliberately stay per-spawn — they must not leak process-wide across concurrent projects.
        for key in [
            "HP_GOAL_PERSONA_DIR",
            "HP_GOAL_SETTINGS",
            "HP_GOAL_ACCOUNTS",
        ] {
            if let Some(val) = env.get(key) {
                std::env::set_var(key, val);
            }
        }
        let opts = NewPaneOpts {
            label: Some(if proj_name.is_empty() {
                "goals".to_string()
            } else {
                proj_name.clone()
            }),
            cwd: Some(project_path.to_string()),
            command: Some(command),
            accent: (!proj_color.is_empty()).then(|| parse_hex(&proj_color)),
            show_frame: (!proj_color.is_empty()).then_some(true),
            show_dot: (!proj_color.is_empty()).then_some(true),
            env: Some(env),
            ..Default::default()
        };
        if let Some(uid) = self.add_pane_opts(mgr, opts) {
            if let Some((ti, pi)) = self.find_pane(&uid) {
                self.tabs[ti].panes[pi].subtitle = Some(subtitle.clone().into());
            }
            self.goal_orchestrators
                .insert(project_path.to_string(), uid.clone());
            self.queue_pending_goal(uid, payload);
        }
        self.close_overlay();
        self.dirty = true;
        true
    }

    /// Enqueue a goal for robust delivery into pane `uid` (see [`PendingGoal`] /
    /// `deliver_pending_goals`). Snapshots the current draft images and clears the draft.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn queue_pending_goal(&mut self, uid: String, text: String) {
        self.pending_goals.push(PendingGoal {
            uid,
            text,
            images: std::mem::take(&mut self.goal_draft_images),
            queued_at: std::time::Instant::now(),
        });
    }

    /// Goals system: open the OS file picker to attach one or more images to the in-progress
    /// goal (step 2 of the New-goal dialog). Held as file paths until submit, when their paths
    /// are written into the goal prompt (Claude reads image files by path).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_attach_images(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
            .set_title("Attach image(s) to the goal")
            .pick_files()
        {
            self.goal_draft_images.extend(paths);
            self.dirty = true;
        }
    }

    /// Goals system: capture the current OS-clipboard image (if any) into a temp PNG and attach
    /// it to the in-progress goal. No-op (returns false) when the clipboard holds no image.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_paste_image(&mut self) -> bool {
        let Ok(mut cb) = arboard::Clipboard::new() else {
            return false;
        };
        let Ok(img) = cb.get_image() else {
            return false;
        };
        // Encode RGBA8 → PNG into a temp file the goal prompt can reference.
        let path = std::env::temp_dir().join(format!(
            "hp-goal-{}-{}.png",
            std::process::id(),
            self.goal_draft_images.len()
        ));
        let Some(buf) = image_rgba_to_png(&img) else {
            return false;
        };
        if std::fs::write(&path, buf).is_ok() {
            self.goal_draft_images.push(path);
            self.dirty = true;
            return true;
        }
        false
    }

    /// Goals system: drop one attached image from the in-progress goal (its ✕ chip).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn goal_remove_image(&mut self, idx: usize) {
        if idx < self.goal_draft_images.len() {
            self.goal_draft_images.remove(idx);
            self.dirty = true;
        }
    }

    /// Record an Escape key event and decide what to do with it. A lone tap
    /// forwards to the shell; holding Escape (rapid auto-repeat) while in
    /// fullscreen sets [`Self::esc_holding`] (so the hint + its progress fill
    /// appear) and, after [`HOLD`], leaves fullscreen. The repeat tail is
    /// swallowed so the hold doesn't spam the shell with escapes.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn note_esc(&mut self) -> EscOutcome {
        // A gap under this means "still held" (auto-repeat — incl. the OS's
        // initial repeat delay); a longer gap starts a fresh tap.
        const RAPID: std::time::Duration = std::time::Duration::from_millis(600);
        // How long to hold (from the first repeat) before leaving fullscreen.
        const HOLD: std::time::Duration = std::time::Duration::from_millis(600);

        let now = Instant::now();
        let cont = self.esc_last.is_some_and(|l| now.duration_since(l) < RAPID);
        self.esc_last = Some(now);

        if !cont {
            // Fresh tap → goes to the shell.
            if self.esc_holding {
                self.dirty = true;
            }
            self.esc_holding = false;
            self.esc_hold_start = None;
            self.esc_fired = false;
            return EscOutcome::Forward;
        }

        // Continuation (held). Start the progress clock on the first repeat.
        if !self.esc_holding {
            self.esc_holding = true;
            self.esc_hold_start = Some(now);
            self.dirty = true;
        }
        if self.fullscreen
            && !self.esc_fired
            && self
                .esc_hold_start
                .is_some_and(|s| now.duration_since(s) >= HOLD)
        {
            self.esc_fired = true;
            self.esc_holding = false;
            self.dirty = true;
            return EscOutcome::Exit;
        }
        EscOutcome::Ignore
    }

    /// Clear the held-Esc state once the auto-repeat stops (no key-release event
    /// reaches us, so we time it out). Returns whether anything changed.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn tick_esc(&mut self) -> bool {
        const RELEASE: std::time::Duration = std::time::Duration::from_millis(250);
        if self.esc_holding && self.esc_last.is_some_and(|l| l.elapsed() >= RELEASE) {
            self.esc_holding = false;
            self.esc_hold_start = None;
            self.esc_fired = false;
            self.dirty = true;
            return true;
        }
        false
    }
}

/// Phase-5: context menus + the actions they invoke. Kept in its own `impl` block so the
/// right-click feature reads as one unit (it only ever calls the same mutate→set-dirty seam).
impl State {
    // ---- context-menu lifecycle ----

    /// Open the pane header menu for active-tab pane `idx`, anchored at window-logical `(x, y)`.
    /// Built fresh so gating + checkmarks reflect the moment of the right-click; never changes
    /// the focused pane or active tab.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_pane_context(&mut self, idx: usize, x: f32, y: f32) {
        if idx < self.active_tab().panes.len() {
            self.ctx = Some(crate::contextmenu::pane_menu(self, idx, x, y, false));
            self.dirty = true;
        }
    }

    /// Open the single-layout taskbar's pane menu for pane `idx` (the `inTaskbar` variant:
    /// a leading Show row, no Maximize), anchored at window-logical `(x, y)`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_taskbar_context(&mut self, idx: usize, x: f32, y: f32) {
        if idx < self.active_tab().panes.len() {
            self.ctx = Some(crate::contextmenu::pane_menu(self, idx, x, y, true));
            self.dirty = true;
        }
    }

    /// Open the application (hamburger) menu, anchored at window-logical `(x, y)`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_app_context(&mut self, x: f32, y: f32) {
        self.ctx = Some(crate::contextmenu::app_menu(self, x, y));
        self.dirty = true;
    }

    /// Open the tab-strip menu for tab `idx`, anchored at window-logical `(x, y)`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_tab_context(&mut self, idx: usize, x: f32, y: f32) {
        if idx < self.tabs.len() {
            self.ctx = Some(crate::contextmenu::tab_menu(self, idx, x, y));
            self.dirty = true;
        }
    }

    /// Dismiss the open context menu (select / Esc / click-away).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn close_context(&mut self) {
        if self.ctx.take().is_some() {
            self.dirty = true;
        }
    }

    /// Whether a context menu is currently open.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn ctx_open(&self) -> bool {
        self.ctx.is_some()
    }

    /// The command bound to context row `row`, if any (separators / submenu headers have
    /// none). Rows past the visible entries are the Reminder flyout's hidden quick-offset
    /// slots; rows at/above [`crate::contextmenu::CTX_CUSTOM_REMIND_BASE`] are not indices
    /// at all — the flyout's Custom input encodes its Rust-parsed minutes through the
    /// frozen `pick(int)` channel as `BASE + minutes` (pane menus only).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn ctx_command(&self, row: usize) -> Option<Command> {
        self.ctx.as_ref().and_then(|c| {
            if row >= crate::contextmenu::CTX_CUSTOM_REMIND_BASE {
                let minutes = (row - crate::contextmenu::CTX_CUSTOM_REMIND_BASE) as u32;
                return (c.kind == crate::contextmenu::CtxKind::Pane
                    && (1..=1440).contains(&minutes))
                .then_some(Command::RemindPane(
                    c.target,
                    ReminderOffset::Custom(minutes),
                ));
            }
            c.commands.get(row).cloned().flatten()
        })
    }

    /// The open menu's target index (pane idx for a pane menu, tab idx for a tab menu).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn ctx_target(&self) -> Option<usize> {
        self.ctx.as_ref().map(|c| c.target)
    }

    // ---- pane chrome actions ----

    /// Recolor active-tab pane `idx` to swatch `swatch` of the current frame palette: adopt the
    /// color, pin it, and turn the per-pane frame + dot ON (mirrors `ColorSwatches`' pickColor).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn recolor_pane(&mut self, idx: usize, swatch: usize) {
        let palette = self.settings.frame_palette;
        let colors = theme::frame_palette(palette);
        let Some((r, g, b)) = colors.get(swatch).copied() else {
            return;
        };
        let color = Color::from_rgb_u8(r, g, b);
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.accent = color;
            p.pinned_accent = Some(color);
            p.show_frame = Some(true);
            p.show_dot = Some(true);
            self.dirty = true;
        }
    }

    /// Set pane `idx`'s per-pane frame override.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_pane_frame(&mut self, idx: usize, on: bool) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.show_frame = Some(on);
            self.dirty = true;
        }
    }

    /// Set pane `idx`'s per-pane dot override.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    /// Put a tier-5 module surface on keymap preset `preset` (`None` restores the module's
    /// own default) and persist the choice. Nothing is validated against the module's
    /// declaration here — [`crate::module_ui::grid`] falls back to the default for a preset
    /// name it cannot find, which is also what happens when a module drops one in an update.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_module_preset(&mut self, module: &str, surface: &str, preset: Option<&str>) {
        let Ok(module) = avada_core::rights::ModuleId::new(module) else {
            return;
        };
        crate::module_ui::grid::set_preset(&module, surface, preset);
        crate::module_ui::grid::save_prefs();
        self.dirty = true;
    }

    pub fn set_pane_dot(&mut self, idx: usize, on: bool) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.show_dot = Some(on);
            self.dirty = true;
        }
    }

    /// Toggle whether pane `idx`'s ambient-AI summary line is muted.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_mute_ai(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.ai_muted = !p.ai_muted;
            self.dirty = true;
        }
    }

    /// Toggle whether pane `idx`'s "talk" (speak new assistant replies aloud) is on.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_talk(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.talk = !p.talk;
            self.dirty = true;
        }
    }

    /// Maximize/restore (zoom-in-tab) pane `idx`. Focuses it first, then toggles its zoom.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn zoom_pane(&mut self, idx: usize) {
        let t = self.active_tab_mut();
        if idx >= t.panes.len() {
            return;
        }
        t.focused = idx;
        t.zoomed = if t.zoomed == Some(idx) {
            None
        } else {
            Some(idx)
        };
        self.dirty = true;
    }

    /// Open the in-pane search box on pane `idx` (or, if already open, re-focus it). Bumps the
    /// focus sequence so the widget (re)focuses the query input even when the box was already up.
    ///
    /// A view pane is skipped. The box is drawn BY the terminal widget, and a view pane covers
    /// that widget completely (D3) — opening it there would put the pane into a searching state
    /// with nothing on screen to show for it and no visible way back out. The pane menu already
    /// omits the row; this is the same answer for the keyboard chord.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn open_search(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if !p.kind.is_pty() {
                return;
            }
            p.pane.search_open();
            p.search_focus_seq = p.search_focus_seq.wrapping_add(1);
            self.dirty = true;
        }
    }

    /// Set pane `idx`'s search query (find-as-you-type).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_search_query(&mut self, idx: usize, query: &str) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.search_set_query(query);
            self.dirty = true;
        }
    }

    /// Step pane `idx`'s search to the next/previous match.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_search_step(&mut self, idx: usize, forward: bool) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.search_step(forward);
            self.dirty = true;
        }
    }

    /// Close pane `idx`'s search box.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_search_close(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.search_close();
            self.dirty = true;
        }
    }

    /// Copy pane `idx`'s current selection to the clipboard (no-op without a selection).
    ///
    /// Two panes, two selections. A pty pane's lives in the terminal widget's grid; a view
    /// pane has no grid at all (D3), so its selection is a row range on the projection and
    /// the text has to be re-joined from the rows. Both end at the same clipboard, and both
    /// raise the same "Copied …" toast, because from the human's side this is one gesture.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn copy_pane(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if p.kind.is_pty() {
                // No selection to copy? Fall back to the link under the cursor.
                //
                // Cmd+C in a pane where the running program holds the mouse grab used to be a
                // dead key in both directions: the chord never reaches the program (the app
                // claims it), and there is no selection for it to copy either, because a plain
                // drag went to the program rather than making one. So the gesture was swallowed
                // and produced nothing at all. When a link is lit under the pointer, that is
                // unambiguously the text the human is pointing at — copy it.
                if p.pane.copy_selection().is_none() {
                    if let Some(target) = p.link.as_ref().map(|l| l.abs_path.clone()) {
                        p.pane.copy_text(&target);
                    }
                }
            } else if let Some((lo, hi)) = crate::viewpane::selected_range(&p.uid) {
                let text = crate::viewpane::copy_range(&p.uid, lo, hi);
                if !text.is_empty() {
                    p.pane.copy_text(&text);
                }
            }
            self.dirty = true;
        }
    }

    /// Move a view pane's row selection: plain click replaces it, shift-click stretches it
    /// from the anchor. Lives here rather than in the view because the selection has to
    /// outlive a re-render and be readable by Copy, which is a controller concern.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn view_select(&mut self, idx: usize, row: usize, extend: bool) {
        if let Some(p) = self.active_tab().panes.get(idx) {
            if !p.kind.is_pty() {
                crate::viewpane::select_row(&p.uid, row, extend);
                self.dirty = true;
            }
        }
    }

    /// Copy a Ctrl+clicked link/path into the clipboard via pane `idx`'s own arboard instance,
    /// raising its "Copied …" toast. Replaces the blocking `clip.exe` shell-out (which froze
    /// the UI thread per Ctrl+click and gave no feedback).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn copy_link_text(&mut self, idx: usize, text: &str) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            p.pane.copy_text(text);
            self.dirty = true;
        }
    }

    /// The Windows-Terminal right-click nuance (#32): when pane `idx` has an active
    /// drag-selection, right-click COPIES it (and clears the highlight) instead of pasting.
    /// Returns true when that happened — the caller then skips the paste. The selection is
    /// cleared even if the clipboard write failed (the gesture consumed it either way), so a
    /// follow-up right-click always pastes.
    ///
    /// Only in the modal (copy-on-select OFF) mode — WT's coupling: with copy-on-select ON the
    /// release already copied, so a modal copy would be redundant and right-click ALWAYS pastes.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn copy_selection_on_right_click(&mut self, idx: usize) -> bool {
        if self.settings.copy_on_select {
            return false;
        }
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if p.pane.selection_is_drag() {
                p.pane.copy_selection();
                p.pane.selection_clear();
                self.dirty = true;
                return true;
            }
        }
        false
    }

    /// Paste the clipboard into pane `idx`'s session (the widget owns the clipboard; the
    /// controller owns the transport, so we write the returned text via the manager).
    /// PASTE-OVER: a type-over-eligible selection (single row on the prompt line, main screen)
    /// is erased first — same clamp-safe sequence as typing — so the paste REPLACES it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn paste_pane(&mut self, idx: usize, mgr: &SessionManager) {
        let payload = self.active_tab_mut().panes.get_mut(idx).and_then(|p| {
            // Nothing to paste INTO on a view pane — there is no pty on the other end (D3).
            if !p.kind.is_pty() {
                return None;
            }
            let text = p.pane.paste_from_clipboard()?;
            // Erase a prompt-line selection so the paste lands in its place; elsewhere just
            // drop the highlight (it must clear on paste, and a lingering "live" selection
            // could otherwise be re-copied, pasting stale text).
            let erase = p.pane.type_over_selection();
            p.pane.selection_clear();
            // Snap the viewport to the live edge so the caret lands at the end of the pasted
            // text (visible), regardless of where the pane was scrolled when pasting.
            p.pane.scroll_to_bottom();
            Some((p.uid.clone(), erase, text))
        });
        if let Some((uid, erase, text)) = payload {
            if let Some(erase) = erase {
                pane_write(mgr, &uid, &String::from_utf8_lossy(&erase), "paste-erase");
            }
            pane_write(mgr, &uid, &text, "paste");
            self.dirty = true;
            return;
        }
        // No clipboard TEXT: if the clipboard holds an IMAGE, forward a literal Ctrl+V (0x16)
        // so an in-pane TUI that reads the OS clipboard itself (e.g. Claude Code's image paste)
        // can pull the image — the pty can't carry image bytes and the widget clipboard wrapper
        // is text-only. Mirrors the explicit Alt+V gesture (`paste_image_focused`). Today a
        // Ctrl+V on an image-only clipboard was a silent no-op, so this only adds behavior.
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if p.kind.is_pty() && p.pane.clipboard_has_image() {
                let uid = p.uid.clone();
                p.pane.set_toast("Pasting image…");
                pane_write(mgr, &uid, "\u{16}", "paste-image-ctrl-v");
                self.dirty = true;
            }
        }
    }

    /// Insert files dropped from the OS file manager into pane `idx` as shell-quoted words.
    ///
    /// Deliberately stops short of Enter — see [`crate::filedrop`]. Delivered through the
    /// pane's own paste preparation so a TUI that reads bracketed paste treats a drop the
    /// way it treats a paste, and a path with a control character in its name is refused
    /// rather than typed. Returns whether anything reached the pty.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn drop_files(&mut self, idx: usize, paths: &[String], mgr: &SessionManager) -> bool {
        if paths.is_empty() {
            return false;
        }
        let (text, refused) =
            crate::filedrop::format_paths(paths, crate::filedrop::native_quoting());
        let payload = match self.active_tab_mut().panes.get_mut(idx) {
            None => return false,
            Some(p) if !p.kind.is_pty() => {
                // A view pane (a file, a diff, the git panel) has nothing on the other end.
                p.pane.set_toast("Nothing here to drop files into");
                None
            }
            Some(p) if text.is_empty() => {
                // Every path in the batch was refused (or there were none) — say so, rather
                // than leaving a drag that visibly landed with no visible effect.
                p.pane.set_toast(format!(
                    "Skipped {refused} dropped path{} — a control character in the name",
                    if refused == 1 { "" } else { "s" }
                ));
                None
            }
            Some(p) => {
                let kept = paths.len() - refused;
                p.pane.set_toast(if refused == 0 {
                    format!("Dropped {kept} path{}", if kept == 1 { "" } else { "s" })
                } else {
                    format!("Dropped {kept}, skipped {refused}")
                });
                // Same housekeeping a paste does: the highlight must not survive (it could
                // be re-copied as stale text) and the caret has to be on screen to be seen.
                p.pane.selection_clear();
                p.pane.scroll_to_bottom();
                Some((p.uid.clone(), p.pane.prepare_insert(&text)))
            }
        };
        self.dirty = true;
        match payload {
            Some((uid, wire)) => {
                pane_write(mgr, &uid, &wire, "drop-files");
                true
            }
            None => false,
        }
    }

    /// Forward a literal Ctrl+V (0x16) to pane `idx`'s session so an in-pane TUI that reads the
    /// OS clipboard itself (e.g. Claude Code) can paste a clipboard IMAGE — which avada'
    /// own text paste can't deliver through the pty. Bound to Alt+V, the shortcut Claude Code
    /// documents for "your terminal intercepts Ctrl+V". Unconditional (the user knows there's an
    /// image): the app simply lets the focused program resolve the clipboard.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn paste_image_focused(&mut self, idx: usize, mgr: &SessionManager) {
        if let Some(p) = self
            .active_tab_mut()
            .panes
            .get_mut(idx)
            .filter(|p| p.kind.is_pty())
        {
            let uid = p.uid.clone();
            p.pane.set_toast("Pasting image…");
            pane_write(mgr, &uid, "\u{16}", "paste-image-ctrl-v");
            self.dirty = true;
        }
    }

    /// Select all of pane `idx`'s viewport — its grid on a pty pane, every projected row
    /// on a view pane (see [`State::copy_pane`] for why those are two different selections).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn select_all_pane(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if p.kind.is_pty() {
                p.pane.select_all();
            } else {
                crate::viewpane::select_all(&p.uid);
            }
            self.dirty = true;
        }
    }

    // ---- text selection (drag-to-select; copy-on-release) ----
    // The widget reports pointer-down/drag/up in the pane's logical-px space; the controller
    // hit-tests against the pane's on-screen surface size (`surf`, recorded from the widget's
    // geometry callback) and the pane's own font cell metrics. Marking `dirty` re-runs resync,
    // which re-projects the selection highlight rects into the pane model each tick.

    /// Begin a selection in pane `idx` at the pressed point (logical px within the surface).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_selection_begin(&mut self, idx: usize, x: f32, y: f32) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            let (w, h) = p.surf;
            p.pane.selection_begin(x, y, w, h);
            self.dirty = true;
        }
    }

    /// Extend the in-progress selection in pane `idx` to the dragged point.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn pane_selection_update(&mut self, idx: usize, x: f32, y: f32) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            let (w, h) = p.surf;
            p.pane.selection_update(x, y, w, h);
            self.dirty = true;
        }
    }

    /// Finish a selection in pane `idx`: a real drag copies to the clipboard (raising the
    /// "Copied …" toast) and keeps its highlight; a stationary click on the shell's input line
    /// moves the caret there, and any other stationary click just clears the zero-size selection
    /// so it doesn't linger or block the next click.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn pane_selection_end(&mut self, idx: usize, mgr: &SessionManager) {
        let copy_on_select = self.settings.copy_on_select;
        let mut caret = None;
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            // Button released → stop edge-autoscroll (the selection itself is kept/copied below).
            p.pane.end_selection_drag();
            if p.pane.selection_is_drag() {
                // Copy-on-select is a PREF, ON by default: a finished drag lands on the
                // clipboard. Turning it off makes a drag only highlight, so the clipboard keeps
                // whatever you copied elsewhere and "select the target, paste over it" works;
                // copy then goes through right-click (modal), the copy chord, or the menu.
                if copy_on_select {
                    p.pane.copy_selection();
                }
            } else {
                // Click-to-place-caret. Read BEFORE the clear — the anchor cell is the click.
                // `click_move_cursor` declines for everything that isn't unambiguously an edit
                // position (see its safety model), so this is a no-op on output, scrollback, a
                // mouse-grabbing app or the alternate screen.
                if p.kind.is_pty() {
                    caret = p.pane.click_move_cursor().map(|b| (p.uid.clone(), b));
                }
                p.pane.selection_clear();
            }
            self.dirty = true;
        }
        if let Some((uid, bytes)) = caret {
            pane_write(
                mgr,
                &uid,
                &String::from_utf8_lossy(&bytes),
                "click-move-cursor",
            );
        }
    }

    /// Clear pane `idx`'s screen + scrollback. A view pane's content is a projection of a file,
    /// not a grid the human typed into, so there is nothing here for a clear to mean.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn clear_pane(&mut self, idx: usize) {
        if let Some(p) = self.active_tab_mut().panes.get_mut(idx) {
            if !p.kind.is_pty() {
                return;
            }
            p.pane.clear();
            self.dirty = true;
        }
    }

    /// Restart pane `idx`'s shell: spawn a fresh session, swap it into the pane slot (resetting
    /// the grid), and kill the old session. The cwd resets to the default and any per-pane env
    /// overrides are dropped, otherwise chrome (title / color / frame) is preserved.
    ///
    /// Returns the `(old_uid, new_uid)` swap so the caller can re-point anything keyed by the
    /// old session uid (the control-plane pane-id alias above all) at the replacement.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn restart_pane(&mut self, idx: usize, mgr: &SessionManager) -> Option<(String, String)> {
        self.restart_pane_at(self.active, idx, mgr, None, None)
    }

    /// The Hyperpane tab's own agent pane: the first pty pane of the system tab.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn hyperpane_pane_uid(&self) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.system)?
            .panes
            .iter()
            .find(|p| p.kind.is_pty())
            .map(|p| p.uid.clone())
    }

    /// The panes the restart loop respawns: every tool pane outside the system tab, as
    /// `(tab, pane, uid, tool id)`. The system tab's own agent is the one doing the
    /// monitoring and is left alone.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn monitored_panes(&self) -> Vec<(usize, usize, String, String)> {
        let mut out = Vec::new();
        for (ti, t) in self.tabs.iter().enumerate() {
            if t.system {
                continue;
            }
            for (pi, p) in t.panes.iter().enumerate() {
                if let PaneKind::Tool(tool) = &p.kind {
                    out.push((ti, pi, p.uid.clone(), tool.clone()));
                }
            }
        }
        out
    }

    /// Restart-loop respawn of the monitored tool pane `(ti, pi)`: a fresh shell in the
    /// same directory with the same env overrides, and the tool typed back into its
    /// conversation at the shell's first output. The conversation comes from `marker`
    /// (the pane's Claude session marker, when the hook wrote one) or else the pane's own
    /// tool mark; with neither, the tool is started fresh. Returns the `(old, new)` uid
    /// swap for the caller to re-key, like [`Self::pane_exited`] does.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn restart_monitored_pane(
        &mut self,
        ti: usize,
        pi: usize,
        mgr: &SessionManager,
        marker: Option<&avada_core::claude_panes::PaneClaudeSession>,
    ) -> Option<(String, String)> {
        let (tool, cwd, env, mark) = {
            let p = self.tabs.get(ti)?.panes.get(pi)?;
            let PaneKind::Tool(tool) = &p.kind else {
                return None;
            };
            (
                tool.clone(),
                p.cwd.clone(),
                p.env.clone(),
                p.tool_session.clone(),
            )
        };
        let bin = avada_core::tools::by_id(&tool)
            .map(|t| t.bin)
            .unwrap_or(tool.as_str());
        let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let (session, resume_cwd, prefix) = match (marker, &mark) {
            (Some(m), _) => (
                Some(m.session_id.clone()),
                non_empty(&m.cwd).or_else(|| cwd.clone()),
                non_empty(&m.config_dir)
                    .filter(|d| avada_core::claude_panes::valid_config_dir(d))
                    .map(|d| format!("CLAUDE_CONFIG_DIR='{d}' "))
                    .unwrap_or_default(),
            ),
            (None, Some(mark)) => (
                Some(mark.id.clone()),
                non_empty(&mark.cwd).or_else(|| cwd.clone()),
                String::new(),
            ),
            (None, None) => (None, cwd.clone(), String::new()),
        };
        let line = crate::loops::restart_line(
            &tool,
            bin,
            resume_cwd.as_deref(),
            &prefix,
            session.as_deref(),
        );
        let swap = self.restart_pane_at(ti, pi, mgr, cwd, env)?;
        if let Some(p) = self.tabs.get_mut(ti).and_then(|t| t.panes.get_mut(pi)) {
            p.startup = Some(line);
            // The pane still IS this tool's pane (the respawn keeps its kind); remember the
            // conversation it is being put back into so a later relaunch resumes it too.
            if let (Some(id), Some(cwd)) = (&session, &resume_cwd) {
                if let Some(m) = ToolSessionMark::new(id, cwd) {
                    p.tool_session = Some(ToolSessionMark {
                        tool: mark.and_then(|m| m.tool),
                        ..m
                    });
                }
            }
        }
        Some(swap)
    }

    /// "Refresh Env" (#28): restart pane `idx`'s shell in place but KEEP its live cwd and its
    /// env overrides. The spawn path resolves a fresh registry-backed environment on every
    /// spawn (core `session::env::fresh_env`), so a restart IS the refresh — this variant just
    /// avoids also losing where the user was and what the pane had layered on top.
    ///
    /// Returns the `(old_uid, new_uid)` swap like [`Self::restart_pane`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn refresh_env_pane(
        &mut self,
        idx: usize,
        mgr: &SessionManager,
    ) -> Option<(String, String)> {
        let (cwd, env) = self
            .active_tab()
            .panes
            .get(idx)
            .map(|p| (p.cwd.clone(), p.env.clone()))?;
        self.restart_pane_at(self.active, idx, mgr, cwd, env)
    }

    /// Shared respawn core of [`Self::restart_pane`] / [`Self::refresh_env_pane`] / the
    /// exit fallback in [`Self::pane_exited`]: spawn a replacement session (optionally
    /// pinning a cwd + env overrides), swap it into the pane slot, and kill the old session.
    ///
    /// Addressed by `(ti, idx)` rather than the active tab: a pane whose program exits is
    /// very often not the one being looked at, and respawning into the focused tab instead
    /// would replace a pane the user is working in.
    ///
    /// Returns `(old uid, new uid)` — a restart mints a fresh session, so anything keyed by
    /// the pane's uid outside this `State` (the control plane's stable pane-id alias) has to
    /// be moved across. `None` when there was nothing to restart.
    #[tracing::instrument(level = "debug", skip_all)]
    fn restart_pane_at(
        &mut self,
        ti: usize,
        idx: usize,
        mgr: &SessionManager,
        cwd: Option<String>,
        env: Option<avada_core::session::spawn::EnvMap>,
    ) -> Option<(String, String)> {
        let (cols, rows) = match self.tabs.get(ti).and_then(|t| t.panes.get(idx)) {
            // A view pane has no session to restart — restarting it would spawn a shell into a
            // pane that renders no pty, and strand the `view-` uid it was found by (D3).
            Some(p) if !p.kind.is_pty() => return None,
            Some(p) => p.applied,
            None => return None,
        };
        let (cols, rows) = (cols.max(2) as u16, rows.max(1) as u16);
        // A restart is a brand-new session — mint via the backend (daemon → cross-run-unique
        // uid; in-process → `pane-N`). See `SessionManager::fresh_uid`.
        let uid = mgr.fresh_uid();
        let shell = prefs::effective_shell(&self.settings.default_shell);
        let shell_path = shell
            .clone()
            .unwrap_or_else(avada_core::session::spawn::default_shell);
        let integration = avada_core::shell_integration::integration_for(
            &shell_path,
            &avada_core::shell_integration::shell_integration_dir(),
        )
        .map(|si| avada_core::session_manager::Integration {
            args: si.args,
            env: si.env.into_iter().collect(),
        });
        if let Err(e) = mgr.create(SpawnOptions {
            uid: uid.clone(),
            cols: Some(cols),
            rows: Some(rows),
            pane_id: Some(uid.clone()),
            cwd,
            // Cloned so the resolved shell is also recorded on the pane (below) as its new
            // spawn spec.
            shell: shell.clone(),
            // `BROWSER` points at our shim so a link a tool in this pane wants to
            // show goes back through `App::route_event` → `Command::OpenLink` (and
            // thus Preferences → Browser) instead of straight to the OS. Wrapped
            // here and not where the env is recorded on the pane: the shim path is
            // regenerated every run, so it must not be baked into a snapshot.
            env: avada_core::open::with_browser_shim(env.clone()),
            integration,
            ..Default::default()
        }) {
            tracing::error!("failed to restart {uid}: {e}");
            return None;
        }
        let mut newgrid = TerminalPane::with_scrollback(
            cols as usize,
            rows as usize,
            self.settings.scrollback as usize,
            Box::new(SoftwareRenderer::new()),
        );
        newgrid.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        newgrid.set_project_roots(self.project_roots());
        let mut stale_uid: Option<String> = None;
        let new_uid = uid.clone();
        if let Some(p) = self.tabs.get_mut(ti).and_then(|t| t.panes.get_mut(idx)) {
            let old = std::mem::replace(&mut p.uid, uid);
            mgr.kill(&old);
            // The restart mints a NEW uid, so the old key can never be reached again.
            stale_uid = Some(old);
            p.pane = newgrid;
            p.applied = (cols as usize, rows as usize);
            // A restart spawns a brand-new pty AT this size, so the backend already agrees —
            // recording it keeps the debounced push from firing a pointless SIGWINCH.
            p.pty = (cols as usize, rows as usize);
            p.pty_since = std::time::Instant::now();
            p.started = false;
            p.startup = None;
            p.shell_title = String::new();
            // The restart re-resolves the shell → refresh the cached header badge.
            p.shell_label = shell_label(&shell_path);
            p.surface = Image::default();
            // Plain restart drops the overrides (env: None); refresh re-applies them.
            p.env = env;
            // A restart re-spawns a plain interactive shell at the resolved default — drop any
            // original command/args and record the new shell so a later relaunch snapshot
            // reflects what's actually running.
            p.spawn_command = None;
            p.spawn_args = None;
            p.spawn_shell = shell;
        }
        let swap = stale_uid.map(|old| {
            self.forget_pane_runtime(&old);
            (old, new_uid)
        });
        self.dirty = true;
        swap
    }

    // ---- move panes across tabs ----

    /// Remove active-tab pane `idx` **without** killing its session, as a [`DetachedPane`] (the
    /// session stays alive centrally for replay-primed re-host). An emptied tab is dropped by
    /// [`Self::take_pane_in`], which also fixes the active index.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn detach_pane_idx(&mut self, idx: usize) -> Option<DetachedPane> {
        self.detach_pane_in(self.active, idx)
    }

    /// [`Self::detach_pane_idx`] for an arbitrary tab — the left panel's workspace tree can
    /// drag a pane out of a BACKGROUND tab, which the active-tab-only path can't express.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn detach_pane_in(&mut self, ti: usize, idx: usize) -> Option<DetachedPane> {
        let (ps, _alive) = self.take_pane_in(ti, idx)?;
        Some(DetachedPane {
            uid: ps.uid,
            title: ps.title,
            subtitle: ps.subtitle,
            pinned_accent: ps.pinned_accent,
            show_frame: ps.show_frame,
            show_dot: ps.show_dot,
            font_px: ps.font_px,
            spawn_command: ps.spawn_command,
            spawn_args: ps.spawn_args,
            spawn_shell: ps.spawn_shell,
            kind: ps.kind,
            tool_session: ps.tool_session,
            cwd: ps.cwd,
        })
    }

    /// Adopt a detached session at the end of tab `ti` **without** changing the active tab
    /// (re-host into a background tab — replay-primed, no PTY restart). Used by move-to-tab +
    /// reopen-closed.
    #[tracing::instrument(level = "debug", skip_all)]
    fn adopt_into_tab(&mut self, mgr: &SessionManager, det: DetachedPane, ti: usize) {
        if ti >= self.tabs.len() {
            return;
        }
        let palette = self.settings.frame_palette;
        let (cols, rows) = (80u16, 24u16);
        let mut pane = TerminalPane::with_scrollback(
            cols as usize,
            rows as usize,
            self.settings.scrollback as usize,
            Box::new(SoftwareRenderer::new()),
        );
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        pane.set_project_roots(self.project_roots());
        if let Some(replay) = mgr.replay(&det.uid) {
            pane.feed(&replay);
        }
        let glow = Glow::new(crate::glow::seed_from(&det.uid));
        let at = self.tabs[ti].panes.len();
        let accent = det
            .pinned_accent
            .unwrap_or_else(|| theme::accent_for(at, palette));
        let font = theme::load_font_at(&self.settings.font_path(), det.font_px, self.last_scale);
        let ps = PaneState {
            uid: det.uid,
            title: det.title,
            subtitle: det.subtitle,
            show_frame: det.show_frame,
            show_dot: det.show_dot,
            accent,
            pane,
            applied: (cols as usize, rows as usize),
            pty: (cols as usize, rows as usize),
            pty_since: std::time::Instant::now(),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            started: true,
            startup: None,
            pinned_accent: det.pinned_accent,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            talk: false,
            recording: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            scrollbar_on: false,
            search_focus_seq: 0,
            refocus_seq: 0,
            font_px: det.font_px,
            font,
            font_dirty: false,
            // Carried, not reset: for a view pane this IS the file/directory it renders, and
            // for a terminal it is what the file tree follows.
            cwd: det.cwd,
            env: None,
            // A re-hosted session: its original spawn shell isn't tracked across the detach,
            // so the badge stays hidden ("") rather than guessing.
            shell_label: String::new(),
            // The spawn spec IS carried across the detach, so a relaunch snapshot of a
            // re-hosted pane still records its program.
            spawn_command: det.spawn_command,
            spawn_args: det.spawn_args,
            spawn_shell: det.spawn_shell,
            kind: det.kind,
            tool_session: det.tool_session,
        };
        let auto = self.tabs[ti].layout == Layout::Auto;
        let t = &mut self.tabs[ti];
        t.sizes = if auto {
            equal_sizes(at + 1)
        } else {
            insert_size(&t.sizes, at)
        };
        t.panes.push(ps);
        t.zoomed = None;
        t.relabel(palette);
        self.dirty = true;
    }

    /// Move active-tab pane `idx` into a brand-new tab (the pane menu's "Move to New Tab",
    /// gated to ≥2 panes so the source tab survives), switching to it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn move_pane_to_new_tab(&mut self, idx: usize, mgr: &SessionManager) {
        if self.active_tab().panes.len() < 2 {
            return;
        }
        let Some(dp) = self.detach_pane_idx(idx) else {
            return;
        };
        let tab = self.fresh_tab();
        tracing::info!(title = %tab.title, "tab created (pane moved out)");
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        self.adopt_pane(mgr, dp);
    }

    /// Move active-tab pane `idx` into existing tab `target` (the "Move to Tab" submenu), without
    /// switching away from the current tab. Handles the source tab being dropped when its last
    /// pane leaves (which shifts `target` when the source sat before it).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn move_pane_to_tab(&mut self, idx: usize, target: usize, mgr: &SessionManager) {
        if target >= self.tabs.len() || target == self.active {
            return;
        }
        let src = self.active;
        let before = self.tabs.len();
        let Some(dp) = self.detach_pane_idx(idx) else {
            return;
        };
        let mut target = target;
        if self.tabs.len() < before && src < target {
            target -= 1;
        }
        if target >= self.tabs.len() {
            return;
        }
        self.adopt_into_tab(mgr, dp, target);
        // Moving the system tab's last pane away does not delete the system tab (it is never
        // dropped — `take_pane_inner`); it is refilled with a fresh shell instead.
        self.reseed_system_tab(mgr);
    }

    /// Move pane `idx` of tab `from` into tab `target` — the general form of
    /// [`Self::move_pane_to_tab`], which can only move out of the ACTIVE tab. Used by the
    /// left panel's workspace tree, where any pane of any tab can be dragged onto any other
    /// tab. Like the menu path it neither switches tabs nor restarts the PTY (the session is
    /// detached and re-hosted replay-primed), and it handles the source tab being dropped
    /// when its last pane leaves (which shifts `target` when the source sat before it).
    ///
    /// Both indices are re-validated here because they arrive from a UI model snapshot: the
    /// tree the user dragged in is whatever `resync` last projected, and a session could
    /// have exited in between.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn move_pane_between_tabs(
        &mut self,
        from: usize,
        idx: usize,
        target: usize,
        mgr: &SessionManager,
    ) {
        if from >= self.tabs.len() || target >= self.tabs.len() || from == target {
            return;
        }
        if idx >= self.tabs[from].panes.len() {
            return;
        }
        // Moving out of the active tab is exactly the menu path — reuse it so the two can
        // never drift apart.
        if from == self.active {
            self.move_pane_to_tab(idx, target, mgr);
            return;
        }
        let before = self.tabs.len();
        let Some(dp) = self.detach_pane_in(from, idx) else {
            return;
        };
        let mut target = target;
        if self.tabs.len() < before && from < target {
            target -= 1;
        }
        if target >= self.tabs.len() {
            return;
        }
        self.adopt_into_tab(mgr, dp, target);
        // Same as the menu path: an emptied system tab is refilled, never dropped.
        self.reseed_system_tab(mgr);
    }

    /// [`Self::move_pane_between_tabs`] with a landing position: the pane ends up at
    /// insertion index `at` inside `target` rather than appended. Composed of the two moves
    /// that already exist rather than a third detach path, so the cross-tab rehost keeps
    /// exactly one implementation. `target` is re-resolved across the move because the source
    /// tab is dropped when its last pane leaves, which shifts every tab after it.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn move_pane_between_tabs_at(
        &mut self,
        from: usize,
        idx: usize,
        target: usize,
        at: usize,
        mgr: &SessionManager,
    ) {
        // Re-validated here as well as inside the move: a rejected move leaves the target
        // untouched, and reordering its last row afterwards would be a phantom edit.
        if from >= self.tabs.len() || target >= self.tabs.len() || from == target {
            return;
        }
        if idx >= self.tabs[from].panes.len() {
            return;
        }
        let before = self.tabs.len();
        self.move_pane_between_tabs(from, idx, target, mgr);
        let target = if self.tabs.len() < before && from < target {
            target - 1
        } else {
            target
        };
        let Some(t) = self.tabs.get(target) else {
            return;
        };
        // The pane was appended, so it is the last row; `at` is an insertion index in the
        // post-append list, which for a move DOWN the list is exactly the destination.
        let last = t.panes.len().saturating_sub(1);
        self.reorder_pane_in(target, last, at.min(last));
    }

    // ---- tab actions ----

    /// Duplicate tab `idx`: a fresh tab adopting its layout + title with the same number of
    /// (fresh-shell) panes, switched to. (Sessions aren't cloned — the renderer spawns new
    /// shells too.)
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn duplicate_tab(&mut self, idx: usize, mgr: &SessionManager) {
        let Some(src) = self.tabs.get(idx) else {
            return;
        };
        let layout = src.layout;
        let title = src.title.clone();
        let sizes = src.sizes.clone();
        let main_fraction = src.main_fraction;
        let focused = src.focused;
        let zoomed = src.zoomed;
        // Snapshot each source pane's chrome so the duplicate carries it (label, color/pin,
        // frame/dot, subtitle, per-pane zoom) — not just the pane count + layout. The accent is
        // pinned only when the source's was; an unpinned pane re-derives its by-slot color at
        // the same index, so order-preserving duplication keeps the same colors.
        let chrome: Vec<(NewPaneOpts, Option<SharedString>, f32)> = src
            .panes
            .iter()
            .map(|p| {
                (
                    NewPaneOpts {
                        label: Some(p.title.to_string()),
                        accent: p.pinned_accent,
                        show_frame: p.show_frame,
                        show_dot: p.show_dot,
                        ..Default::default()
                    },
                    p.subtitle.clone(),
                    p.font_px,
                )
            })
            .collect();
        let mut tab = self.fresh_tab();
        tab.layout = layout;
        tab.title = title;
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        if chrome.is_empty() {
            // A tab should always have ≥1 pane, but guard the empty case with a default pane.
            self.add_pane(mgr);
        } else {
            for (opts, subtitle, font_px) in chrome {
                self.add_pane_opts(mgr, opts);
                if let Some(p) = self.active_tab_mut().panes.last_mut() {
                    p.subtitle = subtitle;
                    if (p.font_px - font_px).abs() > f32::EPSILON {
                        p.font_px = font_px;
                        p.font_dirty = true; // pump reloads the font at the carried zoom
                    }
                }
            }
        }
        // Carry the split geometry + focus/zoom from the source (clamped to the new pane count).
        let t = self.active_tab_mut();
        if sizes.len() == t.panes.len() {
            t.sizes = sizes;
        }
        t.main_fraction = main_fraction;
        if !t.panes.is_empty() {
            t.focused = focused.min(t.panes.len() - 1);
        }
        t.zoomed = zoomed.filter(|&z| z < t.panes.len());
        self.dirty = true;
    }

    /// Park a closed pane or tab on the recently-closed history, capping it. Eviction is the
    /// point where a close finally becomes irreversible, so it is also where the sessions the
    /// entry was keeping alive are killed — the cap is a budget of live off-layout PTYs.
    #[tracing::instrument(level = "debug", skip_all)]
    fn push_closed(&mut self, what: ClosedWhat, mgr: &SessionManager) {
        self.closed.push(ClosedItem {
            what,
            id: fresh_closed_id(),
            at_ms: crate::glow::now_epoch_ms(),
        });
        while self.closed.len() > CLOSED_STACK_CAP {
            let evicted = self.closed.remove(0);
            for p in evicted.panes() {
                kill_session_of(mgr, &p.uid, &p.kind);
            }
        }
    }

    /// Detach the whole of tab `idx` (its panes as live [`DetachedPane`]s, plus title/layout/
    /// sizes) for re-hosting or parking. Requires ≥2 tabs; fixes the active index. Returns the
    /// detached tab + `source_alive` (always `true` here — other tabs remain).
    ///
    /// Refuses the system tab: every consumer of a `DetachedTab` (the closed stack, "Move to
    /// New Window", tab drag-out) would either lose the Hyperpane or resurrect it as an
    /// ordinary tab. The one legitimate move — re-homing it when its window closes — goes
    /// through [`Self::take_system_tab`] / [`Self::adopt_system_tab`], which keep the flag.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn detach_tab(&mut self, idx: usize) -> Option<(DetachedTab, bool)> {
        if idx >= self.tabs.len() || self.tabs.len() < 2 {
            return None;
        }
        if self.tabs[idx].system {
            tracing::info!(tab = idx, "refusing to detach the Hyperpane tab");
            return None;
        }
        tracing::info!(tab = idx, title = %self.tabs[idx].title, "tab detached");
        Some((self.detach_tab_unchecked(idx), true))
    }

    /// Lift the system tab out of this window for re-homing into another one (the owner
    /// window is closing while others survive). `None` when this window does not hold it.
    /// The active tab is fixed up; the window is NOT left empty by this alone unless the
    /// system tab was its only tab — the caller is closing it anyway.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn take_system_tab(&mut self) -> Option<DetachedTab> {
        let idx = self.tabs.iter().position(|t| t.system)?;
        Some(self.detach_tab_unchecked(idx))
    }

    /// Re-host a system tab lifted by [`Self::take_system_tab`]: it becomes THIS window's
    /// Hyperpane at index 0, flag intact, sessions alive. The user's current tab stays
    /// selected — a window closing elsewhere is no reason to yank them to the Hyperpane.
    /// If this window somehow already has a system tab the incoming one is adopted as an
    /// ordinary tab so two never coexist (its sessions are worth more than the flag).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn adopt_system_tab(&mut self, mgr: &SessionManager, det: DetachedTab) {
        let had_system = self.tabs.iter().any(|t| t.system);
        let was_active = self.active;
        let title = det.title.clone();
        self.reopen_tab(mgr, det);
        let ti = self.tabs.len() - 1;
        if had_system {
            tracing::warn!(
                title = %title,
                "window already holds a Hyperpane tab; adopting the incoming one as ordinary"
            );
            self.active = was_active;
            return;
        }
        self.tabs[ti].system = true;
        // Restore the selection before pinning: the pin rotates indices and fixes `active`.
        self.active = was_active;
        self.pin_system_tab_first();
        tracing::info!(title = %title, "Hyperpane tab re-homed");
    }

    /// The take half of [`Self::detach_tab`] without its refusals; `idx` must be in range.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn detach_tab_unchecked(&mut self, idx: usize) -> DetachedTab {
        let tab = self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if idx < self.active {
            self.active -= 1;
        }
        self.editing_tab = -1;
        self.dirty = true;
        let focused = tab.focused;
        let zoomed = tab.zoomed;
        let panes = tab
            .panes
            .into_iter()
            .map(|p| DetachedPane {
                uid: p.uid,
                title: p.title,
                subtitle: p.subtitle,
                pinned_accent: p.pinned_accent,
                show_frame: p.show_frame,
                show_dot: p.show_dot,
                font_px: p.font_px,
                spawn_command: p.spawn_command,
                spawn_args: p.spawn_args,
                spawn_shell: p.spawn_shell,
                kind: p.kind,
                tool_session: p.tool_session,
                cwd: p.cwd,
            })
            .collect();
        DetachedTab {
            title: tab.title,
            layout: tab.layout,
            sizes: tab.sizes,
            main_fraction: tab.main_fraction,
            focused,
            zoomed,
            panes,
        }
    }

    /// Close tab `idx` reopenably: with ≥2 tabs it's parked (sessions alive) on the closed stack;
    /// the last tab is killed for real (returns `false` → the window quits).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn close_tab_menu(&mut self, idx: usize, mgr: &SessionManager) -> bool {
        if self.tabs.get(idx).is_some_and(|t| t.system) {
            return true;
        }
        if self.tabs.len() >= 2 {
            if let Some((det, _)) = self.detach_tab(idx) {
                self.push_closed(ClosedWhat::Tab(Box::new(det)), mgr);
            }
            true
        } else {
            self.close_tab(idx, mgr)
        }
    }

    /// Close every tab except `idx` (all reopenable). Removes from the highest index down so the
    /// surviving indices stay valid.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn close_other_tabs(&mut self, idx: usize, mgr: &SessionManager) {
        if idx >= self.tabs.len() {
            return;
        }
        let mut others: Vec<usize> = (0..self.tabs.len()).filter(|&i| i != idx).collect();
        others.sort_unstable_by(|a, b| b.cmp(a));
        for i in others {
            self.close_tab_menu(i, mgr);
        }
    }

    /// Close every tab to the right of `idx` (all reopenable), highest index first.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn close_tabs_to_right(&mut self, idx: usize, mgr: &SessionManager) {
        let mut i = self.tabs.len();
        while i > idx + 1 {
            i -= 1;
            self.close_tab_menu(i, mgr);
        }
    }

    /// Re-host a parked tab as a fresh tab, switched to (replay-primed; its sessions were
    /// kept alive). Split out of the reopen command so a history-row click can reach it too.
    #[tracing::instrument(level = "debug", skip_all)]
    fn reopen_tab(&mut self, mgr: &SessionManager, det: DetachedTab) {
        let mut tab = Tab::empty(det.title.clone());
        tab.layout = det.layout;
        tracing::info!(title = %tab.title, "tab created (reopened)");
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.editing_tab = -1;
        let ti = self.active;
        for dp in det.panes {
            self.adopt_into_tab(mgr, dp, ti);
        }
        if det.sizes.len() == self.tabs[ti].panes.len() {
            self.tabs[ti].sizes = det.sizes;
        }
        self.tabs[ti].main_fraction = det.main_fraction;
        // Restore the detached focus + zoom (clamped to the adopted pane count) rather than
        // snapping to pane 0 / dropping the maximized pane. `adopt_into_tab` clears `zoomed`
        // on each pane, so this must run after the adopt loop.
        let n = self.tabs[ti].panes.len();
        if n > 0 {
            self.tabs[ti].focused = det.focused.min(n - 1);
        }
        self.tabs[ti].zoomed = det.zoomed.filter(|&z| z < n);
        self.dirty = true;
    }

    /// Fill a freshly-spawned window's initial (empty) tab with a detached tab's panes + its
    /// title/layout/sizes (the seed for the tab menu's "Move to New Window"). Replay-primed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn adopt_tab(&mut self, mgr: &SessionManager, det: DetachedTab) {
        let ti = self.active;
        self.tabs[ti].title = det.title;
        self.tabs[ti].layout = det.layout;
        // A moved tab is an ordinary tab in its new window, whatever it was before: the
        // Hyperpane is created per process (`ensure_hyperpane_tab`) and re-homed only via
        // `adopt_system_tab`. Explicit so a future seed path cannot mint a second one.
        self.tabs[ti].system = false;
        for dp in det.panes {
            self.adopt_into_tab(mgr, dp, ti);
        }
        if det.sizes.len() == self.tabs[ti].panes.len() {
            self.tabs[ti].sizes = det.sizes;
        }
        self.tabs[ti].main_fraction = det.main_fraction;
        // Restore the detached focus + zoom (clamped to the adopted pane count) rather than
        // snapping to pane 0 / dropping the maximized pane. `adopt_into_tab` clears `zoomed`
        // on each pane, so this must run after the adopt loop.
        let n = self.tabs[ti].panes.len();
        if n > 0 {
            self.tabs[ti].focused = det.focused.min(n - 1);
        }
        self.tabs[ti].zoomed = det.zoomed.filter(|&z| z < n);
        self.dirty = true;
    }

    /// Set tab `idx`'s layout (the tab menu's Layout submenu).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_tab_layout(&mut self, idx: usize, layout: Layout) {
        if let Some(t) = self.tabs.get_mut(idx) {
            if t.layout != layout {
                t.layout = layout;
                self.dirty = true;
            }
        }
    }

    // ---- workspace file (application menu: Open / Save) ----

    /// Whether the single-layout pane taskbar should show: the active tab uses the explicit
    /// `single` preset, has more than one pane, and we're not in fullscreen. (The single
    /// preset renders only the focused pane, so the strip is how the hidden panes stay
    /// reachable — the native port of Electron's `PaneTaskbar` gate.)
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn taskbar_visible(&self) -> bool {
        let t = self.active_tab();
        t.layout == Layout::Single && t.panes.len() > 1 && !self.fullscreen
    }

    /// Snapshot the **active tab** into the persistable file shape — the native port of
    /// `serializeWorkspace()` (`{ name, layout, panes }`; runtime-only fields dropped). Pane
    /// identity is the label + color; the pane's original spawn command/args/shell ARE recorded
    /// so a reloaded pane re-runs its program (e.g. `claude`) rather than a plain shell.
    ///
    /// Per-pane zoom (Task 14) IS persisted: a pane whose terminal font differs from the base
    /// size carries its `font_size`, so a zoomed pane keeps its zoom across save→load. A pane
    /// at the base size omits it (it then tracks the current base on reload).
    ///
    /// **M6 change:** each pane's **live session uid** is now recorded too. A saved workspace
    /// used to be a pure launch *template* (re-spawn fresh); the library layer wants it to be
    /// re-*openable* while its panes are still running, so it carries the durable ids from M0
    /// and [`Self::load_workspace`] can reattach-or-spawn per pane
    /// ([`SessionManager::pane_load`]). A stale uid costs nothing: on the in-process backend,
    /// or once the session is gone, `pane_load` falls back to a fresh spawn from the recorded
    /// command/args/shell — exactly the old behaviour.
    /// Refuse a "save the active tab" action when that tab is the pinned Hyperpane, saying
    /// why. It is app-owned: it can't be closed and exactly one may exist, so a workspace
    /// file describing it would only ever be a second one waiting to be opened.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn refuse_saving_system_tab(&mut self) -> bool {
        if !self.active_tab().system {
            return false;
        }
        self.toast_active("the Hyperpane tab can't be saved");
        true
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub fn to_library_workspace_file(&self) -> WorkspaceFile {
        self.library_workspace_of(self.active_tab())
    }

    /// Snapshot ONE tab into the library shape (name/layout/panes + durable uids) — the
    /// single serializer behind [`Self::to_library_workspace_file`] (the active tab) and
    /// [`Self::save_set`] (every tab), so a set member and a saved workspace are byte-for-byte
    /// the same shape.
    #[tracing::instrument(level = "debug", skip_all)]
    fn library_workspace_of(&self, t: &Tab) -> WorkspaceFile {
        let base = self.settings.font_px.round() as u32;
        let panes: Vec<PaneSpec> = t
            .panes
            .iter()
            .map(|p| {
                let px = p.font_px.round() as u32;
                let mut spec = PaneSpec {
                    label: Some(p.title.to_string()),
                    color: Some(color_hex(p.accent)),
                    // The original program so a reloaded pane re-runs it (not a plain shell).
                    command: p.spawn_command.clone(),
                    args: p.spawn_args.clone(),
                    shell: p.spawn_shell.clone(),
                    // The live (shell-integration tracked) cwd, so a reloaded pane reopens
                    // where it was — same as the relaunch snapshot.
                    cwd: p.cwd.clone(),
                    // Only a zoomed pane records its size (keeps un-zoomed files clean).
                    font_size: (px != base).then_some(px),
                    uid: Some(p.uid.clone()),
                    ..Default::default()
                };
                // A Claude pane must reload as a Claude pane, not as the plain shell its
                // command alone would suggest once detection has upgraded it. `Terminal`
                // writes nothing, so an ordinary pane's file stays byte-identical.
                spec.set_pane_kind(&p.kind);
                // …and it must reload into the same CONVERSATION, not a fresh one. The kind
                // above already says which tool, so the mark records only which chat, where.
                if let Some(m) = &p.tool_session {
                    m.write_into(spec.meta.get_or_insert_with(Default::default));
                }
                spec
            })
            .collect();
        WorkspaceFile {
            name: Some(t.title.to_string()),
            layout: Some(theme::layout_name(t.layout).to_string()),
            panes: Some(panes),
            ..Default::default()
        }
    }

    /// The library snapshot of tab `i`, or `None` when that tab has no panes — a 0-pane tab
    /// describes nothing and must never be written as a set member. The pinned Hyperpane is
    /// skipped for a second reason: it is app-owned, can never be closed, and exactly one may
    /// exist, so a member describing it could only ever append a duplicate on open.
    #[tracing::instrument(level = "debug", skip_all)]
    fn library_workspace_of_tab(&self, i: usize) -> Option<WorkspaceFile> {
        let t = self
            .tabs
            .get(i)
            .filter(|t| !t.panes.is_empty() && !t.system)?;
        Some(self.library_workspace_of(t))
    }

    /// Snapshot **every tab** into the persistable file shape — the relaunch-restore
    /// ("last session") variant of [`Self::to_library_workspace_file`]. One `GroupSpec` per tab
    /// (layout + split state + focus/zoom) so a plain relaunch rebuilds the whole window,
    /// not just the active tab. Two deliberate differences from the save-dialog snapshot:
    ///   * `color` is recorded only for a PINNED accent (project tint / manual recolor) —
    ///     a clean slot-coloured pane restores clean instead of becoming a tinted
    ///     project pane (specs with a color pin their accent on load);
    ///   * each pane's live `cwd` (shell-integration tracked) is recorded so the
    ///     restored shell reopens where it was;
    ///   * the pane's original spawn `command`/`args`/`shell` are recorded so restore re-runs
    ///     the program it was running (e.g. `claude`) instead of a plain shell — the
    ///     crash-recovery gap this prep fixes;
    ///   * each pane's live session `uid` is recorded so a future session-daemon relaunch can
    ///     `Attach{uid}` a surviving session before falling back to a re-spawn (the M2
    ///     re-attach payoff in `docs/session-daemon-plan.md`).
    /// Per-pane zoom (#14): exactly like the workspace path, a pane whose terminal font
    /// differs from the base size records `font_size`, so zoom survives a plain relaunch.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn to_session_file(&self) -> WorkspaceFile {
        let base = self.settings.font_px.round() as u32;
        // A 0-pane tab can exist transiently while the window is closing (the emptied
        // last tab is left in place — `take_pane_in`), and this snapshot is taken exactly
        // then. Never persist it: an empty group restores to nothing, and an `active`
        // index pointing at it would land the relaunch on an empty tab. Filter, and remap
        // `active` to the filtered indexing.
        let mut active_out = 0u32;
        let groups: Vec<GroupSpec> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.panes.is_empty())
            .enumerate()
            .map(|(out_i, (i, t))| {
                if i == self.active {
                    active_out = out_i as u32;
                }
                let panes: Vec<PaneSpec> = t
                    .panes
                    .iter()
                    .map(|p| {
                        let px = p.font_px.round() as u32;
                        let mut spec = PaneSpec {
                            label: Some(p.title.to_string()),
                            color: p.pinned_accent.map(color_hex),
                            // The original program (command/args/shell) so restore re-runs it
                            // instead of a default shell — and the live session uid so a future
                            // session-daemon relaunch can re-attach a surviving session by uid
                            // before falling back to a re-spawn (session-daemon plan, M2).
                            // Live-Claude meta ("claude.session") is embedded at the App layer
                            // (`App::embed_claude_sessions`) — only the control host knows a
                            // control-spawned pane's external pane id (the hook-marker key).
                            command: p.spawn_command.clone(),
                            args: p.spawn_args.clone(),
                            shell: p.spawn_shell.clone(),
                            cwd: p.cwd.clone(),
                            font_size: (px != base).then_some(px),
                            uid: Some(p.uid.clone()),
                            ..Default::default()
                        };
                        // Same as the library snapshot: identity survives a relaunch, so a
                        // restored Claude pane is branded before its first byte of output
                        // rather than waiting to be re-detected.
                        spec.set_pane_kind(&p.kind);
                        // And the conversation with it, so a restarted tool pane resumes the
                        // chat it was in instead of opening an empty one.
                        if let Some(m) = &p.tool_session {
                            m.write_into(spec.meta.get_or_insert_with(Default::default));
                        }
                        spec
                    })
                    .collect();
                GroupSpec {
                    title: Some(t.title.to_string()),
                    layout: Some(theme::layout_name(t.layout).to_string()),
                    panes,
                    sizes: Some(t.sizes.clone()),
                    main_fraction: Some(t.main_fraction),
                    focused: Some(t.focused as u32),
                    zoomed: t.zoomed.map(|z| z as u32),
                    // Only written when set, so an ordinary tab's group is byte-identical to
                    // what earlier versions wrote.
                    system: t.system.then_some(true),
                }
            })
            .collect();
        WorkspaceFile {
            groups: Some(groups),
            active: Some(active_out),
            ..Default::default()
        }
    }

    /// "Save workspace…": pick a destination via the native save dialog and write the active
    /// tab's serialized workspace there (versioned `.avada` container by default; the
    /// reader keeps accepting legacy bare `.json`). No-op if the dialog is cancelled.
    /// Save to the remembered [`Self::workspace_path`] when there is one (a silent write-back,
    /// the usual Save semantics), otherwise fall through to [`Self::save_workspace_as`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_workspace(&mut self) {
        if self.refuse_saving_system_tab() {
            return;
        }
        if let Some(path) = self.workspace_path.clone() {
            self.write_workspace_to(&path);
            return;
        }
        self.save_workspace_as();
    }

    /// "Save workspace as…" (M6): ALWAYS prompt for a destination, write the active tab there,
    /// and remember it so a subsequent [`Self::save_workspace`] writes back silently. Defaults
    /// into the workspace library ([`paths::workspaces_dir`]) so saved workspaces gather in one
    /// place a set can reference. No-op if the dialog is cancelled.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_workspace_as(&mut self) {
        if self.refuse_saving_system_tab() {
            return;
        }
        let file = self.to_library_workspace_file();
        let default_name = match &file.name {
            Some(n) if !n.is_empty() => format!("{}.avada", sets::slug(n)),
            _ => "workspace.avada".to_string(),
        };
        let library = paths::workspaces_dir();
        let _ = std::fs::create_dir_all(&library);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Avada workspace", &["avada"])
            .add_filter("JSON workspace", &["json"])
            .set_directory(&library)
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };
        if self.write_workspace_to(&path) {
            self.set_workspace_path(path);
        }
    }

    /// Write the active tab's **library** snapshot (durable pane uids included) to `path`.
    /// The one place the app writes a workspace file; returns whether it landed.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn write_workspace_to(&mut self, path: &std::path::Path) -> bool {
        let file = self.to_library_workspace_file();
        let ok = write_workspace(path, &file);
        if ok {
            tracing::info!(path = %path.display(), "workspace saved");
        } else {
            tracing::warn!("failed to write workspace {}", path.display());
        }
        ok
    }

    /// "Open workspace…": pick a `.avada`/`.json` workspace via the native open dialog,
    /// read + validate it, and load its groups as new tabs (switching to the first).
    /// Non-destructive: existing tabs/sessions are left intact. No-op if cancelled or the
    /// file has no panes.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn open_workspace(&mut self, mgr: &SessionManager) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Workspace", &["avada", "json"])
            .pick_file()
        else {
            return;
        };
        let Some(file) = read_workspace(&path) else {
            tracing::warn!("{} is not a valid workspace", path.display());
            return;
        };
        self.load_workspace(file, mgr);
        // Remember where it came from, so plain "Save workspace" writes back here.
        self.set_workspace_path(path);
    }

    // ---- workspace sets (M6: the library layer over WorkspaceFile) ----

    /// "Save set…": write **every non-empty tab** of this window as a member workspace under
    /// [`paths::set_members_dir`], then write a [`WorkspaceSet`] naming them to `sets/<slug>.json`.
    /// The set file is picked with the native save dialog (its stem names the set) — there is
    /// no text-entry dialog in this UI, and the file name is the name the user is already
    /// typing. No-op if cancelled.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_set(&mut self) {
        let dir = paths::sets_dir();
        let _ = std::fs::create_dir_all(&dir);
        let default_name = format!("{}.json", sets::slug(self.active_tab().title.as_str()));
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Avada set", &["json"])
            .set_directory(&dir)
            .set_file_name(default_name)
            .save_file()
        else {
            return;
        };
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "set".to_string());
        self.save_set_to(&path, &paths::set_members_dir(), &name);
    }

    /// The dialog-free half of [`Self::save_set`] (the tested one). Writes one member
    /// workspace per non-empty tab into `members_dir` and the set index to `set_path`.
    /// Returns the set as written, or `None` if nothing could be saved.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn save_set_to(
        &mut self,
        set_path: &std::path::Path,
        members_dir: &std::path::Path,
        name: &str,
    ) -> Option<sets::WorkspaceSet> {
        let stem = sets::slug(name);
        let mut members = Vec::new();
        for i in 0..self.tabs.len() {
            let Some(ws) = self.library_workspace_of_tab(i) else {
                continue; // 0-pane tab
            };
            let title = ws.name.clone().unwrap_or_default();
            let member_path =
                members_dir.join(format!("{stem}-{}-{}.avada", i + 1, sets::slug(&title)));
            // `write_workspace` does not create directories; the set dir may be brand new.
            if let Some(parent) = member_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if !write_workspace(&member_path, &ws) {
                tracing::warn!("failed to write set member {}", member_path.display());
                continue;
            }
            members.push(sets::SetMember {
                path: member_path.to_string_lossy().into_owned(),
                name: (!title.is_empty()).then_some(title),
            });
        }
        if members.is_empty() {
            tracing::warn!("nothing to save into set {name:?} (no non-empty tabs)");
            return None;
        }
        let set = sets::WorkspaceSet {
            name: name.to_string(),
            members,
        };
        if !sets::write_set(set_path, &set) {
            tracing::warn!("failed to write set {}", set_path.display());
            return None;
        }
        Some(set)
    }

    /// "Open set…": pick a `sets/*.json`, then load every member workspace into this window.
    /// Each pane goes through the same reattach-or-spawn decision as any other load
    /// ([`SessionManager::pane_load`]), so a member whose panes are still alive in the daemon
    /// is ADOPTED rather than re-run. No-op if cancelled or unreadable.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn open_set(&mut self, mgr: &SessionManager) {
        let dir = paths::sets_dir();
        let _ = std::fs::create_dir_all(&dir);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Avada set", &["json"])
            .set_directory(&dir)
            .pick_file()
        else {
            return;
        };
        self.open_set_from(&path, mgr);
    }

    /// The dialog-free half of [`Self::open_set`] (the tested one). Returns how many member
    /// workspaces were loaded.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn open_set_from(&mut self, path: &std::path::Path, mgr: &SessionManager) -> usize {
        let Some(set) = sets::read_set(path) else {
            tracing::warn!("{} is not a valid workspace set", path.display());
            return 0;
        };
        let members = sets::load_members(&set);
        let n = members.len();
        // `load_workspace` selects the tab IT just appended, so loading N members in a row
        // would leave the window focused on member N. A set opens on its FIRST member — the
        // order the user wrote it in — so keep the first landing and restore it at the end.
        // (Nothing is purged after the first member: every appended tab has panes, so the
        // remembered index stays valid.)
        let mut first_landing: Option<usize> = None;
        for file in members {
            let landed = self.load_workspace(file, mgr);
            first_landing = first_landing.or(landed);
        }
        if let Some(i) = first_landing {
            self.active = i.min(self.tabs.len().saturating_sub(1));
            self.editing_tab = -1;
            self.dirty = true;
        }
        n
    }

    /// Load a parsed workspace file: append a tab per group of its first window and switch to
    /// the tab it landed on (the file's saved active tab, else the first appended one). Each
    /// pane takes the reattach-or-spawn decision from its spec (uid/label/color/cwd/command/
    /// shell). Shared by [`Self::open_workspace`] and [`Self::open_set_from`].
    ///
    /// Returns the index of the tab it selected, or `None` when the file described nothing
    /// and no tab was appended — [`Self::open_set_from`] needs that to land a multi-member
    /// set on its FIRST member rather than wherever the last member happened to select.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn load_workspace(&mut self, file: WorkspaceFile, mgr: &SessionManager) -> Option<usize> {
        let windows = windows_of(Some(&file));
        let win = windows.into_iter().next()?;
        let first_new = self.tabs.len();
        let saved_active = win.active.map(|a| a as usize);
        // Contentless groups are skipped by `append_tab_from_group`, which shifts the
        // indices of everything after them — remap the file's `active` to the tab its
        // group ACTUALLY became, so a saved active index can never select (or be clamped
        // onto) the wrong tab, let alone an empty one.
        let mut active_new: Option<usize> = None;
        for (i, g) in win.groups.into_iter().enumerate() {
            let before = self.tabs.len();
            self.append_tab_from_group(mgr, g);
            if self.tabs.len() > before && saved_active == Some(i) {
                active_new = Some(before);
            }
        }
        if self.tabs.len() > first_new {
            // Land on the file's saved active tab so a session restore comes back where
            // it was; absent or skipped → the first appended tab.
            self.active = active_new.unwrap_or(first_new);
            self.editing_tab = -1;
            // The file brought real content — drop any pre-existing 0-pane tab. The only
            // legal one is `State::new`'s pristine placeholder (a workspace-seeded window
            // loads into a fresh State whose seed tab never got a pane); leaving it in
            // produces a ghost empty "term 1" tab next to the restored session (the live
            // 0-pane-tab sighting that motivated the B6 hardening).
            self.purge_empty_tabs();
            // The Hyperpane is one per process, and lives at index 0: a file whose groups
            // came from another install (or a hand edit) may carry more than one flagged
            // group, and `append_tab_from_group` only skips a flagged group when a system
            // tab ALREADY exists — two flagged groups in the same file both land. Keep the
            // first as the system tab, demote the rest to ordinary tabs, and pin.
            let mut seen = false;
            for t in &mut self.tabs {
                if t.system {
                    if seen {
                        tracing::warn!(title = %t.title, "demoting a duplicate Hyperpane tab");
                        t.system = false;
                    }
                    seen = true;
                }
            }
            self.pin_system_tab_first();
            self.dirty = true;
            tracing::info!(tabs = self.tabs.len() - first_new, "workspace restored");
            return Some(self.active);
        }
        None
    }

    /// Remove every 0-pane tab, keeping `active` pointed at the same tab. Callers must
    /// guarantee at least one non-empty tab remains.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn purge_empty_tabs(&mut self) {
        let mut i = 0;
        while i < self.tabs.len() {
            if self.tabs[i].panes.is_empty() {
                self.tabs.remove(i);
                if self.active > i {
                    self.active -= 1;
                }
            } else {
                i += 1;
            }
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
    }

    /// Attach panes (spawned from specs) into the ACTIVE tab — the `--as panes` routing of
    /// a second-instance hand-off. Sizes re-equalize; specs that spawn nothing are skipped.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn attach_panes_from_specs(&mut self, mgr: &SessionManager, specs: &[PaneSpec]) {
        let palette = self.settings.frame_palette;
        let start = self.active_tab().panes.len();
        let mut added = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            if let Some(ps) = self.make_pane_from_spec(mgr, start + i, spec) {
                added.push(ps);
            }
        }
        if added.is_empty() {
            return;
        }
        let t = self.active_tab_mut();
        t.panes.extend(added);
        t.sizes = equal_sizes(t.panes.len());
        t.relabel(palette);
        self.dirty = true;
    }

    /// Build a tab from a `GroupSpec` (spawning a pane per spec) and append it.
    #[tracing::instrument(level = "debug", skip_all)]
    fn append_tab_from_group(&mut self, mgr: &SessionManager, g: GroupSpec) {
        // Exactly one Hyperpane may exist — it is app-owned and can never be closed — so a
        // file that describes one while we already have one must not append a second. This is
        // checked BEFORE any pane is spawned: a duplicate that re-attached the live session
        // would show the same terminal in two tabs, which is how the duplicates got here.
        let hyperpane = group_is_hyperpane(&g);
        if hyperpane && self.tabs.iter().any(|t| t.system) {
            return;
        }
        let palette = self.settings.frame_palette;
        let title: SharedString = match g.title {
            Some(t) if !t.is_empty() => t.into(),
            _ => self.fresh_tab_title(),
        };
        let layout = g
            .layout
            .as_deref()
            .map(layout_from_name)
            .unwrap_or(Layout::Auto);
        let mut tab = Tab::empty(title);
        tab.layout = layout;
        for (i, spec) in g.panes.iter().enumerate() {
            if let Some(ps) = self.make_pane_from_spec(mgr, i, spec) {
                tab.panes.push(ps);
            }
        }
        let n = tab.panes.len();
        if n == 0 {
            return; // a contentless group — skip it
        }
        tab.sizes = match &g.sizes {
            Some(s) if s.len() == n => s.clone(),
            _ => equal_sizes(n),
        };
        if let Some(mf) = g.main_fraction {
            tab.main_fraction = clamp_fraction(mf);
        }
        tab.focused = g.focused.map(|f| (f as usize).min(n - 1)).unwrap_or(0);
        tab.zoomed = g.zoomed.map(|z| (z as usize).min(n - 1));
        // Restoring the flag matters as much as writing it: without it the restored
        // "Hyperpane" would be an ordinary tab, and `ensure_hyperpane_tab` would then find
        // no system tab and append a second one. A legacy file carries no flag, so the
        // recognizer above supplies it — that is what stops the files already on disk from
        // producing a duplicate on their very next relaunch.
        tab.system = hyperpane;
        tab.relabel(palette);
        self.tabs.push(tab);
    }

    /// Spawn a pane from a `PaneSpec` (its command/args/cwd/shell), returning the `PaneState`.
    /// A spec with a `color` is treated like a project pane (tinted: frame + dot on, accent
    /// pinned); a colorless spec is a clean pane coloured by slot.
    #[tracing::instrument(level = "debug", skip_all)]
    fn make_pane_from_spec(
        &mut self,
        mgr: &SessionManager,
        idx: usize,
        spec: &PaneSpec,
    ) -> Option<PaneState> {
        let palette = self.settings.frame_palette;
        let shell = spec
            .shell
            .clone()
            .or_else(|| prefs::effective_shell(&self.settings.default_shell));
        let shell_path = shell
            .clone()
            .unwrap_or_else(avada_core::session::spawn::default_shell);
        let integration = avada_core::shell_integration::integration_for(
            &shell_path,
            &avada_core::shell_integration::shell_integration_dir(),
        )
        .map(|si| avada_core::session_manager::Integration {
            args: si.args,
            env: si.env.into_iter().collect(),
        });
        let (cols, rows) = self.spawn_cells();

        // A recorded kind is what the pane WAS, and outranks re-deriving it from the
        // command: detection may have upgraded a shell pane to a tool pane after it was
        // spawned, and that upgrade is precisely what the snapshot exists to preserve.
        // Only a spec with no recorded kind — every file written before this feature —
        // falls back to naming the kind from the program. Read here, before the re-attach
        // decision, because a non-pty view pane skips that decision entirely (D3).
        let kind = match spec.pane_kind() {
            PaneKind::Terminal => spec
                .command
                .as_deref()
                .map(PaneKind::for_command)
                .unwrap_or_default(),
            k => k,
        };
        let is_view = !kind.is_pty();

        // ---- M2 re-attach decision (session-daemon-plan "Reconnect / re-attach") ----
        // When the backend is the daemon AND the snapshot recorded this pane's session uid
        // AND that session is STILL ALIVE in the daemon (the program survived the last GUI
        // crash/quit), we re-ADOPT the surviving pty under that exact uid instead of spawning
        // a new one: the live process comes back on screen, its prior output replayed into the
        // fresh grid. Otherwise (in-process backend, no recorded uid, or a dead/unknown uid —
        // the program had exited) we fall back to today's behaviour: re-spawn from the spec
        // (Prep made the spec re-run the original program, not a bare shell).
        // The decision itself lives in core (`SessionManager::pane_load`) so every load path —
        // relaunch restore, Open workspace, Open set (M6) — branches identically, and so it can
        // be tested against a real daemon (`daemon_client::tests`).
        // A view pane never had a session, so it never asks the backend about one — that
        // question is exactly the phantom D3 exists to prevent. Its recorded uid is also
        // deliberately NOT reused: `view-N` comes from a per-run counter, so honouring a
        // restored `view-3` could collide with the next pane this run mints. Nothing keys
        // off a view uid across runs, so a fresh one costs nothing and closes the class.
        let (reattach, uid) = if is_view {
            (false, fresh_view_uid())
        } else {
            let load = mgr.pane_load(spec.uid.as_deref());
            (load.is_reattach(), load.uid().to_string())
        };

        // ---- Claude conversation resume (the dead-session fallback) ----
        // The snapshot carries the pane's live conversation id as meta (claude_panes::META_KEY,
        // fed by the Claude Code SessionStart hook). A re-attached survivor still HAS that
        // conversation running — only a re-spawn needs to pick it back up. Re-validated here
        // because workspace.json is user-editable and the id lands on a command line.
        let resume_id = if reattach {
            None
        } else {
            spec.meta
                .as_ref()
                .and_then(|m| m.get(avada_core::claude_panes::META_KEY))
                .filter(|id| avada_core::claude_panes::valid_session_id(id))
                .cloned()
        };
        // Two resume shapes: a pane whose *program* is claude gets `--resume <id>` appended to
        // its own spawn; a shell pane instead has the resume line typed at first output (through
        // the interactive shell, so a user's `claude` alias — wrapper/scope included — applies).
        // A pane running some other program ignores the marker rather than typing into it.
        // The conversation's own cwd (claude_panes::META_CWD_KEY) overrides the pane cwd and
        // prefixes the typed line with a `cd`: `--resume` only finds sessions in the current
        // directory's project, and the snapshot cwd has been observed stale (OSC 7 silence
        // inside a TUI across a GUI re-attach).
        let resume_cwd = (!reattach)
            .then_some(spec.meta.as_ref())
            .flatten()
            .and_then(|m| m.get(avada_core::claude_panes::META_CWD_KEY))
            .filter(|c| avada_core::claude_panes::valid_resume_cwd(c))
            .cloned();
        // The account (CLAUDE_CONFIG_DIR) the conversation was saved under. `claude` stores
        // transcripts in `$CLAUDE_CONFIG_DIR/projects`, so resume must re-set it or claude
        // looks in `~/.claude` and finds nothing (multi-account). Empty/absent ⇒ the default
        // account — leave the env alone.
        let resume_config_dir = (!reattach)
            .then_some(spec.meta.as_ref())
            .flatten()
            .and_then(|m| m.get(avada_core::claude_panes::META_CONFIG_DIR_KEY))
            .filter(|d| avada_core::claude_panes::valid_config_dir(d))
            .cloned();
        let mut spawn_command = spec.command.clone();
        let mut spawn_args = spec.args.clone();
        let mut spawn_cwd = spec.cwd.clone();
        let mut spawn_env: Option<avada_core::session::spawn::EnvMap> = None;
        let mut startup = None;
        if let Some(id) = &resume_id {
            // `CLAUDE_CONFIG_DIR='<dir>' ` prefix for typed resume lines (the shell-pane path);
            // empty when there's no recorded account (the default).
            let cfg_prefix = resume_config_dir
                .as_deref()
                .map(|d| format!("CLAUDE_CONFIG_DIR='{d}' "))
                .unwrap_or_default();
            let head = spawn_command
                .as_deref()
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("");
            let head = head.rsplit(['/', '\\']).next().unwrap_or(head);
            if spawn_command.is_none() {
                startup = Some(resume_startup_line(
                    resume_cwd.as_deref(),
                    &cfg_prefix,
                    "claude",
                    &format!("--resume {id}"),
                ));
            } else if head == "claude" || head == "claude.exe" {
                if resume_cwd.is_some() {
                    spawn_cwd = resume_cwd.clone();
                }
                // Direct claude relaunch keeps its born-with flags; the account rides the spawn
                // env rather than the command line so the resumed process reads the right store.
                if let Some(dir) = &resume_config_dir {
                    spawn_env
                        .get_or_insert_with(Default::default)
                        .insert("CLAUDE_CONFIG_DIR".to_string(), dir.clone());
                }
                match &mut spawn_args {
                    // Direct-argv spawn: extend the argv.
                    Some(a) => a.extend(["--resume".to_string(), id.clone()]),
                    // Shell-string spawn: extend the command line.
                    None => spawn_command = spawn_command.map(|c| format!("{c} --resume {id}")),
                }
            }
        }

        // ---- Every other tool's conversation resume ----
        // Claude is resumed above from a hook-written marker; no other tool offers a hook.
        // What every tool does have is the pane Avada itself opened out of the left
        // panel's session list: that pane was handed one exact conversation, and the mark
        // records which one, in which directory. A re-spawn puts the pane back into it.
        //
        // The mark is read even when it isn't used, so a pane that came back by re-attach
        // still carries it into the NEXT snapshot instead of forgetting after one restart.
        let tool_session = ToolSessionMark::read(spec.meta.as_ref());
        // Kept for the `PaneState` below: a tool pane never re-learns its directory the way
        // a shell does — a process parked in a TUI stops emitting OSC 7 — so without this
        // the pane's cwd stays empty, the next snapshot writes `cwd: null`, and the pane
        // reads as belonging to no project at all.
        let mut resume_cwd: Option<String> = None;
        if let Some(mark) = tool_session
            .as_ref()
            // A re-attached survivor is still IN the conversation, and the Claude block
            // above has already resumed the panes it applies to.
            .filter(|_| !reattach && resume_id.is_none())
        {
            // `kind` is the only record of WHICH tool (the mark deliberately doesn't repeat
            // it) and `tools::resume_args` the only record of HOW. A tool with no verified
            // resume flag starts fresh rather than being handed a guessed one.
            let extra = match &kind {
                PaneKind::Tool(tool) => {
                    avada_core::tools::resume_args(tool, &mark.id).map(|a| (tool.clone(), a))
                }
                // ...unless the pane's kind cannot say. A pane the user turned into a tool
                // pane by TYPING the tool's name persists as `Terminal` — the foreground
                // sniff re-brands the chrome but deliberately never writes the persisted
                // kind — so the arm above answers `None` for exactly the panes this whole
                // feature exists to rescue. A mark adopted for such a pane names its own
                // tool; that name is validated against the registry on the way in, so it
                // can only ever be a real id.
                _ => mark.tool.as_deref().and_then(|tool| {
                    avada_core::tools::resume_args(tool, &mark.id).map(|a| (tool.to_string(), a))
                }),
            };
            // A pane opened from the session list already persists `<tool> --resume <id>` as
            // its command; appending a second `--resume` is a usage error, not a no-op.
            let already = spawn_command
                .as_deref()
                .is_some_and(|c| c.contains(&mark.id))
                || spawn_args
                    .as_ref()
                    .is_some_and(|a| a.iter().any(|x| x == &mark.id));
            // Resume is directory-scoped for all three of them, so the conversation's own
            // directory wins over the possibly-stale snapshot cwd — the same reason the
            // Claude arm prefers `claude.cwd`. Outside the `already` gate deliberately: that
            // gate exists to stop a SECOND `--resume` being appended, and a pane opened from
            // the session list is exactly the pane that trips it. Directing the cwd from the
            // same `if` put every such pane back in `$HOME` on the next launch, where
            // `--resume <id>` finds no project and the conversation is gone.
            //
            // Only for a tool that HAS a verified resume shape: one that starts fresh is an
            // ordinary new pane and belongs wherever the snapshot put it.
            if extra.is_some() {
                spawn_cwd = Some(mark.cwd.clone());
                resume_cwd = Some(mark.cwd.clone());
            }
            if let Some((tool, extra)) = extra.filter(|_| !already) {
                match (&mut spawn_args, &mut spawn_command) {
                    // Direct-argv spawn: extend the argv.
                    (Some(a), _) => a.extend(extra),
                    // Shell-string spawn: extend the command line.
                    (None, Some(c)) => {
                        *c = format!("{c} {}", extra.join(" "));
                    }
                    // A tool pane running an interactive shell: type the resume line at first
                    // output, through the shell, so the user's own alias for the tool applies
                    // (the same shape the Claude shell-pane arm uses).
                    (None, None) => {
                        let bin = avada_core::tools::by_id(&tool)
                            .map(|t| t.bin)
                            .unwrap_or(tool.as_str());
                        startup = Some(resume_startup_line(
                            Some(&mark.cwd),
                            "",
                            bin,
                            &extra.join(" "),
                        ));
                    }
                }
            }
        }

        // Same resolution order as a hand-made pane, with one difference that matters: a
        // RESTORED pane already carries its own cwd (from the snapshot, or forced above by a
        // tool resume), and that always wins — a workspace that comes back in the wrong
        // directory loses the `--resume` project along with it. Only a spec that never had one
        // falls through to the focused pane / project anchor / launch dir.
        let spawn_cwd = self.resolve_new_pane_cwd(&kind, spawn_cwd);

        // A survivor's grid is born at the size the DAEMON says its pty has, not at
        // `spawn_cells()`'s 80x24 guess. The replay we are about to feed it is the raw pty
        // stream, cursor-positioning escapes and all, written for that width — replay it
        // narrower or wider and every absolute column move lands somewhere else, so the
        // scrollback comes back with lines overwriting each other. Seed at the daemon's
        // width, let the pump's `place()` reflow to the real rect afterwards (alacritty's
        // reflow preserves scrollback; a mis-rendered seed is not recoverable).
        // `None` = daemon can't say (predates `SessionMeta::cols`, or in-process backend) —
        // keep the guess.
        let (cols, rows) = match reattach.then(|| mgr.dims(&uid)).flatten() {
            Some((c, r)) if c >= 2 && r >= 1 => (c, r),
            _ => (cols, rows),
        };
        let mut pane = TerminalPane::with_scrollback(
            cols as usize,
            rows as usize,
            self.settings.scrollback as usize,
            Box::new(SoftwareRenderer::new()),
        );
        pane.set_palette(theme::terminal_theme(self.settings.terminal_theme));
        pane.set_project_roots(self.project_roots());
        if reattach {
            // Re-host the survivor: seed the fresh grid from the daemon's retained replay (the
            // same replay-into-a-fresh-grid path `adopt_into_tab` uses for a moved pane). No
            // pty spawn — the live process is already running in the daemon.
            if let Some(replay) = mgr.replay(&uid) {
                pane.feed(&replay);
            }
        } else if !is_view {
            Self::spawn_session_async(
                mgr,
                SpawnOptions {
                    uid: uid.clone(),
                    cols: Some(cols),
                    rows: Some(rows),
                    pane_id: Some(uid.clone()),
                    cwd: spawn_cwd,
                    // Cloned so the resolved shell is also kept on the PaneState (below) for a
                    // subsequent relaunch snapshot.
                    shell: shell.clone(),
                    command: spawn_command,
                    args: spawn_args,
                    // `BROWSER` points at our shim so a link a tool in this pane wants to
                    // show goes back through `App::route_event` → `Command::OpenLink` (and
                    // thus Preferences → Browser) instead of straight to the OS. Wrapped
                    // here and not where the env is recorded on the pane: the shim path is
                    // regenerated every run, so it must not be baked into a snapshot.
                    env: avada_core::open::with_browser_shim(spawn_env),
                    integration,
                    ..Default::default()
                },
            );
        }
        let glow = Glow::new(crate::glow::seed_from(&uid));
        let pinned = spec.color.as_deref().map(parse_hex);
        let project = pinned.is_some();
        let label = match &spec.label {
            Some(l) if !l.is_empty() => l.clone(),
            _ if idx == 0 => "shell".to_string(),
            _ => format!("pane {}", idx + 1),
        };
        // Restore the pane's persisted per-pane zoom (Task 14); absent → the configured base.
        let font_px = spec
            .font_size
            .map(|s| Settings::clamp_font(s as f32))
            .unwrap_or(self.settings.font_px);
        let font = theme::load_font_at(&self.settings.font_path(), font_px, self.last_scale);
        Some(PaneState {
            uid,
            kind,
            tool_session,
            title: label.into(),
            subtitle: None,
            show_frame: Some(project),
            show_dot: Some(project),
            accent: pinned.unwrap_or_else(|| theme::accent_for(idx, palette)),
            pane,
            applied: (cols as usize, rows as usize),
            pty: (cols as usize, rows as usize),
            pty_since: std::time::Instant::now(),
            surface: Image::default(),
            rect: (0.0, 0.0, 0.0, 0.0),
            visible: true,
            // A re-attached survivor is already running (replay-primed, like an adopted
            // pane); a freshly spawned pane starts unstarted so its first-output startup
            // pump tracks the new shell coming up.
            started: reattach,
            startup,
            pinned_accent: pinned,
            surf: (0.0, 0.0),
            link: None,
            link_cursor: (0.0, 0.0),
            glow,
            shell_title: String::new(),
            ai_muted: false,
            talk: spec.talk.unwrap_or(false),
            recording: false,
            ai: AiLine::default(),
            last_toast: String::new(),
            scrollbar_on: false,
            search_focus_seq: 0,
            refocus_seq: 0,
            font_px,
            font,
            font_dirty: false,
            // Restored view panes get their target back the same way (2); a restored pty pane
            // re-learns its cwd from the shell it just respawned.
            cwd: is_view.then(|| spec.cwd.clone()).flatten().or(resume_cwd),
            env: None,
            // The resolved shell program → its short header badge (computed once here);
            // suppressed for a view pane for the same reason as in `make_pane`.
            shell_label: if is_view {
                String::new()
            } else {
                shell_label(&shell_path)
            },
            // Carry the spawned program forward so a later relaunch snapshot still records it
            // (the spec's command/args + the resolved shell).
            spawn_command: (!is_view).then(|| spec.command.clone()).flatten(),
            spawn_args: (!is_view).then(|| spec.args.clone()).flatten(),
            spawn_shell: (!is_view).then_some(shell).flatten(),
        })
    }
}

/// Track F: reminder panes — park a live pane until a chosen time. Its own `impl` block so
/// the feature reads as one unit. Parking reuses the detach machinery (the session stays
/// alive centrally, exactly like the `closed` history); restoring reuses `adopt_pane` (replay-primed
/// re-dock into the ACTIVE tab). All methods follow the mutate→set-dirty seam.
impl State {
    /// Park active-tab pane `idx` until `offset` from now: remove it from the layout WITHOUT
    /// killing its session and push a [`Reminder`]. Gated when it's the only pane of the only
    /// tab (parking it would empty the window). The bell list is where it lives meanwhile.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn remind_pane(&mut self, idx: usize, offset: ReminderOffset) {
        if self.tabs.len() <= 1 && self.active_tab().panes.len() < 2 {
            return;
        }
        // The system tab's only pane is not parked either: there is no manager here to
        // refill the tab with, and a Hyperpane that sits empty until the bell fires is worse
        // than a reminder that has to be set on a split. Refused rather than reseeded.
        if self.active_tab().system && self.active_tab().panes.len() < 2 {
            tracing::info!("refusing to park the system tab's only pane as a reminder");
            return;
        }
        let Some(dp) = self.detach_pane_idx(idx) else {
            return;
        };
        let (delay_ms, due_label) = reminder_due(offset);
        self.reminders.push(Reminder {
            pane: dp,
            due_ms: crate::glow::now_epoch_ms() + delay_ms,
            due_label: due_label.into(),
            fired: false,
            fired_at_ms: 0,
            toast_dismissed: false,
        });
        self.dirty = true;
    }

    /// Toggle the bell's reminder-list panel (collapses the projects flyout — one rail
    /// panel at a time, mirroring how the flyouts behave).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_reminders(&mut self) {
        self.reminders_open = !self.reminders_open;
        if self.reminders_open {
            self.sidebar_open = false;
            self.closed_open = false;
        }
        self.dirty = true;
    }

    /// Mark any reminder whose due time has passed as `fired` (the bell/list highlight).
    /// Called by the app tick with the current epoch ms; returns whether anything changed.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn tick_reminders(&mut self, now_ms: u64) -> bool {
        let mut changed = false;
        for r in &mut self.reminders {
            if !r.fired && now_ms >= r.due_ms {
                r.fired = true;
                r.fired_at_ms = now_ms;
                changed = true;
            }
            // Age out the alert toast (the bell badge stays — only the toast is transient).
            if r.fired && !r.toast_dismissed && now_ms >= r.fired_at_ms + REMINDER_TOAST_MS {
                r.toast_dismissed = true;
                changed = true;
            }
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    /// The fired-reminder alert toast's × was clicked: hide the toast without restoring the
    /// pane. The reminder itself (and the bell badge) stays until the pane is restored.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn dismiss_reminder_toast(&mut self, uid: &str) {
        if let Some(r) = self.reminders.iter_mut().find(|r| r.pane.uid == uid) {
            if !r.toast_dismissed {
                r.toast_dismissed = true;
                self.dirty = true;
            }
        }
    }

    /// A bell-list row was clicked: re-dock the parked pane into the ACTIVE tab's layout
    /// (replay-primed, focused — the standard `adopt_pane` path) and clear its reminder.
    /// Keyed by session uid so a row click can't race a concurrent list change.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn restore_reminder(&mut self, uid: &str, mgr: &SessionManager) {
        let Some(i) = self.reminders.iter().position(|r| r.pane.uid == uid) else {
            return;
        };
        let r = self.reminders.remove(i);
        self.reminders_open = false;
        self.adopt_pane(mgr, r.pane); // focuses the pane + sets dirty
    }
}

/// Closing a pane or tab: the confirmation that precedes it and the history that survives it.
/// Its own `impl` block so the feature reads as one unit.
///
/// The shape is deliberate. A confirm dialog and an undo list are two answers to the same
/// worry — "I didn't mean to close that" — and answering it twice is how a workspace ends up
/// nagging on every ×. So the close itself is made *reversible* first: closing parks the pane
/// or tab with its session still running, and reopening re-docks that same shell rather than
/// respawning one. The card is then only the second line of defence, and the one close it can
/// never be undone for — the last pane of the last tab, which quits — is confirmed
/// unconditionally, preference or not.
impl State {
    // ---- asking first ----

    /// The × on a pane. Puts the confirm card up (and returns `true` — nothing has closed
    /// yet); closes straight through when the card is switched off and the close is undoable.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn request_close_pane(&mut self, ti: usize, idx: usize, mgr: &SessionManager) -> bool {
        let Some(ps) = self.tabs.get(ti).and_then(|t| t.panes.get(idx)) else {
            return true;
        };
        let pending = PendingClose {
            target: CloseTarget::Pane(ps.uid.to_string()),
            title: ps.title.clone(),
            final_close: self.ends_window_pane(ti, idx),
        };
        // A view pane is read-only and has no session behind it (D3): there is no work in
        // flight to interrupt and nothing to lose but a scroll position, so the card has
        // nothing to warn about and would be pure nagging. The window-ending close still
        // asks — that one quits, and no pane kind makes quitting undoable.
        let read_only = !ps.kind.is_pty();
        if self.skip_confirm(&pending) || (read_only && !pending.final_close) {
            return self.close_pane_menu(ti, idx, mgr);
        }
        self.ask_close(pending);
        true
    }

    /// [`Self::request_close_pane`] for the focused pane of the active tab (the keybinding).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn request_close_focused(&mut self, mgr: &SessionManager) -> bool {
        let ti = self.active;
        let idx = self.tabs[ti].focused;
        self.request_close_pane(ti, idx, mgr)
    }

    /// The × on a tab — same contract as [`Self::request_close_pane`].
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn request_close_tab(&mut self, idx: usize, mgr: &SessionManager) -> bool {
        let Some(t) = self.tabs.get(idx) else {
            return true;
        };
        // A tab has no uid of its own, so it borrows its first pane's: the most stable name
        // available for "this tab" once the indices underneath may have shifted.
        let Some(first) = t.panes.first() else {
            return self.close_tab_menu(idx, mgr);
        };
        let n = t.panes.len();
        let pending = PendingClose {
            target: CloseTarget::Tab(first.uid.to_string()),
            title: if n > 1 {
                format!("{} — {n} panes", t.title).into()
            } else {
                t.title.clone()
            },
            final_close: self.tabs.len() <= 1,
        };
        if self.skip_confirm(&pending) {
            return self.close_tab_menu(idx, mgr);
        }
        self.ask_close(pending);
        true
    }

    /// Whether this close may go through unasked: only when the human turned the card off
    /// AND the close is one the history can undo. A window-ending close always asks.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn skip_confirm(&self, pending: &PendingClose) -> bool {
        !self.settings.confirm_close && !pending.final_close
    }

    /// Raise the confirm card over whatever else was open.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn ask_close(&mut self, pending: PendingClose) {
        self.pending_close = Some(pending);
        self.overlay = Overlay::ConfirmClose;
        self.dirty = true;
    }

    /// The card's "Close" button. Re-resolves the held uid against the CURRENT layout — a
    /// background shell can exit and renumber the tabs while the question is on screen — so a
    /// target that is already gone simply closes the card. Returns whether the window lives.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn confirm_close_go(&mut self, mgr: &SessionManager) -> bool {
        let Some(pending) = self.pending_close.take() else {
            self.close_overlay_now();
            return true;
        };
        self.close_overlay_now();
        match pending.target {
            CloseTarget::Pane(uid) => match self.find_pane(&uid) {
                Some((ti, idx)) => self.close_pane_menu(ti, idx, mgr),
                None => true,
            },
            CloseTarget::Tab(uid) => match self.find_pane(&uid) {
                Some((ti, _)) => self.close_tab_menu(ti, mgr),
                None => true,
            },
        }
    }

    /// The card's "Don't ask again" checkbox. Persisted with the other prefs; it only ever
    /// governs the undoable closes (see [`Self::skip_confirm`]).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set_confirm_close(&mut self, on: bool) {
        if self.settings.confirm_close != on {
            self.settings.confirm_close = on;
            prefs::save(&self.settings);
            self.dirty = true;
        }
    }

    // ---- the history ----

    /// Toggle the rail's RECENTLY CLOSED section (collapsing the other two — one panel at a
    /// time, mirroring the bell and the projects flyout).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn toggle_closed(&mut self) {
        self.closed_open = !self.closed_open;
        if self.closed_open {
            self.sidebar_open = false;
            self.reminders_open = false;
        }
        self.dirty = true;
    }

    /// Reopen the most recently closed pane or tab — the keyboard/menu command. No-op on an
    /// empty history.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn reopen_closed(&mut self, mgr: &SessionManager) {
        let Some(item) = self.closed.pop() else {
            return;
        };
        self.restore_item(item, mgr);
    }

    /// A history-row click. Keyed by the row's id rather than its index so a click can't race
    /// a concurrent close (which pushes) or an eviction (which shifts everything down).
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn restore_closed(&mut self, id: i32, mgr: &SessionManager) {
        let Some(i) = self.closed.iter().position(|c| c.id == id) else {
            return;
        };
        let item = self.closed.remove(i);
        self.closed_open = false;
        self.restore_item(item, mgr);
    }

    /// Re-host one history entry: a pane re-docks into the ACTIVE tab (the same replay-primed
    /// `adopt_pane` the bell uses), a tab comes back as its own tab, switched to.
    #[tracing::instrument(level = "debug", skip_all)]
    fn restore_item(&mut self, item: ClosedItem, mgr: &SessionManager) {
        match item.what {
            ClosedWhat::Pane(p) => self.adopt_pane(mgr, *p), // focuses the pane + sets dirty
            ClosedWhat::Tab(t) => self.reopen_tab(mgr, *t),
        }
    }

    /// The × on a history row: give up on this entry for good, killing the sessions it was
    /// holding open. Same finality as an eviction, just asked for.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn discard_closed(&mut self, id: i32, mgr: &SessionManager) {
        let Some(i) = self.closed.iter().position(|c| c.id == id) else {
            return;
        };
        let item = self.closed.remove(i);
        for p in item.panes() {
            kill_session_of(mgr, &p.uid, &p.kind);
        }
        self.dirty = true;
    }
}

/// Seconds since LOCAL midnight (the wall clock — what "tomorrow 9am" means to the user).
#[cfg(windows)]
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn local_secs_since_midnight() -> u64 {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    st.wHour as u64 * 3600 + st.wMinute as u64 * 60 + st.wSecond as u64
}
#[cfg(not(windows))]
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn local_secs_since_midnight() -> u64 {
    // `localtime_r` applies the real local offset (incl. DST); the old `epoch % 86400`
    // was UTC, which put every reminder due-time/label off by the timezone offset.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    secs_from_hms(tm.tm_hour as u64, tm.tm_min as u64, tm.tm_sec as u64)
}

/// Pure H:M:S → seconds-since-midnight (the unix local-clock math, testable with
/// injected values).
#[cfg_attr(windows, allow(dead_code))]
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn secs_from_hms(h: u64, m: u64, s: u64) -> u64 {
    h * 3600 + m * 60 + s
}

#[cfg(test)]
mod goal_project_tests {
    use super::*;

    fn proj(path: &str) -> Project {
        Project {
            id: path.to_string(),
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            color: String::new(),
            last_opened_at: None,
        }
    }

    #[test]
    fn cwd_maps_to_its_project_not_the_recency_top() {
        // Recency-top is index 0 (`working`); the focused pane sits in `target` → pick `target`,
        // NOT 0. This is the wrong-orchestrator fix: the default must follow the pane, not order.
        let projects = [proj("/home/me/working"), proj("/home/me/target")];
        assert_eq!(goal_project_for_cwd(&projects, "/home/me/target"), Some(1));
        // A worktree/subdir cwd still resolves to the project root.
        assert_eq!(
            goal_project_for_cwd(&projects, "/home/me/target/.worktrees/x"),
            Some(1)
        );
    }

    #[test]
    fn longest_prefix_wins_and_outsiders_are_none() {
        // Nested roots: the most specific project wins.
        let projects = [proj("/home/me/repo"), proj("/home/me/repo/sub")];
        assert_eq!(
            goal_project_for_cwd(&projects, "/home/me/repo/sub/deep"),
            Some(1)
        );
        // A cwd outside every project (and a bare-prefix false match) → None (caller falls to 0).
        assert_eq!(goal_project_for_cwd(&projects, "/home/me/other"), None);
        assert_eq!(goal_project_for_cwd(&projects, "/home/me/repo-x"), None);
    }
}

#[cfg(test)]
mod local_clock_tests {
    #[test]
    fn hms_to_secs_since_midnight() {
        assert_eq!(super::secs_from_hms(0, 0, 0), 0);
        assert_eq!(super::secs_from_hms(9, 0, 0), 32_400);
        assert_eq!(super::secs_from_hms(14, 32, 5), 14 * 3600 + 32 * 60 + 5);
        assert_eq!(super::secs_from_hms(23, 59, 59), 86_399);
    }
}

/// Resolve a quick offset against the local clock: `(delay from now in ms, due label)`.
#[tracing::instrument(level = "debug", ret)]
fn reminder_due(offset: ReminderOffset) -> (u64, String) {
    due_for(local_secs_since_midnight(), offset)
}

/// Pure core of [`reminder_due`], parameterised on the local seconds-since-midnight so the
/// label arithmetic is testable. A due time that rolls past midnight labels as "tomorrow".
#[tracing::instrument(level = "debug", ret)]
fn due_for(since_mid: u64, offset: ReminderOffset) -> (u64, String) {
    const DAY: u64 = 86_400;
    let delay_secs = match offset {
        ReminderOffset::Min15 => 15 * 60,
        ReminderOffset::Hour1 => 3_600,
        ReminderOffset::Hour3 => 3 * 3_600,
        ReminderOffset::Tomorrow9 => (DAY - since_mid) + 9 * 3_600,
        ReminderOffset::Custom(minutes) => minutes as u64 * 60,
    };
    let due = since_mid + delay_secs;
    let (hh, mm) = ((due % DAY) / 3_600, (due % 3_600) / 60);
    let label = if due >= DAY {
        format!("tomorrow {hh:02}:{mm:02}")
    } else {
        format!("{hh:02}:{mm:02}")
    };
    (delay_secs * 1_000, label)
}

/// Format a Slint [`Color`] as `#rrggbb` (the workspace-file pane color format).
/// The line typed into a restored shell pane to resume a tool session, `\r` included:
/// `cd '<cwd>' && <prefix><bin> <args>`, or without the `cd` when the snapshot recorded no
/// cwd. `prefix` is an environment assignment such as `CLAUDE_CONFIG_DIR='…' ` (with its
/// trailing space) or empty. Shared by the Claude shell-pane arm and the generic tool arm
/// of `make_pane_from_spec`, so a change to the shape reaches both.
#[tracing::instrument(level = "debug", ret)]
pub(crate) fn resume_startup_line(
    cwd: Option<&str>,
    prefix: &str,
    bin: &str,
    args: &str,
) -> String {
    match cwd {
        Some(cwd) => format!("cd '{cwd}' && {prefix}{bin} {args}\r"),
        None => format!("{prefix}{bin} {args}\r"),
    }
}

/// Does this saved group describe the app-owned Hyperpane tab? The `system` flag is
/// authoritative; files written before that flag existed carry none, so a group whose panes
/// all sit in the Hyperpane directory counts too — nothing else opens there by default.
#[tracing::instrument(level = "debug", ret)]
fn group_is_hyperpane(g: &GroupSpec) -> bool {
    if g.system == Some(true) {
        return true;
    }
    let dir = paths::hyperpane_dir();
    !g.panes.is_empty()
        && g.panes
            .iter()
            .all(|p| p.cwd.as_deref().is_some_and(|c| Path::new(c) == dir))
}

#[tracing::instrument(level = "debug", ret)]
fn color_hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

/// Parse a workspace-file layout token (`"single"`/`"columns"`/… / `"main-stack"`, or a
/// `"grid-2x3"` shape) back to a [`Layout`], defaulting to `Auto` for an unknown/absent
/// token — a file written by a newer build must still open here, minus the layout it names.
#[tracing::instrument(level = "debug", ret)]
pub fn layout_from_name(name: &str) -> Layout {
    Layout::from_token(name).unwrap_or(Layout::Auto)
}

/// Encode an `arboard` clipboard image (RGBA8) to PNG bytes. Returns `None` if the encoder
/// rejects the buffer (e.g. a zero-sized image). Used when attaching a pasted image to a goal.
#[tracing::instrument(level = "debug", ret)]
fn image_rgba_to_png(img: &arboard::ImageData) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width as u32, img.height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&img.bytes).ok()?;
    }
    Some(out)
}

/// Parse a `#rrggbb` hex string (the project palette format) into a Slint [`Color`],
/// falling back to the default accent on a malformed value.
#[tracing::instrument(level = "debug", ret)]
pub fn parse_hex(s: &str) -> Color {
    let h = s.trim_start_matches('#');
    if h.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&h[0..2], 16),
            u8::from_str_radix(&h[2..4], 16),
            u8::from_str_radix(&h[4..6], 16),
        ) {
            return Color::from_rgb_u8(r, g, b);
        }
    }
    theme::accent_for(0, 0)
}

/// Whether a pane `cwd` belongs to the project rooted at `project_path` — the SAME matcher
/// the cwd tint uses ([`State::note_pane_cwd`]): walk up to the enclosing git root, then
/// compare by the projects-store dedup key (canonical path, case-insensitive on Windows).
#[tracing::instrument(level = "debug", ret)]
fn cwd_in_project(cwd: &str, project_path: &str) -> bool {
    sidebar::git_root_of(cwd).is_some_and(|root| {
        projects::path_key(&root.to_string_lossy()) == projects::path_key(project_path)
    })
}

/// Whether a matching pane's accent still follows the project tint, so a project recolor
/// may retint it. Both the cwd tint and an explicit per-pane recolor pin the accent
/// ([`PaneState::pinned_accent`]); the two are told apart by VALUE: a pin equal to the
/// project's previous color is the tint, anything else is a user choice and is kept
/// (the same precedence the cwd tint itself applies — it re-pins on every cwd report,
/// while a user pin only exists until the next report).
#[tracing::instrument(level = "debug", ret)]
fn follows_project_tint(pinned: Option<Color>, old_project_color: Color) -> bool {
    pinned.is_none_or(|c| c == old_project_color)
}

#[cfg(test)]
mod project_recolor_tests {
    //! Track A (#24): propagating a project recolor to already-open panes. Pins the two
    //! pure pieces of `set_project_color`'s pane sweep — the cwd→project matcher (shared
    //! with the cwd tint) and the pin-respecting retint predicate.
    use super::*;

    /// A unique throwaway dir that LOOKS like a git repo (`<tmp>/<name>/.git/`), so
    /// `git_root_of`'s `.git`-exists walk resolves it. Cleaned up by the OS temp policy.
    fn fake_repo(name: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("hp_recolor_test_{}_{}", std::process::id(), name));
        std::fs::create_dir_all(root.join(".git")).expect("create fake repo");
        root
    }

    #[test]
    fn matches_a_cwd_inside_the_project_repo() {
        let repo = fake_repo("inside");
        let sub = repo.join("src").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(cwd_in_project(
            &sub.to_string_lossy(),
            &repo.to_string_lossy()
        ));
        // The root itself counts too.
        assert!(cwd_in_project(
            &repo.to_string_lossy(),
            &repo.to_string_lossy()
        ));
    }

    #[test]
    fn rejects_a_cwd_outside_the_repo_or_in_a_sibling_repo() {
        let repo = fake_repo("mine");
        let other = fake_repo("other");
        assert!(!cwd_in_project(
            &other.to_string_lossy(),
            &repo.to_string_lossy()
        ));
        // A non-repo cwd never matches (git_root_of walks up to nothing relevant).
        assert!(!cwd_in_project(
            &std::env::temp_dir().to_string_lossy(),
            &repo.to_string_lossy()
        ));
    }

    #[cfg(windows)]
    #[test]
    fn matching_is_case_insensitive_on_windows_like_the_store_key() {
        let repo = fake_repo("case");
        let upper = repo.to_string_lossy().to_uppercase();
        assert!(cwd_in_project(&upper, &repo.to_string_lossy()));
    }

    #[test]
    fn unpinned_and_tint_pinned_panes_follow_a_project_recolor_but_user_pins_win() {
        let old = Color::from_rgb_u8(0xe5, 0x48, 0x4d);
        let user = Color::from_rgb_u8(0x12, 0x34, 0x56);
        // Pinned by the project tint (== old project color) → retint.
        assert!(follows_project_tint(Some(old), old));
        // Never pinned (e.g. a re-hosted pane) → safe to retint.
        assert!(follows_project_tint(None, old));
        // Explicit per-pane recolor to a custom color → kept.
        assert!(!follows_project_tint(Some(user), old));
    }
}

#[cfg(test)]
mod ctx_menu_borrow_tests {
    //! Regression for the issue-#18 crash: changing the layout via the hamburger Layout
    //! submenu panicked with `RefCell already borrowed`.
    //!
    //! The `on_ctx_layout` callback (and its five `ctx_target()` siblings) did
    //! `if let Some(t) = win.state.borrow().ctx_target() { run_command(...) }`, and in Rust
    //! edition 2021 the `state.borrow()` temporary lives for the *whole* `if let` arm. Inside
    //! the arm, `run_command` takes `state.borrow_mut()` — a second, mutable borrow of the same
    //! `RefCell` → panic. The fix binds `ctx_target()` to a local so the shared borrow is
    //! released before the command runs. These tests reproduce that exact borrow ordering
    //! against a `RefCell<State>` (as `Window::state` is), so the regression can't return.
    use super::*;
    use std::cell::RefCell;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    /// The fixed shape: read the target out of the shared borrow first, then mutate. The
    /// hamburger Layout submenu opens via `open_app_context` (target = active tab) and routes
    /// `ctx_target` → `set_tab_layout`. This must not panic at any layout.
    #[test]
    fn layout_submenu_pick_does_not_double_borrow() {
        let cell = RefCell::new(fresh());
        cell.borrow_mut().open_app_context(0.0, 0.0);

        for layout in [
            Layout::Single,
            Layout::Columns,
            Layout::Rows,
            Layout::Grid,
            Layout::MainStack,
            Layout::Auto,
        ] {
            // Mirror `on_ctx_layout`'s FIXED body: bind the target out of the borrow, THEN mutate.
            let target = cell.borrow().ctx_target();
            if let Some(t) = target {
                cell.borrow_mut().set_tab_layout(t, layout);
            }
            assert_eq!(cell.borrow().active_tab().layout, layout);
        }
    }

    /// Pin the root cause: the OLD callback shape (mutating while the `ctx_target()` borrow is
    /// still held across the `if let` arm) double-borrows and panics. If a future refactor lets
    /// the shared borrow escape again, this catches it.
    #[test]
    fn holding_the_ctx_borrow_across_a_mutation_panics() {
        let cell = RefCell::new(fresh());
        cell.borrow_mut().open_app_context(0.0, 0.0);

        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // The buggy pattern: `state.borrow()` (via ctx_target) outlives the arm in 2021,
            // so the inner `borrow_mut()` is a re-entrant borrow → panic.
            if let Some(t) = cell.borrow().ctx_target() {
                cell.borrow_mut().set_tab_layout(t, Layout::Grid);
            }
        }))
        .is_err();
        assert!(crashed, "the held-borrow pattern must still double-borrow");
    }

    /// Task 7 (right-click chaining): opening a second context menu *while one is open* must
    /// REPLACE it — `State::ctx` is a single slot, never a stack — so the net state is exactly
    /// the new menu (kind + anchor swapped, old target gone) in one transition. This pins the
    /// state half of `App::reopen_context_at_cursor`, and mirrors its borrow-safe shape: every
    /// read is bound to a local and the shared borrow is dropped before the reopen mutates.
    #[test]
    fn right_click_chain_replaces_the_open_menu() {
        use crate::contextmenu::CtxKind;
        let cell = RefCell::new(fresh());

        // Menu A: the tab menu for tab 0 at one anchor.
        cell.borrow_mut().open_tab_context(0, 10.0, 20.0);
        {
            let st = cell.borrow();
            let m = st.ctx.as_ref().expect("menu A should be open");
            assert_eq!(m.kind, CtxKind::Tab);
            assert_eq!(m.target, 0);
            assert_eq!((m.x, m.y), (10.0, 20.0));
        }

        // The chain: a reopen reads via a local-bound borrow that is released before mutating
        // (the #18 rule), then opens a *different* surface (the app menu) at a *new* anchor.
        let was_open = cell.borrow().ctx_open();
        assert!(was_open, "a menu must be open for a chain to replace it");
        cell.borrow_mut().open_app_context(99.0, 88.0);

        // Net state is exactly menu B — A was replaced, not stacked.
        {
            let st = cell.borrow();
            let m = st.ctx.as_ref().expect("menu B should be open");
            assert_eq!(m.kind, CtxKind::App);
            assert_eq!((m.x, m.y), (99.0, 88.0));
        }
        assert!(cell.borrow().ctx_open());

        // A right-click with no chain target (or a left-click away) is a plain dismiss.
        cell.borrow_mut().close_context();
        assert!(!cell.borrow().ctx_open());
        assert!(cell.borrow().ctx_target().is_none());
    }
}

#[cfg(test)]
mod spawn_cells_tests {
    //! Option C of the ConPTY scroll-region investigation (docs/conpty-passthrough-
    //! investigation.md): conhost's repaint cost is proportional to pty rows, so a NEW
    //! pane spawns at the best-known visible cell size — the focused sibling's laid-out
    //! grid — and only falls back to 80×24 when no layout exists yet (eager first-pane
    //! seed, workspace restore into a fresh window).
    use super::*;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    fn det(uid: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: "t".into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    /// No panes at all (the eager first-pane seed): fall back to 80×24.
    #[test]
    fn empty_tab_falls_back_to_default() {
        let st = fresh();
        assert_eq!(st.spawn_cells(), (80, 24));
    }

    /// A pane exists but was never laid out (rect still zero — e.g. mid workspace
    /// restore): its `applied` is the spawn default, not a real size — keep the fallback.
    #[test]
    fn unlaid_out_pane_keeps_the_fallback() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        assert_eq!(st.active_tab().panes[0].rect.2, 0.0);
        assert_eq!(st.spawn_cells(), (80, 24));
    }

    /// Once the pump has laid the focused pane out, a sibling spawn inherits its grid —
    /// the pty starts at (about) the cells it will actually show instead of 80×24,
    /// skipping a ResizePseudoConsole (a full-grid re-render on the in-box conhost).
    #[test]
    fn laid_out_focused_pane_sizes_the_spawn() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        {
            let p = &mut st.active_tab_mut().panes[0];
            p.rect = (0.0, 0.0, 800.0, 600.0); // pump-placed
            p.applied = (132, 41); // pump-applied cells
        }
        assert_eq!(st.spawn_cells(), (132, 41));
    }

    /// Degenerate applied sizes are clamped to pty-sane bounds.
    #[test]
    fn applied_size_is_clamped() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        {
            let p = &mut st.active_tab_mut().panes[0];
            p.rect = (0.0, 0.0, 8.0, 6.0);
            p.applied = (0, 0);
        }
        assert_eq!(st.spawn_cells(), (2, 1));
    }
}

#[cfg(test)]
mod session_file_tests {
    //! The relaunch-restore snapshot (#14): `to_session_file` must record every tab with
    //! its layout/split/focus state and carry per-pane zoom exactly like the workspace
    //! path (`font_size` only when off the base size).
    use super::*;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str, font_px: f32) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: uid.into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    #[test]
    fn zoomed_pane_records_font_size_base_pane_omits_it() {
        let mut st = fresh();
        let m = mgr();
        assert_eq!(st.settings.font_px, prefs::DEFAULT_FONT_PX);
        st.adopt_pane(&m, det("zoomed", 20.0));
        st.adopt_pane(&m, det("base", prefs::DEFAULT_FONT_PX));
        let file = st.to_session_file();
        let groups = file.groups.expect("one group per tab");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].panes[0].font_size,
            Some(20),
            "zoomed pane carries its px"
        );
        assert_eq!(
            groups[0].panes[1].font_size, None,
            "base pane omits font_size"
        );
    }

    #[test]
    fn snapshot_records_split_state_focus_cwd_and_active_tab() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a", 14.0));
        st.adopt_pane(&m, det("b", 14.0));
        {
            let t = st.active_tab_mut();
            t.focused = 1;
            t.zoomed = Some(1);
            t.panes[0].cwd = Some("C:/work".into());
            // Pane 0 was launched running a program; pane 1 is a plain shell.
            t.panes[0].spawn_command = Some("claude".into());
            t.panes[0].spawn_shell = Some("pwsh".into());
        }
        let file = st.to_session_file();
        assert_eq!(file.active, Some(0));
        let g = &file.groups.unwrap()[0];
        assert_eq!(g.focused, Some(1));
        assert_eq!(g.zoomed, Some(1));
        assert_eq!(g.panes[0].cwd.as_deref(), Some("C:/work"));
        assert_eq!(g.panes[1].cwd, None);
        assert_eq!(g.sizes.as_ref().map(|s| s.len()), Some(2));
        assert!(g.layout.is_some());
        // An unpinned (slot-coloured) pane restores clean — no color recorded.
        assert_eq!(g.panes[0].color, None);
        // The session snapshot records each pane's live uid + its spawn command/shell, so a
        // relaunch can re-attach by uid (session-daemon M2) or re-run the original program.
        assert_eq!(g.panes[0].uid.as_deref(), Some("a"));
        assert_eq!(g.panes[1].uid.as_deref(), Some("b"));
        assert_eq!(g.panes[0].command.as_deref(), Some("claude"));
        assert_eq!(g.panes[0].shell.as_deref(), Some("pwsh"));
        // A plain-shell pane records no command (omitted) but still records its uid.
        assert_eq!(g.panes[1].command, None);
    }

    /// Session-daemon prep round-trip: a pane LOADED from a spec carrying a `command` keeps
    /// that command on its [`PaneState`] (`make_pane_from_spec`), so the next `to_session_file`
    /// re-records it (the crash-recovery gap fix) AND stamps the pane's live uid. Re-loading the
    /// snapshot preserves the command — restore re-runs the original program rather than a plain
    /// shell. (The live uid is freshly minted per launch, so it's recorded, not round-tripped.)
    #[test]
    fn session_snapshot_round_trips_uid_and_command() {
        // `load_workspace` fires an async pty spawn that grabs the current Tokio handle, so
        // hold a runtime context for the duration of this test (the app always runs inside one).
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let mut st = fresh();
        let m = mgr();
        // Seed a pane from a command-bearing spec (the restore path stores the spawn spec).
        let seed = WorkspaceFile {
            groups: Some(vec![GroupSpec {
                panes: vec![PaneSpec {
                    command: Some("claude".into()),
                    args: Some(vec!["--model".into(), "opus".into()]),
                    shell: Some("pwsh".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            active: Some(0),
            ..Default::default()
        };
        st.load_workspace(seed, &m);
        // The loaded pane remembers what it was spawned with.
        let live = &st.active_tab().panes[0];
        assert_eq!(live.spawn_command.as_deref(), Some("claude"));
        assert_eq!(
            live.spawn_args.as_deref(),
            Some(&["--model".into(), "opus".into()][..])
        );
        assert_eq!(live.spawn_shell.as_deref(), Some("pwsh"));
        let live_uid = live.uid.clone();

        // Snapshot: the live uid + the spawn command/args/shell are recorded.
        let snap = st.to_session_file();
        let pane = &snap.groups.as_ref().unwrap()[0].panes[0];
        assert_eq!(
            pane.uid.as_deref(),
            Some(live_uid.as_str()),
            "live uid recorded"
        );
        assert_eq!(pane.command.as_deref(), Some("claude"));
        assert_eq!(
            pane.args.as_deref(),
            Some(&["--model".into(), "opus".into()][..])
        );
        assert_eq!(pane.shell.as_deref(), Some("pwsh"));

        // Re-load the snapshot into a fresh window: the command survives, so restore re-runs
        // the original program (uid is freshly minted, but the program is preserved).
        let mut st2 = fresh();
        st2.load_workspace(snap, &mgr());
        let restored = &st2.active_tab().panes[0];
        assert_eq!(restored.spawn_command.as_deref(), Some("claude"));
        assert_eq!(restored.spawn_shell.as_deref(), Some("pwsh"));
    }

    /// M2 restore on the IN-PROCESS backend always RE-SPAWNS (re-attach needs a daemon — the
    /// PTYs die with the GUI here). Even though the snapshot carries a uid, `make_pane_from_spec`
    /// must NOT try to re-attach it: `mgr.is_daemon()` is false → it freshly mints a uid and
    /// re-spawns from the spec. So the restored pane's uid differs from the recorded one, and
    /// the pane starts unstarted (a fresh shell coming up, not a replay-primed survivor).
    #[test]
    fn in_process_restore_respawns_with_a_fresh_uid_not_the_recorded_one() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let mut st = fresh();
        let m = mgr(); // in-process backend
        assert!(
            !m.is_daemon(),
            "this test exercises the in-process (re-spawn) path"
        );
        let snap = WorkspaceFile {
            groups: Some(vec![GroupSpec {
                panes: vec![PaneSpec {
                    // A recorded uid from a "previous run" — on the in-process backend it can't
                    // be alive (the prior GUI's PTYs are gone), so it must be ignored for restore.
                    uid: Some("pane-from-last-run".into()),
                    command: Some("htop".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            active: Some(0),
            ..Default::default()
        };
        st.load_workspace(snap, &m);
        let restored = &st.active_tab().panes[0];
        assert_ne!(
            restored.uid, "pane-from-last-run",
            "in-process restore mints a fresh uid (no re-attach), got {}",
            restored.uid
        );
        assert!(
            !restored.started,
            "a re-spawned pane starts unstarted (fresh shell coming up)"
        );
        // The original program is still re-run from the spec (Prep's re-spawn-the-program fix).
        assert_eq!(restored.spawn_command.as_deref(), Some("htop"));
    }

    /// The emptied-last-tab-mid-close case: closing the only pane of the only tab leaves
    /// a 0-pane tab in place (the window is about to close) — and that's exactly when the
    /// last-session snapshot is taken. It must not persist the empty group, or the next
    /// restore can land on a tab with 0 panes.
    #[test]
    fn an_emptied_last_tab_is_not_persisted() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("only", 14.0));
        assert!(
            !st.close_pane_in(st.active, 0, &m),
            "workspace emptied → caller quits"
        );
        assert!(
            st.active_tab().panes.is_empty(),
            "empty tab left while closing"
        );
        let file = st.to_session_file();
        assert_eq!(file.groups.as_deref().map(|g| g.len()), Some(0));
        assert_eq!(file.active, Some(0));
    }

    /// A 0-pane tab anywhere in the list is skipped and `active` is remapped to the
    /// filtered indexing (not blindly recorded), so the restored session activates the
    /// same CONTENT, not the same raw index.
    #[test]
    fn snapshot_skips_empty_tabs_and_remaps_active() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a", 14.0)); // tab 0
                                           // Manufacture an empty tab before it (defensive: no normal flow keeps one).
        st.tabs.insert(0, Tab::empty("ghost".into()));
        st.active = 1; // the real tab
        let file = st.to_session_file();
        let groups = file.groups.unwrap();
        assert_eq!(groups.len(), 1, "empty tab dropped");
        assert_eq!(groups[0].panes.len(), 1);
        assert_eq!(
            file.active,
            Some(0),
            "active remapped to the filtered index"
        );
        // The surviving pane still carries its uid through the empty-tab filtering, so a
        // session-daemon relaunch can match it.
        assert_eq!(groups[0].panes[0].uid.as_deref(), Some("a"));
    }

    /// A workspace-seeded window starts from a fresh `State` whose `State::new` placeholder
    /// tab never receives a pane — once the file's content lands, that 0-pane ghost tab
    /// must be dropped (with `active` following the content tab).
    #[test]
    fn purge_drops_the_empty_seed_tab_and_keeps_active_on_content() {
        let mut st = fresh();
        let m = mgr();
        // fresh() = State::new → tabs[0] is the pristine empty placeholder.
        assert!(st.tabs[0].panes.is_empty());
        // Simulate the appended workspace tab (tab 1, with a pane) + active landing on it.
        st.tabs.push(Tab::empty("restored".into()));
        st.active = 1;
        st.adopt_pane(&m, det("content", 14.0)); // adopts into the active tab
        assert_eq!(st.tabs[1].panes.len(), 1);
        st.purge_empty_tabs();
        assert_eq!(st.tabs.len(), 1, "placeholder dropped");
        assert_eq!(st.active, 0, "active follows the content tab");
        assert_eq!(st.tabs[0].title.as_str(), "restored");
    }

    /// Loading a workspace whose groups are all contentless appends nothing and leaves
    /// the current tabs/active untouched (the seed path then falls back to a fresh pane).
    #[test]
    fn loading_an_all_empty_workspace_is_a_noop() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("keep", 14.0));
        let file = WorkspaceFile {
            groups: Some(vec![GroupSpec {
                panes: vec![],
                ..Default::default()
            }]),
            active: Some(0),
            ..Default::default()
        };
        st.load_workspace(file, &m);
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.active, 0);
        assert_eq!(st.active_tab().panes.len(), 1);
    }

    /// The `host.panes.spawn { kind: "terminal" }` landing: a module's launch spec reaches
    /// `apply_pane_op`, which submits exactly this `NewPaneOpts` (command + argv + cwd + env).
    /// `make_pane` must carry the argv onto the live pane as `spawn_args` — not drop it — so the
    /// program the module named runs with its arguments AND a relaunch re-execs the same argv.
    /// A pty pane learns its live cwd from OSC-7, so `cwd` rides the pty spawn but is not seeded
    /// onto the pane; `env`, by contrast, is kept verbatim on the pane.
    #[test]
    fn a_module_terminal_pane_keeps_its_argv_and_env() {
        // `add_pane_opts` fires the async pty spawn on the current Tokio handle (as in the
        // running app), so hold a runtime for the call.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let mut st = fresh();
        let m = mgr();
        let uid = st
            .add_pane_opts(
                &m,
                NewPaneOpts {
                    label: Some("git".into()),
                    command: Some("git".into()),
                    args: Some(vec!["status".into(), "--porcelain".into()]),
                    cwd: Some("/work/repo".into()),
                    env: Some(
                        [("GIT_PAGER".to_string(), "cat".to_string())]
                            .into_iter()
                            .collect(),
                    ),
                    ..Default::default()
                },
            )
            .expect("a command pane spawns");
        let pane = st
            .active_tab()
            .panes
            .iter()
            .find(|p| p.uid == uid)
            .expect("the spawned pane is in the active tab");
        assert_eq!(
            pane.spawn_command.as_deref(),
            Some("git"),
            "the module's program is what the pane runs"
        );
        assert_eq!(
            pane.spawn_args.as_deref(),
            Some(&["status".into(), "--porcelain".into()][..]),
            "the module's argv rides onto the pane so the program runs with its args and a \
             relaunch re-execs it — not the plain-shell None a New Pane dialog leaves"
        );
        assert_eq!(
            pane.env.as_ref().and_then(|e| e.get("GIT_PAGER")).map(String::as_str),
            Some("cat"),
            "the module's env is kept on the pane verbatim"
        );
        assert!(
            pane.cwd.is_none(),
            "a pty pane is seeded with no cwd — it learns its live one from the shell's OSC-7; \
             the requested cwd rode the pty spawn instead"
        );
    }
}

#[cfg(test)]
mod shell_label_tests {
    //! The basename→label mapping behind the pane-header shell-type badge (Task 12). Derived
    //! app-side from the resolved spawn program, so no `core` change is needed.
    use super::shell_label;

    #[test]
    fn maps_the_common_windows_shells() {
        assert_eq!(shell_label("pwsh.exe"), "pwsh");
        assert_eq!(shell_label("powershell.exe"), "powershell");
        assert_eq!(shell_label("cmd.exe"), "cmd");
        assert_eq!(shell_label("wsl.exe"), "wsl");
    }

    #[test]
    fn maps_posix_shells() {
        assert_eq!(shell_label("/bin/bash"), "bash");
        assert_eq!(shell_label("/usr/bin/zsh"), "zsh");
        assert_eq!(shell_label("bash"), "bash");
        assert_eq!(shell_label("fish"), "fish");
        assert_eq!(shell_label("/bin/sh"), "sh");
        assert_eq!(shell_label("nu"), "nu");
    }

    #[test]
    fn strips_a_full_path_to_the_basename() {
        // COMSPEC is a full path; the badge uses the basename only.
        assert_eq!(shell_label("C:\\Windows\\system32\\cmd.exe"), "cmd");
        assert_eq!(
            shell_label("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
            "pwsh"
        );
        assert_eq!(shell_label("C:\\Program Files\\Git\\bin\\bash.exe"), "bash");
    }

    #[test]
    fn is_case_insensitive_on_program_and_extension() {
        assert_eq!(shell_label("PWSH.EXE"), "pwsh");
        assert_eq!(shell_label("Cmd.Exe"), "cmd");
        assert_eq!(shell_label("BASH"), "bash");
    }

    #[test]
    fn falls_back_to_the_bare_basename_for_unknown_programs() {
        // Unrecognised program → its basename with a trailing .exe stripped (case preserved).
        assert_eq!(shell_label("C:\\tools\\MyShell.exe"), "MyShell");
        assert_eq!(shell_label("/opt/elvish/elvish"), "elvish");
        assert_eq!(shell_label("xonsh"), "xonsh");
    }

    #[test]
    fn empty_or_whitespace_program_yields_empty() {
        assert_eq!(shell_label(""), "");
        assert_eq!(shell_label("   "), "");
        // a path ending in a separator has no basename.
        assert_eq!(shell_label("C:\\bin\\"), "");
    }
}

#[cfg(test)]
mod reminder_tests {
    //! Track F: the reminder-pane state machine — park (detach, session alive) → fire
    //! (tick marks due entries) → restore (re-dock into the active tab, entry cleared) —
    //! plus the local-clock due-label arithmetic.
    use super::*;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    fn det(uid: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: uid.into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    #[test]
    fn park_fire_restore_roundtrip() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        st.adopt_pane(&m, det("b"));
        assert_eq!(st.active_tab().panes.len(), 2);

        st.remind_pane(0, ReminderOffset::Min15);
        assert_eq!(st.active_tab().panes.len(), 1);
        assert_eq!(st.reminders.len(), 1);
        assert!(!st.reminders[0].fired);
        // The parked session must still be killed when the window closes.
        assert!(st.session_uids().iter().any(|u| u == "a"));

        // Not due yet → no change; due → fired (the bell highlight), pane stays parked.
        let due = st.reminders[0].due_ms;
        assert!(!st.tick_reminders(due - 1));
        assert!(st.tick_reminders(due));
        assert!(st.reminders[0].fired);
        assert_eq!(st.active_tab().panes.len(), 1, "v1 never auto-restores");

        // Restore re-docks into the active tab (focused) and clears the entry.
        st.restore_reminder("a", &m);
        assert!(st.reminders.is_empty());
        assert_eq!(st.active_tab().panes.len(), 2);
        let f = st.active_tab().focused;
        assert_eq!(st.active_tab().panes[f].uid, "a");
    }

    #[test]
    fn toast_shows_on_fire_ages_out_and_dismisses() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        st.adopt_pane(&m, det("b"));
        st.remind_pane(0, ReminderOffset::Min15);

        // Fire → the toast becomes visible (fired && !dismissed) with the fire time stamped.
        let due = st.reminders[0].due_ms;
        assert!(st.tick_reminders(due));
        assert!(st.reminders[0].fired && !st.reminders[0].toast_dismissed);
        assert_eq!(st.reminders[0].fired_at_ms, due);

        // Inside the window the toast stays; at the boundary it ages out (a state change,
        // so the pump re-pushes), while `fired` — the bell badge — is untouched.
        assert!(!st.tick_reminders(due + REMINDER_TOAST_MS - 1));
        assert!(!st.reminders[0].toast_dismissed);
        assert!(st.tick_reminders(due + REMINDER_TOAST_MS));
        assert!(st.reminders[0].toast_dismissed);
        assert!(st.reminders[0].fired);
    }

    #[test]
    fn toast_dismiss_keeps_reminder_and_is_idempotent() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        st.adopt_pane(&m, det("b"));
        st.remind_pane(0, ReminderOffset::Min15);
        let due = st.reminders[0].due_ms;
        st.tick_reminders(due);

        st.dirty = false;
        st.dismiss_reminder_toast("a");
        assert!(st.reminders[0].toast_dismissed);
        assert!(st.dirty);
        // The reminder (and the bell badge) survive a toast dismiss; restoring still works.
        assert!(st.reminders[0].fired);
        st.dirty = false;
        st.dismiss_reminder_toast("a"); // already dismissed → no work
        assert!(!st.dirty);
        st.dismiss_reminder_toast("nope"); // unknown uid → noop
        st.restore_reminder("a", &m);
        assert!(st.reminders.is_empty());
    }

    #[test]
    fn last_pane_of_last_tab_cannot_be_parked() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("only"));
        st.remind_pane(0, ReminderOffset::Hour1);
        assert_eq!(st.active_tab().panes.len(), 1);
        assert!(st.reminders.is_empty());
    }

    #[test]
    fn parked_pane_exit_drops_its_reminder() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        st.adopt_pane(&m, det("b"));
        st.remind_pane(0, ReminderOffset::Hour3);
        assert!(st.hosts_session("a"));
        // The parked shell exits on its own → the reminder dies with the session.
        assert!(st.pane_exited("a", &m, 0).alive);
        assert!(st.reminders.is_empty());
        assert!(!st.hosts_session("a"));
    }

    #[test]
    fn restore_by_unknown_uid_is_a_noop() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("a"));
        st.restore_reminder("nope", &m);
        assert_eq!(st.active_tab().panes.len(), 1);
    }

    #[test]
    fn due_labels_roll_over_midnight() {
        // noon + 15m → same day.
        assert_eq!(
            due_for(12 * 3600, ReminderOffset::Min15),
            (900_000, "12:15".into())
        );
        // 23:50 + 15m → tomorrow 00:05.
        let (d, l) = due_for(23 * 3600 + 50 * 60, ReminderOffset::Min15);
        assert_eq!((d, l.as_str()), (900_000, "tomorrow 00:05"));
        // tomorrow 9am from 18:00 → 15h delay, labelled tomorrow.
        let (d, l) = due_for(18 * 3600, ReminderOffset::Tomorrow9);
        assert_eq!((d, l.as_str()), (15 * 3_600_000, "tomorrow 09:00"));
        // a Custom 90 min from 23:00 rolls over midnight too.
        let (d, l) = due_for(23 * 3600, ReminderOffset::Custom(90));
        assert_eq!((d, l.as_str()), (90 * 60_000, "tomorrow 00:30"));
    }
}

#[cfg(test)]
mod view_pane_tests {
    //! D3 — a non-pty view pane (file browser / viewer / markdown) is pane identity WITHOUT
    //! session identity. `PaneState.uid` is doing four jobs at once — the `SessionManager`
    //! registry key, `AVADA_PANE_ID`, the Claude hook's marker filename, and the
    //! `PaneSpec` re-attach key — so handing a view pane a backend uid would put a phantom
    //! in front of `pane_load`, `has`, and the cross-window `claim_session` arbitration:
    //! a uid the daemon is asked about forever and can never answer for.
    //!
    //! These tests pin the two halves of the fix: the uid SCHEME (`view-N`, which cannot
    //! alias `pane-N` or `pane-<uuid>`) and the GATE (`kind.is_pty()` in front of every
    //! backend call). The second is what actually matters; the first is what makes it
    //! checkable.
    use super::*;

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn view_opts(kind: PaneKind) -> NewPaneOpts {
        NewPaneOpts {
            kind: Some(kind),
            ..Default::default()
        }
    }

    #[test]
    fn a_view_pane_mints_a_view_uid_not_a_session_uid() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st
            .add_pane_opts(&m, view_opts(PaneKind::FileBrowser))
            .expect("view pane added");
        assert!(
            uid.starts_with("view-"),
            "a pane with no pty must not carry a backend uid, got {uid}"
        );
    }

    // Spawns a real pty (that is the point — the contrast case), so it needs a runtime.
    #[tokio::test]
    async fn a_terminal_pane_still_mints_a_session_uid() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st
            .add_pane_opts(&m, NewPaneOpts::default())
            .expect("terminal pane added");
        assert!(
            uid.starts_with("pane-"),
            "a pty-backed pane keeps the backend scheme, got {uid}"
        );
    }

    #[test]
    fn view_uids_are_unique_within_a_window() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let a = st.add_pane_opts(&m, view_opts(PaneKind::Markdown)).unwrap();
        let b = st.add_pane_opts(&m, view_opts(PaneKind::Markdown)).unwrap();
        assert_ne!(a, b);
    }

    /// A view pane has no text grid to select into (D3), so Select All and Copy have to
    /// reach the projection instead. This pins the ROUTING: the controller must send a
    /// non-pty pane's selection to `viewpane`, not to the terminal widget that is not even
    /// drawn there.
    #[test]
    fn select_all_on_a_view_pane_selects_its_projected_rows() {
        let dir = std::env::temp_dir().join("hp-state-viewsel");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("notes.txt");
        std::fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();

        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st
            .add_pane_opts(
                &m,
                NewPaneOpts {
                    kind: Some(PaneKind::FileViewer),
                    cwd: Some(f.display().to_string()),
                    ..Default::default()
                },
            )
            .expect("view pane added");
        // The projection is normally built by the render pass; a headless test builds it
        // itself so there are rows for a selection to name.
        crate::viewpane::model_for(
            &uid,
            &PaneKind::FileViewer,
            Some(&f.display().to_string()),
            0,
        );

        st.select_all_pane(0);
        assert_eq!(crate::viewpane::selected_range(&uid), Some((0, 2)));
        assert_eq!(
            crate::viewpane::copy_range(&uid, 0, 2),
            "alpha\nbeta\ngamma"
        );

        // A click narrows it to one row; a shift-click stretches back from that anchor.
        st.view_select(0, 2, false);
        assert_eq!(crate::viewpane::selected_range(&uid), Some((2, 2)));
        st.view_select(0, 1, true);
        assert_eq!(crate::viewpane::selected_range(&uid), Some((1, 2)));

        crate::viewpane::forget(&uid);
    }

    /// D3's other half: a view pane's `cwd` is not bookkeeping, it is the CONTENT — the file
    /// `viewpane::rows_for` renders. Dropping it on a re-host is what turned a README preview
    /// dragged onto a new tab into "No path set for this pane".
    #[test]
    fn a_view_pane_dropped_on_a_new_tab_still_renders_its_file() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st
            .add_pane_opts(
                &m,
                NewPaneOpts {
                    kind: Some(PaneKind::Markdown),
                    cwd: Some("/repo/README.md".into()),
                    ..Default::default()
                },
            )
            .expect("markdown pane added");

        // Exactly the drop-on-the-empty-tab-strip path: detach the dragged uid, re-host as a tab.
        let (det, _alive) = st.detach_uid(&uid).expect("the dragged pane detaches");
        assert_eq!(det.cwd.as_deref(), Some("/repo/README.md"));
        st.adopt_pane_as_tab(&m, det);

        let p = st
            .active_tab()
            .panes
            .iter()
            .find(|p| p.uid == uid)
            .expect("re-hosted into the new tab");
        assert_eq!(p.cwd.as_deref(), Some("/repo/README.md"));
        assert!(
            !crate::viewpane::rows_for(&p.kind, p.cwd.as_deref(), 0).is_empty(),
            "a target-less view pane projects only the 'No path set' notice"
        );
    }

    #[test]
    fn a_view_pane_records_no_program_to_relaunch() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // Even asked to run something, a pane declared a view runs nothing: the explicit
        // kind is the authority, and a relaunch must not resurrect a program the pane
        // never started.
        st.add_pane_opts(
            &m,
            NewPaneOpts {
                kind: Some(PaneKind::FileViewer),
                command: Some("claude".into()),
                ..Default::default()
            },
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.kind, PaneKind::FileViewer);
        assert_eq!(p.spawn_command, None, "a view pane relaunches no program");
        assert_eq!(p.spawn_shell, None);
    }

    #[test]
    fn a_view_pane_is_not_registered_with_the_backend() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st
            .add_pane_opts(&m, view_opts(PaneKind::FileBrowser))
            .unwrap();
        assert!(
            !m.has(&uid),
            "a view pane must leave no session behind it for the manager to answer for"
        );
    }

    #[test]
    fn restarting_a_view_pane_does_nothing() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st.add_pane_opts(&m, view_opts(PaneKind::Markdown)).unwrap();
        st.restart_pane(0, &m);
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.uid, uid, "a restart must not strand the pane's identity");
        assert_eq!(p.kind, PaneKind::Markdown, "nor change what the pane is");
        assert!(!m.has(&p.uid), "nor spawn a shell into a pane with no pty");
    }

    /// The pane a tool was launched into must survive the tool exiting: Ctrl-C out of
    /// `claude` and you get a shell where the pane was, not a closed tab.
    #[tokio::test]
    async fn a_tool_pane_falls_back_to_a_shell_instead_of_closing() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st.add_pane_opts(&m, NewPaneOpts::default()).unwrap();
        // Stand in for a pane the tool launcher opened, without actually running the tool.
        st.tabs[0].panes[0].kind = PaneKind::Tool("claude".into());
        st.tabs[0].panes[0].spawn_command = Some("claude".into());

        let out = st.pane_exited(&uid, &m, 130);

        assert!(
            out.alive,
            "the workspace must not be torn down by a tool exiting"
        );
        assert_eq!(
            st.active_tab().panes.len(),
            1,
            "the pane stays where it was"
        );
        let p = &st.active_tab().panes[0];
        assert_ne!(p.uid, uid, "the fallback shell is a new session");
        assert_eq!(
            p.kind,
            PaneKind::Terminal,
            "it is a plain terminal now, so re-running the tool can re-upgrade it"
        );
        assert_eq!(
            out.rebound,
            Some((uid, p.uid.clone())),
            "the uid swap has to be reported so uid-keyed state outside State can follow it"
        );
    }

    /// A plain shell keeps the old behaviour — there `exit` means "close this terminal".
    #[tokio::test]
    async fn a_plain_terminal_pane_still_closes_on_exit() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st.add_pane_opts(&m, NewPaneOpts::default()).unwrap();
        st.add_pane_opts(&m, NewPaneOpts::default()).unwrap();

        let out = st.pane_exited(&uid, &m, 0);

        assert!(out.alive);
        assert_eq!(out.rebound, None);
        assert_eq!(st.active_tab().panes.len(), 1, "the exited pane is gone");
    }

    /// `tab_seq` restarts at 0 every run while tab titles survive one, so the counter alone
    /// hands out a name the strip already shows. The generator has to skip what is taken.
    #[tokio::test]
    async fn a_new_tab_never_reuses_a_name_already_on_the_strip() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        st.tabs[0].title = "term 2".into();

        st.new_tab(&m);
        st.new_tab(&m);

        let titles: Vec<String> = st.tabs.iter().map(|t| t.title.to_string()).collect();
        let mut uniq = titles.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), titles.len(), "duplicate tab names: {titles:?}");
        assert!(
            !titles.iter().skip(1).any(|t| t == "term 2"),
            "the restored tab's name must not be handed out again: {titles:?}"
        );
    }

    #[test]
    fn a_restored_view_pane_asks_the_backend_nothing() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        st.attach_panes_from_specs(
            &m,
            &[PaneSpec {
                // The recorded uid is deliberately ignored: `view-N` is per-run, so honouring
                // a stale one could collide with the next pane this run mints.
                uid: Some("view-99".into()),
                meta: Some(
                    [(
                        avada_core::tools::kind::META_KIND_KEY.to_string(),
                        "view:markdown".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.kind, PaneKind::Markdown, "the recorded kind is restored");
        assert!(p.uid.starts_with("view-"));
        assert!(
            !m.has(&p.uid),
            "restore must not spawn a pty for a view pane"
        );
    }

    #[test]
    fn a_view_pane_keeps_the_cwd_it_was_opened_at() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // "Browse Files" passes the terminal's cwd as the new pane's cwd, and for a view
        // pane that value IS the thing being browsed. It used to be dropped on the floor —
        // the pane's header showed the directory's name while its body said "No path set
        // for this pane", because only the label was built from it.
        st.add_pane_opts(
            &m,
            NewPaneOpts {
                kind: Some(PaneKind::FileBrowser),
                cwd: Some("/tmp".to_string()),
                ..Default::default()
            },
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.cwd.as_deref(), Some("/tmp"));

        // A pty pane is the contrast: its cwd is a *spawn* argument, and the live value is
        // whatever the shell reports over OSC 7 — so it must still start out unknown rather
        // than claiming a directory the shell may already have left.
        assert!(!PaneKind::FileBrowser.is_pty());
    }

    #[test]
    fn a_restored_view_pane_comes_back_at_its_target() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // The snapshot records a view pane's cwd like any other pane's; restore has to read
        // it back, or every browser/viewer/markdown pane returns from a relaunch blank.
        st.attach_panes_from_specs(
            &m,
            &[PaneSpec {
                cwd: Some("/tmp".into()),
                meta: Some(
                    [(
                        avada_core::tools::kind::META_KIND_KEY.to_string(),
                        "view:files".to_string(),
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Default::default()
            }],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.kind, PaneKind::FileBrowser);
        assert_eq!(p.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn a_view_pane_carries_no_shell_badge() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // The header badge names the pane's shell program. A view pane never spawned one,
        // so the badge has to be empty — it read "notes.md  zsh" otherwise, claiming a
        // process that does not exist.
        st.add_pane_opts(
            &m,
            NewPaneOpts {
                kind: Some(PaneKind::Markdown),
                cwd: Some("/tmp".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(st.active_tab().panes.last().unwrap().shell_label, "");

        // The contrast is the helper itself: a resolved shell program still yields a
        // badge, so the suppression above is about the *kind*, not a broken label.
        assert_eq!(shell_label("/bin/zsh"), "zsh");
    }

    #[test]
    fn closing_a_view_pane_kills_no_session() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // Two views, so the close is not the last pane of the last tab — and so the whole
        // test stays pty-free, which is itself the assertion: none of this needs a runtime.
        st.add_pane_opts(&m, view_opts(PaneKind::Markdown));
        let view = st
            .add_pane_opts(&m, view_opts(PaneKind::FileBrowser))
            .unwrap();
        // The gate is `kill_session_of`; what it protects is the daemon being asked to kill a
        // uid it never issued.
        assert!(st.close_pane_in(0, 1, &m), "window survives the close");
        assert!(!st.active_tab().panes.iter().any(|p| p.uid == view));
    }
}

#[cfg(test)]
mod tool_session_tests {
    //! H1/H2 — what a pane has to remember so a relaunch puts it back where it was.
    //!
    //! For a tool pane that is the *conversation*: the id alone is not enough, because
    //! every provider resumes by id **within a project directory**, so the mark is the
    //! pair. For a view pane it is the *target*, which rides along as the pane's cwd.
    //!
    //! The round trip these pin is the real one — `to_session_file()` out,
    //! `attach_panes_from_specs()` back in — not a hand-built `PaneSpec`, because the
    //! bug class here is a serializer and a restore path that disagree about a key.
    use super::*;

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    /// The panes of the first (only) group of a snapshot.
    fn snapshot_panes(st: &State) -> Vec<PaneSpec> {
        st.to_session_file()
            .groups
            .expect("a snapshot has groups")
            .swap_remove(0)
            .panes
    }

    fn spec_with(meta: &[(&str, &str)]) -> PaneSpec {
        PaneSpec {
            meta: Some(
                meta.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    // Spawns a real pty — a `Tool` pane is a terminal pane, that is the whole D1 premise.
    #[tokio::test]
    async fn a_pane_opened_into_a_conversation_records_which_one() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        st.add_pane_opts(
            &m,
            NewPaneOpts {
                kind: Some(PaneKind::Tool("copilot".into())),
                cwd: Some("/tmp".to_string()),
                command: Some("/bin/cat".to_string()),
                session: Some("aaaa-bbbb-cccc".to_string()),
                ..Default::default()
            },
        );
        let meta = snapshot_panes(&st)
            .pop()
            .expect("the pane is in the snapshot")
            .meta
            .expect("a tool pane carries meta");
        // The kind is what says *which* tool to relaunch; the mark is what says which
        // chat. Both, or the relaunch is a fresh copilot in the right directory.
        assert_eq!(
            meta.get(avada_core::tools::kind::META_KIND_KEY)
                .map(String::as_str),
            Some("copilot")
        );
        assert_eq!(
            meta.get(avada_core::tools::META_SESSION_KEY)
                .map(String::as_str),
            Some("aaaa-bbbb-cccc")
        );
        assert_eq!(
            meta.get(avada_core::tools::META_SESSION_CWD_KEY)
                .map(String::as_str),
            Some("/tmp")
        );
    }

    // Restoring a tool pane re-spawns its pty, so this one needs a runtime.
    #[tokio::test]
    async fn a_restarted_tool_pane_comes_back_in_the_same_conversation() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // No `command`: the pane was an interactive shell the human ran the tool inside
        // of, which is the shape the left panel's "resume" produces. The resume then has
        // to be *typed*, since there is no argv to extend.
        st.attach_panes_from_specs(
            &m,
            &[spec_with(&[
                (avada_core::tools::kind::META_KIND_KEY, "copilot"),
                (avada_core::tools::META_SESSION_KEY, "aaaa-bbbb-cccc"),
                (avada_core::tools::META_SESSION_CWD_KEY, "/tmp"),
            ])],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(
            p.startup.as_deref(),
            Some("cd '/tmp' && copilot --resume aaaa-bbbb-cccc\r")
        );
        // And it still holds the mark, so the NEXT snapshot keeps it: a pane that
        // forgets after one restart is a pane that only survives one restart.
        assert_eq!(
            p.tool_session.as_ref().map(|m| m.id.as_str()),
            Some("aaaa-bbbb-cccc")
        );
    }

    // Restoring a tool pane re-spawns its pty, so this one needs a runtime.
    #[tokio::test]
    async fn a_command_that_already_names_the_conversation_is_not_resumed_twice() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // The pane was spawned as `copilot --resume <id>` in the first place. Appending
        // the flag again would hand copilot two `--resume` values.
        st.attach_panes_from_specs(
            &m,
            &[PaneSpec {
                command: Some("copilot --resume aaaa-bbbb-cccc".into()),
                ..spec_with(&[
                    (avada_core::tools::kind::META_KIND_KEY, "copilot"),
                    (avada_core::tools::META_SESSION_KEY, "aaaa-bbbb-cccc"),
                    (avada_core::tools::META_SESSION_CWD_KEY, "/tmp"),
                ])
            }],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.startup, None);
        // ...and it still comes back in the conversation's own directory. This is the pane
        // the `already` gate is written for, so a cwd decision made inside that gate never
        // runs for it: the pane relaunched in `$HOME`, where `--resume <id>` finds no
        // project and the chat is gone. The snapshot cannot cover for it either — a tool
        // parked in a TUI never emits OSC 7, so `spec.cwd` is `None`.
        assert_eq!(p.cwd.as_deref(), Some("/tmp"));
    }

    // Restoring a tool pane re-spawns its pty, so this one needs a runtime.
    #[tokio::test]
    async fn a_resumed_pane_is_directed_at_the_conversations_directory_not_the_snapshots() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // The snapshot disagrees with the mark. The mark wins: the id only resolves inside
        // the project it was recorded in, and a stale tracked cwd is exactly the failure
        // the pair was introduced to survive.
        st.attach_panes_from_specs(
            &m,
            &[PaneSpec {
                cwd: Some("/var".into()),
                ..spec_with(&[
                    (avada_core::tools::kind::META_KIND_KEY, "copilot"),
                    (avada_core::tools::META_SESSION_KEY, "aaaa-bbbb-cccc"),
                    (avada_core::tools::META_SESSION_CWD_KEY, "/tmp"),
                ])
            }],
        );
        assert_eq!(
            st.active_tab().panes.last().unwrap().cwd.as_deref(),
            Some("/tmp")
        );
    }

    // Restoring a tool pane re-spawns its pty, so this one needs a runtime.
    #[tokio::test]
    async fn a_tool_with_no_checked_resume_shape_starts_fresh() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // `resume_args` is the one authority, and it only answers for tools whose flag was
        // read off a real `--help`. Guessing at one would relaunch the pane into an
        // argument error instead of a chat.
        st.attach_panes_from_specs(
            &m,
            &[spec_with(&[
                (avada_core::tools::kind::META_KIND_KEY, "aider"),
                (avada_core::tools::META_SESSION_KEY, "aaaa-bbbb-cccc"),
                (avada_core::tools::META_SESSION_CWD_KEY, "/tmp"),
            ])],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.startup, None);
        // And it is NOT redirected into the mark's directory: nothing was resumed, so this
        // is an ordinary new `aider` pane and belongs where the snapshot put it.
        assert_eq!(p.cwd, None);
    }

    // Restoring a tool pane re-spawns its pty, so this one needs a runtime.
    #[tokio::test]
    async fn half_a_mark_resumes_nothing() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // An id with no directory cannot be resumed *anywhere* — every provider keys off
        // the project too — so the pane must come back as a plain tool pane, not as a
        // resume against whatever cwd happens to be current.
        st.attach_panes_from_specs(
            &m,
            &[spec_with(&[
                (avada_core::tools::kind::META_KIND_KEY, "copilot"),
                (avada_core::tools::META_SESSION_KEY, "aaaa-bbbb-cccc"),
            ])],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.startup, None);
        assert!(p.tool_session.is_none());
        assert_eq!(p.kind, PaneKind::Tool("copilot".into()));
    }

    #[test]
    fn a_view_panes_target_survives_a_real_snapshot_round_trip() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let want = [
            (PaneKind::FileBrowser, "/tmp"),
            (PaneKind::FileViewer, "/tmp/notes.txt"),
            (PaneKind::Markdown, "/tmp/README.md"),
        ];
        for (kind, target) in &want {
            st.add_pane_opts(
                &m,
                NewPaneOpts {
                    kind: Some(kind.clone()),
                    cwd: Some((*target).to_string()),
                    ..Default::default()
                },
            );
        }
        // Out through the real serializer and back through the real restore — the two
        // halves that have to agree on `pane.kind` and on cwd-as-target.
        let specs = snapshot_panes(&st);
        let mut back = State::new(theme::load_font(1.0));
        back.attach_panes_from_specs(&m, &specs);

        let got: Vec<_> = back
            .active_tab()
            .panes
            .iter()
            .map(|p| (p.kind.clone(), p.cwd.clone().unwrap_or_default()))
            .collect();
        assert_eq!(
            got,
            want.iter()
                .map(|(k, t)| (k.clone(), (*t).to_string()))
                .collect::<Vec<_>>()
        );
    }

    // Restoring a tool pane re-spawns its pty, so this one needs a runtime.
    #[tokio::test]
    async fn a_hand_started_pane_resumes_from_the_tool_its_own_mark_names() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // No `pane.kind` at all — this is the shape a pane the human turned into a tool
        // pane BY TYPING the tool's name snapshots as, because the runtime sniff that
        // re-brands its chrome deliberately never writes the persisted kind. Before the
        // mark carried `tool.id` there was nothing here to name a binary, so the pane came
        // back as a bare shell and the conversation was gone.
        st.attach_panes_from_specs(
            &m,
            &[spec_with(&[
                (avada_core::tools::META_SESSION_KEY, "aaaa-bbbb-cccc"),
                (avada_core::tools::META_SESSION_CWD_KEY, "/tmp"),
                (avada_core::tools::META_SESSION_TOOL_KEY, "copilot"),
            ])],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(
            p.startup.as_deref(),
            Some("cd '/tmp' && copilot --resume aaaa-bbbb-cccc\r")
        );
        // And the pane is directed at the conversation's own directory, like every other
        // resumed tool pane: the id only resolves inside the project it was recorded in.
        assert_eq!(p.cwd.as_deref(), Some("/tmp"));
    }

    // Restoring a tool pane re-spawns its pty, so this one needs a runtime.
    #[tokio::test]
    async fn a_mark_that_names_no_tool_and_a_kind_that_names_none_either_resume_nothing() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        // The pair alone cannot say which binary takes the id. Guessing one is how a pane
        // ends up running the wrong program against somebody else's conversation id, so
        // the pane comes back as the plain shell it was recorded as.
        st.attach_panes_from_specs(
            &m,
            &[spec_with(&[
                (avada_core::tools::META_SESSION_KEY, "aaaa-bbbb-cccc"),
                (avada_core::tools::META_SESSION_CWD_KEY, "/tmp"),
            ])],
        );
        let p = st.active_tab().panes.last().unwrap();
        assert_eq!(p.startup, None);
        // The mark is still kept, though: a later build that learns the tool, or a hook
        // that fills the gap, must not find the fact already thrown away.
        assert_eq!(
            p.tool_session.as_ref().map(|m| m.id.as_str()),
            Some("aaaa-bbbb-cccc")
        );
    }

    // Spawns a real pty.
    #[tokio::test]
    async fn a_pane_that_already_knows_its_conversation_is_never_told_a_different_one() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st
            .add_pane_opts(
                &m,
                NewPaneOpts {
                    kind: Some(PaneKind::Tool("copilot".into())),
                    cwd: Some("/tmp".to_string()),
                    command: Some("/bin/cat".to_string()),
                    session: Some("aaaa-bbbb-cccc".to_string()),
                    ..Default::default()
                },
            )
            .expect("the pane spawns");
        // The spawn out of the session list handed this pane one exact conversation. A
        // hook report or an inference arriving afterwards must not overwrite it — a
        // silently swapped id resumes the human into the wrong chat, which is worse than
        // not resuming at all.
        let other = ToolSessionMark::new("dddd-eeee-ffff", "/tmp").unwrap();
        assert!(!st.adopt_tool_session(&uid, other));
        assert_eq!(
            st.active_tab()
                .panes
                .last()
                .unwrap()
                .tool_session
                .as_ref()
                .map(|m| m.id.as_str()),
            Some("aaaa-bbbb-cccc")
        );
        // ...and it is not looked for either, so nothing keeps scanning on its behalf.
        assert_eq!(st.tool_session_wanted(&uid), None);
    }

    // Spawns a real pty.
    #[tokio::test]
    async fn a_shell_pane_seen_running_a_tool_is_looked_for_under_that_tool() {
        let m = mgr();
        let mut st = State::new(theme::load_font(1.0));
        let uid = st
            .add_pane_opts(
                &m,
                NewPaneOpts {
                    cwd: Some("/tmp".to_string()),
                    command: Some("/bin/cat".to_string()),
                    ..Default::default()
                },
            )
            .expect("the pane spawns");
        // A plain shell has no conversation to find and no tool to find one under.
        assert_eq!(st.tool_session_wanted(&uid), None);

        // The human types `claude`. The persisted kind stays `Terminal` — only the sniff
        // moves — so the search has to read the EFFECTIVE kind or it would never look for
        // the one pane this whole path exists to rescue.
        st.note_pane_foreground(&uid, Some("claude"));
        // The shell has not reported a cwd yet, so there is a tool to look under but no
        // directory to scope a scan by. The hook path can still run; the scan waits.
        assert_eq!(
            st.tool_session_wanted(&uid),
            Some(("claude".to_string(), None))
        );

        // OSC 7 lands (see the cwd arm of the pane-event pump) and now both halves are known.
        let (ti, pi) = st.find_pane(&uid).expect("the pane is laid out");
        st.tabs[ti].panes[pi].cwd = Some("/tmp".to_string());
        assert_eq!(
            st.tool_session_wanted(&uid),
            Some(("claude".to_string(), Some("/tmp".to_string())))
        );
        assert!(matches!(
            st.active_tab().panes.last().unwrap().kind,
            PaneKind::Terminal
        ));

        // Once a conversation is adopted the pane stops being looked for.
        let mark = ToolSessionMark::new("aaaa-bbbb-cccc", "/tmp")
            .unwrap()
            .with_tool("claude");
        assert!(st.adopt_tool_session(&uid, mark));
        assert_eq!(st.tool_session_wanted(&uid), None);
        // And the adopted mark rides out through the real serializer, tool id and all —
        // without it the restore above has no binary to name.
        let meta = snapshot_panes(&st)
            .pop()
            .expect("the pane is in the snapshot")
            .meta
            .expect("an adopted pane carries meta");
        assert_eq!(
            meta.get(avada_core::tools::META_SESSION_KEY)
                .map(String::as_str),
            Some("aaaa-bbbb-cccc")
        );
        assert_eq!(
            meta.get(avada_core::tools::META_SESSION_TOOL_KEY)
                .map(String::as_str),
            Some("claude")
        );
    }
}

#[cfg(test)]
mod tool_identity_tests {
    //! T3 — the runtime half of tool detection: an OSC title upgrades a plain terminal's
    //! CHROME, and never what it relaunches as.
    //!
    //! The plan's detection precedence is Explicit → Marker → Sniff, with one hard rule:
    //! an inference may change how a pane is drawn but must never rewrite `spawn_command`
    //! or the persisted `PaneKind`. Here that rule is structural rather than remembered —
    //! the sniff lives in a runtime-only side map (`State::sniffed_tool`) that nothing
    //! serializes, and the two are combined only in `effective_kind`, which the UI
    //! projection reads. These tests pin both halves of that split.
    use super::*;

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    /// One window holding one plain-terminal pane called `uid`.
    fn window_with(m: &SessionManager, uid: &str) -> State {
        let mut st = State::new(theme::load_font(1.0));
        st.adopt_pane(
            m,
            DetachedPane {
                uid: uid.into(),
                title: uid.into(),
                subtitle: None,
                pinned_accent: None,
                show_frame: None,
                show_dot: None,
                font_px: 14.0,
                spawn_command: None,
                spawn_args: None,
                spawn_shell: None,
                kind: PaneKind::default(),
                tool_session: None,
                cwd: None,
            },
        );
        st
    }

    fn pane_of<'a>(st: &'a State, uid: &str) -> &'a PaneState {
        let (ti, pi) = st
            .tabs
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.panes.iter().position(|p| p.uid == uid).map(|pi| (ti, pi)))
            .expect("pane is hosted");
        &st.tabs[ti].panes[pi]
    }

    /// The core of D5: after the sniff the pane *draws* as Claude, but the field a relaunch
    /// replays is untouched. Getting this wrong permanently brands a shell as Claude.
    #[test]
    fn a_title_sniff_upgrades_the_chrome_and_nothing_else() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude — ~/code/hyperpanes");

        let ps = pane_of(&st, "p1");
        assert_eq!(
            st.effective_kind(ps),
            PaneKind::Tool("claude".into()),
            "the sniff must drive what the header draws"
        );
        assert_eq!(
            ps.kind,
            PaneKind::Terminal,
            "the sniff must NOT touch the persisted kind — that is what a relaunch replays"
        );
        assert!(
            ps.spawn_command.is_none(),
            "and it must not invent a command"
        );
    }

    /// Explicit beats inferred. A pane spawned as Codex that happens to print a title
    /// mentioning Claude stays Codex, and nothing is recorded for it at all.
    #[test]
    fn a_sniff_never_overwrites_an_explicit_kind() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        {
            let (ti, pi) = st.find_pane("p1").unwrap();
            st.tabs[ti].panes[pi].kind = PaneKind::Tool("codex".into());
        }
        st.note_pane_title("p1", "claude");

        assert!(
            st.sniffed_tool.is_empty(),
            "an already-identified pane must not even be sniffed"
        );
        assert_eq!(
            st.effective_kind(pane_of(&st, "p1")),
            PaneKind::Tool("codex".into())
        );
    }

    /// The downgrade path. Without it, a pane that ran `claude` once wears the mark forever.
    #[test]
    fn returning_to_a_prompt_drops_the_sniff_and_the_badge() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude");
        st.note_agent_state("p1", AgentLiveness::AwaitingInput);
        assert_eq!(st.liveness_ui("p1"), 2);

        st.note_agent_idle("p1");
        assert_eq!(st.effective_kind(pane_of(&st, "p1")), PaneKind::Terminal);
        assert_eq!(st.liveness_ui("p1"), 0, "a stale badge is worse than none");
    }

    /// A tool prints plenty of transient titles. One frame that names no tool is not a
    /// downgrade signal — only the shell reaching a prompt is.
    #[test]
    fn a_title_naming_no_tool_leaves_the_previous_sniff_alone() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude");
        st.note_pane_title("p1", "~/code/hyperpanes");
        assert_eq!(
            st.effective_kind(pane_of(&st, "p1")),
            PaneKind::Tool("claude".into())
        );
    }

    // ---- D13: the layout as a property of the checkout ----

    /// The save seam end to end: what lands in `.avada/project.json` is what the
    /// checkout needs to rebuild these windows — the command, and a cwd expressed
    /// *relative to the root* so the file still means something on a machine that keeps
    /// its checkouts somewhere else. The run-local session uid must not be in it.
    #[test]
    fn saving_to_a_repo_writes_a_relocatable_file_with_no_run_local_state() {
        let root = std::env::temp_dir().join(format!(
            "hp-d13-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // `find_project_root` walks up to a `.git`, so a plain checkout with no
        // `.avada/` yet is exactly where a first save lands.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let sub = root.join("crates").join("core");
        std::fs::create_dir_all(&sub).unwrap();

        let m = mgr();
        let mut st = window_with(&m, "pane-0");
        {
            let (ti, pi) = st.find_pane("pane-0").unwrap();
            st.tabs[ti].panes[pi].cwd = Some(sub.to_string_lossy().into_owned());
            st.tabs[ti].panes[pi].spawn_command = Some("claude".into());
        }

        st.save_project();

        let written = root.join(".avada").join("project.json");
        let raw = std::fs::read_to_string(&written).expect("the save landed in the checkout");
        assert!(
            raw.contains("claude"),
            "what the window runs is the point: {raw}"
        );
        assert!(
            !raw.contains("\"uid\""),
            "a run-local uid must not travel in a version-controlled file: {raw}"
        );
        assert!(
            raw.contains("\"cwd\": \"crates/core\""),
            "the cwd is stored relative to the root: {raw}"
        );
        assert!(
            !raw.contains(&root.to_string_lossy().into_owned()),
            "an absolute path outside the clone would not survive the trip: {raw}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The "already live" test that guards the open path is scoped to *this* project: a
    /// pane sitting in an unrelated checkout is somebody else's work and must not count as
    /// "this project is already on screen".
    #[test]
    fn only_panes_inside_the_checkout_count_as_that_project_being_live() {
        let root = std::env::temp_dir().join(format!("hp-d13-live-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let inside = root.join("crates");
        std::fs::create_dir_all(&inside).unwrap();

        let root_s = root.to_string_lossy().into_owned();
        assert!(cwd_in_project(&inside.to_string_lossy(), &root_s));
        assert!(
            !cwd_in_project(&std::env::temp_dir().to_string_lossy(), &root_s),
            "a cwd outside the checkout is not this project"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The kernel's answer upgrades chrome exactly like a title does — and, like a title,
    /// leaves the field a relaunch replays alone.
    #[test]
    fn a_foreground_probe_upgrades_the_chrome_and_nothing_else() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_foreground("p1", Some("claude"));

        let ps = pane_of(&st, "p1");
        assert_eq!(st.effective_kind(ps), PaneKind::Tool("claude".into()));
        assert_eq!(ps.kind, PaneKind::Terminal);
        assert!(ps.spawn_command.is_none());
    }

    /// `None` is "the question has no answer here" — a platform with no foreground process
    /// group, a daemon that predates the field. Treating it as a downgrade would erase
    /// every other detection signal on exactly those platforms.
    #[test]
    fn no_answer_from_the_probe_leaves_every_belief_alone() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude");
        st.note_agent_state("p1", AgentLiveness::Busy);

        st.note_pane_foreground("p1", None);

        assert_eq!(
            st.effective_kind(pane_of(&st, "p1")),
            PaneKind::Tool("claude".into()),
            "silence is not a downgrade"
        );
        assert_eq!(st.liveness_ui("p1"), 1);
    }

    /// A definite answer naming something that is not a tool *is* a downgrade: the shell
    /// owns the terminal again, so the mark and the badge both go. This is the difference
    /// between the probe and a title — a title that names no tool means nothing.
    #[test]
    fn a_shell_in_the_foreground_is_an_honest_downgrade() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude");
        st.note_agent_state("p1", AgentLiveness::AwaitingInput);

        st.note_pane_foreground("p1", Some("zsh"));

        assert_eq!(st.effective_kind(pane_of(&st, "p1")), PaneKind::Terminal);
        assert_eq!(st.liveness_ui("p1"), 0, "a stale badge is worse than none");
    }

    /// Explicit still beats inferred: a pane spawned as Codex whose foreground reads
    /// `claude` (a subprocess, a typo, a wrapper) stays Codex, and records nothing.
    #[test]
    fn a_foreground_probe_never_overwrites_an_explicit_kind() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        {
            let (ti, pi) = st.find_pane("p1").unwrap();
            st.tabs[ti].panes[pi].kind = PaneKind::Tool("codex".into());
        }
        st.note_pane_foreground("p1", Some("claude"));
        assert!(st.sniffed_tool.is_empty());
        assert_eq!(
            st.effective_kind(pane_of(&st, "p1")),
            PaneKind::Tool("codex".into())
        );
    }

    /// An ambiguous title names two tools; `registry::by_title` refuses to guess, so no
    /// upgrade happens rather than a coin-flip one.
    #[test]
    fn an_ambiguous_title_upgrades_nothing() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude · codex");
        assert_eq!(st.effective_kind(pane_of(&st, "p1")), PaneKind::Terminal);
    }

    /// Every liveness state gets its own code, and an unknown pane reports 0.
    #[test]
    fn liveness_codes_are_distinct() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        assert_eq!(st.liveness_ui("p1"), 0);
        let mut seen = vec![];
        for s in [
            AgentLiveness::Busy,
            AgentLiveness::AwaitingInput,
            AgentLiveness::Done,
            AgentLiveness::Error,
        ] {
            st.note_agent_state("p1", s);
            seen.push(st.liveness_ui("p1"));
        }
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "each state needs its own code");
        assert!(!seen.contains(&0), "0 is reserved for 'nothing reported'");
    }

    /// The side maps are keyed by uid and nothing ever revisits a closed pane's key, so
    /// every removal path has to drop them or they grow for the life of the process.
    #[test]
    fn closing_a_pane_forgets_its_runtime_facts() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude");
        st.note_agent_state("p1", AgentLiveness::Busy);
        assert!(!st.sniffed_tool.is_empty() && !st.agent_live.is_empty());

        let (ti, pi) = st.find_pane("p1").unwrap();
        st.close_pane_in(ti, pi, &m);
        assert!(st.sniffed_tool.is_empty(), "sniff outlived its pane");
        assert!(st.agent_live.is_empty(), "badge outlived its pane");
    }

    /// Detaching for re-host takes the same path — the target window re-learns from the
    /// next title, and the source window must not keep a dangling key.
    #[test]
    fn detaching_a_pane_forgets_its_runtime_facts() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("p1", "claude");
        st.detach_focused(&m).expect("the pane detaches");
        assert!(st.sniffed_tool.is_empty());
    }

    /// A pane that isn't hosted here can't be sniffed — the events are routed per window,
    /// and a stray uid must not seed an entry that nothing will ever clean up.
    #[test]
    fn an_unhosted_uid_records_nothing() {
        let m = mgr();
        let mut st = window_with(&m, "p1");
        st.note_pane_title("ghost", "claude");
        assert!(st.sniffed_tool.is_empty());
    }
}

#[cfg(test)]
mod left_panel_tests {
    //! M5 — the left slide-out panel's STATE side: the workspace tree's click-to-focus and
    //! drag-between-tabs, the "what is this window holding?" question the DETACHED section
    //! subtracts, and the guards on adopt. The projection itself (`paneview::resync`) and
    //! the panel's geometry live in `ui/leftpanel.slint`; everything reachable without a
    //! window is pinned here.
    use super::*;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: uid.into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    /// A window of `tabs` tabs, tab *i* holding the uids in `tabs[i]`. Built without
    /// spawning anything: `adopt_pane*` re-hosts a detached pane, which is all a tree test
    /// needs. Leaves the LAST tab active (that is what `adopt_pane_as_tab` does).
    fn window(m: &SessionManager, tabs: &[&[&str]]) -> State {
        let mut st = fresh();
        for (i, uids) in tabs.iter().enumerate() {
            if i > 0 {
                st.adopt_pane_as_tab(m, det(uids[0]));
                for u in &uids[1..] {
                    st.adopt_pane(m, det(u));
                }
            } else {
                for u in uids.iter() {
                    st.adopt_pane(m, det(u));
                }
            }
        }
        st
    }

    fn uids_in(st: &State, ti: usize) -> Vec<String> {
        st.tabs[ti]
            .panes
            .iter()
            .map(|p| p.uid.to_string())
            .collect()
    }

    #[test]
    fn tree_click_switches_to_the_tab_then_focuses_the_pane() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0"]]);
        assert_eq!(st.active, 1, "the helper leaves the last tab active");

        // A click on a BACKGROUND tab's second pane means "take me there".
        st.focus_pane_in_tab(0, 1);
        assert_eq!(st.active, 0);
        assert_eq!(st.active_tab().focused, 1);

        // Out-of-range indices arrive from a UI model snapshot — they must be ignored, not
        // clamped onto the wrong pane.
        st.focus_pane_in_tab(9, 0);
        assert_eq!((st.active, st.active_tab().focused), (0, 1));
        st.focus_pane_in_tab(1, 5);
        assert_eq!(
            (st.active, st.active_tab().focused),
            (0, 1),
            "no tab switch either"
        );
    }

    #[test]
    fn drag_moves_a_pane_between_two_background_tabs() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0"], &["c0"]]);
        assert_eq!(st.active, 2);

        // Neither end of the drag is the active tab — the case `move_pane_to_tab` cannot
        // express, and the reason `move_pane_between_tabs` exists.
        st.move_pane_between_tabs(0, 0, 1, &m);
        assert_eq!(uids_in(&st, 0), ["a1"]);
        assert_eq!(
            uids_in(&st, 1),
            ["b0", "a0"],
            "appended at the end of the target"
        );
        assert_eq!(st.active, 2, "a tree drag never steals the active tab");
        // A move is a re-host, not a respawn: the session is still this window's.
        assert!(st.claimed_uids().contains("a0"));
    }

    #[test]
    fn dragging_the_last_pane_out_drops_the_tab_and_shifts_the_target() {
        let m = mgr();
        // Tab 0 holds a single pane; dragging it into tab 1 empties tab 0, which is dropped —
        // so the target index the UI sent (1) now names a DIFFERENT tab. Without the shift
        // the pane lands in the wrong workspace, which is the whole bug this guards.
        let mut st = window(&m, &[&["a0"], &["b0"], &["c0"]]);
        assert_eq!(st.active, 2);

        st.move_pane_between_tabs(0, 0, 1, &m);
        assert_eq!(st.tabs.len(), 2, "the emptied source tab is dropped");
        assert_eq!(
            uids_in(&st, 0),
            ["b0", "a0"],
            "landed in the tab that WAS index 1"
        );
        assert_eq!(uids_in(&st, 1), ["c0"]);
        assert_eq!(st.active, 1, "the active tab followed its own shift");
    }

    #[test]
    fn dragging_out_of_the_active_tab_takes_the_shared_move_path() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0"]]);
        assert_eq!(st.active, 1);

        st.move_pane_between_tabs(1, 0, 0, &m);
        // Tab 1 emptied → dropped; its pane is now in tab 0.
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(uids_in(&st, 0), ["a0", "a1", "b0"]);
        assert_eq!(st.active, 0);
    }

    #[test]
    fn a_stale_drag_is_ignored_rather_than_clamped() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0"]]);
        let before: Vec<Vec<String>> = (0..st.tabs.len()).map(|i| uids_in(&st, i)).collect();

        st.move_pane_between_tabs(0, 0, 0, &m); // onto itself
        st.move_pane_between_tabs(9, 0, 1, &m); // source tab gone
        st.move_pane_between_tabs(0, 9, 1, &m); // pane gone (session exited mid-drag)
        st.move_pane_between_tabs(0, 0, 9, &m); // target tab gone

        let after: Vec<Vec<String>> = (0..st.tabs.len()).map(|i| uids_in(&st, i)).collect();
        assert_eq!(before, after, "no snapshot-stale drag may move anything");
    }

    #[test]
    fn claimed_uids_covers_every_place_a_session_is_held() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0", "b1"]]);

        // laid out in a background tab AND in the active one
        let claimed = st.claimed_uids();
        for u in ["a0", "a1", "b0", "b1"] {
            assert!(claimed.contains(u), "{u} is laid out");
        }

        // parked as a reminder — still alive, so still claimed
        st.remind_pane(0, ReminderOffset::Min15);
        assert_eq!(st.reminders.len(), 1);
        assert!(st.claimed_uids().contains("b0"));

        // …and on the recently-closed history, whose PTYs stay alive for reopen. Missing
        // these would offer them in the DETACHED list and let one click give a uid two homes.
        st.close_tab_menu(0, &m);
        assert!(!st.closed.is_empty(), "closing parked the tab for reopen");
        let claimed = st.claimed_uids();
        assert!(
            claimed.contains("a0") && claimed.contains("a1"),
            "{claimed:?}"
        );
        // Exactly the set this window kills when it closes — no more, no less.
        let killed: std::collections::HashSet<String> = st.session_uids().into_iter().collect();
        assert_eq!(claimed, killed);
    }

    #[test]
    fn adopt_refuses_a_session_this_window_already_holds() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0"]]);
        let before = st.claimed_uids();

        st.adopt_detached_session("", &m); // no uid
        st.adopt_detached_session("a0", &m); // laid out in a background tab
        st.adopt_detached_session("b0", &m); // laid out in the active tab
        assert_eq!(st.claimed_uids(), before);
        assert_eq!(uids_in(&st, 1), ["b0"], "no duplicate pane appeared");

        // A pane on the reopen stack is held too — adopting it would give the uid two homes.
        st.close_tab_menu(0, &m);
        let before = st.claimed_uids();
        st.adopt_detached_session("a0", &m);
        assert_eq!(st.claimed_uids(), before);
        assert_eq!(st.active_tab().panes.len(), 1, "nothing was re-hosted");
    }

    #[test]
    fn toggling_the_panel_is_pure_window_state() {
        let mut st = fresh();
        assert!(!st.left_panel_open);
        st.dirty = false;
        st.toggle_left_panel();
        assert!(st.left_panel_open && st.dirty);
        st.dirty = false;
        st.toggle_left_panel();
        assert!(!st.left_panel_open && st.dirty);
        // The panel is a sibling of the pane area, not an overlay: toggling it must never
        // touch the overlay slot (which is what would dim the terminals behind a scrim).
        assert!(matches!(st.overlay, Overlay::None));
    }
}

#[cfg(test)]
mod set_tests {
    //! M6 — the workspace library and sets: `SaveWorkspaceAs` / `SaveSet` / `OpenSet`, and
    //! the reattach-or-spawn decision every load path now shares
    //! (`SessionManager::pane_load`). The genuine *re-attach* half needs a live daemon and is
    //! proven end-to-end in core (`session::daemon_client::tests`); here we prove the app-side
    //! plumbing: durable uids are written into library workspaces, survive the set round-trip,
    //! and reach the loader — where the in-process backend correctly declines to re-attach.
    use super::*;
    use avada_core::session_manager::PaneLoad;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str, command: Option<&str>) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: uid.into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: prefs::DEFAULT_FONT_PX,
            spawn_command: command.map(str::to_string),
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hp-app-sets-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A saved workspace carries the durable pane id from M0 alongside the program to re-run.
    /// The uid is what makes reattach-or-spawn possible when the workspace is re-opened; the
    /// command is the fallback when that uid names nothing live.
    #[test]
    fn library_snapshot_records_durable_uids_and_the_program() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("pane-live-1", Some("claude")));
        let library = st.to_library_workspace_file();
        let pane = &library.panes.as_ref().unwrap()[0];
        assert_eq!(pane.uid.as_deref(), Some("pane-live-1"));
        assert_eq!(pane.command.as_deref(), Some("claude"));
    }

    /// `SaveSet` writes one member workspace per non-empty tab plus the `sets/*.json` index,
    /// and `OpenSet` loads them all back — panes and their programs intact, and the durable
    /// uids preserved on disk so a live session could be adopted.
    #[test]
    fn save_set_then_open_set_round_trips_every_tab() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let dir = temp_dir("roundtrip");
        let sets_dir = dir.join("sets");
        let members_dir = dir.join("workspaces");

        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("pane-a", Some("claude"))); // tab 0
        st.tabs.push(Tab::empty("second".into()));
        st.active = 1;
        st.adopt_pane(&m, det("pane-b", Some("htop"))); // tab 1
        st.tabs.push(Tab::empty("empty".into())); // 0-pane tab: never becomes a member

        let set_path = sets_dir.join("morning.json");
        let set = st
            .save_set_to(&set_path, &members_dir, "Morning Routine")
            .expect("set written");
        assert_eq!(set.name, "Morning Routine");
        assert_eq!(set.members.len(), 2, "the 0-pane tab is not a member");
        assert!(set_path.exists(), "the set index landed in sets/");
        for mem in &set.members {
            assert!(
                std::path::Path::new(&mem.path).exists(),
                "member workspace {} written",
                mem.path
            );
        }
        // The member file on disk carries the durable pane id (the reattach key).
        let first = avada_core::workspace::io::read_workspace(&set.members[0].path).unwrap();
        assert_eq!(
            first.panes.as_ref().unwrap()[0].uid.as_deref(),
            Some("pane-a")
        );

        // Re-open into a fresh window: both member workspaces are appended as tabs, and the
        // pristine 0-pane placeholder `State::new` seeds is purged (`load_workspace`), so the
        // window ends up with exactly the set's tabs — no ghost empty tab.
        let mut st2 = fresh();
        let m2 = mgr();
        assert_eq!(st2.open_set_from(&set_path, &m2), 2, "both members loaded");
        assert_eq!(st2.tabs.len(), 2, "one tab per member, placeholder purged");
        let commands: Vec<Option<String>> = st2
            .tabs
            .iter()
            .map(|t| t.panes[0].spawn_command.clone())
            .collect();
        assert_eq!(
            commands,
            vec![Some("claude".to_string()), Some("htop".to_string())],
            "each member re-runs its own program"
        );
        // A set opens on its FIRST member, not on whichever member happened to load last.
        assert_eq!(st2.active, 0, "Open set lands on the first member's tab");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Reattach-or-spawn, app side.** Loading a set member whose pane recorded a uid asks
    /// `SessionManager::pane_load`. On the in-process backend the recorded uid can never name
    /// a survivor, so every pane SPAWNS under a fresh uid — never silently adopting the
    /// recorded one. (The daemon's re-attach half is proven in core against a real daemon.)
    #[test]
    fn loading_a_set_member_spawns_when_the_recorded_uid_is_not_live() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let dir = temp_dir("reattach-or-spawn");
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("pane-recorded", Some("htop")));
        let set_path = dir.join("sets").join("s.json");
        st.save_set_to(&set_path, &dir.join("workspaces"), "s")
            .expect("set written");

        let m2 = mgr();
        assert!(!m2.is_daemon(), "this leg exercises the re-spawn branch");
        // The decision the loader makes for that pane, taken directly:
        let decision = m2.pane_load(Some("pane-recorded"));
        assert!(
            !decision.is_reattach(),
            "no daemon ⇒ spawn, got {decision:?}"
        );
        assert_ne!(decision.uid(), "pane-recorded");
        assert!(matches!(decision, PaneLoad::Spawn(_)));

        let mut st2 = fresh();
        assert_eq!(st2.open_set_from(&set_path, &m2), 1);
        let pane = &st2.active_tab().panes[0];
        assert_ne!(
            pane.uid, "pane-recorded",
            "a spawn mints a fresh uid instead of adopting the recorded one, got {}",
            pane.uid
        );
        assert!(!pane.started, "a re-spawned pane starts unstarted");
        assert_eq!(pane.spawn_command.as_deref(), Some("htop"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt / missing set file is reported, not fatal, and changes nothing.
    #[test]
    fn opening_an_invalid_set_is_a_noop() {
        let dir = temp_dir("invalid");
        let bad = dir.join("bad.json");
        std::fs::write(&bad, b"not json {").unwrap();
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("keep", None));
        assert_eq!(st.open_set_from(&bad, &m), 0);
        assert_eq!(st.open_set_from(&dir.join("gone.json"), &m), 0);
        assert_eq!(st.tabs.len(), 1, "no tab was appended");
        assert_eq!(st.active_tab().panes.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Backward compatibility, app side.** A workspace file written by a PRE-M6 build has
    /// no `uid` on any pane. Loading it must not fail, must not adopt anything, and must
    /// simply spawn every pane from its recorded program — exactly the pre-M6 behaviour.
    #[test]
    fn a_legacy_workspace_file_loads_and_spawns_every_pane() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let dir = temp_dir("legacy");
        let path = dir.join("legacy.avada");
        // Verbatim shape of an old save-dialog file: bare object, no envelope, no uid.
        std::fs::write(
            &path,
            br#"{
  "name": "Old Save",
  "layout": "grid",
  "panes": [
    { "label": "one", "command": "claude" },
    { "label": "two", "shell": "/bin/zsh", "fontSize": 18 }
  ]
}"#,
        )
        .unwrap();

        let file = avada_core::workspace::io::read_workspace(&path).expect("legacy parses");
        for p in file.panes.iter().flatten() {
            assert_eq!(p.uid, None, "the fixture really is uid-less");
        }

        let mut st = fresh();
        let m = mgr();
        st.load_workspace(file, &m);
        let tab = st.active_tab();
        assert_eq!(tab.panes.len(), 2, "both legacy panes materialised");
        assert_eq!(tab.panes[0].spawn_command.as_deref(), Some("claude"));
        assert_eq!(tab.panes[1].spawn_shell.as_deref(), Some("/bin/zsh"));
        for p in &tab.panes {
            assert!(!p.started, "a uid-less pane is spawned, not re-attached");
            assert!(!p.uid.is_empty(), "the loader minted a fresh uid");
        }
        assert_ne!(tab.panes[0].uid, tab.panes[1].uid, "fresh uids are unique");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Forward round-trip.** "Save workspace as…" → disk → "Open workspace" preserves the
    /// durable uid on disk (the reattach key) while the load itself still spawns, because the
    /// in-process backend has no survivor to adopt. Uids are written, never `null`.
    #[test]
    fn save_as_then_reload_round_trips_the_durable_uid() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let dir = temp_dir("save-as");
        let path = dir.join("saved.avada");

        let mut st = fresh();
        let m = mgr();
        st.adopt_pane(&m, det("pane-keep-1", Some("claude")));
        st.adopt_pane(&m, det("pane-keep-2", Some("htop")));
        assert!(st.write_workspace_to(&path), "save-as wrote the file");

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"uid\""), "the field is camelCase `uid`");
        assert!(
            !raw.contains("null"),
            "unset optionals are omitted, never null"
        );
        assert!(raw.contains("pane-keep-1") && raw.contains("pane-keep-2"));

        let back = avada_core::workspace::io::read_workspace(&path).expect("re-reads");
        let uids: Vec<Option<String>> =
            back.panes.iter().flatten().map(|p| p.uid.clone()).collect();
        assert_eq!(
            uids,
            vec![
                Some("pane-keep-1".to_string()),
                Some("pane-keep-2".to_string())
            ],
            "both durable uids survived the disk round-trip in order"
        );

        let mut st2 = fresh();
        let m2 = mgr();
        st2.load_workspace(back, &m2);
        let tab = st2.active_tab();
        assert_eq!(tab.panes.len(), 2);
        for p in &tab.panes {
            assert!(
                !p.uid.starts_with("pane-keep"),
                "no live session named those uids, so each pane re-spawns fresh, got {}",
                p.uid
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A window with nothing but empty tabs has no set to save.
    #[test]
    fn saving_a_set_from_an_empty_window_writes_nothing() {
        let dir = temp_dir("empty");
        let mut st = fresh(); // State::new's pristine 0-pane placeholder tab
        let set_path = dir.join("sets").join("e.json");
        assert!(st
            .save_set_to(&set_path, &dir.join("workspaces"), "e")
            .is_none());
        assert!(!set_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The browser routing seam: `State::open_link` (which setting sends a URL where) and
/// `State::pick_browser` (answering the chooser). Deliberately free of any real launch —
/// what these pin down is the *decision*, which is the part a setting can get wrong.
#[cfg(test)]
mod browser_routing_tests {
    use super::*;

    fn fresh() -> State {
        State::new(crate::theme::load_font(1.0))
    }

    /// A scheme the open seam refuses never reaches the chooser: it reports the refusal and
    /// leaves the overlay alone. Otherwise "ask each time" would put a card on screen for a
    /// URL that could not be opened by any of the answers.
    #[test]
    fn refused_url_never_opens_the_chooser() {
        let mut st = fresh();
        st.settings.browser_mode = crate::prefs::BROWSER_MODE_ASK.to_string();
        assert!(st.open_link("javascript:alert(1)").is_err());
        assert_eq!(st.overlay, Overlay::None);
        assert!(st.ask_url.is_empty());
    }

    /// "ask" holds the URL rather than opening it — the whole point of the mode. Skipped
    /// where the machine reports no browsers at all, which is the documented degrade-to-OS
    /// path (there is nothing to ask about) and not something a test should assert against.
    #[test]
    fn ask_mode_holds_the_url_for_a_human() {
        if avada_core::open::list_browsers().is_empty() {
            return;
        }
        let mut st = fresh();
        st.settings.browser_mode = crate::prefs::BROWSER_MODE_ASK.to_string();
        assert!(st.open_link("https://example.com/x").is_ok());
        assert_eq!(st.overlay, Overlay::AskBrowser);
        assert_eq!(st.ask_url, "https://example.com/x");
        assert!(!st.ask_browsers.is_empty());
    }

    /// A row index that is no longer there closes the card instead of erroring or stranding
    /// it — a stale click must never leave the chooser stuck over the terminal.
    #[test]
    fn out_of_range_pick_closes_without_opening() {
        let mut st = fresh();
        st.ask_url = "https://example.com/x".into();
        st.ask_browsers = vec![avada_core::open::BrowserApp {
            id: "test.browser".into(),
            name: "Test".into(),
            launcher: "/nonexistent/browser".into(),
        }];
        st.overlay = Overlay::AskBrowser;

        assert!(st.pick_browser(99).is_ok());
        assert_eq!(st.overlay, Overlay::None);
        assert!(st.ask_url.is_empty());
        assert!(st.ask_browsers.is_empty());
    }

    /// Dismissing the card (Esc / Cancel → `close_overlay`) drops the held URL. "Ask" has to
    /// allow the answer "none of these", and a URL left in `ask_url` would be re-shown by the
    /// next open.
    #[test]
    fn dismissing_the_chooser_drops_the_url() {
        let mut st = fresh();
        st.ask_url = "https://example.com/x".into();
        st.overlay = Overlay::AskBrowser;
        st.close_overlay();
        assert_eq!(st.overlay, Overlay::None);
        assert!(st.ask_url.is_empty());
    }

    /// `"app"` naming a browser that is not installed degrades to the OS default handler
    /// rather than resolving to nothing — losing a browser must not turn links into dead
    /// clicks. (`browser_launcher` returning `None` is exactly the "OS handler" branch.)
    #[test]
    fn uninstalled_chosen_browser_falls_back_to_the_os() {
        let mut st = fresh();
        st.settings.browser_mode = crate::prefs::BROWSER_MODE_APP.to_string();
        st.settings.browser_app = "com.example.browser.that.is.not.installed".into();
        assert!(st.settings.browser_launcher().is_none());
        assert!(!st.settings.browser_asks());
    }
}

/// What the host kept of Request J once the working tree left for `bshuler/avada-git`:
/// the project anchor those rows are listed under, and the commit link that sends a hash
/// clicked in a pane to whichever module answers to the `git` rail entry.
#[cfg(test)]
mod git_links {
    use super::*;

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    fn module() -> avada_core::rights::ModuleId {
        avada_core::rights::ModuleId::new("bshuler/avada-git").unwrap()
    }

    /// The wire payload of `host.rail.register`; `tier` is the SDK's `UiTier`, which this
    /// crate cannot name.
    fn entry(id: &str) -> avada_core::module::RailEntry {
        serde_json::from_value(serde_json::json!({
            "id": id, "label": "Git", "tier": 1, "order": 0
        }))
        .unwrap()
    }

    /// The project anchor follows the SELECTED pane (K): it is derived from the focused
    /// pane's cwd, and moves when that pane's cwd moves to another project. A `cd` that
    /// stays inside the same repository derives the same root and must NOT re-root — that
    /// would collapse the tree under a human who only changed directory, in whatever
    /// module has a list open at that root.
    #[test]
    fn the_root_follows_the_selected_pane_but_not_a_cd_inside_it() {
        let base = std::env::temp_dir().join(format!("hp-anchor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("a"), base.join("b"));
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let git = |root: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git runs")
        };
        if !git(&a, &["init"]).status.success() {
            let _ = std::fs::remove_dir_all(&base);
            return; // no git on this machine
        }
        git(&b, &["init"]);

        let mut st = fresh();
        if st.active_tab().panes.is_empty() {
            let _ = std::fs::remove_dir_all(&base);
            return; // no placeholder pane to anchor on
        }
        let set_cwd = |st: &mut State, p: &Path| {
            let i = st.active_tab().focused;
            st.active_tab_mut().panes[i].cwd = Some(p.to_string_lossy().into_owned());
        };

        set_cwd(&mut st, &a);
        st.sync_left_root();
        let root_a = st
            .project_root
            .clone()
            .expect("rooted on the selected pane");
        assert!(
            root_a.ends_with("a"),
            "rooted at {root_a:?}, wanted the a repo"
        );

        // A cd deeper into the SAME repository derives the same root, and a root that did
        // not move is not re-applied — which is what keeps a module's open tree from being
        // rebuilt under the human.
        set_cwd(&mut st, &a.join("sub"));
        st.sync_left_root();
        assert_eq!(st.project_root.as_deref(), Some(root_a.as_path()));

        // A pane in a different project moves the panel there.
        set_cwd(&mut st, &b);
        st.sync_left_root();
        assert!(
            st.project_root.as_deref().is_some_and(|r| r.ends_with("b")),
            "rooted at {:?}, wanted the b repo",
            st.project_root
        );

        // An explicitly chosen root is the human's: it survives every tick until the
        // selected pane itself moves.
        st.set_project_root(b.join("chosen"));
        let manual = st.project_root.clone().unwrap();
        st.sync_left_root();
        st.sync_left_root();
        assert_eq!(st.project_root.as_deref(), Some(manual.as_path()));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A module is found by the ENTRY id it registered, never by its module id: `git` is a
    /// role, and any module that fills it gets the commit links. The host has no opinion
    /// about who publishes it.
    #[test]
    fn the_git_entry_is_found_by_role_not_by_publisher() {
        let mut st = fresh();
        assert_eq!(
            st.git_entry_key(),
            None,
            "nothing registered, nothing found"
        );
        st.apply_rail_event(avada_core::module::RailEvent::Registered {
            module: module(),
            entries: vec![entry(GIT_ENTRY)],
        });
        assert_eq!(
            st.git_entry_key().as_deref(),
            Some(crate::leftpanel::entry_key(&module(), GIT_ENTRY).as_str())
        );
    }

    /// The whole commit link, end to end over a real repository: a hash printed in a pane
    /// re-roots the panel on the commit's OWN repository, activates the git entry, and
    /// emits the event the module answers by listing that commit's files.
    #[test]
    fn a_commit_link_re_roots_activates_and_announces_the_commit() {
        let root = std::env::temp_dir().join(format!("hp-showcommit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs")
        };
        assert!(git(&["init", "-b", "trunk"]).status.success(), "git init");
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("a.txt"), "hi").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "the subject"]);
        let out = git(&["rev-parse", "HEAD"]);
        let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(!hash.is_empty(), "no commit hash");
        let real = std::fs::canonicalize(&root).unwrap();

        let mut st = fresh();
        st.apply_rail_event(avada_core::module::RailEvent::Registered {
            module: module(),
            entries: vec![entry(GIT_ENTRY)],
        });

        assert!(
            !st.show_commit(
                &root.to_string_lossy(),
                "0000000000000000000000000000000000000000"
            ),
            "a hash git cannot resolve must report failure, so the caller can say so"
        );

        assert!(st.show_commit(&root.to_string_lossy(), &hash));
        assert!(st.left_panel_open, "the link opens the panel it draws in");
        assert_eq!(st.project_root.as_deref(), Some(real.as_path()));
        assert_eq!(
            st.rail.active.as_deref(),
            Some(crate::leftpanel::entry_key(&module(), GIT_ENTRY).as_str())
        );
        let events = st.take_module_events();
        let (kind, payload) = events.last().expect("the module was told");
        assert_eq!(kind, avada_core::module::rpc::GIT_COMMIT_EVENT);
        assert_eq!(payload["rev"], serde_json::json!(hash));
        assert_eq!(
            payload["root"],
            serde_json::json!(real.display().to_string())
        );
        assert_eq!(payload["short"], serde_json::json!(hash[..7].to_string()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// With no git module installed the commit still RESOLVED, so the click is reported as
    /// handled and explained by a toast. Reporting failure would make the caller add a
    /// second, untrue explanation ("no such commit") for the same nothing-happened.
    #[test]
    fn a_commit_link_with_no_module_installed_explains_itself_once() {
        let mut st = fresh();
        let here = std::env::current_dir().expect("a cwd");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&here)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git runs");
        if !out.status.success() {
            return; // not run from inside a repository
        }
        let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(
            st.show_commit(&here.to_string_lossy(), &hash),
            "the commit is real; there is simply nowhere to draw it"
        );
        assert!(
            st.take_module_events().is_empty(),
            "nothing to announce it to"
        );
    }
}

#[cfg(test)]
mod keyboard_focus_tests {
    use super::*;

    fn fresh() -> State {
        State::new(crate::theme::load_font(1.0))
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: uid.into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    /// The `refocus_seq` of whatever pane is selected right now.
    fn seq(st: &State) -> i32 {
        let t = st.active_tab();
        t.panes[t.focused].refocus_seq
    }

    /// Clicking a tab must make its pane typeable, without a second click on the pane.
    #[test]
    fn selecting_a_tab_hands_the_keyboard_to_its_pane() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane_as_tab(&m, det("a"));
        st.adopt_pane_as_tab(&m, det("b"));
        let (ta, tb) = (st.tabs.len() - 2, st.tabs.len() - 1);

        st.switch_tab(ta);
        st.sync_pane_keyboard_focus();
        let on_a = seq(&st);

        st.switch_tab(tb);
        st.sync_pane_keyboard_focus();
        let on_b = seq(&st);

        // Slint only re-focuses on a CHANGED tick, and the two tabs render through the same
        // repeater slot — so b's value must differ from the one a left showing there.
        assert_ne!(
            on_b, on_a,
            "the newly selected tab's pane must present a value Slint sees as changed"
        );

        // Going back is a move again, and must not reuse a: the clock never repeats.
        st.switch_tab(ta);
        st.sync_pane_keyboard_focus();
        let back_on_a = seq(&st);
        assert_ne!(back_on_a, on_a);
        assert_ne!(back_on_a, on_b);
    }

    /// A tick where nothing moved must stop touching focus once the widget has CONFIRMED it
    /// holds the keyboard — otherwise every frame would yank the keyboard back off whatever
    /// the user had clicked into.
    #[test]
    fn a_confirmed_hand_off_makes_every_later_tick_free() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane_as_tab(&m, det("a"));
        st.sync_pane_keyboard_focus();
        // The widget answers: it really does hold the keyboard now.
        let f = st.active_tab().focused;
        let uid = st.active_tab().panes[f].uid.clone();
        st.note_pane_focus(&uid, true);
        st.sync_pane_keyboard_focus();
        let settled = seq(&st);
        for _ in 0..10 {
            st.sync_pane_keyboard_focus();
        }
        assert_eq!(seq(&st), settled, "a confirmed selection must cost nothing");
    }

    /// Until the widget confirms, the hand-off RETRIES — one bump per tick, because the
    /// pane's Slint element may not exist yet on the tick the selection moved. The retry is
    /// bounded: an element that never answers (a non-terminal pane, a hidden window) must not
    /// leave us bumping focus every frame forever.
    #[test]
    fn an_unconfirmed_hand_off_retries_a_bounded_number_of_times() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane_as_tab(&m, det("a"));
        let before = seq(&st);
        // Derived from the cap, not a fixed count: the loop has to outrun the budget for the
        // "then gives up" half of the assertion to mean anything.
        for _ in 0..(KBD_FOCUS_TRIES as u32 + 8) {
            st.sync_pane_keyboard_focus();
        }
        let bumps = seq(&st) - before;
        assert_eq!(
            bumps, KBD_FOCUS_TRIES as i32,
            "an unanswered hand-off retries exactly KBD_FOCUS_TRIES times, then gives up"
        );
    }

    /// Selecting a pane inside the SAME tab is a move too — Ctrl+arrow, a drag, a close.
    #[test]
    fn selecting_another_pane_in_the_same_tab_moves_the_keyboard() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane_as_tab(&m, det("a"));
        st.adopt_pane(&m, det("b"));
        st.sync_pane_keyboard_focus();
        let on_b = seq(&st);

        let a = st
            .active_tab()
            .panes
            .iter()
            .position(|p| p.uid == "a")
            .expect("a is in this tab");
        st.focus_pane(a);
        st.sync_pane_keyboard_focus();
        assert_ne!(seq(&st), on_b);
    }

    /// A double click on an UNFOCUSED pane header selects it and opens its label editor in
    /// one gesture. Grabbing the keyboard back for the terminal would cancel the rename.
    #[test]
    fn an_open_editor_keeps_the_keyboard() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane_as_tab(&m, det("a"));
        st.adopt_pane(&m, det("b"));
        st.sync_pane_keyboard_focus();

        let a = st
            .active_tab()
            .panes
            .iter()
            .position(|p| p.uid == "a")
            .expect("a is in this tab");
        let before = st.active_tab().panes[a].refocus_seq;
        st.focus_pane(a);
        st.editing_pane = a as i32;
        st.sync_pane_keyboard_focus();
        assert_eq!(
            seq(&st),
            before,
            "the pane label editor must keep the keys it was opened to receive"
        );

        // Once the editor closes, the pending move lands.
        st.editing_pane = -1;
        st.sync_pane_keyboard_focus();
        assert_ne!(seq(&st), before);
    }

    /// The acknowledgement is queued by the window and applied on the NEXT pump, so it can
    /// arrive after the rows it was reported against have moved. Keying it by uid is what
    /// makes that safe: an ack for the pane that really holds the keyboard must confirm THAT
    /// pane, whatever slot it now sits in.
    #[test]
    fn a_late_acknowledgement_confirms_the_pane_it_names_not_a_slot() {
        let mut st = fresh();
        let m = mgr();
        st.adopt_pane_as_tab(&m, det("a"));
        st.adopt_pane(&m, det("b"));
        st.sync_pane_keyboard_focus();

        // The widget reports for "a" while "b" is the selected pane — the shape of a stale
        // report. It must land on "a" and leave the hand-off to "b" unconfirmed.
        st.note_pane_focus("a", true);
        assert_eq!(st.kbd_focus_live.as_deref(), Some("a"));
        let before = seq(&st);
        st.sync_pane_keyboard_focus();
        assert_ne!(seq(&st), before, "an ack for another pane confirms nothing");

        // And the real one settles it.
        let sel = st.active_tab().focused;
        let uid = st.active_tab().panes[sel].uid.clone();
        st.note_pane_focus(&uid, true);
        let settled = seq(&st);
        st.sync_pane_keyboard_focus();
        assert_eq!(seq(&st), settled);

        // A pane that does NOT hold it cannot give it up on another's behalf.
        st.note_pane_focus("a", false);
        assert_eq!(st.kbd_focus_live.as_deref(), Some(uid.as_str()));
        st.note_pane_focus(&uid, false);
        assert_eq!(st.kbd_focus_live, None);
    }
}

#[cfg(test)]
mod close_history_tests {
    //! The × on a pane or a tab: what it asks, and what it takes back.
    //!
    //! Two rules carry the whole feature and are pinned here. **A close is reversible** —
    //! the pane/tab is parked with its shell still running, so it must stay in
    //! `session_uids` (the window's kill list) until it is reopened or discarded; anything
    //! else either leaks a PTY or kills a session the history still offers. And **the one
    //! close nothing can undo asks anyway** — the last pane of the last tab ends the
    //! window, so the preference must not be able to silence it.
    use super::*;

    fn fresh() -> State {
        let mut st = State::new(theme::load_font(1.0));
        // `State::new` reads the real user prefs; the confirm tests set this explicitly so
        // they say what they mean regardless of what is on this machine's disk.
        st.settings.confirm_close = true;
        st
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: uid.into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    /// A window of `tabs` tabs holding the given uids, last tab active.
    fn window(m: &SessionManager, tabs: &[&[&str]]) -> State {
        let mut st = fresh();
        for (i, uids) in tabs.iter().enumerate() {
            for (j, u) in uids.iter().enumerate() {
                if i > 0 && j == 0 {
                    st.adopt_pane_as_tab(m, det(u));
                } else {
                    st.adopt_pane(m, det(u));
                }
            }
        }
        st
    }

    fn laid_out(st: &State) -> Vec<String> {
        st.tabs
            .iter()
            .flat_map(|t| t.panes.iter().map(|p| p.uid.to_string()))
            .collect()
    }

    /// A view pane is read-only and has no session behind it (D3): there is no work in
    /// flight for the card to warn about, so the × just closes it. The close is still
    /// reversible — it goes on the same history as any other.
    #[test]
    fn closing_a_read_only_pane_asks_nothing() {
        let m = mgr();
        let mut st = fresh();
        for u in ["view-a", "view-b"] {
            st.adopt_pane(
                &m,
                DetachedPane {
                    kind: PaneKind::Markdown,
                    ..det(u)
                },
            );
        }

        assert!(st.request_close_pane(0, 0, &m), "the window lives on");
        assert!(
            !matches!(st.overlay, Overlay::ConfirmClose),
            "a read-only pane must not raise the confirm card"
        );
        assert_eq!(laid_out(&st), ["view-b"]);
        assert_eq!(st.closed.len(), 1, "and it is still reopenable");
    }

    /// The exception: the last pane of the last tab ends the WINDOW. Being read-only says
    /// nothing about that close, so the card comes up as it would for any other pane.
    #[test]
    fn the_last_read_only_pane_still_asks_because_it_quits() {
        let m = mgr();
        let mut st = fresh();
        st.adopt_pane(
            &m,
            DetachedPane {
                kind: PaneKind::Markdown,
                ..det("view-only")
            },
        );

        assert!(st.request_close_pane(0, 0, &m));
        assert!(
            matches!(st.overlay, Overlay::ConfirmClose),
            "the close that ends the window asks whatever the pane is"
        );
        assert_eq!(laid_out(&st), ["view-only"], "and nothing closed yet");
    }

    #[test]
    fn closing_a_pane_parks_it_instead_of_killing_it() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"]]);

        assert!(st.close_pane_menu(0, 0, &m), "the window lives on");
        assert_eq!(laid_out(&st), ["a1"]);
        assert_eq!(st.closed.len(), 1);
        // Still this window's responsibility: its PTY is running off-layout.
        assert!(
            st.session_uids().iter().any(|u| u == "a0"),
            "a parked pane stays on the window's kill list"
        );

        st.reopen_closed(&m);
        assert!(st.closed.is_empty());
        assert!(laid_out(&st).contains(&"a0".to_string()), "it came back");
    }

    /// Panes and tabs share ONE ordered history — with two stacks, "reopen the last thing I
    /// closed" would be unanswerable, and closing a pane after a tab would strand the tab.
    #[test]
    fn panes_and_tabs_share_one_ordered_history() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0", "b1"]]);

        st.close_tab_menu(0, &m); // the tab first…
        st.close_pane_menu(st.active, 0, &m); // …then a pane, which must come back first
        assert_eq!(st.closed.len(), 2);
        assert!(st.closed[0].is_tab(), "the tab is the older entry");
        assert!(!st.closed[1].is_tab(), "the pane is the newer entry");

        st.reopen_closed(&m);
        assert_eq!(st.tabs.len(), 1, "the pane came back, not the tab");
        assert!(laid_out(&st).contains(&"b0".to_string()));
        st.reopen_closed(&m);
        assert_eq!(st.tabs.len(), 2, "and the tab is still reachable");
        assert!(st.closed.is_empty());
    }

    /// A row click names its entry by id, never by position: a close landing (push) or an
    /// eviction (shift) between the frame the human saw and the click arriving must not
    /// restore something else.
    #[test]
    fn a_history_row_is_restored_by_id_not_by_index() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1", "a2"]]);

        st.close_pane_menu(0, 0, &m); // a0
        let wanted = st.closed[0].id;
        st.close_pane_menu(0, 0, &m); // a1 — pushes on top, shifting nothing the human saw

        st.restore_closed(wanted, &m);
        assert!(laid_out(&st).contains(&"a0".to_string()), "the clicked row");
        assert_eq!(st.closed.len(), 1);
        assert_eq!(st.closed[0].label(), "a1");

        // An id that is already gone is a no-op, not a panic or a wrong restore.
        st.restore_closed(wanted, &m);
        assert_eq!(st.closed.len(), 1);
    }

    #[test]
    fn discarding_a_row_drops_it_for_good() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"]]);
        st.close_pane_menu(0, 0, &m);
        let id = st.closed[0].id;

        st.discard_closed(id, &m);
        assert!(st.closed.is_empty());
        assert!(
            !st.session_uids().iter().any(|u| u == "a0"),
            "a discarded entry is no longer held alive"
        );
    }

    /// The history is a live-session budget, not a log: the cap has to evict, and eviction
    /// is where those sessions actually die.
    #[test]
    fn the_history_is_capped() {
        let m = mgr();
        let uids: Vec<String> = (0..CLOSED_STACK_CAP + 3).map(|i| format!("p{i}")).collect();
        let refs: Vec<&str> = uids.iter().map(|s| s.as_str()).collect();
        let mut st = window(&m, &[&refs]);

        for _ in 0..CLOSED_STACK_CAP + 2 {
            st.close_pane_menu(0, 0, &m);
        }
        assert_eq!(st.closed.len(), CLOSED_STACK_CAP);
        assert_eq!(st.closed[0].label(), "p2", "the two oldest were evicted");
    }

    #[test]
    fn the_x_asks_first_and_the_answer_does_the_closing() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"]]);

        assert!(st.request_close_pane(0, 0, &m));
        assert!(matches!(st.overlay, Overlay::ConfirmClose));
        assert_eq!(laid_out(&st), ["a0", "a1"], "nothing closed on the ask");
        assert_eq!(st.pending_close.as_ref().unwrap().title, "a0");
        assert!(!st.pending_close.as_ref().unwrap().final_close);

        assert!(st.confirm_close_go(&m));
        assert!(matches!(st.overlay, Overlay::None));
        assert!(st.pending_close.is_none());
        assert_eq!(laid_out(&st), ["a1"]);
        assert_eq!(st.closed.len(), 1);
    }

    /// Dismissing the card by any route — Esc, Cancel, the scrim — IS cancelling.
    #[test]
    fn dismissing_the_card_cancels_the_close() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"]]);

        st.request_close_pane(0, 0, &m);
        st.close_overlay();
        assert!(
            st.pending_close.is_none(),
            "the close was dropped, not held"
        );
        assert_eq!(laid_out(&st), ["a0", "a1"]);
        assert!(st.closed.is_empty());
    }

    /// The card holds a session uid, not an index. A background shell exiting while the
    /// question is on screen renumbers everything; re-resolving is what stops the answer
    /// from landing on the wrong pane.
    #[test]
    fn a_target_that_moved_is_still_the_right_one() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1", "a2"]]);

        st.request_close_pane(0, 2, &m); // asking about a2…
        st.close_pane_in(0, 0, &m); // …while a0 exits underneath, shifting a2 to index 1

        st.confirm_close_go(&m);
        assert_eq!(
            laid_out(&st),
            ["a1"],
            "a2 closed, not whatever is at index 2"
        );
    }

    #[test]
    fn a_target_that_vanished_closes_nothing() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"]]);

        st.request_close_pane(0, 0, &m);
        st.close_pane_in(0, 0, &m); // the shell exited on its own first

        assert!(st.confirm_close_go(&m), "the window lives");
        assert_eq!(
            laid_out(&st),
            ["a1"],
            "the survivor was not closed in its place"
        );
    }

    /// The preference only ever governs the closes the history can undo.
    #[test]
    fn the_preference_skips_only_the_undoable_closes() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"]]);
        st.settings.confirm_close = false;

        assert!(st.request_close_pane(0, 0, &m));
        assert!(matches!(st.overlay, Overlay::None), "no card");
        assert_eq!(st.closed.len(), 1, "it closed straight through");

        // …but the last pane of the last tab ends the window, and nothing brings that back.
        assert!(st.request_close_pane(0, 0, &m), "asked, so still alive");
        assert!(matches!(st.overlay, Overlay::ConfirmClose));
        assert!(st.pending_close.as_ref().unwrap().final_close);
        assert!(
            !st.confirm_close_go(&m),
            "confirming the last pane quits the window"
        );
    }

    #[test]
    fn closing_the_last_tab_asks_even_with_confirmations_off() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0"]]);
        st.settings.confirm_close = false;

        st.request_close_tab(0, &m); // two tabs → undoable, goes straight through
        assert!(matches!(st.overlay, Overlay::None));
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.closed.len(), 1);

        st.request_close_tab(0, &m); // the last one ends the window
        assert!(matches!(st.overlay, Overlay::ConfirmClose));
        assert!(st.pending_close.as_ref().unwrap().final_close);
        assert_eq!(st.tabs.len(), 1, "nothing closed on the ask");
    }

    /// The card names what it is closing — a confirmation that doesn't is just a speed bump.
    #[test]
    fn a_multi_pane_tab_says_how_much_is_going() {
        let m = mgr();
        let mut st = window(&m, &[&["a0", "a1"], &["b0"]]);
        st.request_close_tab(0, &m);
        let title = st.pending_close.as_ref().unwrap().title.to_string();
        assert!(title.ends_with("— 2 panes"), "{title}");
    }

    /// Only the RECENTLY CLOSED section may be open at a time — same rule the bell and the
    /// projects flyout already follow.
    #[test]
    fn opening_the_history_collapses_the_other_rail_sections() {
        let mut st = fresh();
        st.sidebar_open = true;
        st.reminders_open = true;

        st.toggle_closed();
        assert!(st.closed_open && !st.sidebar_open && !st.reminders_open);

        st.toggle_reminders();
        assert!(!st.closed_open, "the bell takes the rail back");
    }

    #[test]
    fn ages_read_as_a_human_would_say_them() {
        assert_eq!(rel_age(30_000, 0), "just now");
        assert_eq!(rel_age(90_000, 0), "1m ago");
        assert_eq!(rel_age(7_200_000, 0), "2h ago");
        assert_eq!(rel_age(172_800_000, 0), "2d ago");
        assert_eq!(rel_age(0, 5_000), "just now", "a clock that went backwards");
    }
}

#[cfg(test)]
mod system_tab_pin_tests {
    //! The always-on "Hyperpane" tab is pinned to slot 0 — it is the tab every other tab is
    //! driven from, so it has a fixed home the way a browser's pinned tab does. Three rules,
    //! all enforced here rather than only in the UI (the control plane and the keyboard both
    //! reach `reorder_tab` without going near a drag): it starts at the front, it can't be
    //! dragged out of the front, and nothing can be dropped in front of it.
    use super::*;

    /// A tab strip of `(title, system)`, with no panes and no sessions — enough for the
    /// ordering rules, which never look inside a tab.
    fn strip(tabs: &[(&str, bool)]) -> State {
        let mut s = State::new(theme::load_font(1.0));
        s.tabs.clear();
        for (title, system) in tabs {
            let mut t = s.fresh_tab();
            t.title = (*title).into();
            t.system = *system;
            s.tabs.push(t);
        }
        s
    }

    fn titles(s: &State) -> Vec<String> {
        s.tabs.iter().map(|t| t.title.to_string()).collect()
    }

    #[test]
    fn pinning_moves_the_system_tab_to_the_front() {
        let mut s = strip(&[("a", false), ("b", false), ("hp", true), ("c", false)]);
        s.pin_system_tab_first();
        assert_eq!(titles(&s), ["hp", "a", "b", "c"]);
    }

    #[test]
    fn pinning_keeps_you_looking_at_the_same_tab() {
        // Every position relative to the tab being moved: before it, it, and after it.
        for (active, expect) in [(0, "a"), (1, "b"), (2, "hp"), (3, "c")] {
            let mut s = strip(&[("a", false), ("b", false), ("hp", true), ("c", false)]);
            s.active = active;
            s.pin_system_tab_first();
            assert_eq!(
                s.tabs[s.active].title.to_string(),
                expect,
                "active was {active}"
            );
        }
    }

    #[test]
    fn pinning_a_strip_that_is_already_pinned_or_has_no_system_tab_does_nothing() {
        let mut s = strip(&[("hp", true), ("a", false)]);
        s.active = 1;
        s.pin_system_tab_first();
        assert_eq!(titles(&s), ["hp", "a"]);
        assert_eq!(s.active, 1);

        let mut s = strip(&[("a", false), ("b", false)]);
        s.pin_system_tab_first();
        assert_eq!(titles(&s), ["a", "b"]);
    }

    #[test]
    fn the_system_tab_cannot_be_dragged_out_of_slot_zero() {
        let mut s = strip(&[("hp", true), ("a", false), ("b", false)]);
        s.reorder_tab(0, 3);
        assert_eq!(
            titles(&s),
            ["hp", "a", "b"],
            "the drag must simply not take"
        );
    }

    #[test]
    fn nothing_can_be_dropped_in_front_of_the_system_tab() {
        let mut s = strip(&[("hp", true), ("a", false), ("b", false)]);
        // A drop at the very front lands *after* the pin instead — which for "b" is a real
        // move, and for "a" is no move at all.
        s.reorder_tab(2, 0);
        assert_eq!(titles(&s), ["hp", "b", "a"]);
        s.reorder_tab(2, 0);
        assert_eq!(titles(&s), ["hp", "a", "b"]);
    }

    #[test]
    fn ordinary_tabs_still_reorder_normally() {
        let mut s = strip(&[("hp", true), ("a", false), ("b", false), ("c", false)]);
        s.active = 1;
        s.reorder_tab(1, 4);
        assert_eq!(titles(&s), ["hp", "b", "c", "a"]);
        assert_eq!(s.tabs[s.active].title.to_string(), "a");
    }
}

#[cfg(test)]
mod hyperpane_uniqueness_tests {
    //! The Hyperpane tab is app-owned: it can never be closed, and exactly one may exist. Two
    //! halves enforce that. *Save side* — it is left out of every file the user can write
    //! (workspace, library, set, repo project), because a file describing it could only ever
    //! be a second one waiting to be opened. *Load side* — a group describing it is skipped
    //! outright when one is already open, which is also the only thing that repairs the files
    //! already on disk from before this rule existed.
    use super::*;

    fn fresh() -> State {
        State::new(crate::theme::load_font(1.0))
    }

    fn titles(s: &State) -> Vec<String> {
        s.tabs.iter().map(|t| t.title.to_string()).collect()
    }

    fn mgr() -> SessionManager {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        SessionManager::new(tx)
    }

    fn det(uid: &str) -> DetachedPane {
        DetachedPane {
            uid: uid.into(),
            title: uid.into(),
            subtitle: None,
            pinned_accent: None,
            show_frame: None,
            show_dot: None,
            font_px: 14.0,
            spawn_command: None,
            spawn_args: None,
            spawn_shell: None,
            kind: PaneKind::default(),
            tool_session: None,
            cwd: None,
        }
    }

    /// A window of `(title, system)` tabs, each holding one pane —
    /// `library_workspace_of_tab` skips a 0-pane tab for an unrelated reason, so a test about
    /// the system flag has to give every tab something to describe.
    fn strip(tabs: &[(&str, bool)]) -> State {
        let mut s = fresh();
        let m = mgr();
        // Drop the empty starter tab so slot 0 is the first tab asked for.
        s.tabs.clear();
        s.active = 0;
        for (title, system) in tabs {
            s.adopt_pane_as_tab(&m, det(title));
            let last = s.tabs.len() - 1;
            s.tabs[last].title = (*title).into();
            s.tabs[last].system = *system;
        }
        s.active = 0;
        s
    }

    /// A saved group for the Hyperpane as the CURRENT writer would record it.
    fn flagged_group() -> GroupSpec {
        GroupSpec {
            title: Some("Hyperpane".into()),
            panes: vec![PaneSpec {
                ..Default::default()
            }],
            system: Some(true),
            ..Default::default()
        }
    }

    /// …and as files written before the `system` flag existed recorded it: no flag, but every
    /// pane sitting in the Hyperpane directory.
    fn legacy_group() -> GroupSpec {
        GroupSpec {
            title: Some("Hyperpane".into()),
            panes: vec![PaneSpec {
                cwd: Some(paths::hyperpane_dir().to_string_lossy().into_owned()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_set_leaves_the_hyperpane_out() {
        let s = strip(&[("Hyperpane", true), ("work", false)]);
        assert!(
            s.library_workspace_of_tab(0).is_none(),
            "the pinned Hyperpane is not a saveable member"
        );
        assert!(
            s.library_workspace_of_tab(1).is_some(),
            "an ordinary tab still saves"
        );
    }

    #[test]
    fn saving_the_hyperpane_as_a_workspace_is_refused_with_a_reason() {
        let mut s = strip(&[("Hyperpane", true), ("work", false)]);
        s.active = 0;
        assert!(s.refuse_saving_system_tab(), "refused while it is active");
        s.active = 1;
        assert!(
            !s.refuse_saving_system_tab(),
            "an ordinary active tab saves normally"
        );
    }

    #[test]
    fn a_flagged_group_is_recognized_as_the_hyperpane() {
        assert!(group_is_hyperpane(&flagged_group()));
    }

    /// The legacy shape matters more than the flagged one: the duplicates already sitting in
    /// `last-workspace.json` and in saved sets were written before the flag existed, so the
    /// flag alone would never clean them up.
    #[test]
    fn a_legacy_flagless_group_in_the_hyperpane_directory_is_recognized_too() {
        assert!(group_is_hyperpane(&legacy_group()));
    }

    #[test]
    fn an_ordinary_group_is_not_mistaken_for_it() {
        let g = GroupSpec {
            title: Some("Hyperpane".into()),
            panes: vec![PaneSpec {
                cwd: Some("/Users/someone/code".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            !group_is_hyperpane(&g),
            "a same-named tab elsewhere on disk is just a tab"
        );
        assert!(
            !group_is_hyperpane(&GroupSpec::default()),
            "an empty group describes nothing"
        );
    }

    /// Opening a file that carries a Hyperpane while one is already open must not add a
    /// second — that is the bug: a set saved with the Hyperpane in it re-attached the same
    /// live session into a duplicate tab on every re-open.
    #[test]
    fn loading_a_file_that_carries_a_hyperpane_never_adds_a_second() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        for g in [flagged_group(), legacy_group()] {
            let mut s = strip(&[("Hyperpane", true), ("work", false)]);
            let before = s.tabs.len();
            s.load_workspace(
                WorkspaceFile {
                    groups: Some(vec![g]),
                    ..Default::default()
                },
                &mgr(),
            );
            assert_eq!(
                s.tabs.len(),
                before,
                "a second Hyperpane must never be appended"
            );
        }
    }

    /// …but the restore path still works: with no Hyperpane open yet, the saved one loads.
    /// Crash recovery depends on this — `last-workspace.json` keeps it so its conversation
    /// resumes where it was.
    #[test]
    fn a_saved_hyperpane_still_restores_when_none_is_open() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        let mut s = fresh();
        s.load_workspace(
            WorkspaceFile {
                groups: Some(vec![flagged_group()]),
                ..Default::default()
            },
            &mgr(),
        );
        assert_eq!(
            s.tabs.iter().filter(|t| t.system).count(),
            1,
            "the saved Hyperpane restores, and restores as the system tab"
        );
    }

    /// A file written before the flag existed must come back FLAGGED, not as an ordinary tab.
    /// Otherwise `ensure_hyperpane_tab` finds no system tab on the next launch and appends a
    /// second Hyperpane — which is exactly how the duplicates on disk were made.
    #[test]
    fn a_legacy_hyperpane_restores_as_the_system_tab() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _guard = rt.enter();
        // The real directory, so the pane can actually spawn there.
        let dir = paths::hyperpane_dir();
        let _ = std::fs::create_dir_all(&dir);
        let mut s = fresh();
        s.load_workspace(
            WorkspaceFile {
                groups: Some(vec![legacy_group()]),
                ..Default::default()
            },
            &mgr(),
        );
        assert_eq!(
            s.tabs.iter().filter(|t| t.system).count(),
            1,
            "a flagless Hyperpane is recognized and re-flagged on the way in"
        );
    }

    /// A one-tab window of `(title, system)` tabs as a moveable unit.
    fn det_tab(title: &str, panes: &[&str]) -> DetachedTab {
        DetachedTab {
            title: title.into(),
            layout: Layout::Auto,
            sizes: Vec::new(),
            main_fraction: 0.5,
            focused: 0,
            zoomed: None,
            panes: panes.iter().map(|p| det(p)).collect(),
        }
    }

    /// Pty spawns (the reseed shell) need a runtime handle on the calling thread.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
    }

    #[test]
    fn closing_the_avada_only_pane_leaves_a_system_tab_at_index_0() {
        let rt = runtime();
        let _guard = rt.enter();
        let m = mgr();
        let mut s = strip(&[("Hyperpane", true), ("work", false)]);
        assert!(s.close_pane_in(0, 0, &m), "the window stays open");
        assert_eq!(s.tabs.len(), 2, "the tab was kept, not dropped");
        assert!(
            s.tabs[0].system,
            "and it is still the system tab at index 0"
        );
        assert_eq!(
            s.tabs[0].panes.len(),
            1,
            "an emptied Hyperpane is reseeded with a shell"
        );
        assert!(!s.tabs[1].system);
    }

    #[test]
    fn moving_the_avada_only_pane_to_another_tab_keeps_the_system_tab() {
        let rt = runtime();
        let _guard = rt.enter();
        let m = mgr();
        let mut s = strip(&[("Hyperpane", true), ("work", false)]);
        s.active = 0;
        s.move_pane_to_tab(0, 1, &m);
        assert_eq!(s.tabs.len(), 2);
        assert!(s.tabs[0].system, "the Hyperpane survives its pane leaving");
        assert_eq!(s.tabs[0].panes.len(), 1, "reseeded");
        assert_eq!(
            s.tabs[1].panes.len(),
            2,
            "the pane arrived in the target tab"
        );
    }

    #[test]
    fn the_hyperpane_cannot_be_detached_but_an_ordinary_tab_can() {
        let mut s = strip(&[("Hyperpane", true), ("work", false)]);
        assert!(s.detach_tab(0).is_none(), "Move to New Window is refused");
        assert_eq!(titles(&s), ["Hyperpane", "work"]);
        assert!(s.detach_tab(1).is_some());
        assert_eq!(titles(&s), ["Hyperpane"]);
    }

    #[test]
    fn a_tab_adopted_into_a_new_window_is_never_the_system_tab() {
        let m = mgr();
        let mut s = fresh();
        // Even if the seed tab were somehow flagged, the moved tab arrives ordinary.
        s.tabs[0].system = true;
        s.adopt_tab(&m, det_tab("Hyperpane", &["p"]));
        assert!(!s.tabs[0].system);
        assert_eq!(s.tabs[0].panes.len(), 1);
    }

    #[test]
    fn a_closing_windows_hyperpane_is_rehomed_at_index_0_of_the_survivor() {
        let m = mgr();
        let mut closing = strip(&[("Hyperpane", true), ("work", false)]);
        let mut survivor = strip(&[("other", false), ("more", false)]);
        survivor.active = 1;

        let det = closing
            .take_system_tab()
            .expect("the closing window held it");
        assert_eq!(titles(&closing), ["work"]);
        assert!(!closing.tabs.iter().any(|t| t.system));
        assert!(survivor.take_system_tab().is_none(), "nothing to take here");

        survivor.adopt_system_tab(&m, det);
        assert_eq!(titles(&survivor), ["Hyperpane", "other", "more"]);
        assert!(survivor.tabs[0].system, "flag intact, pinned first");
        assert_eq!(survivor.active, 2, "the user's tab stays selected");

        // A second arrival can never make two: it lands as an ordinary tab.
        survivor.adopt_system_tab(&m, det_tab("Hyperpane", &["x"]));
        assert_eq!(survivor.tabs.iter().filter(|t| t.system).count(), 1);
        assert_eq!(survivor.tabs.len(), 4);
    }

    #[test]
    fn loading_a_workspace_pins_the_hyperpane_first_and_demotes_duplicates() {
        let rt = runtime();
        let _guard = rt.enter();
        let m = mgr();
        // Two flagged tabs (a hand-edited file could produce this), neither at index 0.
        let mut s = strip(&[("work", false), ("A", true), ("B", true)]);
        let landed = s.load_workspace(
            WorkspaceFile {
                groups: Some(vec![GroupSpec {
                    title: Some("restored".into()),
                    panes: vec![PaneSpec::default()],
                    ..Default::default()
                }]),
                ..Default::default()
            },
            &m,
        );
        assert!(landed.is_some());
        assert_eq!(s.tabs.iter().filter(|t| t.system).count(), 1);
        assert!(s.tabs[0].system, "pinned to index 0");
        assert_eq!(titles(&s), ["A", "work", "B", "restored"]);
        assert_eq!(
            s.active, 3,
            "landed on the restored tab, index fixed by the pin"
        );
    }

    #[test]
    fn a_settings_patch_applies_a_known_key_and_collapses_the_sidebar() {
        let mut s = fresh();
        s.settings.show_sidebar = true;
        s.sidebar_open = true;
        let keys = s
            .apply_settings_patch(&serde_json::json!({ "showSidebar": false }))
            .expect("a known key with the right type");
        assert_eq!(keys, ["showSidebar"]);
        assert!(!s.settings.show_sidebar);
        assert!(
            !s.sidebar_open,
            "hiding the sidebar also closes it, as the toggle does"
        );
        assert!(s.dirty);
    }

    #[test]
    fn a_settings_patch_rejects_an_unknown_key() {
        let mut s = fresh();
        let err = s
            .apply_settings_patch(&serde_json::json!({ "noSuchSetting": 1 }))
            .unwrap_err();
        assert!(err.contains("unknown setting"), "{err}");
    }

    #[test]
    fn a_settings_patch_rejects_a_wrong_type() {
        let mut s = fresh();
        let before = s.settings.font_px;
        let err = s
            .apply_settings_patch(&serde_json::json!({ "fontPx": "big" }))
            .unwrap_err();
        assert!(err.contains("bad settings patch"), "{err}");
        assert_eq!(s.settings.font_px, before, "nothing applied");
    }

    #[test]
    fn a_settings_patch_rejects_an_unknown_log_level() {
        let mut s = fresh();
        let before = s.settings.log_level.clone();
        let err = s
            .apply_settings_patch(&serde_json::json!({ "logLevel": "loud" }))
            .unwrap_err();
        assert!(err.contains("log level"), "{err}");
        assert_eq!(s.settings.log_level, before);
        assert!(s
            .apply_settings_patch(&serde_json::json!({ "logLevel": "debug" }))
            .is_ok());
    }

    /// `scrollback` used to be persisted and ignored; now every grid is built at the setting's
    /// depth and a patch resizes the live ones without a restart.
    #[tokio::test]
    async fn a_scrollback_patch_resizes_every_live_pane() {
        let m = mgr();
        let mut s = fresh();
        s.add_pane(&m);
        s.add_pane(&m);
        let before = s.settings.scrollback as usize;
        for p in &s.active_tab().panes {
            assert_eq!(
                p.pane.scrollback_capacity(),
                before,
                "built at the setting's depth"
            );
        }
        s.apply_settings_patch(&serde_json::json!({ "scrollback": 123 }))
            .unwrap();
        assert_eq!(s.settings.scrollback, 123);
        for p in &s.active_tab().panes {
            assert_eq!(p.pane.scrollback_capacity(), 123);
        }
        // Beyond alacritty's ceiling is a refused patch, and the live grids are untouched.
        let err = s
            .apply_settings_patch(&serde_json::json!({ "scrollback": 1_000_000 }))
            .unwrap_err();
        assert!(err.contains("scrollback"), "{err}");
        assert_eq!(s.settings.scrollback, 123);
    }

    /// Rules the panel cannot break (a palette index past the end, a loop cadence of a
    /// year) are still rules for a JSON caller.
    #[test]
    fn a_settings_patch_is_checked_by_the_shared_validator() {
        let mut s = fresh();
        let err = s
            .apply_settings_patch(&serde_json::json!({ "restartLoopHours": 100_000 }))
            .unwrap_err();
        assert!(err.contains("restartLoopHours"), "{err}");
        let err = s
            .apply_settings_patch(&serde_json::json!({ "statusLoopPrompt": "two\nlines" }))
            .unwrap_err();
        assert!(err.contains("statusLoopPrompt"), "{err}");
        assert!(s
            .apply_settings_patch(&serde_json::json!({ "statusLoopPrompt": "check the panes" }))
            .is_ok());
    }

    #[test]
    fn resume_startup_line_has_both_shapes() {
        assert_eq!(
            resume_startup_line(
                Some("/w/p"),
                "CLAUDE_CONFIG_DIR='/c' ",
                "claude",
                "--resume abc"
            ),
            "cd '/w/p' && CLAUDE_CONFIG_DIR='/c' claude --resume abc\r"
        );
        assert_eq!(
            resume_startup_line(None, "", "claude", "--resume abc"),
            "claude --resume abc\r"
        );
        assert_eq!(
            resume_startup_line(Some("/w"), "", "codex", "resume 1234"),
            "cd '/w' && codex resume 1234\r"
        );
    }

    // ---- project roots pushed into panes (track L1) ----

    /// A scratch project holding `docs/x.md`, next to an `elsewhere` dir for the pane to
    /// stand in. Not a git repo, so the repository fallback has nothing to say and the
    /// remembered root is the only witness.
    fn root_fixture(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hp-state-roots-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proj/docs")).expect("scratch project");
        std::fs::write(dir.join("proj/docs/x.md"), "# x\n").expect("scratch file");
        std::fs::create_dir_all(dir.join("elsewhere")).expect("scratch elsewhere");
        dir
    }

    fn project_at(dir: &std::path::Path) -> Project {
        Project {
            id: "p1".into(),
            path: dir.to_string_lossy().into_owned(),
            name: "proj".into(),
            color: "#3b82f6".into(),
            last_opened_at: Some(1),
        }
    }

    /// A view pane: no pty, no runtime, but a real `TerminalPane` behind it.
    fn view_opts(kind: PaneKind) -> NewPaneOpts {
        NewPaneOpts {
            kind: Some(kind),
            ..Default::default()
        }
    }

    /// Feed `docs/x.md` into a pane standing in `elsewhere` and hover it: the only way
    /// the relative token lights is through the roots the app handed the pane.
    fn hover_docs_x(pane: &mut TerminalPane, elsewhere: &std::path::Path) -> Option<String> {
        pane.set_cwd(Some(elsewhere.to_string_lossy().into_owned()));
        pane.feed("see docs/x.md\r\n");
        let (cols, rows) = pane.grid_size();
        let at = "see ".len() as f32 + 0.5;
        pane.link_at(at, 0.5, cols as f32, rows as f32)
            .map(|h| h.abs_path)
    }

    #[test]
    fn the_roots_are_the_projects_paths_in_list_order() {
        let a = project_at(std::path::Path::new("/a"));
        let mut b = project_at(std::path::Path::new("/b"));
        b.id = "p2".into();
        assert_eq!(
            State::roots_of(&[a, b]),
            vec!["/a".to_string(), "/b".to_string()]
        );
        assert!(State::roots_of(&[]).is_empty());
    }

    #[test]
    fn pushing_projects_hands_every_pane_the_roots() {
        let dir = root_fixture("push");
        let proj = dir.join("proj");
        let m = mgr();
        let mut st = fresh();
        // Two view panes (no pty, no runtime) across two tabs, so the sweep has to cross a
        // tab boundary to reach the second one.
        st.add_pane_opts(&m, view_opts(PaneKind::FileBrowser));
        st.adopt_pane_as_tab(&m, det("second-tab"));
        assert!(st.tabs.len() >= 2, "expected a second tab");

        // Before the push: dark in every pane.
        for t in &mut st.tabs {
            for p in &mut t.panes {
                assert_eq!(hover_docs_x(&mut p.pane, &dir.join("elsewhere")), None);
            }
        }

        // The list changes (set directly: the real store must not be touched by a test)
        // and the sweep hands the roots to every pane.
        st.projects = vec![project_at(&proj)];
        st.push_project_roots();
        let mut seen = 0;
        for t in &mut st.tabs {
            for p in &mut t.panes {
                let hit = hover_docs_x(&mut p.pane, &dir.join("elsewhere"))
                    .expect("every pane should have the roots after the push");
                assert_eq!(std::path::Path::new(&hit), proj.join("docs/x.md"));
                seen += 1;
            }
        }
        assert!(
            seen >= 2,
            "the sweep should have reached at least two panes"
        );

        // The project is forgotten: the pane must stop answering.
        st.projects.clear();
        st.push_project_roots();
        for t in &mut st.tabs {
            for p in &mut t.panes {
                assert_eq!(
                    hover_docs_x(&mut p.pane, &dir.join("elsewhere")),
                    None,
                    "a root that left the list must stop answering"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_pane_starts_with_the_current_roots() {
        let dir = root_fixture("new-pane");
        let proj = dir.join("proj");
        let m = mgr();
        let mut st = fresh();
        st.projects = vec![project_at(&proj)];

        // A pane added AFTER the list was loaded, with no reload in between: it has to be
        // handed the roots at construction or it would be dark until the next `cd`.
        let uid = st
            .add_pane_opts(&m, view_opts(PaneKind::FileBrowser))
            .expect("view pane added");
        let p = st
            .tabs
            .iter_mut()
            .flat_map(|t| t.panes.iter_mut())
            .find(|p| p.uid == uid)
            .expect("the new pane");
        let hit = hover_docs_x(&mut p.pane, &dir.join("elsewhere"))
            .expect("a new pane should carry the current roots");
        assert_eq!(std::path::Path::new(&hit), proj.join("docs/x.md"));

        // And one adopted from another window goes through the same door.
        st.adopt_pane_as_tab(&m, det("adopted"));
        let last = st.tabs.len() - 1;
        let p = &mut st.tabs[last].panes[0];
        let hit = hover_docs_x(&mut p.pane, &dir.join("elsewhere"))
            .expect("an adopted pane should carry the current roots");
        assert_eq!(std::path::Path::new(&hit), proj.join("docs/x.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod module_keymap_routing_tests {
    //! One Preferences editor, two stores. A row id carrying a `module:` prefix belongs to
    //! a module's own keymap and its own file; a bare one is the app's. Every entry point
    //! the editor calls has to route on that, or a user rebinding an editor's arrow keys
    //! would silently rewrite the app's.
    use super::*;
    use crate::module_ui::grid;
    use avada_core::module::grid::{DeclareKeymap, GridAction, KeymapPreset};
    use avada_core::rights::ModuleId;

    const MODULE: &str = "bshuler/avada-editor";
    /// An app binding that must never move when a module row is edited.
    const APP_ROW: &str = "pane.toggleZoom";

    fn id() -> ModuleId {
        ModuleId::new(MODULE).unwrap()
    }

    /// A fresh state whose module prefs file is a scratch file on this thread.
    fn fresh(tag: &str) -> State {
        let dir = std::env::temp_dir().join(format!("avada-route-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let path = dir.join("module-keymaps.json");
        let _ = std::fs::remove_file(&path); // last run's choices are not this run's
        grid::set_prefs_path(path);
        grid::set_keymap(
            &id(),
            DeclareKeymap {
                surface: "editor".into(),
                actions: vec![
                    GridAction {
                        id: "move.left".into(),
                        label: "Move left".into(),
                    },
                    GridAction {
                        id: "file.save".into(),
                        label: "Save".into(),
                    },
                ],
                presets: vec![
                    KeymapPreset {
                        name: "helix".into(),
                        label: "Helix".into(),
                        bindings: [("h".to_string(), "move.left".to_string())]
                            .into_iter()
                            .collect(),
                    },
                    KeymapPreset {
                        name: "vscode".into(),
                        label: "VS Code".into(),
                        bindings: [("arrowleft".to_string(), "move.left".to_string())]
                            .into_iter()
                            .collect(),
                    },
                ],
                default_preset: Some("helix".into()),
            },
        );
        State::new(crate::theme::load_font(1.0))
    }

    /// The chord that currently fires `action` on the editor surface.
    fn chord(action: &str) -> String {
        grid::binding_rows(&id(), "editor")
            .into_iter()
            .find(|(a, _, _)| a == action)
            .map(|(_, _, c)| c)
            .unwrap_or_else(|| panic!("{action} is a declared action"))
    }

    /// A capture on a module row writes the module store and leaves the app's keymap alone.
    #[test]
    fn a_captured_chord_lands_in_the_store_the_row_id_names() {
        let mut st = fresh("capture");
        assert_eq!(chord("move.left"), "h", "helix is the module's own default");

        st.begin_rebind(&grid::row_id(&id(), "editor", "move.left"));
        st.capture_chord(true, false, false, "b");
        assert_eq!(chord("move.left"), "ctrl+b");
        assert!(grid::overridden(&id(), "move.left"));
        assert!(
            !st.keymap.is_overridden(APP_ROW),
            "a module rebind must not touch an app binding"
        );
        assert_eq!(st.capturing_binding, None, "the capture closed");

        // The app's own rows still route to the app's keymap.
        st.begin_rebind(APP_ROW);
        st.capture_chord(true, true, false, "b");
        assert!(st.keymap.is_overridden(APP_ROW));
    }

    /// Unbind stores the choice; reset removes it. The difference is the whole point — an
    /// unbind that were merely dropped would come straight back as the preset's chord.
    #[test]
    fn an_unbind_is_stored_and_only_a_reset_puts_the_preset_back() {
        let mut st = fresh("unbind");
        let row = grid::row_id(&id(), "editor", "move.left");

        st.unbind_binding(&row);
        assert_eq!(chord("move.left"), "", "nothing fires it now");
        assert!(grid::overridden(&id(), "move.left"), "and that is a choice");

        st.reset_binding(&row);
        assert!(!grid::overridden(&id(), "move.left"));
        assert_eq!(chord("move.left"), "h", "back to what the module shipped");
    }

    /// "Reset all" clears both stores' rebinds — and neither store's preset, because a
    /// preset is a choice of dialect, not an override of one.
    #[test]
    fn reset_all_clears_every_rebind_and_no_dialect() {
        let mut st = fresh("resetall");
        st.begin_rebind(APP_ROW);
        st.capture_chord(true, true, false, "b");
        st.unbind_binding(&grid::row_id(&id(), "editor", "file.save"));
        st.set_module_preset(MODULE, "editor", Some("vscode"));
        assert!(st.keymap.is_overridden(APP_ROW) && grid::any_overridden());

        st.reset_all_bindings();
        assert!(
            !st.keymap.is_overridden(APP_ROW),
            "the app's rebinds are gone"
        );
        assert!(!grid::any_overridden(), "and so are every module's");
        assert_eq!(
            grid::preset_of(&id(), "editor").as_deref(),
            Some("vscode"),
            "the dialect the user chose is not a rebind and survives"
        );
    }

    /// A preset is set by identity, not by pane, so it outlives the pane it was chosen in —
    /// and an id that is not a module id is a no-op rather than a panic.
    #[test]
    fn a_preset_is_chosen_by_module_identity_and_a_bad_id_does_nothing() {
        let mut st = fresh("preset");
        assert_eq!(chord("move.left"), "h");

        st.set_module_preset(MODULE, "editor", Some("vscode"));
        assert_eq!(chord("move.left"), "arrowleft");
        st.set_module_preset(MODULE, "editor", None);
        assert_eq!(
            chord("move.left"),
            "h",
            "None restores the module's own default"
        );

        st.dirty = false;
        st.set_module_preset("not a module id", "editor", Some("vscode"));
        assert!(!st.dirty, "a malformed id changes nothing at all");
        assert_eq!(grid::preset_of(&id(), "editor"), None);
    }
}
