//! Keybindings — Wave-2 feature plugging into **Seam #2** (command dispatch), now
//! **user-editable** (the native port of `src/renderer/{keybindings.ts,
//! store/useKeybindings.ts}`).
//!
//! A declarative `id → (default chord, [`Command`])` table ([`default_bindings`]) plus a
//! persisted [`Keymap`] of per-binding **overrides**. The key router (`main::route_chord`)
//! consults the keymap for every key event: the *effective* chord for a binding is its
//! override if the user set one, else its default — so an override always wins, exactly
//! like the renderer's `combos` overlay. The Preferences → Keybindings editor rebinds a
//! chord (capture a key combo → [`Keymap::set`]), resets one to default ([`Keymap::reset`]),
//! or resets them all ([`Keymap::reset_all`]); changes persist to
//! `%APPDATA%\hyperpanes\native-keybindings.json`.

use std::collections::BTreeMap;

use hyperpanes_core::layout::navigate::Direction;
use hyperpanes_core::persistence::paths;
use serde::{Deserialize, Serialize};

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

impl KeyTok {
    /// The persisted/normalized token string (mirrors the renderer's `e.key`): a single
    /// lowercase letter, or `left`/`right`/`up`/`down`/`f11`.
    pub fn token(self) -> String {
        match self {
            KeyTok::Letter(c) => c.to_ascii_lowercase().to_string(),
            KeyTok::Left => "left".into(),
            KeyTok::Right => "right".into(),
            KeyTok::Up => "up".into(),
            KeyTok::Down => "down".into(),
            KeyTok::F11 => "f11".into(),
        }
    }

    /// Parse a normalized token back into a [`KeyTok`] (the inverse of [`Self::token`]).
    /// Unknown multi-char tokens return `None`.
    pub fn from_token(s: &str) -> Option<KeyTok> {
        match s {
            "left" => Some(KeyTok::Left),
            "right" => Some(KeyTok::Right),
            "up" => Some(KeyTok::Up),
            "down" => Some(KeyTok::Down),
            "f11" => Some(KeyTok::F11),
            _ => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_alphabetic() => Some(KeyTok::Letter(c.to_ascii_lowercase())),
                    _ => None,
                }
            }
        }
    }

    /// The display chip for this key (`P`, `←`, `F11`).
    fn label(self) -> String {
        match self {
            KeyTok::Letter(c) => c.to_ascii_uppercase().to_string(),
            KeyTok::Left => "←".into(),
            KeyTok::Right => "→".into(),
            KeyTok::Up => "↑".into(),
            KeyTok::Down => "↓".into(),
            KeyTok::F11 => "F11".into(),
        }
    }
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
        parts.push(self.key.label());
        parts
    }

    /// Human chord label, e.g. `Ctrl+Shift+P` or `Alt+←` (the joined [`Self::parts`]). Used
    /// by tests + available for diagnostics; the editor renders [`Self::parts`] as chips.
    #[allow(dead_code)]
    pub fn label(&self) -> String {
        self.parts().join("+")
    }
}

/// The serde wire form of a [`Chord`] — a `{ctrl, alt, shift, key}` object whose `key` is the
/// normalized token (mirrors the renderer's persisted `Combo`). Kept separate so the file
/// format is stable + human-readable regardless of the in-memory [`KeyTok`] shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChordRepr {
    #[serde(default)]
    ctrl: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    shift: bool,
    key: String,
}

impl ChordRepr {
    fn to_chord(&self) -> Option<Chord> {
        KeyTok::from_token(&self.key).map(|key| Chord {
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
            key,
        })
    }
}

impl From<Chord> for ChordRepr {
    fn from(c: Chord) -> Self {
        ChordRepr { ctrl: c.ctrl, alt: c.alt, shift: c.shift, key: c.key.token() }
    }
}

