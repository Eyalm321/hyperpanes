//! Port of `src/main/ansi-strip.ts` — strip ANSI escape sequences (CSI, OSC, the
//! two-byte ESC forms) to clean text for a plain-text view of pane output.
//!
//! Printable text, newlines and tabs are left intact so a manager can parse a
//! worker's TUI. The original is three sequential regex `replace` passes — OSC
//! first (so the CSI pass can't nibble an OSC body), then CSI, then the remaining
//! two-byte ESC Fe forms. This port keeps that exact ordering, hand-rolling each
//! pass as a scanner (the `regex` crate is not an available dependency).

const ESC: char = '\u{1b}'; // ESC
const CSI8: char = '\u{9b}'; // 8-bit CSI
const OSC8: char = '\u{9d}'; // 8-bit OSC
const BEL: char = '\u{07}'; // OSC terminator (BEL)

/// Strip ANSI escape sequences, leaving printable text / newlines / tabs intact.
pub fn strip_ansi(input: &str) -> String {
    strip_esc_fe(&strip_csi(&strip_osc(input)))
}

// OSC: (ESC ] | 8-bit OSC) … (BEL | ST = ESC \). Lazy to the terminator. An
// unterminated OSC start is left as-is (the regex match simply fails).
fn strip_osc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let start_len = if chars[i] == ESC && i + 1 < chars.len() && chars[i + 1] == ']' {
            2
        } else if chars[i] == OSC8 {
            1
        } else {
            0
        };
        if start_len > 0 {
            let mut j = i + start_len;
            let mut found = false;
            let mut term_len = 0;
            while j < chars.len() {
                if chars[j] == BEL {
                    term_len = 1;
                    found = true;
                    break;
                }
                if chars[j] == ESC && j + 1 < chars.len() && chars[j + 1] == '\\' {
                    term_len = 2;
                    found = true;
                    break;
                }
                j += 1;
            }
            if found {
                i = j + term_len;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// CSI: (ESC [ | 8-bit CSI) params(0x30-0x3F) intermediates(0x20-0x2F) final(0x40-0x7E).
fn strip_csi(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let start_len = if chars[i] == ESC && i + 1 < chars.len() && chars[i + 1] == '[' {
            2
        } else if chars[i] == CSI8 {
            1
        } else {
            0
        };
        if start_len > 0 {
            let mut j = i + start_len;
            while j < chars.len() && ('\u{30}'..='\u{3f}').contains(&chars[j]) {
                j += 1;
            }
            while j < chars.len() && ('\u{20}'..='\u{2f}').contains(&chars[j]) {
                j += 1;
            }
            if j < chars.len() && ('\u{40}'..='\u{7e}').contains(&chars[j]) {
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// Remaining two-byte ESC Fe sequences (ESC followed by 0x40-0x5F), e.g. ESC M.
fn strip_esc_fe(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ESC && i + 1 < chars.len() && ('\u{40}'..='\u{5f}').contains(&chars[i + 1]) {
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: &str = "\u{1b}";
    const BEL: &str = "\u{07}";

    #[test]
    fn removes_sgr_color_codes_keeps_text() {
        assert_eq!(strip_ansi(&format!("{ESC}[31mred{ESC}[0m text")), "red text");
        assert_eq!(strip_ansi(&format!("{ESC}[1;32mok{ESC}[39m")), "ok");
    }

    #[test]
    fn removes_cursor_erase_csi_sequences() {
        assert_eq!(strip_ansi(&format!("a{ESC}[2Kb{ESC}[Hc")), "abc");
        assert_eq!(strip_ansi(&format!("{ESC}[2J{ESC}[3Jclear")), "clear");
    }

    #[test]
    fn removes_osc_title_sequences_bel_or_st_terminated() {
        assert_eq!(strip_ansi(&format!("{ESC}]0;my title{BEL}body")), "body");
        assert_eq!(strip_ansi(&format!("{ESC}]2;t{ESC}\\after")), "after");
    }

    #[test]
    fn preserves_newlines_tabs_and_punctuation() {
        let s = "line1\n\tline2 — done!";
        assert_eq!(strip_ansi(s), s);
    }

    #[test]
    fn is_a_no_op_on_plain_text() {
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
    }
}
