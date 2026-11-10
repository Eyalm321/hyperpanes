//! Which of a pane's spawn-env entries the relaunch snapshot (`last-workspace.json`) may keep.
//!
//! A restored pane is re-spawned from its `PaneSpec`; without its explicit spawn env it comes
//! back with only the base environment. But the snapshot sits on disk, so only the pane's
//! EXPLICIT spawn env is ever considered (never the inherited process environment), and it is
//! filtered by [`persistable_env`]. Rules, applied in this order:
//!
//! 1. Runtime-owned keys (`HYPERPANES_PANE_ID`, `HYPERPANES_CONTROL_FILE`, `HP_TASK_*`,
//!    `HP_FENCING_TOKEN`, `HP_QUEUE`, `HP_WORKTREE_CACHE_DIR`) are always dropped.
//! 2. Secret-looking KEYS (`TOKEN`, `SECRET`, `PASSWORD`, `API_KEY`, `AUTH`, ... as whole
//!    underscore-separated words, case-insensitive) are always dropped.
//! 3. Secret-looking VALUES (`sk-`, `ghp_`, `github_pat_`, `AKIA`, `-----BEGIN`, ...) are always
//!    dropped, in every mode.
//! 4. `PATH`, `LD_*` and `DYLD_*` are kept only in the explicit key-list mode.
//! 5. The mode is the pane meta `hp.persist-env`: absent or `default` keeps an allow-list
//!    (`HP_*`, `HYPERPANES_*`, `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_*`, `ENABLE_TOOL_SEARCH`,
//!    `CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS`, `RUST_LOG`, `RUST_BACKTRACE`, `LANG`, `LC_*`,
//!    `TMPDIR`, plus the embedder's `extra_allow` prefixes); `none` keeps nothing; `all` keeps
//!    everything rules 1-4 left; `K1,K2,...` keeps exactly those keys (minus rules 1-3).
//!
//! The dropped key NAMES are returned (values never are) so a restore can warn about them.

use std::collections::BTreeMap;

/// How much of a pane's spawn env to persist (pane meta `hp.persist-env`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Default,
    None,
    All,
    Keys(Vec<String>),
}

/// Pane meta key selecting the [`Mode`].
pub const META_PERSIST_ENV: &str = "hp.persist-env";
/// Pane meta key listing (comma-joined, capped) the names a restore could not recover.
pub const META_ENV_DROPPED: &str = "hp.env-dropped";
/// Most names listed in `hp.env-dropped`; the rest become `…(+N)`.
pub const DROPPED_META_CAP: usize = 20;

/// Read the persistence mode from pane meta. Unknown/absent/empty ⇒ [`Mode::Default`].
pub fn persist_mode(meta: Option<&BTreeMap<String, String>>) -> Mode {
    let Some(v) = meta.and_then(|m| m.get(META_PERSIST_ENV)) else {
        return Mode::Default;
    };
    let v = v.trim();
    match v.to_ascii_lowercase().as_str() {
        "" | "default" => Mode::Default,
        "none" => Mode::None,
        "all" => Mode::All,
        _ => Mode::Keys(
            v.split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect(),
        ),
    }
}

const RUNTIME_KEYS: &[&str] = &[
    "HYPERPANES_PANE_ID",
    "HYPERPANES_CONTROL_FILE",
    "HP_FENCING_TOKEN",
    "HP_QUEUE",
    "HP_WORKTREE_CACHE_DIR",
    "HYPERPANES_VERSION",
    "HYPERPANES_UNATTENDED",
];

fn is_runtime_key(k: &str) -> bool {
    RUNTIME_KEYS.contains(&k) || k.starts_with("HP_TASK_")
}

pub(crate) const SECRET_WORDS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "APIKEY",
    "ACCESSKEY",
    "PRIVATEKEY",
    "CREDENTIAL",
    "CREDENTIALS",
    "AUTH",
    "COOKIE",
    "BEARER",
];

/// `(^|_)(TOKEN|SECRET|PASSWORD|PASSWD|API_?KEY|ACCESS_?KEY|PRIVATE_?KEY|CREDENTIALS?|AUTH|
/// COOKIE|BEARER)($|_)`, case-insensitive, without a regex dependency.
fn is_secret_key(k: &str) -> bool {
    let up = k.to_ascii_uppercase();
    let words: Vec<&str> = up.split('_').collect();
    for (i, w) in words.iter().enumerate() {
        if SECRET_WORDS.contains(w) {
            return true;
        }
        if let Some(next) = words.get(i + 1) {
            if *next == "KEY" && matches!(*w, "API" | "ACCESS" | "PRIVATE") {
                return true;
            }
        }
    }
    false
}

