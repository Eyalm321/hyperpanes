//! Path-token extraction for clickable terminal links — a 1:1 port of
//! `src/renderer/components/pathLinks.ts`.
//!
//! Detects file-path tokens in a single rendered terminal row so the pane can turn them into
//! clickable links (resolved + verified by [`hyperpanes_core::paths`], opened/copied on click —
//! see [`crate::pane`]). Pure + unit-tested; the on-disk verification, cwd resolution and
//! open/copy actions live in `core::paths` and the pane's link layer that consumes these
//! candidates.
//!
//! Shape rule (decided): a candidate must contain a path separator OR end in a file extension.
//! Bare words like `build`/`src`/`test` never linkify even when a matching file exists — that
//! keeps prose from lighting up. The drive-letter colon in `C:\foo.ts:42` stays part of the
//! path; only a trailing `:line[:col]` is parsed off as a location suffix.
//!
//! Indices ([`PathCandidate::start`]/[`end`](PathCandidate::end)) are **character columns** into
//! the source row — exact for ASCII paths; wide (CJK) glyphs earlier on the line can shift this,
//! an accepted v1 limitation (mirrors the TS note on `cellFromIndex`).

/// A detected path-shaped token and the column range it occupies on the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCandidate {
    /// The path portion, with any `:line:col` suffix and wrapping punctuation removed.
    pub path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Inclusive start column into the source row (for the link's underline range).
    pub start: usize,
    /// Exclusive end column into the source row.
    pub end: usize,
}

// Wrapping punctuation stripped from the ends of an unquoted token: brackets, backticks/quotes,
// and trailing sentence punctuation (`see src/a.ts.`).
const LEAD: &[char] = &['(', '[', '{', '<', '`', '"', '\''];
const TRAIL: &[char] = &[')', ']', '}', '>', '`', '"', '\'', ',', ';', '.', '!', '?'];

/// True when a string looks path-shaped: has a separator, or ends in an extension. Mirrors
/// `hasPathShape` (`/[\\/]/` OR `/\.[A-Za-z0-9]{1,12}$/`).
pub fn has_path_shape(p: &str) -> bool {
    if p.contains('/') || p.contains('\\') {
        return true; // separator → covers ./ ../ ~/ and C:\ too
    }
    if let Some(dot) = p.rfind('.') {
        let after = &p[dot + 1..];
        let n = after.chars().count();
        if (1..=12).contains(&n) && after.chars().all(|c| c.is_ascii_alphanumeric()) {
            return true; // trailing .ext (also catches .gitignore)
        }
    }
    false
}

/// Parse `^\d+(?::\d+)?$` — the whole string is `line` or `line:col`. Used for the unquoted
/// trailing-suffix split.
fn parse_loc(s: &str) -> Option<(u32, Option<u32>)> {
    let mut parts = s.splitn(2, ':');
    let a = parts.next()?;
    if a.is_empty() || !a.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let line: u32 = a.parse().ok()?;
    match parts.next() {
        None => Some((line, None)),
        Some(b) => {
            if b.is_empty() || !b.bytes().all(|x| x.is_ascii_digit()) {
                return None;
            }
            Some((line, Some(b.parse().ok()?)))
        }
    }
}

/// Split a trailing `:line[:col]` off a token, but only when the part before it is itself
/// path-shaped (so `localhost:3000` isn't mistaken for a located path). Mirrors `splitSuffix`:
/// the regex finds the *leftmost* colon whose tail is a pure location, then gates on
/// `hasPathShape(head)` — failing that gate yields the whole token unsplit.
fn split_suffix(core: &str) -> (String, Option<u32>, Option<u32>) {
    for (i, ch) in core.char_indices() {
        if ch != ':' || i == 0 {
            continue; // head (`.+?`) must be non-empty
        }
        let tail = &core[i + 1..];
        if let Some((line, col)) = parse_loc(tail) {
            let head = &core[..i];
            return if has_path_shape(head) {
                (head.to_string(), Some(line), col)
            } else {
                (core.to_string(), None, None)
            };
        }
    }
    (core.to_string(), None, None)
}

