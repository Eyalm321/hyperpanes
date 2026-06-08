//! Port of `src/main/paths.ts` — clickable terminal paths: take a candidate path token + a
//! pane's cwd, resolve to an absolute path, verify it on disk (exists / is-dir / is-exe), and
//! open it (in an editor with optional line:col, or via the OS default handler). The grid-side
//! extraction (which tokens look like paths) is ported from `src/renderer/components/pathLinks.ts`
//! and lives in the terminal-widget (it has the cell grid); it calls into THIS for resolve+open.
//! Keep resolution pure/testable; opening shells out (editor command or OS open).
//!
//! Owned by track `clickable-paths`.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Extensions we refuse to auto-open via the OS default handler, because on Windows the
/// shell would EXECUTE them. Only relevant on the OS-default branch: a configured editor
/// opens these as text just fine (`.js`/`.ps1` are source). Mirrors `EXECUTABLE_EXTS` in
/// `paths.ts`.
pub const EXECUTABLE_EXTS: &[&str] = &[
    ".exe", ".bat", ".cmd", ".com", ".scr", ".msi", ".msp", ".ps1", ".psm1", ".vbs", ".vbe",
    ".js", ".jse", ".wsf", ".wsh", ".hta", ".cpl", ".jar", ".reg", ".lnk", ".pif", ".sh",
    ".bash", ".zsh", ".fish", ".command", ".app",
];

/// True when `abs_path`'s (lowercased) extension is one we refuse to OS-open.
pub fn is_executable_ext(abs_path: &str) -> bool {
    match ext_lower(abs_path) {
        Some(ext) => EXECUTABLE_EXTS.contains(&ext.as_str()),
        None => false,
    }
}

/// The lowercased extension *including the leading dot* (e.g. `.ts`), or `None` when the
/// final path component has no `.` (or is a dotfile like `.gitignore`, which `path::extension`
/// treats as having no extension — matching Node's `path.extname`).
fn ext_lower(p: &str) -> Option<String> {
    Path::new(p)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
}

// ---------------------------------------------------------------------------------------
// Resolve (pure)
// ---------------------------------------------------------------------------------------

/// The on-disk verdict for a resolved path. Mirrors `ResolveResult` in `paths.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveResult {
    pub token: String,
    pub abs_path: String,
    pub exists: bool,
    pub is_dir: bool,
    pub is_exe: bool,
}

/// Best-effort home directory (the pty's own start dir falls back to this, matching
/// `opts.cwd || os.homedir()`). Empty string if neither is set.
fn home_dir() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default()
}

/// Lexically resolve `token` against `base` into an absolute, normalized path string — PURE
/// (no filesystem access). Expands a leading `~` to the home dir, then joins onto `base` when
/// the token is relative, and collapses `.`/`..` segments (like Node's `path.resolve`).
pub fn resolve_token(base: &str, token: &str) -> String {
    let expanded: String = if token == "~" || token.starts_with("~/") || token.starts_with("~\\") {
        format!("{}{}", home_dir(), &token[1..])
    } else {
        token.to_string()
    };

    let p = Path::new(&expanded);
    let joined: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(base).join(p)
    };
    normalize(&joined)
}

/// Lexically collapse `.` and `..` segments without touching disk. Keeps the path's prefix
/// (Windows drive) and root, and leaves any leading `..` that can't be popped (relative input).
fn normalize(p: &Path) -> String {
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop a preceding normal segment; otherwise keep the `..` (or ignore it right
                // after a root/prefix, where it has no effect).
                match out.last() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                    _ => out.push(comp),
                }
            }
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for c in out {
        buf.push(c.as_os_str());
    }
    buf.to_string_lossy().into_owned()
}

