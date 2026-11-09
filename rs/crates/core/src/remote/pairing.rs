//! Pairing-target parsing: `hp://host:port/?token=T&v=1` and the other spellings a user may
//! paste. The token is a SECRET — `Debug`/`Display` redact it; only [`RemoteTarget::pairing_url`]
//! and [`RemoteTarget::token`] expose it (for persistence / the wire).

use std::fmt;

/// Where a remote host's control server lives and the bearer token that opens it.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteTarget {
    /// Bare host (no brackets for IPv6 literals).
    pub host: String,
    pub port: u16,
    token: String,
}

/// Why a pairing string was rejected. Messages never include the input (it may hold a token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    Empty,
    /// Only `hp`, `http` and `ws` are accepted (no TLS: `https`/`wss` are rejected on purpose).
    UnsupportedScheme(String),
    MissingHost,
    MissingPort,
    InvalidPort,
    MissingToken,
    InvalidHost,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairingError::Empty => write!(f, "empty pairing string"),
            PairingError::UnsupportedScheme(s) => {
                write!(f, "unsupported scheme `{s}` (expected hp, http or ws)")
            }
            PairingError::MissingHost => write!(f, "pairing string has no host"),
            PairingError::MissingPort => write!(f, "pairing string has no port"),
            PairingError::InvalidPort => write!(f, "invalid port"),
            PairingError::MissingToken => write!(f, "pairing string has no token"),
            PairingError::InvalidHost => write!(f, "invalid host"),
        }
    }
}

impl std::error::Error for PairingError {}

impl RemoteTarget {
    /// Build from parts (host may be a bracketed IPv6 literal).
    pub fn new(
        host: impl Into<String>,
        port: u16,
        token: impl Into<String>,
    ) -> Result<Self, PairingError> {
        let host = host.into();
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&host)
            .to_string();
        validate_host(&host)?;
        let token = token.into();
        if token.trim().is_empty() {
            return Err(PairingError::MissingToken);
        }
        Ok(Self {
            host,
            port,
            token: token.trim().to_string(),
        })
    }

    /// Parse a pairing string that carries its own token.
    pub fn parse(input: &str) -> Result<Self, PairingError> {
        Self::parse_inner(input, None)
    }

    /// Parse `input`, with an explicitly supplied `token` (e.g. typed into a form next to a bare
    /// `host:port`). An explicit non-empty token wins over one embedded in the string.
    pub fn parse_with_token(input: &str, token: &str) -> Result<Self, PairingError> {
        Self::parse_inner(input, Some(token))
    }

    fn parse_inner(input: &str, explicit: Option<&str>) -> Result<Self, PairingError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(PairingError::Empty);
        }
        let rest = match input.split_once("://") {
            Some((scheme, rest)) => {
                let scheme = scheme.to_ascii_lowercase();
                if !matches!(scheme.as_str(), "hp" | "http" | "ws") {
                    return Err(PairingError::UnsupportedScheme(scheme));
                }
                rest
            }
            None => input,
        };
        // authority ends at the first / ? #
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let (before_frag, fragment) = match tail.split_once('#') {
            Some((b, f)) => (b, Some(f)),
            None => (tail, None),
        };
        let query = before_frag.split_once('?').map(|(_, q)| q);

        let (host, port) = split_host_port(authority)?;

        let embedded = query
            .and_then(|q| query_param(q, "token"))
            .or_else(|| fragment.and_then(fragment_token))
            .filter(|t| !t.trim().is_empty());
        let token = match explicit.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => t.to_string(),
            None => embedded.ok_or(PairingError::MissingToken)?,
        };
        Self::new(host, port, token)
    }

    /// The bearer token. Treat as a secret: never log it.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// `host:port` with IPv6 literals bracketed.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// `http://host:port` (no trailing slash, no token).
    pub fn http_base(&self) -> String {
        format!("http://{}", self.authority())
    }

    /// `ws://host:port/events?token=…` — INCLUDES the secret; build it at the last moment and
    /// never log it.
    pub fn events_url(&self) -> String {
        format!(
            "ws://{}/events?token={}",
            self.authority(),
            pct_encode(&self.token)
        )
    }

    /// The canonical pairing URL INCLUDING the token, for persistence (same shape as
    /// `hyperpanes pair` prints). Treat as a secret.
    pub fn pairing_url(&self) -> String {
        format!(
            "hp://{}/?token={}&v=1",
            self.authority(),
            pct_encode(&self.token)
        )
    }

    /// Same as [`pairing_url`](Self::pairing_url) with the token masked — safe to show/log.
    pub fn redacted_url(&self) -> String {
        format!("hp://{}/?token=***", self.authority())
    }
}

