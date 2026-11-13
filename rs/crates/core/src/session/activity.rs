//! Output-activity gate: tells GENUINE output from COSMETIC redraws, so a pane whose only output
//! is an indicator or cursor toggling the same cells back and forth (a Claude tool bullet
//! blinking under a permission dialog, an app-drawn blinking cursor) stops reading `busy`.
//!
//! The gate is byte-level and runs once per flushed output chunk on whichever side observes the
//! `Data` stream (the in-process session pipeline, or the daemon client's shadow). It never parses
//! VTE: the screen mirror stays lazy (see `Shared::screen_pending` in `session_manager`), and the
//! work per chunk is a scan of at most [`COSMETIC_MAX_CHUNK`] bytes.
//!
//! A chunk is a cosmetic CANDIDATE only when its net screen effect can be an in-place redraw:
//! - it is small (at most [`COSMETIC_MAX_CHUNK`] bytes; floods are always real output);
//! - it cannot scroll or append lines (no LF/VT/FF, no `ESC D`/`ESC E`/`ESC M`, no CSI
//!   `S`/`T`/`L`/`M`);
//! - it either prints nothing, or moves the cursor back to redraw (CR, BS, CSI `A`–`H`/`f`/`d`/`` ` ``,
//!   `ESC 7`/`ESC 8`, CSI `s`/`u`). Plain printable text with no repositioning appends, which is
//!   real output (progress dots).
//!
//! A candidate is cosmetic when it repeats one of the last two DISTINCT candidates (an exact
//! repeat, or an A/B toggle). A spinner with three or more frames, or with a changing timer, never
//! repeats either of its last two frames, so it stays genuine. The two-entry history expires after
//! [`COSMETIC_HISTORY_TTL_MS`] without a candidate, so a redraw that happens to repeat long after
//! the last one still counts.

/// Largest chunk (bytes) that can be a cosmetic candidate. A blink frame is ~45 bytes.
pub const COSMETIC_MAX_CHUNK: usize = 1024;

/// The candidate history is forgotten after this long without a candidate chunk.
pub const COSMETIC_HISTORY_TTL_MS: u64 = 5_000;

/// Per-session gate state. Feed every flushed output chunk to [`OutputGate::observe`].
#[derive(Debug, Clone, Default)]
pub struct OutputGate {
    /// Hashes of the last two distinct candidate chunks, most recent first.
    recent: [Option<u64>; 2],
    /// Epoch-ms of the last candidate chunk (for the history TTL).
    last_candidate_at: u64,
}

impl OutputGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify one flushed chunk at `now_ms` (epoch ms). `true` = genuine output, which the
    /// caller stamps as activity; `false` = cosmetic, ignored for activity.
    pub fn observe(&mut self, chunk: &[u8], now_ms: u64) -> bool {
        if !is_candidate(chunk) {
            return true;
        }
        if now_ms.saturating_sub(self.last_candidate_at) > COSMETIC_HISTORY_TTL_MS {
            self.recent = [None, None];
        }
        self.last_candidate_at = now_ms;
        let h = fnv1a(chunk);
        if self.recent[0] == Some(h) {
            return false;
        }
        if self.recent[1] == Some(h) {
            self.recent.swap(0, 1);
            return false;
        }
        self.recent = [Some(h), self.recent[0]];
        true
    }
}

/// The activity inputs a session exposes to `control::server::activity_for`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivitySignal {
    /// Epoch-ms of the last GENUINE output chunk, or `None` if there has been none.
    pub content_at: Option<u64>,
    /// The output cursor (`output_bytes`) right after the last genuine chunk: the key the
    /// awaiting-input classification is cached under. Cosmetic chunks do not move it.
    pub content_cursor: u64,
    /// The cached screen classification `(content_cursor it was computed for, awaiting)`.
    /// Valid only while its cursor equals [`content_cursor`](Self::content_cursor).
    pub awaiting: Option<(u64, bool)>,
}

impl ActivitySignal {
    /// Record a genuine output chunk flushed at `at_ms` that left the cursor at `cursor`.
    pub fn note_genuine(&mut self, at_ms: u64, cursor: u64) {
        self.content_at = Some(at_ms);
        self.content_cursor = cursor;
    }

    /// Whether the current screen content was classified as blocked on a decision.
    pub fn awaiting_now(&self) -> bool {
        self.awaiting == Some((self.content_cursor, true))
    }

    /// Whether the cached classification is missing or was computed for older content.
    pub fn classification_stale(&self) -> bool {
        !matches!(self.awaiting, Some((c, _)) if c == self.content_cursor)
    }
}

