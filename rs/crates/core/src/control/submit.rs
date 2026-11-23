//! Verified submit: type text, send the CR, then CHECK it landed and send one insurance CR only
//! when it provably did not.
//!
//! Why: Claude Code keeps a submitted prompt visible in its input box while its
//! `UserPromptSubmit` hooks run (a couple of seconds under host load), so a screen read right
//! after the CR can show the text still after the `❯` even though the prompt was accepted. A
//! blind second CR then would double-submit; a missing CR would lose the prompt. The decision
//! is [`decide`] (pure, unit-tested); [`verified_submit`] drives the pty and the signals.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::session::screen::{InputLine, InputLineKind};
use crate::session_manager::SessionManager;

/// How long after the CR we keep watching before declaring the CR dropped.
pub const VERIFY_WINDOW_MS: u64 = 5_000;
/// Poll cadence inside the window.
pub const VERIFY_POLL_MS: u64 = 250;
/// How many non-space characters of the typed text identify "our text is still in the box".
const PREFIX_CHARS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The prompt was accepted (or the box no longer holds our text): nothing more to send.
    Submitted,
    /// Keep polling.
    Wait,
    /// The window elapsed with our text still in the box and no transcript record: send one CR.
    Retry,
    /// The screen has no prompt box to judge by (not a Claude-style pane): never retry blind.
    NoRetry,
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Does the prompt box still hold (the start of) `typed`?
fn holds_typed(typed: &str, line: &InputLine) -> bool {
    if line.kind != InputLineKind::Typed {
        return false;
    }
    let want: String = squash(typed).chars().take(PREFIX_CHARS).collect();
    if want.is_empty() {
        return false;
    }
    squash(&line.text).starts_with(&want)
}

/// Decide the next step after the submit CR.
///
/// * a transcript user record newer than the send → `Submitted`, even if the box still shows
///   the text (the hook window), so the insurance CR is never sent then;
/// * no input line at all → `NoRetry`;
/// * box empty / holds different text (the owner typing, a suggestion) → `Submitted`;
/// * box still holds our text → `Wait` until `elapsed_ms` reaches the window, then `Retry`.
pub fn decide(
    typed: &str,
    input: Option<&InputLine>,
    transcript_advanced: bool,
    elapsed_ms: u64,
) -> Verdict {
    if transcript_advanced {
        return Verdict::Submitted;
    }
    let Some(line) = input else {
        return Verdict::NoRetry;
    };
    if !holds_typed(typed, line) {
        return Verdict::Submitted;
    }
    if elapsed_ms >= VERIFY_WINDOW_MS {
        Verdict::Retry
    } else {
        Verdict::Wait
    }
}

/// Count the user-authored records (not tool results) in a Claude transcript file.
pub fn count_user_records(jsonl: &str) -> usize {
    jsonl
        .lines()
        .filter(|l| l.contains("\"user\""))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some("user"))
        .filter(|v| {
            // A tool_result is also a `user` record; only count real prompts.
            !v.pointer("/message/content")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                })
                .unwrap_or(false)
        })
        .count()
}

/// The pane's Claude transcript path, from its session marker (None: not a Claude pane).
fn transcript_path(pane_id: &str) -> Option<PathBuf> {
    let marker = crate::claude_panes::read_pane_session(pane_id)?;
    let root = if marker.config_dir.is_empty() {
        crate::claude_history::claude_projects_root()?
    } else {
        PathBuf::from(&marker.config_dir).join("projects")
    };
    Some(
        root.join(crate::claude_history::encode_project_dir(
            std::path::Path::new(&marker.cwd),
        ))
        .join(format!("{}.jsonl", marker.session_id)),
    )
}

/// The transcript's length in bytes now: the baseline a later [`advanced_since`] reads from.
fn transcript_len(path: &Option<PathBuf>) -> Option<u64> {
    std::fs::metadata(path.as_ref()?).ok().map(|m| m.len())
}

/// Did a real user prompt land in the transcript after byte offset `before`? Reads only the
/// appended tail, never the whole (often multi-MB) file.
fn advanced_since(path: &Option<PathBuf>, before: Option<u64>) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let (Some(p), Some(off)) = (path.as_ref(), before) else {
        return false;
    };
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    if f.seek(SeekFrom::Start(off)).is_err() {
        return false;
    }
    let mut tail = String::new();
    if f.read_to_string(&mut tail).is_err() {
        return false;
    }
    count_user_records(&tail) > 0
}

/// Write `text`, wait `gap_ms`, write the CR, then verify in this task (callers spawn it) and
/// send one insurance CR only when [`decide`] says `Retry`.
pub async fn verified_submit(
    sessions: Arc<SessionManager>,
    uid: String,
    pane_id: String,
    text: String,
    gap_ms: u64,
) {
    let (path, before) = snapshot(&pane_id);
    sessions.write(&uid, &text);
    tokio::time::sleep(Duration::from_millis(gap_ms)).await;
    sessions.write(&uid, "\r");
    verify_after_cr(&sessions, &uid, &path, before, &text).await;
}

