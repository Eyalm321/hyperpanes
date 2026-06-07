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
pub fn encode_key(text: &str, ctrl: bool, alt: bool, _shift: bool) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }

    // Compare against the Slint special-key codepoints (Key → its char/SharedString). We
    // do this via the enum rather than hardcoding U+F7xx values so it tracks Slint.
    let is = |k: Key| -> bool {
        let s: slint::SharedString = k.into();
        text == s.as_str()
    };

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
        return Some(b"\t".to_vec());
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
}
