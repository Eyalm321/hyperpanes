//! Port of `src/main/control-output.ts` — the read-path pure cores:
//! `wait_decision` / `next_poll_delay` / `slice_since` / `detect_awaiting_input`
//! (powers `waitForIdle`, the `since` delta cursor, and awaiting-input detection).
//! Mirror every case in `control-output.test.ts` — including the rendered-screen
//! fixtures (trust dialog vs idle box vs working spinner vs menu tail).
//!
//! ⚠ PARITY TRAP — UTF-16 cursor units. The TS `since` cursor counts JS string
//! `.length` = UTF-16 code units, and `sliceSince` slices by that count. Rust
//! `String::len()` / byte slicing is UTF-8 bytes — they DIFFER for non-ASCII, and
//! MCP clients persist these cursors across reads. So `slice_since` operates over
//! `encode_utf16()` code units (matching JS exactly), NOT bytes. See the
//! `slices_by_utf16_code_units_not_utf8_bytes` test, which would fail under a naive
//! byte-slicing port.

// Defaults for `read_pane({ waitForIdle })`. The settle window is DELIBERATELY
// short — a single interactive turn — and unrelated to useIdle's 10 s glow
// threshold (idleAlertSeconds), which is far too slow for chat. The activity
// busy→idle flip is the coarse signal; settleMs is the fine one.
pub const DEFAULT_SETTLE_MS: i64 = 600;
pub const DEFAULT_WAIT_TIMEOUT_MS: i64 = 30_000;
// How often the server re-checks quiescence while waiting. Bounded so a long
// settle window doesn't spin and a short one still resolves promptly.
pub const WAIT_POLL_MIN_MS: i64 = 25;
pub const WAIT_POLL_MAX_MS: i64 = 100;

/// Outcome of a single `wait_decision` poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitVerdict {
    Settled,
    Timeout,
    Wait,
}

// Decide, from the current tracking snapshot, whether a `waitForIdle` read should
// resolve now (output quiet for settleMs), give up (timed out), or keep waiting.
//
// `since` makes the wait composable for a type→read turn (prompt_pane): when
// given, the read won't settle until output has actually advanced PAST that
// cursor — i.e. the agent has begun replying — so a stale-but-quiet screen from
// before the prompt can't satisfy the wait. Without `since`, an already-quiet
// pane settles immediately (the plain "wait until it's done" case).
//
// `last_output_at`: ms epoch of the pane's last pty output, or None if none yet.
// `total_bytes`: monotonic count of bytes ever emitted by the pane.
// `since`: require total_bytes to exceed this before settling.
pub fn wait_decision(
    last_output_at: Option<i64>,
    total_bytes: i64,
    since: Option<i64>,
    now: i64,
    start: i64,
    settle_ms: i64,
    timeout_ms: i64,
) -> WaitVerdict {
    let advanced = match since {
        None => true,
        Some(s) => total_bytes > s,
    };
    // A pane that has never produced output counts as quiet (nothing is streaming).
    let quiet = match last_output_at {
        None => true,
        Some(l) => now - l >= settle_ms,
    };
    if advanced && quiet {
        WaitVerdict::Settled
    } else if now - start >= timeout_ms {
        WaitVerdict::Timeout
    } else {
        WaitVerdict::Wait
    }
}

// How long to sleep before the next quiescence check: just past the point the
// pane would become quiet, clamped to the poll band and never overshooting the
// deadline. Pure so the cadence is testable.
pub fn next_poll_delay(
    last_output_at: Option<i64>,
    now: i64,
    start: i64,
    settle_ms: i64,
    timeout_ms: i64,
) -> i64 {
    let until_quiet = match last_output_at {
        None => WAIT_POLL_MIN_MS,
        Some(l) => settle_ms - (now - l),
    };
    let until_deadline = timeout_ms - (now - start);
    // Aim for the quiet point, clamped to the poll band so we neither spin nor
    // sleep so long we lag a settle. But the deadline always wins — never sleep
    // past it (even below the band), so the wait returns its `timeout` on time.
    let target = until_quiet.max(WAIT_POLL_MIN_MS).min(WAIT_POLL_MAX_MS);
    1.max(target.min(until_deadline))
}

