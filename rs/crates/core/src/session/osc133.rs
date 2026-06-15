//! Phase-4 completion signal: a pure, state-free scanner for the **semantic
//! prompt markers** that turn "10 s of output silence" into a precise pane state.
//!
//! Two marker channels, both stripped from the stream after sniffing (exactly like
//! the cwd OSC in [`crate::session::cwd`]), so nothing renders and other terminals
//! ignore them:
//!
//! 1. **Shell semantic prompt — OSC 133** (FinalTerm / iTerm2 / VS Code / WezTerm /
//!    Kitty / Ghostty de-facto standard), emitted from `hp-init.sh`:
//!    * `ESC ] 133 ; A ST`          — prompt about to be drawn (ready for input)
//!    * `ESC ] 133 ; B ST`          — end of prompt / start of typed command (ignored)
//!    * `ESC ] 133 ; C ST`          — command output begins (command is now running)
//!    * `ESC ] 133 ; D ; <code> ST` — command finished, with its exit code
//! 2. **Agent liveness — hyperpanes-private `OSC 9 ; hp ; …`** (beside the existing
//!    `OSC 9 ; 9 ; <cwd>` cwd convention), for long-running TUI agents that never
//!    return to a shell prompt:
//!    * `ESC ] 9 ; hp ; state=busy ST`
//!    * `ESC ] 9 ; hp ; state=awaiting-input ST`
//!    * `ESC ] 9 ; hp ; state=done ST`
//!    * `ESC ] 9 ; hp ; state=error ; code=<n> ST`
//!
//! `ST` = `BEL` (`\007`) or `ESC \`. The scanner mirrors `parse_osc_cwd`: bounded
//! split-across-chunk carry, last-event-wins within a window, fast-reject when there
//! is no ESC and nothing pending. De-duping on change is the caller's job.

/// Bound on a carried, still-incomplete OSC sequence (matches `cwd::OSC_MAX`).
const OSC_MAX: usize = 8192;
const OSC_PREFIX: &str = "\u{1b}]"; // ESC ]
const BEL: char = '\u{07}';
const ST: &str = "\u{1b}\\"; // ST = ESC \

/// Agent-reported liveness from an `OSC 9 ; hp ; state=…` marker. Mirrors the design's
/// `AgentLiveness`; carried verbatim on a [`Marker::Agent`] and re-exported via the
/// session event so the orchestrator sees the agent's own self-report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLiveness {
    Busy,
    AwaitingInput,
    Done,
    Error,
}

impl AgentLiveness {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentLiveness::Busy => "busy",
            AgentLiveness::AwaitingInput => "awaiting-input",
            AgentLiveness::Done => "done",
            AgentLiveness::Error => "error",
        }
    }
}

/// One recognized semantic marker. The pipeline turns each into a `SessionEvent` and
/// updates the liveness mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// `133;A` (or `133;B`) — the shell is at / drawing a prompt → ready for input.
    PromptReady,
    /// `133;C` — the just-typed command's output begins → a command is now running.
    CommandStart,
    /// `133;D` or `133;D;<code>` — the command finished, optionally with its exit code.
    CommandEnd { code: Option<i32> },
    /// `9;hp;state=…` — the program self-reports its liveness.
    Agent { state: AgentLiveness, code: Option<i32> },
}

// Interpret one OSC payload (the bytes between `ESC]` and its terminator) as a marker.
// Anything that is not a recognized prompt/agent marker (a title `0;…`, a cwd `7;…`,
// `9;9;…`, a progress `9;4;…`, …) yields `None`.
fn osc_data_to_marker(data: &str) -> Option<Marker> {
    if let Some(rest) = data.strip_prefix("133;") {
        // The sub-letter is the first char of `rest`; `D` may carry `;<code>`.
        let mut parts = rest.split(';');
        return match parts.next() {
            Some("A") | Some("B") => Some(Marker::PromptReady),
            Some("C") => Some(Marker::CommandStart),
            Some("D") => {
                // `133;D` (no code) or `133;D;<code>`.
                let code = parts.next().and_then(|c| c.trim().parse::<i32>().ok());
                Some(Marker::CommandEnd { code })
            }
            _ => None,
        };
    }
    if let Some(rest) = data.strip_prefix("9;hp;") {
        // Semicolon-separated key=value list, e.g. `state=error;code=2`.
        let mut state: Option<AgentLiveness> = None;
        let mut code: Option<i32> = None;
        for kv in rest.split(';') {
            let (k, v) = match kv.split_once('=') {
                Some(pair) => pair,
                None => continue,
            };
            match k.trim() {
                "state" => {
                    state = match v.trim() {
                        "busy" => Some(AgentLiveness::Busy),
                        "awaiting-input" => Some(AgentLiveness::AwaitingInput),
                        "done" => Some(AgentLiveness::Done),
                        "error" => Some(AgentLiveness::Error),
                        _ => None,
                    };
                }
                "code" => code = v.trim().parse::<i32>().ok(),
                _ => {}
            }
        }
        return state.map(|state| Marker::Agent { state, code });
    }
    None
}

