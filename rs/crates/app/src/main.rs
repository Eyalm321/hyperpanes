//! `hyperpanes` — the native Slint GUI (Phase 3, Wave 1: multi-tab workspace).
//!
//! This file is the thin **controller**: it owns the runtime + session manager,
//! realizes the frameless window, and wires every Slint callback to a
//! [`command::Command`] dispatched against the central [`state::State`]. All the
//! interesting logic lives in the modules:
//!
//!   * [`state`]    — the central workspace state (tabs/panes/layout/zoom) and its
//!                    mutate-then-resync API (**Seam #1**);
//!   * [`command`]  — the `Command` enum + `dispatch` (**Seam #2**);
//!   * [`paneview`] — resync (State → Slint models) + the per-frame pump;
//!   * [`theme`]    — palette, layout metadata, font loading;
//!   * [`window`]   — Win32 frameless / fullscreen glue.
//!
//! The `.slint` views carry an empty overlay slot (**Seam #3**) for Wave-2 panels.
//! See `ARCHITECTURE.md`.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod command;
mod keybindings;
mod palette;
mod paneview;
mod prefs;
mod sidebar;
mod state;
mod theme;
mod window;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use hyperpanes_core::layout::presets::DividerKind;
use hyperpanes_core::session_manager::{SessionEvent, SessionManager};
use hyperpanes_terminal_widget::encode_key;

use slint::platform::Key;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc::unbounded_channel;

use command::{dispatch, set_layout_from_id, Command, Effect};
use paneview::Ui;
use state::{EscOutcome, State};

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

/// Shared controller handles threaded through every callback.
struct Ctx {
    state: Rc<RefCell<Option<State>>>,
    mgr: Rc<SessionManager>,
    app: slint::Weak<AppWindow>,
    hwnd: Rc<RefCell<isize>>,
    saved: Rc<RefCell<Option<window::SavedPlacement>>>,
}