/// The slice of pane output produced since a byte cursor. Mirrors the TS
/// `SinceSlice` interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinceSlice {
    /// Bytes produced since the cursor (best-effort; see `truncated`).
    pub output: String,
    /// Next cursor to pass back — the pane's current `total_bytes`.
    pub cursor: i64,
    /// True if the cursor fell off the back of the replay buffer (output was lost).
    pub truncated: bool,
}

// Return only the output produced since a cursor, against the pane's rolling
// replay buffer (which holds at most the last N units ever emitted). `total_bytes`
// is the monotonic count of ALL units emitted, so:
//   • since >= total_bytes  → nothing new (also covers a stale/ahead cursor);
//   • since within buffer    → the exact tail slice;
//   • since older than the buffer holds → the whole buffer, flagged truncated
//     (older output between the cursor and the buffer start was already evicted).
// The returned cursor is always total_bytes, so the next delta read continues cleanly.
//
// ⚠ The cursor counts UTF-16 code units, matching the JS `.length` the control
// server persists — we slice over `encode_utf16()` units, NOT UTF-8 bytes, so the
// cursor arithmetic is byte-parity with the TS source for non-ASCII output.
pub fn slice_since(replay: &str, total_bytes: i64, since: i64) -> SinceSlice {
    if since >= total_bytes {
        return SinceSlice { output: String::new(), cursor: total_bytes, truncated: false };
    }
    // UTF-16 code units — the same unit JS `String#length` / `String#slice` count.
    let units: Vec<u16> = replay.encode_utf16().collect();
    let replay_len = units.len() as i64;
    let new_bytes = total_bytes - since.max(0);
    if new_bytes >= replay_len {
        // The cursor predates the buffer's oldest retained unit (or since < 0): we
        // can't reconstruct the gap, so hand back everything we still hold.
        return SinceSlice {
            output: replay.to_string(),
            cursor: total_bytes,
            truncated: new_bytes > replay_len,
        };
    }
    let start = (replay_len - new_bytes) as usize;
    // from_utf16_lossy mirrors JS's tolerance of a slice that splits a surrogate
    // pair (JS keeps the lone surrogate; we substitute U+FFFD). Cursors are kept
    // in whole code units by callers, so in practice the slice lands on a boundary.
    let output = String::from_utf16_lossy(&units[start..]);
    SinceSlice { output, cursor: total_bytes, truncated: false }
}

// Patterns that mark a TUI's last visible line as a prompt WAITING for the user
// (interactive-pane-driving plan C2). Idle alone can't tell "agent finished" from
// "agent blocked on a y/n / trust prompt"; matched against the rendered screen's
// last non-empty line, these let an orchestrator know to ANSWER rather than wait
// forever. Deliberately conservative — explicit prompt markers, not any prose.
//
// The TS source uses regexes; this port matches the same intent with hand-rolled
// scanners (no `regex` crate is available). Each helper below maps to one pattern:
//   /\(y\/n\)/i  /\[y\/n\]/i  /\(yes\/no\)/i
//   /press\s+(enter|return|any key)/i
//   /\benter to (confirm|continue)\b/i
//   /\bdo you (want|wish|trust)\b/i
//   /❯/
//   /\?\s*$/

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

// `\b<sub>\b` — find `sub` (already lowercase) with a word boundary on each side.
fn bounded_contains(s: &str, sub: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = s[start..].find(sub) {
        let idx = start + pos;
        let end = idx + sub.len();
        let before_ok = idx == 0 || !is_word(s[..idx].chars().next_back().unwrap());
        let after_ok = end == s.len() || !is_word(s[end..].chars().next().unwrap());
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
        if start > s.len() {
            break;
        }
    }
    false
}

