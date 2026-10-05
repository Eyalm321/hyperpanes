//! Port of the spawn-resolution helpers in `src/main/session.ts`:
//! `resolveSpawn` / `buildArgs` / `resolveWindowsCommand` / `defaultShell`, including
//! the PATHEXT/PATH search for a direct, no-shell `args[]` spawn (P4a) and the
//! scoped-control-token env suppression (a scoped child must NOT see
//! `HYPERPANES_CONTROL_FILE`). Pure + unit-testable.
//!
//! The fs- and platform-touching entry points (`resolve_windows_command`,
//! `resolve_spawn`, `default_shell`) are thin wrappers over `*_with` cores that take
//! an injected `is_file` predicate and an explicit `windows` flag — exactly so the
//! TS test suite (which mocks `fs.statSync` and assumes win32) can be mirrored 1:1
//! without spawning anything or depending on the host platform.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

/// An environment map (case-sensitive keys, like Node's `process.env` /
/// `Record<string,string>`). Case-insensitive lookup is done by [`get_env_var`].
pub type EnvMap = HashMap<String, String>;

/// The resolved pty spawn target: the executable `file` and its verbatim `args`.
/// Mirrors the `{ file, args }` returned by TS `resolveSpawn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    pub file: String,
    pub args: Vec<String>,
}

// Default Windows PATHEXT, mirroring the TS fallback when the var is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";

/// Case-insensitive lookup of `name` first in the optional `env` override, then in
/// the process environment — the port of TS `getEnvVar`.
pub fn get_env_var(name: &str, env: Option<&EnvMap>) -> Option<String> {
    let target = name.to_ascii_uppercase();
    if let Some(env) = env {
        for (k, v) in env {
            if k.to_ascii_uppercase() == target {
                return Some(v.clone());
            }
        }
    }
    for (k, v) in std::env::vars() {
        if k.to_ascii_uppercase() == target {
            return Some(v);
        }
    }
    None
}

/// Build argv. When a `command` is supplied it's wrapped for the SHELL so the real
/// exit code flows back via `onExit` (powers pane status + restart). The invocation
/// flag is keyed off the shell, not the platform, so a custom shell (pwsh, or
/// git-bash on Windows) is launched with the right switch. Port of TS `buildArgs`.
pub fn build_args(shell: &str, command: Option<&str>, base_args: Option<&[String]>) -> Vec<String> {
    let command = match command {
        Some(c) => c,
        None => return base_args.map(|a| a.to_vec()).unwrap_or_default(),
    };
    let lower = shell.to_ascii_lowercase();
    // Check PowerShell first — 'powershell' also ends in 'sh', so the POSIX test
    // below would otherwise misfire on it.
    if lower.contains("powershell") || lower.contains("pwsh") {
        return vec!["-NoLogo".into(), "-Command".into(), command.to_string()];
    }
    // POSIX-family shells use `-c` on every platform (covers git-bash on Windows).
    if is_posix_shell(&lower) {
        return vec!["-c".into(), command.to_string()];
    }
    if cfg!(windows) {
        return vec!["/c".into(), command.to_string()]; // cmd.exe
    }
    vec!["-c".into(), command.to_string()]
}

// Mirror of the TS regex `/(?:^|[\\/])(?:bash|zsh|fish|sh|dash|ash)(?:\.exe)?$/`:
// a POSIX shell basename, optionally `.exe`, at the end of the (lowercased) path.
fn is_posix_shell(lower: &str) -> bool {
    let stem = lower.strip_suffix(".exe").unwrap_or(lower);
    // basename after the last path separator
    let base = stem.rsplit(['\\', '/']).next().unwrap_or(stem);
    matches!(base, "bash" | "zsh" | "fish" | "sh" | "dash" | "ash")
}

fn real_is_file(p: &str) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Resolve a Windows command name to a concrete executable path by searching cwd
/// then PATH and applying PATHEXT — port of TS `resolveWindowsCommand`. Uses the
/// real filesystem; see [`resolve_windows_command_with`] for the injectable core.
pub fn resolve_windows_command(command: &str, cwd: Option<&str>, env: Option<&EnvMap>) -> String {
    resolve_windows_command_with(command, cwd, env, &real_is_file)
}

