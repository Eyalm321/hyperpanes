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
//! `%APPDATA%\avada\native-keybindings.json`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use avada_core::layout::navigate::Direction;
use avada_core::persistence::paths;
use serde::{Deserialize, Serialize};

use crate::command::Command;

/// The non-text key tokens a chord can target (mirrors the renderer's normalized
/// `e.key`). Printable chords carry their lowercase character ([`KeyTok::Char`] —
/// letters, digits, and symbols like `=`/`-`/`0`); the named keys are the ones the
/// renderer spells out (`arrowleft`, `tab`, `f11`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTok {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    F11,
    Tab,
    Enter,
    Space,
    Escape,
}

impl KeyTok {
    /// The persisted/normalized token string (mirrors the renderer's `e.key`): a single
    /// lowercase character, or `arrowleft`/`arrowright`/`arrowup`/`arrowdown`/`f11`/`tab`/
    /// `enter`/`space`/`escape`.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn token(self) -> String {
        match self {
            KeyTok::Char(c) => c.to_ascii_lowercase().to_string(),
            KeyTok::Left => "arrowleft".into(),
            KeyTok::Right => "arrowright".into(),
            KeyTok::Up => "arrowup".into(),
            KeyTok::Down => "arrowdown".into(),
            KeyTok::F11 => "f11".into(),
            KeyTok::Tab => "tab".into(),
            KeyTok::Enter => "enter".into(),
            KeyTok::Space => "space".into(),
            KeyTok::Escape => "escape".into(),
        }
    }

    /// Parse a normalized token back into a [`KeyTok`] (the inverse of [`Self::token`]).
    /// Unknown multi-char tokens return `None`.
    #[tracing::instrument(level = "debug", ret)]
    pub fn from_token(s: &str) -> Option<KeyTok> {
        match s {
            "arrowleft" => Some(KeyTok::Left),
            "arrowright" => Some(KeyTok::Right),
            "arrowup" => Some(KeyTok::Up),
            "arrowdown" => Some(KeyTok::Down),
            "f11" => Some(KeyTok::F11),
            "tab" => Some(KeyTok::Tab),
            "enter" => Some(KeyTok::Enter),
            "space" => Some(KeyTok::Space),
            "escape" => Some(KeyTok::Escape),
            _ => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if is_printable(c) => {
                        Some(KeyTok::Char(c.to_ascii_lowercase()))
                    }
                    _ => None,
                }
            }
        }
    }

    /// The display chip for this key (`P`, `←`, `F11`, `Tab`) — the native port of the
    /// renderer's `keyLabel`: arrows show glyphs, named keys are spelled out, a single
    /// character is upper-cased.
    #[tracing::instrument(level = "debug", ret)]
    pub(crate) fn label(self) -> String {
        match self {
            KeyTok::Char(c) => c.to_ascii_uppercase().to_string(),
            KeyTok::Left => "←".into(),
            KeyTok::Right => "→".into(),
            KeyTok::Up => "↑".into(),
            KeyTok::Down => "↓".into(),
            KeyTok::F11 => "F11".into(),
            KeyTok::Tab => "Tab".into(),
            KeyTok::Enter => "Enter".into(),
            KeyTok::Space => "Space".into(),
            KeyTok::Escape => "Esc".into(),
        }
    }
}

/// Whether `c` is a single printable (non-control) character a chord can target.
#[tracing::instrument(level = "debug", ret)]
fn is_printable(c: char) -> bool {
    let u = c as u32;
    u >= 0x20 && u != 0x7f && !(0xe000..=0xf8ff).contains(&u)
}