/// Parse a `:line[:col]` suffix appearing right after a closing quote. Returns the values plus
/// the number of characters consumed. Mirrors the quoted-path branch's
/// `/^:(\d+)(?::(\d+))?/`.
fn parse_quote_suffix(rest: &[char]) -> Option<(u32, Option<u32>, usize)> {
    if rest.first() != Some(&':') {
        return None;
    }
    let mut idx = 1;
    let line_start = idx;
    while idx < rest.len() && rest[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == line_start {
        return None; // need ≥1 digit
    }
    let line: u32 = rest[line_start..idx].iter().collect::<String>().parse().ok()?;
    let mut col = None;
    if idx < rest.len() && rest[idx] == ':' {
        let csave = idx;
        idx += 1;
        let col_start = idx;
        while idx < rest.len() && rest[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == col_start {
            idx = csave; // bare ':' with no digits → optional group doesn't match
        } else {
            col = Some(rest[col_start..idx].iter().collect::<String>().parse().ok()?);
        }
    }
    Some((line, col, idx))
}

/// One token: a double-/single-quoted string (incl. its quotes), or a run of non-space,
/// non-quote characters. Returns `(token_chars, start_column)` pairs. Mirrors the TS
/// `TOKEN_RE = /"[^"]*"|'[^']*'|[^\s"']+/g` (an unterminated quote is skipped, not a token).
fn tokenize(chars: &[char]) -> Vec<(Vec<char>, usize)> {
    let mut out = Vec::new();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            // find the matching close quote
            if let Some(j) = (i + 1..n).find(|&j| chars[j] == c) {
                out.push((chars[i..=j].to_vec(), i));
                i = j + 1;
                continue;
            }
            // unterminated → the lone quote matches nothing; skip it.
            i += 1;
            continue;
        }
        // run of non-space, non-quote chars
        let start = i;
        while i < n && !chars[i].is_whitespace() && chars[i] != '"' && chars[i] != '\'' {
            i += 1;
        }
        out.push((chars[start..i].to_vec(), start));
    }
    out
}

/// Extract every path-shaped candidate from one rendered terminal row. Mirrors
/// `extractPathCandidates`.
pub fn extract_path_candidates(line: &str) -> Vec<PathCandidate> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();

    for (tok, tok_start) in tokenize(&chars) {
        let tlen = tok.len();
        let first = tok[0];

        // ---- quoted path ("C:\Program Files\app\read me.txt"), optionally with a :line:col
        // suffix right after the closing quote. ----
        if first == '"' || first == '\'' {
            let inner: String = tok[1..tlen - 1].iter().collect();
            let after = tok_start + tlen; // column just past the closing quote
            let suffix = parse_quote_suffix(&chars[after.min(chars.len())..]);
            let (line_no, col_no, end) = match suffix {
                Some((l, c, consumed)) => (Some(l), c, after + consumed),
                None => (None, None, after - 1), // exclude the closing quote from the range
            };
            if !inner.is_empty() && has_path_shape(&inner) {
                out.push(PathCandidate {
                    path: inner,
                    line: line_no,
                    col: col_no,
                    start: tok_start + 1,
                    end,
                });
            }
            continue;
        }

        // ---- unquoted run: trim wrapping punctuation, then peel the location suffix. ----
        let mut s = 0;
        let mut e = tlen;
        while s < e && LEAD.contains(&tok[s]) {
            s += 1;
        }
        while e > s && TRAIL.contains(&tok[e - 1]) {
            e -= 1;
        }
        if e <= s {
            continue;
        }
        let core: String = tok[s..e].iter().collect();
        let (path, line_no, col_no) = split_suffix(&core);
        if path.is_empty() || !has_path_shape(&path) {
            continue;
        }
        out.push(PathCandidate {
            path,
            line: line_no,
            col: col_no,
            start: tok_start + s,
            end: tok_start + e,
        });
    }

    out
}

