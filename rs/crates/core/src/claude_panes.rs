//! Per-pane Claude Code session markers — the bridge that lets a relaunch resume the
//! conversation each pane was having.
//!
//! A Claude Code `SessionStart` hook (`resources/claude/hp-claude-session-hook.sh`) runs
//! inside every `claude` a user launches in a pane. The pane's environment carries
//! `HYPERPANES_PANE_ID`, and the hook's stdin carries the conversation's `session_id` —
//! so the hook writes `<state dir>/claude-sessions/<pane-id>.json`:
//!
//! ```json
//! { "sessionId": "0198c4a2-…", "cwd": "/home/me/dev/x" }
//! ```
//!
//! `SessionEnd` removes the marker, so a marker exists exactly while a conversation is
//! live in that pane. The GUI's relaunch snapshot ([`crate::workspace::model::PaneSpec`]
//! via `to_session_file`) embeds the id as pane meta (key [`META_KEY`]); on restore, a
//! pane whose live session did NOT survive re-spawns and resumes the conversation with
//! `claude --resume <id>` in its original cwd.
//!
//! The session id is embedded into a command line at restore, so [`read_pane_session`]
//! accepts only ids matching Claude's UUID shape (`[0-9a-fA-F-]`) — anything else is
//! treated as a corrupt/hostile marker and ignored.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::persistence::paths;

/// Pane-meta key under which the snapshot records the pane's live Claude session id.
pub const META_KEY: &str = "claude.session";

/// Pane-meta key for the conversation's working directory. `claude --resume <id>` only
/// finds sessions belonging to the CURRENT directory's project, and a pane's own cwd
/// snapshot can be stale (a shell parked inside a TUI never re-emits OSC 7 after a GUI
/// re-attach) — so the hook-reported cwd is authoritative and restore must `cd` first.
pub const META_CWD_KEY: &str = "claude.cwd";

/// Pane-meta key for the conversation's `CLAUDE_CONFIG_DIR` (the account it was saved
/// under). `claude` stores transcripts in `$CLAUDE_CONFIG_DIR/projects`, so a relaunch must
/// set the SAME dir for `claude --resume <id>` to find the session — without it, a pane that
/// ran under a rotated/non-default account resumes against `~/.claude` and finds nothing.
/// Empty/absent ⇒ the default account (`~/.claude`), so restore sets nothing.
pub const META_CONFIG_DIR_KEY: &str = "claude.config_dir";

/// Pane-meta key for the argv (after the program) of the `claude` a user typed into a shell
/// pane: a JSON array string. Absent for old markers and for panes that launch claude directly
/// (their own argv is already in the snapshot).
pub const META_ARGV_KEY: &str = "claude.argv";

/// Pane-meta key holding the reason the argv was deliberately not recorded (`print`, `secret`,
/// `too-long`, `unsupported`). Restore shows it in a notice.
pub const META_ARGV_UNAVAILABLE_KEY: &str = "claude.argv_unavailable";

/// Longest JSON-encoded argv recorded or replayed.
pub const MAX_ARGV_JSON_BYTES: usize = 4096;

/// Is `cwd` safe to interpolate into a single-quoted `cd '<cwd>'`? Absolute, no control
/// characters, and no single quotes (rather than escaping, refuse — real project paths
/// never contain them, and refusing keeps the injection reasoning trivial).
pub fn valid_resume_cwd(cwd: &str) -> bool {
    cwd.starts_with('/')
        && cwd.len() < 1024
        && !cwd.contains('\'')
        && !cwd.chars().any(|c| c.is_control())
}

/// Is `dir` safe to interpolate into a single-quoted `CLAUDE_CONFIG_DIR='<dir>'` prefix (or
/// pass as a spawn env value)? Same gate as [`valid_resume_cwd`] — absolute, bounded, no
/// single quotes, no control chars — since a marker is external, best-effort state that
/// lands on a command line. A non-empty but invalid dir is treated as "no config dir".
pub fn valid_config_dir(dir: &str) -> bool {
    valid_resume_cwd(dir)
}

