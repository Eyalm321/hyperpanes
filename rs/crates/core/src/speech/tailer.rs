//! Incremental tailer over a pane's Claude Code session transcript — yields the text of
//! NEW assistant replies only, never history and never terminal output.
//!
//! A pane's live transcript is `<config_dir, or ~/.claude>/projects/<encoded-cwd>/<session
//! id>.jsonl`, the same per-project JSONL store [`crate::claude_history`] reads for the
//! resume/history browser (see [`transcript_path`], which resolves it from the pane's live
//! session marker). Unlike that reader, which parses a bounded prefix once, [`TranscriptTail`]
//! is built to be polled repeatedly (once per pane, on a timer) and only ever look at bytes
//! appended since the last poll — a "talk" pane must never re-speak old history, so the
//! cursor starts at end-of-file, not at the top of the transcript.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::claude_history::encode_path_str;
use crate::claude_panes::PaneClaudeSession;

/// A single JSONL line longer than this is skipped rather than buffered in full — bounds
/// memory against a pathological or corrupt transcript line instead of growing the tail
/// buffer without limit while waiting for its newline.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Resolve `marker`'s live session to its transcript path: `<config_dir, or ~/.claude>/
/// projects/<encoded cwd>/<session_id>.jsonl`. `None` when the marker has no cwd (nothing
/// to encode) or no home directory is known for the default-account fallback.
pub fn transcript_path(marker: &PaneClaudeSession) -> Option<PathBuf> {
    if marker.cwd.is_empty() {
        return None;
    }
    let projects_root = if marker.config_dir.is_empty() {
        crate::claude_history::claude_projects_root()?
    } else {
        PathBuf::from(&marker.config_dir).join("projects")
    };
    let encoded = encode_path_str(&marker.cwd);
    Some(
        projects_root
            .join(encoded)
            .join(format!("{}.jsonl", marker.session_id)),
    )
}

/// An incremental cursor over one transcript file. Starts at end-of-file (see
/// [`TranscriptTail::start_at_end`]) so [`poll`](TranscriptTail::poll) only ever surfaces
/// assistant replies written after the tail was created.
pub struct TranscriptTail {
    path: PathBuf,
    /// Byte offset in the file already read (both emitted complete lines and any bytes
    /// folded into `partial`). The next [`poll`](Self::poll) reads from here.
    cursor: u64,
    /// Bytes of the trailing, not-yet-newline-terminated line from the last poll.
    partial: Vec<u8>,
    /// Set when `partial` was dropped for exceeding [`MAX_LINE_BYTES`] before its newline
    /// arrived — the next newline seen ends that oversized line and is discarded, rather
    /// than being (wrongly) treated as the start of a fresh, already-complete line.
    skipping: bool,
}

impl TranscriptTail {
    /// Start tailing `path` from its current end — pre-existing content (all prior history)
    /// is never returned by [`poll`](Self::poll). A missing file starts at offset 0, so it
    /// picks up everything once the file (and any assistant replies) appear.
    pub fn start_at_end(path: PathBuf) -> TranscriptTail {
        let cursor = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        TranscriptTail {
            path,
            cursor,
            partial: Vec::new(),
            skipping: false,
        }
    }

