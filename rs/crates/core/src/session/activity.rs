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

/// How long an ask episode survives without a fresh "asking" verdict before it ends. Bridges the
/// short stale windows a dialog screen with a ticking element causes (its content changed, the
/// screen is re-classified ~1 s later), and ends the episode soon after the prompt is answered.
pub const ASK_GRACE_MS: u64 = 3_000;

/// What one render of a pane's screen said, cached under the content cursor it was taken at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenVerdict {
    /// The content cursor the screen was rendered at.
    pub cursor: u64,
    /// `detect_awaiting_input`: the screen waits for input (any prompt). Drives `AwaitingInput`.
    pub awaiting: bool,
    /// `blocks_typed_input`: the screen waits on a DECISION (a permission / confirm dialog, a
    /// picker, a y/n question). Drives the tab's ASK state and the "needs input" alerts.
    pub asking: bool,
    /// The prompt's question line, when one was found.
    pub question: Option<String>,
}

/// One "waiting on a decision" episode of a pane: from the first fresh asking verdict until
/// [`ASK_GRACE_MS`] pass without one. Stepped on a clock by the screen classifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskEpisode {
    /// When the episode began (epoch ms), or `None` while the pane is not asking.
    pub since: Option<u64>,
    /// The latest question seen during the episode.
    pub question: Option<String>,
    /// When a fresh asking verdict was last seen (epoch ms).
    last_fresh_at: u64,
}

impl AskEpisode {
    /// Advance at `now`. `fresh` is the current verdict's question when the pane's CURRENT
    /// content is classified as asking (`Some(None)` = asking, no question found), else `None`.
    /// `started_at` is when that content appeared (the episode start on a new episode).
    pub fn step(&mut self, fresh: Option<Option<&str>>, started_at: u64, now: u64) {
        match fresh {
            Some(question) => {
                if self.since.is_none() {
                    self.since = Some(started_at.min(now));
                }
                self.last_fresh_at = now;
                if let Some(q) = question {
                    self.question = Some(q.to_string());
                }
            }
            None => {
                if self.since.is_some() && now.saturating_sub(self.last_fresh_at) > ASK_GRACE_MS {
                    *self = AskEpisode::default();
                }
            }
        }
    }
}

/// The activity inputs a session exposes to `control::server::activity_for`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivitySignal {
    /// Epoch-ms of the last GENUINE output chunk, or `None` if there has been none.
    pub content_at: Option<u64>,
    /// The output cursor (`output_bytes`) right after the last genuine chunk: the key the
    /// screen verdict is cached under. Cosmetic chunks do not move it.
    pub content_cursor: u64,
    /// The latest screen verdict. Current only while its cursor equals
    /// [`content_cursor`](Self::content_cursor).
    pub verdict: Option<ScreenVerdict>,
    /// The pane's current ask episode (see [`AskEpisode`]).
    pub ask: AskEpisode,
}

impl ActivitySignal {
    /// Record a genuine output chunk flushed at `at_ms` that left the cursor at `cursor`.
    pub fn note_genuine(&mut self, at_ms: u64, cursor: u64) {
        self.content_at = Some(at_ms);
        self.content_cursor = cursor;
    }

    /// The verdict, if it was taken at the current content.
    pub fn current_verdict(&self) -> Option<&ScreenVerdict> {
        self.verdict
            .as_ref()
            .filter(|v| v.cursor == self.content_cursor)
    }

    /// Whether the current screen content was classified as waiting for input.
    pub fn awaiting_now(&self) -> bool {
        self.current_verdict().is_some_and(|v| v.awaiting)
    }

    /// Whether the cached verdict is missing or was taken at older content.
    pub fn classification_stale(&self) -> bool {
        self.current_verdict().is_none()
    }

    /// Step the ask episode at `now` from the current verdict.
    pub fn step_ask(&mut self, now: u64) {
        let fresh = self
            .current_verdict()
            .filter(|v| v.asking)
            .map(|v| v.question.clone());
        let started_at = self.content_at.unwrap_or(now);
        self.ask
            .step(fresh.as_ref().map(|q| q.as_deref()), started_at, now);
    }

    /// `(since, question)` while the pane is in an ask episode.
    pub fn asking(&self) -> Option<(u64, Option<&str>)> {
        self.ask
            .since
            .map(|since| (since, self.ask.question.as_deref()))
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

    fn verdict(cursor: u64, awaiting: bool, asking: bool) -> ScreenVerdict {
        ScreenVerdict {
            cursor,
            awaiting,
            asking,
            question: asking.then(|| "Do you want to proceed?".to_string()),
        }
    }

    #[test]
    fn signal_classification_is_keyed_by_the_content_cursor() {
        let mut s = ActivitySignal {
            content_at: Some(1),
            content_cursor: 10,
            ..Default::default()
        };
        assert!(s.classification_stale());
        assert!(!s.awaiting_now());
        s.verdict = Some(verdict(10, true, false));
        assert!(!s.classification_stale());
        assert!(s.awaiting_now());
        s.content_cursor = 11;
        assert!(s.classification_stale());
        assert!(!s.awaiting_now());
    }

    #[test]
    fn ask_episode_starts_on_a_fresh_verdict_and_ends_after_the_grace() {
        let mut s = ActivitySignal {
            content_at: Some(1_000),
            content_cursor: 10,
            ..Default::default()
        };
        s.step_ask(1_200);
        assert_eq!(s.asking(), None);
        s.verdict = Some(verdict(10, true, true));
        s.step_ask(2_000);
        // The episode starts when the dialog's content appeared.
        assert_eq!(s.asking(), Some((1_000, Some("Do you want to proceed?"))));
        // The content changes (a ticking element): stale for a moment, episode holds.
        s.note_genuine(4_000, 11);
        s.step_ask(4_500);
        assert_eq!(s.asking().map(|a| a.0), Some(1_000));
        // Re-classified as still asking: same episode.
        s.verdict = Some(verdict(11, true, true));
        s.step_ask(4_800);
        assert_eq!(s.asking().map(|a| a.0), Some(1_000));
        // Answered: new content, never re-classified as asking. Ends once the grace passes.
        s.note_genuine(6_000, 20);
        s.step_ask(4_800 + ASK_GRACE_MS);
        assert!(s.asking().is_some());
        s.step_ask(4_800 + ASK_GRACE_MS + 1);
        assert_eq!(s.asking(), None);
        // A later dialog is a new episode with its own start.
        s.note_genuine(20_000, 30);
        s.verdict = Some(verdict(30, true, true));
        s.step_ask(21_000);
        assert_eq!(s.asking().map(|a| a.0), Some(20_000));
    }

    #[test]
    fn a_not_asking_verdict_never_starts_an_episode() {
        let mut s = ActivitySignal {
            content_at: Some(1_000),
            content_cursor: 10,
            verdict: Some(verdict(10, true, false)),
            ..Default::default()
        };
        for now in (2_000..20_000).step_by(500) {
            s.step_ask(now);
        }
        assert_eq!(s.asking(), None);
    }
}