const SECRET_VALUE_PREFIXES: &[&str] = &[
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

pub(crate) fn is_secret_value(v: &str) -> bool {
    SECRET_VALUE_PREFIXES.iter().any(|p| v.starts_with(p)) || v.contains("-----BEGIN")
}

/// `PATH`, `LD_*`, `DYLD_*`, compared case-insensitively: Windows env keys are case-insensitive
/// and the search path there is usually spelled `Path`.
fn is_path_like(k: &str) -> bool {
    let up = k.to_ascii_uppercase();
    up == "PATH" || up.starts_with("LD_") || up.starts_with("DYLD_")
}

const DEFAULT_ALLOW: &[&str] = &[
    "HP_*",
    "HYPERPANES_*",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_*",
    "ENABLE_TOOL_SEARCH",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "LANG",
    "LC_*",
    "TMPDIR",
];

/// `pat` is exact, or a prefix when it ends in `*`.
fn pattern_matches(pat: &str, key: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => pat == key,
    }
}

/// Filter `env` for persistence. Returns `(kept, dropped key names)`; see the module doc for
/// the rules. `extra_allow` adds embedder-specific key patterns (`*` = prefix) to the default
/// allow-list; it has no effect in the other modes.
pub fn persistable_env(
    env: &BTreeMap<String, String>,
    mode: &Mode,
    extra_allow: &[String],
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut kept = BTreeMap::new();
    let mut dropped = Vec::new();
    for (k, v) in env {
        let keep = !is_runtime_key(k)
            && !is_secret_key(k)
            && !is_secret_value(v)
            && match mode {
                Mode::None => false,
                Mode::Keys(list) => list.iter().any(|l| l == k),
                Mode::All => !is_path_like(k),
                Mode::Default => {
                    !is_path_like(k)
                        && (DEFAULT_ALLOW.iter().any(|p| pattern_matches(p, k))
                            || extra_allow.iter().any(|p| pattern_matches(p, k)))
                }
            };
        if keep {
            kept.insert(k.clone(), v.clone());
        } else {
            dropped.push(k.clone());
        }
    }
    (kept, dropped)
}

/// The `hp.env-dropped` meta value: comma-joined names, at most [`DROPPED_META_CAP`] followed by
/// `…(+N)` for the rest. `None` when nothing was dropped.
pub fn dropped_meta_value(dropped: &[String]) -> Option<String> {
    if dropped.is_empty() {
        return None;
    }
    let mut s = dropped
        .iter()
        .take(DROPPED_META_CAP)
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    if dropped.len() > DROPPED_META_CAP {
        s.push_str(&format!("…(+{})", dropped.len() - DROPPED_META_CAP));
    }
    Some(s)
}

