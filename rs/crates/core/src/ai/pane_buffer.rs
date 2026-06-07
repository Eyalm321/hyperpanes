//! Port of `src/main/ai/pane-buffer.ts` — the per-pane rolling, ANSI-stripped tail used as
//! model context (uses `crate::ansi_strip`), with alt-screen / clear-screen / CR handling.
//! Bounded ring. Mirror `pane-buffer.test.ts`.
//!
//! Consumes RAW pty output chunks and keeps a bounded, ANSI-stripped tail. The
//! hard parts (matching the TS):
//!   - detect screen state (alt-screen enter/leave, full clear) from the RAW
//!     chunk BEFORE the ANSI is stripped away,
//!   - reuse `crate::ansi_strip::strip_ansi` (no second ANSI implementation),
//!   - stitch partial lines that straddle chunk boundaries,
//!   - stay bounded in memory per uid (line cap + char cap), for many uids.

use crate::ansi_strip::strip_ansi;
use std::collections::HashMap;

const ESC: u8 = 0x1b; // ESC
const CR: char = '\r'; // carriage return
const CLEAR_SCREEN: &str = "\u{1b}[2J"; // ESC [ 2 J

const DEFAULT_MAX_LINES: usize = 120;
const DEFAULT_MAX_CHARS: usize = 6144;

/// A point-in-time view of a pane's retained tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailSnapshot {
    /// last N ANSI-stripped lines joined with "\n" (includes the pending partial)
    pub text: String,
    /// true while the pane is in an alt-screen / full-screen TUI
    pub alt_screen: bool,
    /// true if appended since the last `mark_clean()`
    pub dirty: bool,
    /// number of lines currently retained (including the pending partial)
    pub lines: usize,
}

#[derive(Default)]
struct PaneState {
    lines: Vec<String>, // completed lines, oldest first
    pending: String,    // partial line not yet terminated by a newline
    alt_screen: bool,
    dirty: bool,
}

pub struct PaneTailBuffer {
    max_lines: usize,
    max_chars: usize,
    panes: HashMap<String, PaneState>,
}

impl Default for PaneTailBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneTailBuffer {
    pub fn new() -> Self {
        Self::with_opts(DEFAULT_MAX_LINES, DEFAULT_MAX_CHARS)
    }

    pub fn with_opts(max_lines: usize, max_chars: usize) -> Self {
        Self {
            max_lines,
            max_chars,
            panes: HashMap::new(),
        }
    }

    pub fn append(&mut self, uid: &str, raw_chunk: &str) {
        if raw_chunk.is_empty() {
            return; // tolerate empty chunks: nothing changed
        }

        let max_lines = self.max_lines;
        let max_chars = self.max_chars;
        let state = self.panes.entry(uid.to_string()).or_default();

        // 1. Screen state from the RAW chunk, before stripping. The last toggle wins.
        if let Some(enter) = last_alt_toggle(raw_chunk) {
            state.alt_screen = enter;
        }

        // 2. Full clear: drop everything retained and process only what follows the
        //    last clear in this chunk.
        let mut raw = raw_chunk;
        if let Some(clear_at) = raw.rfind(CLEAR_SCREEN) {
            state.lines.clear();
            state.pending.clear();
            raw = &raw[clear_at + CLEAR_SCREEN.len()..];
        }

        // 3. Strip ANSI, stitch the pending partial onto the front, normalise CRLF,
        //    then split into lines. The trailing fragment becomes the new pending.
        let cleaned = strip_ansi(raw);
        let combined = format!("{}{}", state.pending, cleaned).replace("\r\n", "\n");
        let mut parts: Vec<&str> = combined.split('\n').collect();
        let new_pending = parts.pop().unwrap_or("").to_string();
        state.pending = new_pending;
        for part in parts {
            state.lines.push(apply_carriage_returns(part).to_string());
        }

        // 4. Stay bounded: keep pending and the retained lines under their caps.
        if char_len(&state.pending) > max_chars {
            state.pending = slice_last_chars(&state.pending, max_chars);
        }
        if state.lines.len() > max_lines {
            let drop = state.lines.len() - max_lines;
            state.lines.drain(0..drop);
        }
        enforce_char_cap(state, max_chars);

        state.dirty = true;
    }

