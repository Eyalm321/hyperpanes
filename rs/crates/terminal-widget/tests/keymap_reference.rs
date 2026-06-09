//! Keymap reference tests — assert [`encode_key`] turns representative key presses +
//! modifiers into the exact PTY byte sequence a VT/xterm shell expects.
//!
//! Modeled on Alacritty's keymap reference: a small, explicit table of (key, modifiers)
//! → bytes. Special (non-printable) keys are addressed through `slint::platform::Key`
//! rather than hardcoded private-use codepoints, so the table tracks Slint exactly the
//! way the encoder itself does.

use hyperpanes_terminal_widget::encode_key;
use slint::platform::Key;

/// The `KeyEvent.text` Slint delivers for a special key (a private-use codepoint).
fn special(k: Key) -> String {
    let s: slint::SharedString = k.into();
    s.to_string()
}

#[test]
fn printable_text_passes_through_unchanged() {
    assert_eq!(encode_key("a", false, false, false), Some(b"a".to_vec()));
    assert_eq!(encode_key("Z", false, false, true), Some(b"Z".to_vec()));
    assert_eq!(encode_key("7", false, false, false), Some(b"7".to_vec()));
}

#[test]
fn arrow_keys_emit_csi_sequences() {
    assert_eq!(
        encode_key(&special(Key::UpArrow), false, false, false),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::DownArrow), false, false, false),
        Some(b"\x1b[B".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::RightArrow), false, false, false),
        Some(b"\x1b[C".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::LeftArrow), false, false, false),
        Some(b"\x1b[D".to_vec())
    );
}

#[test]
fn page_and_home_end_keys_emit_their_sequences() {
    assert_eq!(
        encode_key(&special(Key::PageUp), false, false, false),
        Some(b"\x1b[5~".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::PageDown), false, false, false),
        Some(b"\x1b[6~".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::Home), false, false, false),
        Some(b"\x1b[H".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::End), false, false, false),
        Some(b"\x1b[F".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::Delete), false, false, false),
        Some(b"\x1b[3~".to_vec())
    );
}

#[test]
fn enter_tab_backspace_escape() {
    assert_eq!(
        encode_key(&special(Key::Return), false, false, false),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        encode_key(&special(Key::Tab), false, false, false),
        Some(b"\t".to_vec())
    );
    // Terminals conventionally map Backspace to DEL (0x7f).
    assert_eq!(
        encode_key(&special(Key::Backspace), false, false, false),
        Some(vec![0x7f])
    );
    assert_eq!(
        encode_key(&special(Key::Escape), false, false, false),
        Some(vec![0x1b])
    );
}

#[test]
fn ctrl_letters_map_to_control_bytes() {
    // Ctrl-C -> ETX (0x03), Ctrl-D -> EOT (0x04), case-insensitive.
    assert_eq!(encode_key("c", true, false, false), Some(vec![0x03]));
    assert_eq!(encode_key("C", true, false, false), Some(vec![0x03]));
    assert_eq!(encode_key("d", true, false, false), Some(vec![0x04]));
    assert_eq!(encode_key("a", true, false, false), Some(vec![0x01]));
}

#[test]
fn ctrl_punctuation_maps_to_low_control_bytes() {
    assert_eq!(encode_key(" ", true, false, false), Some(vec![0x00])); // Ctrl-Space -> NUL
    assert_eq!(encode_key("[", true, false, false), Some(vec![0x1b])); // Ctrl-[ -> ESC
    assert_eq!(encode_key("\\", true, false, false), Some(vec![0x1c]));
    assert_eq!(encode_key("]", true, false, false), Some(vec![0x1d]));
}

#[test]
fn alt_prefixes_an_escape() {
    // Alt/Meta sends ESC then the text (e.g. Alt-b for word-back in readline).
    assert_eq!(encode_key("b", false, true, false), Some(vec![0x1b, b'b']));
}

#[test]
fn empty_text_sends_nothing() {
    // A bare modifier press (no text) must not emit bytes.
    assert_eq!(encode_key("", false, false, false), None);
}
