//! Port of `src/main/shell-integration.ts` — the INJECTION side of shell integration:
//! classify the shell, locate the shipped init scripts (hp-init.ps1 / hp-init.sh / cmd
//! PROMPT injection), and build the spawn arguments that turn on OSC-7 / OSC 9;9 cwd
//! reporting. Strictly additive — no-ops (plain shell) when a script is missing or the
//! shell is unknown.
//!
//! NOTE: the cwd PARSER (`parseOscCwd` / `fileUriToPath`) already lives in
//! [`crate::session::cwd`] (owned by core-text). This module is the injection side
//! only — it does NOT re-implement the parser.
//!
//! Owned by track `platform`. Mirrors the non-parser cases of `shell-integration.test.ts`
//! (`classify` + `integrationFor — cmd`), plus added coverage for the pwsh/bash branches
//! that the TS test can't reach without a script on disk.

use std::path::{Path, PathBuf};

/// How a shell executable is classified for integration purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Pwsh,
    Bash,
    Cmd,
    Other,
}

/// Spawn additions for an interactive shell: the extra argv that loads our init script
/// plus any env to merge. (`Record<string,string>` in the TS becomes an ordered list of
/// pairs here — only `cmd` ever sets one.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    /// Extra argv, PREPENDED to the caller's args.
    pub args: Vec<String>,
    /// Env vars to merge into the child environment.
    pub env: Vec<(String, String)>,
}

/// Classify a shell by its executable name/path. PowerShell is checked FIRST because
/// "powershell" also ends in "sh", so a naive POSIX test would misfire.
pub fn classify(shell: &str) -> ShellKind {
    let lower = shell.to_lowercase();
    if lower.contains("pwsh") || lower.contains("powershell") {
        return ShellKind::Pwsh;
    }
    // `/(?:^|[\\/])bash(?:\.exe)?$/` and the cmd equivalent reduce to "the basename is
    // exactly bash / bash.exe (or cmd / cmd.exe)" — i.e. matched at a path boundary and
    // anchored at the end.
    let base = basename(&lower);
    if base == "bash" || base == "bash.exe" {
        return ShellKind::Bash;
    }
    if base == "cmd" || base == "cmd.exe" {
        return ShellKind::Cmd;
    }
    ShellKind::Other
}

// The final path segment, splitting on BOTH separators (a Windows path may use either).
fn basename(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Absolute on-disk directory holding the `hp-init.*` scripts. The *external shell* reads
/// these, so they must be on the real filesystem next to the binary (never inside a
/// bundle/asar). Resolves relative to the running executable, preferring an
/// `extraResources`-style `resources/shell-integration` then a flat `shell-integration`.
///
/// Returns the first existing candidate, else the first candidate as a default — a missing
/// directory simply yields no integration (`integration_for` → `None`), staying additive.
/// The exact packaged layout is finalized at packaging time (Phase 5).
pub fn shell_integration_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let candidates = [
        exe_dir.join("resources").join("shell-integration"),
        exe_dir.join("shell-integration"),
    ];
    for c in &candidates {
        if c.is_dir() {
            return c.clone();
        }
    }
    candidates.into_iter().next().unwrap()
}

