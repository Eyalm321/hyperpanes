//! Spike B — cross-window live tear-off (Phase 0, go/no-go).
//! Throwaway harness owned entirely by track `spike-tearoff`.
//!
//! Goal: prove a Slint multi-window tear-off where a `TouchArea`-grabbed drag, on
//! leaving the source window, spawns a transparent / click-through / always-on-top
//! "ghost" window that follows the Win32 cursor (`GetCursorPos`), hit-tests against
//! another window's edges for a "stitch" drop, and reparents the pane on release.
//! Slint has no global-cursor / cross-window grab, so this is custom Win32 + Slint
//! glue. Full go/no-go criteria in FANOUT-HANDOFF.md.

fn main() {
    println!("spike-tearoff: not yet implemented — see FANOUT-HANDOFF.md");
}
