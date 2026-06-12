//! Translate a Slint key event into the byte sequence a PTY shell expects.
//!
//! Slint delivers key presses as a `KeyEvent { text, modifiers }`, where `text` holds the
//! typed character(s) for printable keys and a private-use codepoint for special keys
//! (matching the `slint::platform::Key` enum). We map the common terminal keys to their
//! VT/xterm escape sequences and synthesize Ctrl-/Alt- combos, so the controller can pipe
//! the result straight to `SessionManager::write`.

use slint::platform::Key;

/// Encode a key press into PTY bytes, or `None` if nothing should be sent (e.g. a bare
/// modifier press). `text` is the Slint `KeyEvent.text`; `ctrl`/`alt`/`shift` are its
/// modifier flags.
pub fn encode_key(text: &str, ctrl: bool, alt: bool, shift: bool) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }

    // Compare against the Slint special-key codepoints (Key → its char/SharedString). We
    // do this via the enum rather than hardcoding U+F7xx values so it tracks Slint.
    let is = |k: Key| -> bool {
        let s: slint::SharedString = k.into();
        text == s.as_str()
    };

    // Shift+PageUp / Shift+PageDown are the scrollback gesture — the controller turns them into
    // a one-page viewport scroll (see [`scroll_page_key`] + `TerminalPane::scroll_page`), so they
    // must NEVER reach the shell. Gate them here too (defense-in-depth) so a direct caller can't
    // leak a CSI ~ into the pty. Plain (un-shifted) PageUp/Down fall through to their sequences.
    if shift && (is(Key::PageUp) || is(Key::PageDown)) {
        return None;
    }

    // ---- special keys → VT/xterm sequences ----
    if is(Key::UpArrow) {
        return Some(b"\x1b[A".to_vec());
    }
    if is(Key::DownArrow) {
        return Some(b"\x1b[B".to_vec());
    }
    if is(Key::RightArrow) {
        return Some(b"\x1b[C".to_vec());
    }
    if is(Key::LeftArrow) {
        return Some(b"\x1b[D".to_vec());
    }
    if is(Key::Home) {
        return Some(b"\x1b[H".to_vec());
    }
    if is(Key::End) {
        return Some(b"\x1b[F".to_vec());
    }
    if is(Key::PageUp) {
        return Some(b"\x1b[5~".to_vec());
    }
    if is(Key::PageDown) {
        return Some(b"\x1b[6~".to_vec());
    }
    if is(Key::Delete) {
        return Some(b"\x1b[3~".to_vec());
    }
    if is(Key::Return) {
        return Some(b"\r".to_vec());
    }
    if is(Key::Backspace) {
        // Terminals conventionally map Backspace to DEL (0x7f).
        return Some(vec![0x7f]);
    }
    if is(Key::Tab) {
        // Shift+Tab is the backtab sequence (CSI Z) — TUIs bind it (e.g. Claude Code's
        // mode cycle). Slint normally reports Shift+Tab as `Key::Backtab` (below), but
        // some backends deliver Tab + the shift modifier instead; handle both.
        return Some(if shift { b"\x1b[Z".to_vec() } else { b"\t".to_vec() });
    }
    if is(Key::Backtab) {
        return Some(b"\x1b[Z".to_vec());
    }
    if is(Key::Escape) {
        return Some(vec![0x1b]);
    }

    // ---- Ctrl-modified keys → control bytes ----
    if ctrl {
        let mut chars = text.chars();
        if let Some(c) = chars.next() {
            if c.is_ascii_alphabetic() {
                // Ctrl-A..Ctrl-Z → 0x01..0x1a
                let b = c.to_ascii_uppercase() as u8 - b'A' + 1;
                return Some(vec![b]);
            }
            match c {
                ' ' | '@' => return Some(vec![0x00]), // Ctrl-Space / Ctrl-@ → NUL
                '[' => return Some(vec![0x1b]),
                '\\' => return Some(vec![0x1c]),
                ']' => return Some(vec![0x1d]),
                '^' => return Some(vec![0x1e]),
                '_' => return Some(vec![0x1f]),
                _ => {}
            }
        }
    }

    // ---- Alt (Meta) → ESC prefix, then the text ----
    if alt {
        let mut v = vec![0x1b];
        v.extend_from_slice(text.as_bytes());
        return Some(v);
    }

    // ---- plain printable text (already shifted/cased by Slint) ----
    Some(text.as_bytes().to_vec())
}

