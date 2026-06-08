//! Keybindings — Wave-2 feature plugging into **Seam #2** (command dispatch).
//!
//! A declarative chord → [`Command`] table, the native port of the semantics in
//! `src/renderer/keybindings.ts`. The key router (`main.rs`) consults
//! [`match_chord`] for every key event before forwarding text to the focused shell,
//! so the displayed defaults can never drift from what actually fires.
//!
//! The Wave-1 router already reserved `Ctrl+Shift` chords; this generalises that into
//! a table that also covers `Alt`+arrow focus moves and the palette/sidebar toggles,
//! reusing the existing [`Command`]s. A chord with no `Command` simply doesn't match.

use hyperpanes_core::layout::navigate::Direction;

use crate::command::Command;

/// The non-text key tokens a chord can target (mirrors the renderer's normalized
/// `e.key`). Printable chords carry their lowercase letter instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTok {
    Letter(char),
    Left,
    Right,
    Up,
    Down,
    F11,
}

/// A modifier+key combo. `ctrl`/`alt`/`shift` must match exactly.
#[derive(Debug, Clone, Copy)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: KeyTok,
}

impl Chord {
    const fn new(ctrl: bool, alt: bool, shift: bool, key: KeyTok) -> Self {
        Chord { ctrl, alt, shift, key }
    }
    fn matches(&self, ctrl: bool, alt: bool, shift: bool, key: KeyTok) -> bool {
        self.ctrl == ctrl && self.alt == alt && self.shift == shift && self.key == key
    }

    /// The chord's pieces in display order, e.g. `["Ctrl", "Shift", "P"]` — the native port
    /// of the renderer's `comboParts`, rendered as `<kbd>` chips in the keybindings list.
    pub fn parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".into());
        }
        if self.alt {
            parts.push("Alt".into());
        }
        if self.shift {
            parts.push("Shift".into());
        }
        parts.push(match self.key {
            KeyTok::Letter(c) => c.to_ascii_uppercase().to_string(),
            KeyTok::Left => "←".into(),
            KeyTok::Right => "→".into(),
            KeyTok::Up => "↑".into(),
            KeyTok::Down => "↓".into(),
            KeyTok::F11 => "F11".into(),
        });
        parts
    }

    /// Human chord label, e.g. `Ctrl+Shift+P` or `Alt+←` (the joined [`Self::parts`]).
    pub fn label(&self) -> String {
        self.parts().join("+")
    }
}

/// One default binding: a chord, its category + human label, and the command it dispatches.
pub struct Binding {
    pub chord: Chord,
    /// Category heading the keybindings list groups this row under (see [`CATEGORY_ORDER`]).
    pub category: &'static str,
    /// Human label — surfaced by the Preferences keybindings list.
    pub label: &'static str,
    pub command: Command,
}

/// Category headings, in the order the Preferences keybindings list shows them (mirrors the
/// renderer's `CATEGORY_ORDER`, adapted to the native keymap which adds a Windows group).
pub const CATEGORY_ORDER: [&str; 4] = ["General", "Windows", "Tabs", "Panes"];

/// The default keymap, reusing existing Wave-1 commands. Order is the display order.
pub fn default_bindings() -> Vec<Binding> {
    use KeyTok::*;
    let b = |ctrl, alt, shift, key, category, label, command| Binding {
        chord: Chord::new(ctrl, alt, shift, key),
        category,
        label,
        command,
    };
    vec![
        // General
        b(true, false, true, Letter('p'), "General", "Command palette", Command::PaletteOpen),
        b(true, false, true, Letter('b'), "General", "Toggle sidebar", Command::ToggleSidebar),
        // Windows
        b(true, false, true, Letter('o'), "Windows", "New window", Command::NewWindow),
        b(true, false, true, Letter('m'), "Windows", "Move pane to new window", Command::MovePaneToNewWindow),
        // Tabs
        b(true, false, true, Letter('t'), "Tabs", "New tab", Command::NewTab),
        // Panes
        b(true, false, true, Letter('n'), "Panes", "New pane", Command::NewPane),
        b(true, false, true, Letter('w'), "Panes", "Close pane", Command::CloseFocused),
        b(true, false, true, Letter('l'), "Panes", "Cycle layout", Command::CycleLayout),
        b(true, false, true, Letter('z'), "Panes", "Zoom pane", Command::ToggleZoom),
        b(false, true, false, Letter('z'), "Panes", "Zoom pane", Command::ToggleZoom),
        b(true, false, true, Letter('f'), "Panes", "Fullscreen", Command::ToggleFullscreen),
        b(false, false, false, F11, "Panes", "Fullscreen", Command::ToggleFullscreen),
        // Directional focus — both the Wave-1 Ctrl+Shift+arrows and Alt+arrows.
        b(true, false, true, Left, "Panes", "Focus left", Command::FocusDir(Direction::Left)),
        b(true, false, true, Right, "Panes", "Focus right", Command::FocusDir(Direction::Right)),
        b(true, false, true, Up, "Panes", "Focus up", Command::FocusDir(Direction::Up)),
        b(true, false, true, Down, "Panes", "Focus down", Command::FocusDir(Direction::Down)),
        b(false, true, false, Left, "Panes", "Focus left", Command::FocusDir(Direction::Left)),
        b(false, true, false, Right, "Panes", "Focus right", Command::FocusDir(Direction::Right)),
        b(false, true, false, Up, "Panes", "Focus up", Command::FocusDir(Direction::Up)),
        b(false, true, false, Down, "Panes", "Focus down", Command::FocusDir(Direction::Down)),
    ]
}

