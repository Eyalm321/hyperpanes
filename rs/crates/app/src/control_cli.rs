//! Shared plumbing for the CLI subcommands that drive a running control server as a LOCAL client
//! over HTTP — `pair` (mint a device token), `devices` (list), `revoke` (drop one). Each reads
//! `control.json` for the port + master token, figures the address the server actually listens on
//! (a specific `bindAddress` isn't reachable via loopback — the server binds a single socket), and
//! hands back a blocking `reqwest` client primed with the master bearer.

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use hyperpanes_core::persistence::{control_settings, paths};

/// A live connection to the local control server: `base` URL, the master `token`, the listen
/// `port`, and a blocking HTTP client.
pub struct Conn {
    pub base: String,
    pub token: String,
    pub client: reqwest::blocking::Client,
}

fn io_err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Read `{ port, token }` from `control.json` (honouring `HYPERPANES_CONTROL_FILE`, treating an
/// empty value as unset like the rest of the CLI), resolve the reachable base URL, and build the
/// client. Errors when no control API is running / the file is unreadable.
pub fn connect() -> io::Result<Conn> {
    let control_file = std::env::var_os("HYPERPANES_CONTROL_FILE")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(paths::control_json);
    let raw = std::fs::read_to_string(&control_file).map_err(|e| {
        io_err(format!(
            "no running control API ({}): {e}.\nStart hyperpanes and enable Preferences → Control API.",
            control_file.display()
        ))
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| io_err(format!("control.json parse error: {e}")))?;
    let port = v
        .get("port")
        .and_then(|p| p.as_u64())
        .filter(|&p| (1..=65535).contains(&p))
        .ok_or_else(|| io_err("control.json is missing a valid port"))? as u16;
    let token = v
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| io_err("control.json is missing a token"))?
        .to_string();

    let settings = control_settings::load();
    let base = base_url(port, settings.bind_address.as_deref());
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| io_err(format!("http client: {e}")))?;
    Ok(Conn {
        base,
        token,
        client,
    })
}

/// Base URL for LOCAL calls to the control server. A configured, specific `bindAddress` is the
/// only address the server listens on (single-socket bind), so loopback would refuse — dial it
/// directly. An unspecified bind (`0.0.0.0`/`::`) or none means loopback works.
pub fn base_url(port: u16, bind_address: Option<&str>) -> String {
    let host = match bind_address.and_then(|a| a.parse::<IpAddr>().ok()) {
        Some(ip) if !ip.is_unspecified() => bracket(&ip.to_string()),
        _ => "127.0.0.1".to_string(),
    };
    format!("http://{host}:{port}")
}

fn bracket(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// A sensible default label for a freshly-paired device: the machine's hostname (so `hyperpanes
/// devices` reads meaningfully), falling back to `device` when it can't be determined.
pub fn default_device_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".to_string())
}

/// Parse a TTL spec into milliseconds: a bare integer is milliseconds; a `s`/`m`/`h`/`d` suffix is
/// seconds/minutes/hours/days. Returns `Err` on anything unparseable so a typo can't silently mint
/// a never-expiring token.
pub fn parse_ttl_ms(spec: &str) -> Result<i64, String> {
    let spec = spec.trim();
    let (num, mult) = match spec.chars().last() {
        Some('s') => (&spec[..spec.len() - 1], 1_000),
        Some('m') => (&spec[..spec.len() - 1], 60_000),
        Some('h') => (&spec[..spec.len() - 1], 3_600_000),
        Some('d') => (&spec[..spec.len() - 1], 86_400_000),
        _ => (spec, 1),
    };
    let n: i64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid --ttl '{spec}' (use e.g. 30d, 12h, 90m, or a ms integer)"))?;
    if n <= 0 {
        return Err(format!("invalid --ttl '{spec}': must be positive"));
    }
    n.checked_mul(mult)
        .ok_or_else(|| format!("--ttl '{spec}' overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_specific_bind_is_dialed_directly() {
        assert_eq!(
            base_url(41419, Some("100.120.216.17")),
            "http://100.120.216.17:41419"
        );
        assert_eq!(base_url(41419, Some("fd7a::1")), "http://[fd7a::1]:41419");
    }

    #[test]
    fn base_url_unspecified_or_none_is_loopback() {
        assert_eq!(base_url(8080, Some("0.0.0.0")), "http://127.0.0.1:8080");
        assert_eq!(base_url(8080, Some("::")), "http://127.0.0.1:8080");
        assert_eq!(base_url(8080, None), "http://127.0.0.1:8080");
        assert_eq!(base_url(8080, Some("garbage")), "http://127.0.0.1:8080");
    }

    #[test]
    fn ttl_parsing_covers_units_and_rejects_junk() {
        assert_eq!(parse_ttl_ms("500"), Ok(500));
        assert_eq!(parse_ttl_ms("30s"), Ok(30_000));
        assert_eq!(parse_ttl_ms("90m"), Ok(5_400_000));
        assert_eq!(parse_ttl_ms("12h"), Ok(43_200_000));
        assert_eq!(parse_ttl_ms("30d"), Ok(2_592_000_000));
        assert!(parse_ttl_ms("0").is_err());
        assert!(parse_ttl_ms("-5m").is_err());
        assert!(parse_ttl_ms("soon").is_err());
    }
}
