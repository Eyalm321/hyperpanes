//! Source guard: no Slint `changed <prop> => { … }` handler and no `init => { … }` callback may
//! move keyboard focus synchronously (`x.focus()` / `x.clear-focus()`).
//!
//! Why (goal trcrash): Slint runs pending change handlers, and the `init` of newly created
//! conditionals/repeaters, from inside AccessKit's accessibility-tree build
//! (`build_new_tree` → `WindowInner::ensure_tree_instantiated`) while the winit adapter holds
//! its `RefCell<AccessKitAdapter>` borrow. A focus change there calls `handle_focus_change`,
//! which re-borrows the adapter: `RefCell already borrowed` panic, whenever an AT-SPI client is
//! listening. Cancelling the New goal box crashed the app this way (`changed refocus-tick`).
//!
//! The pattern instead: the handler arms a 0 ms single-fire `Timer` (or, for mount focus, a
//! `Timer` that starts running when its component is created) and the timer's `triggered`
//! moves focus on the next event-loop turn, outside any borrow. `triggered`, `clicked`,
//! `pointer-event`, `key-pressed` etc. run from timers or input dispatch and may focus.
//!
//! Runtime counterparts that drive the real crash path: `src/focus_reentrancy_tests.rs`
//! (AppWindow) and `terminal-widget/tests/focus_reentrancy.rs` (TerminalPane).
//!
//! Limitation: a handler calling a `function` that focuses is not followed; keep focus calls
//! out of functions reachable from `changed`/`init`.

use std::path::{Path, PathBuf};

/// Every `.slint` file compiled into the app: its own UI plus the terminal widget library.
fn slint_sources() -> Vec<PathBuf> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<PathBuf> = std::fs::read_dir(manifest.join("ui"))
        .expect("read ui/")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "slint"))
        .collect();
    files.push(manifest.join("../terminal-widget/ui/widget.slint"));
    files.sort();
    assert!(
        files.len() > 5,
        "expected the app's .slint sources, found {files:?}"
    );
    files
}

/// Blank out comments and string literals (keeping newlines so line numbers survive), so a
/// `focus()` mentioned in a comment or a label never counts.
fn strip_comments_and_strings(src: &str) -> Vec<char> {
    let c: Vec<char> = src.chars().collect();
    let mut out = c.clone();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                out[i] = ' ';
                i += 1;
            }
        } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                if c[i] != '\n' {
                    out[i] = ' ';
                }
                i += 1;
            }
            for ch in out.iter_mut().take(i + 2).skip(i) {
                *ch = ' ';
            }
            i += 2;
        } else if c[i] == '"' {
            out[i] = ' ';
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' {
                    out[i] = ' ';
                    i += 1;
                }
                if i < c.len() && c[i] != '\n' {
                    out[i] = ' ';
                }
                i += 1;
            }
            if i < c.len() {
                out[i] = ' ';
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    out
}

fn is_ident(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

/// The identifier ending right before `end` (exclusive), skipping trailing whitespace.
fn ident_before(c: &[char], mut end: usize) -> (String, usize) {
    while end > 0 && c[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_ident(c[start - 1]) {
        start -= 1;
    }
    (c[start..end].iter().collect(), start)
}

/// `(line, header, body)` for every `changed <prop> => { … }` and `init => { … }` block.
fn focus_sensitive_blocks(c: &[char]) -> Vec<(usize, String, String)> {
    let mut blocks = Vec::new();
    let mut i = 0;
    while i + 1 < c.len() {
        if !(c[i] == '=' && c[i + 1] == '>') {
            i += 1;
            continue;
        }
        let (last, last_start) = ident_before(c, i);
        let header = if last == "init" {
            Some("init".to_string())
        } else {
            let (prev, _) = ident_before(c, last_start);
            (prev == "changed" && !last.is_empty()).then(|| format!("changed {last}"))
        };
        let mut j = i + 2;
        while j < c.len() && c[j].is_whitespace() {
            j += 1;
        }
        if let (Some(header), Some('{')) = (header, c.get(j)) {
            let (mut depth, mut k) = (0usize, j);
            while k < c.len() {
                match c[k] {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            let line = c[..i].iter().filter(|&&ch| ch == '\n').count() + 1;
            blocks.push((line, header, c[j + 1..k.min(c.len())].iter().collect()));
        }
        i += 2;
    }
    blocks
}

/// The focus-moving calls (`focus(` / `clear-focus(` as a whole identifier) in `body`.
fn focus_calls(body: &str) -> Vec<String> {
    let c: Vec<char> = body.chars().collect();
    let mut found = Vec::new();
    for (i, &ch) in c.iter().enumerate() {
        if ch == '(' {
            let (name, _) = ident_before(&c, i);
            if name == "focus" || name == "clear-focus" {
                found.push(format!("{name}()"));
            }
        }
    }
    found
}

#[test]
fn no_synchronous_focus_in_change_handlers_or_init() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap();
    let mut violations = Vec::new();
    let mut blocks_seen = 0;
    for path in slint_sources() {
        let src = std::fs::read_to_string(&path).unwrap();
        let clean = strip_comments_and_strings(&src);
        let path = path.canonicalize().unwrap();
        let shown = path
            .strip_prefix(&repo)
            .unwrap_or(&path)
            .display()
            .to_string();
        for (line, header, body) in focus_sensitive_blocks(&clean) {
            blocks_seen += 1;
            let calls = focus_calls(&body);
            if !calls.is_empty() {
                violations.push(format!(
                    "{shown}:{line}: `{header}` calls {}",
                    calls.join(", ")
                ));
            }
        }
    }
    assert!(
        blocks_seen > 0,
        "the scanner found no changed/init blocks at all"
    );
    assert!(
        violations.is_empty(),
        "focus moved synchronously in a change handler / init — this panics the app inside \
         AccessKit's tree build when an AT-SPI client is listening (goal trcrash). Arm a 0 ms \
         single-fire Timer and focus from its `triggered` instead:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn scanner_catches_the_trcrash_shapes() {
    let src = r#"
        component P {
            changed refocus-tick => { if (!root.search-open) { fs.focus(); } }
            changed search-open => {
                if (root.search-open) { query.focus(); } else { fs.focus(); }
            }
            ti := TextInput { init => { ti.select-all(); ti.focus(); } }
            changed x => { y.clear-focus(); }
            // changed z => { fs.focus(); }   (comment: ignored)
            changed w => { root.focus-pane(1); t.running = true; }
            init => { root.label = "focus()"; }
            t := Timer { interval: 0ms; triggered => { self.running = false; fs.focus(); } }
            TouchArea { clicked => { fs.focus(); } }
        }
    "#;
    let clean = strip_comments_and_strings(src);
    let hits: Vec<(usize, String, Vec<String>)> = focus_sensitive_blocks(&clean)
        .into_iter()
        .map(|(l, h, b)| (l, h, focus_calls(&b)))
        .filter(|(_, _, calls)| !calls.is_empty())
        .collect();
    let headers: Vec<&str> = hits.iter().map(|(_, h, _)| h.as_str()).collect();
    assert_eq!(
        headers,
        vec![
            "changed refocus-tick",
            "changed search-open",
            "init",
            "changed x"
        ],
        "{hits:?}"
    );
    assert_eq!(hits[1].2, vec!["focus()", "focus()"]);
    assert_eq!(hits[3].2, vec!["clear-focus()"]);
}