impl fmt::Debug for RemoteTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &"***")
            .finish()
    }
}

impl fmt::Display for RemoteTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted_url())
    }
}

fn validate_host(host: &str) -> Result<(), PairingError> {
    if host.is_empty() {
        return Err(PairingError::MissingHost);
    }
    if host.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '/' | '?' | '#' | '@' | '[' | ']')
    }) {
        return Err(PairingError::InvalidHost);
    }
    Ok(())
}

fn split_host_port(authority: &str) -> Result<(String, u16), PairingError> {
    if authority.is_empty() {
        return Err(PairingError::MissingHost);
    }
    let (host, port_str) = if let Some(rest) = authority.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or(PairingError::InvalidHost)?;
        let p = match after {
            "" => None,
            a => Some(a.strip_prefix(':').ok_or(PairingError::InvalidHost)?),
        };
        (h.to_string(), p)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => {
                if h.contains(':') {
                    // unbracketed IPv6 — ambiguous port; refuse
                    return Err(PairingError::InvalidHost);
                }
                (h.to_string(), Some(p))
            }
            None => (authority.to_string(), None),
        }
    };
    validate_host(&host)?;
    let port_str = port_str.ok_or(PairingError::MissingPort)?;
    let port: u16 = port_str.parse().map_err(|_| PairingError::InvalidPort)?;
    if port == 0 {
        return Err(PairingError::InvalidPort);
    }
    Ok((host, port))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| pct_decode(v))
    })
}

/// `#T` or `#token=T` (optionally followed by `&v=1`).
fn fragment_token(frag: &str) -> Option<String> {
    if let Some(t) = query_param(frag, "token") {
        return Some(t);
    }
    let first = frag.split('&').next().unwrap_or("");
    if first.is_empty() || first.contains('=') {
        None
    } else {
        Some(pct_decode(first))
    }
}