/// Injectable core of [`resolve_windows_command`]: `is_file` decides which candidate
/// paths "exist" (the unit tests substitute a closure for `fs.statSync`).
pub fn resolve_windows_command_with(
    command: &str,
    cwd: Option<&str>,
    env: Option<&EnvMap>,
    is_file: &dyn Fn(&str) -> bool,
) -> String {
    if command.is_empty() {
        return command.to_string();
    }

    let pathext_val = get_env_var("PATHEXT", env).unwrap_or_else(|| DEFAULT_PATHEXT.to_string());
    let pathexts: Vec<String> = pathext_val
        .split(';')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .map(|e| {
            if e.starts_with('.') {
                e
            } else {
                format!(".{e}")
            }
        })
        .collect();

    let find_executable = |base_path: &str| -> Option<String> {
        if is_file(base_path) {
            return Some(base_path.to_string());
        }
        for ext in &pathexts {
            let candidate = format!("{base_path}{ext}");
            if is_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    };

    let cwd_or = || cwd.map(|c| c.to_string()).unwrap_or_else(process_cwd);

    // An explicit path (contains a separator): resolve it against cwd and probe.
    if command.contains('/') || command.contains('\\') {
        let resolved = win_resolve(&cwd_or(), command);
        return find_executable(&resolved).unwrap_or_else(|| command.to_string());
    }

    // A bare name: search cwd first, then each PATH dir.
    let mut search_dirs: Vec<String> = vec![cwd_or()];
    if let Some(path_val) = get_env_var("PATH", env) {
        search_dirs.extend(
            path_val
                .split(';')
                .map(|d| d.trim())
                .filter(|d| !d.is_empty())
                .map(|d| d.to_string()),
        );
    }

    for dir in search_dirs {
        let resolved_base = win_resolve(&dir, command);
        if let Some(found) = find_executable(&resolved_base) {
            return found;
        }
    }

    command.to_string()
}

// The process cwd as a string (best-effort). Mirrors Node `process.cwd()`.
fn process_cwd() -> String {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// A minimal Windows `path.resolve(base, p)`: backslash-join an absolute or
// base-relative path and normalize `.`/`..`/duplicate-separator segments. Only the
// Windows shapes the resolver needs (drive-qualified absolutes and cwd-relative
// names) are handled — this is a win32-only code path.
fn win_resolve(base: &str, p: &str) -> String {
    let pb = p.as_bytes();
    let is_abs = (pb.len() >= 2 && pb[0].is_ascii_alphabetic() && pb[1] == b':')
        || p.starts_with('\\')
        || p.starts_with('/');
    let combined = if is_abs {
        p.to_string()
    } else {
        format!("{}\\{}", base.trim_end_matches(['\\', '/']), p)
    };
    normalize_win(&combined)
}

fn normalize_win(path: &str) -> String {
    let bytes = path.as_bytes();
    let (drive, rest) = if bytes.len() >= 2 && bytes[1] == b':' {
        (&path[..2], &path[2..])
    } else {
        ("", path)
    };
    let mut stack: Vec<&str> = Vec::new();
    for seg in rest.split(['\\', '/']) {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    if stack.is_empty() {
        format!("{drive}\\")
    } else {
        format!("{drive}\\{}", stack.join("\\"))
    }
}

/// Byte spans of the whitespace-separated tokens in `s`, in order. Char-boundary safe
/// (`char::is_whitespace`), so every span can be used to slice `s` verbatim.
fn token_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if let Some(st) = start.take() {
                spans.push((st, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        spans.push((st, s.len()));
    }
    spans
}

/// Shell punctuation that attaches to a token without belonging to it: the quote of an enclosing
/// `sh -c '…'` string and the separators that end a command. Strips a leading quote run, then a
/// trailing separator run, then a trailing quote run, looping until stable (bounded — every pass
/// removes at least one byte) so `claude';` and `"claude"` both reduce to `claude`.
fn unquote_token(token: &str) -> &str {
    let mut s = token;
    for _ in 0..4 {
        let before = s;
        s = s.trim_start_matches(['\'', '"']);
        s = s.trim_end_matches([';', '&', '|']);
        s = s.trim_end_matches(['\'', '"']);
        if s == before {
            break;
        }
    }
    s
}

/// Is this token a `claude` launch target: exactly `claude`, or a path whose basename is
/// `claude` (`/usr/local/bin/claude`, `C:\bin\claude`)? Anything else is left untouched.
///
/// Quote-aware: a launch nested in a shell string reads `'claude` inside `sh -c 'claude …'`, and
/// `"claude"` when doubly quoted, so the enclosing punctuation is stripped FOR THE TEST only —
/// this predicate never rewrites a byte.
fn is_claude_command(token: &str) -> bool {
    let token = unquote_token(token);
    token == "claude" || token.rsplit(['\\', '/']).next() == Some("claude")
}

/// Byte offset of the first non-whitespace byte at or after `from` (char-boundary safe), or
/// `command.len()` when only whitespace follows.
fn skip_whitespace(command: &str, from: usize) -> usize {
    let Some(tail) = command.get(from..) else {
        return command.len();
    };
    match tail.find(|c: char| !c.is_whitespace()) {
        Some(i) => from + i,
        None => command.len(),
    }
}

/// Byte offset just past the `--mcp-config` value that begins at or after `from`, respecting
/// shell quoting: a `'`-opened value runs to the next `'`, a `"`-opened value to the next
/// unescaped `"` (honouring `\`), anything else to the next whitespace or the string's end.
/// `None` when there is no value — only whitespace follows, or a bare command separator does
/// (`--mcp-config; sleep 1` has nothing to attach to). Char-boundary safe: every offset it
/// compares or returns is an ASCII quote/whitespace boundary, never inside a multi-byte char.
fn mcp_config_value_end(command: &str, from: usize) -> Option<usize> {
    let start = skip_whitespace(command, from);
    let bytes = command.as_bytes();
    match bytes.get(start) {
        None => return None,
        // A separator is not a value: the flag would be attached to nothing.
        Some(b';') | Some(b'&') | Some(b'|') => return None,
        _ => {}
    }
    let quote = match bytes[start] {
        b'\'' => Some(b'\''),
        b'"' => Some(b'"'),
        _ => None,
    };
    if let Some(quote) = quote {
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                // Inside double quotes a backslash escapes the next char (possibly multi-byte;
                // a continuation byte never equals an ASCII quote, so skipping two is safe).
                b'\\' if quote == b'"' => i += 2,
                b if b == quote => return Some(i + 1),
                _ => i += 1,
            }
        }
        // Unterminated quote: fall through to the whitespace scan (best effort).
    }
    match command[start..].find(char::is_whitespace) {
        Some(i) => Some(start + i),
        None => Some(command.len()),
    }
}

/// Roll an insertion offset back over a trailing run of quotes and command separators, so the
/// flag lands INSIDE the quoting the value belongs to: `--mcp-config /x/m.json'` (that `'` closes
/// an enclosing `sh -c '…'` string) takes the flag before the quote, `--mcp-config /x/m.json;`
/// before the `;`. Every byte rolled over is ASCII, so the result stays on a char boundary.
fn trim_insert_point(command: &str, end: usize) -> usize {
    let bytes = command.as_bytes();
    let mut i = end;
    while i > 0 && matches!(bytes[i - 1], b'\'' | b'"' | b';' | b'&' | b'|') {
        i -= 1;
    }
    i
}

/// The `--mcp-config` prefix, both spellings (`--mcp-config <path>` / `--mcp-config=<path>`).
const MCP_CONFIG_ARG: &str = "--mcp-config";
const MCP_CONFIG_EQ: &str = "--mcp-config=";
const STRICT_MCP_CONFIG_ARG: &str = "--strict-mcp-config";

/// Insert `--strict-mcp-config` into a pane command that launches `claude` with an explicit
/// `--mcp-config`, unless it already carries the flag.
///
/// Why this is structural rather than prose: `claude --mcp-config <file>` WITHOUT
/// `--strict-mcp-config` does not restrict claude to that file, it MERGES the config with every
/// server the account dir (`CLAUDE_CONFIG_DIR`, rotated per goal agent) registers. The
/// code-index servers in there (tokensave, serena) then index the repo once per agent — 6-10GB
/// each against a ~300MB normal footprint — which is how a goal org drove the machine into an
/// OOM kill loop. Agents spawn agents here (`open_pane`, `spawn_workers`), so the guard has to
/// live at the pane-spawn choke point, not in a persona that an LLM may or may not follow.
///
/// Rules (byte-preserving: every other byte of `command`, including quoting, spacing and the
/// prompt text, comes back verbatim):
///   1. no `--mcp-config` / `--mcp-config=` token → unchanged;
///   2. a `--strict-mcp-config` token already present → unchanged (idempotent);
///   3. no `claude` token (exact, or basename `claude`, both quote-stripped — `sh -c 'claude …'`
///      and `"claude"` count) → unchanged (never touch other tools);
///   4. else insert ` --strict-mcp-config` immediately after the value of the FIRST `--mcp-config`
///      occurrence, resolved quote-aware (`'…'` / `"…"` values keep their closing quote before the
///      flag; a trailing quote or `;` that belongs to an enclosing construct stays after it);
///   5. a `--mcp-config` with no value (end of string, a bare separator, or an empty
///      `--mcp-config=`) → unchanged, rather than emitting a dangling flag;
///   6. `Cow::Borrowed` whenever nothing is edited.
///
/// The nested worker-runner shape is covered: the `claude … --mcp-config <f>` inside
/// `hyperpanes worker … --command "sh -c '…'"` is still a token sequence (`'claude` reduces to
/// `claude`), and the insertion lands on a token boundary so the shell string stays valid.
pub fn ensure_strict_mcp_config(command: &str) -> Cow<'_, str> {
    let spans = token_spans(command);
    let mut has_claude = false;
    let mut has_strict = false;
    let mut has_mcp = false;
    // Byte offset to insert at: just past the value of the FIRST `--mcp-config` occurrence.
    let mut insert_at: Option<usize> = None;
    for &(start, end) in &spans {
        let token = &command[start..end];
        if is_claude_command(token) {
            has_claude = true;
        } else if token == STRICT_MCP_CONFIG_ARG {
            has_strict = true;
        } else if !has_mcp && (token == MCP_CONFIG_ARG || token.starts_with(MCP_CONFIG_EQ)) {
            has_mcp = true;
            // The first occurrence decides. Spaced form: the value starts after this token.
            // Equals form: it starts right after the `=` — an empty `--mcp-config=` has none.
            let value_start = if token == MCP_CONFIG_ARG {
                Some(end)
            } else if token.len() > MCP_CONFIG_EQ.len() {
                Some(start + MCP_CONFIG_EQ.len())
            } else {
                None
            };
            if let Some(value_start) = value_start {
                if let Some(value_end) = mcp_config_value_end(command, value_start) {
                    // The spaced form's value must be a FOLLOWING token; the equals form carries
                    // its value inside the token, where `value_end == end` is the normal case.
                    let value_ok = if token == MCP_CONFIG_ARG {
                        value_end > end
                    } else {
                        value_end >= end
                    };
                    if value_ok {
                        let value_at = skip_whitespace(command, value_start);
                        let quoted =
                            matches!(command.as_bytes().get(value_at), Some(b'\'') | Some(b'"'));
                        insert_at = Some(if quoted {
                            // A quoted value owns its closing quote: the flag goes after it.
                            value_end
                        } else {
                            // A trailing quote/separator here closes an enclosing construct, so
                            // the flag goes inside it, before that punctuation.
                            trim_insert_point(command, value_end)
                        });
                    }
                }
            }
        }
    }
    let Some(insert_at) = insert_at else {
        return Cow::Borrowed(command);
    };
    if has_strict || !has_claude {
        return Cow::Borrowed(command);
    }
    let mut out = String::with_capacity(command.len() + STRICT_MCP_CONFIG_ARG.len() + 1);
    out.push_str(&command[..insert_at]);
    out.push(' ');
    out.push_str(STRICT_MCP_CONFIG_ARG);
    out.push_str(&command[insert_at..]);
    Cow::Owned(out)
}

/// [`ensure_strict_mcp_config`] for an already-split argv (the direct, no-shell spawn shape):
/// same rule, no re-parse — the flag goes right after the `--mcp-config` value element. A no-op
/// when the argv already carries the strict flag, has no `--mcp-config`, or leaves it dangling.
/// The caller decides whether this argv is a `claude` launch.
fn ensure_strict_mcp_config_in_args(args: &mut Vec<String>) {
    if args.iter().any(|a| a == STRICT_MCP_CONFIG_ARG) {
        return;
    }
    let Some(i) = args
        .iter()
        .position(|a| a == MCP_CONFIG_ARG || a.starts_with(MCP_CONFIG_EQ))
    else {
        return;
    };
    let at = if args[i] == MCP_CONFIG_ARG {
        // Spaced form: the next element is the value; nothing after it to insert behind.
        if i + 1 < args.len() {
            i + 2
        } else {
            return;
        }
    } else if args[i].len() > MCP_CONFIG_EQ.len() {
        i + 1
    } else {
        return;
    };
    args.insert(at, STRICT_MCP_CONFIG_ARG.to_string());
}

/// Resolve the actual pty spawn target from a pane's shell/command/args
/// (interactive-pane-driving plan P4a). Three shapes:
///   * `command` + non-empty `args` → spawn `command` DIRECTLY with `args` as its
///     verbatim argv: NO shell, NO re-parse (the P4a fix for args with spaces/quotes).
///   * `command` alone → run it through the shell (`shell -c "command"` etc.).
///   * no `command` → an interactive shell, with any `args` handed to it verbatim.
///
/// Port of TS `resolveSpawn`. Uses the real fs/platform; see [`resolve_spawn_with`].
pub fn resolve_spawn(
    shell: &str,
    command: Option<&str>,
    args: Option<&[String]>,
    cwd: Option<&str>,
    env: Option<&EnvMap>,
) -> Spawn {
    resolve_spawn_with(shell, command, args, cwd, env, cfg!(windows), &real_is_file)
}

/// Injectable core of [`resolve_spawn`] — `windows` selects the direct-spawn path
/// resolution and `is_file` stands in for `fs.statSync` (so the P4a tests are
/// deterministic on any host).
pub fn resolve_spawn_with(
    shell: &str,
    command: Option<&str>,
    args: Option<&[String]>,
    cwd: Option<&str>,
    env: Option<&EnvMap>,
    windows: bool,
    is_file: &dyn Fn(&str) -> bool,
) -> Spawn {
    if let (Some(command), Some(args)) = (command, args) {
        if !args.is_empty() {
            let file = if windows {
                resolve_windows_command_with(command, cwd, env, is_file)
            } else {
                command.to_string()
            };
            // Structural MCP guard (see [`ensure_strict_mcp_config`]): a direct `claude` argv
            // carrying an explicit `--mcp-config` must also carry `--strict-mcp-config`, whatever
            // an upstream agent did or forgot. Every other direct spawn is untouched.
            let mut argv = args.to_vec();
            if is_claude_command(command) {
                ensure_strict_mcp_config_in_args(&mut argv);
            }
            return Spawn { file, args: argv };
        }
    }
    // The shell-wrapped shape (and, for free, an agent-built string that nests a claude launch
    // inside a worker-runner command): same guard on the command string itself.
    let guarded = command.map(ensure_strict_mcp_config);
    Spawn {
        file: shell.to_string(),
        args: build_args(shell, guarded.as_deref(), args),
    }
}

/// The default interactive shell for this platform. On Windows: prefer PowerShell 7
/// (`pwsh.exe`) when installed — only pwsh gets our PSReadLine history prediction and
/// OSC-7 cwd reporting — else `COMSPEC` (cmd), else Windows PowerShell. Resolved once
/// and cached. Elsewhere: `$SHELL` or `/bin/bash`. Port of TS `defaultShell`.
pub fn default_shell() -> String {
    if cfg!(windows) {
        static WIN_DEFAULT: OnceLock<String> = OnceLock::new();
        return WIN_DEFAULT
            .get_or_init(|| {
                let pwsh = resolve_windows_command("pwsh.exe", None, None);
                if real_is_file(&pwsh) {
                    "pwsh.exe".to_string()
                } else {
                    std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
                }
            })
            .clone();
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

/// Inputs to [`build_env`], mirroring the env block assembled in the TS `Session`
/// constructor. All borrowed; `build_env` owns the resulting map.
pub struct EnvInputs<'a> {
    /// The inherited process environment (TS `process.env`).
    pub process_env: &'a EnvMap,
    /// Per-pane env override (TS `opts.env`).
    pub opts_env: Option<&'a EnvMap>,
    /// Shell-integration env (TS `integration.env`); empty when not integrated.
    pub integration_env: &'a EnvMap,
    /// Owning pane id → injected as `HYPERPANES_PANE_ID` (agent-orchestration A).
    pub pane_id: Option<&'a str>,
    /// Path to `control.json` — set as `HYPERPANES_CONTROL_FILE` UNLESS a scoped
    /// `HYPERPANES_CONTROL_TOKEN` was injected (scoped child must not read master).
    /// `None` (or empty) omits the var entirely rather than injecting an empty string
    /// — see [`resolve_control_file`], which callers should use to produce this.
    pub control_file: Option<&'a str>,
}

/// Resolve the `HYPERPANES_CONTROL_FILE` value for a spawned child: `explicit` (e.g.
/// `SpawnOptions.control_file`) wins if non-empty; otherwise fall back to this
/// process's own `HYPERPANES_CONTROL_FILE` env var if non-empty; otherwise the
/// default `control.json` path — the same discovery default every consumer
/// (worker/pair/MCP) assumes, and absent-while-control-is-off by contract — so
/// GUI-native panes are self-describing too. Never an empty string a child might
/// mistake for "unset but present" (see `hyperpanes pair`'s workaround for that
/// symptom).
pub fn resolve_control_file(explicit: Option<&str>) -> Option<String> {
    resolve_control_file_with(explicit, |name| std::env::var(name).ok()).or_else(|| {
        Some(
            crate::persistence::paths::control_json()
                .to_string_lossy()
                .into_owned(),
        )
    })
}

fn resolve_control_file_with(
    explicit: Option<&str>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if let Some(v) = explicit {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    env_lookup("HYPERPANES_CONTROL_FILE").filter(|v| !v.is_empty())
}

/// Assemble the child's environment exactly as the TS `Session` constructor does:
/// merge `process.env` ◁ `opts.env` ◁ `integrationEnv`, force `TERM`/`COLORTERM`,
/// drop Electron's leaked `GOOGLE_API_KEY`, inject `HYPERPANES_PANE_ID`, and point at
/// the control discovery file ONLY when no scoped token is present.
pub fn build_env(inputs: &EnvInputs<'_>) -> EnvMap {
    let mut env: EnvMap = inputs.process_env.clone();
    if let Some(o) = inputs.opts_env {
        for (k, v) in o {
            env.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in inputs.integration_env {
        env.insert(k.clone(), v.clone());
    }
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());

    // Electron injects a default GOOGLE_API_KEY; don't leak it to the shell.
    if let (Some(cur), Some(base)) = (
        env.get("GOOGLE_API_KEY").cloned(),
        inputs.process_env.get("GOOGLE_API_KEY"),
    ) {
        if &cur == base {
            env.remove("GOOGLE_API_KEY");
        }
    }

    if let Some(pane_id) = inputs.pane_id {
        env.insert("HYPERPANES_PANE_ID".into(), pane_id.to_string());
    }

    // A pane handed a SCOPED control token via env (capability scoping, leg F) must
    // NOT also be able to read the master token from control.json — so only point at
    // the discovery file when no scoped token was injected, and only when a
    // (non-empty) path is actually known.
    if !env.contains_key("HYPERPANES_CONTROL_TOKEN") {
        if let Some(control_file) = inputs.control_file.filter(|v| !v.is_empty()) {
            env.insert("HYPERPANES_CONTROL_FILE".into(), control_file.to_string());
        }
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ---- buildArgs ----

    #[test]
    fn wraps_a_command_for_powershell_pwsh() {
        assert_eq!(
            build_args("powershell.exe", Some("npm run dev"), None),
            argv(&["-NoLogo", "-Command", "npm run dev"])
        );
        assert_eq!(
            build_args("pwsh", Some("echo hi"), None),
            argv(&["-NoLogo", "-Command", "echo hi"])
        );
    }

    #[test]
    fn wraps_a_command_for_posix_shells_with_dash_c() {
        assert_eq!(
            build_args("/bin/bash", Some("ls -la"), None),
            argv(&["-c", "ls -la"])
        );
        assert_eq!(build_args("zsh", Some("ls"), None), argv(&["-c", "ls"]));
        assert_eq!(
            build_args("C:\\Program Files\\Git\\bin\\bash.exe", Some("ls"), None),
            argv(&["-c", "ls"])
        );
    }

    #[test]
    fn returns_bare_base_args_for_an_interactive_shell() {
        assert_eq!(build_args("pwsh", None, None), Vec::<String>::new());
        assert_eq!(
            build_args("bash", None, Some(&argv(&["-l"]))),
            argv(&["-l"])
        );
    }

    // ---- resolveSpawn (P4a) ---- (is_file always-false mirrors statSync throwing)

    fn no_files(_: &str) -> bool {
        false
    }

    #[test]
    fn command_plus_args_spawns_directly_verbatim_argv() {
        let args = argv(&["--append-system-prompt", "be a pirate, matey"]);
        assert_eq!(
            resolve_spawn_with(
                "powershell.exe",
                Some("claude"),
                Some(&args),
                None,
                None,
                true,
                &no_files
            ),
            Spawn {
                file: "claude".into(),
                args
            }
        );
    }

    #[test]
    fn preserves_an_arg_with_spaces_and_quotes_as_one_element() {
        let args = argv(&["--msg", "hello \"world\" of panes"]);
        assert_eq!(
            resolve_spawn_with(
                "cmd.exe",
                Some("mytool"),
                Some(&args),
                None,
                None,
                true,
                &no_files
            ),
            Spawn {
                file: "mytool".into(),
                args
            }
        );
    }

    #[test]
    fn command_no_args_runs_through_the_shell() {
        assert_eq!(
            resolve_spawn_with(
                "pwsh",
                Some("npm run dev"),
                None,
                None,
                None,
                true,
                &no_files
            ),
            Spawn {
                file: "pwsh".into(),
                args: argv(&["-NoLogo", "-Command", "npm run dev"])
            }
        );
        assert_eq!(
            resolve_spawn_with(
                "/bin/bash",
                Some("ls -la"),
                None,
                None,
                None,
                false,
                &no_files
            ),
            Spawn {
                file: "/bin/bash".into(),
                args: argv(&["-c", "ls -la"])
            }
        );
    }

    #[test]
    fn empty_args_array_is_treated_as_no_args() {
        let empty: Vec<String> = vec![];
        assert_eq!(
            resolve_spawn_with(
                "pwsh",
                Some("top"),
                Some(&empty),
                None,
                None,
                true,
                &no_files
            ),
            Spawn {
                file: "pwsh".into(),
                args: argv(&["-NoLogo", "-Command", "top"])
            }
        );
    }

    #[test]
    fn no_command_spawns_the_interactive_shell() {
        assert_eq!(
            resolve_spawn_with("pwsh", None, None, None, None, true, &no_files),
            Spawn {
                file: "pwsh".into(),
                args: vec![]
            }
        );
        assert_eq!(
            resolve_spawn_with(
                "/bin/bash",
                None,
                Some(&argv(&["-l"])),
                None,
                None,
                false,
                &no_files
            ),
            Spawn {
                file: "/bin/bash".into(),
                args: argv(&["-l"])
            }
        );
    }

    // ---- resolveWindowsCommand ----

    #[test]
    fn resolves_absolute_path_exactly_if_it_exists() {
        let target = "C:\\Program Files\\MyTool\\tool.exe";
        let is_file = |p: &str| p == target;
        assert_eq!(
            resolve_windows_command_with(target, Some("C:\\"), Some(&map(&[])), &is_file),
            target
        );
    }

    #[test]
    fn resolves_relative_path_with_extension_in_cwd() {
        let expected = "C:\\myproj\\bin\\tool.exe";
        let is_file = |p: &str| p == expected;
        let env = map(&[("PATHEXT", ".EXE;.CMD")]);
        assert_eq!(
            resolve_windows_command_with(".\\bin\\tool", Some("C:\\myproj"), Some(&env), &is_file),
            expected
        );
    }

    #[test]
    fn searches_cwd_first_then_path() {
        let env = map(&[
            ("PATH", "C:\\bin;C:\\Windows\\system32"),
            ("PATHEXT", ".EXE;.CMD"),
        ]);

        // Case 1: exists in cwd
        let is_file1 = |p: &str| p == "C:\\myproj\\tool.cmd";
        assert_eq!(
            resolve_windows_command_with("tool", Some("C:\\myproj"), Some(&env), &is_file1),
            "C:\\myproj\\tool.cmd"
        );

        // Case 2: exists in PATH (C:\Windows\system32)
        let is_file2 = |p: &str| p == "C:\\Windows\\system32\\tool.exe";
        assert_eq!(
            resolve_windows_command_with("tool", Some("C:\\myproj"), Some(&env), &is_file2),
            "C:\\Windows\\system32\\tool.exe"
        );
    }

    #[test]
    fn falls_back_to_verbatim_command_if_not_found() {
        let env = map(&[("PATH", "C:\\bin")]);
        assert_eq!(
            resolve_windows_command_with("unknowncmd", Some("C:\\"), Some(&env), &no_files),
            "unknowncmd"
        );
    }

    // ---- resolve_control_file (hermetic — env injected, never touches real process env) ----

    #[test]
    fn resolve_control_file_prefers_explicit_non_empty() {
        let resolved = resolve_control_file_with(Some("/explicit/control.json"), |_| {
            Some("/env/control.json".to_string())
        });
        assert_eq!(resolved.as_deref(), Some("/explicit/control.json"));
    }

    #[test]
    fn resolve_control_file_falls_back_to_env_when_explicit_empty_or_absent() {
        let via_empty =
            resolve_control_file_with(Some(""), |_| Some("/env/control.json".to_string()));
        assert_eq!(via_empty.as_deref(), Some("/env/control.json"));

        let via_none = resolve_control_file_with(None, |_| Some("/env/control.json".to_string()));
        assert_eq!(via_none.as_deref(), Some("/env/control.json"));
    }

    #[test]
    fn resolve_control_file_none_when_explicit_and_env_both_empty_or_absent() {
        assert_eq!(resolve_control_file_with(None, |_| None), None);
        assert_eq!(
            resolve_control_file_with(Some(""), |_| Some(String::new())),
            None
        );
    }

    // ---- build_env (scoped-token suppression + GOOGLE_API_KEY + paneId) ----

    #[test]
    fn build_env_omits_control_file_when_none() {
        let proc_env = map(&[]);
        let integ = map(&[]);
        let env = build_env(&EnvInputs {
            process_env: &proc_env,
            opts_env: None,
            integration_env: &integ,
            pane_id: None,
            control_file: None,
        });
        assert!(!env.contains_key("HYPERPANES_CONTROL_FILE"));
    }

    #[test]
    fn build_env_omits_control_file_when_empty_string() {
        let proc_env = map(&[]);
        let integ = map(&[]);
        let env = build_env(&EnvInputs {
            process_env: &proc_env,
            opts_env: None,
            integration_env: &integ,
            pane_id: None,
            control_file: Some(""),
        });
        assert!(!env.contains_key("HYPERPANES_CONTROL_FILE"));
    }

    #[test]
    fn build_env_points_at_control_file_when_no_scoped_token() {
        let proc_env = map(&[("HOME", "/home/me")]);
        let integ = map(&[]);
        let env = build_env(&EnvInputs {
            process_env: &proc_env,
            opts_env: None,
            integration_env: &integ,
            pane_id: Some("pane-7"),
            control_file: Some("/data/control.json"),
        });
        assert_eq!(
            env.get("HYPERPANES_CONTROL_FILE").map(String::as_str),
            Some("/data/control.json")
        );
        assert_eq!(
            env.get("HYPERPANES_PANE_ID").map(String::as_str),
            Some("pane-7")
        );
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
    }

    #[test]
    fn build_env_suppresses_control_file_for_a_scoped_child() {
        let proc_env = map(&[]);
        let integ = map(&[]);
        let opts = map(&[("HYPERPANES_CONTROL_TOKEN", "scoped-abc")]);
        let env = build_env(&EnvInputs {
            process_env: &proc_env,
            opts_env: Some(&opts),
            integration_env: &integ,
            pane_id: None,
            control_file: Some("/data/control.json"),
        });
        assert_eq!(
            env.get("HYPERPANES_CONTROL_TOKEN").map(String::as_str),
            Some("scoped-abc")
        );
        assert!(!env.contains_key("HYPERPANES_CONTROL_FILE"));
    }

    #[test]
    fn build_env_drops_electron_leaked_google_api_key() {
        let proc_env = map(&[("GOOGLE_API_KEY", "leaked-default")]);
        let integ = map(&[]);
        let env = build_env(&EnvInputs {
            process_env: &proc_env,
            opts_env: None,
            integration_env: &integ,
            pane_id: None,
            control_file: Some("/data/control.json"),
        });
        assert!(!env.contains_key("GOOGLE_API_KEY"));
    }

    #[test]
    fn build_env_keeps_a_user_supplied_google_api_key() {
        // opts.env overrides the leaked default → values differ → kept.
        let proc_env = map(&[("GOOGLE_API_KEY", "leaked-default")]);
        let integ = map(&[]);
        let opts = map(&[("GOOGLE_API_KEY", "real-user-key")]);
        let env = build_env(&EnvInputs {
            process_env: &proc_env,
            opts_env: Some(&opts),
            integration_env: &integ,
            pane_id: None,
            control_file: Some("/data/control.json"),
        });
        assert_eq!(
            env.get("GOOGLE_API_KEY").map(String::as_str),
            Some("real-user-key")
        );
    }

    #[test]
    fn is_posix_shell_matches_only_posix_basenames() {
        assert!(is_posix_shell("/bin/bash"));
        assert!(is_posix_shell("c:\\program files\\git\\bin\\bash.exe"));
        assert!(is_posix_shell("zsh"));
        assert!(!is_posix_shell("powershell.exe"));
        assert!(!is_posix_shell("pwsh"));
        assert!(!is_posix_shell("cmd.exe"));
    }

    // ---- ensure_strict_mcp_config (the goal-agent MCP guard) ----

    #[test]
    fn strict_mcp_config_added_after_mcp_config_value() {
        let cmd = "claude --dangerously-skip-permissions --mcp-config /x/mcp.json -p hello";
        let out = ensure_strict_mcp_config(cmd);
        assert!(
            out.contains("--mcp-config /x/mcp.json --strict-mcp-config"),
            "got {out}"
        );
        assert!(out.ends_with(" -p hello"), "got {out}");
    }

    #[test]
    fn strict_mcp_config_is_idempotent() {
        let cmd = "claude --dangerously-skip-permissions --mcp-config /x/mcp.json --strict-mcp-config -p hi";
        let out = ensure_strict_mcp_config(cmd);
        assert_eq!(out, cmd);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn strict_mcp_config_leaves_non_claude_commands_alone() {
        let cmd = "tokensave --mcp-config /x.json serve";
        let out = ensure_strict_mcp_config(cmd);
        assert_eq!(out, cmd);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn strict_mcp_config_leaves_commands_without_mcp_config_alone() {
        let cmd = "claude --resume abc";
        let out = ensure_strict_mcp_config(cmd);
        assert_eq!(out, cmd);
        assert!(matches!(out, Cow::Borrowed(_)));

        let wrapped = "headroom wrap claude --tool-search false --resume abc";
        let out = ensure_strict_mcp_config(wrapped);
        assert_eq!(out, wrapped);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn strict_mcp_config_handles_equals_form() {
        let cmd = "claude --mcp-config=/x/m.json -p hi";
        let out = ensure_strict_mcp_config(cmd);
        assert!(
            out.contains("--mcp-config=/x/m.json --strict-mcp-config"),
            "got {out}"
        );
    }

    /// A quoted value containing spaces must not be split at the whitespace: the guard has to
    /// find the value's closing quote, insert after it, and leave every other byte alone.
    #[test]
    fn strict_mcp_config_handles_quoted_value_with_spaces() {
        let cmd = "claude --dangerously-skip-permissions --mcp-config '/x/my config.json' -p hi";
        let out = ensure_strict_mcp_config(cmd);
        assert_eq!(
            out,
            "claude --dangerously-skip-permissions --mcp-config '/x/my config.json' --strict-mcp-config -p hi"
        );
    }

    /// The worker-runner shape: the claude launch lives inside a shell string an agent built, so
    /// the guard has to fire on the token stream and insert on a token boundary — the surrounding
    /// quoting and the `$HP_TASK_PAYLOAD` text must come back byte-for-byte.
    #[test]
    fn strict_mcp_config_handles_worker_runner_shell_string() {
        let cmd = "hyperpanes worker --queue g1 --command \"sh -c 'claude --dangerously-skip-permissions --mcp-config /home/u/.local/state/hyperpanes/goals-mcp.json -p \"$HP_TASK_PAYLOAD\"'\"";
        let out = ensure_strict_mcp_config(cmd);
        assert!(
            out.contains(
                "--mcp-config /home/u/.local/state/hyperpanes/goals-mcp.json --strict-mcp-config"
            ),
            "got {out}"
        );
        assert!(out.contains("sh -c 'claude"), "got {out}");
        assert!(out.contains("$HP_TASK_PAYLOAD"), "got {out}");
    }

    #[test]
    fn strict_mcp_config_skips_dangling_mcp_config() {
        let cmd = "claude --dangerously-skip-permissions --mcp-config";
        let out = ensure_strict_mcp_config(cmd);
        assert_eq!(out, cmd);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn resolve_spawn_shell_wraps_claude_with_strict_mcp_config() {
        let spawn = resolve_spawn(
            "/bin/bash",
            Some("claude --dangerously-skip-permissions --mcp-config /x/mcp.json -p hi"),
            None,
            None,
            None,
        );
        assert_eq!(spawn.args[0], "-c");
        assert!(
            spawn.args[1].contains("--strict-mcp-config"),
            "got {:?}",
            spawn.args
        );
    }

    #[test]
    fn resolve_spawn_direct_args_gain_strict_mcp_config() {
        let args = argv(&["--mcp-config", "/x/m.json", "-p", "hi"]);
        let spawn = resolve_spawn("/bin/bash", Some("claude"), Some(&args), None, None);
        assert_eq!(
            spawn.args,
            argv(&[
                "--mcp-config",
                "/x/m.json",
                "--strict-mcp-config",
                "-p",
                "hi"
            ])
        );
    }
}
