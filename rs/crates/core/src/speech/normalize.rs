//! Markdown-to-speech text normalization: strips markup syntax so a TTS backend
//! reads clean prose instead of literal asterisks, hashes, pipes, and link brackets.

/// Hard cap on a spoken utterance's length, in `char`s.
const MAX_LEN: usize = 1200;
const TRUNCATED_SUFFIX: &str = " Truncated.";

/// Reduce a Claude assistant reply to speakable plain text:
/// - drop code-fence delimiters, ATX header markers (`#`), and markdown link syntax
///   (`[text](url)` -> `text`), and bare `http(s)://` URLs
/// - flatten pipe-delimited table rows into `", "`-joined cells (dropping separator
///   rows like `|---|---|`)
/// - collapse all whitespace (including newlines) to single spaces and trim
/// - cap the result at [`MAX_LEN`] chars, cutting at the last sentence boundary
///   (`.`, `!`, `?`) before the cap and appending `" Truncated."`
pub fn normalize_for_speech(input: &str) -> String {
    let without_fences = strip_code_fences(input);
    let lines: Vec<String> = without_fences
        .lines()
        .map(|line| convert_table_row_or_pass(&strip_atx_header(line)))
        .collect();
    let joined = lines.join("\n");
    let joined = strip_markdown_links(&joined);
    let joined = strip_bare_urls(&joined);
    // Belt-and-suspenders: any markup character that survived structured handling
    // (e.g. a stray `*`/`` ` `` emphasis marker) is dropped outright rather than read
    // aloud literally.
    let joined: String = joined
        .chars()
        .filter(|c| !matches!(c, '`' | '*' | '#' | '['))
        .collect();
    let collapsed = collapse_whitespace(&joined);
    truncate_at_sentence(&collapsed, MAX_LEN)
}

/// Drop ``` fence delimiter lines, keeping the code content itself (it still reads
/// as plain text, which is the best a speech backend can do with it).
fn strip_code_fences(input: &str) -> String {
    let mut out_lines = Vec::new();
    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            continue;
        }
        out_lines.push(line);
    }
    out_lines.join("\n")
}

/// Strip a leading ATX header marker (`#` through `######` followed by a space) from
/// one line, leaving the heading text.
fn strip_atx_header(line: &str) -> String {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &trimmed[hashes..];
        return rest.strip_prefix(' ').unwrap_or(rest).to_string();
    }
    line.to_string()
}

fn is_table_separator_row(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && trimmed.contains('-')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// If `line` looks like a `|`-delimited table row, flatten it to `", "`-joined cells
/// (dropping a pure separator row entirely); otherwise return it unchanged.
fn convert_table_row_or_pass(line: &str) -> String {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return line.to_string();
    }
    if is_table_separator_row(trimmed) {
        return String::new();
    }
    let inner = trimmed.trim_start_matches('|').trim_end_matches('|');
    inner
        .split('|')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Replace `[text](url)` with `text`. Non-nested, single-line matches only.
fn strip_markdown_links(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(close_rel) = chars[i + 1..].iter().position(|&c| c == ']') {
                let text_end = i + 1 + close_rel;
                let paren_start = text_end + 1;
                if chars.get(paren_start) == Some(&'(') {
                    if let Some(close_paren_rel) =
                        chars[paren_start + 1..].iter().position(|&c| c == ')')
                    {
                        let url_end = paren_start + 1 + close_paren_rel;
                        let text: String = chars[i + 1..text_end].iter().collect();
                        out.push_str(&text);
                        i = url_end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Remove bare `http://`/`https://` URLs (anything up to the next whitespace).
fn strip_bare_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = &rest[end..];
            continue;
        }
        let ch = rest.chars().next().expect("rest is non-empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Collapse every run of whitespace (including newlines) to a single space, and trim
/// the ends.
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_space = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

/// Cap `input` at `max` chars, cutting at the last sentence-ending punctuation before
/// the cap (inclusive) and appending [`TRUNCATED_SUFFIX`]. If no sentence boundary is
/// found, hard-cuts at `max` chars instead.
fn truncate_at_sentence(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    let truncated: String = input.chars().take(max).collect();
    let cut = truncated.rfind(['.', '!', '?']).map(|idx| idx + 1);
    let base = match cut {
        Some(idx) => &truncated[..idx],
        None => truncated.as_str(),
    };
    format!("{}{TRUNCATED_SUFFIX}", base.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_atx_headers() {
        assert_eq!(normalize_for_speech("# Heading\ntext"), "Heading text");
        assert_eq!(normalize_for_speech("### Deep heading"), "Deep heading");
    }

    #[test]
    fn strips_markdown_links_keeping_text() {
        assert_eq!(
            normalize_for_speech("See [the docs](https://example.com/docs) for more."),
            "See the docs for more."
        );
    }

    #[test]
    fn strips_bare_urls() {
        assert_eq!(
            normalize_for_speech("Visit https://example.com/path?x=1 now"),
            "Visit now"
        );
    }

    #[test]
    fn flattens_table_rows_and_drops_separator() {
        let input = "| Name | Age |\n|---|---|\n| Alice | 30 |";
        assert_eq!(normalize_for_speech(input), "Name, Age Alice, 30");
    }

    #[test]
    fn strips_bold_and_inline_code() {
        assert_eq!(
            normalize_for_speech("Run **`cargo test`** to check."),
            "Run cargo test to check."
        );
    }

    #[test]
    fn strips_code_fence_delimiters() {
        let input = "Before\n```rust\nlet x = 1;\n```\nAfter";
        assert_eq!(normalize_for_speech(input), "Before let x = 1; After");
    }

    #[test]
    fn collapses_whitespace_and_newlines() {
        assert_eq!(
            normalize_for_speech("Hello   \n\n  world\t\tagain"),
            "Hello world again"
        );
    }

    #[test]
    fn truncates_at_sentence_boundary_and_appends_suffix() {
        let sentence = "This is a sentence. ";
        let input = sentence.repeat(100);
        let out = normalize_for_speech(&input);
        assert!(out.len() <= MAX_LEN + TRUNCATED_SUFFIX.len());
        assert!(out.ends_with("Truncated."));
        // The cut must land right after a sentence-ending punctuation mark.
        let body = out.strip_suffix(TRUNCATED_SUFFIX).unwrap();
        assert!(body.ends_with(['.', '!', '?']));
    }

    #[test]
    fn short_text_is_unchanged_by_truncation() {
        assert_eq!(normalize_for_speech("Short reply."), "Short reply.");
    }

    #[test]
    fn realistic_combined_reply_has_no_markdown_characters() {
        let input = "\
# Summary

I updated **`speech/normalize.rs`** to strip markdown. See [the PR](https://example.com/pr/42)
for details.

| File | Status |
|------|--------|
| normalize.rs | *done* |
| engine.rs | in progress |

```rust
fn main() {
    println!(\"hi\");
}
```

- Next: wire up `engine.rs`
- Then: settings load/save
";
        let out = normalize_for_speech(input);
        for forbidden in ['*', '`', '#', '['] {
            assert!(
                !out.contains(forbidden),
                "output must not contain {forbidden:?}: {out:?}"
            );
        }
        assert!(out.contains("Summary"));
        assert!(out.contains("the PR"));
        assert!(!out.contains("example.com"));
    }
}
