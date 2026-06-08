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

mod app;
mod command;
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
mod window;

use std::rc::Rc;
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let mgr = Rc::new(SessionManager::new(etx));

    // The app owns the window registry + the shared session stream.
    let application = App::new(mgr.clone(), erx);
    application.spawn_window(PendingSeed::EmptyTab);

    // One shared pump timer drives every window (drain → render → reap).
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(8), {
        let application = application.clone();
        move || application.tick()
    });

    slint::run_event_loop()?;
    drop(timer); // keep the pump alive for the whole loop
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

/// Translate a key event into a [`keybindings::KeyTok`] (the modifier-agnostic key
/// token). Arrows + F11 map directly; letters are normalised — with Ctrl held Slint
/// reports the control char (Ctrl+A = U+0001 … Ctrl+Z = U+001A), so map it back, and
/// lowercase plain letters so a chord matches regardless of Shift.
fn key_tok(msg: &KeyMsg) -> Option<keybindings::KeyTok> {
    use keybindings::KeyTok;
    if is_key(&msg.text, Key::LeftArrow) {
        return Some(KeyTok::Left);
    }
    if is_key(&msg.text, Key::RightArrow) {
        return Some(KeyTok::Right);
    }
    if is_key(&msg.text, Key::UpArrow) {
        return Some(KeyTok::Up);
    }
    if is_key(&msg.text, Key::DownArrow) {
        return Some(KeyTok::Down);
    }
    if is_key(&msg.text, Key::F11) {
        return Some(KeyTok::F11);
    }
    let c = msg.text.chars().next()?;
    let u = c as u32;
    let letter = if (1..=26).contains(&u) {
        (b'a' + (u as u8) - 1) as char
    } else {
        c.to_ascii_lowercase()
    };
    if letter.is_ascii_alphabetic() {
        Some(KeyTok::Letter(letter))
    } else {
        None
    }
}

/// Resolve a key event to a bound [`Command`] via the keybindings table.
pub(crate) fn route_chord(msg: &KeyMsg) -> Option<Command> {
    let tok = key_tok(msg)?;
    keybindings::match_chord(msg.control, msg.alt, msg.shift, tok)
}