/// One pane's live Claude conversation, as reported by the session hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneClaudeSession {
    /// The conversation's session id — what `claude --resume <id>` takes.
    pub session_id: String,
    /// The conversation's working directory at SessionStart (informational; the pane's
    /// own cwd snapshot is what restore actually uses).
    #[serde(default)]
    pub cwd: String,
    /// The `CLAUDE_CONFIG_DIR` this conversation was saved under (empty ⇒ the default
    /// `~/.claude` account). Restore must re-set it so `claude --resume` finds the session
    /// in the right per-account transcript store. Older markers without the field parse
    /// with an empty string (the default account), preserving pre-multi-account behaviour.
    #[serde(default)]
    pub config_dir: String,
    /// The argv (after the program) of the `claude` that started this conversation, when the
    /// hook could capture it. Older markers omit it.
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    /// Why `argv` was not captured (`print`, `secret`, `too-long`, `unsupported`).
    #[serde(default)]
    pub argv_unavailable: Option<String>,
}

/// What the snapshot knows about the flags a shell-typed claude was started with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RecordedArgv {
    /// An old marker (or a direct claude launch): nothing recorded, nothing to say.
    #[default]
    Absent,
    Argv(Vec<String>),
    /// Deliberately not recorded, with the reason.
    Unavailable(String),
}

impl RecordedArgv {
    /// Read the two argv meta keys of a snapshot pane. A malformed `claude.argv` counts as
    /// unavailable-as-unsafe rather than absent: something was recorded, and it is not usable.
    pub fn from_meta(meta: Option<&std::collections::BTreeMap<String, String>>) -> Self {
        let Some(meta) = meta else {
            return Self::Absent;
        };
        if let Some(reason) = meta.get(META_ARGV_UNAVAILABLE_KEY) {
            return Self::Unavailable(reason.clone());
        }
        match meta.get(META_ARGV_KEY) {
            None => Self::Absent,
            Some(j) => match serde_json::from_str::<Vec<String>>(j) {
                Ok(v) => Self::Argv(v),
                Err(_) => Self::Unavailable("unsafe".to_string()),
            },
        }
    }
}

impl PaneClaudeSession {
    /// The snapshot meta entry for the recorded argv: `claude.argv_unavailable` (the reason)
    /// wins over `claude.argv` (a JSON array string); `None` for an old marker with neither.
    pub fn argv_meta(&self) -> Option<(&'static str, String)> {
        if let Some(reason) = self.argv_unavailable.as_ref().filter(|r| !r.is_empty()) {
            return Some((META_ARGV_UNAVAILABLE_KEY, reason.clone()));
        }
        let argv = self.argv.as_ref()?;
        Some((META_ARGV_KEY, serde_json::to_string(argv).ok()?))
    }
}

const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "AIza",
];

/// Does `tokens` look like it carries a credential, or inline JSON config (which can embed
/// one) in place of a file path? Mirrors the capture-time filter in the session hook.
pub fn argv_has_secret(tokens: &[String]) -> bool {
    let inline = |t: &str| t.starts_with('{') || t.starts_with('[');
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if SECRET_PREFIXES.iter().any(|p| t.starts_with(p)) || t.contains("-----BEGIN") {
            return true;
        }
        if let Some((f, v)) = t.split_once('=') {
            if matches!(f, "--settings" | "--mcp-config" | "--agents") && inline(v) {
                return true;
            }
        } else if matches!(t, "--settings" | "--agents") {
            if tokens.get(i + 1).is_some_and(|v| inline(v)) {
                return true;
            }
        } else if t == "--mcp-config" {
            let mut j = i + 1;
            while j < tokens.len() && !tokens[j].starts_with('-') {
                if inline(&tokens[j]) {
                    return true;
                }
                j += 1;
            }
        }
        i += 1;
    }
    false
}

/// Defence-in-depth gate on a recorded argv before it is typed into a shell: bounded size, no
/// control characters (a newline would end the typed line early), no secrets. `Err` carries
/// the notice reason.
pub fn validate_recorded_argv(tokens: &[String]) -> Result<(), &'static str> {
    let json_len = serde_json::to_string(tokens)
        .map(|j| j.len())
        .unwrap_or(usize::MAX);
    if json_len > MAX_ARGV_JSON_BYTES
        || tokens.iter().any(|t| t.chars().any(|c| c.is_control()))
        || argv_has_secret(tokens)
    {
        return Err("unsafe");
    }
    Ok(())
}