/// One row of the Preferences → Keybindings list: its category, label, and chord pieces.
pub struct BindingRow {
    pub category: &'static str,
    pub label: &'static str,
    pub parts: Vec<String>,
}

/// The default bindings grouped by [`CATEGORY_ORDER`] (category order, then table order) —
/// the read-only mirror of [`default_bindings`] the keybindings list renders as `<kbd>`
/// chips. Each category's rows are contiguous so the view can draw a heading per group.
pub fn binding_rows() -> Vec<BindingRow> {
    let bindings = default_bindings();
    let mut rows = Vec::with_capacity(bindings.len());
    for category in CATEGORY_ORDER {
        for b in bindings.iter().filter(|b| b.category == category) {
            rows.push(BindingRow { category, label: b.label, parts: b.chord.parts() });
        }
    }
    rows
}

/// Find the command bound to the given modifier+key combo, if any.
pub fn match_chord(ctrl: bool, alt: bool, shift: bool, key: KeyTok) -> Option<Command> {
    default_bindings()
        .into_iter()
        .find(|b| b.chord.matches(ctrl, alt, shift, key))
        .map(|b| b.command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_chord_resolves() {
        assert!(matches!(
            match_chord(true, false, true, KeyTok::Letter('p')),
            Some(Command::PaletteOpen)
        ));
    }

    #[test]
    fn alt_arrow_focus_resolves() {
        assert!(matches!(
            match_chord(false, true, false, KeyTok::Left),
            Some(Command::FocusDir(Direction::Left))
        ));
    }

    #[test]
    fn unbound_combo_is_none() {
        assert!(match_chord(false, false, false, KeyTok::Letter('q')).is_none());
    }

    #[test]
    fn chord_labels_format() {
        assert_eq!(
            Chord::new(true, false, true, KeyTok::Letter('p')).label(),
            "Ctrl+Shift+P"
        );
        assert_eq!(Chord::new(false, true, false, KeyTok::Left).label(), "Alt+←");
        assert_eq!(Chord::new(false, false, false, KeyTok::F11).label(), "F11");
    }

    #[test]
    fn binding_rows_cover_every_default_grouped() {
        let rows = binding_rows();
        assert_eq!(rows.len(), default_bindings().len());
        // Every row has a label + at least one chord chip.
        assert!(rows.iter().all(|r| !r.label.is_empty() && !r.parts.is_empty()));
        // The palette row renders as Ctrl / Shift / P chips under General.
        let palette = rows.iter().find(|r| r.label == "Command palette").unwrap();
        assert_eq!(palette.category, "General");
        assert_eq!(palette.parts, vec!["Ctrl", "Shift", "P"]);
        // Rows are grouped: each category appears as one contiguous block, in CATEGORY_ORDER.
        let mut seen: Vec<&str> = Vec::new();
        for r in &rows {
            if seen.last() != Some(&r.category) {
                assert!(!seen.contains(&r.category), "category {} not contiguous", r.category);
                seen.push(r.category);
            }
        }
        assert_eq!(seen, CATEGORY_ORDER);
    }

    #[test]
    fn exact_modifier_match_required() {
        // Ctrl+Shift+T is New tab; plain T is not bound.
        assert!(match_chord(true, false, true, KeyTok::Letter('t')).is_some());
        assert!(match_chord(false, false, false, KeyTok::Letter('t')).is_none());
    }
}