    /// The transcript path this tail is following.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read whatever was appended to the transcript since the last poll and return the
    /// speakable text of each complete NEW assistant record, in order. A trailing partial
    /// line (append still in flight) is buffered and completed on a later poll. A missing
    /// or unreadable file yields an empty vec and leaves the cursor untouched — the file may
    /// simply not exist yet. If the file shrank (rotated/truncated) since the last poll, the
    /// cursor resets to the new length and the buffered partial is dropped, since whatever
    /// was appended after our old cursor is no longer there.
    pub fn poll(&mut self) -> Vec<String> {
        let mut texts = Vec::new();
        let Ok(mut file) = fs::File::open(&self.path) else {
            return texts;
        };
        let Ok(meta) = file.metadata() else {
            return texts;
        };
        let len = meta.len();
        if len < self.cursor {
            self.cursor = len;
            self.partial.clear();
            self.skipping = false;
            return texts;
        }
        if len == self.cursor {
            return texts;
        }
        if file.seek(SeekFrom::Start(self.cursor)).is_err() {
            return texts;
        }
        let mut new_bytes = Vec::new();
        if file.read_to_end(&mut new_bytes).is_err() {
            return texts;
        }
        self.cursor += new_bytes.len() as u64;

        let mut data = std::mem::take(&mut self.partial);
        data.extend_from_slice(&new_bytes);

        let mut start = 0usize;
        while let Some(rel_nl) = data[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel_nl;
            if self.skipping {
                self.skipping = false;
            } else {
                let line_bytes = &data[start..end];
                if line_bytes.len() <= MAX_LINE_BYTES {
                    if let Ok(line) = std::str::from_utf8(line_bytes) {
                        if let Some(text) = extract_assistant_text(line) {
                            texts.push(text);
                        }
                    }
                }
            }
            start = end + 1;
        }

        let tail = &data[start..];
        if tail.len() > MAX_LINE_BYTES {
            // Drop the oversized in-flight line rather than keep buffering it; `skipping`
            // remembers to discard the rest of it once its newline finally arrives.
            self.partial = Vec::new();
            self.skipping = true;
        } else {
            self.partial = tail.to_vec();
        }

        texts
    }
}