/// Classify a key as the **scrollback** gesture (Shift+PageUp / Shift+PageDown), which scrolls
/// the viewport instead of going to the shell. Returns `Some(true)` for page-up (into history),
/// `Some(false)` for page-down (toward the live edge), and `None` for everything else — including
/// plain (un-shifted) PageUp/PageDown, which still encode to their CSI sequences via
/// [`encode_key`]. The app shell calls this first and, on `Some`, scrolls the focused pane
/// ([`TerminalPane::scroll_page`](crate::pane::TerminalPane::scroll_page)) rather than writing the
/// key to the pty.
pub fn scroll_page_key(text: &str, shift: bool) -> Option<bool> {
    if !shift {
        return None;
    }
    let is = |k: Key| -> bool {
        let s: slint::SharedString = k.into();
        text == s.as_str()
    };
    if is(Key::PageUp) {
        Some(true)
    } else if is(Key::PageDown) {
        Some(false)
    } else {
        None
    }
}

/// True when this key press should drop an active selection highlight (the standard terminal
/// "typing clears the selection" rule). Clearing keys are the ones that *edit* the shell's input
/// line: printable characters plus Enter / Backspace / Delete. Everything else keeps the
/// selection — bare modifiers (which never reach the encode path anyway), Ctrl-/Alt- combos
/// (Ctrl+C interrupt, app chords, Alt-meta sequences) and navigation/special keys, so copying
/// with a chord or arrow-scrolling history can't eat the highlight. The caller only clears the
/// HIGHLIGHT — the key still goes to the shell unmodified (no speculative erase of off-row text).
pub fn clears_selection(text: &str, ctrl: bool, alt: bool) -> bool {
    if ctrl || alt {
        return false;
    }
    let is = |k: Key| -> bool {
        let s: slint::SharedString = k.into();
        text == s.as_str()
    };
    if is(Key::Return) || is(Key::Backspace) || is(Key::Delete) {
        return true;
    }
    is_printable(text, ctrl, alt)
}

