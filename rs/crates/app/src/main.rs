//! `hyperpanes` — the native Slint GUI (Phase 4, Wave 1: **multi-window**).
//!
//! This file is now a thin **bootstrap**: it owns the Tokio runtime + the one shared
//! [`session_manager::SessionManager`], creates the app-level [`app::App`] window
//! registry, spawns the first window, and starts the single 8 ms pump timer that drives
//! every window. All the interesting logic lives in the modules:
//!
//!   * [`app`]      — the **window registry** + central event drain + per-window wiring;
//!   * [`state`]    — one window's workspace state (tabs/panes/layout/zoom) and its
//!                    mutate-then-resync API (**Seam #1**);
//!   * [`command`]  — the `Command` enum + `dispatch` (**Seam #2**);
//!   * [`paneview`] — resync (State → Slint models) + the per-window render pump;
//!   * [`theme`]    — palette, layout metadata, font loading;
//!   * [`window`]   — Win32 frameless / fullscreen glue (per window).
//!
//! The `.slint` views carry an empty overlay slot (**Seam #3**) for Wave-2 panels.
//! See `ARCHITECTURE.md`. PTYs are owned centrally; a window only references pane uids,
//! so a pane can be re-hosted in any window (replay-primed, no PTY restart).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod ai;
mod app;
mod command;
mod contextmenu;
mod control_host;
mod drag;
mod glow;
mod keybindings;
mod palette;
mod paneview;
mod prefs;
mod sidebar;
mod state;
mod tetris;
mod theme;
mod update;
mod window;

use std::sync::Arc;
use std::time::Duration;

use hyperpanes_core::session_manager::{SessionEvent, SessionManager};

use slint::platform::Key;
use slint::SharedString;
use tokio::sync::mpsc::unbounded_channel;

use app::{App, PendingSeed};
use command::{dispatch, Command};
use state::State;

slint::include_modules!();

