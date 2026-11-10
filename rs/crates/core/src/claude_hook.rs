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
//!
//! **Only a real install registers** ([`registration_decision`], issue #92). The hook path is
//! written into the *account's* `settings.json`, which outlives the process: a scratch / dev
//! instance that registered its own copy left every later `claude` on that account failing
//! with `SessionStart:startup hook error` once the scratch tree was deleted. So an isolated
//! instance (its own `HYPERPANES_CONTROL_FILE` / XDG dirs) and a cargo dev build never write.
//! They lose nothing: the hook resolves its marker dir from the *pane's* `XDG_STATE_HOME`, so
//! the system-installed hook already serves an isolated instance's panes. Override with
//! `HYPERPANES_CLAUDE_HOOK=1` (always register) or `=0` (never).
//!
//! **Self-healing.** Every register pass also drops entries for a `hp-claude-session-hook.sh`
//! whose absolute path no longer exists (a removed scratch tree, an old AppImage mount).
//! Only entries that are ours by script name are touched; foreign hooks never are.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::persistence::paths::write_atomic;

/// File name of the bundled hook script — how stale entries are recognised as ours.
pub const HOOK_SCRIPT_NAME: &str = "hp-claude-session-hook.sh";

/// Registration override: `1`/`true`/`on`/`install` always registers, `0`/`false`/`off`/`never`
/// never does; unset (or anything else) uses the automatic rule in [`registration_decision`].
pub const OVERRIDE_ENV: &str = "HYPERPANES_CLAUDE_HOOK";

/// Whether this instance may write its hook into the account `settings.json` files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Register,
    /// Skip, with the reason (logged).
    Skip(String),
}

/// One hyperpanes location as this process resolves it, next to its no-override default:
/// `(label, actual, default)`. A mismatch means the instance is (at least partly) isolated.
pub type Location<'a> = (&'a str, &'a Path, &'a Path);

/// Decide whether to register `hook`. Pure over the [`OVERRIDE_ENV`] value and the resolved
/// `locations` (control file, state/data/config dirs vs their defaults — the same baseline the
/// startup isolation check uses), so it is unit-testable. Order:
///   1. [`OVERRIDE_ENV`] forces either way;
///   2. an isolated instance (any location diverted from its default) skips;
///   3. a hook inside a cargo target dir (a dev build) skips;
///   4. otherwise (a packaged install: FHS prefix, AppImage, `.app`, Windows dir) registers.
pub fn registration_decision(
    hook: &Path,
    override_val: Option<&OsStr>,
    locations: &[Location],
) -> Decision {
    if let Some(v) = override_val {
        match v.to_string_lossy().trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "install" => return Decision::Register,
            "0" | "false" | "no" | "off" | "never" => {
                return Decision::Skip(format!("{OVERRIDE_ENV}=0"))
            }
            _ => {}
        }
    }
    if let Some((label, actual, _)) = locations.iter().find(|(_, a, d)| a != d) {
        return Decision::Skip(format!(
            "isolated instance ({label} is {})",
            actual.display()
        ));
    }
    if in_cargo_target_dir(hook) {
        return Decision::Skip(format!(
            "dev build ({} is in a cargo target dir)",
            hook.display()
        ));
    }
    Decision::Register
}