/// Pull the speakable text out of one transcript JSONL line, or `None` if it isn't a
/// complete `"type":"assistant"` record with text content. `message.content` is an array
/// of blocks; only `{"type":"text"}` blocks are kept (space-joined) — `tool_use` /
/// `tool_result` blocks are silently skipped. Malformed JSON, non-assistant records, and
/// assistant records with no text blocks (tool-only turns) all yield `None`.
pub fn extract_assistant_text(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let blocks = v.get("message")?.get("content")?.as_array()?;
    let mut out = String::new();
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(t);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-speech-tailer-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn assistant_line(text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n",
            serde_json::to_string(text).unwrap()
        )
    }

    // ---- transcript_path ----

    #[test]
    fn transcript_path_honors_config_dir() {
        let marker = PaneClaudeSession {
            session_id: "sess-1".into(),
            cwd: "/home/me/dev/x".into(),
            config_dir: "/home/me/.claude-alt".into(),
        };
        let path = transcript_path(&marker).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/me/.claude-alt/projects/-home-me-dev-x/sess-1.jsonl")
        );
    }

    #[test]
    fn transcript_path_falls_back_to_default_account_when_config_dir_empty() {
        let marker = PaneClaudeSession {
            session_id: "sess-1".into(),
            cwd: "/home/me/dev/x".into(),
            config_dir: String::new(),
        };
        let path = transcript_path(&marker).unwrap();
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .unwrap();
        let expected = PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join("-home-me-dev-x")
            .join("sess-1.jsonl");
        assert_eq!(path, expected);
    }

    #[test]
    fn transcript_path_none_without_cwd() {
        let marker = PaneClaudeSession {
            session_id: "sess-1".into(),
            cwd: String::new(),
            config_dir: String::new(),
        };
        assert!(transcript_path(&marker).is_none());
    }

    // ---- extract_assistant_text ----

    #[test]
    fn extracts_text_blocks_and_ignores_tool_blocks() {
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[\
            {\"type\":\"text\",\"text\":\"hello\"},\
            {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\",\"input\":{}},\
            {\"type\":\"text\",\"text\":\"world\"}]}}";
        assert_eq!(extract_assistant_text(line).as_deref(), Some("hello world"));
    }

    #[test]
    fn ignores_non_assistant_records() {
        let user = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}";
        assert!(extract_assistant_text(user).is_none());
        let tool_result = "{\"type\":\"tool_result\",\"content\":\"x\"}";
        assert!(extract_assistant_text(tool_result).is_none());
    }

    #[test]
    fn tool_only_assistant_record_has_no_text() {
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[\
            {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\",\"input\":{}}]}}";
        assert!(extract_assistant_text(line).is_none());
    }

    #[test]
    fn malformed_json_is_none() {
        assert!(extract_assistant_text("not json").is_none());
    }

    // ---- TranscriptTail ----

    #[test]
    fn start_at_end_skips_preexisting_content() {
        let dir = temp_dir("preexisting");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, assistant_line("old reply")).unwrap();

        let mut tail = TranscriptTail::start_at_end(path.clone());
        assert!(
            tail.poll().is_empty(),
            "pre-existing content never returned"
        );

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("new reply").as_bytes()).unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["new reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_at_end_missing_file_starts_at_zero() {
        let dir = temp_dir("missing");
        let path = dir.join("missing.jsonl");
        let mut tail = TranscriptTail::start_at_end(path.clone());
        assert!(tail.poll().is_empty(), "still missing, nothing to read");

        std::fs::write(&path, assistant_line("first reply")).unwrap();
        assert_eq!(tail.poll(), vec!["first reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appended_record_returned_exactly_once() {
        let dir = temp_dir("once");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone());

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("only once").as_bytes()).unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["only once".to_string()]);
        assert!(tail.poll().is_empty(), "not returned again on next poll");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_line_then_completion() {
        let dir = temp_dir("partial");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone());

        let full = assistant_line("split reply");
        let (first_half, second_half) = full.split_at(full.len() / 2);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(first_half.as_bytes()).unwrap();
        f.flush().unwrap();
        assert!(
            tail.poll().is_empty(),
            "partial line withheld until newline"
        );

        f.write_all(second_half.as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["split reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_and_tool_result_records_ignored() {
        let dir = temp_dir("ignored");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone());

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n")
            .unwrap();
        f.write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"x\"}]}}\n")
            .unwrap();
        f.write_all(assistant_line("real reply").as_bytes())
            .unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["real reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_use_blocks_ignored_text_blocks_kept() {
        let dir = temp_dir("mixed");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone());

        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[\
            {\"type\":\"text\",\"text\":\"before\"},\
            {\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\",\"input\":{}},\
            {\"type\":\"text\",\"text\":\"after\"}]}}\n";
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(line.as_bytes()).unwrap();
        drop(f);

        assert_eq!(tail.poll(), vec!["before after".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shrink_resets_cursor() {
        let dir = temp_dir("shrink");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, assistant_line("first")).unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone());

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("second").as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["second".to_string()]);

        // Truncate the file back down below the tail's cursor (rotation/reset).
        std::fs::write(&path, assistant_line("after shrink")).unwrap();
        assert!(
            tail.poll().is_empty(),
            "shrink resets cursor to new EOF, doesn't re-surface the new content as if old"
        );

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(assistant_line("post-shrink reply").as_bytes())
            .unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["post-shrink reply".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_appends_preserve_order() {
        let dir = temp_dir("order");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone());

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(assistant_line("one").as_bytes()).unwrap();
        f.write_all(assistant_line("two").as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["one".to_string(), "two".to_string()]);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(assistant_line("three").as_bytes()).unwrap();
        drop(f);
        assert_eq!(tail.poll(), vec!["three".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_accessor_returns_the_tailed_path() {
        let dir = temp_dir("path-acc");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let tail = TranscriptTail::start_at_end(path.clone());
        assert_eq!(tail.path(), path.as_path());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_single_line_is_skipped() {
        let dir = temp_dir("oversize");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut tail = TranscriptTail::start_at_end(path.clone());

        // A single line far past MAX_LINE_BYTES, followed by a normal record.
        let huge = assistant_line(&"x".repeat(MAX_LINE_BYTES + 1024));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        f.write_all(huge.as_bytes()).unwrap();
        f.write_all(assistant_line("normal reply").as_bytes())
            .unwrap();
        drop(f);

        assert_eq!(
            tail.poll(),
            vec!["normal reply".to_string()],
            "oversized line skipped, normal line after it still surfaces"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
