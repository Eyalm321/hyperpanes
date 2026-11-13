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

use crate::session::activity::{AskKind, ScreenVerdict};

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
    let target = until_quiet.clamp(WAIT_POLL_MIN_MS, WAIT_POLL_MAX_MS);
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
        return SinceSlice {
            output: String::new(),
            cursor: total_bytes,
            truncated: false,
        };
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
    SinceSlice {
        output,
        cursor: total_bytes,
        truncated: false,
    }
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
    let Some(last) = last_content_line(&lines) else {
        return false;
    };
    explicit_prompt(last)
        || last.contains('❯')
        || last.ends_with('?')
        || detect_selection_dialog(&lines)
}

/// Whether text + Enter typed by the app itself (an inbox nudge, a queued prompt, a goal) would
/// land in a prompt that wants a DECISION instead of in the agent's input box, and so must be
/// held: a permission or trust dialog, a picker, a y/n question, "press enter". Narrower than
/// [`detect_awaiting_input`] on purpose: it ignores the generic last-line `❯` and `?` cues, so a
/// shell prompt or a question in the chat never holds a delivery forever. It is the one
/// predicate every automated typing path checks.
pub fn blocks_typed_input(screen_text: &str) -> bool {
    let lines: Vec<&str> = screen_text.split('\n').collect();
    last_content_line(&lines).is_some_and(explicit_prompt)
        || detect_selection_dialog(&lines)
        || shows_option_form(&lines)
}

// The last non-blank row, trimmed.
fn last_content_line<'a>(lines: &[&'a str]) -> Option<&'a str> {
    lines.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty())
}

// An explicit "answer me" phrase on the last row: y/n, yes/no, press a key, enter to confirm, or
// a "do you want / wish / trust" question.
fn explicit_prompt(last: &str) -> bool {
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
}

// An interactive multi-option form (trust prompt, theme / model picker, permission dialog,
// account select): a `❯ N.` cursored option in a run of adjacent rows holding at least two
// numbered options, with no agent input box below it. Anything typed while one is up lands in
// the form. A numbered list in an agent's reply above its input box, a past prompt in the chat
// history, or a shell listing (`1.png`) never matches: the cursor must sit ON a numbered option,
// and the input box below the history rules the history out. (Moved from the app's goal
// delivery.)
fn shows_option_form(lines: &[&str]) -> bool {
    // Rows with their frame stripped; a blank row ends a run of adjacent rows.
    let rows: Vec<&str> = lines.iter().map(|l| strip_frame(l)).collect();
    let input_box = |i: usize| {
        rows[i].starts_with('❯')
            && rows[..i]
                .iter()
                .rev()
                .find(|r| !r.is_empty())
                .is_some_and(|r| is_rule_row(r))
    };
    let Some(sel) = (0..rows.len())
        .rev()
        .find(|&i| selected_option(rows[i]) && !input_box(i))
    else {
        return false;
    };
    if (sel + 1..rows.len()).any(input_box) {
        return false;
    }
    let first = (0..sel)
        .rev()
        .take_while(|&i| !rows[i].is_empty())
        .last()
        .unwrap_or(sel);
    let last = (sel + 1..rows.len())
        .take_while(|&i| !rows[i].is_empty())
        .last()
        .unwrap_or(sel);
    let numbered = (first..=last)
        .filter(|&i| numbered_option(rows[i]) || selected_option(rows[i]))
        .count();
    numbered >= 2
}

// A row that asks something: ends with `?`, or a "do you want / wish / trust" / "would you like".
fn is_question(row: &str) -> bool {
    let lower = row.to_lowercase();
    row.ends_with('?')
        || bounded_contains(&lower, "do you want")
        || bounded_contains(&lower, "do you wish")
        || bounded_contains(&lower, "do you trust")
        || bounded_contains(&lower, "would you like")
}

/// Longest question (chars) a verdict carries; longer ones end in `…`.
pub const QUESTION_MAX_CHARS: usize = 200;

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// A decision dialog found by [`decision_dialog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    pub kind: AskKind,
    /// The question row above the dialog's options, frame stripped, capped at
    /// [`QUESTION_MAX_CHARS`]; `None` when the dialog only has a footer.
    pub question: Option<String>,
}

