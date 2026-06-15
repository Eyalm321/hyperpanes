//! Port of the cwd parser in `src/main/shell-integration.ts`:
//! `parseOscCwd` / `fileUriToPath` / `oscDataToCwd`. Handles OSC 7 (POSIX file URI)
//! AND OSC 9;9 (cmd / Windows Terminal), MSYS `/c/...` → `C:\...`, remote-authority
//! rejection, and split-across-chunk carry with `OSC_MAX` abandonment.
//!
//! This is the pure, state-free scanner kept verbatim from the TS — it does NOT
//! delegate to alacritty's OSC handling, because the contract's quirks (MSYS drive
//! rewriting, remote-host rejection, OSC 9;9, bounded carry) are tested here.
//! De-duping on change is the caller's job (a prompt re-emits its cwd OSC every
//! keystroke).

// Bound on a carried, still-incomplete OSC sequence. A real cwd payload is short; a
// sequence that grows past this is junk and is abandoned rather than buffered.
const OSC_MAX: usize = 8192;
const OSC_PREFIX: &str = "\u{1b}]"; // ESC ]
const BEL: char = '\u{07}';
const ST: &str = "\u{1b}\\"; // ST = ESC \

/// Convert a `file://` URI (from OSC 7) to an OS-native absolute path, or `None` if
/// it can't / shouldn't be used as a local cwd.
///   * pwsh emits `file:///C:/Users/me/repo`  → `C:\Users\me\repo`
///   * git-bash emits MSYS `file:///c/Users/me/repo` → `C:\Users\me\repo`
///   * `%20` etc. are percent-decoded
///   * a non-empty, non-localhost authority (a REMOTE host) is rejected so a remote
///     shell can't relocate the local pane.
pub fn file_uri_to_path(uri: &str) -> Option<String> {
    if uri.is_empty() {
        return None;
    }
    let trimmed = uri.trim();
    let prefix = "file://";
    match trimmed.get(..prefix.len()) {
        Some(head) if head.eq_ignore_ascii_case(prefix) => {}
        _ => return None,
    }

    let rest = &trimmed[prefix.len()..]; // authority + path
    let (authority, path) = match rest.find('/') {
        None => (rest, ""),
        Some(idx) => (&rest[..idx], &rest[idx..]), // path keeps the leading '/'
    };
    // Reject remote hosts; allow empty authority or explicit localhost.
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }

    let decoded = decode_uri_component(path).unwrap_or_else(|| path.to_string());
    let b = decoded.as_bytes();

    // Windows drive with colon: /C:/Users/me → C:\Users\me
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        let drive = b[1].to_ascii_uppercase() as char;
        let tail = decoded[3..].replace('/', "\\");
        return Some(format!("{drive}:{tail}"));
    }
    // MSYS drive (git-bash): /c/Users/me → C:\Users\me
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/' {
        let drive = b[1].to_ascii_uppercase() as char;
        let tail = decoded[3..].replace('/', "\\");
        return Some(format!("{drive}:\\{tail}"));
    }
    // POSIX absolute path: hand back as-is.
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