/// Spawn additions for an interactive shell, given the shell path and the directory holding
/// the init scripts. Returns `None` (→ plain shell, no integration) for `cmd`'s unknown
/// siblings, `other`, or when the expected script is missing on disk.
pub fn integration_for(shell: &str, dir: &Path) -> Option<Integration> {
    match classify(shell) {
        ShellKind::Pwsh => {
            let script = dir.join("hp-init.ps1");
            if !script.is_file() {
                return None;
            }
            // Dot-source the script (runs in session scope) AFTER the user's $PROFILE.
            // -Command (NOT -File) so the profile loads first; -NoExit keeps the shell
            // interactive; single-quote the path and double-up any embedded quotes.
            let quoted = script.to_string_lossy().replace('\'', "''");
            Some(Integration {
                args: vec![
                    "-NoExit".to_string(),
                    "-Command".to_string(),
                    format!(". '{quoted}'"),
                ],
                env: Vec::new(),
            })
        }
        ShellKind::Bash => {
            let script = dir.join("hp-init.sh");
            if !script.is_file() {
                return None;
            }
            // --rcfile REPLACES ~/.bashrc, so the script sources it back itself. Bash wants
            // forward slashes even on Windows (git-bash).
            let posix = script.to_string_lossy().replace('\\', "/");
            Some(Integration {
                args: vec!["--rcfile".to_string(), posix, "-i".to_string()],
                env: Vec::new(),
            })
        }
        ShellKind::Cmd => {
            // cmd has no init-script hook, but its PROMPT can carry the cwd: $E=ESC,
            // $P=current path, $G='>'. We prefix the OSC 9;9 cwd report (ESC]9;9;<path>ST)
            // then restore the normal "<path>>" prompt. No script/args needed, just the env
            // var. Strictly additive to functionality (the cwd parser reads OSC 9;9 too).
            Some(Integration {
                args: Vec::new(),
                env: vec![("PROMPT".to_string(), "$E]9;9;$P$E\\$P$G".to_string())],
            })
        }
        ShellKind::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- classify (mirrors the TS `classify` describe block) ----

    #[test]
    fn detects_pwsh_first_powershell_also_ends_in_sh() {
        assert_eq!(classify("powershell.exe"), ShellKind::Pwsh);
        assert_eq!(
            classify("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
            ShellKind::Pwsh
        );
        assert_eq!(classify("pwsh"), ShellKind::Pwsh);
    }

    #[test]
    fn detects_bash_incl_git_bash_path() {
        assert_eq!(classify("/bin/bash"), ShellKind::Bash);
        assert_eq!(
            classify("C:\\Program Files\\Git\\bin\\bash.exe"),
            ShellKind::Bash
        );
    }

    #[test]
    fn detects_cmd() {
        assert_eq!(classify("cmd.exe"), ShellKind::Cmd);
        assert_eq!(classify("C:\\Windows\\System32\\cmd.exe"), ShellKind::Cmd);
    }

    #[test]
    fn everything_else_is_other_no_integration() {
        assert_eq!(classify("zsh"), ShellKind::Other);
        assert_eq!(classify("fish"), ShellKind::Other);
        assert_eq!(classify(""), ShellKind::Other);
    }

    // ---- integration_for — cmd (mirrors the TS `integrationFor — cmd` describe block) ----

    #[test]
    fn gives_cmd_a_prompt_that_emits_the_osc_9_9_cwd_with_no_extra_args() {
        let r = integration_for("C:\\Windows\\System32\\cmd.exe", Path::new("/whatever"))
            .expect("cmd integration is never null");
        assert_eq!(r.args, Vec::<String>::new());
        let prompt = r
            .env
            .iter()
            .find(|(k, _)| k == "PROMPT")
            .map(|(_, v)| v.as_str())
            .expect("cmd sets PROMPT");
        assert!(prompt.contains("]9;9;"));
        assert!(prompt.contains("$P"));
    }

    #[test]
    fn still_returns_none_for_an_unknown_other_shell() {
        assert_eq!(integration_for("zsh", Path::new("/x")), None);
    }

    // ---- pwsh / bash branches (not reachable from the TS test — needs a script on disk) ----

    // A unique temp dir per test so parallel runs don't collide.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hp-shellint-{tag}-{:?}", std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pwsh_dot_sources_the_init_script_when_present() {
        let dir = temp_dir("pwsh");
        let script = dir.join("hp-init.ps1");
        fs::write(&script, b"# init").unwrap();
        let r = integration_for("pwsh.exe", &dir).expect("script exists → integration");
        assert_eq!(r.args[0], "-NoExit");
        assert_eq!(r.args[1], "-Command");
        assert!(r.args[2].starts_with(". '"));
        assert!(r.args[2].contains("hp-init.ps1"));
        assert!(r.env.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pwsh_is_null_when_the_script_is_missing() {
        let dir = temp_dir("pwsh-missing"); // empty dir
        assert_eq!(integration_for("pwsh", &dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pwsh_doubles_embedded_single_quotes_in_the_path() {
        let dir = temp_dir("pwsh-quote-o'brien");
        let script = dir.join("hp-init.ps1");
        fs::write(&script, b"# init").unwrap();
        let r = integration_for("pwsh", &dir).expect("script exists");
        // a literal `'` in the path must become `''` inside the single-quoted dot-source.
        assert!(r.args[2].contains("o''brien"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_uses_rcfile_with_forward_slashes_when_present() {
        let dir = temp_dir("bash");
        let script = dir.join("hp-init.sh");
        fs::write(&script, b"# init").unwrap();
        let r = integration_for("C:\\Program Files\\Git\\bin\\bash.exe", &dir)
            .expect("script exists → integration");
        assert_eq!(r.args[0], "--rcfile");
        assert!(!r.args[1].contains('\\'), "bash rcfile path must be posix-slashed");
        assert!(r.args[1].ends_with("hp-init.sh"));
        assert_eq!(r.args[2], "-i");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bash_is_null_when_the_script_is_missing() {
        let dir = temp_dir("bash-missing");
        assert_eq!(integration_for("/bin/bash", &dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