/// Layer the restore spawn env: the persisted `base`, overridden key by key by `overlay` (the
/// rebuilt launch env), overridden by `config_dir` (the conversation's account) as
/// `CLAUDE_CONFIG_DIR`. `None` when all three are empty.
pub fn merge_restore_env(
    base: Option<&BTreeMap<String, String>>,
    overlay: Option<&std::collections::HashMap<String, String>>,
    config_dir: Option<&str>,
) -> Option<std::collections::HashMap<String, String>> {
    let mut out: std::collections::HashMap<String, String> = base
        .map(|b| b.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    if let Some(o) = overlay {
        out.extend(o.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    if let Some(d) = config_dir {
        out.insert("CLAUDE_CONFIG_DIR".to_string(), d.to_string());
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
    fn keys(m: &BTreeMap<String, String>) -> Vec<&str> {
        m.keys().map(String::as_str).collect()
    }
    fn meta(v: &str) -> BTreeMap<String, String> {
        env(&[(META_PERSIST_ENV, v)])
    }

    #[test]
    fn running_version_is_never_persisted() {
        let e = env(&[
            ("HYPERPANES_VERSION", "1.2.3"),
            ("HYPERPANES_UNATTENDED", "1"),
            ("EDITOR", "vim"),
        ]);
        for mode in [
            Mode::Default,
            Mode::All,
            Mode::Keys(vec![
                "HYPERPANES_VERSION".into(),
                "HYPERPANES_UNATTENDED".into(),
                "EDITOR".into(),
            ]),
        ] {
            let (kept, dropped) = persistable_env(&e, &mode, &[]);
            for k in ["HYPERPANES_VERSION", "HYPERPANES_UNATTENDED"] {
                assert!(!kept.contains_key(k), "{k} {mode:?}");
                assert!(dropped.iter().any(|d| d == k), "{k} {mode:?}");
            }
        }
    }

    #[test]
    fn modes_parse() {
        assert_eq!(persist_mode(None), Mode::Default);
        assert_eq!(persist_mode(Some(&meta("default"))), Mode::Default);
        assert_eq!(persist_mode(Some(&meta("none"))), Mode::None);
        assert_eq!(persist_mode(Some(&meta("all"))), Mode::All);
        assert_eq!(
            persist_mode(Some(&meta("A, B"))),
            Mode::Keys(vec!["A".into(), "B".into()])
        );
    }

    #[test]
    fn default_mode_is_allow_list_only() {
        let e = env(&[
            ("HP_FOO", "1"),
            ("LANG", "C"),
            ("LC_ALL", "C"),
            ("RANDOM_VAR", "x"),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000"),
            ("PATH", "/usr/bin"),
        ]);
        let (k, d) = persistable_env(&e, &Mode::Default, &[]);
        assert_eq!(
            keys(&k),
            ["CLAUDE_CODE_MAX_OUTPUT_TOKENS", "HP_FOO", "LANG", "LC_ALL"]
        );
        assert_eq!(d, ["PATH", "RANDOM_VAR"]);
    }

    #[test]
    fn none_keeps_nothing_all_keeps_the_rest() {
        let e = env(&[("MY_VAR", "1"), ("PATH", "/bin"), ("GH_TOKEN", "x")]);
        let (k, d) = persistable_env(&e, &Mode::None, &[]);
        assert!(k.is_empty());
        assert_eq!(d.len(), 3);
        let (k, d) = persistable_env(&e, &Mode::All, &[]);
        assert_eq!(keys(&k), ["MY_VAR"]);
        assert_eq!(d, ["GH_TOKEN", "PATH"]);
    }

    #[test]
    fn path_like_keys_match_case_insensitively() {
        // Windows spells the search path `Path`; `all` must still drop it.
        let e = env(&[
            ("Path", "C:\\Windows"),
            ("ld_preload", "/x.so"),
            ("MY_VAR", "1"),
        ]);
        let (k, d) = persistable_env(&e, &Mode::All, &[]);
        assert_eq!(keys(&k), ["MY_VAR"]);
        assert_eq!(d, ["Path", "ld_preload"]);
    }

    #[test]
    fn list_mode_keeps_exactly_listed_and_allows_path_and_ld() {
        let e = env(&[
            ("PATH", "/bin"),
            ("LD_PRELOAD", "/x.so"),
            ("DYLD_LIBRARY_PATH", "/d"),
            ("MY_VAR", "1"),
            ("OTHER", "2"),
        ]);
        let m = Mode::Keys(vec!["PATH".into(), "LD_PRELOAD".into(), "MY_VAR".into()]);
        let (k, d) = persistable_env(&e, &m, &[]);
        assert_eq!(keys(&k), ["LD_PRELOAD", "MY_VAR", "PATH"]);
        assert_eq!(d, ["DYLD_LIBRARY_PATH", "OTHER"]);
        // ...but never in default/all.
        for m in [Mode::Default, Mode::All] {
            let (k, _) = persistable_env(&e, &m, &[]);
            assert!(!k.contains_key("PATH") && !k.contains_key("LD_PRELOAD"));
            assert!(!k.contains_key("DYLD_LIBRARY_PATH"));
        }
    }

    #[test]
    fn deny_beats_allow_and_list() {
        let e = env(&[
            ("HP_QUEUE", "q"),
            ("HP_TASK_ID", "t"),
            ("HP_FENCING_TOKEN", "f"),
            ("HYPERPANES_PANE_ID", "p"),
            ("HYPERPANES_CONTROL_FILE", "/c"),
            ("HP_WORKTREE_CACHE_DIR", "/w"),
            ("HP_MY_TOKEN", "x"),
            ("MY_API_KEY", "x"),
            ("GH_TOKEN", "x"),
        ]);
        let all_keys = Mode::Keys(e.keys().cloned().collect());
        for m in [Mode::Default, Mode::All, all_keys] {
            let (k, d) = persistable_env(&e, &m, &["MY_*".to_string()]);
            assert!(k.is_empty(), "{m:?}: {k:?}");
            assert_eq!(d.len(), e.len());
        }
    }

    #[test]
    fn secret_key_matching_is_word_bounded() {
        for k in [
            "GH_TOKEN",
            "token",
            "MY_API_KEY",
            "MY_APIKEY_X",
            "AWS_ACCESS_KEY_ID",
            "SSH_AUTH_SOCK",
            "DB_PASSWORD",
            "X_PRIVATE_KEY",
            "SESSION_COOKIE",
            "CREDENTIALS",
        ] {
            assert!(is_secret_key(k), "{k}");
        }
        for k in [
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "AUTHOR",
            "KEYBOARD",
            "MONKEY",
            "LANG",
        ] {
            assert!(!is_secret_key(k), "{k}");
        }
    }

    #[test]
    fn secret_values_dropped_in_all_and_list_mode() {
        let e = env(&[
            ("A", "sk-abc"),
            ("B", "ghp_x"),
            ("C", "gho_x"),
            ("D", "ghs_x"),
            ("E", "github_pat_x"),
            ("F", "xoxb-1"),
            ("G", "xoxp-1"),
            ("H", "AKIA123"),
            ("I", "AIzaXYZ"),
            ("J", "a-----BEGIN KEY"),
            ("OK", "fine"),
        ]);
        let (k, d) = persistable_env(&e, &Mode::All, &[]);
        assert_eq!(keys(&k), ["OK"]);
        assert_eq!(d.len(), 10);
        let m = Mode::Keys(e.keys().cloned().collect());
        let (k, _) = persistable_env(&e, &m, &[]);
        assert_eq!(keys(&k), ["OK"]);
    }

    #[test]
    fn extra_allow_adds_a_prefix() {
        let e = env(&[("MYTOOL_TEAM", "a"), ("OTHER", "b")]);
        let (k, _) = persistable_env(&e, &Mode::Default, &[]);
        assert!(k.is_empty());
        let (k, d) = persistable_env(&e, &Mode::Default, &["MYTOOL_*".to_string()]);
        assert_eq!(keys(&k), ["MYTOOL_TEAM"]);
        assert_eq!(d, ["OTHER"]);
    }

    #[test]
    fn orchestrator_like_env_round_trips() {
        let e = env(&[
            ("HP_GOAL_PERSONA_DIR", "/home/user/p"),
            ("HP_GOAL_ACCOUNTS", "a,b"),
            ("MYTOOL_TEAM", "core"),
            ("CLAUDE_CONFIG_DIR", "/home/user/.claude-a"),
            ("ENABLE_TOOL_SEARCH", "false"),
            ("ANTHROPIC_API_KEY", "x"),
            ("GH_TOKEN", "y"),
            ("PATH", "/usr/bin"),
        ]);
        let (k, d) = persistable_env(&e, &Mode::Default, &["MYTOOL_*".to_string()]);
        assert_eq!(
            keys(&k),
            [
                "CLAUDE_CONFIG_DIR",
                "ENABLE_TOOL_SEARCH",
                "HP_GOAL_ACCOUNTS",
                "HP_GOAL_PERSONA_DIR",
                "MYTOOL_TEAM"
            ]
        );
        assert_eq!(d, ["ANTHROPIC_API_KEY", "GH_TOKEN", "PATH"]);
    }

    #[test]
    fn dropped_meta_is_capped() {
        assert_eq!(dropped_meta_value(&[]), None);
        let names: Vec<String> = (0..25).map(|i| format!("K{i}")).collect();
        let v = dropped_meta_value(&names).unwrap();
        assert!(v.starts_with("K0,K1,"));
        assert!(v.ends_with("K19…(+5)"), "{v}");
        assert_eq!(v.matches(',').count(), 19);
        assert_eq!(dropped_meta_value(&names[..2]).unwrap(), "K0,K1");
    }

    #[test]
    fn restore_merge_order_base_then_overlay_then_config_dir() {
        let base = env(&[("A", "base"), ("B", "base"), ("CLAUDE_CONFIG_DIR", "base")]);
        let overlay: std::collections::HashMap<String, String> = [
            ("B", "overlay"),
            ("C", "overlay"),
            ("CLAUDE_CONFIG_DIR", "overlay"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let m = merge_restore_env(Some(&base), Some(&overlay), Some("meta")).unwrap();
        assert_eq!(m["A"], "base");
        assert_eq!(m["B"], "overlay");
        assert_eq!(m["C"], "overlay");
        assert_eq!(m["CLAUDE_CONFIG_DIR"], "meta");
        let m = merge_restore_env(Some(&base), Some(&overlay), None).unwrap();
        assert_eq!(m["CLAUDE_CONFIG_DIR"], "overlay");
        assert!(merge_restore_env(None, None, None).is_none());
    }
}
