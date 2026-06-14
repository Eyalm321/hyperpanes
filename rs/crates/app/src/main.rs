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
mod crash;
mod drag;
mod glow;
mod history_scan;
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

    // Crash-reporter mode: a fresh process spawned by the panic hook (or by the next launch when a
    // crash went unacknowledged) to show the recovery dialog. Handle it before ANY app init or the
    // single-instance gate — it only needs a Tokio runtime for rfd's portal backend, and it never
    // installs the panic hook below (so a panic in the reporter can't recurse).
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--crash-report") {
            let log = args
                .get(i + 1)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(crash::default_log_path);
            let rt = tokio::runtime::Runtime::new()?;
            let _guard = rt.enter();
            let outcome = crash::run_report(&log);
            crash::clear_marker();
            if matches!(outcome, crash::Outcome::Relaunch) {
                crash::relaunch();
            }
            return Ok(());
        }
    }

    // Capture any panic to a crash log (the windowed subsystem has no console), then pop a crash
    // reporter from a fresh process (this one is unwinding) — see `crate::crash`.
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let path = std::env::temp_dir().join("hyperpanes-crash.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "PANIC: {info}");
            let bt = std::backtrace::Backtrace::force_capture();
            let _ = writeln!(f, "{bt}");
        }
        crate::crash::write_marker(&path);
        // Guard against recursion if the reporter itself panics (it sets this env on its child).
        if std::env::var_os("HYPERPANES_CRASH_CHILD").is_none() {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe)
                    .arg("--crash-report")
                    .arg(&path)
                    .env("HYPERPANES_CRASH_CHILD", "1")
                    .spawn();
            }
        }
    }));

    // Extract the baked-in OFL fonts (Fira Code / JetBrains Mono) so they always resolve.
    crate::prefs::init_bundled_fonts();

    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    // Single-instance gate (replaces Electron `requestSingleInstanceLock`). Salted by the
    // userData dir so an isolated instance (temp APPDATA / XDG dirs) or a differently-housed
    // dev build never collides with the installed app, exactly like Electron keyed its lock
    // off the userData path. A second launch forwards `{argv, cwd}` to the primary and exits;
    // the primary drains hand-offs in `App::tick` and routes them (attach / new window).
    let argv: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let salt = hyperpanes_core::persistence::paths::user_data_dir()
        .to_string_lossy()
        .into_owned();
    let mut handoff_primary = None;
    match hyperpanes_core::single_instance::acquire(&salt) {
        Ok(hyperpanes_core::single_instance::Instance::Secondary(sec)) => {
            dbg_log("single-instance: secondary, forwarding argv");
            let msg = hyperpanes_core::single_instance::HandoffMessage { argv, cwd };
            let fwd = rt.block_on(async move { sec.forward(&msg).await });
            dbg_log(&format!("single-instance: forward -> {fwd:?}"));
            // Don't wait for the runtime's worker threads on the way out — the hand-off
            // is flushed; exit like Electron's second instance does.
            drop(_guard);
            rt.shutdown_timeout(Duration::from_secs(2));
            fwd?;
            return Ok(());
        }
        Ok(hyperpanes_core::single_instance::Instance::Primary(primary)) => {
            dbg_log("single-instance: primary, serving hand-offs");
            handoff_primary = Some(primary);
        }
        Err(e) => {
            // Gate unavailable on this platform/setup → run standalone.
            dbg_log(&format!("single-instance: gate unavailable ({e})"));
        }
    }

    let (etx, erx) = unbounded_channel::<SessionEvent>();
    let mgr = Arc::new(SessionManager::new(etx));

    // The app owns the window registry + the shared session stream.
    let application = App::new(mgr.clone(), erx);

    // Primary: accept hand-offs on a background task; `App::tick` drains the channel on the
    // UI thread (the handler runs on the tokio runtime and must not touch UI state).
    if let Some(primary) = handoff_primary {
        let (htx, hrx) = std::sync::mpsc::channel();
        application.set_handoff_rx(hrx);
        rt.spawn(async move {
            let _ = primary
                .run_server(move |msg| {
                    let _ = htx.send(msg);
                })
                .await;
        });
    }

    // Wire the launch seed: `hyperpanes -c "<cmd>" --shell … --cwd … --name …` (or a
    // positional workspace `.hyperpanes`/`.json`) seeds the first window from that spec;
    // a bare launch falls back to the LAST SESSION (`last-workspace.json`, written when
    // the final window closes — see `app::persist_last_session`), so tabs/layout/per-pane
    // zoom survive a plain relaunch (#14). A first-ever launch (no last-session file)
    // stays an empty shell pane.
    let seed = match hyperpanes_core::workspace::launch::resolve_launch_workspace(&argv, &cwd) {
        Some(file) => PendingSeed::Workspace(Box::new(file)),
        None => PendingSeed::EmptyTab,
    };
    // Next-launch crash detection: if a previous run crashed and its instant reporter never ran
    // (or was killed), surface the dialog now from a separate process. Primary only — a secondary
    // already returned above. The instant reporter clears the marker once shown, so this won't
    // double-fire after a normal crash + dismiss.
    if let Some(log) = crash::pending() {
        crash::clear_marker();
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg("--crash-report")
                .arg(&log)
                .env("HYPERPANES_CRASH_CHILD", "1")
                .spawn();
        }
    }

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
    const ALLOWED: [Key; 14] = [
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
        // Shift+Tab arrives as Backtab (U+0019, a C0 control char) — without this entry
        // the control-char filter below ate it and Shift+Tab never reached the pty.
        Key::Backtab,
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
    // Slint's NAMED MODIFIER keys are C0 control codepoints (key_codes.rs): Shift=U+0010,
    // Control=U+0011, Alt=U+0012, AltGr=U+0013, CapsLock=U+0014, ShiftR=U+0015,
    // ControlR=U+0016, Meta=U+0017, MetaR=U+0018. They must NEVER reach the Ctrl
    // control-char remap below: pressing the bare Shift key while Ctrl was already down
    // delivered U+0010 with ctrl+shift modifiers, which remapped to 'p' — a phantom
    // Ctrl+Shift+P that popped the command palette on every Ctrl+Shift press. Real letter
    // keys arrive as their literal character on this backend (live-traced: Ctrl+Shift+C =
    // "C"), so dropping the modifier codepoints loses nothing.
    if (0x10..=0x18).contains(&u) {
        return None;
    }
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

/// Translate a key event into a palette command while the palette overlay is open
/// (`query` is the current `state.palette_query`; the key router calls this before any
/// pty forwarding). The palette's query is **controller-owned**, not a focused Slint
/// `TextInput`: the old input grabbed focus with a one-shot `init => focus()` on the
/// freshly created overlay, which doesn't reliably land in Slint (the in-pane search box
/// hit the same thing — see widget.slint), so typed keys leaked into the shell underneath
/// and dismissing the palette could leave nothing focused (keyboard dead, Ctrl+Shift+P
/// included, until a click). Routing the keys here keeps the terminal `FocusScope` focused
/// the whole time, so the palette needs no focus hand-off in either direction.
/// `None` = swallow (no key reaches the pty while the palette is open).
pub(crate) fn palette_key(query: &str, msg: &KeyMsg) -> Option<Command> {
    if is_key(&msg.text, Key::UpArrow) {
        return Some(Command::PaletteNav(-1));
    }
    if is_key(&msg.text, Key::DownArrow) {
        return Some(Command::PaletteNav(1));
    }
    if is_key(&msg.text, Key::Return) {
        return Some(Command::PaletteActivate);
    }
    if is_key(&msg.text, Key::Escape) {
        return Some(Command::CloseOverlay);
    }
    if is_key(&msg.text, Key::Backspace) {
        let mut q = query.to_string();
        q.pop();
        return Some(Command::PaletteQuery(q));
    }
    // Modifier chords are not query text (Ctrl+Shift+… is handled before this; the rest
    // are swallowed so e.g. Ctrl+V can't dump a control char into the shell).
    if msg.control || msg.alt {
        return None;
    }
    // Ordinary printable text extends the query (same printable test as `forwardable`).
    let c = msg.text.chars().next()?;
    let u = c as u32;
    if u >= 0x20 && u != 0x7f && !(0xe000..=0xf8ff).contains(&u) {
        return Some(Command::PaletteQuery(format!("{query}{}", msg.text)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, ctrl: bool, alt: bool, shift: bool) -> KeyMsg {
        KeyMsg { text: text.into(), control: ctrl, alt, shift }
    }

    fn keymap() -> keybindings::Keymap {
        // No user overrides → the compiled-in defaults (Ctrl+Shift+P → palette).
        keybindings::Keymap::default_for_tests()
    }

    // ---- Ctrl+Shift+P → palette, pinned at the ROUTER level (the full text→tok→chord
    // path a live key event takes), for both encodings Slint can deliver the key in.

    #[test]
    fn bare_modifier_presses_route_no_chord() {
        // Slint named modifiers are C0 codepoints (Shift=U+0010 … MetaR=U+0018). U+0010 used
        // to remap to 'p' under the Ctrl control-char rule, so pressing the bare Shift key
        // with Ctrl already down was a phantom Ctrl+Shift+P that popped the palette (live-
        // reproduced; the old test here pinned that phantom as "the control-char encoding").
        for u in 0x10u32..=0x18 {
            let text = char::from_u32(u).unwrap().to_string();
            let cmd = route_chord(&keymap(), &msg(&text, true, false, true));
            assert!(cmd.is_none(), "modifier codepoint {u:#x} routed {cmd:?}");
        }
    }

    #[test]
    fn ctrl_shift_p_opens_palette_letter_encoding() {
        // Real letter keys arrive as their literal character (live-traced: Ctrl+Shift+C =
        // "C"): shifted = "P", plain = "p".
        for text in ["P", "p"] {
            let cmd = route_chord(&keymap(), &msg(text, true, false, true));
            assert!(matches!(cmd, Some(Command::PaletteOpen)), "text {text:?} got {cmd:?}");
        }
    }

    #[test]
    fn ctrl_p_without_shift_is_not_the_palette() {
        for text in ["P", "p"] {
            assert!(route_chord(&keymap(), &msg(text, true, false, false)).is_none());
        }
    }

    #[test]
    fn ctrl_shift_c_copies_not_palette() {
        // The chord that surfaced the phantom: Ctrl+Shift+C must copy — and the bare-Shift
        // press on the way to it (previous test) must not open the palette first.
        for text in ["C", "c"] {
            let cmd = route_chord(&keymap(), &msg(text, true, false, true));
            assert!(matches!(cmd, Some(Command::CopyFocused)), "text {text:?} got {cmd:?}");
        }
    }

    // ---- palette_key: the app-side keyboard while the palette overlay is open ----

    #[test]
    fn palette_key_edits_query() {
        // Printable text appends; Backspace pops (and is a no-op edit on empty).
        assert!(matches!(
            palette_key("la", &msg("y", false, false, false)),
            Some(Command::PaletteQuery(q)) if q == "lay"
        ));
        let bs: slint::SharedString = Key::Backspace.into();
        assert!(matches!(
            palette_key("lay", &msg(bs.as_str(), false, false, false)),
            Some(Command::PaletteQuery(q)) if q == "la"
        ));
        assert!(matches!(
            palette_key("", &msg(bs.as_str(), false, false, false)),
            Some(Command::PaletteQuery(q)) if q.is_empty()
        ));
    }

    #[test]
    fn palette_key_navigates_activates_dismisses() {
        let up: slint::SharedString = Key::UpArrow.into();
        let down: slint::SharedString = Key::DownArrow.into();
        let enter: slint::SharedString = Key::Return.into();
        let esc: slint::SharedString = Key::Escape.into();
        assert!(matches!(palette_key("", &msg(up.as_str(), false, false, false)), Some(Command::PaletteNav(-1))));
        assert!(matches!(palette_key("", &msg(down.as_str(), false, false, false)), Some(Command::PaletteNav(1))));
        assert!(matches!(palette_key("", &msg(enter.as_str(), false, false, false)), Some(Command::PaletteActivate)));
        assert!(matches!(palette_key("", &msg(esc.as_str(), false, false, false)), Some(Command::CloseOverlay)));
    }

    #[test]
    fn palette_key_swallows_chords_and_control_chars() {
        // Ctrl+V (control char 0x16) must not become query text — and must not reach the
        // pty either (the caller swallows on None).
        assert!(palette_key("q", &msg("\u{16}", true, false, false)).is_none());
        // Alt+letter is a chord, not text.
        assert!(palette_key("q", &msg("x", false, true, false)).is_none());
        // Bare modifier presses carry control/private-use text — swallowed.
        assert!(palette_key("q", &msg("\u{11}", false, false, false)).is_none());
    }
}