/// Whether the screen ends in a real decision DIALOG that waits on its owner: what drives the
/// tab's ASK state and the "needs input" alert. Stricter than [`blocks_typed_input`] on purpose:
/// a screen that never changes keeps its verdict, so a false ASK would read ASK, and alert,
/// forever. The typing guard may hold on a doubtful screen; ASK may not fire on one.
///
/// - Only a cursored selection dialog counts: a `❯ N.` option in a block of at least two
///   numbered options, with a question above it or a key-hint footer ("Esc to cancel") below.
///   A bare y/n, "press enter" or "do you want" line never counts: a log tail can end with one,
///   and nothing in the rendered text tells it from a live prompt. Those still read
///   awaiting-input and still hold typed input.
/// - The dialog must be the bottom of the screen. Under its options only the options' own
///   wrapped text, blank rows, rules, key hints and at most [`STATUS_ROWS_MAX`] other rows (a
///   statusline) may follow, and none of them may look like a shell prompt: a dialog printed by
///   `cat` or `grep` in a shell has the shell prompt under it.
/// - An agent input box (a `❯` row under a rule) below the menu means the menu is only quoted in
///   the chat.
/// - A side panel drawn beside the chat (a `│` column most rows share) is cut off first, so its
///   text neither fakes a dialog nor hides one.
pub fn decision_dialog(screen_text: &str) -> Option<Dialog> {
    let lines: Vec<&str> = screen_text.split('\n').collect();
    let cut = cut_side_panel(&lines);
    let rows: Vec<&str> = cut.iter().map(|l| strip_frame(l)).collect();
    let rule: Vec<bool> = rows.iter().map(|r| is_rule_row(r)).collect();
    let content: Vec<usize> = (0..rows.len())
        .filter(|&i| !rows[i].is_empty() && !rule[i])
        .collect();
    let window = &content[content.len().saturating_sub(DIALOG_SCAN_ROWS)..];
    let input_box = |i: usize| {
        rows[i].starts_with('❯')
            && rows[..i]
                .iter()
                .rposition(|r| !r.is_empty())
                .is_some_and(|j| rule[j])
    };
    let &sel = window
        .iter()
        .rev()
        .find(|&&i| selected_option(rows[i]) && !input_box(i))?;
    if (sel + 1..rows.len()).any(input_box) {
        return None;
    }
    // The option block: the rows adjacent to the cursor (a blank row or a rule ends it).
    let in_block = |i: usize| !rows[i].is_empty() && !rule[i];
    let first = (0..sel)
        .rev()
        .take_while(|&i| in_block(i))
        .last()
        .unwrap_or(sel);
    let end = (sel + 1..rows.len())
        .take_while(|&i| in_block(i))
        .last()
        .unwrap_or(sel);
    let is_option = |i: usize| numbered_option(rows[i]) || selected_option(rows[i]);
    if (first..=end).filter(|&i| is_option(i)).count() < 2 {
        return None;
    }
    // After the last option, inside the block: only its wrapped text (indented deeper than the
    // option) or a key hint. A shell prompt right under a printed menu fails here.
    let last_option = (sel..=end).rev().find(|&i| is_option(i)).unwrap_or(sel);
    let option_indent = indent(cut[last_option]);
    if (last_option + 1..=end).any(|i| indent(cut[i]) <= option_indent && !is_key_hint(rows[i])) {
        return None;
    }
    // Below the block: rules, key hints and a short statusline; never a shell prompt.
    let below: Vec<usize> = content.iter().copied().filter(|&i| i > end).collect();
    if below.iter().any(|&i| shell_prompt_row(rows[i]))
        || below.iter().filter(|&&i| !is_key_hint(rows[i])).count() > STATUS_ROWS_MAX
    {
        return None;
    }
    // The question: the nearest question row above the cursor, inside the dialog (below the
    // nearest rule above it, the dialog's top edge, if there is one), never the chat above.
    let top = (0..sel).rev().find(|&i| rule[i]);
    let question = window
        .iter()
        .rev()
        .filter(|&&i| i < sel && top.is_none_or(|t| i > t))
        .find(|&&i| is_question(rows[i]))
        .map(|&i| rows[i]);
    let footers: Vec<String> = window
        .iter()
        .filter(|&&i| i > sel)
        .map(|&i| rows[i].to_lowercase())
        .collect();
    let footer = footers.iter().any(|l| {
        bounded_contains(l, "esc to cancel")
            || bounded_contains(l, "enter to confirm")
            || bounded_contains(l, "esc to exit")
    });
    if question.is_none() && !footer {
        return None;
    }
    let kind = ask_kind(question, &footers);
    Some(Dialog {
        kind,
        question: question.map(|q| cap_chars(q, QUESTION_MAX_CHARS)),
    })
}

/// The most rows other than key hints a dialog may have under its options (a statusline and a
/// mode line), see [`decision_dialog`].
pub const STATUS_ROWS_MAX: usize = 3;