/// Could this chunk's net screen effect be an in-place redraw? See the module docs.
fn is_candidate(b: &[u8]) -> bool {
    if b.is_empty() || b.len() > COSMETIC_MAX_CHUNK {
        return false;
    }
    let mut printable = false;
    let mut repositions = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            // LF / VT / FF: a new line (scrolls at the bottom). Always real output.
            b'\n' | 0x0b | 0x0c => return false,
            b'\r' | 0x08 => {
                repositions = true;
                i += 1;
            }
            0x1b => match b.get(i + 1) {
                Some(b'[') => {
                    let mut j = i + 2;
                    while j < b.len() && (0x30..=0x3f).contains(&b[j]) {
                        j += 1;
                    }
                    while j < b.len() && (0x20..=0x2f).contains(&b[j]) {
                        j += 1;
                    }
                    // A truncated sequence: be conservative and call it real output.
                    let Some(&fin) = b.get(j) else {
                        return false;
                    };
                    match fin {
                        // Scroll up/down, insert/delete lines.
                        b'S' | b'T' | b'L' | b'M' => return false,
                        b'A'..=b'H' | b'f' | b'd' | b'`' | b's' | b'u' => repositions = true,
                        _ => {}
                    }
                    i = j + 1;
                }
                // IND / NEL / RI move by a line and can scroll.
                Some(b'D') | Some(b'E') | Some(b'M') => return false,
                // DECSC / DECRC: save / restore the cursor.
                Some(b'7') | Some(b'8') => {
                    repositions = true;
                    i += 2;
                }
                // OSC / DCS / APC / PM / SOS: a string up to BEL or ST, never drawn.
                Some(b']') | Some(b'P') | Some(b'_') | Some(b'^') | Some(b'X') => {
                    let mut j = i + 2;
                    loop {
                        match b.get(j) {
                            None => return false,
                            Some(0x07) => {
                                j += 1;
                                break;
                            }
                            Some(0x1b) if b.get(j + 1) == Some(&b'\\') => {
                                j += 2;
                                break;
                            }
                            Some(_) => j += 1,
                        }
                    }
                    i = j;
                }
                Some(_) => i += 2,
                None => return false,
            },
            c if c < 0x20 || c == 0x7f => i += 1,
            _ => {
                printable = true;
                i += 1;
            }
        }
    }
    !printable || repositions
}