/// Display label for the chord's primary modifier. Slint remaps modifiers on macOS
/// (control → Command, meta → Control — i_slint_core::input), so the `ctrl` chord slot
/// IS the Command key there: show it as "Cmd" so the Preferences list matches what the
/// user actually presses (Cmd+Shift+P opens the palette on a Mac).
pub const CTRL_LABEL: &str = if cfg!(target_os = "macos") {
    "Cmd"
} else {
    "Ctrl"
};

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
        Chord {
            ctrl,
            alt,
            shift,
            key,
        }
    }
    #[tracing::instrument(level = "debug", ret)]
    fn matches(&self, ctrl: bool, alt: bool, shift: bool, key: KeyTok) -> bool {
        self.ctrl == ctrl && self.alt == alt && self.shift == shift && self.key == key
    }

    /// The chord's pieces in display order, e.g. `["Ctrl", "Shift", "P"]` — the native port
    /// of the renderer's `comboParts`, rendered as `<kbd>` chips in the keybindings list.
    #[tracing::instrument(level = "debug", ret)]
    pub fn parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push(CTRL_LABEL.into());
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
    #[tracing::instrument(level = "debug", ret)]
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
    #[tracing::instrument(level = "debug", ret)]
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
    #[tracing::instrument(level = "debug", ret)]
    fn from(c: Chord) -> Self {
        ChordRepr {
            ctrl: c.ctrl,
            alt: c.alt,
            shift: c.shift,
            key: c.key.token(),
        }
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

/// Category headings, in the order the Preferences keybindings list shows them (an exact
/// mirror of the renderer's `CATEGORY_ORDER` in `src/renderer/keybindings.ts`).
pub const CATEGORY_ORDER: [&str; 4] = ["General", "Tabs", "Panes", "Zoom"];

/// The default keymap — an exact port of the renderer's `BINDING_DEFS`
/// (`src/renderer/keybindings.ts`): same ids, labels, categories and default chords. Order
/// is the display order within each category. (The non-rebindable "Focus pane by number →
/// Alt 1…9" documentation row is rendered by the editor, not a binding here.) Native-only
/// addition: `pane.paste` (Ctrl+V → app-side paste, #9) has no renderer counterpart.
#[tracing::instrument(level = "debug")]
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
        b(
            "palette.toggle",
            true,
            false,
            true,
            Char('p'),
            "General",
            "Command palette",
            Command::PaletteOpen,
        ),
        // The left panel's only keyboard route. Ctrl+B matches the "toggle the side panel"
        // chord editors have trained everyone on, and is free here (Ctrl+F is pane search,
        // Ctrl+V paste — B is unclaimed).
        b(
            "panel.toggle",
            true,
            false,
            false,
            Char('b'),
            "General",
            "Left panel (workspace tree)",
            Command::ToggleLeftPanel,
        ),
        // Tabs
        b(
            "tab.new",
            true,
            false,
            false,
            Char('t'),
            "Tabs",
            "New tab",
            Command::NewTab,
        ),
        b(
            "tab.next",
            true,
            false,
            false,
            Tab,
            "Tabs",
            "Next tab",
            Command::NextTab,
        ),
        b(
            "tab.prev",
            true,
            false,
            true,
            Tab,
            "Tabs",
            "Previous tab",
            Command::PrevTab,
        ),
        b(
            "tab.reopen",
            true,
            false,
            true,
            Char('t'),
            "Tabs",
            "Reopen closed pane or tab",
            Command::ReopenClosedTab,
        ),
        // Panes
        b(
            "pane.focusLeft",
            false,
            true,
            false,
            Left,
            "Panes",
            "Focus pane left",
            Command::FocusDir(Direction::Left),
        ),
        b(
            "pane.focusRight",
            false,
            true,
            false,
            Right,
            "Panes",
            "Focus pane right",
            Command::FocusDir(Direction::Right),
        ),
        b(
            "pane.focusUp",
            false,
            true,
            false,
            Up,
            "Panes",
            "Focus pane up",
            Command::FocusDir(Direction::Up),
        ),
        b(
            "pane.focusDown",
            false,
            true,
            false,
            Down,
            "Panes",
            "Focus pane down",
            Command::FocusDir(Direction::Down),
        ),
        b(
            "pane.toggleZoom",
            false,
            true,
            false,
            Char('z'),
            "Panes",
            "Zoom / unzoom pane",
            Command::ToggleZoom,
        ),
        b(
            "pane.toggleFullscreen",
            false,
            false,
            false,
            F11,
            "Panes",
            "Fullscreen pane",
            Command::ToggleFullscreen,
        ),
        b(
            "pane.search",
            true,
            false,
            false,
            Char('f'),
            "Panes",
            "Search in pane",
            Command::SearchFocused,
        ),
        // Ctrl+V pastes via the app (fresh OS-clipboard read + bracketed paste), matching
        // Windows Terminal. Unbind it to forward a literal 0x16 to the shell instead (#9).
        b(
            "pane.paste",
            true,
            false,
            false,
            Char('v'),
            "Panes",
            "Paste",
            Command::PasteFocused,
        ),
        // Alt+V forwards a literal Ctrl+V (0x16) to the focused pane so an in-pane TUI that reads
        // the OS clipboard itself — Claude Code's image paste — can pull a clipboard IMAGE that
        // the text-only app paste above can't carry through the pty. Alt+V is the shortcut Claude
        // Code documents for "your terminal intercepts Ctrl+V" (which `pane.paste` does). The
        // natural Ctrl+V also auto-forwards when the clipboard holds an image, not text.
        b(
            "pane.pasteImage",
            false,
            true,
            false,
            Char('v'),
            "Panes",
            "Paste image (Alt+V)",
            Command::PasteImageFocused,
        ),
        // The explicit copy gesture, for when copy-on-select is off or there is no selection
        // to copy-on-select from (it then falls back to the hovered link — see
        // `State::copy_pane`). Ctrl+Shift+C
        // everywhere but macOS, where the chord's `ctrl` slot IS Command (see `CTRL_LABEL`) and
        // the platform gesture is a bare Cmd+C. Shifting it there costs nothing: the shell
        // interrupt lives on the *physical* Control key on macOS (`crate::pty_ctrl`), so Cmd+C
        // was never the interrupt to begin with.
        b(
            "pane.copy",
            true,
            false,
            !cfg!(target_os = "macos"),
            Char('c'),
            "Panes",
            "Copy selection",
            Command::CopyFocused,
        ),
        // Zoom (font)
        b(
            "zoom.in",
            true,
            false,
            false,
            Char('='),
            "Zoom",
            "Zoom in (font)",
            Command::FontZoom(1),
        ),
        b(
            "zoom.out",
            true,
            false,
            false,
            Char('-'),
            "Zoom",
            "Zoom out (font)",
            Command::FontZoom(-1),
        ),
        b(
            "zoom.reset",
            true,
            false,
            false,
            Char('0'),
            "Zoom",
            "Reset zoom (font)",
            Command::FontReset,
        ),
    ]
}

