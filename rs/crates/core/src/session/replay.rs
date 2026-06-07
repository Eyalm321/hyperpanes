//! Port of the 128KB rolling replay buffer in `src/main/session.ts` (lets a
//! re-attaching view replay recent output instead of showing a blank pane).
//!
//! ⚠ Length and trimming are tracked in **UTF-16 code units**, not UTF-8 bytes, to
//! match the control-output `since`/`sliceSince` cursor (MCP persists those cursors).
//! See `control::output`.
//!
//! The TS original is `this.replay = (this.replay + data).slice(-N)`. JS `slice(-N)`
//! keeps exactly the last N UTF-16 units, which can split a surrogate pair and leave
//! a lone surrogate. A Rust `String` can't hold a lone surrogate, so we trim on whole
//! `char` boundaries instead: when a 2-unit (astral) char straddles the cut point we
//! drop it entirely. The buffer can therefore hold at most one fewer code unit than
//! the JS version at that exact boundary — invisible to terminal output and harmless
//! to the cursor math (the cursor counts total emitted units, independent of how much
//! the buffer still retains).

/// Rolling replay buffer size, in UTF-16 code units. 128KB still restores many
/// screens of recent output while halving resident replay per live pty vs. 256KB.
pub const REPLAY_BUFFER_SIZE: usize = 128 * 1024;

/// Bounded ring of the most recent output for a session. One per live pty.
#[derive(Debug, Default)]
pub struct Replay {
    buf: String,
    /// Length of `buf` in UTF-16 code units (kept incrementally).
    len_u16: usize,
    cap: usize,
}

impl Replay {
    /// A replay buffer with the default [`REPLAY_BUFFER_SIZE`] cap.
    pub fn new() -> Self {
        Self::with_capacity(REPLAY_BUFFER_SIZE)
    }

    /// A replay buffer with an explicit UTF-16 cap (used by tests).
    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: String::new(), len_u16: 0, cap }
    }

    /// Append a flushed chunk, evicting oldest output so the retained length stays at
    /// or below the cap (UTF-16 units). Mirrors `(replay + data).slice(-N)`.
    pub fn append(&mut self, data: &str) {
        self.buf.push_str(data);
        self.len_u16 += data.encode_utf16().count();
        if self.len_u16 <= self.cap {
            return;
        }
        // Drop whole chars from the front until we're within cap.
        let mut to_drop = self.len_u16 - self.cap;
        let mut drop_bytes = 0;
        for ch in self.buf.chars() {
            if to_drop == 0 {
                break;
            }
            let u = ch.len_utf16();
            drop_bytes += ch.len_utf8();
            self.len_u16 -= u;
            // `len_utf16()` is 1 or 2; saturating handles the straddle case where a
            // 2-unit char covers the last needed unit (we drop the whole char).
            to_drop = to_drop.saturating_sub(u);
        }
        self.buf.drain(..drop_bytes);
    }

    /// The retained recent output, replayed into a re-attaching terminal.
    pub fn get(&self) -> &str {
        &self.buf
    }

    /// Retained length in UTF-16 code units (what `control::output::sliceSince` treats
    /// as `replay.length`).
    pub fn len_utf16(&self) -> usize {
        self.len_u16
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_everything_below_the_cap() {
        let mut r = Replay::with_capacity(16);
        r.append("hello");
        r.append(" world");
        assert_eq!(r.get(), "hello world");
        assert_eq!(r.len_utf16(), 11);
    }

    #[test]
    fn evicts_oldest_output_past_the_cap() {
        let mut r = Replay::with_capacity(8);
        r.append("abcdef"); // 6
        r.append("ghij"); // +4 = 10 → keep last 8 → "cdefghij"
        assert_eq!(r.get(), "cdefghij");
        assert_eq!(r.len_utf16(), 8);
    }

    #[test]
    fn a_single_over_cap_append_is_trimmed_to_the_tail() {
        let mut r = Replay::with_capacity(4);
        r.append("0123456789"); // keep last 4
        assert_eq!(r.get(), "6789");
        assert_eq!(r.len_utf16(), 4);
    }

    #[test]
    fn len_is_counted_in_utf16_units() {
        let mut r = Replay::with_capacity(1024);
        r.append("😀"); // 1 char, 2 UTF-16 units
        assert_eq!(r.len_utf16(), 2);
        r.append("a");
        assert_eq!(r.len_utf16(), 3);
    }

    #[test]
    fn trims_on_char_boundaries_dropping_a_straddling_astral_char() {
        // cap 3. Buffer "😀ab" is 4 UTF-16 units (emoji=2, a=1, b=1). Need to drop 1
        // unit; the emoji at the front is 2 units, so it's dropped whole → "ab" (2
        // units), one fewer than a raw JS slice(-3) would keep (a lone low surrogate
        // + "ab"). Documented divergence; never matters for terminal text.
        let mut r = Replay::with_capacity(3);
        r.append("😀ab");
        assert_eq!(r.get(), "ab");
        assert_eq!(r.len_utf16(), 2);
        // Buffer remains valid UTF-8 / has no lone surrogates (it's a Rust String).
        assert!(r.get().chars().all(|c| c != '\u{fffd}'));
    }

    #[test]
    fn incremental_appends_match_a_single_combined_append() {
        let mut a = Replay::with_capacity(8);
        for part in ["ab", "cd", "ef", "gh", "ij"] {
            a.append(part);
        }
        let mut b = Replay::with_capacity(8);
        b.append("abcdefghij");
        assert_eq!(a.get(), b.get());
        assert_eq!(a.get(), "cdefghij");
    }
}
