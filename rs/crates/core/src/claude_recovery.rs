//! Recovery from Claude Code CLI `API Error` failures surfaced in an agent pane's
//! terminal output.
//!
//! Claude Code sometimes dies (or auto-retries) mid-session with a line like
//! `API Error: <code> <detail>` printed to the pane. This module is the *pure*
//! decision logic for reacting to that: detect the sighting from raw pane-tail
//! text, classify what kind of failure it is, and — for the one class that's
//! mechanically fixable — surgically repair the on-disk transcript so a
//! `claude --resume` can proceed.
//!
//! **Detection is pure over pane-tail text.** It does not know whether the pane
//! is currently busy. The caller MUST gate on pane activity (e.g. only inspect a
//! pane once it's gone idle) — an `API Error` substring appearing mid-scrollback
//! of a *busy* pane (still streaming, mid auto-retry) must never be treated as a
//! dead pane. [`ApiErrorSighting::retrying`] helps with the common case (Claude's
//! own transient auto-retry banner) but is not a substitute for an activity gate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

// ===================== 1) Detection =====================

/// One `API Error` line found in a pane's (already ANSI-stripped) tail text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiErrorSighting {
    /// HTTP-ish status code, when Claude Code printed one (`API Error: 400 …`).
    /// `None` for codeless variants like `API Error (Request timed out)`.
    pub code: Option<u16>,
    /// Everything after the code (or after `API Error`/`API Error:` when there's
    /// no code), trimmed. Still contains the surrounding parens for the codeless
    /// style, since that's the only content there is.
    pub detail: String,
    /// `true` when this same line also says "Retrying" — Claude Code's own
    /// transient auto-retry banner (e.g. `API Error: 529 overloaded · Retrying in
    /// 8 seconds...`). The pane is still alive and working, not dead.
    pub retrying: bool,
}

/// Scan `tail` for the LAST line containing `"API Error"` and parse it into an
/// [`ApiErrorSighting`]. Returns `None` when no such line exists (including the
/// common healthy-but-quiet case: normal Claude/compile output with no error).
pub fn detect_api_error(tail: &str) -> Option<ApiErrorSighting> {
    let line = tail.lines().rev().find(|l| l.contains("API Error"))?;

    let after = line.split_once("API Error").map(|(_, rest)| rest).unwrap_or("");
    let after = after.strip_prefix(':').unwrap_or(after).trim_start();

    let digit_len = after.chars().take_while(|c| c.is_ascii_digit()).count();
    let next_is_boundary = after[digit_len..].chars().next().is_none_or(char::is_whitespace);
    let (code, detail) = if digit_len > 0 && next_is_boundary {
        let code = after[..digit_len].parse::<u16>().ok();
        (code, after[digit_len..].trim_start().to_string())
    } else {
        (None, after.trim().to_string())
    };

    Some(ApiErrorSighting {
        code,
        detail,
        retrying: line.contains("Retrying"),
    })
}

// ===================== 2) Classification =====================

/// What kind of `API Error` this is, and — implicitly — what recovery makes sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transport/server hiccup (rate limit, overload, timeout, gateway error).
    /// Same account, same transcript — a plain retry is expected to work.
    Transient,
    /// The account itself is out of runway (auth, credit balance, usage limit,
    /// quota, billing). Retrying the same account is futile; recovery should
    /// hand off to account rotation instead.
    AccountLimit,
    /// The on-disk transcript has a structurally invalid content-block
    /// arrangement (Anthropic's own words: an orphaned `tool_result`/
    /// `tool_search_tool_result` with no preceding `tool_use`/`server_tool_use`,
    /// or similar). Mechanically repairable — see [`repair_poisoned_transcript`].
    Poisoned,
    /// Recognized as an `API Error` but not confidently bucketed into any of the
    /// above. Deliberately NOT auto-retried and NOT auto-repaired: guessing at an
    /// unrecognized error class risks masking a real, unrelated problem. Surface
    /// it to a human instead.
    Unknown,
}

const ACCOUNT_LIMIT_WORDS: [&str; 4] = ["credit balance", "usage limit", "quota", "billing"];
const TRANSIENT_CODES: [u16; 7] = [408, 429, 500, 502, 503, 504, 529];
const TRANSIENT_WORDS: [&str; 8] = [
    "overloaded",
    "rate limit",
    "timed out",
    "timeout",
    "etimedout",
    "econnreset",
    "econnrefused",
    "fetch failed",
];