/// Append a line to the debug log when `HYPERPANES_DEBUG` is set. The path is
/// printed once at startup. Used to trace the divider/command paths.
pub fn dbg_log(msg: &str) {
    use std::io::Write;
    if std::env::var_os("HYPERPANES_DEBUG").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("hyperpanes-debug.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

/// Lightweight perf instrumentation for the Wave-2 perf track (Task 17). Enabled by setting
/// `HYPERPANES_PERFLOG` to a file path (or `1` / empty for a default temp path), so the
/// startup-latency (#2) and scroll-region-throughput (#1) work can be measured before/after
/// without an external profiler. Completely inert (one `OnceLock` load) when the env var is
/// unset, so it costs nothing in normal runs. Single UI thread, so the tick aggregates live
/// in a `thread_local`.
pub(crate) mod perf {
    use std::cell::RefCell;
    use std::io::Write;
    use std::sync::OnceLock;
    use std::time::Instant;

    static START: OnceLock<Instant> = OnceLock::new();
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

    /// Capture t0 + resolve the log path. Call once at the very top of `main`.
    pub fn init() {
        let _ = START.get_or_init(Instant::now);
        let _ = PATH.get_or_init(|| {
            std::env::var_os("HYPERPANES_PERFLOG").map(|v| {
                let s = v.to_string_lossy();
                if s.is_empty() || s == "1" {
                    std::env::temp_dir().join("hyperpanes-perf.log")
                } else {
                    std::path::PathBuf::from(s.as_ref())
                }
            })
        });
    }

    /// Whether perf logging is on (cheap — a resolved `OnceLock` load).
    #[inline]
    pub fn enabled() -> bool {
        matches!(PATH.get(), Some(Some(_)))
    }

    /// Milliseconds since [`init`].
    pub fn elapsed_ms() -> f64 {
        START.get().map(|s| s.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0)
    }

    fn write_line(line: &str) {
        if let Some(Some(p)) = PATH.get() {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Record a one-off timestamped milestone (used for the startup path, #2).
    pub fn mark(label: &str) {
        if !enabled() {
            return;
        }
        write_line(&format!("[+{:.1}ms] {label}", elapsed_ms()));
    }

    struct TickStats {
        ticks: u64,
        events: u64,
        bytes: u64,
        renders: u64,
        drain_ns: u128,
        render_ns: u128,
        tick_ns: u128,
        window: Option<Instant>,
    }
    impl TickStats {
        const fn zero() -> Self {
            TickStats {
                ticks: 0,
                events: 0,
                bytes: 0,
                renders: 0,
                drain_ns: 0,
                render_ns: 0,
                tick_ns: 0,
                window: None,
            }
        }
    }
    thread_local! {
        static TICK: RefCell<TickStats> = const { RefCell::new(TickStats::zero()) };
    }

    /// Accumulate one tick's work; flush a `[tick/s]` summary ~once a second while there is
    /// activity. `drain_ns` covers the session-event drain+feed, `render_ns` the per-window
    /// render pump, `tick_ns` the whole tick — so the summary shows the app's busy fraction
    /// (is the app the throughput bottleneck, or is it idle waiting on the pty?).
    pub fn tick(events: u64, bytes: u64, renders: u64, drain_ns: u128, render_ns: u128, tick_ns: u128) {
        if !enabled() {
            return;
        }
        TICK.with(|t| {
            let mut t = t.borrow_mut();
            if t.window.is_none() {
                t.window = Some(Instant::now());
            }
            t.ticks += 1;
            t.events += events;
            t.bytes += bytes;
            t.renders += renders;
            t.drain_ns += drain_ns;
            t.render_ns += render_ns;
            t.tick_ns += tick_ns;
            let elapsed = t.window.map(|w| w.elapsed()).unwrap_or_default();
            if elapsed.as_millis() >= 1000 && (t.events > 0 || t.renders > 0) {
                let secs = elapsed.as_secs_f64().max(1e-6);
                write_line(&format!(
                    "[tick/s] ticks={} events={} bytes={} ({:.2} MB/s) renders={} drain={:.1}ms/s render={:.1}ms/s busy={:.1}ms/s",
                    t.ticks,
                    t.events,
                    t.bytes,
                    (t.bytes as f64 / 1e6) / secs,
                    t.renders,
                    t.drain_ns as f64 / 1e6 / secs,
                    t.render_ns as f64 / 1e6 / secs,
                    t.tick_ns as f64 / 1e6 / secs,
                ));
                *t = TickStats { window: Some(Instant::now()), ..TickStats::zero() };
            }
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // t0 for the perf log (#2 startup) — must be the very first thing so every mark is
    // relative to process entry. Inert unless `HYPERPANES_PERFLOG` is set.
    perf::init();
    perf::mark("main: enter");

    // Capture any panic to a crash log (the windowed subsystem has no console).
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let path = std::env::temp_dir().join("hyperpanes-crash.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "PANIC: {info}");
            let bt = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(f, "{bt}");
        }
    }));

    // Extract the baked-in OFL fonts (Fira Code / JetBrains Mono) so they always resolve.
    crate::prefs::init_bundled_fonts();

    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let (etx, erx) = unbounded_channel::<SessionEvent>();
    let mgr = Arc::new(SessionManager::new(etx));

    // The app owns the window registry + the shared session stream.
    let application = App::new(mgr.clone(), erx);

    // Wire the launch seed: `hyperpanes -c "<cmd>" --shell … --cwd … --name …` (or a
    // positional workspace `.json`) seeds the first window from that spec; a bare launch
    // falls back to the LAST SESSION (`last-workspace.json`, written when the final window
    // closes — see `app::persist_last_session`), so tabs/layout/per-pane zoom survive a
    // plain relaunch (#14). A first-ever launch (no last-session file) stays an empty
    // shell pane.
    let argv: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let seed = match hyperpanes_core::workspace::launch::resolve_launch_workspace(&argv, &cwd) {
        Some(file) => PendingSeed::Workspace(Box::new(file)),
        None => PendingSeed::EmptyTab,
    };
    perf::mark("main: spawn_window begin");
    application.spawn_window(seed);
    perf::mark("main: spawn_window done");

    // If auto-update is on, do a quiet GitHub-releases check on startup. This runs on a
    // background thread inside `check`, so it never blocks startup; an offline/failed check
    // is silently skipped, and an available update only surfaces a hint in Preferences →
    // General (never auto-downloads/-installs). Reads the persisted setting directly so we
    // don't depend on a window's state being seeded yet.
    if prefs::load().auto_update {
        application.update.check(true);
    }

    // One shared pump timer drives every window (drain → render → reap). The interval is
    // ADAPTIVE (#3): it starts at the fast cadence and `App::tick` slows it to the idle
    // cadence after a stretch with no work, waking back to fast on input/output. The closure
    // holds a `Weak` so storing the `Timer` inside the `App` (so `tick` can re-interval it)
    // doesn't create a strong reference cycle.
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(app::TICK_FAST_MS),
        {
            let weak = std::rc::Rc::downgrade(&application);
            move || {
                if let Some(app) = weak.upgrade() {
                    app.tick();
                }
            }
        },
    );
    application.set_timer(timer); // App owns the timer for the whole loop + adjusts its interval

    slint::run_event_loop()?;
    mgr.kill_all();
    Ok(())
}

/// Seed a richer workspace (2 tabs, several panes, non-default layouts) so a
/// screenshot exercises the Wave-1 surface. Gated by `HYPERPANES_DEMO`.
pub(crate) fn demo_seed(st: &mut State, mgr: &SessionManager) {
    use hyperpanes_core::layout::presets::Layout;
    // tab 0: 3 panes in main-stack (shows the main divider + focus ring)
    dispatch(st, Command::NewPane, mgr);
    dispatch(st, Command::NewPane, mgr);
    dispatch(st, Command::SetLayout(Layout::MainStack), mgr);
    // tab 1: 2 panes in columns (a vertical divider)
    dispatch(st, Command::NewTab, mgr);
    dispatch(st, Command::NewPane, mgr);
    dispatch(st, Command::SetLayout(Layout::Columns), mgr);
    // land on tab 0
    dispatch(st, Command::SwitchTab(0), mgr);
}

/// Whether `text` is the Slint special key `k`.
pub(crate) fn is_key(text: &str, k: Key) -> bool {
    let s: SharedString = k.into();
    text == s.as_str()
}

/// Whether a key event should reach the shell at all. Drops bare modifiers
/// (Slint reports Shift/Ctrl/Alt/Meta as low control codepoints), F-keys, and
/// other special keys Slint delivers as control/private-use codepoints that
/// `encode_key` would otherwise pass through as garbage bytes.
pub(crate) fn forwardable(text: &str) -> bool {
    // Special keys we explicitly translate to terminal sequences (encode_key).
    const ALLOWED: [Key; 13] = [
        Key::UpArrow,
        Key::DownArrow,
        Key::LeftArrow,
        Key::RightArrow,
        Key::Home,
        Key::End,
        Key::PageUp,
        Key::PageDown,
        Key::Delete,
        Key::Return,
        Key::Backspace,
        Key::Tab,
        Key::Escape,
    ];
    if ALLOWED.iter().any(|k| {
        let s: SharedString = (*k).into();
        text == s.as_str()
    }) {
        return true;
    }
    // Otherwise only forward normal printable text. Bare modifiers (U+0010..0012)
    // and other control chars, DEL (U+007F), and private-use special keys
    // (U+E000..F8FF: F-keys, Insert, Menu, …) are dropped.
    text.chars().next().is_some_and(|c| {
        let u = c as u32;
        u >= 0x20 && u != 0x7f && !(0xe000..=0xf8ff).contains(&u)
    })
}

/// Translate a key event's text into a [`keybindings::KeyTok`] (the modifier-agnostic key
/// token, the native port of the renderer's normalised `e.key`). Arrows / F11 / Tab / Enter /
/// Escape map to their named tokens; every other printable key becomes a [`KeyTok::Char`]
/// (letters, digits, and symbols like `=`/`-`/`0`) lower-cased so a chord matches regardless
/// of Shift. With Ctrl held Slint reports a control char (Ctrl+A = U+0001 … Ctrl+Z = U+001A),
/// so map that back to its letter. Shared by the router and the keybindings editor's capture.
pub(crate) fn key_tok_from_text(text: &str, control: bool) -> Option<keybindings::KeyTok> {
    use keybindings::KeyTok;
    // Named keys first — these must win before the Ctrl control-char remap (e.g. Ctrl+Tab
    // arrives as U+0009 which would otherwise look like Ctrl+I).
    if is_key(text, Key::LeftArrow) {
        return Some(KeyTok::Left);
    }
    if is_key(text, Key::RightArrow) {
        return Some(KeyTok::Right);
    }
    if is_key(text, Key::UpArrow) {
        return Some(KeyTok::Up);
    }
    if is_key(text, Key::DownArrow) {
        return Some(KeyTok::Down);
    }
    if is_key(text, Key::F11) {
        return Some(KeyTok::F11);
    }
    if is_key(text, Key::Tab) {
        return Some(KeyTok::Tab);
    }
    if is_key(text, Key::Return) {
        return Some(KeyTok::Enter);
    }
    if is_key(text, Key::Escape) {
        return Some(KeyTok::Escape);
    }
    let c = text.chars().next()?;
    let u = c as u32;
    // Remap a control codepoint back to a letter only when Ctrl is actually held (Ctrl+A =
    // U+0001 … Ctrl+Z = U+001A). The named-key checks above already consumed Tab/Enter/Esc.
    if control && (1..=26).contains(&u) {
        return Some(KeyTok::Char((b'a' + (u as u8) - 1) as char));
    }
    if c == ' ' {
        return Some(KeyTok::Space);
    }
    // On many keyboard layouts "+" is Shift+"=", so a Ctrl++ chord arrives with the literal
    // "+" text. Normalize it to "=" so it resolves to the zoom-in binding (Ctrl+=) the same as
    // the unshifted key (match_chord is also Shift-tolerant for "=").
    if c == '+' {
        return Some(KeyTok::Char('='));
    }
    let lc = c.to_ascii_lowercase();
    let lu = lc as u32;
    // Any other printable, non-control character is a Char token.
    if lu >= 0x20 && lu != 0x7f && !(0xe000..=0xf8ff).contains(&lu) {
        Some(KeyTok::Char(lc))
    } else {
        None
    }
}

fn key_tok(msg: &KeyMsg) -> Option<keybindings::KeyTok> {
    key_tok_from_text(&msg.text, msg.control)
}

/// Resolve a key event to a bound [`Command`] via the user's keymap (overrides win over
/// defaults — see [`keybindings::Keymap::match_chord`]).
pub(crate) fn route_chord(keymap: &keybindings::Keymap, msg: &KeyMsg) -> Option<Command> {
    let tok = key_tok(msg)?;
    keymap.match_chord(msg.control, msg.alt, msg.shift, tok)
}