pub(crate) fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Percent-encode everything outside the RFC 3986 unreserved set.
pub(crate) fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> RemoteTarget {
        RemoteTarget::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn hp_query_form() {
        let t = ok("hp://100.64.0.1:51888/?token=tok123&v=1");
        assert_eq!(t.host, "100.64.0.1");
        assert_eq!(t.port, 51888);
        assert_eq!(t.token(), "tok123");
        assert_eq!(t.http_base(), "http://100.64.0.1:51888");
        assert_eq!(t.events_url(), "ws://100.64.0.1:51888/events?token=tok123");
    }

    #[test]
    fn hp_fragment_forms() {
        assert_eq!(ok("hp://example-host:1234/#abc").token(), "abc");
        assert_eq!(ok("hp://example-host:1234#abc").token(), "abc");
        assert_eq!(ok("hp://example-host:1234/#token=abc").token(), "abc");
        assert_eq!(ok("hp://example-host:1234/#token=abc&v=1").token(), "abc");
    }

    #[test]
    fn http_and_ws_schemes() {
        assert_eq!(ok("http://example-host:80/?token=t").port, 80);
        let t = ok("ws://example-host:9/events?token=t");
        assert_eq!(
            (t.host.as_str(), t.port, t.token()),
            ("example-host", 9, "t")
        );
        assert_eq!(ok("HP://example-host:9/?token=t").port, 9);
    }

    #[test]
    fn ipv6_literals() {
        let t = ok("hp://[::1]:51888/?token=t&v=1");
        assert_eq!(t.host, "::1");
        assert_eq!(t.authority(), "[::1]:51888");
        assert_eq!(t.http_base(), "http://[::1]:51888");
        assert_eq!(t.pairing_url(), "hp://[::1]:51888/?token=t&v=1");
        assert_eq!(ok("[fd7a::1]:7#tk").host, "fd7a::1");
    }

    #[test]
    fn bare_host_port_with_separate_token() {
        let t = RemoteTarget::parse_with_token("example-host:4000", "sekret").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.token()),
            ("example-host", 4000, "sekret")
        );
        let t = RemoteTarget::parse_with_token("[::1]:4000", "s").unwrap();
        assert_eq!(t.host, "::1");
        // explicit token wins over an embedded one
        let t = RemoteTarget::parse_with_token("hp://h:1/?token=old", "new").unwrap();
        assert_eq!(t.token(), "new");
        // blank explicit token falls back to the embedded one
        let t = RemoteTarget::parse_with_token("hp://h:1/?token=old", "  ").unwrap();
        assert_eq!(t.token(), "old");
        // bare host:port alone has no token
        assert_eq!(
            RemoteTarget::parse("example-host:4000"),
            Err(PairingError::MissingToken)
        );
    }

    #[test]
    fn percent_encoded_token_roundtrips() {
        let t = ok("hp://h:1/?token=a%2Bb%20c&v=1");
        assert_eq!(t.token(), "a+b c");
        assert_eq!(ok(&t.pairing_url()).token(), "a+b c");
    }

    #[test]
    fn pairing_url_roundtrip() {
        let t = RemoteTarget::new("example-host", 51888, "abc123").unwrap();
        assert_eq!(t.pairing_url(), "hp://example-host:51888/?token=abc123&v=1");
        assert_eq!(ok(&t.pairing_url()), t);
    }

    #[test]
    fn rejections() {
        assert_eq!(RemoteTarget::parse("  "), Err(PairingError::Empty));
        assert_eq!(
            RemoteTarget::parse("https://h:1/?token=t"),
            Err(PairingError::UnsupportedScheme("https".into()))
        );
        assert_eq!(
            RemoteTarget::parse("wss://h:1/?token=t"),
            Err(PairingError::UnsupportedScheme("wss".into()))
        );
        assert_eq!(
            RemoteTarget::parse("hp://:1/?token=t"),
            Err(PairingError::MissingHost)
        );
        assert_eq!(
            RemoteTarget::parse("hp://h/?token=t"),
            Err(PairingError::MissingPort)
        );
        assert_eq!(
            RemoteTarget::parse("hp://h:99999/?token=t"),
            Err(PairingError::InvalidPort)
        );
        assert_eq!(
            RemoteTarget::parse("hp://h:0/?token=t"),
            Err(PairingError::InvalidPort)
        );
        assert_eq!(
            RemoteTarget::parse("hp://h:abc/?token=t"),
            Err(PairingError::InvalidPort)
        );
        assert_eq!(
            RemoteTarget::parse("hp://h:1/"),
            Err(PairingError::MissingToken)
        );
        assert_eq!(
            RemoteTarget::parse("hp://h:1/?token="),
            Err(PairingError::MissingToken)
        );
        assert_eq!(
            RemoteTarget::parse("hp://h:1/#"),
            Err(PairingError::MissingToken)
        );
        assert_eq!(
            RemoteTarget::parse("hp://::1:5/?token=t"),
            Err(PairingError::InvalidHost)
        );
        assert_eq!(
            RemoteTarget::parse("hp://[::1/?token=t"),
            Err(PairingError::InvalidHost)
        );
        assert_eq!(
            RemoteTarget::parse("hp://a b:1/?token=t"),
            Err(PairingError::InvalidHost)
        );
    }

    #[test]
    fn token_is_redacted_in_debug_and_display() {
        let t = ok("hp://example-host:1/?token=supersecret&v=1");
        let dbg = format!("{t:?}");
        let disp = format!("{t}");
        assert!(!dbg.contains("supersecret"), "{dbg}");
        assert!(!disp.contains("supersecret"), "{disp}");
        assert_eq!(disp, "hp://example-host:1/?token=***");
        assert!(dbg.contains("***"));
        assert!(t.pairing_url().contains("supersecret"));
        // errors never echo the input
        let e = RemoteTarget::parse("hp://h:zzz/?token=supersecret").unwrap_err();
        assert!(!format!("{e} {e:?}").contains("supersecret"));
    }
}
