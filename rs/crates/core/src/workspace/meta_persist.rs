//! Which of a pane's control meta entries the relaunch snapshot may keep, and which a restore
//! may re-apply. Generic: no key names beyond the runtime-owned ones are special-cased.
//!
//! Dropped: runtime-recomputed keys (`hp.recovered`, `hp.env-dropped`), liveness/state keys (last
//! segment, split on `. _ - : /`, ends with `state` or is `status`/`activity`/`liveness`),
//! secret-looking keys or values, empty keys, values over [`MAX_VALUE_BYTES`], and everything
//! past [`MAX_KEYS`] entries.

use std::collections::BTreeMap;

use super::env_persist::{is_secret_value, SECRET_WORDS};

/// Most keys persisted per pane.
pub const MAX_KEYS: usize = 64;
/// Longest value (bytes) persisted.
pub const MAX_VALUE_BYTES: usize = 4096;

const RUNTIME_META: &[&str] = &["hp.recovered", "hp.env-dropped"];

fn is_sep(c: char) -> bool {
    matches!(c, '_' | '.' | '-' | ':' | '/')
}

fn is_state_key(k: &str) -> bool {
    let last = k.rsplit(is_sep).next().unwrap_or(k).to_ascii_lowercase();
    last.ends_with("state") || matches!(last.as_str(), "status" | "activity" | "liveness")
}

fn is_secret_meta_key(k: &str) -> bool {
    let words: Vec<String> = k.split(is_sep).map(|w| w.to_ascii_uppercase()).collect();
    words.iter().enumerate().any(|(i, w)| {
        SECRET_WORDS.contains(&w.as_str())
            || (w == "KEY"
                && i > 0
                && matches!(words[i - 1].as_str(), "API" | "ACCESS" | "PRIVATE"))
            || matches!(w.as_str(), "APIKEY" | "ACCESSKEY" | "PRIVATEKEY")
    })
}

/// Filter `meta` down to the entries safe to write to disk / re-apply. `None` when nothing remains.
pub fn persistable_meta(meta: &BTreeMap<String, String>) -> Option<BTreeMap<String, String>> {
    let out: BTreeMap<String, String> = meta
        .iter()
        .filter(|(k, v)| {
            !k.trim().is_empty()
                && !RUNTIME_META.contains(&k.as_str())
                && !is_state_key(k)
                && !is_secret_meta_key(k)
                && !is_secret_value(v)
                && v.len() <= MAX_VALUE_BYTES
        })
        .take(MAX_KEYS)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &[(&str, &str)]) -> BTreeMap<String, String> {
        p.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn keeps_identity_and_hp_policy_keys() {
        let got = persistable_meta(&m(&[
            ("role", "worker"),
            ("parent", "p1"),
            ("goal", "g"),
            ("task", "t"),
            ("agentType", "claude"),
            ("hp.persist-env", "all"),
            ("hp.nudge", "1"),
            ("hp.supervise", "1"),
            ("hp.backoff", "5"),
            ("custom.identity", "x"),
        ]))
        .unwrap();
        assert_eq!(got.len(), 10);
    }

    #[test]
    fn excludes_runtime_state_and_secrets() {
        let got = persistable_meta(&m(&[
            ("role", "w"),
            ("hp.recovered", "1"),
            ("hp.env-dropped", "A,B"),
            ("agent.runState", "busy"),
            ("x_state", "1"),
            ("status", "ok"),
            ("a/activity", "idle"),
            ("liveness", "up"),
            ("api_token", "t"),
            ("db.password", "p"),
            ("x.API-KEY", "k"),
            ("note", "sk-abc"),
            ("note2", "-----BEGIN KEY"),
            ("", "empty"),
            ("big", &"a".repeat(MAX_VALUE_BYTES + 1)),
        ]))
        .unwrap();
        assert_eq!(got, m(&[("role", "w")]));
    }

    #[test]
    fn none_when_empty_and_capped() {
        assert!(persistable_meta(&m(&[("hp.recovered", "1")])).is_none());
        let many: BTreeMap<String, String> =
            (0..100).map(|i| (format!("k{i:03}"), "v".into())).collect();
        assert_eq!(persistable_meta(&many).unwrap().len(), MAX_KEYS);
    }
}