/// Classify an [`ApiErrorSighting`]. Wording is checked before falling back to
/// the status code, since a wording match (e.g. a 429 with usage-limit wording)
/// is more specific than the code's usual bucket.
pub fn classify_error(sighting: &ApiErrorSighting) -> ErrorClass {
    let detail_lc = sighting.detail.to_lowercase();
    let code = sighting.code;

    let is_account_limit = matches!(code, Some(401) | Some(403))
        || ACCOUNT_LIMIT_WORDS.iter().any(|w| detail_lc.contains(w));
    if is_account_limit {
        return ErrorClass::AccountLimit;
    }

    let is_4xx_ish =
        code.is_some_and(|c| (400..500).contains(&c)) || detail_lc.contains("invalid_request_error");
    if is_4xx_ish && looks_structurally_poisoned(&sighting.detail, &detail_lc) {
        return ErrorClass::Poisoned;
    }

    let transient_code = code.is_some_and(|c| TRANSIENT_CODES.contains(&c));
    let transient_wording = TRANSIENT_WORDS.iter().any(|w| detail_lc.contains(w));
    if transient_code || transient_wording {
        return ErrorClass::Transient;
    }

    ErrorClass::Unknown
}

/// Does `detail` complain about message/content-block *structure* (as opposed to,
/// say, an invalid parameter value)? `detail_lc` is `detail.to_lowercase()`,
/// passed in to avoid re-lowering per caller.
fn looks_structurally_poisoned(detail: &str, detail_lc: &str) -> bool {
    detail.contains("tool_use_id")
        || (detail_lc.contains("tool_result") && detail_lc.contains("tool_use"))
        || detail_lc.contains("must be followed by")
        || detail_lc.contains("must followed by")
        || contains_messages_dot_n_dot_content(detail)
}

/// Does `detail` contain a `messages.<digits>.content` path fragment anywhere
/// (Anthropic's per-message error path, e.g. `messages.1.content.0: …`)?
fn contains_messages_dot_n_dot_content(detail: &str) -> bool {
    const PREFIX: &str = "messages.";
    for (idx, _) in detail.match_indices(PREFIX) {
        let rest = &detail[idx + PREFIX.len()..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(".content") {
            return true;
        }
    }
    false
}

// ===================== 3) Surgical transcript repair =====================

/// Result of [`repair_poisoned_transcript`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairResult {
    /// The transcript with poisoned records removed. Every byte of every
    /// *kept* line (including its original line terminator) is copied verbatim
    /// from the input — repair never re-serializes JSON, so surviving lines are
    /// byte-identical to the source.
    pub repaired: String,
    /// 0-based indices (by line, `\n`-delimited) of records that were dropped.
    pub dropped_lines: Vec<usize>,
}

/// Drop any JSONL record whose `message.content` array contains a
/// `tool_result`/`tool_search_tool_result` block referencing a `tool_use_id`
/// that has no matching `tool_use`/`server_tool_use` block earlier in the
/// transcript (including earlier in the *same* record). This is exactly the
/// shape of the `API Error: 400 … tool_search_tool_result …` incident: a
/// leftover tool-result block whose producer never made it into the transcript.
///
/// Pure and byte-preserving: records that don't need to change are copied
/// verbatim (not re-parsed-and-reserialized), so running this on an already-
/// healthy transcript is a no-op, and running it twice is idempotent.
pub fn repair_poisoned_transcript(jsonl: &str) -> RepairResult {
    let mut producer_ids: HashSet<String> = HashSet::new();
    let mut dropped = Vec::new();
    let mut out = String::with_capacity(jsonl.len());

    for (i, (line, terminator)) in split_lines_with_terminators(jsonl).into_iter().enumerate() {
        let parsed = serde_json::from_str::<serde_json::Value>(line).ok();
        let healthy = match &parsed {
            Some(v) => record_is_healthy(v, &producer_ids),
            // Unparseable lines aren't ours to judge — pass them through untouched.
            None => true,
        };
        if !healthy {
            dropped.push(i);
            continue;
        }
        if let Some(v) = &parsed {
            collect_producer_ids(v, &mut producer_ids);
        }
        out.push_str(line);
        out.push_str(terminator);
    }

    RepairResult {
        repaired: out,
        dropped_lines: dropped,
    }
}

