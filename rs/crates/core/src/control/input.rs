//! Port of `src/main/control-input.ts` — input normalization for the control API:
//! `submit_newlines` (Windows CR vs LF), `key_to_bytes` / `keys_to_bytes`, and the
//! `NAMED_KEYS` table (enter, ctrl+c, arrows, …). Mirror every case in
//! `control-input.test.ts`. Byte-exact: these feed a real PTY.

// Normalize control-API `send_input` line endings to what the local pty actually
// submits a line on. Windows conpty runs a line on CR (\r), not LF (\n): an agent
// that ends send_input with a bare "\n" types the line but never executes it
// (live finding, 2026-06-05). On Windows, collapse every newline — CRLF or a lone
// LF — to a single CR so "\n" submits exactly as it does on a POSIX pty, where LF
// is itself a canonical line delimiter. No-op off Windows. The platform is a
// parameter (the caller passes the real one) so this stays pure and unit-testable.
pub fn submit_newlines(data: &str, platform: &str) -> String {
    if platform != "win32" {
        return data.to_string();
    }
    data.replace("\r\n", "\r").replace('\n', "\r")
}

// How long to wait between writing `send_input` text and the trailing bare CR
// when `submit` is set. A TUI that has bracketed-paste mode on (e.g. claude's)
// treats text + "\r" arriving in ONE pty read as a paste — the CR lands in the
// input box instead of submitting. Splitting them into two writes a beat apart
// makes the CR a distinct keystroke. ~40 ms is enough for conpty to deliver them
// as separate reads without a human-perceptible lag (live finding, 2026-06-05).
pub const SUBMIT_DELAY_MS: u64 = 40;

// Named-key vocabulary for `send_keys`: a stable, terminal-agnostic name → the
// byte sequence a VT/xterm pty expects. Menus and prompts need real keystrokes
// (the first-run trust dialog wants `enter`; cancelling wants `escape`/`ctrl+c`)
// that a plain `send_input` string can't express. Keep this table the single
// source of truth — it's pure and unit-tested, and the control server writes its
// bytes straight to the pty (NO submit_newlines: these are already the exact
// bytes, e.g. `enter` IS the CR a Windows pty submits on).
fn named_key(k: &str) -> Option<&'static str> {
    Some(match k {
        "enter" => "\r",
        "return" => "\r",
        "escape" => "\x1b",
        "esc" => "\x1b",
        "tab" => "\t",
        "shift+tab" => "\x1b[Z",
        "backtab" => "\x1b[Z",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "pageup" => "\x1b[5~",
        "pgup" => "\x1b[5~",
        "pagedown" => "\x1b[6~",
        "pgdn" => "\x1b[6~",
        "insert" => "\x1b[2~",
        "delete" => "\x1b[3~",
        "del" => "\x1b[3~",
        "backspace" => "\x7f",
        "space" => " ",
        _ => return None,
    })
}

// Resolve one named key to its bytes, or None if unknown. Case/space-insensitive.
// `ctrl+<a-z>` is handled generically (the C0 control code, ctrl+a → 0x01) on top
// of the explicit table above.
pub fn key_to_bytes(key: &str) -> Option<String> {
    let k = key.trim().to_lowercase();
    if let Some(b) = named_key(&k) {
        return Some(b.to_string());
    }
    // /^ctrl\+([a-z])$/ — "ctrl+" (5 bytes) followed by exactly one a-z.
    let bytes = k.as_bytes();
    if bytes.len() == 6 && k.starts_with("ctrl+") {
        let c = bytes[5];
        if c.is_ascii_lowercase() {
            return Some(((c - 96) as char).to_string()); // 'a'(97) → 0x01
        }
    }
    None
}

/// Result of translating a list of named keys to bytes. Mirrors the TS
/// discriminated union `{ ok: true; bytes } | { ok: false; unknown }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeysResult {
    Ok { bytes: String },
    Err { unknown: Vec<String> },
}