// `press\s+(enter|return|any key)` over a lowercase line.
fn matches_press_key(s: &str) -> bool {
    let b = s.as_bytes();
    let needle = b"press";
    let mut i = 0;
    while i + needle.len() <= b.len() {
        if &b[i..i + needle.len()] == needle {
            let mut j = i + needle.len();
            let mut saw_ws = false;
            while j < b.len() && (b[j] as char).is_whitespace() {
                saw_ws = true;
                j += 1;
            }
            if saw_ws {
                let rest = &s[j..];
                if rest.starts_with("enter")
                    || rest.starts_with("return")
                    || rest.starts_with("any key")
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

// Best-effort "is this pane blocked on a prompt?" over the RENDERED screen text
// (clean — run it on a mode:"screen" read, not the mangled raw stream). Looks at
// the last non-empty line only. A heuristic, not a guarantee.
pub fn detect_awaiting_input(screen_text: &str) -> bool {
    let lines: Vec<&str> = screen_text.split('\n').collect();
    let mut i = lines.len() as isize - 1;
    while i >= 0 && lines[i as usize].trim().is_empty() {
        i -= 1;
    }
    if i < 0 {
        return false;
    }
    let last = lines[i as usize].trim();
    let lower = last.to_lowercase();

    lower.contains("(y/n)")
        || lower.contains("[y/n]")
        || lower.contains("(yes/no)")
        || matches_press_key(&lower)
        || bounded_contains(&lower, "enter to confirm")
        || bounded_contains(&lower, "enter to continue")
        || bounded_contains(&lower, "do you want")
        || bounded_contains(&lower, "do you wish")
        || bounded_contains(&lower, "do you trust")
        || last.contains('❯')
        || last.ends_with('?')
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── waitDecision ────────────────────────────────────────────────────────
    // base = { totalBytes: 100, since: None, now: 1000, start: 0, settleMs: 600, timeoutMs: 30000 }
    #[test]
    fn settles_once_output_has_been_quiet_for_settle_ms() {
        assert_eq!(
            wait_decision(Some(1000 - 600), 100, None, 1000, 0, 600, 30000),
            WaitVerdict::Settled
        );
        assert_eq!(
            wait_decision(Some(1000 - 599), 100, None, 1000, 0, 600, 30000),
            WaitVerdict::Wait
        );
    }

    #[test]
    fn treats_a_pane_that_never_emitted_output_as_quiet() {
        assert_eq!(
            wait_decision(None, 100, None, 1000, 0, 600, 30000),
            WaitVerdict::Settled
        );
    }

    #[test]
    fn keeps_waiting_while_streaming_until_the_timeout() {
        assert_eq!(
            wait_decision(Some(990), 100, None, 1000, 0, 600, 30000),
            WaitVerdict::Wait
        );
        // Past the deadline with output still recent → give up.
        assert_eq!(
            wait_decision(Some(29990), 100, None, 30001, 0, 600, 30000),
            WaitVerdict::Timeout
        );
    }

    #[test]
    fn with_since_will_not_settle_until_output_advances_past_the_cursor() {
        // Quiet, but no new output since the cursor → the reply has not started yet.
        assert_eq!(
            wait_decision(Some(0), 100, Some(100), 1000, 0, 600, 30000),
            WaitVerdict::Wait
        );
        // Output advanced past the cursor and then went quiet → settle.
        assert_eq!(
            wait_decision(Some(0), 250, Some(100), 1000, 0, 600, 30000),
            WaitVerdict::Settled
        );
    }

    #[test]
    fn with_since_still_times_out_if_output_never_arrives() {
        assert_eq!(
            wait_decision(None, 100, Some(100), 30001, 0, 600, 30000),
            WaitVerdict::Timeout
        );
    }

    // ── nextPollDelay ───────────────────────────────────────────────────────
    #[test]
    fn sleeps_until_just_past_the_quiet_point_clamped_to_the_poll_band() {
        // 500ms since last output, settle 600 → ~100ms until quiet (== max band).
        assert_eq!(next_poll_delay(Some(500), 1000, 0, 600, 30000), WAIT_POLL_MAX_MS);
        // Output just arrived (still ~590ms from quiet) → re-check at the band max.
        assert_eq!(next_poll_delay(Some(990), 1000, 0, 600, 30000), WAIT_POLL_MAX_MS);
        // Almost quiet (only ~10ms left in the settle window) → floor at the band min.
        assert_eq!(next_poll_delay(Some(410), 1000, 0, 600, 30000), WAIT_POLL_MIN_MS);
    }

    #[test]
    fn never_overshoots_the_remaining_deadline_even_below_the_poll_band() {
        assert_eq!(next_poll_delay(Some(1000), 1010, 0, 600, 1020), 10);
    }

    // ── sliceSince ──────────────────────────────────────────────────────────
    #[test]
    fn returns_nothing_new_when_cursor_at_or_ahead_of_total() {
        assert_eq!(
            slice_since("abcdef", 6, 6),
            SinceSlice { output: String::new(), cursor: 6, truncated: false }
        );
        assert_eq!(
            slice_since("abcdef", 6, 99),
            SinceSlice { output: String::new(), cursor: 6, truncated: false }
        );
    }

    #[test]
    fn returns_the_exact_tail_slice_for_a_cursor_inside_the_buffer() {
        assert_eq!(
            slice_since("abcdef", 6, 4),
            SinceSlice { output: "ef".to_string(), cursor: 6, truncated: false }
        );
        assert_eq!(
            slice_since("abcdef", 6, 0),
            SinceSlice { output: "abcdef".to_string(), cursor: 6, truncated: false }
        );
    }

    #[test]
    fn flags_truncation_when_cursor_predates_the_retained_buffer() {
        let r = slice_since("uvwxyz", 1000, 10);
        assert_eq!(r.output, "uvwxyz");
        assert_eq!(r.cursor, 1000);
        assert!(r.truncated);
    }

    #[test]
    fn hands_back_the_whole_buffer_without_truncation_when_the_gap_exactly_fits() {
        // newBytes (6) == replay.length (6): everything new is still retained.
        assert_eq!(
            slice_since("abcdef", 6, 0),
            SinceSlice { output: "abcdef".to_string(), cursor: 6, truncated: false }
        );
    }

    // ⚠ PARITY TRAP — non-ASCII cursor. This is the case the handoff demands:
    // it must slice by UTF-16 code units, NOT UTF-8 bytes.
    //
    // replay = "abcé"  →  JS .length == 4 (é = U+00E9 is one UTF-16 unit),
    //                     but .len() (UTF-8 bytes) == 5 (é is 2 bytes).
    // The persisted cursor counts code units, so total_bytes = 4, since = 2.
    // Correct (UTF-16) result: slice the last 2 units → "cé".
    // A naive UTF-8 port would compute start = 5 - 2 = 3, landing in the MIDDLE
    // of é's two bytes — yielding garbage / a panic, never "cé".
    #[test]
    fn slices_by_utf16_code_units_not_utf8_bytes() {
        let replay = "abcé";
        assert_eq!(replay.encode_utf16().count(), 4); // UTF-16 units
        assert_eq!(replay.len(), 5); // UTF-8 bytes — deliberately different
        assert_eq!(
            slice_since(replay, 4, 2),
            SinceSlice { output: "cé".to_string(), cursor: 4, truncated: false }
        );
        // And the whole-buffer / nothing-new paths stay parity for non-ASCII too.
        assert_eq!(
            slice_since(replay, 4, 0),
            SinceSlice { output: "abcé".to_string(), cursor: 4, truncated: false }
        );
        assert_eq!(
            slice_since(replay, 4, 4),
            SinceSlice { output: String::new(), cursor: 4, truncated: false }
        );
    }

    // ── detectAwaitingInput ─────────────────────────────────────────────────
    #[test]
    fn flags_y_n_and_yes_no_prompts() {
        assert!(detect_awaiting_input("Overwrite the file? (y/n)"));
        assert!(detect_awaiting_input("Continue [Y/n]"));
        assert!(detect_awaiting_input("Proceed? (yes/no)"));
    }

    #[test]
    fn flags_trust_dialogs_press_enter_prompts_and_the_claude_prompt_caret() {
        assert!(detect_awaiting_input("Do you trust the files in this folder?"));
        assert!(detect_awaiting_input("Press enter to continue"));
        assert!(detect_awaiting_input("❯ 1. Yes, proceed"));
    }

    #[test]
    fn looks_only_at_the_last_non_empty_line() {
        assert!(detect_awaiting_input("lots of output\nmore\n\nReady? (y/n)\n\n"));
        // A question earlier in the scrollback, but the agent has moved on.
        assert!(!detect_awaiting_input("Are you sure?\nOK, done.\nSpun you up a server."));
    }

    #[test]
    fn does_not_flag_ordinary_completed_output() {
        assert!(!detect_awaiting_input("Spun you up a server on port 3000."));
        assert!(!detect_awaiting_input(""));
        assert!(!detect_awaiting_input("\n\n   \n"));
    }

    // P4b — regression fixtures over REALISTIC rendered screens (the kind a
    // mode:"screen" read produces). `awaitingInput` means "blocked on a decision a
    // human must answer", NOT merely "idle at a prompt box".
    const TRUST_DIALOG: &str = concat!(
        "╭──────────────────────────────────────────────────────────╮\n",
        "│ Do you trust the files in this folder?                     │\n",
        "│                                                            │\n",
        "│ C:\\hyperpanes                                              │\n",
        "│                                                            │\n",
        "│ ❯ 1. Yes, proceed                                          │\n",
        "│   2. No, exit                                              │\n",
        "│                                                            │\n",
        "╰──────────────────────────────────────────────────────────╯\n",
        "   Enter to confirm · Esc to exit\n",
        ""
    );

    // An idle claude session: an empty input box (with the ❯ caret INSIDE it) and a
    // status hint as the last visible line. The ❯ is not the last non-empty line, so
    // — correctly — this does NOT read as blocked.
    const IDLE_PROMPT: &str = concat!(
        "╭──────────────────────────────────────────────────────────╮\n",
        "│ ❯ Try \"edit src/main/session.ts\"                           │\n",
        "╰──────────────────────────────────────────────────────────╯\n",
        "  ⏵⏵ accept edits on (shift+tab to cycle)\n",
        ""
    );

    // Mid-turn: the agent is actively working (spinner + token counter). Not blocked.
    const WORKING: &str = concat!(
        "● Reading session.ts…\n",
        "  ⎿ 173 lines\n",
        "\n",
        "✶ Crafting response… (12s · ↑ 2.1k tokens)\n",
        ""
    );

    // A selection menu whose last visible line IS the cursored option (no footer):
    // a genuine "blocked on a decision" state the ❯ caret is meant to catch.
    const MENU_TAIL: &str = concat!("Select a model:\n", "  1. Opus\n", "❯ 2. Sonnet");

    #[test]
    fn flags_the_blocking_trust_dialog() {
        assert!(detect_awaiting_input(TRUST_DIALOG));
    }

    #[test]
    fn flags_a_cursored_menu_selection_awaiting_a_choice() {
        assert!(detect_awaiting_input(MENU_TAIL));
    }

    #[test]
    fn flags_an_inline_confirm_at_the_bottom_of_a_transcript() {
        assert!(detect_awaiting_input(
            "Applied 3 edits across 2 files.\nRun the tests now? (y/n)"
        ));
    }

    #[test]
    fn does_not_flag_the_idle_prompt_box() {
        assert!(!detect_awaiting_input(IDLE_PROMPT));
    }

    #[test]
    fn does_not_flag_an_agent_that_is_mid_turn_working() {
        assert!(!detect_awaiting_input(WORKING));
    }
}