/// The default keymap built once and reused. [`default_bindings`] allocates a `Vec` of 16
/// owned `Command`s; the key router consults this table on **every** key event
/// ([`Keymap::match_chord`]) and the menus on every render ([`Keymap::label_for`]), so caching
/// it behind a [`OnceLock`] avoids rebuilding the whole table each time.
#[tracing::instrument(level = "debug")]
fn bindings() -> &'static [Binding] {
    static BINDINGS: OnceLock<Vec<Binding>> = OnceLock::new();
    BINDINGS.get_or_init(default_bindings)
}

/// One row of the Preferences → Keybindings list: its binding id, category, label, the
/// **effective** chord pieces (override or default), whether it's been overridden (so the
/// editor can show a "reset" affordance), and whether it's currently **unbound** (the user
/// cleared its chord, or it lost its chord to another binding — the row shows "Unbound").
pub struct BindingRow {
    pub id: &'static str,
    pub category: &'static str,
    pub label: &'static str,
    pub parts: Vec<String>,
    pub overridden: bool,
    pub unbound: bool,
}

/// The user's per-binding chord overrides (the native port of `useKeybindings`'s persisted
/// `combos`, stored as a sparse override map rather than the full table so bindings added in
/// a later version still get their fresh default). A value of `Some(chord)` rebinds the
/// shortcut; `None` means the user explicitly **unbound** it (no chord fires it); an *absent*
/// id falls back to the compiled-in default.
pub struct Keymap {
    overrides: BTreeMap<String, Option<Chord>>,
}