/// One default binding: a stable `id`, a default chord, its category + human label, and the
/// command it dispatches. The `id` keys the user override (and survives a chord/label change).
pub struct Binding {
    pub id: &'static str,
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

/// The default keymap, reusing existing Wave-1 commands. Order is the display order. The
/// secondary directional/zoom/fullscreen bindings (the native build supports both
/// `Ctrl+Shift` and `Alt`/`F11`) carry a distinguishing label so each editor row is
/// unambiguous.
pub fn default_bindings() -> Vec<Binding> {
    use KeyTok::*;
    let b = |id, ctrl, alt, shift, key, category, label, command| Binding {
        id,
        chord: Chord::new(ctrl, alt, shift, key),
        category,
        label,
        command,
    };
    vec![
        // General
        b("palette.toggle", true, false, true, Letter('p'), "General", "Command palette", Command::PaletteOpen),
        b("sidebar.toggle", true, false, true, Letter('b'), "General", "Toggle sidebar", Command::ToggleSidebar),
        // Windows
        b("window.new", true, false, true, Letter('o'), "Windows", "New window", Command::NewWindow),
        b("window.movePane", true, false, true, Letter('m'), "Windows", "Move pane to new window", Command::MovePaneToNewWindow),
        // Tabs
        b("tab.new", true, false, true, Letter('t'), "Tabs", "New tab", Command::NewTab),
        // Panes
        b("pane.new", true, false, true, Letter('n'), "Panes", "New pane", Command::NewPane),
        b("pane.close", true, false, true, Letter('w'), "Panes", "Close pane", Command::CloseFocused),
        b("pane.cycleLayout", true, false, true, Letter('l'), "Panes", "Cycle layout", Command::CycleLayout),
        b("pane.zoom", true, false, true, Letter('z'), "Panes", "Zoom pane", Command::ToggleZoom),
        b("pane.zoom.alt", false, true, false, Letter('z'), "Panes", "Zoom pane (alt)", Command::ToggleZoom),
        b("pane.fullscreen", true, false, true, Letter('f'), "Panes", "Fullscreen", Command::ToggleFullscreen),
        b("pane.fullscreen.f11", false, false, false, F11, "Panes", "Fullscreen (F11)", Command::ToggleFullscreen),
        // Directional focus — both the Wave-1 Ctrl+Shift+arrows and Alt+arrows.
        b("pane.focusLeft", true, false, true, Left, "Panes", "Focus left", Command::FocusDir(Direction::Left)),
        b("pane.focusRight", true, false, true, Right, "Panes", "Focus right", Command::FocusDir(Direction::Right)),
        b("pane.focusUp", true, false, true, Up, "Panes", "Focus up", Command::FocusDir(Direction::Up)),
        b("pane.focusDown", true, false, true, Down, "Panes", "Focus down", Command::FocusDir(Direction::Down)),
        b("pane.focusLeft.alt", false, true, false, Left, "Panes", "Focus left (alt)", Command::FocusDir(Direction::Left)),
        b("pane.focusRight.alt", false, true, false, Right, "Panes", "Focus right (alt)", Command::FocusDir(Direction::Right)),
        b("pane.focusUp.alt", false, true, false, Up, "Panes", "Focus up (alt)", Command::FocusDir(Direction::Up)),
        b("pane.focusDown.alt", false, true, false, Down, "Panes", "Focus down (alt)", Command::FocusDir(Direction::Down)),
    ]
}

/// One row of the Preferences → Keybindings list: its binding id, category, label, the
/// **effective** chord pieces (override or default), and whether it's been overridden (so the
/// editor can show a "reset" affordance).
pub struct BindingRow {
    pub id: &'static str,
    pub category: &'static str,
    pub label: &'static str,
    pub parts: Vec<String>,
    pub overridden: bool,
}

/// The user's per-binding chord overrides (the native port of `useKeybindings`'s persisted
/// `combos`, stored as a sparse override map rather than the full table so bindings added in
/// a later version still get their fresh default).
pub struct Keymap {
    overrides: BTreeMap<String, Chord>,
}

impl Keymap {
    /// Load the persisted overrides (an empty map on a missing/corrupt file). Unknown ids and
    /// un-parseable chords are dropped, so an older blob never breaks the editor.
    pub fn load() -> Self {
        let mut overrides = BTreeMap::new();
        let path = paths::user_data_dir().join("native-keybindings.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(map) = serde_json::from_str::<BTreeMap<String, ChordRepr>>(&raw) {
                let valid: std::collections::HashSet<&str> =
                    default_bindings().iter().map(|b| b.id).collect();
                for (id, repr) in map {
                    if valid.contains(id.as_str()) {
                        if let Some(chord) = repr.to_chord() {
                            overrides.insert(id, chord);
                        }
                    }
                }
            }
        }
        Keymap { overrides }
    }

