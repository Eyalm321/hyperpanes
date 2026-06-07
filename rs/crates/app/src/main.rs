//! `hyperpanes` — the native Slint GUI (Phase 2: single-window MVP).
//!
//! Builds the actual app on top of the finished headless core + the terminal widget:
//!   - frameless window + custom icon-only top bar (min/max/close — lift the Win32 from
//!     `rs/spikes/tearoff`), a single tab;
//!   - central workspace state (panes / layout / sizes / focus — the MVP subset of
//!     `src/renderer/store/useWorkspace.ts`);
//!   - `hyperpanes_core::layout::compute_tiles` → place `hyperpanes_terminal_widget::TerminalPane`
//!     instances at the tile rects; spawn each via `hyperpanes_core::session_manager`;
//!   - focus / close / resize, keyboard routing to the focused pane, theme + font.
//!
//! STUB — owned by track `app-shell`. See FANOUT-HANDOFF.md.

fn main() {
    println!("hyperpanes GUI: not yet implemented — see FANOUT-HANDOFF.md");
}