/// Map a 0-based index into a (wrap-joined) logical line to a 1-based xterm-style cell.
/// Assumes one column per character — exact for ASCII paths. Kept for parity with the TS
/// `cellFromIndex` (the in-crate hit-tester works per single row, so it doesn't need the wrap
/// math, but downstream/test parity does).
pub fn cell_from_index(index: usize, start_row: usize, cols: usize) -> (usize, usize) {
    let cols = cols.max(1);
    ((index % cols) + 1, start_row + index / cols + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(line: &str) -> Vec<PathCandidate> {
        extract_path_candidates(line)
    }
    fn paths(line: &str) -> Vec<String> {
        only(line).into_iter().map(|c| c.path).collect()
    }
    /// The substring of `line` covered by candidate `c`'s [start, end) column range.
    fn slice(line: &str, c: &PathCandidate) -> String {
        line.chars().skip(c.start).take(c.end - c.start).collect()
    }

    // ---- hasPathShape ----
    #[test]
    fn has_path_shape_accepts_separators() {
        assert!(has_path_shape("src/index.ts"));
        assert!(has_path_shape("src\\index.ts"));
        assert!(has_path_shape("./build"));
        assert!(has_path_shape("../a"));
        assert!(has_path_shape("C:\\foo"));
    }
    #[test]
    fn has_path_shape_accepts_bare_files_with_extension() {
        assert!(has_path_shape("package.json"));
        assert!(has_path_shape(".gitignore"));
    }
    #[test]
    fn has_path_shape_rejects_bare_words() {
        assert!(!has_path_shape("build"));
        assert!(!has_path_shape("src"));
        assert!(!has_path_shape("README"));
    }

    // ---- extractPathCandidates ----
    #[test]
    fn finds_a_relative_path_and_underlines_exactly_it() {
        let line = "see src/renderer/Terminal.tsx for details";
        let c = only(line);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].path, "src/renderer/Terminal.tsx");
        assert_eq!(slice(line, &c[0]), "src/renderer/Terminal.tsx");
        assert_eq!(c[0].line, None);
    }

    #[test]
    fn parses_line_and_line_col_suffixes() {
        let c = only("a/b.ts:42");
        assert_eq!(c[0].path, "a/b.ts");
        assert_eq!((c[0].line, c[0].col), (Some(42), None));
        let c = only("a/b.ts:42:7");
        assert_eq!(c[0].path, "a/b.ts");
        assert_eq!((c[0].line, c[0].col), (Some(42), Some(7)));
    }

    #[test]
    fn keeps_drive_letter_colon_in_absolute_windows_path() {
        let c = only("at C:\\hyperpanes\\src\\Terminal.tsx:224");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].path, "C:\\hyperpanes\\src\\Terminal.tsx");
        assert_eq!(c[0].line, Some(224));
    }

    #[test]
    fn handles_a_quoted_path_with_spaces() {
        let line = "open \"C:\\Program Files\\app\\read me.txt\" now";
        let c = only(line);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].path, "C:\\Program Files\\app\\read me.txt");
        // range excludes the surrounding quotes
        assert_eq!(slice(line, &c[0]), "C:\\Program Files\\app\\read me.txt");
    }

    #[test]
    fn handles_a_quoted_path_with_suffix_after_closing_quote() {
        let c = only("\"a b.ts\":10:3");
        assert_eq!(c[0].path, "a b.ts");
        assert_eq!((c[0].line, c[0].col), (Some(10), Some(3)));
    }

    #[test]
    fn strips_wrapping_punctuation() {
        assert_eq!(only("(src/a.ts)")[0].path, "src/a.ts");
        assert_eq!(only("`src/a.ts`")[0].path, "src/a.ts");
        assert_eq!(only("edited src/a.ts.")[0].path, "src/a.ts");
        assert_eq!(only("files: a.ts, b.ts")[0].path, "a.ts");
    }

    #[test]
    fn does_not_mistake_host_port_or_bare_words_for_paths() {
        assert_eq!(only("listening on localhost:3000").len(), 0);
        assert_eq!(only("run the build step in src now").len(), 0);
        // shape-passes (looks like .1 ext) but the disk check downstream rejects it
        assert_eq!(paths("version v18.3.1 here"), vec!["v18.3.1"]);
    }

    #[test]
    fn finds_multiple_paths_on_one_line() {
        assert_eq!(paths("moved src/a.ts -> dist/a.js"), vec!["src/a.ts", "dist/a.js"]);
    }

    #[test]
    fn matches_dot_dotdot_and_tilde_prefixes() {
        assert_eq!(only("./scripts/run.sh")[0].path, "./scripts/run.sh");
        assert_eq!(only("../shared/x.ts")[0].path, "../shared/x.ts");
        assert_eq!(only("~/notes/todo.md")[0].path, "~/notes/todo.md");
    }

    // ---- cellFromIndex ----
    #[test]
    fn cell_from_index_maps_within_a_single_row() {
        assert_eq!(cell_from_index(0, 5, 80), (1, 6));
        assert_eq!(cell_from_index(79, 5, 80), (80, 6));
    }
    #[test]
    fn cell_from_index_wraps_past_the_column_count() {
        assert_eq!(cell_from_index(80, 5, 80), (1, 7));
        assert_eq!(cell_from_index(165, 0, 80), (6, 3));
    }
}
