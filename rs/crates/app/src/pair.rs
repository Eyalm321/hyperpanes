//! `hyperpanes pair` — print pairing info for the mobile app (docs/mobile-client-plan.md).
//!
//! Reads the running app's `control.json` discovery file (port + master token), figures
//! out which addresses a phone could reach (the configured `bindAddress`, else the
//! machine's default-route + Tailscale IPs via the connected-UDP-socket trick — no
//! packets are sent), and prints `hp://<host>:<port>/?token=<token>` pairing URLs plus a
//! scannable terminal QR code for the best candidate.
//!
//! Remote reachability requires `bindAddress` in `control-settings.json` (the server
//! binds loopback-only by default); when it's missing we still print the URLs but warn
//! that only this machine can connect.

use std::net::UdpSocket;
use std::path::Path;

use hyperpanes_core::persistence::{control_settings, paths};

pub fn wants_pair(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "pair").unwrap_or(false)
}

pub fn run() -> std::io::Result<()> {
    // Panes inherit HYPERPANES_CONTROL_FILE set-but-EMPTY from the app; treat empty as
    // unset or `pair` run inside a pane resolves a blank path instead of the state dir.
    let control_file = std::env::var_os("HYPERPANES_CONTROL_FILE")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(paths::control_json);
    let Some((port, token)) = read_discovery(&control_file) else {
        eprintln!(
            "No running control API found ({}).\n\
             Start hyperpanes and enable Preferences → Control API, then re-run `hyperpanes pair`.",
            control_file.display()
        );
        std::process::exit(1);
    };

    let settings = control_settings::load();
    let bound_remote = settings.bind_address.is_some();
    let hosts = candidate_hosts(settings.bind_address.as_deref());

    println!("hyperpanes pairing — control API on port {port}\n");
    if !bound_remote {
        println!(
            "⚠ control server is bound to 127.0.0.1 (loopback only) — a phone CANNOT connect yet.\n\
             Add a bind address to {}:\n\
             {{ \"enabled\": true, \"allowInput\": true, \"bindAddress\": \"<this machine's Tailscale/LAN IP>\", \"port\": {port} }}\n\
             then toggle Preferences → Control API (or restart), and re-run `hyperpanes pair`.\n\
             Prefer a Tailscale IP (100.x.y.z): WireGuard-encrypted, no open LAN ports.\n",
            paths::control_settings_json().display()
        );
    }
    if !settings.allow_input {
        println!("⚠ allowInput is off — the mobile app will be read-only (no typing/keys).\n");
    }

    let urls: Vec<String> = hosts
        .iter()
        .map(|h| pairing_url(h, port, &token))
        .collect();
    for u in &urls {
        println!("  {u}");
    }
    if let Some(best) = urls.first() {
        println!("\nScan with the hyperpanes mobile app:\n");
        match qr_text(best) {
            Some(qr) => println!("{qr}"),
            None => println!("(QR render failed — paste the URL manually)"),
        }
    }
    Ok(())
}

/// `{ port, token }` from control.json, or `None` when missing/corrupt.
fn read_discovery(path: &Path) -> Option<(u16, String)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let port = v.get("port")?.as_u64().filter(|&p| (1..=65535).contains(&p))? as u16;
    let token = v.get("token")?.as_str()?.to_string();
    Some((port, token))
}

/// The pairing URL the mobile app parses (keep in sync with `mobile/…/pairing.dart`).
fn pairing_url(host: &str, port: u16, token: &str) -> String {
    let h = if host.contains(':') {
        format!("[{host}]") // IPv6 literal
    } else {
        host.to_string()
    };
    format!("hp://{h}:{port}/?token={token}&v=1")
}

/// Addresses a phone could dial, best first. A configured SPECIFIC bind address wins
/// outright (that's the only address the server listens on); an unspecified bind
/// (`0.0.0.0`) or no config falls back to discovering this machine's Tailscale +
/// default-route IPs.
fn candidate_hosts(bind_address: Option<&str>) -> Vec<String> {
    if let Some(addr) = bind_address {
        if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
            if !ip.is_unspecified() {
                return vec![addr.to_string()];
            }
        }
    }
    let mut out = Vec::new();
    // Tailscale first (encrypted path; 100.100.100.100 is the tailnet's MagicDNS resolver,
    // so the OS routes this via the tailscale interface). connect() sends nothing.
    if let Some(ip) = local_ip_toward("100.100.100.100:53") {
        if ip.starts_with("100.") {
            out.push(ip);
        }
    }
    // Default-route LAN IP.
    if let Some(ip) = local_ip_toward("8.8.8.8:53") {
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    if out.is_empty() {
        out.push("127.0.0.1".to_string());
    }
    out
}

/// The local address the OS would use to reach `target` — connected-UDP trick, no I/O.
fn local_ip_toward(target: &str) -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(target).ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

/// Render `data` as a terminal QR (quiet zone + half-block cells, dark-on-light).
fn qr_text(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .dark_color(qrcode::render::unicode::Dense1x2::Dark)
            .light_color(qrcode::render::unicode::Dense1x2::Light)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_url_shape() {
        assert_eq!(
            pairing_url("100.71.2.9", 51888, "tok123"),
            "hp://100.71.2.9:51888/?token=tok123&v=1"
        );
        // IPv6 hosts get bracketed so port parsing stays unambiguous.
        assert_eq!(
            pairing_url("fd7a::1", 51888, "t"),
            "hp://[fd7a::1]:51888/?token=t&v=1"
        );
    }

    #[test]
    fn discovery_parses_port_and_token() {
        let dir = std::env::temp_dir().join(format!("hp-pair-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("control.json");
        std::fs::write(
            &p,
            br#"{ "port": 51888, "token": "abc", "pid": 1, "version": "x", "events": "ws://..." }"#,
        )
        .unwrap();
        assert_eq!(read_discovery(&p), Some((51888, "abc".to_string())));
        std::fs::write(&p, b"not json").unwrap();
        assert_eq!(read_discovery(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn specific_bind_address_wins() {
        assert_eq!(candidate_hosts(Some("100.71.2.9")), vec!["100.71.2.9"]);
        // Unspecified bind → discovery path (non-empty, never contains 0.0.0.0).
        let hosts = candidate_hosts(Some("0.0.0.0"));
        assert!(!hosts.is_empty());
        assert!(hosts.iter().all(|h| h != "0.0.0.0"));
    }

    #[test]
    fn qr_renders() {
        assert!(qr_text("hp://100.71.2.9:51888/?token=t&v=1").is_some());
    }
}