// What the dialog decides, from its question and footer rows: a folder trust prompt, a tool
// permission, or any other choice.
fn ask_kind(question: Option<&str>, footers: &[String]) -> AskKind {
    let q = question.map(str::to_lowercase).unwrap_or_default();
    if bounded_contains(&q, "trust") {
        AskKind::Trust
    } else if bounded_contains(&q, "do you want to")
        || bounded_contains(&q, "allow")
        || bounded_contains(&q, "proceed")
        || footers.iter().any(|f| bounded_contains(f, "tab to amend"))
    {
        AskKind::Permission
    } else {
        AskKind::Choice
    }
}

// A key-hint row: "Esc to cancel · Tab to amend", "Enter to confirm", "↑↓ to navigate",
// "shift+tab to cycle"…
fn is_key_hint(row: &str) -> bool {
    let lower = row.to_lowercase();
    [
        "esc to",
        "enter to",
        "tab to",
        "to navigate",
        "to select",
        "to cancel",
    ]
    .iter()
    .any(|k| bounded_contains(&lower, k))
        || lower.contains("ctrl+")
        || lower.contains("shift+tab")
        || lower.contains('↑')
}

// A row that looks like a shell or pager prompt: it ends with a prompt glyph (`$ # > ❯ ➜ » λ`,
// or a `%` that is not a percentage), a prompt is followed by a typed command (`host:~$ ls`), or
// it is a pager's `:` / `(END)`.
fn shell_prompt_row(row: &str) -> bool {
    let row = row.trim();
    // A prompt glyph right after a non-blank char (`~$`, `host%`); a `%` after a digit is a
    // percentage.
    let glyph_after = |i: usize, glyph: char| {
        row[..i]
            .chars()
            .next_back()
            .is_some_and(|c| !c.is_whitespace() && !(glyph == '%' && c.is_ascii_digit()))
    };
    let ends_with_prompt = match row.char_indices().next_back() {
        Some((_, '$' | '#' | '>' | '❯' | '➜' | '»' | 'λ')) => true,
        Some((i, '%')) => i == 0 || glyph_after(i, '%'),
        _ => false,
    };
    ends_with_prompt
        || row == ":"
        || row.ends_with("(END)")
        || ['$', '#', '%'].iter().any(|&g| {
            row.match_indices(&format!("{g} ") as &str)
                .any(|(i, _)| glyph_after(i, g))
        })
}

// How far a row's text is indented, after any left border glyph (`│   2. No` → 4).
fn indent(row: &str) -> usize {
    row.chars()
        .take_while(|&c| c.is_whitespace() || is_border(c))
        .count()
}

// The leftmost column a side-panel separator may sit at: a left frame (`│ …`) is not a panel.
const PANEL_MIN_COL: usize = 8;

// Each row with a side panel (or a right frame) cut off: a column at which a border glyph sits
// on at least a third of the non-blank rows (and at least 3) is a separator, and a row is cut at
// the first separator it reaches. A wide glyph left of the separator renders as one char for two
// cells, so the glyph is looked for a few chars left of the column too.
fn cut_side_panel<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    let mut at: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut nonblank = 0usize;
    for l in lines.iter().filter(|l| !l.trim().is_empty()) {
        nonblank += 1;
        for (i, c) in l.chars().enumerate() {
            if i >= PANEL_MIN_COL && is_border(c) {
                *at.entry(i).or_default() += 1;
            }
        }
    }
    let need = (nonblank / 3).max(3);
    let mut seps: Vec<usize> = at
        .into_iter()
        .filter(|&(_, n)| n >= need)
        .map(|(c, _)| c)
        .collect();
    if seps.is_empty() {
        return lines.to_vec();
    }
    seps.sort_unstable();
    lines
        .iter()
        .map(|l| {
            let chars: Vec<(usize, char)> = l.char_indices().collect();
            let cut = seps.iter().find_map(|&col| {
                (col.saturating_sub(4).max(PANEL_MIN_COL)..=col)
                    .rev()
                    .find(|&i| chars.get(i).is_some_and(|&(_, c)| is_border(c)))
                    .map(|i| chars[i].0)
            });
            cut.map_or(*l, |byte| &l[..byte])
        })
        .collect()
}

/// One render's verdict, cached under the content cursor `cursor` it was taken at, rendered at
/// `at` (epoch ms): whether the screen waits for input ([`detect_awaiting_input`]), and whether
/// it waits on a decision dialog ([`decision_dialog`]), of which kind and on which question.
pub fn verdict_for(screen_text: &str, cursor: u64, at: u64) -> ScreenVerdict {
    let dialog = decision_dialog(screen_text);
    ScreenVerdict {
        cursor,
        rendered_at: at,
        awaiting: detect_awaiting_input(screen_text),
        asking: dialog.is_some(),
        kind: dialog.as_ref().map(|d| d.kind).unwrap_or_default(),
        question: dialog.and_then(|d| d.question),
    }
}

// How many content rows above the bottom of the screen the selection-dialog scan looks at
// (blank rows, borders and horizontal rules are not counted).
const DIALOG_SCAN_ROWS: usize = 12;

