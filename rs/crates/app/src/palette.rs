//! Command palette — Wave-2 feature plugging into **Seam #2** (command dispatch).
//!
//! A searchable overlay (Ctrl+Shift+P) that lists a registry of [`Entry`]s, each a
//! human label + a ready-to-run [`Command`]. The query is fuzzy-matched against the
//! title + keywords; the controller dispatches the selected entry's `Command` through
//! the same [`crate::command::dispatch`] every other action uses.
//!
//! Ports `src/renderer/commands/{fuzzy,registry}.ts`:
//!   * [`fuzzy_score`] is a 1:1 port of `fuzzyScore` (subsequence match with a
//!     consecutive-run bonus + word-boundary bonus);
//!   * [`build`] mirrors `buildCommands` — rebuilt from current state each open so the
//!     pane-focus / active-layout entries stay fresh.

use hyperpanes_core::layout::presets::Layout;

use crate::command::Command;
use crate::state::State;
use crate::theme;

/// One palette row: a label + the action it dispatches.
#[derive(Clone)]
pub struct Entry {
    pub title: String,
    pub subtitle: String,
    /// Extra search terms (not shown) — mirrors the TS `keywords`.
    pub keywords: String,
    pub command: Command,
}

impl Entry {
    fn new(title: &str, subtitle: &str, keywords: &str, command: Command) -> Self {
        Entry {
            title: title.into(),
            subtitle: subtitle.into(),
            keywords: keywords.into(),
            command,
        }
    }
}

/// Lightweight subsequence fuzzy matcher — a 1:1 port of `fuzzyScore`. Returns a
/// score (higher is better) or `None` when `query` isn't a subsequence of `text`.
/// Rewards consecutive matches and word-boundary hits so "lg" ranks "Layout: Grid".
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    if q.is_empty() {
        return Some(0);
    }
    let mut ti = 0usize;
    let mut score = 0i32;
    let mut streak = 0i32;
    for ch in q {
        let mut found: Option<usize> = None;
        for j in ti..t.len() {
            if t[j] == ch {
                found = Some(j);
                break;
            }
        }
        let found = found?;
        if found == ti {
            streak += 1;
            score += 2 + streak; // consecutive run bonus
        } else {
            streak = 0;
            score += 1;
        }
        let prev = if found > 0 { t[found - 1] } else { ' ' };
        if prev == ' ' || prev == ':' || prev == '-' {
            score += 4; // word-boundary bonus
        }
        ti = found + 1;
    }
    Some(score)
}

/// The layouts offered in the palette, automatic first (mirrors the TS order).
const PALETTE_LAYOUTS: [Layout; 6] = [
    Layout::Auto,
    Layout::Single,
    Layout::Columns,
    Layout::Rows,
    Layout::Grid,
    Layout::MainStack,
];

/// Build the command list from current state. Rebuilt each open so the pane-focus
/// entries + the active-layout `current` marker stay fresh (mirrors `buildCommands`).
pub fn build(state: &State) -> Vec<Entry> {
    let mut cmds: Vec<Entry> = Vec::new();

    // ---- tabs ----
    cmds.push(Entry::new(
        "New tab",
        "Open a new workspace tab",
        "group workspace add",
        Command::NewTab,
    ));
    cmds.push(Entry::new(
        "Close tab",
        "Close the current tab",
        "group workspace remove",
        Command::CloseTab(state.active),
    ));

    // ---- panes ----
    cmds.push(Entry::new(
        "New pane",
        "Spawn an interactive shell",
        "add create terminal shell",
        Command::NewPane,
    ));
    cmds.push(Entry::new(
        "New goal",
        "Set an autonomous goal for a project (spawns its goals orchestrator)",
        "goal agent orchestrator autonomous objective",
        Command::OpenNewGoal,
    ));

    // ---- windows ----
    cmds.push(Entry::new(
        "New window",
        "Open a second OS window on the shared session engine",
        "os window monitor split",
        Command::NewWindow,
    ));

    let t = state.active_tab();
    if !t.panes.is_empty() {
        let focused = t.focused;
        cmds.push(Entry::new(
            "Move pane to new window",
            "Re-host the focused pane in a new window (keeps its session)",
            "detach tear-off rehost window",
            Command::MovePaneToNewWindow,
        ));
        cmds.push(Entry::new(
            if t.zoomed.is_some() {
                "Unzoom pane"
            } else {
                "Zoom pane"
            },
            "Toggle full-tab zoom of the focused pane",
            "maximize fullscreen expand",
            Command::ToggleZoom,
        ));
        cmds.push(Entry::new(
            "Fullscreen",
            "Toggle borderless OS fullscreen",
            "maximize monitor",
            Command::ToggleFullscreen,
        ));
        cmds.push(Entry::new(
            &format!("Close pane: {}", focused + 1),
            "Close the focused pane",
            "remove kill",
            Command::ClosePane(focused),
        ));
    }

    // ---- preferences + sidebar ----
    cmds.push(Entry::new(
        "Preferences…",
        "Terminal font, frame & dot",
        "settings options config appearance",
        Command::PrefsOpen,
    ));
    cmds.push(Entry::new(
        "Toggle sidebar",
        "Show/hide the git-projects panel",
        "projects git folder rail",
        Command::ToggleSidebar,
    ));

    // ---- layouts (automatic first, then the concrete presets) ----
    let cur = t.layout;
    for l in PALETTE_LAYOUTS {
        cmds.push(Entry::new(
            &format!("Layout: {}", theme::layout_name(l)),
            if l == cur { "current" } else { "" },
            "arrange tile split automatic",
            Command::SetLayout(l),
        ));
    }

    // ---- focus a specific pane ----
    for (i, _p) in t.panes.iter().enumerate() {
        cmds.push(Entry::new(
            &format!("Focus: pane {}", i + 1),
            &format!("pane {}", i + 1),
            "go switch select",
            Command::FocusPane(i),
        ));
    }

    cmds
}

/// Filter + rank `entries` against `query`, returning the surviving indices in best-
/// first order. An empty query keeps the natural order (mirrors the TS palette).
pub fn filter(entries: &[Entry], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let mut scored: Vec<(usize, i32)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let hay = format!("{} {}", e.title, e.keywords);
        if let Some(s) = fuzzy_score(query, &hay) {
            scored.push((i, s));
        }
    }
    // Stable sort by score desc (ties keep registry order).
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_scores_zero() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn non_subsequence_is_none() {
        assert_eq!(fuzzy_score("zzz", "Layout: Grid"), None);
    }

    #[test]
    fn boundary_match_outranks_scattered() {
        // "lg" should match "Layout: Grid" (word boundaries) better than a scattered hit.
        let strong = fuzzy_score("lg", "Layout: Grid").unwrap();
        let weak = fuzzy_score("lg", "abclxxxg").unwrap();
        assert!(
            strong > weak,
            "boundary {strong} should beat scattered {weak}"
        );
    }

    #[test]
    fn consecutive_run_beats_gaps() {
        // Isolate the consecutive-run bonus (no competing word boundaries): "ab"
        // matched contiguously must outrank the same letters split by a gap.
        let run = fuzzy_score("ab", "ab").unwrap();
        let gappy = fuzzy_score("ab", "axb").unwrap();
        assert!(run > gappy, "contiguous {run} should beat gapped {gappy}");
    }
}