/// Resolve a single `token` against `cwd` (falling back to the home dir) and stat it.
pub fn resolve_path(cwd: Option<&str>, token: &str) -> ResolveResult {
    let base = match cwd {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => home_dir(),
    };
    let abs_path = resolve_token(&base, token);
    match std::fs::metadata(&abs_path) {
        Ok(md) => ResolveResult {
            token: token.to_string(),
            is_exe: is_executable_ext(&abs_path),
            is_dir: md.is_dir(),
            exists: true,
            abs_path,
        },
        Err(_) => ResolveResult {
            token: token.to_string(),
            abs_path,
            exists: false,
            is_dir: false,
            is_exe: false,
        },
    }
}

/// Resolve each candidate `token` against `cwd` and stat it (the batched form the renderer
/// calls). Mirrors `resolvePaths` in `paths.ts`.
pub fn resolve_paths(cwd: Option<&str>, tokens: &[String]) -> Vec<ResolveResult> {
    tokens.iter().map(|t| resolve_path(cwd, t)).collect()
}

// ---------------------------------------------------------------------------------------
// Open (shells out)
// ---------------------------------------------------------------------------------------

/// Outcome of an open attempt. Mirrors `OpenResult` in `paths.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenResult {
    pub ok: bool,
    /// Refused an executable on the OS-default path (the renderer toasts "Ctrl+click to copy").
    pub blocked: bool,
    pub error: Option<String>,
}

impl OpenResult {
    fn ok() -> Self {
        OpenResult { ok: true, blocked: false, error: None }
    }
    fn err(msg: impl Into<String>) -> Self {
        OpenResult { ok: false, blocked: false, error: Some(msg.into()) }
    }
    fn blocked(ext: impl Into<String>) -> Self {
        OpenResult { ok: false, blocked: true, error: Some(ext.into()) }
    }
}

/// Open a verified absolute path: a configured editor (with `line:col`) wins and is trusted for
/// any extension; otherwise zero-config VS Code if on PATH; otherwise the OS default handler,
/// which refuses to execute scripts/binaries. Directories just open the folder. Mirrors
/// `openResolvedPath` in `paths.ts`.
pub fn open_resolved_path(
    abs_path: &str,
    line: Option<u32>,
    col: Option<u32>,
    editor_command: &str,
) -> OpenResult {
    let md = match std::fs::metadata(abs_path) {
        Ok(m) => m,
        Err(_) => return OpenResult::err("not found"),
    };

    // Directories: just open the folder via the OS handler.
    if md.is_dir() {
        return match os_open(abs_path) {
            Ok(()) => OpenResult::ok(),
            Err(e) => OpenResult::err(e),
        };
    }

    // A configured editor wins and is trusted to handle any extension (incl. source scripts),
    // so the executable guard does not apply to this branch.
    let template = editor_command.trim();
    if !template.is_empty() {
        run_editor_template(template, abs_path, line, col);
        return OpenResult::ok();
    }

    // Zero-config default: VS Code if present, with a line/col jump.
    if let Some(code) = detect_vscode() {
        let target = match line {
            Some(l) => match col {
                Some(c) => format!("{abs_path}:{l}:{c}"),
                None => format!("{abs_path}:{l}"),
            },
            None => abs_path.to_string(),
        };
        launch(&format!("{} -g {}", quote(&code), quote(&target)));
        return OpenResult::ok();
    }

    // OS default handler — refuse to execute scripts/binaries.
    if let Some(ext) = ext_lower(abs_path) {
        if EXECUTABLE_EXTS.contains(&ext.as_str()) {
            return OpenResult::blocked(ext);
        }
    }
    match os_open(abs_path) {
        Ok(()) => OpenResult::ok(),
        Err(e) => OpenResult::err(e),
    }
}

