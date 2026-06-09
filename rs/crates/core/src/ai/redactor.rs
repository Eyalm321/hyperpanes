//! Port of `src/main/ai/redactor.ts` — secret redaction (credit cards, API tokens, …)
//! applied to pane text before it leaves for the model. Pure regex. Mirror `redactor.test.ts`.
//!
//! Scrubs likely secrets out of a string before it's handed to a local LLM or
//! persisted in a summary, replacing each with the literal token `[REDACTED]`
//! and leaving everything else intact.
//!
//! **Best-effort, not exhaustive.** This is keyword/shape-based: it catches
//! `KEY=VALUE` pairs whose key *names* a secret (`SECRET`/`TOKEN`/`PASSWORD`/
//! `API_KEY`/…), `Authorization:` headers, JWTs, AWS access-key ids, and PEM
//! private-key blocks. A *bare* high-entropy token with no recognizable key
//! name around it — e.g. a raw `ghp_…` GitHub token or an `xoxb-…` Slack token
//! pasted on its own — is NOT caught (there is no high-entropy heuristic here).
//! This is an acceptable residual risk because the only consumer is a local
//! Ollama instance on the LAN, never a third-party/cloud endpoint; callers that
//! need stronger guarantees must not feed it untrusted secrets. A high-entropy
//! heuristic could be added if the threat model ever widens.
//!
//! Invariants (matching the TS source):
//!  - pure and total — never panics, any string is valid.
//!  - idempotent — `redact(redact(x)) == redact(x)`; redacting `[REDACTED]` is a no-op.
//!  - conservative — ordinary prose, paths, code, and non-secret `KEY=VALUE`
//!    (e.g. `NODE_ENV=production`, `PORT=3000`) pass through unchanged.
//!  - line-count preserving for in-place redactions (only the multiline PEM
//!    block, by spec, collapses to a single token).
//!
//! The `regex` crate is not an available dependency, so each TS regex is
//! hand-rolled as a byte scanner — matching the convention already set by
//! `crate::ansi_strip`. We scan bytes: every literal and character class here is
//! ASCII, and multibyte UTF-8 bytes (all >= 0x80) never match any class, so they
//! are copied through verbatim and the output is always valid UTF-8.

const TOKEN: &[u8] = b"[REDACTED]";

/// Strip likely secrets from `text`, replacing each with `[REDACTED]`.
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    // Same ordering as the TS chained `.replace` calls: PEM first (so its inner
    // base64 can't be mistaken for other patterns), then JWT, AWS, Authorization,
    // and finally secret-ish KEY=VALUE.
    let b = text.as_bytes().to_vec();
    let b = redact_pem(&b);
    let b = redact_jwt(&b);
    let b = redact_aws(&b);
    let b = redact_auth(&b);
    let b = redact_kv(&b);
    // SAFETY/total: only ASCII bytes are ever inserted/removed at ASCII boundaries;
    // multibyte sequences are copied intact, so the result stays valid UTF-8.
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

// ---- character classes (ASCII) ----

fn is_word_dot_dash(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'
}

fn is_jwt_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_pem_header_char(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b' '
}

// JS \s (the ASCII subset; the non-ASCII members never appear in our inputs).
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c | 0x0b)
}