/// True for a plain printable character press (ordinary text — no Ctrl/Alt, at/above space, not
/// DEL, and not a Slint private-use special key: `Key::*` map to U+F700-range codepoints inside
/// the BMP private-use area). These are the keys that *replace* a prompt-line selection
/// (type-over), a strict subset of [`clears_selection`].
pub fn is_printable(text: &str, ctrl: bool, alt: bool) -> bool {
    if ctrl || alt {
        return false;
    }
    text.chars().next().is_some_and(|c| {
        let u = c as u32;
        u >= 0x20 && u != 0x7f && !(0xe000..=0xf8ff).contains(&u)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn special(k: Key) -> String {
        let s: slint::SharedString = k.into();
        s.to_string()
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(encode_key("a", false, false, false), Some(b"a".to_vec()));
        assert_eq!(encode_key("A", false, false, true), Some(b"A".to_vec()));
        assert_eq!(encode_key("5", false, false, false), Some(b"5".to_vec()));
    }

    #[test]
    fn enter_and_backspace_and_tab() {
        assert_eq!(encode_key(&special(Key::Return), false, false, false), Some(b"\r".to_vec()));
        assert_eq!(encode_key(&special(Key::Backspace), false, false, false), Some(vec![0x7f]));
        assert_eq!(encode_key(&special(Key::Tab), false, false, false), Some(b"\t".to_vec()));
    }

    #[test]
    fn shift_tab_is_backtab() {
        // Slint's normal delivery: the dedicated Backtab key (with or without the
        // shift flag set).
        assert_eq!(encode_key(&special(Key::Backtab), false, false, true), Some(b"\x1b[Z".to_vec()));
        assert_eq!(encode_key(&special(Key::Backtab), false, false, false), Some(b"\x1b[Z".to_vec()));
        // Defensive: a backend that reports Tab + shift modifier instead.
        assert_eq!(encode_key(&special(Key::Tab), false, false, true), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn arrows_emit_csi() {
        assert_eq!(encode_key(&special(Key::UpArrow), false, false, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&special(Key::LeftArrow), false, false, false), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn ctrl_c_is_etx() {
        assert_eq!(encode_key("c", true, false, false), Some(vec![0x03]));
        assert_eq!(encode_key("C", true, false, false), Some(vec![0x03]));
        assert_eq!(encode_key("d", true, false, false), Some(vec![0x04]));
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(encode_key("b", false, true, false), Some(vec![0x1b, b'b']));
    }

    #[test]
    fn empty_text_sends_nothing() {
        assert_eq!(encode_key("", false, false, false), None);
    }

    #[test]
    fn plain_pageup_down_still_reach_the_shell() {
        // Without Shift, PageUp/PageDown encode to their CSI sequences (unchanged behavior).
        assert_eq!(encode_key(&special(Key::PageUp), false, false, false), Some(b"\x1b[5~".to_vec()));
        assert_eq!(encode_key(&special(Key::PageDown), false, false, false), Some(b"\x1b[6~".to_vec()));
    }

    #[test]
    fn shift_pageup_down_are_gated_from_the_pty() {
        // The scrollback gesture must never leak bytes to the shell.
        assert_eq!(encode_key(&special(Key::PageUp), false, false, true), None);
        assert_eq!(encode_key(&special(Key::PageDown), false, false, true), None);
    }

    #[test]
    fn printables_and_line_edits_clear_the_selection() {
        // Printable characters (any case/shift state — shift isn't consulted).
        assert!(clears_selection("a", false, false));
        assert!(clears_selection("A", false, false));
        assert!(clears_selection("5", false, false));
        assert!(clears_selection(" ", false, false));
        // The line-editing specials.
        assert!(clears_selection(&special(Key::Return), false, false));
        assert!(clears_selection(&special(Key::Backspace), false, false));
        assert!(clears_selection(&special(Key::Delete), false, false));
    }

    #[test]
    fn modifiers_chords_and_navigation_keep_the_selection() {
        // Ctrl-/Alt- combos never clear (Ctrl+C interrupt, app chords, Alt-meta sequences).
        assert!(!clears_selection("c", true, false));
        assert!(!clears_selection("v", true, false));
        assert!(!clears_selection("b", false, true));
        assert!(!clears_selection(&special(Key::Return), true, false));
        // Bare modifier presses (Slint private-use codepoints) never clear.
        assert!(!clears_selection(&special(Key::Control), false, false));
        assert!(!clears_selection(&special(Key::Shift), false, false));
        assert!(!clears_selection(&special(Key::Alt), false, false));
        // Navigation / non-editing specials never clear.
        assert!(!clears_selection(&special(Key::UpArrow), false, false));
        assert!(!clears_selection(&special(Key::PageUp), false, false));
        assert!(!clears_selection(&special(Key::Escape), false, false));
        assert!(!clears_selection(&special(Key::Tab), false, false));
        assert!(!clears_selection(&special(Key::F5), false, false));
        // Empty text (nothing typed) never clears.
        assert!(!clears_selection("", false, false));
    }

    #[test]
    fn scroll_page_key_classifies_shift_pageup_down_only() {
        assert_eq!(scroll_page_key(&special(Key::PageUp), true), Some(true));
        assert_eq!(scroll_page_key(&special(Key::PageDown), true), Some(false));
        // Un-shifted PageUp/Down are NOT scroll keys (they go to the shell).
        assert_eq!(scroll_page_key(&special(Key::PageUp), false), None);
        assert_eq!(scroll_page_key(&special(Key::PageDown), false), None);
        // A plain printable key is never a scroll key.
        assert_eq!(scroll_page_key("a", true), None);
    }
}