    pub fn snapshot(&self, uid: &str) -> TailSnapshot {
        let Some(state) = self.panes.get(uid) else {
            return TailSnapshot {
                text: String::new(),
                alt_screen: false,
                dirty: false,
                lines: 0,
            };
        };
        let mut display: Vec<String> = state.lines.clone();
        if !state.pending.is_empty() {
            display.push(apply_carriage_returns(&state.pending).to_string());
        }
        TailSnapshot {
            text: display.join("\n"),
            alt_screen: state.alt_screen,
            dirty: state.dirty,
            lines: display.len(),
        }
    }

    pub fn mark_clean(&mut self, uid: &str) {
        if let Some(state) = self.panes.get_mut(uid) {
            state.dirty = false;
        }
    }

    pub fn clear(&mut self, uid: &str) {
        self.panes.remove(uid);
    }
}

// A lone `\r` redraws the current line in place; the visible result is whatever
// follows the last carriage return.
fn apply_carriage_returns(line: &str) -> &str {
    match line.rfind(CR) {
        Some(i) => &line[i + 1..],
        None => line,
    }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

// Equivalent of JS `s.slice(-n)` — keep the last `n` characters.
fn slice_last_chars(s: &str, n: usize) -> String {
    let total = char_len(s);
    if total <= n {
        return s.to_string();
    }
    s.chars().skip(total - n).collect()
}

// Trim oldest lines until the retained text is under the char cap. If a single
// line is itself over the cap, truncate it to its last `max_chars`.
fn enforce_char_cap(state: &mut PaneState, max_chars: usize) {
    let mut total = char_count(&state.lines);
    while state.lines.len() > 1 && total > max_chars {
        let removed = state.lines.remove(0);
        total -= char_len(&removed) + 1; // +1 for the joining newline
    }
    if state.lines.len() == 1 && char_len(&state.lines[0]) > max_chars {
        state.lines[0] = slice_last_chars(&state.lines[0], max_chars);
    }
}

fn char_count(lines: &[String]) -> usize {
    if lines.is_empty() {
        return 0;
    }
    let mut n = lines.len() - 1; // joining newlines
    for line in lines {
        n += char_len(line);
    }
    n
}

// Alt-screen toggle: ESC [ ? (1049|47) (h|l). h enters, l leaves. The last toggle
// in the chunk wins; returns Some(true) for enter, Some(false) for leave, None if
// no toggle is present.
fn last_alt_toggle(s: &str) -> Option<bool> {
    let b = s.as_bytes();
    let mut result = None;
    let mut i = 0;
    while i + 3 < b.len() {
        if b[i] == ESC && b[i + 1] == b'[' && b[i + 2] == b'?' {
            let mut j = i + 3;
            let matched = if b[j..].starts_with(b"1049") {
                j += 4;
                true
            } else if b[j..].starts_with(b"47") {
                j += 2;
                true
            } else {
                false
            };
            if matched && j < b.len() && (b[j] == b'h' || b[j] == b'l') {
                result = Some(b[j] == b'h');
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: &str = "\u{1b}";
    const BEL: &str = "\u{07}";
    const CR: &str = "\r";

    #[test]
    fn joins_multi_chunk_append_into_a_correct_snapshot() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", "line1\nline2\n");
        buf.append("a", "line3\n");
        let snap = buf.snapshot("a");
        assert_eq!(snap.text, "line1\nline2\nline3");
        assert_eq!(snap.lines, 3);
        assert!(!snap.alt_screen);
        assert!(snap.dirty);
    }

    #[test]
    fn strips_ansi_via_shared_strip_ansi() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", &format!("{ESC}[31mred{ESC}[0m and {ESC}[1mbold{ESC}[0m\n"));
        assert_eq!(buf.snapshot("a").text, "red and bold");
    }

    #[test]
    fn stitches_a_partial_line_across_chunk_boundaries() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", "foo");
        assert_eq!(buf.snapshot("a").text, "foo"); // pending partial is visible
        buf.append("a", "bar\nbaz");
        let snap = buf.snapshot("a");
        assert_eq!(snap.text, "foobar\nbaz");
        assert_eq!(snap.lines, 2);
        buf.append("a", "qux\n");
        assert_eq!(buf.snapshot("a").text, "foobar\nbazqux");
    }

    #[test]
    fn does_not_split_a_crlf_that_straddles_a_chunk_boundary() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", &format!("done{CR}"));
        buf.append("a", "\nnext\n");
        assert_eq!(buf.snapshot("a").text, "done\nnext");
    }

    #[test]
    fn treats_a_carriage_return_as_an_in_line_redraw() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", &format!("10%{CR}50%{CR}100%\n"));
        assert_eq!(buf.snapshot("a").text, "100%");
    }

    #[test]
    fn retains_only_the_last_max_lines_lines() {
        let mut buf = PaneTailBuffer::with_opts(3, DEFAULT_MAX_CHARS);
        buf.append("a", "l1\nl2\nl3\nl4\nl5\n");
        let snap = buf.snapshot("a");
        assert_eq!(snap.lines, 3);
        assert_eq!(snap.text, "l3\nl4\nl5");
    }

    #[test]
    fn enforces_the_max_chars_hard_cap_by_dropping_oldest_lines() {
        let mut buf = PaneTailBuffer::with_opts(100, 10);
        buf.append("a", "aaaaa\nbbbbb\nccccc\n"); // 3 lines of 5
        let snap = buf.snapshot("a");
        assert_eq!(snap.text, "ccccc");
        assert!(snap.text.chars().count() <= 10);
    }

    #[test]
    fn truncates_a_single_line_that_alone_exceeds_max_chars() {
        let mut buf = PaneTailBuffer::with_opts(DEFAULT_MAX_LINES, 8);
        buf.append("a", &format!("{}\n", "x".repeat(20)));
        let snap = buf.snapshot("a");
        assert_eq!(snap.text, "x".repeat(8));
    }

    #[test]
    fn detects_alt_screen_enter_and_leave_1049_and_47() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", &format!("{ESC}[?1049h"));
        assert!(buf.snapshot("a").alt_screen);
        buf.append("a", &format!("{ESC}[?1049l"));
        assert!(!buf.snapshot("a").alt_screen);

        buf.append("b", &format!("{ESC}[?47h"));
        assert!(buf.snapshot("b").alt_screen);
        buf.append("b", &format!("{ESC}[?47l"));
        assert!(!buf.snapshot("b").alt_screen);
    }

    #[test]
    fn uses_the_last_alt_screen_toggle_in_a_chunk() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", &format!("{ESC}[?1049h{ESC}[?1049l"));
        assert!(!buf.snapshot("a").alt_screen);
    }

    #[test]
    fn resets_retained_lines_on_a_full_clear() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", "keep1\nkeep2\n");
        buf.append("a", &format!("{ESC}[2Jfresh\n"));
        let snap = buf.snapshot("a");
        assert_eq!(snap.text, "fresh");
        assert_eq!(snap.lines, 1);
    }

    #[test]
    fn keeps_only_the_text_after_the_last_clear_in_a_chunk() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", &format!("old{ESC}[2Jnew\n"));
        assert_eq!(buf.snapshot("a").text, "new");
    }

    #[test]
    fn returns_an_empty_snapshot_for_an_unknown_uid_without_panicking() {
        let mut buf = PaneTailBuffer::new();
        assert_eq!(
            buf.snapshot("nope"),
            TailSnapshot {
                text: String::new(),
                alt_screen: false,
                dirty: false,
                lines: 0
            }
        );
        buf.mark_clean("nope"); // safe no-ops
        buf.clear("nope");
    }

    #[test]
    fn tolerates_empty_chunks_no_state_no_dirty() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", "");
        assert_eq!(
            buf.snapshot("a"),
            TailSnapshot {
                text: String::new(),
                alt_screen: false,
                dirty: false,
                lines: 0
            }
        );
    }

    #[test]
    fn mark_clean_clears_the_dirty_flag_until_the_next_append() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", "hi\n");
        assert!(buf.snapshot("a").dirty);
        buf.mark_clean("a");
        assert!(!buf.snapshot("a").dirty);
        buf.append("a", "more\n");
        assert!(buf.snapshot("a").dirty);
    }

    #[test]
    fn clear_drops_all_state_for_a_uid() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", "something\n");
        buf.clear("a");
        assert_eq!(
            buf.snapshot("a"),
            TailSnapshot {
                text: String::new(),
                alt_screen: false,
                dirty: false,
                lines: 0
            }
        );
    }

    #[test]
    fn keeps_uids_independent() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", "alpha\n");
        buf.append("b", "beta\n");
        assert_eq!(buf.snapshot("a").text, "alpha");
        assert_eq!(buf.snapshot("b").text, "beta");
    }

    #[test]
    fn strips_an_osc_title_sequence_from_the_raw_chunk() {
        let mut buf = PaneTailBuffer::new();
        buf.append("a", &format!("{ESC}]0;window title{BEL}real output\n"));
        assert_eq!(buf.snapshot("a").text, "real output");
    }
}