// Translate a list of named keys into one byte string to write to the pty.
// Reports EVERY unknown key (not just the first) so a caller fixes them in one
// round-trip. An empty list is a valid no-op write.
pub fn keys_to_bytes(keys: &[&str]) -> KeysResult {
    let mut unknown: Vec<String> = Vec::new();
    let mut bytes = String::new();
    for key in keys {
        match key_to_bytes(key) {
            Some(b) => bytes.push_str(&b),
            None => unknown.push((*key).to_string()),
        }
    }
    if unknown.is_empty() {
        KeysResult::Ok { bytes }
    } else {
        KeysResult::Err { unknown }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── submitNewlines ──────────────────────────────────────────────────────
    #[test]
    fn passes_input_through_untouched_off_windows() {
        assert_eq!(submit_newlines("dir\n", "linux"), "dir\n");
        assert_eq!(submit_newlines("a\r\nb\n", "darwin"), "a\r\nb\n");
    }

    #[test]
    fn collapses_bare_lf_to_cr_on_windows() {
        assert_eq!(submit_newlines("dir\n", "win32"), "dir\r");
    }

    #[test]
    fn collapses_crlf_to_single_cr_on_windows() {
        assert_eq!(submit_newlines("dir\r\n", "win32"), "dir\r");
    }

    #[test]
    fn submits_every_line_of_multiline_input_on_windows() {
        assert_eq!(submit_newlines("a\nb\n", "win32"), "a\rb\r");
    }

    #[test]
    fn leaves_bare_cr_and_newline_free_text_untouched_on_windows() {
        assert_eq!(submit_newlines("echo hi\r", "win32"), "echo hi\r");
        assert_eq!(submit_newlines("no newline", "win32"), "no newline");
    }

    // ── keyToBytes ──────────────────────────────────────────────────────────
    #[test]
    fn maps_the_core_named_keys_to_their_vt_byte_sequences() {
        assert_eq!(key_to_bytes("enter").as_deref(), Some("\r"));
        assert_eq!(key_to_bytes("escape").as_deref(), Some("\x1b"));
        assert_eq!(key_to_bytes("tab").as_deref(), Some("\t"));
        assert_eq!(key_to_bytes("shift+tab").as_deref(), Some("\x1b[Z"));
        assert_eq!(key_to_bytes("up").as_deref(), Some("\x1b[A"));
        assert_eq!(key_to_bytes("down").as_deref(), Some("\x1b[B"));
        assert_eq!(key_to_bytes("left").as_deref(), Some("\x1b[D"));
        assert_eq!(key_to_bytes("right").as_deref(), Some("\x1b[C"));
        assert_eq!(key_to_bytes("backspace").as_deref(), Some("\x7f"));
        assert_eq!(key_to_bytes("pageup").as_deref(), Some("\x1b[5~"));
        assert_eq!(key_to_bytes("pagedown").as_deref(), Some("\x1b[6~"));
    }

    #[test]
    fn is_case_and_whitespace_insensitive_and_accepts_synonyms() {
        assert_eq!(key_to_bytes("  ENTER ").as_deref(), Some("\r"));
        assert_eq!(key_to_bytes("Esc").as_deref(), Some("\x1b"));
        assert_eq!(key_to_bytes("Return").as_deref(), Some("\r"));
        assert_eq!(key_to_bytes("pgdn").as_deref(), Some("\x1b[6~"));
    }

    #[test]
    fn derives_ctrl_letter_as_the_c0_control_code() {
        assert_eq!(key_to_bytes("ctrl+c").as_deref(), Some("\x03"));
        assert_eq!(key_to_bytes("ctrl+d").as_deref(), Some("\x04"));
        assert_eq!(key_to_bytes("ctrl+a").as_deref(), Some("\x01"));
    }

    #[test]
    fn returns_none_for_an_unknown_key() {
        assert_eq!(key_to_bytes("frobnicate"), None);
        assert_eq!(key_to_bytes("ctrl+shift+x"), None);
    }

    // ── keysToBytes ─────────────────────────────────────────────────────────
    #[test]
    fn concatenates_a_sequence_of_keys_into_one_byte_string() {
        assert_eq!(
            keys_to_bytes(&["escape", "enter"]),
            KeysResult::Ok {
                bytes: "\x1b\r".to_string()
            }
        );
    }

    #[test]
    fn treats_an_empty_list_as_a_valid_no_op_write() {
        assert_eq!(
            keys_to_bytes(&[]),
            KeysResult::Ok {
                bytes: String::new()
            }
        );
    }

    #[test]
    fn reports_every_unknown_key_not_just_the_first() {
        assert_eq!(
            keys_to_bytes(&["enter", "nope", "also-bad"]),
            KeysResult::Err {
                unknown: vec!["nope".to_string(), "also-bad".to_string()]
            }
        );
    }
}