    /// Persist the overrides atomically (best-effort; a write failure is logged, never fatal).
    fn save(&self) {
        let path = paths::user_data_dir().join("native-keybindings.json");
        let map: BTreeMap<&String, ChordRepr> =
            self.overrides.iter().map(|(id, c)| (id, ChordRepr::from(*c))).collect();
        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = paths::write_atomic(&path, json.as_bytes()) {
                    crate::dbg_log(&format!("keybindings save failed: {e}"));
                }
            }
            Err(e) => crate::dbg_log(&format!("keybindings serialize failed: {e}")),
        }
    }

    /// The effective chord for `id`: the user override if set, else `default`.
    fn effective(&self, id: &str, default: Chord) -> Chord {
        self.overrides.get(id).copied().unwrap_or(default)
    }

    /// Whether `id` currently has a user override.
    pub fn is_overridden(&self, id: &str) -> bool {
        self.overrides.contains_key(id)
    }

    /// Find the command bound to a live modifier+key combo, consulting each binding's
    /// **effective** chord (override wins over default), first match in table order.
    pub fn match_chord(&self, ctrl: bool, alt: bool, shift: bool, key: KeyTok) -> Option<Command> {
        default_bindings()
            .into_iter()
            .find(|b| self.effective(b.id, b.chord).matches(ctrl, alt, shift, key))
            .map(|b| b.command)
    }

    /// Override binding `id` with `chord`, persisting. Unknown ids are ignored.
    pub fn set(&mut self, id: &str, chord: Chord) {
        if default_bindings().iter().any(|b| b.id == id) {
            self.overrides.insert(id.to_string(), chord);
            self.save();
        }
    }

    /// Reset binding `id` to its default chord (drop any override), persisting.
    pub fn reset(&mut self, id: &str) {
        if self.overrides.remove(id).is_some() {
            self.save();
        }
    }

    /// Reset *every* binding to its default (clear all overrides), persisting.
    pub fn reset_all(&mut self) {
        if !self.overrides.is_empty() {
            self.overrides.clear();
            self.save();
        }
    }

    /// Whether any binding is overridden (drives the "Reset all" affordance).
    pub fn any_overridden(&self) -> bool {
        !self.overrides.is_empty()
    }

    /// The bindings grouped by [`CATEGORY_ORDER`] (category order, then table order) — the
    /// editor's row model, with each row's **effective** chord chips + overridden flag. Each
    /// category's rows are contiguous so the view can draw a heading per group.
    pub fn rows(&self) -> Vec<BindingRow> {
        let bindings = default_bindings();
        let mut rows = Vec::with_capacity(bindings.len());
        for category in CATEGORY_ORDER {
            for b in bindings.iter().filter(|b| b.category == category) {
                let chord = self.effective(b.id, b.chord);
                rows.push(BindingRow {
                    id: b.id,
                    category,
                    label: b.label,
                    parts: chord.parts(),
                    overridden: self.is_overridden(b.id),
                });
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_keymap() -> Keymap {
        Keymap { overrides: BTreeMap::new() }
    }

    #[test]
    fn palette_chord_resolves() {
        let km = empty_keymap();
        assert!(matches!(
            km.match_chord(true, false, true, KeyTok::Letter('p')),
            Some(Command::PaletteOpen)
        ));
    }

    #[test]
    fn alt_arrow_focus_resolves() {
        let km = empty_keymap();
        assert!(matches!(
            km.match_chord(false, true, false, KeyTok::Left),
            Some(Command::FocusDir(Direction::Left))
        ));
    }

    #[test]
    fn unbound_combo_is_none() {
        let km = empty_keymap();
        assert!(km.match_chord(false, false, false, KeyTok::Letter('q')).is_none());
    }

    #[test]
    fn override_wins_over_default() {
        let mut km = empty_keymap();
        // Rebind the palette to Ctrl+J. The old Ctrl+Shift+P no longer fires; Ctrl+J does.
        km.overrides.insert(
            "palette.toggle".into(),
            Chord::new(true, false, false, KeyTok::Letter('j')),
        );
        assert!(km.match_chord(true, false, true, KeyTok::Letter('p')).is_none());
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Letter('j')),
            Some(Command::PaletteOpen)
        ));
    }

    #[test]
    fn reset_restores_default() {
        let mut km = empty_keymap();
        km.overrides.insert(
            "palette.toggle".into(),
            Chord::new(true, false, false, KeyTok::Letter('j')),
        );
        // reset() persists; in the test we just drop the in-memory override.
        km.overrides.remove("palette.toggle");
        assert!(matches!(
            km.match_chord(true, false, true, KeyTok::Letter('p')),
            Some(Command::PaletteOpen)
        ));
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
    fn keytok_token_roundtrip() {
        for tok in [
            KeyTok::Letter('p'),
            KeyTok::Letter('z'),
            KeyTok::Left,
            KeyTok::Right,
            KeyTok::Up,
            KeyTok::Down,
            KeyTok::F11,
        ] {
            assert_eq!(KeyTok::from_token(&tok.token()), Some(tok));
        }
        assert_eq!(KeyTok::from_token("nope"), None);
    }

    #[test]
    fn rows_cover_every_default_grouped() {
        let km = empty_keymap();
        let rows = km.rows();
        assert_eq!(rows.len(), default_bindings().len());
        // Every row has an id, a label + at least one chord chip, and starts un-overridden.
        assert!(rows.iter().all(|r| !r.id.is_empty() && !r.label.is_empty() && !r.parts.is_empty()));
        assert!(rows.iter().all(|r| !r.overridden));
        // The palette row renders as Ctrl / Shift / P chips under General.
        let palette = rows.iter().find(|r| r.id == "palette.toggle").unwrap();
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
    fn rows_reflect_override() {
        let mut km = empty_keymap();
        km.overrides.insert(
            "palette.toggle".into(),
            Chord::new(true, false, false, KeyTok::Letter('j')),
        );
        let rows = km.rows();
        let palette = rows.iter().find(|r| r.id == "palette.toggle").unwrap();
        assert!(palette.overridden);
        assert_eq!(palette.parts, vec!["Ctrl", "J"]);
    }

    #[test]
    fn exact_modifier_match_required() {
        let km = empty_keymap();
        // Ctrl+Shift+T is New tab; plain T is not bound.
        assert!(km.match_chord(true, false, true, KeyTok::Letter('t')).is_some());
        assert!(km.match_chord(false, false, false, KeyTok::Letter('t')).is_none());
    }
}