/// Split `s` into `(line, terminator)` pairs where `terminator` is `"\n"` for
/// every line but a final one with no trailing newline (terminator `""`).
/// Concatenating every `line` + `terminator` reproduces `s` exactly.
fn split_lines_with_terminators(s: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            out.push((&s[start..i], &s[i..i + 1]));
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push((&s[start..], ""));
    }
    out
}

fn message_content_blocks(v: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    v.get("message")?.get("content")?.as_array()
}

/// Does every `tool_result`/`tool_search_tool_result` block in `v`'s content
/// reference a `tool_use_id` that's already known — either from an earlier
/// record (`producer_ids`) or an earlier block in this same record?
fn record_is_healthy(v: &serde_json::Value, producer_ids: &HashSet<String>) -> bool {
    let Some(blocks) = message_content_blocks(v) else {
        return true;
    };
    let mut local_producers: HashSet<&str> = HashSet::new();
    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") | Some("server_tool_use") => {
                if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                    local_producers.insert(id);
                }
            }
            Some("tool_result") | Some("tool_search_tool_result") => {
                if let Some(tool_use_id) = block.get("tool_use_id").and_then(|i| i.as_str()) {
                    if !producer_ids.contains(tool_use_id) && !local_producers.contains(tool_use_id) {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    true
}

fn collect_producer_ids(v: &serde_json::Value, out: &mut HashSet<String>) {
    let Some(blocks) = message_content_blocks(v) else {
        return;
    };
    for block in blocks {
        if matches!(
            block.get("type").and_then(|t| t.as_str()),
            Some("tool_use") | Some("server_tool_use")
        ) {
            if let Some(id) = block.get("id").and_then(|i| i.as_str()) {
                out.insert(id.to_string());
            }
        }
    }
}

// ===================== 4) Resolving which session to resume =====================

/// One transcript store to search: an optional account `config_dir` (`None` =
/// the default `~/.claude`, matching [`crate::claude_history`]'s convention for
/// the primary account) paired with that store's `projects` directory (the dir
/// that directly contains the encoded-cwd subdirectories).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStore {
    pub config_dir: Option<PathBuf>,
    pub projects_root: PathBuf,
}

/// A candidate transcript file for `claude --resume`, found under one
/// [`SessionStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCandidate {
    /// `.jsonl` filename stem — what `claude --resume <id>` takes.
    pub session_id: String,
    /// Full path to the transcript file.
    pub path: PathBuf,
    /// The account this candidate lives under. When `Some`, resuming it
    /// requires launching `claude` with `CLAUDE_CONFIG_DIR` set to this dir —
    /// resuming under the wrong account silently can't find the session.
    pub config_dir: Option<PathBuf>,
    /// Last-modified time, epoch milliseconds. `None` if unavailable. Drives
    /// recency ordering.
    pub modified_at: Option<u64>,
}

/// Resolve resume candidates for `project_root` across every account's default
/// transcript store, newest-first.
pub fn resolve_session_candidates(project_root: &Path) -> Vec<SessionCandidate> {
    let mut stores = Vec::new();
    if let Some(root) = crate::claude_history::claude_projects_root() {
        stores.push(SessionStore {
            config_dir: None,
            projects_root: root,
        });
    }
    for dir in crate::claude_accounts::config_dirs() {
        stores.push(SessionStore {
            config_dir: Some(dir.clone()),
            projects_root: dir.join("projects"),
        });
    }
    resolve_session_candidates_in(&stores, project_root)
}