/// Single-quote `s` for a POSIX shell (`'` becomes `'\''`).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The line typed into a restored SHELL pane to resume a conversation. The program is the
/// literal `claude` so the user's alias still applies. With a usable recorded argv the original
/// flags are replayed (session flags and the positional prompt dropped); otherwise it is the
/// bare resume line, preceded by a visible notice when flags were recorded-but-unusable.
pub fn shell_resume_line(
    cwd: Option<&str>,
    config_dir: Option<&str>,
    session_id: &str,
    recorded: &RecordedArgv,
) -> String {
    let mut notice = String::new();
    let mut flags: Vec<String> = Vec::new();
    let mut note = |reason: &str| {
        // The reason lands inside single quotes: keep it to a boring alphabet.
        let ok = !reason.is_empty()
            && reason.len() <= 40
            && reason
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == ' ' || c == '-');
        let reason = if ok { reason } else { "unavailable" };
        notice =
            format!("echo '[hyperpanes] resumed without the original claude flags ({reason})'; ");
    };
    match recorded {
        RecordedArgv::Absent => {}
        RecordedArgv::Unavailable(r) => note(r),
        RecordedArgv::Argv(tokens) => match validate_recorded_argv(tokens) {
            Err(r) => note(r),
            Ok(()) => {
                let c = crate::claude_argv::classify(tokens);
                if c.has_unknown {
                    note("unknown flag");
                } else if c.print_mode {
                    note("print");
                } else {
                    let kept: Vec<String> = tokens
                        .iter()
                        .zip(&c.kinds)
                        .filter(|(_, k)| **k == crate::claude_argv::TokenKind::Flag)
                        .map(|(t, _)| t.clone())
                        .collect();
                    flags = crate::control::dispatch::strip_session_flag_tokens(&kept);
                }
            }
        },
    }
    let mut line = String::new();
    line.push_str(&notice);
    if let Some(cwd) = cwd {
        line.push_str(&format!("cd '{cwd}' && "));
    }
    if let Some(d) = config_dir {
        line.push_str(&format!("CLAUDE_CONFIG_DIR='{d}' "));
    }
    line.push_str("claude ");
    for f in &flags {
        line.push_str(&shell_quote(f));
        line.push(' ');
    }
    line.push_str(&format!("--resume {session_id}\r"));
    line
}

/// The marker file for one pane id.
fn marker_path(pane_id: &str) -> PathBuf {
    paths::claude_sessions_dir().join(format!("{pane_id}.json"))
}