/// Pure, state-free scanner for semantic OSC markers (OSC 133 + OSC 9;hp). Given the
/// `carry` from the previous call and the next raw pty `chunk`, returns EVERY recognized
/// marker in this window (in order) plus the carry to feed the next call. Handles
/// sequences split across chunks (split payload and split prefix) via a bounded carry.
///
/// Returning the full ordered list (not last-wins) matters: a single flush can contain
/// `133;D;0` immediately followed by `133;A`, and the supervisor/liveness mirror want
/// both edges.
pub fn parse_osc_markers(carry: &str, chunk: &str) -> (Vec<Marker>, String) {
    // Fast reject: nothing pending and no ESC anywhere → impossible to hold an OSC.
    if carry.is_empty() && !chunk.contains('\u{1b}') {
        return (Vec::new(), String::new());
    }

    let buf = format!("{carry}{chunk}");
    let mut markers = Vec::new();
    let mut search_from = 0usize;
    loop {
        let start = match buf[search_from..].find(OSC_PREFIX) {
            Some(i) => i + search_from,
            None => break,
        };
        let after_prefix = start + OSC_PREFIX.len();
        let bel_idx = buf[after_prefix..].find(BEL).map(|i| i + after_prefix);
        let st_idx = buf[after_prefix..].find(ST).map(|i| i + after_prefix);
        let (end, term_len) = match (bel_idx, st_idx) {
            (Some(b), st) if st.is_none_or(|s| b < s) => (b, 1),
            (_, Some(s)) => (s, ST.len()),
            _ => break, // incomplete sequence at the tail — handled by carry below
        };
        if let Some(m) = osc_data_to_marker(&buf[after_prefix..end]) {
            markers.push(m);
        }
        search_from = end + term_len;
    }

    // Carry forward only a trailing partial that might complete in the next chunk.
    let mut next_carry = String::new();
    if let Some(last_start) = buf.rfind(OSC_PREFIX) {
        let after = last_start + OSC_PREFIX.len();
        let complete = buf[after..].contains(BEL) || buf[after..].contains(ST);
        if !complete {
            let tail = &buf[last_start..];
            next_carry = if tail.len() > OSC_MAX { String::new() } else { tail.to_string() };
        }
    } else if buf.ends_with('\u{1b}') {
        // The 2-char prefix may be split: a lone trailing ESC starts the next OSC.
        next_carry = "\u{1b}".to_string();
    }

    (markers, next_carry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEL: &str = "\u{07}";
    const ESC: &str = "\u{1b}";

    fn osc(body: &str) -> String {
        format!("{ESC}]{body}{BEL}")
    }

    // ---- the core running→prompt-ready transition (the whole point of phase 4) ----

    #[test]
    fn command_start_then_end_then_prompt_ready_in_order() {
        // A real flush around a finished command: C (running) … D;0 (done) … A (ready).
        let stream = format!(
            "{}some output{}{}",
            osc("133;C"),
            osc("133;D;0"),
            osc("133;A")
        );
        let (markers, carry) = parse_osc_markers("", &stream);
        assert_eq!(
            markers,
            vec![
                Marker::CommandStart,
                Marker::CommandEnd { code: Some(0) },
                Marker::PromptReady,
            ]
        );
        assert_eq!(carry, "");
    }

    #[test]
    fn first_prompt_ready_has_no_preceding_command() {
        // The very first prompt: only A (no D before it).
        let (markers, _) = parse_osc_markers("", &osc("133;A"));
        assert_eq!(markers, vec![Marker::PromptReady]);
    }

    #[test]
    fn command_end_carries_its_exit_code() {
        let (markers, _) = parse_osc_markers("", &osc("133;D;127"));
        assert_eq!(markers, vec![Marker::CommandEnd { code: Some(127) }]);
    }

    #[test]
    fn command_end_without_a_code_is_none() {
        let (markers, _) = parse_osc_markers("", &osc("133;D"));
        assert_eq!(markers, vec![Marker::CommandEnd { code: None }]);
    }

    #[test]
    fn prompt_start_b_also_reads_as_ready() {
        let (markers, _) = parse_osc_markers("", &osc("133;B"));
        assert_eq!(markers, vec![Marker::PromptReady]);
    }

    #[test]
    fn accepts_an_st_terminator() {
        let (markers, _) = parse_osc_markers("", &format!("{ESC}]133;C{ESC}\\"));
        assert_eq!(markers, vec![Marker::CommandStart]);
    }

    // ---- agent liveness channel (OSC 9;hp) ----

    #[test]
    fn agent_busy_awaiting_done() {
        let (m1, _) = parse_osc_markers("", &osc("9;hp;state=busy"));
        assert_eq!(m1, vec![Marker::Agent { state: AgentLiveness::Busy, code: None }]);
        let (m2, _) = parse_osc_markers("", &osc("9;hp;state=awaiting-input"));
        assert_eq!(m2, vec![Marker::Agent { state: AgentLiveness::AwaitingInput, code: None }]);
        let (m3, _) = parse_osc_markers("", &osc("9;hp;state=done"));
        assert_eq!(m3, vec![Marker::Agent { state: AgentLiveness::Done, code: None }]);
    }

    #[test]
    fn agent_error_with_code() {
        let (m, _) = parse_osc_markers("", &osc("9;hp;state=error;code=2"));
        assert_eq!(m, vec![Marker::Agent { state: AgentLiveness::Error, code: Some(2) }]);
    }

    #[test]
    fn agent_unknown_state_is_ignored() {
        let (m, _) = parse_osc_markers("", &osc("9;hp;state=banana"));
        assert!(m.is_empty());
    }

    // ---- non-markers are ignored (no false positives on cwd / title / progress) ----

    #[test]
    fn ignores_cwd_title_and_progress_oscs() {
        let (m1, _) = parse_osc_markers("", &osc("0;my tab title"));
        assert!(m1.is_empty(), "title OSC is not a marker");
        let (m2, _) = parse_osc_markers("", &osc("7;file:///home/me"));
        assert!(m2.is_empty(), "OSC 7 cwd is not a marker");
        let (m3, _) = parse_osc_markers("", &osc("9;9;C:\\proj"));
        assert!(m3.is_empty(), "OSC 9;9 cwd is not a marker");
        let (m4, _) = parse_osc_markers("", &osc("9;4;1;50"));
        assert!(m4.is_empty(), "OSC 9;4 progress is not a marker");
    }

    #[test]
    fn fast_rejects_a_plain_chunk_with_no_esc_and_no_carry() {
        let (m, carry) = parse_osc_markers("", "just some normal output\n");
        assert!(m.is_empty());
        assert_eq!(carry, "");
    }

    // ---- split-across-chunk carry (mirrors the cwd scanner's discipline) ----

    #[test]
    fn carries_a_marker_split_across_two_chunks() {
        let (m_a, carry_a) = parse_osc_markers("", &format!("{ESC}]133;D;"));
        assert!(m_a.is_empty());
        assert_eq!(carry_a, format!("{ESC}]133;D;"));
        let (m_b, carry_b) = parse_osc_markers(&carry_a, &format!("42{BEL}"));
        assert_eq!(m_b, vec![Marker::CommandEnd { code: Some(42) }]);
        assert_eq!(carry_b, "");
    }

    #[test]
    fn carries_a_prefix_split_across_two_chunks() {
        let (m_a, carry_a) = parse_osc_markers("", &format!("output{ESC}]133"));
        assert!(m_a.is_empty());
        assert_eq!(carry_a, format!("{ESC}]133"));
        let (m_b, _) = parse_osc_markers(&carry_a, &format!(";A{BEL}"));
        assert_eq!(m_b, vec![Marker::PromptReady]);
    }

    #[test]
    fn carries_a_bare_trailing_esc() {
        let (_, carry_a) = parse_osc_markers("", &format!("text{ESC}"));
        assert_eq!(carry_a, ESC);
        let (m_b, _) = parse_osc_markers(&carry_a, &format!("]133;C{BEL}"));
        assert_eq!(m_b, vec![Marker::CommandStart]);
    }

    #[test]
    fn abandons_an_oversized_unterminated_sequence() {
        let huge = "x".repeat(20000);
        let (m, carry) = parse_osc_markers("", &format!("{ESC}]133;D;{huge}"));
        assert!(m.is_empty());
        assert_eq!(carry, "");
    }

    #[test]
    fn does_not_retain_a_non_osc_escape_tail() {
        let (m, carry) = parse_osc_markers("", "\u{1b}[0m colored text");
        assert!(m.is_empty());
        assert_eq!(carry, "");
    }
}