impl Keymap {
    /// An override-free keymap (pure compiled-in defaults) for tests — `load()` would
    /// read the developer's real `native-keybindings.json` and make tests env-dependent.
    #[cfg(test)]
    pub(crate) fn default_for_tests() -> Self {
        Keymap {
            overrides: BTreeMap::new(),
        }
    }

    /// Load the persisted overrides (an empty map on a missing/corrupt file). Un-parseable
    /// chords fall back to the default, so an older blob never breaks the editor. A `null`
    /// value is an explicit unbind. Ids that no binding in this build declares are **kept**,
    /// not dropped: a module's commands (plan track A4) register bindings only while the
    /// module is installed, and an override the user set for one must survive a disable /
    /// re-enable cycle and a run of the app before that module was loaded. See
    /// [`Keymap::from_json`].
    #[tracing::instrument(level = "debug")]
    pub fn load() -> Self {
        let path = paths::user_data_dir().join("native-keybindings.json");
        match std::fs::read_to_string(&path) {
            Ok(raw) => Self::from_json(&raw),
            Err(_) => Keymap {
                overrides: BTreeMap::new(),
            },
        }
    }

    /// Parse a persisted blob. Corrupt JSON is an empty map; an entry with an un-parseable
    /// chord is skipped (it would fall back to the default anyway); everything else is kept
    /// verbatim, unknown ids included.
    #[tracing::instrument(level = "debug", skip(raw))]
    pub fn from_json(raw: &str) -> Self {
        let mut overrides = BTreeMap::new();
        if let Ok(map) = serde_json::from_str::<BTreeMap<String, Option<ChordRepr>>>(raw) {
            for (id, repr) in map {
                match repr {
                    // a parseable chord rebinds; an un-parseable one falls back to default
                    Some(r) => {
                        if let Some(chord) = r.to_chord() {
                            overrides.insert(id, Some(chord));
                        }
                    }
                    // explicit unbind
                    None => {
                        overrides.insert(id, None);
                    }
                }
            }
        }
        Keymap { overrides }
    }