// Vertical border glyphs a TUI frames its rows with (`│ … │`). They are stripped from both ends
// of a row before matching, so a framed dialog reads like an unframed one.
fn is_border(c: char) -> bool {
    matches!(c, '│' | '┃' | '║' | '╎' | '╏' | '┆' | '┇' | '┊' | '┋')
}

// Horizontal rule and corner glyphs. A row that starts AND ends with one is a rule (a box edge, a
// separator, or a statusline rule that carries a label in the middle: `──── name ─`).
fn is_rule_glyph(c: char) -> bool {
    matches!(
        c,
        '─' | '━'
            | '═'
            | '╌'
            | '╍'
            | '┄'
            | '┅'
            | '┈'
            | '┉'
            | '╭'
            | '╮'
            | '╰'
            | '╯'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '╔'
            | '╗'
            | '╚'
            | '╝'
    )
}

// A row with its frame removed: trimmed, border glyphs stripped from both ends, trimmed again.
fn strip_frame(row: &str) -> &str {
    row.trim().trim_matches(is_border).trim()
}

fn is_rule_row(row: &str) -> bool {
    let first = row.chars().next();
    let last = row.chars().next_back();
    matches!((first, last), (Some(a), Some(b)) if is_rule_glyph(a) && is_rule_glyph(b))
}

// `N. text` / `N) text`: a numbered option (no selection cursor).
fn numbered_option(row: &str) -> bool {
    let digits = row.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let mut rest = row[digits..].chars();
    matches!(rest.next(), Some('.') | Some(')'))
        && matches!(rest.next(), Some(c) if c.is_whitespace())
        && rest.as_str().trim_start().chars().next().is_some()
}

// `❯ N. text`: the cursored (selected) numbered option of a selection menu.
fn selected_option(row: &str) -> bool {
    row.strip_prefix('❯')
        .map(|rest| numbered_option(rest.trim_start()))
        .unwrap_or(false)
}