/// Poll after the CR; split from [`verified_submit`] so the CR can be written by the caller.
pub async fn verify_after_cr(
    sessions: &Arc<SessionManager>,
    uid: &str,
    path: &Option<PathBuf>,
    before: Option<u64>,
    text: &str,
) {
    let mut elapsed = 0u64;
    loop {
        tokio::time::sleep(Duration::from_millis(VERIFY_POLL_MS)).await;
        elapsed += VERIFY_POLL_MS;
        let (s, u, p) = (Arc::clone(sessions), uid.to_string(), path.clone());
        let probe = tokio::task::spawn_blocking(move || {
            let line = s.render_screen_read(&u).and_then(|r| r.input_line);
            (line, advanced_since(&p, before))
        })
        .await;
        let Ok((line, advanced)) = probe else { return };
        match decide(text, line.as_ref(), advanced, elapsed) {
            Verdict::Wait => {}
            Verdict::Retry => {
                sessions.write(uid, "\r");
                return;
            }
            Verdict::Submitted | Verdict::NoRetry => return,
        }
    }
}

/// Entry for the `/input` route: the text is written by the caller; the CR follows after
/// `gap_ms`, then the same verification.
pub async fn submit_after_text(
    sessions: Arc<SessionManager>,
    uid: String,
    text: String,
    gap_ms: u64,
    before: Option<u64>,
    path: Option<PathBuf>,
) {
    tokio::time::sleep(Duration::from_millis(gap_ms)).await;
    sessions.write(&uid, "\r");
    verify_after_cr(&sessions, &uid, &path, before, &text).await;
}

/// Snapshot taken BEFORE the text is typed: (transcript path, its byte length). Cheap: one
/// marker read and one stat, so it is fine on the request path.
pub fn snapshot(pane_id: &str) -> (Option<PathBuf>, Option<u64>) {
    let path = transcript_path(pane_id);
    let len = transcript_len(&path);
    (path, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: InputLineKind, text: &str) -> InputLine {
        InputLine {
            text: text.into(),
            kind,
        }
    }
    const T: &str = "please run the full test suite now";

    #[test]
    fn box_emptied_is_submitted() {
        let l = line(InputLineKind::Empty, "");
        assert_eq!(decide(T, Some(&l), false, 250), Verdict::Submitted);
    }

    #[test]
    fn transcript_advanced_while_box_still_shows_text_does_not_retry() {
        let l = line(InputLineKind::Typed, T);
        assert_eq!(decide(T, Some(&l), true, 250), Verdict::Submitted);
        // even after the whole window: the hook window must never get an insurance CR
        assert_eq!(decide(T, Some(&l), true, 9_000), Verdict::Submitted);
    }

    #[test]
    fn text_still_there_waits_then_retries_once_window_elapsed() {
        let l = line(InputLineKind::Typed, T);
        assert_eq!(decide(T, Some(&l), false, 250), Verdict::Wait);
        assert_eq!(
            decide(T, Some(&l), false, VERIFY_WINDOW_MS - 1),
            Verdict::Wait
        );
        assert_eq!(decide(T, Some(&l), false, VERIFY_WINDOW_MS), Verdict::Retry);
    }

    #[test]
    fn wrapped_box_text_still_matches() {
        let l = line(InputLineKind::Typed, "please run the full\ntest suite now");
        assert_eq!(decide(T, Some(&l), false, 0), Verdict::Wait);
    }

    #[test]
    fn different_text_in_box_never_retries() {
        let l = line(InputLineKind::Typed, "something the owner is typing");
        assert_eq!(decide(T, Some(&l), false, 9_000), Verdict::Submitted);
        let s = line(InputLineKind::Suggestion, T);
        assert_eq!(decide(T, Some(&s), false, 9_000), Verdict::Submitted);
    }

    #[test]
    fn no_input_line_never_retries() {
        assert_eq!(decide(T, None, false, 9_000), Verdict::NoRetry);
    }

    #[test]
    fn counts_only_real_user_prompts() {
        let j = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"x\"}]}}\n",
            "not json \"user\"\n",
        );
        assert_eq!(count_user_records(j), 2);
    }

    #[test]
    fn advanced_since_reads_only_the_appended_tail() {
        let dir = std::env::temp_dir().join(format!("hp-submit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.jsonl");
        std::fs::write(
            &f,
            "{\"type\":\"user\",\"message\":{\"content\":\"old\"}}\n",
        )
        .unwrap();
        let path = Some(f.clone());
        let before = transcript_len(&path);
        assert!(!advanced_since(&path, before), "an old prompt is not new");
        let mut s = std::fs::read_to_string(&f).unwrap();
        s.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\"}]}}\n");
        std::fs::write(&f, &s).unwrap();
        assert!(
            !advanced_since(&path, before),
            "a tool result is not a prompt"
        );
        s.push_str("{\"type\":\"user\",\"message\":{\"content\":\"new\"}}\n");
        std::fs::write(&f, &s).unwrap();
        assert!(advanced_since(&path, before));
        assert!(!advanced_since(&None, before));
        assert!(!advanced_since(&path, None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