// Minimal `decodeURIComponent`: decode `%XX` escapes (others pass through, like
// JS). Returns `None` on a malformed escape or non-UTF-8 result — the caller then
// falls back to the raw (un-decoded) path, mirroring the TS try/catch.
fn decode_uri_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let h = hex_val(bytes[i + 1])?;
            let l = hex_val(bytes[i + 2])?;
            out.push((h << 4) | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// Interpret one OSC payload (the bytes between `ESC]` and its terminator) as a cwd:
//   * `7;<file-uri>` → file_uri_to_path  (pwsh, bash/git-bash)
//   * `9;9;<path>`   → a raw OS path, optionally double-quoted  (cmd, Win Terminal)
// Anything else (title `0;…`, hyperlink `8;…`, progress `9;4;…`, …) is not a cwd.
fn osc_data_to_cwd(data: &str) -> Option<String> {
    if let Some(rest) = data.strip_prefix("7;") {
        return file_uri_to_path(rest);
    }
    if let Some(rest) = data.strip_prefix("9;9;") {
        let mut p = rest.trim();
        if p.len() >= 2 && p.starts_with('"') && p.ends_with('"') {
            p = &p[1..p.len() - 1];
        }
        if p.is_empty() {
            None
        } else {
            Some(p.to_string())
        }
    } else {
        None
    }
}

/// Pure, state-free scanner for cwd-bearing OSC sequences (OSC 7 + OSC 9;9). Given
/// the `carry` from the previous call and the next raw pty `chunk`, returns the cwd
/// of the LAST recognized sequence in this window (or `None`) plus the carry to feed
/// the next call. Handles sequences split across chunks (split payload and split
/// prefix) via a bounded carry.
pub fn parse_osc_cwd(carry: &str, chunk: &str) -> (Option<String>, String) {
    // Fast reject: nothing pending and no ESC anywhere → impossible to hold an OSC.
    if carry.is_empty() && !chunk.contains('\u{1b}') {
        return (None, String::new());
    }

    let buf = format!("{carry}{chunk}");
    let mut last_cwd: Option<String> = None;
    let mut search_from = 0usize;
    while let Some(i) = buf[search_from..].find(OSC_PREFIX) {
        let start = i + search_from;
        let after_prefix = start + OSC_PREFIX.len();
        let bel_idx = buf[after_prefix..].find(BEL).map(|i| i + after_prefix);
        let st_idx = buf[after_prefix..].find(ST).map(|i| i + after_prefix);
        let (end, term_len) = match (bel_idx, st_idx) {
            (Some(b), st) if st.is_none_or(|s| b < s) => (b, 1),
            (_, Some(s)) => (s, ST.len()),
            _ => break, // incomplete sequence at the tail — handled by carry below
        };
        if let Some(cwd) = osc_data_to_cwd(&buf[after_prefix..end]) {
            last_cwd = Some(cwd);
        }
        search_from = end + term_len;
    }

    // Carry forward only a trailing partial that might complete in the next chunk.
    let mut next_carry = String::new();
    if let Some(last_start) = buf.rfind(OSC_PREFIX) {
        let after = last_start + OSC_PREFIX.len();
        let complete = buf[after..].contains(BEL) || buf[after..].contains(ST);
        if !complete {
            let tail = &buf[last_start..];
            // abandon oversized junk
            next_carry = if tail.len() > OSC_MAX {
                String::new()
            } else {
                tail.to_string()
            };
        }
    } else if buf.ends_with('\u{1b}') {
        // The 2-char prefix may be split: a lone trailing ESC starts the next OSC.
        next_carry = "\u{1b}".to_string();
    }

    (last_cwd, next_carry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEL: &str = "\u{07}";
    const ESC: &str = "\u{1b}";

    fn seq(uri: &str) -> String {
        format!("{ESC}]7;{uri}{BEL}")
    }

    // ---- fileUriToPath ----

    #[test]
    fn converts_a_pwsh_windows_file_uri() {
        assert_eq!(
            file_uri_to_path("file:///C:/Users/me/repo").as_deref(),
            Some("C:\\Users\\me\\repo")
        );
    }

    #[test]
    fn uppercases_the_drive_letter() {
        assert_eq!(
            file_uri_to_path("file:///c:/temp").as_deref(),
            Some("C:\\temp")
        );
    }

    #[test]
    fn converts_an_msys_git_bash_drive_path() {
        assert_eq!(
            file_uri_to_path("file:///c/Users/me/repo").as_deref(),
            Some("C:\\Users\\me\\repo")
        );
    }

    #[test]
    fn percent_decodes_space_and_friends() {
        assert_eq!(
            file_uri_to_path("file:///C:/Users/My%20Repo").as_deref(),
            Some("C:\\Users\\My Repo")
        );
        assert_eq!(
            file_uri_to_path("file:///c/Users/My%20Repo").as_deref(),
            Some("C:\\Users\\My Repo")
        );
    }

    #[test]
    fn handles_a_percent_encoded_colon() {
        assert_eq!(
            file_uri_to_path("file:///c%3A/temp").as_deref(),
            Some("C:\\temp")
        );
    }

    #[test]
    fn accepts_an_explicit_localhost_authority() {
        assert_eq!(
            file_uri_to_path("file://localhost/C:/x").as_deref(),
            Some("C:\\x")
        );
    }

    #[test]
    fn rejects_a_remote_host() {
        assert_eq!(file_uri_to_path("file://otherbox/home/me"), None);
        assert_eq!(file_uri_to_path("file://192.168.1.5/srv"), None);
    }

    #[test]
    fn returns_a_posix_absolute_path_unchanged() {
        assert_eq!(
            file_uri_to_path("file:///home/me/proj").as_deref(),
            Some("/home/me/proj")
        );
    }

    #[test]
    fn rejects_non_file_uris_and_empties() {
        assert_eq!(file_uri_to_path("http://example.com"), None);
        assert_eq!(file_uri_to_path(""), None);
    }

    #[test]
    fn keeps_the_drive_root() {
        assert_eq!(file_uri_to_path("file:///C:/").as_deref(), Some("C:\\"));
    }

    // ---- parseOscCwd (OSC 7) ----

    #[test]
    fn finds_a_complete_sequence_in_one_chunk() {
        let (cwd, carry) = parse_osc_cwd("", &format!("hello{}world", seq("file:///C:/a/b")));
        assert_eq!(cwd.as_deref(), Some("C:\\a\\b"));
        assert_eq!(carry, "");
    }

    #[test]
    fn fast_rejects_a_plain_chunk_with_no_esc_and_no_carry() {
        let (cwd, carry) = parse_osc_cwd("", "just some normal output\n");
        assert_eq!(cwd, None);
        assert_eq!(carry, "");
    }

    #[test]
    fn accepts_an_st_terminator() {
        let (cwd, _) = parse_osc_cwd("", &format!("{ESC}]7;file:///C:/x{ESC}\\"));
        assert_eq!(cwd.as_deref(), Some("C:\\x"));
    }

    #[test]
    fn returns_the_last_complete_sequence_when_several_are_present() {
        let (cwd, _) = parse_osc_cwd(
            "",
            &format!("{}{}", seq("file:///C:/first"), seq("file:///C:/second")),
        );
        assert_eq!(cwd.as_deref(), Some("C:\\second"));
    }

    #[test]
    fn carries_a_uri_split_across_two_chunks() {
        let (cwd_a, carry_a) = parse_osc_cwd("", &format!("{ESC}]7;file:///C:/Users/"));
        assert_eq!(cwd_a, None);
        assert_eq!(carry_a, format!("{ESC}]7;file:///C:/Users/"));
        let (cwd_b, carry_b) = parse_osc_cwd(&carry_a, &format!("me/repo{BEL}"));
        assert_eq!(cwd_b.as_deref(), Some("C:\\Users\\me\\repo"));
        assert_eq!(carry_b, "");
    }

    #[test]
    fn carries_a_prefix_split_across_two_chunks() {
        let (cwd_a, carry_a) = parse_osc_cwd("", &format!("output{ESC}]7"));
        assert_eq!(cwd_a, None);
        assert_eq!(carry_a, format!("{ESC}]7"));
        let (cwd_b, _) = parse_osc_cwd(&carry_a, &format!(";file:///C:/proj{BEL}"));
        assert_eq!(cwd_b.as_deref(), Some("C:\\proj"));
    }

    #[test]
    fn carries_a_bare_trailing_esc() {
        let (_, carry_a) = parse_osc_cwd("", &format!("text{ESC}"));
        assert_eq!(carry_a, ESC);
        let (cwd_b, _) = parse_osc_cwd(&carry_a, &format!("]7;file:///C:/q{BEL}"));
        assert_eq!(cwd_b.as_deref(), Some("C:\\q"));
    }

    #[test]
    fn abandons_an_oversized_unterminated_sequence() {
        let huge = "x".repeat(20000);
        let (cwd, carry) = parse_osc_cwd("", &format!("{ESC}]7;file:///C:/{huge}"));
        assert_eq!(cwd, None);
        assert_eq!(carry, "");
    }

    #[test]
    fn does_not_retain_a_non_osc7_escape_tail() {
        let (cwd, carry) = parse_osc_cwd("", "\u{1b}[0m colored text");
        assert_eq!(cwd, None);
        assert_eq!(carry, "");
    }

    // ---- parseOscCwd (OSC 9;9, cmd) ----

    #[test]
    fn reads_a_raw_windows_path_from_osc_9_9_st_terminated() {
        let (cwd, _) = parse_osc_cwd("", &format!("{ESC}]9;9;C:\\Users\\me\\repo{ESC}\\"));
        assert_eq!(cwd.as_deref(), Some("C:\\Users\\me\\repo"));
    }

    #[test]
    fn strips_surrounding_quotes_windows_terminal_style() {
        let (cwd, _) = parse_osc_cwd("", &format!("{ESC}]9;9;\"C:\\Program Files\\x\"{BEL}"));
        assert_eq!(cwd.as_deref(), Some("C:\\Program Files\\x"));
    }

    #[test]
    fn ignores_a_non_cwd_osc_title() {
        let (cwd, _) = parse_osc_cwd("", &format!("{ESC}]0;my tab title{BEL}"));
        assert_eq!(cwd, None);
    }

    #[test]
    fn picks_the_cwd_osc_even_when_a_title_osc_precedes_it() {
        let (cwd, _) = parse_osc_cwd("", &format!("{ESC}]0;title{BEL}{ESC}]9;9;C:\\proj{BEL}"));
        assert_eq!(cwd.as_deref(), Some("C:\\proj"));
    }

    #[test]
    fn carries_a_9_9_path_split_across_two_chunks() {
        let (cwd_a, carry_a) = parse_osc_cwd("", &format!("{ESC}]9;9;C:\\Users\\"));
        assert_eq!(cwd_a, None);
        let (cwd_b, _) = parse_osc_cwd(&carry_a, &format!("me\\repo{BEL}"));
        assert_eq!(cwd_b.as_deref(), Some("C:\\Users\\me\\repo"));
    }
}