// A Claude-Code-style selection dialog anywhere in the bottom rows of the screen:
//
//   Do you want to proceed?
//   ❯ 1. Yes
//     2. No
//
//   Esc to cancel · Tab to amend
//   ──────────────────────────── status ─
//
// The last-line rules above miss it once anything is drawn below the dialog (a statusline rule,
// a frame). It needs a cursored numbered option, at least one other numbered option, and either a
// question above the cursor or a cancel/confirm footer below it, so a numbered list in prose never
// matches. A real dialog REPLACES the agent's input box (a `❯` row right under a rule): when that
// box is still visible below the menu, the menu is only quoted in the chat.
fn detect_selection_dialog(lines: &[&str]) -> bool {
    // Non-blank rows with their frame stripped, each flagged as a rule or not.
    let all: Vec<(&str, bool)> = lines
        .iter()
        .map(|l| strip_frame(l))
        .filter(|r| !r.is_empty())
        .map(|r| (r, is_rule_row(r)))
        .collect();
    let input_box = |i: usize| i > 0 && all[i - 1].1 && all[i].0.starts_with('❯');
    let content: Vec<usize> = (0..all.len()).filter(|&i| !all[i].1).collect();
    let window = &content[content.len().saturating_sub(DIALOG_SCAN_ROWS)..];
    let Some(w_sel) = window
        .iter()
        .rposition(|&i| selected_option(all[i].0) && !input_box(i))
    else {
        return false;
    };
    let sel = window[w_sel];
    if (sel + 1..all.len()).any(input_box) {
        return false;
    }
    let other_option = window
        .iter()
        .any(|&i| i != sel && (numbered_option(all[i].0) || selected_option(all[i].0)));
    if !other_option {
        return false;
    }
    let question = window[..w_sel].iter().any(|&i| is_question(all[i].0));
    let footer = window[w_sel + 1..].iter().any(|&i| {
        let lower = all[i].0.to_lowercase();
        bounded_contains(&lower, "esc to cancel")
            || bounded_contains(&lower, "enter to confirm")
            || bounded_contains(&lower, "esc to exit")
    });
    question || footer
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
        assert_eq!(
            next_poll_delay(Some(500), 1000, 0, 600, 30000),
            WAIT_POLL_MAX_MS
        );
        // Output just arrived (still ~590ms from quiet) → re-check at the band max.
        assert_eq!(
            next_poll_delay(Some(990), 1000, 0, 600, 30000),
            WAIT_POLL_MAX_MS
        );
        // Almost quiet (only ~10ms left in the settle window) → floor at the band min.
        assert_eq!(
            next_poll_delay(Some(410), 1000, 0, 600, 30000),
            WAIT_POLL_MIN_MS
        );
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
            SinceSlice {
                output: String::new(),
                cursor: 6,
                truncated: false
            }
        );
        assert_eq!(
            slice_since("abcdef", 6, 99),
            SinceSlice {
                output: String::new(),
                cursor: 6,
                truncated: false
            }
        );
    }

    #[test]
    fn returns_the_exact_tail_slice_for_a_cursor_inside_the_buffer() {
        assert_eq!(
            slice_since("abcdef", 6, 4),
            SinceSlice {
                output: "ef".to_string(),
                cursor: 6,
                truncated: false
            }
        );
        assert_eq!(
            slice_since("abcdef", 6, 0),
            SinceSlice {
                output: "abcdef".to_string(),
                cursor: 6,
                truncated: false
            }
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
            SinceSlice {
                output: "abcdef".to_string(),
                cursor: 6,
                truncated: false
            }
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
            SinceSlice {
                output: "cé".to_string(),
                cursor: 4,
                truncated: false
            }
        );
        // And the whole-buffer / nothing-new paths stay parity for non-ASCII too.
        assert_eq!(
            slice_since(replay, 4, 0),
            SinceSlice {
                output: "abcé".to_string(),
                cursor: 4,
                truncated: false
            }
        );
        assert_eq!(
            slice_since(replay, 4, 4),
            SinceSlice {
                output: String::new(),
                cursor: 4,
                truncated: false
            }
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
        assert!(detect_awaiting_input(
            "Do you trust the files in this folder?"
        ));
        assert!(detect_awaiting_input("Press enter to continue"));
        assert!(detect_awaiting_input("❯ 1. Yes, proceed"));
    }

    #[test]
    fn looks_only_at_the_last_non_empty_line() {
        assert!(detect_awaiting_input(
            "lots of output\nmore\n\nReady? (y/n)\n\n"
        ));
        // A question earlier in the scrollback, but the agent has moved on.
        assert!(!detect_awaiting_input(
            "Are you sure?\nOK, done.\nSpun you up a server."
        ));
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

    // A permission dialog as a framed Claude pane renders it: every row carries a right border,
    // and a labelled statusline rule sits BELOW the dialog, so the last non-empty line is the
    // rule, not the prompt. Content is synthetic.
    const PERMISSION_DIALOG: &str = concat!(
        "● Running the build script                                         │\n",
        "  ⎿  $ cd /work && ./build.sh 2>&1 | tail -40                       │\n",
        "                                                                   │\n",
        "───────────────────────────────────────────────────────────────────│\n",
        " Bash command                                                      │\n",
        " Run the build script                                              │\n",
        "╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌│\n",
        " │ cd /work && ./build.sh 2>&1 | tail -40                          │\n",
        "╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌│\n",
        "                                                                   │\n",
        " Do you want to proceed?                                           │\n",
        " ❯ 1. Yes                                                          │\n",
        "   2. No                                                           │\n",
        "                                                                   │\n",
        " Esc to cancel · Tab to amend                                      │\n",
        "──────────────────────────────────────────────────────── status ──│\n",
    );

    #[test]
    fn flags_a_permission_dialog_above_a_statusline_rule() {
        assert!(detect_awaiting_input(PERMISSION_DIALOG));
    }

    #[test]
    fn flags_a_boxed_permission_dialog_with_three_options() {
        let screen = concat!(
            "╭──────────────────────────────────────────────╮\n",
            "│ Edit file                                    │\n",
            "│ Do you want to make this edit to main.rs?    │\n",
            "│ ❯ 1. Yes                                     │\n",
            "│   2. Yes, allow all edits during this session │\n",
            "│   3. No, and tell Claude what to do           │\n",
            "│      differently (esc)                        │\n",
            "╰──────────────────────────────────────────────╯\n",
            "──────────────────────────────────── status ──\n",
        );
        assert!(detect_awaiting_input(screen));
    }

    #[test]
    fn flags_a_dialog_with_only_a_cancel_footer() {
        let screen = concat!(
            " Pick a plan\n",
            " ❯ 1. Keep planning\n",
            "   2. Start building\n",
            "\n",
            " Esc to cancel\n",
            "──────── status ─\n",
        );
        assert!(detect_awaiting_input(screen));
    }

    #[test]
    fn does_not_flag_a_numbered_list_in_prose() {
        let screen = concat!(
            "Here is the plan:\n",
            "1. Read the file\n",
            "2. Fix the bug\n",
            "Shall I go ahead?\n",
            "──────────────────\n",
            "❯ \n",
            "──────────────────\n",
            "  ? for shortcuts\n",
        );
        assert!(!detect_awaiting_input(screen));
    }

    // The agent's chat output merely QUOTES a menu (cursor, options, question and footer all
    // present) above its normal, empty input box: not blocked.
    #[test]
    fn does_not_flag_a_menu_quoted_in_chat_above_the_input_box() {
        let screen = concat!(
            "● The dialog offered these choices:\n",
            "  Do you want to proceed?\n",
            "  ❯ 1. Yes\n",
            "    2. No\n",
            "  Esc to cancel\n",
            "\n",
            "────────────────────────────────────────\n",
            "❯ \n",
            "────────────────────────────────────────\n",
            "  ? for shortcuts\n",
        );
        assert!(!detect_awaiting_input(screen));
        // The same quote with typed text in the box reads the same.
        assert!(!detect_awaiting_input(
            &screen.replace("❯ \n", "❯ 1. ship it\n")
        ));
    }

    #[test]
    fn does_not_flag_a_single_cursored_option_without_alternatives() {
        let screen = concat!(
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "──────── status ─\n",
        );
        assert!(!detect_awaiting_input(screen));
    }

    // ── blocks_typed_input: the guard every automated typing path checks ──

    #[test]
    fn typing_is_blocked_by_decision_prompts() {
        assert!(blocks_typed_input(PERMISSION_DIALOG));
        assert!(blocks_typed_input(TRUST_DIALOG));
        assert!(blocks_typed_input(MENU_TAIL));
        assert!(blocks_typed_input("Overwrite the file? (y/n)"));
        assert!(blocks_typed_input("Press enter to continue"));
        // Option forms moved from the app's goal delivery (trust prompt, boxed permission).
        assert!(blocks_typed_input(
            "Do you trust the files in this folder?\n\n ❯ 1. Yes, proceed\n   2. No, exit\n"
        ));
        assert!(blocks_typed_input(
            "│ Do you want to proceed?      │\n│ ❯ 1. Yes                     │\n│   2. No, and tell Claude why │\n"
        ));
    }

    #[test]
    fn typing_is_not_blocked_by_input_boxes_shells_or_chat() {
        assert!(!blocks_typed_input(IDLE_PROMPT));
        assert!(!blocks_typed_input(WORKING));
        assert!(!blocks_typed_input(
            "╭─────────────────╮\n│ > Try \"help\"    │\n╰─────────────────╯\n  ? for shortcuts\n"
        ));
        assert!(!blocks_typed_input(
            "Plan:\n 1. do a thing\n 2. do another\n"
        ));
        // A `❯` shell prompt (starship) at rest: awaiting input, but not a decision.
        assert!(!blocks_typed_input("❯ ls\nsrc  target\n❯ "));
        assert!(detect_awaiting_input("❯ ls\nsrc  target\n❯ "));
        // A question at the end of the chat, above the agent's input box.
        assert!(!blocks_typed_input(concat!(
            "Shall I go ahead?\n",
            "──────────────────\n",
            "❯ \n",
            "──────────────────\n",
        )));
        assert!(!blocks_typed_input(""));
    }

    // An idle screen must never hold a nudge: it never redraws, so the hold would be forever.
    #[test]
    fn typing_is_not_blocked_by_numbered_lists_on_idle_screens() {
        // An agent's reply with a numbered list, above its empty input box.
        assert!(!blocks_typed_input(concat!(
            "Done. Next steps:\n",
            " 1. run the tests\n",
            " 2. open the PR\n",
            "\n",
            "──────────────────\n",
            "❯ \n",
            "──────────────────\n",
            "  ? for shortcuts\n",
        )));
        // A past prompt in the chat history that was itself a numbered list.
        assert!(!blocks_typed_input(concat!(
            "❯ 1. fix the build\n",
            "  2. push it\n",
            "\n",
            "● On it.\n",
            "──────────────────\n",
            "❯ \n",
            "──────────────────\n",
        )));
        // A plain `❯` shell listing numbered files.
        assert!(!blocks_typed_input("❯ ls -1\n1.png\n2.png\n❯ "));
        // A cursor row and numbered rows that are not one block.
        assert!(!blocks_typed_input(concat!(
            "❯ 1. only option\n",
            "\n",
            "Notes:\n",
            "\n",
            " 2. unrelated\n",
            " 3. unrelated\n",
        )));
    }

    // ── decision_dialog: what reads ASK and raises the "needs input" alert ──

    fn dialog(screen: &str) -> Option<(AskKind, Option<String>)> {
        decision_dialog(screen).map(|d| (d.kind, d.question))
    }

    #[test]
    fn decision_dialog_finds_the_kind_and_the_question() {
        assert_eq!(
            dialog(PERMISSION_DIALOG),
            Some((AskKind::Permission, Some("Do you want to proceed?".into())))
        );
        assert_eq!(
            dialog(TRUST_DIALOG),
            Some((
                AskKind::Trust,
                Some("Do you trust the files in this folder?".into())
            ))
        );
        let picker = concat!(
            " Pick a plan\n",
            " ❯ 1. Keep planning\n",
            "   2. Start building\n",
            "\n",
            " Esc to cancel\n",
            "──────── status ─\n",
        );
        assert_eq!(dialog(picker), Some((AskKind::Choice, None)));
        // A question in the chat above the dialog's top edge is not the dialog's.
        let below_chat = format!("● Should I proceed with the plan?\n────────────\n{picker}");
        assert_eq!(dialog(&below_chat), Some((AskKind::Choice, None)));
        let boxed = concat!(
            "╭──────────────────────────────────────────────╮\n",
            "│ Edit file                                    │\n",
            "│ Do you want to make this edit to main.rs?    │\n",
            "│ ❯ 1. Yes                                     │\n",
            "│   2. Yes, allow all edits during this session │\n",
            "│   3. No, and tell Claude what to do           │\n",
            "│      differently (esc)                        │\n",
            "╰──────────────────────────────────────────────╯\n",
        );
        assert_eq!(dialog(boxed).map(|d| d.0), Some(AskKind::Permission));
        let long = format!("{}?\n❯ 1. Yes\n  2. No\n", "x".repeat(400));
        let q = dialog(&long).and_then(|d| d.1).unwrap();
        assert_eq!(q.chars().count(), QUESTION_MAX_CHARS);
        assert!(q.ends_with('…'));
    }

    // Chat rows with a side panel drawn beside them, the way a Claude pane with a panel mod
    // renders: every row is the chat column padded to the separator, `│`, then the panel's row.
    fn beside(chat: &[&str], panel: &[&str]) -> String {
        (0..chat.len().max(panel.len()))
            .map(|i| {
                let c = chat.get(i).copied().unwrap_or("");
                let p = panel.get(i).copied().unwrap_or("");
                format!("{c:<72}│ {p}\n")
            })
            .collect()
    }

    const PANEL: &[&str] = &[
        "Waiting (2)",
        "",
        "■ Review the parser",
        "   change",
        "■ Update the docs",
        "",
        "Asked of others (1)",
        "",
        "■ A pane sat at a push guard",
        "   'Do you want to proceed?'",
        "",
        "Done today (2)",
        "✓ Pin the toolchain",
        "✓ Rotate the key",
    ];

    // A PreToolUse hook that answers "ask" (a push guard) makes Claude show its permission
    // dialog. Such a dialog once sat for hours on two panes that read busy and never needs-input:
    // it must read ASK, with or without a side panel, and with a statusline under it.
    #[test]
    fn a_hook_confirm_dialog_reads_ask() {
        let dialog_rows = [
            "● Bash(git push origin fix/parser)",
            "  ⎿  Running PreToolUse hook…",
            "",
            "──────────────────────────────────────────────────────────────────",
            " Bash command",
            "",
            "   git push origin fix/parser",
            "   Push the branch",
            "",
            " Hook PreToolUse:Bash requires confirmation for this command:",
            " push guard: check the remote first",
            "",
            " Do you want to proceed?",
            " ❯ 1. Yes",
            "   2. No",
            "",
            " Esc to cancel · Tab to amend",
        ];
        let want = Some((AskKind::Permission, Some("Do you want to proceed?".into())));
        // Plain.
        assert_eq!(dialog(&(dialog_rows.join("\n") + "\n")), want);
        // The panel runs beside the dialog and on below it.
        let mut panel = PANEL.to_vec();
        panel.extend(["", "✓ Audit the hooks", "✓ Fix the guard", ""]);
        let screen = beside(&dialog_rows, &panel);
        assert_eq!(dialog(&screen), want, "{screen}");
        // The panel beside the history only; the dialog and a statusline full width below it.
        let mut screen = beside(
            &[
                "● Pushing the branch.",
                "",
                "● Bash(git push origin fix/parser)",
                "  ⎿  Running PreToolUse hook…",
            ],
            PANEL,
        );
        screen.push_str(&dialog_rows[3..].join("\n"));
        screen.push_str("\n  ◆ Opus 5.5 │ ███░░ 15% │ +27/-0 │ my-project\n");
        screen.push_str("  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← 1 agent\n");
        assert_eq!(dialog(&screen), want, "{screen}");
        assert!(blocks_typed_input(&screen));
    }

    #[test]
    fn panel_text_alone_never_reads_ask() {
        // The panel quotes a prompt; the chat column is an idle agent with its input box.
        let screen = beside(
            &[
                "● Done.",
                "",
                "────────────────────────────────────────",
                "❯ ",
                "────────────────────────────────────────",
                "  ? for shortcuts",
            ],
            &[
                "Do you want to proceed?",
                "❯ 1. Yes",
                "  2. No",
                "Esc to cancel",
            ],
        );
        assert_eq!(dialog(&screen), None);
    }

    // Screens that never change once they are up: a false ASK there would alert forever.
    #[test]
    fn a_dialog_printed_in_a_shell_never_reads_ask() {
        let printed = [
            // `cat` of a saved dialog, then the shell prompt right under it.
            concat!(
                "user@host:~/notes$ cat dialog.txt\n",
                "Do you want to proceed?\n",
                "❯ 1. Yes\n",
                "  2. No\n",
                "user@host:~/notes$ \n",
            ),
            // A blank row, a footer and a rule in the file, then the prompt.
            concat!(
                "$ cat screen.txt\n",
                " Do you want to proceed?\n",
                " ❯ 1. Yes\n",
                "   2. No\n",
                "\n",
                " Esc to cancel · Tab to amend\n",
                "──────── status ─\n",
                "user@host:~/notes$ \n",
            ),
            // A starship prompt.
            "❯ cat dialog.txt\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n❯ \n",
            // A prompt with the next command already typed.
            "Do you want to proceed?\n❯ 1. Yes\n  2. No\n\nhost:~/notes% ls\n",
            // `grep` prefixes every row.
            "dialog.txt:1:Do you want to proceed?\ndialog.txt:2:❯ 1. Yes\ndialog.txt:3:  2. No\n$ ",
            // A pager at the end of the file.
            "Do you want to proceed?\n❯ 1. Yes\n  2. No\n\nEsc to cancel\n(END)\n",
        ];
        for screen in printed {
            assert_eq!(dialog(screen), None, "{screen}");
        }
        // A dialog with more output under it than a statusline.
        let mut screen = String::from("Do you want to proceed?\n❯ 1. Yes\n  2. No\n");
        for i in 0..=STATUS_ROWS_MAX {
            screen.push_str(&format!("log line {i}\n"));
        }
        assert_eq!(dialog(&screen), None);
    }

    #[test]
    fn a_prompt_line_without_a_dialog_never_reads_ask() {
        for last in [
            "2026-11-13 08:00:01 INFO user said: do you want to retry",
            "2026-11-13 08:00:02 WARN press enter to continue was printed",
            "Overwrite the file? (y/n)",
            "Continue [Y/n]",
            "Press enter to continue",
        ] {
            let screen = format!("2026-11-13 08:00:00 INFO starting\n{last}\n");
            assert_eq!(dialog(&screen), None, "{last}");
            // Still waiting for input, and still never typed into.
            assert!(detect_awaiting_input(&screen) && blocks_typed_input(&screen));
        }
        // A menu with neither a question nor a footer, a single option, a quoted menu.
        assert_eq!(dialog(MENU_TAIL), None);
        assert_eq!(
            dialog("Do you want to proceed?\n❯ 1. Yes\n──────── status ─\n"),
            None
        );
        assert_eq!(
            dialog(concat!(
                "● The dialog offered these choices:\n",
                "  Do you want to proceed?\n",
                "  ❯ 1. Yes\n",
                "    2. No\n",
                "  Esc to cancel\n",
                "\n",
                "────────────────────────────────────────\n",
                "❯ \n",
                "────────────────────────────────────────\n",
            )),
            None
        );
    }

    #[test]
    fn verdict_separates_waiting_from_deciding() {
        let v = verdict_for(PERMISSION_DIALOG, 42, 9_000);
        assert_eq!((v.cursor, v.rendered_at), (42, 9_000));
        assert!(v.awaiting && v.asking);
        assert_eq!(v.kind, AskKind::Permission);
        assert_eq!(v.question.as_deref(), Some("Do you want to proceed?"));
        // A starship prompt waits for input but asks nothing.
        let v = verdict_for("❯ ls\nsrc  target\n❯ ", 7, 0);
        assert!(v.awaiting && !v.asking);
        assert_eq!(v.question, None);
        // So does a y/n line: it holds typed input, but only a dialog reads ASK.
        let v = verdict_for("Applied 3 edits.\nRun the tests now? (y/n)", 8, 0);
        assert!(v.awaiting && !v.asking);
        assert_eq!(v.question, None);
        let v = verdict_for(WORKING, 1, 0);
        assert!(!v.awaiting && !v.asking);
    }

    #[test]
    fn does_not_flag_a_dialog_scrolled_far_above_the_bottom() {
        let mut screen = String::from(concat!(
            "Do you want to proceed?\n",
            "❯ 1. Yes\n",
            "  2. No\n",
            "Esc to cancel\n",
        ));
        for i in 0..DIALOG_SCAN_ROWS {
            screen.push_str(&format!("output line {i}\n"));
        }
        assert!(!detect_awaiting_input(&screen));
    }
}