/// Whether `hook` sits in a cargo build tree: cargo tags its target root with `CACHEDIR.TAG`,
/// and the hook would be `<target>[/<triple>]/<profile>/resources/claude/<script>`.
fn in_cargo_target_dir(hook: &Path) -> bool {
    hook.ancestors()
        .skip(1)
        .take(5)
        .any(|d| d.join("CACHEDIR.TAG").is_file())
}

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
    let push = |d: PathBuf, dirs: &mut Vec<PathBuf>| {
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

/// Register `hook_path` in every target settings file — if [`registration_decision`] allows it
/// for this process. Returns the number of files modified (0 = skipped / all already had it /
/// none exist). Best-effort: per-file errors are logged.
pub fn ensure_registered(hook_path: &Path) -> usize {
    use crate::persistence::{isolation, paths};
    let layout = isolation::Layout::resolve(&paths::resolve_control_file());
    let default_control = layout.default_control_file();
    let config_dir = isolation::normalize(&paths::config_dir());
    let default_config_dir = isolation::normalize(&paths::default_config_dir());
    let locations: [Location; 4] = [
        ("control file", &layout.control_file, &default_control),
        ("state dir", &layout.state_dir, &layout.default_state_dir),
        ("data dir", &layout.data_dir, &layout.default_data_dir),
        ("config dir", &config_dir, &default_config_dir),
    ];
    let override_val = std::env::var_os(OVERRIDE_ENV).filter(|v| !v.is_empty());
    match registration_decision(hook_path, override_val.as_deref(), &locations) {
        Decision::Register => register_in_files(&target_settings_files(), hook_path),
        Decision::Skip(why) => {
            eprintln!("[claude-hook] not registering: {why} (set {OVERRIDE_ENV}=1 to force)");
            0
        }
    }
}

/// Register `hook_path` in each of `files` (pruning stale entries of ours on the way).
fn register_in_files(files: &[PathBuf], hook_path: &Path) -> usize {
    let cmd = hook_path.to_string_lossy().to_string();
    let mut changed = 0;
    for file in files {
        match ensure_in_file(file, &cmd) {
            Ok(true) => {
                eprintln!(
                    "[claude-hook] registered SessionStart/End in {}",
                    file.display()
                );
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
        .fold(false, |acc, ev| {
            let pruned = prune_stale(&mut root, ev, cmd);
            ensure_event(&mut root, ev, cmd) | pruned | acc
        });
    if modified {
        let pretty = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
        // Write through a symlinked settings.json (dotfile managers) instead of replacing it.
        let target = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        write_atomic(&target, pretty.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(modified)
}

/// Whether `command` is a registration of ours whose script is gone: an absolute path named
/// [`HOOK_SCRIPT_NAME`] that no longer exists, and not the command being registered now.
fn is_stale_command(command: &str, cmd: &str) -> bool {
    let p = Path::new(command);
    command != cmd
        && p.is_absolute()
        && p.file_name().is_some_and(|n| n == HOOK_SCRIPT_NAME)
        && !p.exists()
}

/// Drop stale entries of ours ([`is_stale_command`]) from `hooks.<event>`, removing a matcher
/// group (and the event key) only when that pruning emptied it. Returns whether it changed
/// anything. Foreign hooks and malformed shapes are left untouched.
fn prune_stale(root: &mut Value, event: &str, cmd: &str) -> bool {
    let Some(groups) = root
        .get_mut("hooks")
        .and_then(|h| h.get_mut(event))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    groups.retain_mut(|g| {
        let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = hs.len();
        hs.retain(|h| {
            !h.get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| is_stale_command(c, cmd))
        });
        if hs.len() == before {
            return true;
        }
        changed = true;
        !hs.is_empty()
    });
    if changed && groups.is_empty() {
        if let Some(h) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            h.remove(event);
        }
    }
    changed
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
        g.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
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
        std::fs::write(
            &file,
            r#"{"model":"opus","permissions":{"allow":["Bash"]}}"#,
        )
        .unwrap();

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

    /// A fresh temp dir per test (removed by the caller).
    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hp-claude-hook-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Lay out `<root>/<rel>/resources/claude/<script>` and return the script path.
    fn install_hook(root: &Path, rel: &str) -> PathBuf {
        let dir = root.join(rel).join("resources").join("claude");
        std::fs::create_dir_all(&dir).unwrap();
        let hook = dir.join(HOOK_SCRIPT_NAME);
        std::fs::write(&hook, "#!/bin/sh\n").unwrap();
        hook
    }

    /// The default layout of a user whose home is `/home/u`.
    const DEFAULTS: [(&str, &str); 4] = [
        (
            "control file",
            "/home/u/.local/state/hyperpanes/control.json",
        ),
        ("state dir", "/home/u/.local/state/hyperpanes"),
        ("data dir", "/home/u/.local/share/hyperpanes"),
        ("config dir", "/home/u/.config/hyperpanes"),
    ];

    /// Decide with `diverted` locations (label → actual path); the rest stay at default.
    fn decide(hook: &Path, override_val: Option<&str>, diverted: &[(&str, &Path)]) -> Decision {
        let actual: Vec<(&str, PathBuf, PathBuf)> = DEFAULTS
            .iter()
            .map(|(l, d)| {
                let a = diverted
                    .iter()
                    .find(|(dl, _)| dl == l)
                    .map(|(_, p)| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from(d));
                (*l, a, PathBuf::from(d))
            })
            .collect();
        let locs: Vec<Location> = actual
            .iter()
            .map(|(l, a, d)| (*l, a.as_path(), d.as_path()))
            .collect();
        registration_decision(hook, override_val.map(OsStr::new), &locs)
    }

    fn commands(v: &Value, event: &str) -> Vec<String> {
        v["hooks"][event]
            .as_array()
            .map(|gs| {
                gs.iter()
                    .flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default())
                    .filter_map(|h| h["command"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn system_install_registers() {
        let root = scratch("system");
        let hook = install_hook(&root, "usr/bin");
        assert_eq!(decide(&hook, None, &[]), Decision::Register);
        // …and it lands in the account settings.
        let cfg = root.join("claude-account");
        std::fs::create_dir_all(&cfg).unwrap();
        let file = cfg.join("settings.json");
        assert_eq!(register_in_files(std::slice::from_ref(&file), &hook), 1);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(commands(&v, "SessionStart"), vec![hook.to_string_lossy()]);
        assert_eq!(commands(&v, "SessionEnd"), vec![hook.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn isolated_instance_skips() {
        let root = scratch("isolated");
        let hook = install_hook(&root, "scratch/usr/bin");
        let iso = root.join("iso");
        for (label, _) in DEFAULTS {
            assert!(
                matches!(decide(&hook, None, &[(label, &iso)]), Decision::Skip(_)),
                "{label}"
            );
        }
        // The full isolation recipe from docs/dev-instance-isolation.md.
        let all: Vec<(&str, &Path)> = DEFAULTS.iter().map(|(l, _)| (*l, iso.as_path())).collect();
        let d = decide(&hook, None, &all);
        assert!(
            matches!(&d, Decision::Skip(why) if why.contains("isolated")),
            "{d:?}"
        );
        // Nothing is written when skipping: ensure_registered short-circuits before any file.
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A desktop session that spells a default dir explicitly — trailing slash, or through a
    /// symlink — is NOT isolated: `ensure_registered` normalizes both sides like the startup
    /// isolation check before comparing.
    #[cfg(unix)]
    #[test]
    fn explicit_default_dirs_are_not_isolation() {
        use crate::persistence::isolation::normalize;
        let root = scratch("explicit");
        let hook = install_hook(&root, "usr/bin");
        let home = root.join("home");
        let default_cfg = home.join(".config").join("hyperpanes");
        std::fs::create_dir_all(&default_cfg).unwrap();
        std::os::unix::fs::symlink(home.join(".config"), root.join("cfg-link")).unwrap();
        let default_n = normalize(&default_cfg);
        for spelled in [
            format!("{}/.config/hyperpanes/", home.display()),
            format!("{}/.config/./hyperpanes", home.display()),
            format!("{}/cfg-link/hyperpanes", root.display()),
        ] {
            let actual = normalize(Path::new(&spelled));
            assert_eq!(actual, default_n, "{spelled}");
            let locs: [Location; 1] = [("config dir", &actual, &default_n)];
            assert_eq!(
                registration_decision(&hook, None, &locs),
                Decision::Register
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cargo_dev_build_skips() {
        let root = scratch("cargo");
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target").join("CACHEDIR.TAG"), "").unwrap();
        let hook = install_hook(&root, "target/debug");
        assert!(matches!(decide(&hook, None, &[]), Decision::Skip(_)));
        let cross = install_hook(&root, "target/x86_64-unknown-linux-gnu/release");
        assert!(matches!(decide(&cross, None, &[]), Decision::Skip(_)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn override_env_forces_either_way() {
        let root = scratch("override");
        let hook = install_hook(&root, "usr/bin");
        let iso = root.join("iso");
        let isolated = [
            ("control file", iso.as_path()),
            ("state dir", iso.as_path()),
        ];
        for on in ["1", "install", "ON"] {
            assert_eq!(
                decide(&hook, Some(on), &isolated),
                Decision::Register,
                "{on}"
            );
        }
        for off in ["0", "off", "never"] {
            assert!(matches!(decide(&hook, Some(off), &[]), Decision::Skip(_)));
        }
        // An unrecognised value falls through to the automatic rule.
        assert_eq!(decide(&hook, Some("maybe"), &[]), Decision::Register);
        assert!(matches!(
            decide(&hook, Some("maybe"), &isolated),
            Decision::Skip(_)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_entries_pruned_live_and_foreign_kept_idempotent() {
        let root = scratch("stale");
        let live = install_hook(&root, "usr/bin");
        let other_live = install_hook(&root, "opt/hyperpanes");
        let gone = root
            .join("deleted-scratch/usr/bin/resources/claude")
            .join(HOOK_SCRIPT_NAME);
        let file = root.join("settings.json");
        let start = json!({
            "model": "opus",
            "hooks": {
                "SessionStart": [
                    { "hooks": [ { "type": "command", "command": gone.to_string_lossy() } ] },
                    { "matcher": "startup", "hooks": [
                        { "type": "command", "command": "/usr/local/bin/foreign.sh" },
                        { "type": "command", "command": gone.to_string_lossy() }
                    ] },
                    { "hooks": [ { "type": "command", "command": other_live.to_string_lossy() } ] },
                    // Relative / bare script names are never ours to prune.
                    { "hooks": [ { "type": "command", "command": HOOK_SCRIPT_NAME } ] }
                ],
                "SessionEnd": [
                    { "hooks": [ { "type": "command", "command": gone.to_string_lossy() } ] }
                ],
                "PreToolUse": [
                    { "hooks": [ { "type": "command", "command": gone.to_string_lossy() } ] }
                ]
            }
        });
        std::fs::write(&file, serde_json::to_string_pretty(&start).unwrap()).unwrap();

        assert_eq!(register_in_files(std::slice::from_ref(&file), &live), 1);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let live_s = live.to_string_lossy().to_string();
        let other_s = other_live.to_string_lossy().to_string();
        assert_eq!(
            commands(&v, "SessionStart"),
            vec![
                "/usr/local/bin/foreign.sh".to_string(),
                other_s,
                HOOK_SCRIPT_NAME.to_string(),
                live_s.clone()
            ]
        );
        // The foreign group keeps its matcher.
        assert_eq!(v["hooks"]["SessionStart"][0]["matcher"], "startup");
        assert_eq!(commands(&v, "SessionEnd"), vec![live_s]);
        // Events we don't manage are never touched, stale or not.
        assert_eq!(v["hooks"]["PreToolUse"], start["hooks"]["PreToolUse"]);
        assert_eq!(v["model"], "opus");

        // Second pass: nothing to do, file byte-identical.
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(register_in_files(std::slice::from_ref(&file), &live), 0);
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_a_symlinked_settings_file() {
        let root = scratch("symlink");
        let hook = install_hook(&root, "usr/bin");
        let real = root.join("dotfiles-settings.json");
        std::fs::write(&real, "{}").unwrap();
        let cfg = root.join("acct");
        std::fs::create_dir_all(&cfg).unwrap();
        let link = cfg.join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(register_in_files(std::slice::from_ref(&link), &hook), 1);
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&real)
            .unwrap()
            .contains(HOOK_SCRIPT_NAME));
        let _ = std::fs::remove_dir_all(&root);
    }
}
