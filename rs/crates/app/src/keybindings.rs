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

    /// Human chord label, e.g. `Ctrl+Shift+P` or `Alt+←` — the native port of the
    /// renderer's `comboLabel` (modifiers in Ctrl→Alt→Shift order, then the key). Drives
    /// the Preferences → Keybindings list.
    pub fn label(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        let key_owned;
        let key = match self.key {
            KeyTok::Letter(c) => {
                key_owned = c.to_ascii_uppercase().to_string();
                key_owned.as_str()
            }
            KeyTok::Left => "←",
            KeyTok::Right => "→",
            KeyTok::Up => "↑",
            KeyTok::Down => "↓",
            KeyTok::F11 => "F11",
        };
        parts.push(key);
        parts.join("+")
    }
}

/// One default binding: a chord, its human label (for a future keybindings view), and
/// the command it dispatches.
pub struct Binding {
    pub chord: Chord,
    /// Human label — surfaced by a future preferences keybindings list.
    #[allow(dead_code)]
    pub label: &'static str,
    pub command: Command,
}

/// The default keymap, reusing existing Wave-1 commands. Order is the display order.
pub fn default_bindings() -> Vec<Binding> {
    use KeyTok::*;
    let b = |ctrl, alt, shift, key, label, command| Binding {
        chord: Chord::new(ctrl, alt, shift, key),
        label,
        command,
    };
    vec![
        // General
        b(true, false, true, Letter('p'), "Command palette", Command::PaletteOpen),
        b(true, false, true, Letter('b'), "Toggle sidebar", Command::ToggleSidebar),
        // Windows
        b(true, false, true, Letter('o'), "New window", Command::NewWindow),
        b(true, false, true, Letter('m'), "Move pane to new window", Command::MovePaneToNewWindow),
        // Tabs / panes
        b(true, false, true, Letter('t'), "New tab", Command::NewTab),
        b(true, false, true, Letter('n'), "New pane", Command::NewPane),
        b(true, false, true, Letter('w'), "Close pane", Command::CloseFocused),
        b(true, false, true, Letter('l'), "Cycle layout", Command::CycleLayout),
        b(true, false, true, Letter('z'), "Zoom pane", Command::ToggleZoom),
        b(true, false, true, Letter('f'), "Fullscreen", Command::ToggleFullscreen),
        b(false, false, false, F11, "Fullscreen", Command::ToggleFullscreen),
        // Directional focus — both the Wave-1 Ctrl+Shift+arrows and Alt+arrows.
        b(true, false, true, Left, "Focus left", Command::FocusDir(Direction::Left)),
        b(true, false, true, Right, "Focus right", Command::FocusDir(Direction::Right)),
        b(true, false, true, Up, "Focus up", Command::FocusDir(Direction::Up)),
        b(true, false, true, Down, "Focus down", Command::FocusDir(Direction::Down)),
        b(false, true, false, Left, "Focus left", Command::FocusDir(Direction::Left)),
        b(false, true, false, Right, "Focus right", Command::FocusDir(Direction::Right)),
        b(false, true, false, Up, "Focus up", Command::FocusDir(Direction::Up)),
        b(false, true, false, Down, "Focus down", Command::FocusDir(Direction::Down)),
        b(false, true, false, Letter('z'), "Zoom pane", Command::ToggleZoom),
    ]
}

/// The default bindings as `(label, chord)` display pairs, in table order — the data the
/// Preferences → Keybindings list renders (read-only mirror of [`default_bindings`]).
pub fn binding_rows() -> Vec<(String, String)> {
    default_bindings()
        .iter()
        .map(|b| (b.label.to_string(), b.chord.label()))
        .collect()
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
    fn binding_rows_cover_every_default() {
        let rows = binding_rows();
        assert_eq!(rows.len(), default_bindings().len());
        // Each row is a non-empty label + chord (e.g. "Command palette" / "Ctrl+Shift+P").
        assert!(rows.iter().all(|(l, c)| !l.is_empty() && !c.is_empty()));
        assert!(rows.iter().any(|(l, c)| l == "Command palette" && c == "Ctrl+Shift+P"));
    }

    #[test]
    fn exact_modifier_match_required() {
        // Ctrl+Shift+T is New tab; plain T is not bound.
        assert!(match_chord(true, false, true, KeyTok::Letter('t')).is_some());
        assert!(match_chord(false, false, false, KeyTok::Letter('t')).is_none());
    }
}