/// Build the argv for an editor command template, substituting `{path}`/`{line}`/`{col}`. Split
/// into argv BEFORE substitution so `{path}` stays a single argument even with spaces, then
/// re-quote each piece. Returns the joined, shell-ready command line (also used by tests).
/// Mirrors `runEditorTemplate` in `paths.ts`.
pub fn editor_command_line(
    template: &str,
    abs_path: &str,
    line: Option<u32>,
    col: Option<u32>,
) -> String {
    let line_s = line.map(|l| l.to_string()).unwrap_or_default();
    let col_s = col.map(|c| c.to_string()).unwrap_or_default();
    let argv: Vec<String> = template
        .split_whitespace()
        .map(|part| {
            let mut s = part
                .replace("{path}", abs_path)
                .replace("{line}", &line_s)
                .replace("{col}", &col_s);
            // Tidy a dangling `::` / trailing `:` left when there's no line/col.
            while s.ends_with(':') {
                s.pop();
            }
            s
        })
        .filter(|s| !s.is_empty())
        .collect();
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

fn run_editor_template(template: &str, abs_path: &str, line: Option<u32>, col: Option<u32>) {
    let cmd = editor_command_line(template, abs_path, line, col);
    if cmd.is_empty() {
        return;
    }
    launch(&cmd);
}

/// Shell-quote one argument for the platform (mirrors `quote` in `paths.ts`).
pub fn quote(arg: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", arg.replace('"', "\"\""))
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Spawn a child process without flashing a console window. On Windows a GUI app spawning
/// `cmd`/`where` briefly pops a console; `CREATE_NO_WINDOW` suppresses it. A no-op elsewhere.
trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}
impl NoWindow for Command {
    #[cfg(windows)]
    fn no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
    #[cfg(not(windows))]
    fn no_window(&mut self) -> &mut Self {
        self
    }
}

/// Cached one-shot detection of VS Code on PATH (the zero-config default editor).
fn detect_vscode() -> Option<String> {
    static VSCODE: OnceLock<Option<String>> = OnceLock::new();
    VSCODE
        .get_or_init(|| {
            let finder = if cfg!(windows) { "where" } else { "which" };
            let out = Command::new(finder).arg("code").no_window().output().ok()?;
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        })
        .clone()
}

/// Launch a detached command line through the shell so things like `code.cmd` resolve. Errors
/// are swallowed — a missing editor just no-ops. Mirrors `launch` in `paths.ts`.
fn launch(command_line: &str) {
    let result = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", command_line])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .no_window()
            .spawn()
    } else {
        Command::new("sh")
            .args(["-c", command_line])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    // Detach: drop the handle without waiting.
    drop(result);
}

/// Open a path with the OS default handler (folder or non-executable file). Returns the
/// underlying error string on spawn failure. The Electron version uses `shell.openPath`; here
/// we shell out to the platform opener.
fn os_open(path: &str) -> Result<(), String> {
    let spawn = if cfg!(windows) {
        // `start` is a cmd builtin; the empty "" is the window title arg so a quoted path
        // isn't consumed as the title.
        Command::new("cmd").args(["/C", "start", "", path]).no_window().spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(path).spawn()
    } else {
        Command::new("xdg-open").arg(path).spawn()
    };
    spawn.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_ext_detection_is_case_insensitive() {
        assert!(is_executable_ext("C:/x/run.EXE"));
        assert!(is_executable_ext("/home/a/script.sh"));
        assert!(is_executable_ext("setup.Ps1"));
        assert!(!is_executable_ext("notes/todo.md"));
        assert!(!is_executable_ext("src/index.ts"));
        // A dotfile has no extension (matches Node path.extname('.gitignore') === '').
        assert!(!is_executable_ext(".gitignore"));
    }

    #[test]
    fn resolve_token_joins_relative_against_base() {
        let got = resolve_token("/home/user", "src/a.ts");
        // platform-normalized join of base + relative.
        let want = normalize(Path::new("/home/user").join("src/a.ts").as_path());
        assert_eq!(got, want);
    }

    #[test]
    fn resolve_token_keeps_absolute_token() {
        // An absolute token ignores the base entirely.
        if cfg!(windows) {
            let got = resolve_token("C:/base", "C:\\foo\\bar.ts");
            assert_eq!(got, "C:\\foo\\bar.ts");
        } else {
            let got = resolve_token("/base", "/foo/bar.ts");
            assert_eq!(got, "/foo/bar.ts");
        }
    }

    #[test]
    fn resolve_token_collapses_dot_and_dotdot() {
        let got = resolve_token("/home/user", "./a/../b/c.ts");
        let want = normalize(Path::new("/home/user/b/c.ts"));
        assert_eq!(got, want);
    }

    #[test]
    fn resolve_token_expands_tilde() {
        // Force a deterministic home for the test.
        std::env::set_var("USERPROFILE", "/Users/me");
        std::env::set_var("HOME", "/Users/me");
        let got = resolve_token("/whatever", "~/notes/todo.md");
        let want = normalize(Path::new("/Users/me/notes/todo.md"));
        assert_eq!(got, want);
    }

    #[test]
    fn resolve_path_reports_existence_dir_and_exe() {
        let dir = std::env::temp_dir();
        let sub = dir.join(format!("hp_paths_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&sub);
        let file = sub.join("note.txt");
        std::fs::write(&file, b"hi").unwrap();
        let exe = sub.join("run.exe");
        std::fs::write(&exe, b"MZ").unwrap();

        let base = sub.to_string_lossy().to_string();

        let r = resolve_path(Some(&base), "note.txt");
        assert!(r.exists && !r.is_dir && !r.is_exe);

        let r = resolve_path(Some(&base), "run.exe");
        assert!(r.exists && !r.is_dir && r.is_exe);

        let r = resolve_path(Some(&base), ".");
        assert!(r.exists && r.is_dir);

        let r = resolve_path(Some(&base), "nope.txt");
        assert!(!r.exists && !r.is_dir && !r.is_exe);

        let _ = std::fs::remove_dir_all(&sub);
    }

    #[test]
    fn resolve_paths_batches_in_order() {
        let base = std::env::temp_dir().to_string_lossy().to_string();
        let toks = vec!["a.txt".to_string(), "b.txt".to_string()];
        let res = resolve_paths(Some(&base), &toks);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].token, "a.txt");
        assert_eq!(res[1].token, "b.txt");
    }

    #[test]
    fn editor_template_keeps_spaced_path_as_one_arg() {
        let cmd = editor_command_line("subl {path}:{line}:{col}", "/a b/c.ts", Some(12), Some(4));
        // The path+suffix is one quoted argument; `subl` is the other.
        assert_eq!(cmd, format!("{} {}", quote("subl"), quote("/a b/c.ts:12:4")));
    }

    #[test]
    fn editor_template_trims_dangling_colon_without_line() {
        let cmd = editor_command_line("edit {path}:{line}:{col}", "/x/y.ts", None, None);
        // {line}/{col} empty → the `:::`-style suffix collapses away.
        assert_eq!(cmd, format!("{} {}", quote("edit"), quote("/x/y.ts")));
    }

    #[test]
    fn editor_template_line_only() {
        let cmd = editor_command_line("e {path}:{line}", "/x/y.ts", Some(9), None);
        assert_eq!(cmd, format!("{} {}", quote("e"), quote("/x/y.ts:9")));
    }

    #[test]
    fn open_missing_path_is_not_found() {
        let res = open_resolved_path("/definitely/not/here_zzz.txt", None, None, "");
        assert!(!res.ok);
        assert_eq!(res.error.as_deref(), Some("not found"));
    }

    #[test]
    fn open_executable_via_os_default_is_blocked() {
        // With no editor configured AND VS Code typically absent in CI, an executable file must
        // be refused on the OS-default branch. (If `code` IS on PATH this returns ok instead, so
        // only assert the blocked verdict when we actually reach the OS branch.)
        let dir = std::env::temp_dir();
        let sub = dir.join(format!("hp_paths_block_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&sub);
        let exe = sub.join("danger.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let p = exe.to_string_lossy().to_string();

        let res = open_resolved_path(&p, None, None, "");
        if detect_vscode().is_none() {
            assert!(res.blocked, "an .exe must be blocked on the OS-default branch");
            assert_eq!(res.error.as_deref(), Some(".exe"));
        }
        let _ = std::fs::remove_dir_all(&sub);
    }
}
