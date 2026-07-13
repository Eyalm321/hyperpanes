//! Auto-register the hyperpanes Claude Code SessionStart/SessionEnd hook in the user's Claude
//! settings, so the pane→conversation marker (`<state>/claude-sessions/<pane-id>.json`, written
//! by `resources/claude/hp-claude-session-hook.sh`) exists reliably — without the user
//! hand-editing `settings.json`. This backs both the claude-resume feature and the goals
//! system's marker-gated delivery.
//!
//! **Additive + idempotent + best-effort.** It merges the hook command into
//! `hooks.SessionStart` / `hooks.SessionEnd`, preserving every other setting and never adding a
//! duplicate; a malformed or unreadable file is left untouched. Any error is returned/logged and
//! never fatal. It only touches config dirs that already exist (it never creates a Claude config).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::persistence::paths::write_atomic;

/// Resolve the bundled `hp-claude-session-hook.sh`, mirroring the packaged layouts
/// [`crate::shell_integration::shell_integration_dir`] handles: next to the exe, the macOS
/// `.app` `Contents/Resources`, and the FHS `share`/`lib` install prefixes. Returns the first
/// that exists.
pub fn bundled_hook_path() -> Option<PathBuf> {
    let rel = Path::new("resources")
        .join("claude")
        .join("hp-claude-session-hook.sh");
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))?;
    let mut candidates = vec![exe_dir.join(&rel)];
    if let Some(prefix) = exe_dir.parent() {
        candidates.push(
            prefix
                .join("Resources")
                .join("claude")
                .join("hp-claude-session-hook.sh"),
        );
        candidates.push(prefix.join("share").join("hyperpanes").join(&rel));
        candidates.push(prefix.join("lib").join("hyperpanes").join(&rel));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// `settings.json` files to register in: every account in the registry
/// ([`crate::claude_accounts::config_dirs`] — the `claude-accounts.json` list, else discovered
/// `~/.claude*` dirs, else `~/.claude`), plus `$CLAUDE_CONFIG_DIR` if set — so the marker keeps
/// working across every account the goals system rotates through. Only existing config dirs are
/// targeted (`ensure_in_file` skips a missing parent).
fn target_settings_files() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |d: PathBuf, dirs: &mut Vec<PathBuf>| {
        if d.is_dir() && !dirs.iter().any(|x| x == &d) {
            dirs.push(d);
        }
    };
    if let Some(cfg) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        push(PathBuf::from(cfg), &mut dirs);
    }
    for dir in crate::claude_accounts::config_dirs() {
        push(dir, &mut dirs);
    }
    dirs.into_iter().map(|d| d.join("settings.json")).collect()
}

/// Register `hook_path` in every target settings file. Returns the number of files newly
/// modified (0 = all already had it / none exist). Best-effort: per-file errors are logged.
pub fn ensure_registered(hook_path: &Path) -> usize {
    let cmd = hook_path.to_string_lossy().to_string();
    let mut changed = 0;
    for file in target_settings_files() {
        match ensure_in_file(&file, &cmd) {
            Ok(true) => {
                eprintln!("[claude-hook] registered SessionStart/End in {}", file.display());
                changed += 1;
            }
            Ok(false) => {}
            Err(e) => eprintln!("[claude-hook] {}: {e}", file.display()),
        }
    }
    changed
}

/// Merge the hook command into one `settings.json`'s SessionStart + SessionEnd. Returns whether
/// the file was written. Pure JSON logic in [`ensure_event`] keeps this testable.
fn ensure_in_file(file: &Path, cmd: &str) -> Result<bool, String> {
    match file.parent() {
        Some(p) if p.is_dir() => {}
        _ => return Ok(false), // no such config dir — skip
    }
    let mut root: Value = match std::fs::read_to_string(file) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| e.to_string())?,
        _ => json!({}),
    };
    if !root.is_object() {
        return Err("settings.json is not a JSON object".into());
    }
    let modified = ["SessionStart", "SessionEnd"]
        .iter()
        .fold(false, |acc, ev| ensure_event(&mut root, ev, cmd) | acc);
    if modified {
        let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        write_atomic(file, pretty.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(modified)
}

/// Ensure `hooks.<event>` contains a matcher group whose `hooks[].command == cmd`. Returns
/// whether it added one (idempotent: a no-op if already present; leaves a malformed shape
/// untouched, returning false).
fn ensure_event(root: &mut Value, event: &str, cmd: &str) -> bool {
    let Some(obj) = root.as_object_mut() else {
        return false;
    };
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return false;
    };
    let arr = hooks_obj.entry(event).or_insert_with(|| json!([]));
    let Some(groups) = arr.as_array_mut() else {
        return false;
    };
    let present = groups.iter().any(|g| {
        g.get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hs| {
                hs.iter()
                    .any(|h| h.get("command").and_then(Value::as_str) == Some(cmd))
            })
    });
    if present {
        return false;
    }
    groups.push(json!({ "hooks": [ { "type": "command", "command": cmd } ] }));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_hook_to_empty_settings() {
        let mut root = json!({});
        assert!(ensure_event(&mut root, "SessionStart", "/hook.sh"));
        let cmd = root["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str();
        assert_eq!(cmd, Some("/hook.sh"));
    }

    #[test]
    fn is_idempotent_and_preserves_other_settings() {
        let mut root = json!({
            "model": "opus",
            "hooks": { "SessionStart": [ { "hooks": [ { "type": "command", "command": "/hook.sh" } ] } ] }
        });
        // Already present → no change.
        assert!(!ensure_event(&mut root, "SessionStart", "/hook.sh"));
        // Unrelated settings survive.
        assert_eq!(root["model"], "opus");
        // A different command appends rather than replacing.
        assert!(ensure_event(&mut root, "SessionStart", "/other.sh"));
        assert_eq!(root["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn leaves_malformed_hooks_untouched() {
        let mut root = json!({ "hooks": { "SessionStart": "not-an-array" } });
        assert!(!ensure_event(&mut root, "SessionStart", "/hook.sh"));
        assert_eq!(root["hooks"]["SessionStart"], "not-an-array");
    }

    #[test]
    fn ensure_in_file_merges_writes_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("hp-claude-hook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        // Pre-existing user settings we must preserve.
        std::fs::write(&file, r#"{"model":"opus","permissions":{"allow":["Bash"]}}"#).unwrap();

        // First call writes both events + keeps the user's settings.
        assert_eq!(ensure_in_file(&file, "/hook.sh"), Ok(true));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["permissions"]["allow"][0], "Bash");
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "/hook.sh"
        );
        assert_eq!(
            v["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "/hook.sh"
        );

        // Second call is a no-op (already registered).
        assert_eq!(ensure_in_file(&file, "/hook.sh"), Ok(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_in_file_skips_a_nonexistent_config_dir() {
        let file = std::env::temp_dir()
            .join("hp-claude-hook-nope-xyz")
            .join("settings.json");
        // Parent dir doesn't exist → skip (never create a Claude config).
        assert_eq!(ensure_in_file(&file, "/hook.sh"), Ok(false));
        assert!(!file.exists());
    }
}