fn find_sub(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn eq_ci(hay: &[u8], at: usize, lit: &[u8]) -> bool {
    at + lit.len() <= hay.len() && hay[at..at + lit.len()].eq_ignore_ascii_case(lit)
}

// ---- PEM private-key block -> single token ----
//
//   -----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----[\s\S]*?-----END (?:[A-Z0-9 ]+ )?PRIVATE KEY-----
//
// `[\s\S]*?` is lazy: we take the first *valid* END header after the BEGIN.

// Given `start` is the index immediately after a "-----BEGIN " / "-----END "
// literal, returns the index just past the closing "-----" iff a valid
// `(?:[A-Z0-9 ]+ )?PRIVATE KEY-----` follows.
fn pem_header_end(b: &[u8], start: usize) -> Option<usize> {
    let mut p = start;
    while p < b.len() && is_pem_header_char(b[p]) {
        p += 1;
    }
    // The header chars run up to the first non-[A-Z0-9 ] byte; that run must end
    // with "PRIVATE KEY" and be immediately followed by "-----".
    if b[start..p].ends_with(b"PRIVATE KEY") && b[p..].starts_with(b"-----") {
        Some(p + 5)
    } else {
        None
    }
}

fn redact_pem(b: &[u8]) -> Vec<u8> {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const END: &[u8] = b"-----END ";
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(BEGIN) {
            if let Some(bh_end) = pem_header_end(b, i + BEGIN.len()) {
                // Find the first valid END header (lazy match).
                let mut search = bh_end;
                let mut matched_end: Option<usize> = None;
                while let Some(e) = find_sub(b, END, search) {
                    if let Some(eh_end) = pem_header_end(b, e + END.len()) {
                        matched_end = Some(eh_end);
                        break;
                    }
                    search = e + END.len();
                }
                if let Some(eh_end) = matched_end {
                    out.extend_from_slice(TOKEN);
                    i = eh_end;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

// ---- JWT: eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+ ----
fn redact_jwt(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"eyJ") {
            if let Some(end) = jwt_match(b, i) {
                out.extend_from_slice(TOKEN);
                i = end;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn jwt_match(b: &[u8], start: usize) -> Option<usize> {
    // "eyJ" then >=1 class, '.', >=1 class, '.', >=1 class.
    let mut p = start + 3; // past "eyJ"
    // segment 1 already has "eyJ"; require at least one more class char.
    let s1 = p;
    while p < b.len() && is_jwt_char(b[p]) {
        p += 1;
    }
    if p == s1 {
        return None;
    }
    if p >= b.len() || b[p] != b'.' {
        return None;
    }
    p += 1;
    let s2 = p;
    while p < b.len() && is_jwt_char(b[p]) {
        p += 1;
    }
    if p == s2 {
        return None;
    }
    if p >= b.len() || b[p] != b'.' {
        return None;
    }
    p += 1;
    let s3 = p;
    while p < b.len() && is_jwt_char(b[p]) {
        p += 1;
    }
    if p == s3 {
        return None;
    }
    Some(p)
}

// ---- AWS access key id: AKIA[0-9A-Z]{16} ----
fn redact_aws(b: &[u8]) -> Vec<u8> {
    const PREFIX: &[u8] = b"AKIA";
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(PREFIX) {
            let tail = i + 4;
            if tail + 16 <= b.len()
                && b[tail..tail + 16]
                    .iter()
                    .all(|&c| c.is_ascii_digit() || c.is_ascii_uppercase())
            {
                out.extend_from_slice(TOKEN);
                i = tail + 16;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

// ---- Authorization header: keep scheme, redact only the credential ----
//   (Authorization:\s*(?:Bearer|Basic)\s+)(\S+)  [case-insensitive]
fn redact_auth(b: &[u8]) -> Vec<u8> {
    const AUTH: &[u8] = b"Authorization:";
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if eq_ci(b, i, AUTH) {
            if let Some((prefix_end, cred_end)) = auth_match(b, i) {
                // keep the prefix verbatim, replace the credential with the token
                out.extend_from_slice(&b[i..prefix_end]);
                out.extend_from_slice(TOKEN);
                i = cred_end;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

// Returns (prefix_end, credential_end): prefix is everything up to and including
// the `\s+` after the scheme; the credential is the following `\S+`.
fn auth_match(b: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut p = start + b"Authorization:".len();
    while p < b.len() && is_ws(b[p]) {
        p += 1; // \s*
    }
    if eq_ci(b, p, b"Bearer") {
        p += 6;
    } else if eq_ci(b, p, b"Basic") {
        p += 5;
    } else {
        return None;
    }
    let ws_start = p;
    while p < b.len() && is_ws(b[p]) {
        p += 1; // \s+
    }
    if p == ws_start {
        return None; // need at least one whitespace
    }
    let prefix_end = p;
    let cred_start = p;
    while p < b.len() && !is_ws(b[p]) {
        p += 1; // \S+
    }
    if p == cred_start {
        return None; // need at least one credential char
    }
    Some((prefix_end, p))
}

// ---- secret-ish KEY=VALUE ----
//   ([\w.-]*SECRET_KEY[\w.-]*)(\s*=\s*)(?:"([^"\r\n]*)"|'([^'\r\n]*)'|([^\s\r\n]*))
// SECRET_KEY = SECRET|TOKEN|PASSWORD|PASSWD|API[_-]?KEY|PRIVATE[_-]?KEY|ACCESS[_-]?KEY|CREDENTIAL
fn redact_kv(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // A KEY=VALUE match begins at the start of a contiguous [\w.-] run (the
        // greedy leading group anchors the leftmost match there).
        if is_word_dot_dash(b[i]) && (i == 0 || !is_word_dot_dash(b[i - 1])) {
            let mut run_end = i;
            while run_end < b.len() && is_word_dot_dash(b[run_end]) {
                run_end += 1;
            }
            if run_contains_secret_key(&b[i..run_end]) {
                if let Some((eq_end, value)) = kv_value(b, run_end) {
                    out.extend_from_slice(&b[i..run_end]); // group1: key
                    out.extend_from_slice(&b[run_end..eq_end]); // group2: \s*=\s*
                    match value {
                        KvValue::Double(end) => {
                            out.push(b'"');
                            out.extend_from_slice(TOKEN);
                            out.push(b'"');
                            i = end;
                        }
                        KvValue::Single(end) => {
                            out.push(b'\'');
                            out.extend_from_slice(TOKEN);
                            out.push(b'\'');
                            i = end;
                        }
                        KvValue::Bare(end) => {
                            out.extend_from_slice(TOKEN);
                            i = end;
                        }
                    }
                    continue;
                }
            }
            // Not a secret KV: copy the whole run so we don't re-scan its interior.
            out.extend_from_slice(&b[i..run_end]);
            i = run_end;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

enum KvValue {
    Double(usize), // end index after closing quote
    Single(usize),
    Bare(usize), // end index after the bare value
}

// At `run_end` (just past the key), match `\s*=\s*` then a value. Returns the end
// index of the `\s*=\s*` group and the parsed value.
fn kv_value(b: &[u8], run_end: usize) -> Option<(usize, KvValue)> {
    let mut p = run_end;
    while p < b.len() && is_ws(b[p]) {
        p += 1;
    }
    if p >= b.len() || b[p] != b'=' {
        return None;
    }
    p += 1;
    while p < b.len() && is_ws(b[p]) {
        p += 1;
    }
    let eq_end = p;
    if p < b.len() && b[p] == b'"' {
        let mut q = p + 1;
        while q < b.len() && b[q] != b'"' && b[q] != b'\r' && b[q] != b'\n' {
            q += 1;
        }
        if q < b.len() && b[q] == b'"' {
            return Some((eq_end, KvValue::Double(q + 1)));
        }
        // unterminated quote: fall through to the bare-value alternative
    }
    if p < b.len() && b[p] == b'\'' {
        let mut q = p + 1;
        while q < b.len() && b[q] != b'\'' && b[q] != b'\r' && b[q] != b'\n' {
            q += 1;
        }
        if q < b.len() && b[q] == b'\'' {
            return Some((eq_end, KvValue::Single(q + 1)));
        }
    }
    // bare value: [^\s]* (zero or more non-whitespace), may be empty
    let mut q = p;
    while q < b.len() && !is_ws(b[q]) {
        q += 1;
    }
    Some((eq_end, KvValue::Bare(q)))
}

// Does `run` (a [\w.-] slice) contain a secret keyword as a substring?
fn run_contains_secret_key(run: &[u8]) -> bool {
    let n = run.len();
    for start in 0..n {
        if secret_key_at(run, start) {
            return true;
        }
    }
    false
}

fn secret_key_at(run: &[u8], at: usize) -> bool {
    // simple alternatives
    for lit in [
        &b"SECRET"[..],
        &b"TOKEN"[..],
        &b"PASSWORD"[..],
        &b"PASSWD"[..],
        &b"CREDENTIAL"[..],
    ] {
        if eq_ci(run, at, lit) {
            return true;
        }
    }
    // API[_-]?KEY, PRIVATE[_-]?KEY, ACCESS[_-]?KEY
    for prefix in [&b"API"[..], &b"PRIVATE"[..], &b"ACCESS"[..]] {
        if eq_ci(run, at, prefix) {
            let mut p = at + prefix.len();
            if p < run.len() && (run[p] == b'_' || run[p] == b'-') {
                p += 1;
            }
            if eq_ci(run, p, b"KEY") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redacts_an_aws_access_key_id() {
        assert_eq!(redact("key AKIAIOSFODNN7EXAMPLE here"), "key [REDACTED] here");
    }

    #[test]
    fn redacts_a_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        assert_eq!(redact(&format!("token: {jwt}")), "token: [REDACTED]");
    }

    #[test]
    fn redacts_only_authorization_value_keeping_scheme() {
        assert_eq!(
            redact("Authorization: Bearer abc123def456"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            redact("Authorization: Basic dXNlcjpwYXNz"),
            "Authorization: Basic [REDACTED]"
        );
    }

    #[test]
    fn redacts_a_multiline_pem_block_as_a_single_token() {
        let pem = [
            "-----BEGIN RSA PRIVATE KEY-----",
            "MIIEowIBAAKCAQEA0Z3VS5JJcds3xfn/ygWyF3SXnUgtMcKfDM3oqXBwM3uJ4uJ",
            "aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789+/abcdefghijklmnopqrstuvwxyz",
            "-----END RSA PRIVATE KEY-----",
        ]
        .join("\n");
        assert_eq!(redact(&format!("my key:\n{pem}\ndone")), "my key:\n[REDACTED]\ndone");
    }

    #[test]
    fn redacts_secret_ish_key_value_keeping_key_name() {
        assert_eq!(redact("SECRET=supersecret"), "SECRET=[REDACTED]");
        assert_eq!(redact("API_KEY = \"abc123\""), "API_KEY = \"[REDACTED]\"");
        assert_eq!(redact("DB_PASSWORD='hunter2'"), "DB_PASSWORD='[REDACTED]'");
        assert_eq!(
            redact("AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE"),
            "AWS_ACCESS_KEY_ID=[REDACTED]"
        );
    }

    #[test]
    fn leaves_ordinary_text_and_non_secret_key_value_untouched() {
        let clean = "NODE_ENV=production\nPORT=3000\njust some prose, paths/like/this, and code.";
        assert_eq!(redact(clean), clean);
    }

    #[test]
    fn is_idempotent() {
        let blob = [
            "AKIAIOSFODNN7EXAMPLE",
            "Authorization: Bearer abc.def.ghi",
            "SECRET=\"topsecret\"",
            "PORT=3000",
        ]
        .join("\n");
        let once = redact(&blob);
        assert_eq!(redact(&once), once);
    }

    #[test]
    fn preserves_line_count_for_in_place_redactions() {
        let input = "SECRET=a\nPORT=3000\nTOKEN=b";
        assert_eq!(redact(input).split('\n').count(), 3);
    }

    #[test]
    fn scrubs_a_mixed_multi_secret_blob_leaving_non_secrets_intact() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.Dk5f2nL9q-7gWmVw3Yx1pQrStUvWxYz0123456789AB";
        let input = [
            "starting up on PORT=3000".to_string(),
            "aws id AKIAIOSFODNN7EXAMPLE".to_string(),
            format!("jwt {jwt}"),
            "Authorization: Bearer s3cr3t-token".to_string(),
            "DATABASE_PASSWORD=hunter2".to_string(),
            "NODE_ENV=production".to_string(),
        ]
        .join("\n");
        let out = redact(&input);
        assert_eq!(
            out,
            [
                "starting up on PORT=3000",
                "aws id [REDACTED]",
                "jwt [REDACTED]",
                "Authorization: Bearer [REDACTED]",
                "DATABASE_PASSWORD=[REDACTED]",
                "NODE_ENV=production",
            ]
            .join("\n")
        );
        assert_eq!(redact(&out), out);
    }

    #[test]
    fn never_panics_on_odd_input_and_returns_a_string() {
        assert_eq!(redact(""), "");
        // an unterminated PEM start is left as-is (no END header to match)
        let _ = redact("-----BEGIN PRIVATE KEY-----");
    }
}
