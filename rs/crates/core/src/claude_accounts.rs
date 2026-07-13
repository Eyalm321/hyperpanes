//! The Claude-account registry for the goals system's account rotation. Defines *which*
//! `CLAUDE_CONFIG_DIR`s exist so agents can be spread across (and rotated between) multiple
//! Claude accounts — so a weekly/session limit on one account doesn't stall a project. Because
//! the `claude` CLI stores transcripts under `CLAUDE_CONFIG_DIR`, cross-account `--resume` only
//! works when the accounts share a transcript store (see `scripts/setup-claude-accounts.sh`);
//! this module just enumerates the account dirs.
//!
//! Source of truth, in order: the `claude-accounts.json` registry file, else discovery of
//! `~/.claude*` dirs that hold a `.credentials.json`, else a single default (`~/.claude`). The
//! app assigns one account per orchestrator pane (round-robin) and hands the full list down to
//! the persona (env `HP_GOAL_ACCOUNTS`) so it can spread + rotate its spec/impl agents.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// One Claude account = a name + its `CLAUDE_CONFIG_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub config_dir: PathBuf,
}

#[derive(Deserialize)]
struct AccountsFile {
    accounts: Vec<AccountEntry>,
}

#[derive(Deserialize)]
struct AccountEntry {
    #[serde(default)]
    name: String,
    #[serde(rename = "configDir")]
    config_dir: String,
}

/// Load the account list: registry file → discovery → default. Never empty (falls back to a
/// single `~/.claude`). Deduped by resolved `config_dir`, order preserved.
pub fn load() -> Vec<Account> {
    let home = home_dir();
    if let Ok(text) = std::fs::read_to_string(crate::persistence::paths::claude_accounts_json()) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            let accounts = accounts_from_json(&v, home.as_deref());
            if !accounts.is_empty() {
                return dedup(accounts);
            }
        }
    }
    let discovered = discover(home.as_deref());
    if !discovered.is_empty() {
        return dedup(discovered);
    }
    // Last resort: the default single account (may not exist yet).
    let dir = home
        .map(|h| h.join(".claude"))
        .unwrap_or_else(|| PathBuf::from(".claude"));
    vec![Account {
        name: "default".to_string(),
        config_dir: dir,
    }]
}

/// Just the config dirs (the shape the hook-registration + env-feed want).
pub fn config_dirs() -> Vec<PathBuf> {
    load().into_iter().map(|a| a.config_dir).collect()
}

/// Parse the registry JSON into accounts, expanding a leading `~` against `home`. Pure (no fs),
/// so it's unit-testable. An entry with an empty `configDir` is skipped; an empty `name`
/// defaults to the dir's basename.
fn accounts_from_json(v: &Value, home: Option<&Path>) -> Vec<Account> {
    let Ok(file) = serde_json::from_value::<AccountsFile>(v.clone()) else {
        return Vec::new();
    };
    file.accounts
        .into_iter()
        .filter_map(|e| {
            if e.config_dir.trim().is_empty() {
                return None;
            }
            let config_dir = expand_tilde(&e.config_dir, home);
            let name = if e.name.trim().is_empty() {
                config_dir
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.config_dir.clone())
            } else {
                e.name
            };
            Some(Account { name, config_dir })
        })
        .collect()
}

/// Discover `~/.claude*` sibling dirs that hold a `.credentials.json` (a logged-in account).
/// `~/.claude` sorts first (the primary), the rest alphabetically for a stable order.
fn discover(home: Option<&Path>) -> Vec<Account> {
    let Some(home) = home else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut found: Vec<Account> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.starts_with(".claude") || !path.is_dir() {
                return None;
            }
            if !path.join(".credentials.json").is_file() {
                return None;
            }
            Some(Account {
                name: name.trim_start_matches('.').to_string(),
                config_dir: path,
            })
        })
        .collect();
    found.sort_by(|a, b| {
        let key = |c: &PathBuf| (c.file_name().map(|f| f.to_os_string()) != Some(".claude".into()), c.clone());
        key(&a.config_dir).cmp(&key(&b.config_dir))
    });
    found
}

fn expand_tilde(s: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = home {
            return home.to_path_buf();
        }
    }
    PathBuf::from(s)
}

fn dedup(accounts: Vec<Account>) -> Vec<Account> {
    let mut seen = std::collections::HashSet::new();
    accounts
        .into_iter()
        .filter(|a| seen.insert(a.config_dir.clone()))
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_registry_and_expands_tilde() {
        let home = PathBuf::from("/home/me");
        let v = json!({ "accounts": [
            { "name": "primary", "configDir": "~/.claude" },
            { "configDir": "~/.claude-alt" },            // name defaults to basename
            { "name": "abs", "configDir": "/opt/claude" },
            { "name": "skip", "configDir": "" }           // empty dir → skipped
        ]});
        let a = accounts_from_json(&v, Some(&home));
        assert_eq!(a.len(), 3);
        assert_eq!(a[0], Account { name: "primary".into(), config_dir: PathBuf::from("/home/me/.claude") });
        assert_eq!(a[1].name, ".claude-alt"); // basename of the expanded path
        assert_eq!(a[1].config_dir, PathBuf::from("/home/me/.claude-alt"));
        assert_eq!(a[2].config_dir, PathBuf::from("/opt/claude"));
    }

    #[test]
    fn dedups_by_config_dir_preserving_order() {
        let a = dedup(vec![
            Account { name: "a".into(), config_dir: "/x".into() },
            Account { name: "b".into(), config_dir: "/y".into() },
            Account { name: "c".into(), config_dir: "/x".into() },
        ]);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].name, "a");
        assert_eq!(a[1].name, "b");
    }

    #[test]
    fn empty_or_bad_json_yields_no_accounts() {
        assert!(accounts_from_json(&json!({}), None).is_empty());
        assert!(accounts_from_json(&json!({ "accounts": "nope" }), None).is_empty());
    }
}
