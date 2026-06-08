//! In-pane substring search over the terminal grid + scrollback — the model behind Ctrl+F.
//!
//! A pure match finder (case-insensitive substring) plus the bookkeeping the pane needs to
//! step through results. The grid/scrollback text is supplied by
//! [`TerminalPane`](crate::pane::TerminalPane) (which reads it off the live
//! `alacritty_terminal` grid, history included); this module never touches alacritty types.
//! Mirrors the xterm `@xterm/addon-search` wiring in the Electron `Terminal.tsx`/`SearchBox.tsx`:
//! incremental find-as-you-type, next/prev, and a 1-based "k / n" match counter.

/// One match: its absolute grid `line` (negative = scrollback) and the `[start, end)` column
/// span on that line. Column indices are character columns (exact for ASCII, the same v1
/// caveat as `links.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub line: i32,
    pub start: usize,
    pub end: usize,
}

/// Find every case-insensitive occurrence of `query` across `lines`, where each entry is
/// `(absolute_line, row_text)`. Matches within a line don't overlap (the scan resumes past
/// each hit). Returns them in line order (the caller supplies lines top→bottom).
pub fn find_matches(lines: &[(i32, String)], query: &str) -> Vec<Match> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    let q = query.to_lowercase();
    let qchars = q.chars().count();
    for (line, text) in lines {
        let hay = text.to_lowercase();
        let mut from = 0usize;
        while from <= hay.len() {
            match hay[from..].find(&q) {
                Some(pos) => {
                    let s = from + pos;
                    let start_col = hay[..s].chars().count();
                    out.push(Match {
                        line: *line,
                        start: start_col,
                        end: start_col + qchars,
                    });
                    // Resume past this hit (q is non-empty, so this always advances).
                    from = s + q.len();
                }
                None => break,
            }
        }
    }
    out
}

/// Pick the index of the result to activate when search results change, biased to the first
/// match at or below `prefer_line` (so opening search jumps to the nearest match below the
/// viewport top, like xterm's `findNext`). Returns `None` for an empty set.
pub fn initial_index(matches: &[Match], prefer_line: i32) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    matches
        .iter()
        .position(|m| m.line >= prefer_line)
        .or(Some(0))
}

/// Step `idx` by `+1` (next) or `-1` (prev) with wraparound over `len` results.
pub fn step(idx: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (idx + 1) % len
    } else {
        (idx + len - 1) % len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<(i32, String)> {
        vec![
            (-2, "the quick brown fox".to_string()),
            (-1, "FOX jumps over".to_string()),
            (0, "the lazy dog".to_string()),
        ]
    }

    #[test]
    fn finds_case_insensitive_matches_with_columns() {
        let m = find_matches(&lines(), "fox");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], Match { line: -2, start: 16, end: 19 });
        assert_eq!(m[1], Match { line: -1, start: 0, end: 3 });
    }

    #[test]
    fn finds_repeated_matches_on_one_line() {
        let m = find_matches(&[(0, "aXaXa".to_string())], "a");
        assert_eq!(m.len(), 3);
        assert_eq!(m.iter().map(|h| h.start).collect::<Vec<_>>(), vec![0, 2, 4]);
    }

    #[test]
    fn empty_query_finds_nothing() {
        assert!(find_matches(&lines(), "").is_empty());
    }

    #[test]
    fn initial_index_prefers_first_match_at_or_below_a_line() {
        let m = find_matches(&lines(), "fox");
        // prefer_line 0 → no match >= 0 → falls back to 0
        assert_eq!(initial_index(&m, 0), Some(0));
        // prefer_line -1 → second match (line -1) is the first >= -1
        assert_eq!(initial_index(&m, -1), Some(1));
        assert_eq!(initial_index(&[], 0), None);
    }

    #[test]
    fn step_wraps_both_directions() {
        assert_eq!(step(0, 3, true), 1);
        assert_eq!(step(2, 3, true), 0); // wrap forward
        assert_eq!(step(0, 3, false), 2); // wrap backward
        assert_eq!(step(0, 0, true), 0); // empty is a no-op
    }
}