impl Ctx {
    /// Run a command against the state and apply its [`Effect`] (window-level).
    fn run(&self, cmd: Command) {
        dbg_log(&format!("cmd {cmd:?}"));
        let eff = {
            let mut g = self.state.borrow_mut();
            match g.as_mut() {
                Some(st) => dispatch(st, cmd, &self.mgr),
                None => return,
            }
        };
        dbg_log(&format!("  -> effect {eff:?}"));
        match eff {
            Effect::None => {}
            Effect::Quit => {
                if let Some(a) = self.app.upgrade() {
                    let _ = a.window().hide();
                }
            }
            Effect::SetFullscreen(on) => {
                let raw = *self.hwnd.borrow();
                if on {
                    *self.saved.borrow_mut() = window::enter_fullscreen(raw);
                } else if let Some(s) = self.saved.borrow_mut().take() {
                    window::exit_fullscreen(raw, s);
                }
                // Show the "hold Esc to exit" toast on entry; auto-hide after a beat.
                if let Some(a) = self.app.upgrade() {
                    a.set_fullscreen_hint(on);
                    if on {
                        let w = self.app.clone();
                        slint::Timer::single_shot(Duration::from_millis(2500), move || {
                            if let Some(a) = w.upgrade() {
                                a.set_fullscreen_hint(false);
                            }
                        });
                    }
                }
            }
        }
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

    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let (etx, erx) = unbounded_channel::<SessionEvent>();
    let mgr = Rc::new(SessionManager::new(etx));
    let erx = Rc::new(RefCell::new(erx));

    let app = AppWindow::new()?;

    // The UI models the controller owns.
    let ui = Rc::new(Ui {
        panes: Rc::new(VecModel::default()),
        tabs: Rc::new(VecModel::default()),
        dividers: Rc::new(VecModel::default()),
        layouts: Rc::new(VecModel::default()),
        palette: Rc::new(VecModel::default()),
        projects: Rc::new(VecModel::default()),
        families: Rc::new(VecModel::default()),
    });
    app.set_panes(ModelRc::from(ui.panes.clone()));
    app.set_tabs(ModelRc::from(ui.tabs.clone()));
    app.set_dividers(ModelRc::from(ui.dividers.clone()));
    app.set_layouts(ModelRc::from(ui.layouts.clone()));
    app.set_palette(ModelRc::from(ui.palette.clone()));
    app.set_projects(ModelRc::from(ui.projects.clone()));
    app.set_pref_families(ModelRc::from(ui.families.clone()));

    let state: Rc<RefCell<Option<State>>> = Rc::new(RefCell::new(None));
    let area: Rc<RefCell<(f32, f32)>> = Rc::new(RefCell::new((0.0, 0.0)));
    let hwnd: Rc<RefCell<isize>> = Rc::new(RefCell::new(0));
    let saved: Rc<RefCell<Option<window::SavedPlacement>>> = Rc::new(RefCell::new(None));

    let ctx = Rc::new(Ctx {
        state: state.clone(),
        mgr: mgr.clone(),
        app: app.as_weak(),
        hwnd: hwnd.clone(),
        saved: saved.clone(),
    });

    // ---- area geometry (resize → relayout) ----
    {
        let area = area.clone();
        let state = state.clone();
        app.on_area_resized(move |w, h| {
            *area.borrow_mut() = (w, h);
            if let Some(st) = state.borrow_mut().as_mut() {
                st.dirty = true;
            }
        });
    }

    // ---- pane callbacks ----
    {
        let ctx = ctx.clone();
        app.on_focus_pane(move |idx| ctx.run(Command::FocusPane(idx as usize)));
    }
    {
        let ctx = ctx.clone();
        app.on_new_pane(move || ctx.run(Command::NewPane));
    }
    {
        let ctx = ctx.clone();
        app.on_close_focused(move || ctx.run(Command::CloseFocused));
    }
    {
        let ctx = ctx.clone();
        app.on_toggle_zoom(move || ctx.run(Command::ToggleZoom));
    }
    {
        let ctx = ctx.clone();
        app.on_toggle_fullscreen(move || ctx.run(Command::ToggleFullscreen));
    }
    {
        let ctx = ctx.clone();
        app.on_set_layout(move |id| ctx.run(set_layout_from_id(id)));
    }
    // Pane-header buttons act on that pane: focus it first, then run the action.
    {
        let ctx = ctx.clone();
        app.on_pane_zoom(move |i| {
            ctx.run(Command::FocusPane(i as usize));
            ctx.run(Command::ToggleZoom);
        });
    }
    {
        let ctx = ctx.clone();
        app.on_pane_fullscreen(move |i| {
            ctx.run(Command::FocusPane(i as usize));
            ctx.run(Command::ToggleFullscreen);
        });
    }
    {
        let ctx = ctx.clone();
        app.on_pane_close(move |i| ctx.run(Command::ClosePane(i as usize)));
    }

    // ---- tab callbacks ----
    {
        let ctx = ctx.clone();
        app.on_new_tab(move || ctx.run(Command::NewTab));
    }
    {
        let ctx = ctx.clone();
        app.on_select_tab(move |i| ctx.run(Command::SwitchTab(i as usize)));
    }
    {
        let ctx = ctx.clone();
        app.on_close_tab(move |i| ctx.run(Command::CloseTab(i as usize)));
    }
    {
        let ctx = ctx.clone();
        app.on_begin_rename(move |i| ctx.run(Command::BeginRename(i)));
    }
    {
        let ctx = ctx.clone();
        app.on_rename_tab(move |i, t| ctx.run(Command::RenameTab(i, t.to_string())));
    }

    // ---- Wave-2 overlay callbacks (each maps to a Command) ----
    {
        let ctx = ctx.clone();
        app.on_palette_query(move |q| ctx.run(Command::PaletteQuery(q.to_string())));
    }
    {
        let ctx = ctx.clone();
        app.on_palette_nav(move |d| ctx.run(Command::PaletteNav(d)));
    }
    {
        let ctx = ctx.clone();
        app.on_palette_activate(move || ctx.run(Command::PaletteActivate));
    }
    {
        let ctx = ctx.clone();
        app.on_palette_pick(move |i| ctx.run(Command::PaletteSelect(i as usize)));
    }
    {
        let ctx = ctx.clone();
        app.on_open_project(move |i| ctx.run(Command::OpenProject(i as usize)));
    }
    {
        let ctx = ctx.clone();
        app.on_overlay_dismiss(move || ctx.run(Command::CloseOverlay));
    }
    {
        let ctx = ctx.clone();
        // (kind, arg) → a typed Setting (see ui/overlays.slint).
        app.on_pref_action(move |kind, arg| {
            let setting = match kind {
                0 => state::Setting::FontFamily(arg as usize),
                1 => state::Setting::FontDelta(arg),
                2 => state::Setting::ShowFrame(arg != 0),
                3 => state::Setting::ShowDot(arg != 0),
                _ => return,
            };
            ctx.run(Command::ApplySetting(setting));
        });
    }
    // Top-bar menu entries that open the overlays (mouse access alongside the chords).
    {
        let ctx = ctx.clone();
        app.on_open_palette(move || ctx.run(Command::PaletteOpen));
    }
    {
        let ctx = ctx.clone();
        app.on_open_prefs(move || ctx.run(Command::PrefsOpen));
    }
    {
        let ctx = ctx.clone();
        app.on_toggle_sidebar(move || ctx.run(Command::ToggleSidebar));
    }

    // ---- divider drag: cursor offset from seam centre → size-fraction delta ----
    {
        let ctx = ctx.clone();
        let area = area.clone();
        app.on_divider_drag(move |index, main, vertical, dx, dy| {
            let (aw, ah) = *area.borrow();
            let delta = if vertical {
                if aw > 0.0 { (dx / aw) as f64 } else { 0.0 }
            } else if ah > 0.0 {
                (dy / ah) as f64
            } else {
                0.0
            };
            dbg_log(&format!(
                "divider-drag index={index} main={main} vertical={vertical} dx={dx:.1} dy={dy:.1} area=({aw:.0}x{ah:.0}) -> delta={delta:.4}"
            ));
            if delta == 0.0 {
                return;
            }
            let kind = if main { DividerKind::Main } else { DividerKind::Size };
            ctx.run(Command::ResizeDivider { kind, index, delta });
        });
    }

    // ---- key routing: app shortcuts first, else encode to the focused pane ----
    {
        let ctx = ctx.clone();
        let state = state.clone();
        let mgr = mgr.clone();
        app.on_key(move |idx, msg: KeyMsg| {
            let idx = idx as usize;
            dbg_log(&format!(
                "KEY idx={idx} ctrl={} shift={} alt={} text={:?} cp={:?}",
                msg.control,
                msg.shift,
                msg.alt,
                msg.text.as_str(),
                msg.text.chars().map(|c| c as u32).collect::<Vec<_>>()
            ));
            // App chords come from the keybindings table (Seam #2). Ctrl+Shift is
            // fully app-reserved: run the mapped command (if any) and ALWAYS swallow —
            // never forward to the shell (each chord can also emit a phantom control
            // char; swallowing stops it leaking).
            if msg.control && msg.shift {
                if let Some(cmd) = route_chord(&msg) {
                    ctx.run(cmd);
                }
                return;
            }
            // Other modifier chords (Alt+… focus moves, bare F11) — run + swallow. Plain
            // keys (incl. bare arrows) don't match any binding and fall through.
            if let Some(cmd) = route_chord(&msg) {
                ctx.run(cmd);
                return;
            }
            // Escape: a tap goes to the shell; HOLDING it in fullscreen exits
            // fullscreen (the auto-repeat tail is swallowed).
            if is_key(&msg.text, Key::Escape) {
                let outcome = state.borrow_mut().as_mut().map(|st| st.note_esc());
                match outcome {
                    Some(EscOutcome::Exit) => {
                        ctx.run(Command::ToggleFullscreen);
                        return;
                    }
                    Some(EscOutcome::Ignore) | None => return,
                    Some(EscOutcome::Forward) => {} // fall through to forward it
                }
            }
            // Drop bare modifiers (Slint reports Shift/Ctrl/Alt as U+0010..0012),
            // F-keys, and other non-text special keys so they never leak into the
            // shell. Only real text + the special keys we translate get forwarded.
            if !forwardable(&msg.text) {
                dbg_log(&format!(
                    "drop key codepoints {:?}",
                    msg.text.chars().map(|c| c as u32).collect::<Vec<_>>()
                ));
                return;
            }
            if let Some(bytes) = encode_key(&msg.text, msg.control, msg.alt, msg.shift) {
                if let Some(st) = state.borrow().as_ref() {
                    if let Some(ps) = st.active_tab().panes.get(idx) {
                        mgr.write(&ps.uid, &String::from_utf8_lossy(&bytes));
                    }
                }
            }
        });
    }

    // ---- window controls (Win32) ----
    {
        let hwnd = hwnd.clone();
        app.on_start_drag(move || {
            dbg_log("start-drag");
            window::start_drag(*hwnd.borrow());
        });
    }
    {
        let hwnd = hwnd.clone();
        app.on_min_window(move || window::minimize(*hwnd.borrow()));
    }
    {
        let hwnd = hwnd.clone();
        app.on_max_window(move || {
            dbg_log("max-window");
            window::toggle_max(*hwnd.borrow());
        });
    }
    {
        let hwnd = hwnd.clone();
        let app_weak = app.as_weak();
        app.on_close_window(move || {
            window::close(*hwnd.borrow());
            if let Some(a) = app_weak.upgrade() {
                let _ = a.window().hide();
            }
        });
    }

    // ---- the render / pump loop (8 ms Slint timer on the UI thread) ----
    let timer = slint::Timer::default();
    let app_weak = app.as_weak();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(8), {
        let state = state.clone();
        let area = area.clone();
        let hwnd = hwnd.clone();
        let ui = ui.clone();
        let mgr = mgr.clone();
        let erx = erx.clone();
        move || {
            let app = match app_weak.upgrade() {
                Some(a) => a,
                None => return,
            };
            let scale = app.window().scale_factor().max(1.0);

            // Lazily realize the native HWND + strip the frame, once.
            {
                let mut h = hwnd.borrow_mut();
                if *h == 0 {
                    let raw = window::hwnd_of(app.window());
                    if raw != 0 {
                        window::make_frameless(raw);
                        *h = raw;
                    }
                }
            }

            let (aw, ah) = *area.borrow();

            // Lazy init: wait for the first real area layout, then seed tab 0.
            if state.borrow().is_none() {
                if aw <= 1.0 || ah <= 1.0 {
                    return;
                }
                let mut st = State::new(theme::load_font(scale));
                st.add_pane(&mgr);
                if std::env::var_os("HYPERPANES_DEMO").is_some() {
                    demo_seed(&mut st, &mgr);
                }
                // Screenshot scaffolding: open a named overlay at launch so each
                // Wave-2 feature can be captured deterministically.
                if let Some(which) = std::env::var_os("HYPERPANES_OPEN") {
                    let cmd = match which.to_string_lossy().as_ref() {
                        "palette" => Some(Command::PaletteOpen),
                        "prefs" => Some(Command::PrefsOpen),
                        "sidebar" => Some(Command::ToggleSidebar),
                        _ => None,
                    };
                    if let Some(cmd) = cmd {
                        dispatch(&mut st, cmd, &mgr);
                    }
                }
                *state.borrow_mut() = Some(st);
            }

            let mut guard = state.borrow_mut();
            let st = match guard.as_mut() {
                Some(s) => s,
                None => return,
            };
            let mut rx = erx.borrow_mut();
            let alive = paneview::pump(&app, st, &ui, (aw, ah), scale, &mgr, &mut rx);
            drop(rx);
            drop(guard);
            if !alive {
                let _ = app.window().hide();
            }
        }
    });

    app.run()?;
    mgr.kill_all();
    Ok(())
}

/// Seed a richer workspace (2 tabs, several panes, non-default layouts) so a
/// screenshot exercises the Wave-1 surface. Gated by `HYPERPANES_DEMO`.
fn demo_seed(st: &mut State, mgr: &SessionManager) {
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
fn is_key(text: &str, k: Key) -> bool {
    let s: SharedString = k.into();
    text == s.as_str()
}

/// Whether a key event should reach the shell at all. Drops bare modifiers
/// (Slint reports Shift/Ctrl/Alt/Meta as low control codepoints), F-keys, and
/// other special keys Slint delivers as control/private-use codepoints that
/// `encode_key` would otherwise pass through as garbage bytes.
fn forwardable(text: &str) -> bool {
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
fn route_chord(msg: &KeyMsg) -> Option<Command> {
    let tok = key_tok(msg)?;
    keybindings::match_chord(msg.control, msg.alt, msg.shift, tok)
}