/// 64-bit FNV-1a: cheap, good enough to tell a handful of recent chunks apart.
fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two frames a Claude pane emits while a tool bullet blinks under a permission dialog
    // (recorded shape: cursor home, down 53 rows, grey ● or space, then the cursor back).
    const BLINK_ON: &[u8] =
        b"\x1b[H\r\x1b[53B\x1b[38;2;153;153;153m\xe2\x97\x8f\x1b[39m\x1b[81;1H\x1b[77;2H";
    const BLINK_OFF: &[u8] = b"\x1b[H\r\x1b[53B\x1b[38;2;153;153;153m \x1b[39m\x1b[81;1H\x1b[77;2H";

    // Feed `frames` at `step_ms` intervals; return each frame's verdict (true = genuine).
    fn run(gate: &mut OutputGate, frames: &[Vec<u8>], start: u64, step_ms: u64) -> Vec<bool> {
        frames
            .iter()
            .enumerate()
            .map(|(i, f)| gate.observe(f, start + i as u64 * step_ms))
            .collect()
    }

    fn spinner_frame(glyph: &str, secs: u32) -> Vec<u8> {
        format!(
            "\x1b[H\r\x1b[60B\x1b[38;2;215;119;87m{glyph}\x1b[39m Working\u{2026} ({secs}s \u{b7} esc to interrupt)\x1b[81;1H\x1b[77;2H"
        )
        .into_bytes()
    }

    #[test]
    fn blink_only_stream_is_cosmetic_after_the_first_toggle() {
        let frames: Vec<Vec<u8>> = (0..40)
            .map(|i| (if i % 2 == 0 { BLINK_OFF } else { BLINK_ON }).to_vec())
            .collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 500);
        // The first two frames are new; every later frame toggles back to one of them.
        assert_eq!(&v[..2], &[true, true]);
        assert!(v[2..].iter().all(|g| !g), "{v:?}");
    }

    #[test]
    fn spinner_with_a_changing_timer_is_genuine() {
        let glyphs = [
            "\u{b7}", "\u{2722}", "\u{2733}", "\u{2736}", "\u{273b}", "\u{273d}",
        ];
        let frames: Vec<Vec<u8>> = (0..60)
            .map(|i| spinner_frame(glyphs[i % glyphs.len()], (i / 10) as u32))
            .collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 100);
        assert!(v.iter().all(|g| *g), "{v:?}");
    }

    #[test]
    fn four_glyph_spinner_without_a_timer_is_genuine() {
        let frames: Vec<Vec<u8>> = (0..40)
            .map(|i| format!("\r{} building", ["|", "/", "-", "\\"][i % 4]).into_bytes())
            .collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 120);
        assert!(v.iter().all(|g| *g), "{v:?}");
    }

    #[test]
    fn cursor_visibility_toggle_is_cosmetic() {
        let frames: Vec<Vec<u8>> = (0..20)
            .map(|i| {
                (if i % 2 == 0 { "\x1b[?25l" } else { "\x1b[?25h" })
                    .as_bytes()
                    .to_vec()
            })
            .collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 530);
        assert!(v[2..].iter().all(|g| !g), "{v:?}");
    }

    #[test]
    fn repeated_heartbeat_lines_are_genuine() {
        let frames: Vec<Vec<u8>> = (0..10).map(|_| b"tick\r\n".to_vec()).collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 1_000);
        assert!(v.iter().all(|g| *g), "{v:?}");
    }

    #[test]
    fn progress_dots_are_genuine() {
        let frames: Vec<Vec<u8>> = (0..10).map(|_| b".".to_vec()).collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 1_000);
        assert!(v.iter().all(|g| *g), "{v:?}");
    }

    #[test]
    fn a_repeated_flood_is_genuine() {
        let chunk = "\x1b[H".to_string() + &"y".repeat(4096);
        let frames: Vec<Vec<u8>> = (0..10).map(|_| chunk.clone().into_bytes()).collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 16);
        assert!(v.iter().all(|g| *g), "{v:?}");
    }

    #[test]
    fn scrolling_escapes_are_genuine() {
        for chunk in [
            &b"\x1b[H\x1b[2S"[..],
            b"\x1b[5;1H\x1b[L",
            b"\x1b[H\x1bM",
            b"\x1b[H\x1bD",
        ] {
            let mut gate = OutputGate::new();
            assert!((0..5).all(|i| gate.observe(chunk, 1_000 + i * 500)));
        }
    }

    #[test]
    fn history_expires_after_the_ttl() {
        let mut gate = OutputGate::new();
        assert!(gate.observe(BLINK_ON, 1_000));
        assert!(gate.observe(BLINK_OFF, 1_500));
        assert!(!gate.observe(BLINK_ON, 2_000));
        // Within the TTL the repeat is still cosmetic.
        assert!(!gate.observe(BLINK_OFF, 2_000 + COSMETIC_HISTORY_TTL_MS));
        // A repeat after a quiet gap longer than the TTL counts again.
        let later = 2_000 + 2 * COSMETIC_HISTORY_TTL_MS + 1;
        assert!(gate.observe(BLINK_ON, later));
        assert!(gate.observe(BLINK_OFF, later + 500));
        assert!(!gate.observe(BLINK_ON, later + 1_000));
    }

    #[test]
    fn genuine_output_in_between_keeps_the_blink_cosmetic() {
        let mut gate = OutputGate::new();
        gate.observe(BLINK_ON, 1_000);
        gate.observe(BLINK_OFF, 1_500);
        assert!(gate.observe(b"new line of output\r\n", 1_700));
        assert!(!gate.observe(BLINK_ON, 2_000));
    }

    #[test]
    fn unterminated_sequences_are_genuine() {
        let mut gate = OutputGate::new();
        for chunk in [&b"\x1b[12"[..], b"\x1b]0;title", b"\x1b"] {
            assert!((0..4).all(|i| gate.observe(chunk, 1_000 + i * 500)));
        }
    }

    #[test]
    fn a_title_only_toggle_is_cosmetic() {
        let frames: Vec<Vec<u8>> = (0..10)
            .map(|i| format!("\x1b]0;{} task\x07", if i % 2 == 0 { "a" } else { "b" }).into_bytes())
            .collect();
        let v = run(&mut OutputGate::new(), &frames, 1_000, 500);
        assert!(v[2..].iter().all(|g| !g), "{v:?}");
    }

    #[test]
    fn signal_classification_is_keyed_by_the_content_cursor() {
        let mut s = ActivitySignal {
            content_at: Some(1),
            content_cursor: 10,
            awaiting: None,
        };
        assert!(s.classification_stale());
        assert!(!s.awaiting_now());
        s.awaiting = Some((10, true));
        assert!(!s.classification_stale());
        assert!(s.awaiting_now());
        s.content_cursor = 11;
        assert!(s.classification_stale());
        assert!(!s.awaiting_now());
    }
}