/// Is `id` shaped like a Claude session id (UUID: hex + dashes, sane length)? The id is
/// later interpolated into a shell command line, so this is a safety gate, not a nicety.
/// Public because restore re-checks ids read back from `workspace.json` — that file is
/// user-editable, so the write-time validation here cannot be assumed to hold.
pub fn valid_session_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Read the live-session marker for `pane_id`, if one exists and is well-formed.
/// Missing file, unparseable JSON, or a malformed id all yield `None` — a marker is
/// best-effort state written by an external hook, never trusted blindly.
pub fn read_pane_session(pane_id: &str) -> Option<PaneClaudeSession> {
    let text = fs::read_to_string(marker_path(pane_id)).ok()?;
    let parsed: PaneClaudeSession = serde_json::from_str(&text).ok()?;
    valid_session_id(&parsed.session_id).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_uuid_shaped_ids_only() {
        assert!(valid_session_id("0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a"));
        assert!(valid_session_id("deadbeef"));
        // Shell metacharacters, spaces, quotes — all refused.
        assert!(!valid_session_id("abc; rm -rf /"));
        assert!(!valid_session_id("abc def"));
        assert!(!valid_session_id("$(boom)"));
        assert!(!valid_session_id("ab")); // too short
        assert!(!valid_session_id(&"a".repeat(65))); // too long
    }

    #[test]
    fn resume_cwd_gate() {
        assert!(valid_resume_cwd("/home/me/dev/x"));
        assert!(valid_resume_cwd("/tmp/a b/c")); // spaces fine inside single quotes
        assert!(!valid_resume_cwd("relative/path"));
        assert!(!valid_resume_cwd("/has'quote"));
        assert!(!valid_resume_cwd("/has\nnewline"));
        assert!(!valid_resume_cwd(""));
    }

    #[test]
    fn parses_marker_json_shape() {
        let parsed: PaneClaudeSession = serde_json::from_str(
            r#"{ "sessionId": "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a", "cwd": "/w", "configDir": "/home/me/.claude-alt" }"#,
        )
        .unwrap();
        assert_eq!(parsed.session_id, "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a");
        assert_eq!(parsed.cwd, "/w");
        assert_eq!(parsed.config_dir, "/home/me/.claude-alt");
        // cwd + configDir are optional — an older/minimal hook payload (pre-multi-account)
        // still parses, defaulting to the default account.
        let bare: PaneClaudeSession =
            serde_json::from_str(r#"{ "sessionId": "deadbeef" }"#).unwrap();
        assert_eq!(bare.cwd, "");
        assert_eq!(bare.config_dir, "");
    }

    #[test]
    fn config_dir_gate() {
        assert!(valid_config_dir("/home/me/.claude-alt"));
        assert!(!valid_config_dir("")); // empty = no config dir, not "valid"
        assert!(!valid_config_dir("relative/.claude"));
        assert!(!valid_config_dir("/has'quote"));
    }

    fn toks(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }
    const ID: &str = "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a";

    #[test]
    fn old_marker_parses_without_argv() {
        let m: PaneClaudeSession =
            serde_json::from_str(r#"{ "sessionId": "deadbeef", "cwd": "/w" }"#).unwrap();
        assert_eq!(m.argv, None);
        assert_eq!(m.argv_unavailable, None);
        assert_eq!(m.argv_meta(), None);
    }

    #[test]
    fn argv_meta_embeds_argv_or_reason() {
        let m: PaneClaudeSession =
            serde_json::from_str(r#"{ "sessionId": "deadbeef", "argv": ["--model", "opus"] }"#)
                .unwrap();
        assert_eq!(
            m.argv_meta(),
            Some((META_ARGV_KEY, r#"["--model","opus"]"#.to_string()))
        );
        let m: PaneClaudeSession =
            serde_json::from_str(r#"{ "sessionId": "deadbeef", "argvUnavailable": "secret" }"#)
                .unwrap();
        assert_eq!(
            m.argv_meta(),
            Some((META_ARGV_UNAVAILABLE_KEY, "secret".to_string()))
        );
    }

    #[test]
    fn shell_line_replays_flags_and_drops_session_and_prompt() {
        let rec = RecordedArgv::Argv(toks(&[
            "--dangerously-skip-permissions",
            "--model",
            "opus",
            "do the thing",
            "--resume",
            "old-id",
        ]));
        assert_eq!(
            shell_resume_line(
                Some("/home/user/dev/myapp"),
                Some("/home/user/.alt"),
                ID,
                &rec
            ),
            format!(
                "cd '/home/user/dev/myapp' && CLAUDE_CONFIG_DIR='/home/user/.alt' claude \
                 '--dangerously-skip-permissions' '--model' 'opus' --resume {ID}\r"
            )
        );
    }

    #[test]
    fn shell_line_quotes_hostile_tokens() {
        let rec = RecordedArgv::Argv(toks(&[
            "--append-system-prompt",
            "it's $HOME; echo `x` $(y) a b",
        ]));
        let line = shell_resume_line(None, None, ID, &rec);
        assert_eq!(
            line,
            format!(
                "claude '--append-system-prompt' 'it'\\''s $HOME; echo `x` $(y) a b' --resume {ID}\r"
            )
        );
    }

    #[test]
    fn shell_line_falls_back_with_a_notice() {
        let bare = format!("claude --resume {ID}\r");
        let notice = |r: &str| {
            format!("echo '[hyperpanes] resumed without the original claude flags ({r})'; ")
        };
        let cases: Vec<(RecordedArgv, &str)> = vec![
            (RecordedArgv::Unavailable("secret".into()), "secret"),
            (RecordedArgv::Unavailable("too-long".into()), "too-long"),
            (
                RecordedArgv::Unavailable("x'; rm -rf /".into()),
                "unavailable",
            ),
            (RecordedArgv::Argv(toks(&["--frobnicate"])), "unknown flag"),
            (RecordedArgv::Argv(toks(&["--model", "a\nb"])), "unsafe"),
            (
                RecordedArgv::Argv(toks(&["--model", "sk-abc123"])),
                "unsafe",
            ),
            (RecordedArgv::Argv(toks(&["-p", "hi"])), "print"),
        ];
        for (rec, reason) in cases {
            assert_eq!(
                shell_resume_line(None, None, ID, &rec),
                format!("{}{bare}", notice(reason)),
                "{rec:?}"
            );
        }
        // An old marker: bare line, no notice.
        assert_eq!(
            shell_resume_line(None, None, ID, &RecordedArgv::Absent),
            bare
        );
    }

    #[test]
    fn recorded_argv_from_meta_shapes() {
        use std::collections::BTreeMap;
        assert_eq!(RecordedArgv::from_meta(None), RecordedArgv::Absent);
        let mut m = BTreeMap::new();
        m.insert(META_ARGV_KEY.to_string(), r#"["--verbose"]"#.to_string());
        assert_eq!(
            RecordedArgv::from_meta(Some(&m)),
            RecordedArgv::Argv(toks(&["--verbose"]))
        );
        m.insert(META_ARGV_KEY.to_string(), "not json".to_string());
        assert_eq!(
            RecordedArgv::from_meta(Some(&m)),
            RecordedArgv::Unavailable("unsafe".into())
        );
        m.insert(META_ARGV_UNAVAILABLE_KEY.to_string(), "print".to_string());
        assert_eq!(
            RecordedArgv::from_meta(Some(&m)),
            RecordedArgv::Unavailable("print".into())
        );
    }

    #[test]
    fn validation_catches_secrets_inline_json_and_size() {
        assert!(validate_recorded_argv(&toks(&["--model", "opus"])).is_ok());
        assert!(validate_recorded_argv(&toks(&["--settings", "/p/s.json"])).is_ok());
        assert!(validate_recorded_argv(&toks(&["--settings", "{\"a\":1}"])).is_err());
        assert!(validate_recorded_argv(&toks(&["--mcp-config=[1]"])).is_err());
        assert!(validate_recorded_argv(&toks(&["--mcp-config", "/a.json", "{}"])).is_err());
        assert!(validate_recorded_argv(&toks(&["x-----BEGIN KEY"])).is_err());
        assert!(validate_recorded_argv(&toks(&["--name", &"a".repeat(5000)])).is_err());
    }

    // ---- the session hook's argv capture (inline python, run against a fake /proc) ----

    const HOOK: &str = include_str!("../../../../resources/claude/hp-claude-session-hook.sh");

    /// Build a fake /proc with `(pid, ppid, argv)` entries, then run the hook's `capture` from
    /// `start` with `existing` as the marker already on disk. `None` when python3 is missing.
    fn hook_capture(
        procs: &[(u32, u32, &[&str])],
        start: u32,
        existing: Option<serde_json::Value>,
        platform: &str,
    ) -> Option<serde_json::Value> {
        let dir = std::env::temp_dir().join(format!("hp-hook-{}", uuid::Uuid::new_v4()));
        for (pid, ppid, argv) in procs {
            let d = dir.join(pid.to_string());
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("stat"),
                format!("{pid} (some (proc)) S {ppid} 1 1 0\n"),
            )
            .unwrap();
            let mut raw = argv.join("\0").into_bytes();
            raw.push(0);
            fs::write(d.join("cmdline"), raw).unwrap();
        }
        let start_py = HOOK
            .find("python3 -c '")
            .map(|i| i + "python3 -c '".len())?;
        let end_py = HOOK.rfind("\n' 2>/dev/null")?;
        let src = &HOOK[start_py..end_py];
        let harness = String::from(
            "import json,sys\nexec(compile(sys.argv[1], \"hook\", \"exec\"))\n\
             print(json.dumps(capture(sys.argv[2], int(sys.argv[3]), json.loads(sys.argv[4]), sys.argv[5])))\n"
        );
        let out = std::process::Command::new("python3")
            .env("HP_HOOK_TEST", "1")
            .args(["-I", "-c", &harness, src])
            .arg(&dir)
            .arg(start.to_string())
            .arg(existing.map(|e| e.to_string()).unwrap_or("null".into()))
            .arg(platform)
            .output()
            .ok()?;
        let _ = fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).ok()
    }

    /// shell(10) -> claude(20) -> sh(30, the hook); the capture starts at the hook's parent.
    fn chain<'a>(claude_argv: &'a [&'a str]) -> Vec<(u32, u32, &'a [&'a str])> {
        vec![
            (10, 1, &["bash"][..]),
            (20, 10, claude_argv),
            (30, 20, &["sh", "/x/hook.sh"][..]),
        ]
    }

    #[test]
    fn hook_captures_flags_of_the_nearest_claude() {
        use serde_json::json;
        let procs = chain(&[
            "claude",
            "--dangerously-skip-permissions",
            "--model",
            "opus",
        ]);
        let Some(got) = hook_capture(&procs, 30, None, "linux") else {
            return;
        };
        assert_eq!(
            got,
            json!({ "argv": ["--dangerously-skip-permissions", "--model", "opus"] })
        );
        // The installed-binary and node launch shapes are recognised too.
        let procs = chain(&["/home/user/.local/share/claude/versions/9.9.9", "--verbose"]);
        assert_eq!(
            hook_capture(&procs, 30, None, "linux").unwrap(),
            json!({ "argv": ["--verbose"] })
        );
        let procs = chain(&["node", "/usr/lib/node_modules/.bin/claude", "--verbose"]);
        assert_eq!(
            hook_capture(&procs, 30, None, "linux").unwrap(),
            json!({ "argv": ["--verbose"] })
        );
    }

    #[test]
    fn hook_keeps_the_existing_marker_for_fork_workers() {
        use serde_json::json;
        let existing = Some(json!({ "argv": ["--model", "opus"] }));
        for argv in [
            &["claude", "--fork-session", "--resume", "x"][..],
            &["claude", "bg-pty-host"][..],
            &["claude", "bg-spare"][..],
        ] {
            let procs = chain(argv);
            let Some(got) = hook_capture(&procs, 30, existing.clone(), "linux") else {
                return;
            };
            assert_eq!(got, json!({ "argv": ["--model", "opus"] }), "{argv:?}");
        }
        // No marker yet: nothing is recorded (and nothing is overwritten with garbage).
        let procs = chain(&["claude", "bg-spare"]);
        assert_eq!(hook_capture(&procs, 30, None, "linux").unwrap(), json!({}));
    }

    #[test]
    fn hook_refuses_print_secret_inline_json_too_long_and_unsupported() {
        use serde_json::json;
        let long = "a".repeat(5000);
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (vec!["claude", "-p", "hi"], "print"),
            (vec!["claude", "--print", "hi"], "print"),
            (vec!["claude", "--model", "sk-ant-abc"], "secret"),
            (
                vec!["claude", "--append-system-prompt", "x-----BEGIN KEY"],
                "secret",
            ),
            (
                vec!["claude", "--settings", "{\"env\":{\"A\":\"b\"}}"],
                "secret",
            ),
            (vec!["claude", "--mcp-config", "/p/a.json", "[1]"], "secret"),
            (vec!["claude", "--agents={\"a\":{}}"], "secret"),
            (
                vec!["claude", "--append-system-prompt", long.as_str()],
                "too-long",
            ),
        ];
        for (argv, why) in cases {
            let procs = chain(&argv);
            let Some(got) = hook_capture(&procs, 30, None, "linux") else {
                return;
            };
            assert_eq!(got, json!({ "argvUnavailable": why }), "{argv:?}");
        }
        // A settings FILE path is fine.
        let procs = chain(&["claude", "--settings", "/p/s.json"]);
        assert_eq!(
            hook_capture(&procs, 30, None, "linux").unwrap(),
            json!({ "argv": ["--settings", "/p/s.json"] })
        );
        // Not Linux, or no claude ancestor in /proc.
        assert_eq!(
            hook_capture(&procs, 30, None, "darwin").unwrap(),
            json!({ "argvUnavailable": "unsupported" })
        );
        let no_claude = vec![(10, 1, &["bash"][..]), (30, 10, &["sh"][..])];
        assert_eq!(
            hook_capture(&no_claude, 30, None, "linux").unwrap(),
            json!({ "argvUnavailable": "unsupported" })
        );
    }
}