/// Like [`resolve_session_candidates`] but against explicit `stores` (the
/// test seam). Searches `store.projects_root.join(encode_project_dir(project_root))`
/// in each store, merges, and sorts newest-first (ties broken by `session_id`
/// descending so ordering is deterministic even with equal/missing mtimes).
pub fn resolve_session_candidates_in(stores: &[SessionStore], project_root: &Path) -> Vec<SessionCandidate> {
    let encoded = crate::claude_history::encode_project_dir(project_root);
    let mut out = Vec::new();

    for store in stores {
        let dir = store.projects_root.join(&encoded);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl")) {
                continue;
            }
            let Some(session_id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let modified_at = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64);
            out.push(SessionCandidate {
                session_id,
                path,
                config_dir: store.config_dir.clone(),
                modified_at,
            });
        }
    }

    out.sort_by(|a, b| b.modified_at.cmp(&a.modified_at).then_with(|| b.session_id.cmp(&a.session_id)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const POISONED_FIXTURE: &str = include_str!("../tests/fixtures/g2-poisoned-transcript.jsonl");
    const INCIDENT_DETAIL_ID: &str = "srvtoolu_01UeFmBLSL1NEDjQSCnjjdtx";

    // ---- 1) detection ----

    #[test]
    fn detects_incident_line_idle_prompt() {
        let tail = "some earlier output\n\
             API Error: 400 messages.1.content.0: unexpected `tool_use_id` found in \
             `tool_search_tool_result` blocks: srvtoolu_01UeFmBLSL1NEDjQSCnjjdtx. Each \
             `tool_search_tool_result` block must have a corresponding `server_tool_use` \
             block before it.\n\
             > ";
        let sighting = detect_api_error(tail).expect("should detect");
        assert_eq!(sighting.code, Some(400));
        assert!(sighting.detail.contains(INCIDENT_DETAIL_ID));
        assert!(!sighting.retrying);
    }

    #[test]
    fn detects_transient_variants_with_correct_code() {
        let cases = [
            (
                r#"API Error: 429 {"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                429,
            ),
            ("API Error: 529 overloaded", 529),
            ("API Error: 500 internal server error", 500),
        ];
        for (line, expected_code) in cases {
            let sighting = detect_api_error(line).unwrap_or_else(|| panic!("should detect: {line}"));
            assert_eq!(sighting.code, Some(expected_code), "line: {line}");
        }
    }

    #[test]
    fn healthy_but_quiet_tail_is_not_a_false_positive() {
        let tail = "Compiling hyperpanes-core v0.0.0\n\
             warning: unused variable `x`\n\
             Finished dev profile in 1.2s\n\
             Let me look at that file.\n\
             > ";
        assert_eq!(detect_api_error(tail), None);
    }

    #[test]
    fn retrying_banner_is_flagged_as_still_alive() {
        let tail = "API Error: 529 overloaded · Retrying in 8 seconds...";
        let sighting = detect_api_error(tail).expect("should detect");
        assert_eq!(sighting.code, Some(529));
        assert!(sighting.retrying);
    }

    #[test]
    fn codeless_variant_has_no_code_and_parenthesized_detail() {
        let tail = "API Error (Request timed out)";
        let sighting = detect_api_error(tail).expect("should detect");
        assert_eq!(sighting.code, None);
        assert!(sighting.detail.contains("Request timed out"));
    }

    #[test]
    fn detects_last_api_error_line_when_several_present() {
        let tail = "API Error: 500 first failure\nsome retry text\nAPI Error: 529 overloaded";
        let sighting = detect_api_error(tail).expect("should detect");
        assert_eq!(sighting.code, Some(529));
    }

    // ---- 2) classification ----

    fn sighting(code: Option<u16>, detail: &str) -> ApiErrorSighting {
        ApiErrorSighting {
            code,
            detail: detail.to_string(),
            retrying: false,
        }
    }

    #[test]
    fn classification_table() {
        let cases: Vec<(ApiErrorSighting, ErrorClass)> = vec![
            (
                sighting(
                    Some(400),
                    "messages.1.content.0: unexpected `tool_use_id` found in `tool_search_tool_result` \
                     blocks: srvtoolu_01UeFmBLSL1NEDjQSCnjjdtx. Each `tool_search_tool_result` block must \
                     have a corresponding `server_tool_use` block before it.",
                ),
                ErrorClass::Poisoned,
            ),
            (
                sighting(
                    Some(429),
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                ),
                ErrorClass::Transient,
            ),
            (sighting(Some(529), "overloaded"), ErrorClass::Transient),
            (sighting(Some(500), "internal server error"), ErrorClass::Transient),
            (sighting(Some(503), "service unavailable"), ErrorClass::Transient),
            (sighting(Some(408), "request timeout"), ErrorClass::Transient),
            (sighting(None, "(Request timed out)"), ErrorClass::Transient),
            (sighting(Some(401), "invalid x-api-key"), ErrorClass::AccountLimit),
            (sighting(Some(403), "forbidden"), ErrorClass::AccountLimit),
            (
                sighting(Some(429), "You have exceeded your usage limit for this billing period"),
                ErrorClass::AccountLimit,
            ),
            (
                sighting(Some(400), "max_tokens: invalid value, must be a positive integer"),
                ErrorClass::Unknown,
            ),
            (sighting(Some(418), "I'm a teapot"), ErrorClass::Unknown),
        ];

        for (sighting, expected) in cases {
            assert_eq!(
                classify_error(&sighting),
                expected,
                "sighting: {sighting:?}"
            );
        }
    }

    // ---- 3) surgical repair ----

    fn fixture_lines() -> Vec<(&'static str, &'static str)> {
        split_lines_with_terminators(POISONED_FIXTURE)
    }

    #[test]
    fn golden_fixture_drops_only_the_orphaned_line() {
        let result = repair_poisoned_transcript(POISONED_FIXTURE);
        assert_eq!(result.dropped_lines, vec![9]);

        let expected: String = fixture_lines()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i != 9)
            .map(|(_, (line, term))| format!("{line}{term}"))
            .collect();
        assert_eq!(result.repaired, expected, "kept lines must be byte-identical to source");
    }

    #[test]
    fn repair_is_idempotent() {
        let once = repair_poisoned_transcript(POISONED_FIXTURE);
        let twice = repair_poisoned_transcript(&once.repaired);
        assert!(twice.dropped_lines.is_empty());
        assert_eq!(twice.repaired, once.repaired);
    }

    #[test]
    fn healthy_no_op_on_fixture_with_orphan_already_removed() {
        let already_repaired: String = fixture_lines()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i != 9)
            .map(|(_, (line, term))| format!("{line}{term}"))
            .collect();
        let result = repair_poisoned_transcript(&already_repaired);
        assert!(result.dropped_lines.is_empty());
        assert_eq!(result.repaired, already_repaired);
    }

    #[test]
    fn healthy_no_op_on_synthetic_paired_transcript() {
        let healthy = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\
             [{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{}}]}}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\
             [{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"ok\"}]}}\n";
        let result = repair_poisoned_transcript(healthy);
        assert!(result.dropped_lines.is_empty());
        assert_eq!(result.repaired, healthy);
    }

    // ---- 4) resolve_session_candidates_in ----

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hp-claude-recovery-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(dir: &Path, name: &str, offset_secs: u64) {
        let path = dir.join(name);
        std::fs::write(&path, "{\"type\":\"summary\",\"summary\":\"x\"}\n").unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(offset_secs))
            .unwrap();
    }

    #[test]
    fn resolves_candidates_across_stores_newest_first_with_correct_config_dir() {
        let project_root = Path::new("/home/eyalmizrachi/dev/hyperpanes");
        let encoded = crate::claude_history::encode_project_dir(project_root);

        let default_root = temp_dir("default");
        let alt_root = temp_dir("alt");
        let alt_config_dir = temp_dir("alt-config");

        let default_projects = default_root.join("projects");
        let alt_projects = alt_root.join("projects");
        std::fs::create_dir_all(default_projects.join(&encoded)).unwrap();
        std::fs::create_dir_all(alt_projects.join(&encoded)).unwrap();

        // Three sessions, distinct mtimes, spread across the two stores.
        write_session(&default_projects.join(&encoded), "oldest.jsonl", 100);
        write_session(&default_projects.join(&encoded), "newest.jsonl", 300);
        write_session(&alt_projects.join(&encoded), "middle.jsonl", 200);

        let stores = vec![
            SessionStore {
                config_dir: None,
                projects_root: default_projects,
            },
            SessionStore {
                config_dir: Some(alt_config_dir.clone()),
                projects_root: alt_projects,
            },
        ];

        let candidates = resolve_session_candidates_in(&stores, project_root);
        assert_eq!(candidates.len(), 3);

        let ids: Vec<&str> = candidates.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(ids, vec!["newest", "middle", "oldest"], "newest-first across stores");

        let by_id: std::collections::HashMap<&str, &SessionCandidate> =
            candidates.iter().map(|c| (c.session_id.as_str(), c)).collect();
        assert_eq!(by_id["newest"].config_dir, None);
        assert_eq!(by_id["oldest"].config_dir, None);
        assert_eq!(by_id["middle"].config_dir, Some(alt_config_dir));

        let _ = std::fs::remove_dir_all(&default_root);
        let _ = std::fs::remove_dir_all(&alt_root);
        let _ = std::fs::remove_dir_all(&alt_config_dir);
    }

    #[test]
    fn missing_store_dir_yields_no_candidates() {
        let stores = vec![SessionStore {
            config_dir: None,
            projects_root: std::env::temp_dir().join(format!("hp-claude-recovery-missing-{}", uuid::Uuid::new_v4())),
        }];
        assert!(resolve_session_candidates_in(&stores, Path::new("/nowhere")).is_empty());
    }
}