    /// Persist the overrides atomically (best-effort; a write failure is logged, never fatal).
    /// An unbound binding serializes as `null`.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn save(&self) {
        let path = paths::user_data_dir().join("native-keybindings.json");
        let map: BTreeMap<&String, Option<ChordRepr>> = self
            .overrides
            .iter()
            .map(|(id, c)| (id, c.map(ChordRepr::from)))
            .collect();
        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = paths::write_atomic(&path, json.as_bytes()) {
                    tracing::debug!("keybindings save failed: {e}");
                }
            }
            Err(e) => tracing::debug!("keybindings serialize failed: {e}"),
        }
    }

    /// The effective chord for `id`: the user override if set (which may be `None` = unbound),
    /// else `default`. `None` means nothing fires this binding.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn effective(&self, id: &str, default: Chord) -> Option<Chord> {
        match self.overrides.get(id) {
            Some(opt) => *opt,
            None => Some(default),
        }
    }

    /// Whether `id` currently has a user override (a rebind *or* an explicit unbind).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn is_overridden(&self, id: &str) -> bool {
        self.overrides.contains_key(id)
    }

    /// The effective chord label (e.g. `Ctrl+Shift+Z`) for binding `id` — the native port of the
    /// renderer's `comboLabel(combos[id])`, used to annotate context-menu rows. `None` for an
    /// unknown *or* unbound binding (so the menu shows no shortcut).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn label_for(&self, id: &str) -> Option<String> {
        bindings()
            .iter()
            .find(|b| b.id == id)
            .and_then(|b| self.effective(id, b.chord))
            .map(|c| c.label())
    }

    /// Find the command bound to a live modifier+key combo, consulting each binding's
    /// **effective** chord (override wins over default; an unbound binding never matches),
    /// first match in table order.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn match_chord(&self, ctrl: bool, alt: bool, shift: bool, key: KeyTok) -> Option<Command> {
        if let Some(cmd) = self.match_exact(ctrl, alt, shift, key) {
            return Some(cmd);
        }
        // On layouts where "+" needs Shift, the zoom-in chord arrives as Ctrl+Shift+= (or
        // Ctrl++, which key_tok_from_text normalizes to "="). Retry "=" with Shift dropped so
        // the default Ctrl+= zoom-in still fires, without making every binding Shift-insensitive.
        // The exact pass runs first, so a real binding on Ctrl+Shift+= would still win.
        if shift && key == KeyTok::Char('=') {
            return self.match_exact(ctrl, alt, false, key);
        }
        None
    }

    /// The command whose **effective** chord equals this exact combo (override-first, first match
    /// in table order). The exact half of [`Self::match_chord`].
    #[tracing::instrument(level = "debug", ret, skip(self))]
    fn match_exact(&self, ctrl: bool, alt: bool, shift: bool, key: KeyTok) -> Option<Command> {
        bindings()
            .iter()
            .find(|b| {
                self.effective(b.id, b.chord)
                    .is_some_and(|c| c.matches(ctrl, alt, shift, key))
            })
            .map(|b| b.command.clone())
    }

    /// The id of a *different* binding whose **effective** chord already equals `chord` (the
    /// current owner of that combo), or `None` when the combo is free. Used to "steal" a chord:
    /// rebinding to an in-use combo unbinds its previous owner.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn owner_of(&self, chord: Chord, except: &str) -> Option<&'static str> {
        bindings()
            .iter()
            .find(|b| {
                b.id != except
                    && self
                        .effective(b.id, b.chord)
                        .is_some_and(|c| c.matches(chord.ctrl, chord.alt, chord.shift, chord.key))
            })
            .map(|b| b.id)
    }

    /// Override binding `id` with `chord`, persisting. Unknown ids are ignored.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn set(&mut self, id: &str, chord: Chord) {
        if bindings().iter().any(|b| b.id == id) {
            self.overrides.insert(id.to_string(), Some(chord));
            self.save();
        }
    }

    /// Explicitly unbind `id` (no chord fires it), persisting. Unknown ids are ignored.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn unbind(&mut self, id: &str) {
        if bindings().iter().any(|b| b.id == id) {
            self.overrides.insert(id.to_string(), None);
            self.save();
        }
    }

    /// Reset binding `id` to its default chord (drop any override/unbind), persisting.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reset(&mut self, id: &str) {
        if self.overrides.remove(id).is_some() {
            self.save();
        }
    }

    /// Reset *every* binding to its default (clear all overrides), persisting.
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn reset_all(&mut self) {
        if !self.overrides.is_empty() {
            self.overrides.clear();
            self.save();
        }
    }

    /// Whether any binding is overridden (drives the "Reset all" affordance).
    #[tracing::instrument(level = "debug", ret, skip(self))]
    pub fn any_overridden(&self) -> bool {
        let known: std::collections::HashSet<&str> = bindings().iter().map(|b| b.id).collect();
        self.overrides.keys().any(|id| known.contains(id.as_str()))
    }

    /// The bindings grouped by [`CATEGORY_ORDER`] (category order, then table order) — the
    /// editor's row model, with each row's **effective** chord chips, overridden flag, and
    /// unbound flag. Each category's rows are contiguous so the view can draw a heading per group.
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn rows(&self) -> Vec<BindingRow> {
        let bindings = bindings();
        let mut rows = Vec::with_capacity(bindings.len());
        for category in CATEGORY_ORDER {
            for b in bindings.iter().filter(|b| b.category == category) {
                let effective = self.effective(b.id, b.chord);
                rows.push(BindingRow {
                    id: b.id,
                    category,
                    label: b.label,
                    parts: effective.map(|c| c.parts()).unwrap_or_default(),
                    overridden: self.is_overridden(b.id),
                    unbound: effective.is_none(),
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
        Keymap {
            overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn from_json_keeps_unknown_ids_and_drops_only_unparseable_chords() {
        let raw = r#"{
            "palette.toggle": {"ctrl": true, "key": "p"},
            "module:acme/files:reveal": {"ctrl": true, "shift": true, "key": "r"},
            "module:acme/files:hide": null,
            "pane.paste": {"key": "NotAKey!!"}
        }"#;
        let km = Keymap::from_json(raw);
        assert!(km.is_overridden("palette.toggle"));
        assert!(
            km.is_overridden("module:acme/files:reveal"),
            "an id no binding declares in this build survives the round trip"
        );
        assert!(
            km.is_overridden("module:acme/files:hide"),
            "an unbind survives too"
        );
        assert!(
            !km.is_overridden("pane.paste"),
            "an un-parseable chord falls back to the default"
        );
        // Rows only list declared bindings, so the phantom ids never reach the editor.
        assert!(km.rows().iter().all(|r| !r.id.starts_with("module:")));
    }

    #[test]
    fn any_overridden_ignores_ids_this_build_does_not_declare() {
        let km = Keymap::from_json(r#"{"module:acme/files:reveal": {"ctrl": true, "key": "r"}}"#);
        assert!(
            !km.any_overridden(),
            "no visible override, so no Reset-all affordance"
        );
        let km = Keymap::from_json(r#"{"palette.toggle": null}"#);
        assert!(km.any_overridden());
    }

    #[test]
    fn from_json_on_garbage_is_empty() {
        assert!(Keymap::from_json("not json").overrides.is_empty());
        assert!(Keymap::from_json("").overrides.is_empty());
    }

    #[test]
    fn palette_chord_resolves() {
        let km = empty_keymap();
        assert!(matches!(
            km.match_chord(true, false, true, KeyTok::Char('p')),
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
    fn tab_cycle_and_reopen_resolve() {
        let km = empty_keymap();
        // Ctrl+T = new tab, Ctrl+Tab = next, Ctrl+Shift+Tab = prev, Ctrl+Shift+T = reopen.
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Char('t')),
            Some(Command::NewTab)
        ));
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Tab),
            Some(Command::NextTab)
        ));
        assert!(matches!(
            km.match_chord(true, false, true, KeyTok::Tab),
            Some(Command::PrevTab)
        ));
        assert!(matches!(
            km.match_chord(true, false, true, KeyTok::Char('t')),
            Some(Command::ReopenClosedTab)
        ));
    }

    #[test]
    fn font_zoom_chords_resolve() {
        let km = empty_keymap();
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Char('=')),
            Some(Command::FontZoom(1))
        ));
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Char('-')),
            Some(Command::FontZoom(-1))
        ));
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Char('0')),
            Some(Command::FontReset)
        ));
    }

    #[test]
    fn zoom_in_is_shift_tolerant() {
        let km = empty_keymap();
        // Ctrl+Shift+= (and Ctrl++ via key_tok_from_text normalizing "+"→"=") still zoom in,
        // for layouts where "+" needs Shift. The unshifted Ctrl+= keeps working too.
        assert!(matches!(
            km.match_chord(true, false, true, KeyTok::Char('=')),
            Some(Command::FontZoom(1))
        ));
        // Shift-tolerance is scoped to "=": Shift+other keys aren't silently coerced.
        assert!(km
            .match_chord(true, false, true, KeyTok::Char('-'))
            .is_none());
    }

    #[test]
    fn unbound_combo_is_none() {
        let km = empty_keymap();
        assert!(km
            .match_chord(false, false, false, KeyTok::Char('q'))
            .is_none());
    }

    #[test]
    fn override_wins_over_default() {
        let mut km = empty_keymap();
        // Rebind the palette to Ctrl+J. The old Ctrl+Shift+P no longer fires; Ctrl+J does.
        km.overrides.insert(
            "palette.toggle".into(),
            Some(Chord::new(true, false, false, KeyTok::Char('j'))),
        );
        assert!(km
            .match_chord(true, false, true, KeyTok::Char('p'))
            .is_none());
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Char('j')),
            Some(Command::PaletteOpen)
        ));
    }

    #[test]
    fn owner_of_finds_current_holder() {
        let km = empty_keymap();
        // Ctrl+Shift+P is held by the palette; rebinding another binding to it would steal it.
        assert_eq!(
            km.owner_of(Chord::new(true, false, true, KeyTok::Char('p')), "tab.new"),
            Some("palette.toggle")
        );
        // A free chord has no owner; the holder doesn't count itself.
        assert_eq!(
            km.owner_of(Chord::new(true, false, false, KeyTok::Char('j')), "tab.new"),
            None
        );
        assert_eq!(
            km.owner_of(
                Chord::new(true, false, true, KeyTok::Char('p')),
                "palette.toggle"
            ),
            None
        );
    }

    #[test]
    fn unbound_binding_never_fires() {
        let mut km = empty_keymap();
        km.overrides.insert("palette.toggle".into(), None);
        // The default chord no longer fires, and the row reports unbound.
        assert!(km
            .match_chord(true, false, true, KeyTok::Char('p'))
            .is_none());
        let row = km
            .rows()
            .into_iter()
            .find(|r| r.id == "palette.toggle")
            .unwrap();
        assert!(row.unbound);
        assert!(row.parts.is_empty());
        assert!(km.label_for("palette.toggle").is_none());
    }

    #[test]
    fn reset_restores_default() {
        let mut km = empty_keymap();
        km.overrides.insert(
            "palette.toggle".into(),
            Some(Chord::new(true, false, false, KeyTok::Char('j'))),
        );
        // reset() persists; in the test we just drop the in-memory override.
        km.overrides.remove("palette.toggle");
        assert!(matches!(
            km.match_chord(true, false, true, KeyTok::Char('p')),
            Some(Command::PaletteOpen)
        ));
    }

    #[test]
    fn chord_labels_format() {
        assert_eq!(
            Chord::new(true, false, true, KeyTok::Char('p')).label(),
            format!("{CTRL_LABEL}+Shift+P")
        );
        assert_eq!(
            Chord::new(false, true, false, KeyTok::Left).label(),
            "Alt+←"
        );
        assert_eq!(Chord::new(false, false, false, KeyTok::F11).label(), "F11");
        assert_eq!(
            Chord::new(true, false, true, KeyTok::Tab).label(),
            format!("{CTRL_LABEL}+Shift+Tab")
        );
        assert_eq!(
            Chord::new(true, false, false, KeyTok::Char('=')).label(),
            format!("{CTRL_LABEL}+=")
        );
    }

    #[test]
    fn keytok_token_roundtrip() {
        for tok in [
            KeyTok::Char('p'),
            KeyTok::Char('z'),
            KeyTok::Char('='),
            KeyTok::Char('-'),
            KeyTok::Char('0'),
            KeyTok::Left,
            KeyTok::Right,
            KeyTok::Up,
            KeyTok::Down,
            KeyTok::F11,
            KeyTok::Tab,
            KeyTok::Enter,
            KeyTok::Space,
            KeyTok::Escape,
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
        assert!(rows
            .iter()
            .all(|r| !r.id.is_empty() && !r.label.is_empty() && !r.parts.is_empty()));
        assert!(rows.iter().all(|r| !r.overridden));
        // The palette row renders as Ctrl / Shift / P chips under General.
        let palette = rows.iter().find(|r| r.id == "palette.toggle").unwrap();
        assert_eq!(palette.category, "General");
        assert_eq!(palette.parts, vec![CTRL_LABEL, "Shift", "P"]);
        // Rows are grouped: each category appears as one contiguous block, in CATEGORY_ORDER.
        let mut seen: Vec<&str> = Vec::new();
        for r in &rows {
            if seen.last() != Some(&r.category) {
                assert!(
                    !seen.contains(&r.category),
                    "category {} not contiguous",
                    r.category
                );
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
            Some(Chord::new(true, false, false, KeyTok::Char('j'))),
        );
        let rows = km.rows();
        let palette = rows.iter().find(|r| r.id == "palette.toggle").unwrap();
        assert!(palette.overridden);
        assert_eq!(palette.parts, vec![CTRL_LABEL, "J"]);
    }

    #[test]
    fn ctrl_v_pastes_into_focused_pane() {
        // The #9 fix: Ctrl+V is an app chord (fresh OS-clipboard read) by default…
        let km = empty_keymap();
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Char('v')),
            Some(Command::PasteFocused)
        ));
        // …and explicitly unbinding it restores the literal-0x16 passthrough (no match →
        // on_key falls through to encode_key, which forwards the control char to the pty).
        let mut km = empty_keymap();
        km.overrides.insert("pane.paste".into(), None);
        assert!(km
            .match_chord(true, false, false, KeyTok::Char('v'))
            .is_none());
    }

    #[test]
    fn alt_v_forwards_image_paste_to_focused_pane() {
        // Alt+V (native-only `pane.pasteImage`) resolves to PasteImageFocused, whose handler
        // writes a literal Ctrl+V (0x16) to the focused pane so an in-pane TUI (Claude Code)
        // reads the OS clipboard image itself — the shortcut Claude Code documents for
        // terminals that intercept Ctrl+V (which our `pane.paste` does).
        let km = empty_keymap();
        assert!(matches!(
            km.match_chord(false, true, false, KeyTok::Char('v')),
            Some(Command::PasteImageFocused)
        ));
        // It must stay distinct from the plain Ctrl+V text paste — no cross-talk between the two.
        assert!(matches!(
            km.match_chord(true, false, false, KeyTok::Char('v')),
            Some(Command::PasteFocused)
        ));
        // Bare V and Shift+V are ordinary text, never the image forward.
        assert!(km
            .match_chord(false, false, false, KeyTok::Char('v'))
            .is_none());
        assert!(km
            .match_chord(false, false, true, KeyTok::Char('v'))
            .is_none());
    }

    #[test]
    fn the_copy_chord_is_the_platform_one() {
        // Ctrl+Shift+C everywhere but macOS, where the gesture is a bare Cmd+C (the `ctrl` slot
        // IS Command there). Exactly one of the two is bound on any given platform: the other
        // must fall through, so the unshifted chord can still reach the pty off macOS.
        let km = empty_keymap();
        let shifted = !cfg!(target_os = "macos");
        assert!(matches!(
            km.match_chord(true, false, shifted, KeyTok::Char('c')),
            Some(Command::CopyFocused)
        ));
        assert!(km
            .match_chord(true, false, !shifted, KeyTok::Char('c'))
            .is_none());
    }

    #[test]
    fn exact_modifier_match_required() {
        let km = empty_keymap();
        // Ctrl+Shift+T reopens the last close (pane or tab); plain T is not bound.
        assert!(km
            .match_chord(true, false, true, KeyTok::Char('t'))
            .is_some());
        assert!(km
            .match_chord(false, false, false, KeyTok::Char('t'))
            .is_none());
    }
}
