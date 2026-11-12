//! Remote endpoints for the control API (#131).
//!
//! The control server ALWAYS listens on `127.0.0.1`. Remote clients (the phone app, a desktop
//! remote-attach client) reach it through zero or more explicitly configured *remote endpoints*,
//! each its own socket. An endpoint spec is either an interface name (a VPN interface such as
//! `wg0`, or a Windows adapter's friendly name) or a specific IP literal.
//!
//! Rules this module enforces (pure, so they're unit-testable without sockets):
//! * A wildcard (`0.0.0.0` / `::`) is REFUSED, never bound: it would expose the API on every
//!   interface, LAN included. The refusal names what it would have exposed.
//! * An interface name resolves to that interface's addresses, minus loopback and link-local.
//!   A missing interface (or one without a usable address) is an error, never a fallback.
//! * A loopback literal is redundant (loopback is always served) and reported as such.
//!
//! The server side (binding, the rebind watcher, status publishing) lives in `server.rs`.

use std::net::IpAddr;

use serde::Serialize;

/// One `(interface name, address)` row of the host's interface table. On Windows `alt_name` is
/// the adapter's permanent GUID name, which a spec may name instead of the friendly name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfaceAddr {
    pub name: String,
    pub alt_name: Option<String>,
    pub ip: IpAddr,
}

/// The current interface table, from the OS. An enumeration failure yields an empty table, which
/// surfaces as "interface not found" for name specs instead of crashing the server.
pub fn system_ifaces() -> Vec<IfaceAddr> {
    match if_addrs::get_if_addrs() {
        Ok(list) => list
            .into_iter()
            .map(|i| IfaceAddr {
                ip: i.ip(),
                #[cfg(windows)]
                alt_name: Some(i.adapter_name.clone()),
                #[cfg(not(windows))]
                alt_name: None,
                name: i.name,
            })
            .collect(),
        Err(e) => {
            eprintln!("[control] cannot enumerate network interfaces: {e}");
            Vec::new()
        }
    }
}

/// What one spec resolves to: an address to bind, or why it can't be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Planned {
    Addr { spec: String, ip: IpAddr },
    Failed { spec: String, error: String },
}

/// Per-endpoint status, published on `Shared`, in `control.json` and in the Prefs panel. Never
/// carries a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointStatus {
    /// The configured spec (interface name or IP), verbatim.
    pub spec: String,
    /// The resolved address (no port), when there is one.
    pub address: Option<String>,
    pub bound: bool,
    pub error: Option<String>,
}

impl EndpointStatus {
    /// One-line human form, e.g. `wg0 → 100.64.0.1:41419 ✓` / `wg0 ✗ interface not found`.
    pub fn summary(&self, port: u16) -> String {
        match (&self.address, self.bound, &self.error) {
            (Some(a), true, _) => format!("{} → {}:{port} ✓", self.spec, bracket(a)),
            (Some(a), false, Some(e)) => format!("{} → {} ✗ {e}", self.spec, bracket(a)),
            (_, _, Some(e)) => format!("{} ✗ {e}", self.spec),
            _ => format!("{} ✗", self.spec),
        }
    }
}

/// Bracket an IPv6 literal for use in a URL authority.
pub fn bracket(addr: &str) -> String {
    if addr.contains(':') {
        format!("[{addr}]")
    } else {
        addr.to_string()
    }
}

fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        // fe80::/10
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

fn usable(ip: &IpAddr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && !is_link_local(ip)
}

fn name_matches(spec: &str, row: &IfaceAddr) -> bool {
    if cfg!(windows) {
        row.name.eq_ignore_ascii_case(spec)
            || row
                .alt_name
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(spec))
    } else {
        row.name == spec
    }
}

/// Everything a wildcard bind would have exposed: `name=ip` for each non-loopback address.
pub fn wildcard_exposure(table: &[IfaceAddr]) -> String {
    let mut seen: Vec<String> = table
        .iter()
        .filter(|r| !r.ip.is_loopback())
        .map(|r| format!("{}={}", r.name, r.ip))
        .collect();
    seen.dedup();
    if seen.is_empty() {
        "no non-loopback interfaces".to_string()
    } else {
        seen.join(", ")
    }
}

/// Resolve configured specs against an interface table. Order follows `specs`; duplicate
/// addresses (two specs naming the same IP) are planned once, under the first spec.
pub fn plan(specs: &[String], table: &[IfaceAddr]) -> Vec<Planned> {
    let mut out: Vec<Planned> = Vec::new();
    let mut taken: Vec<IpAddr> = Vec::new();
    for spec in specs {
        let spec_s = spec.trim();
        if spec_s.is_empty() {
            continue;
        }
        if let Ok(ip) = spec_s.parse::<IpAddr>() {
            if ip.is_unspecified() {
                out.push(Planned::Failed {
                    spec: spec_s.to_string(),
                    error: format!(
                        "wildcard bind refused (exposes: {})",
                        wildcard_exposure(table)
                    ),
                });
            } else if ip.is_loopback() {
                out.push(Planned::Failed {
                    spec: spec_s.to_string(),
                    error: "loopback is always served; not a remote endpoint".to_string(),
                });
            } else if !taken.contains(&ip) {
                taken.push(ip);
                out.push(Planned::Addr {
                    spec: spec_s.to_string(),
                    ip,
                });
            }
            continue;
        }
        let rows: Vec<&IfaceAddr> = table.iter().filter(|r| name_matches(spec_s, r)).collect();
        if rows.is_empty() {
            out.push(Planned::Failed {
                spec: spec_s.to_string(),
                error: "interface not found".to_string(),
            });
            continue;
        }
        let mut any = false;
        for r in rows.into_iter().filter(|r| usable(&r.ip)) {
            any = true;
            if !taken.contains(&r.ip) {
                taken.push(r.ip);
                out.push(Planned::Addr {
                    spec: spec_s.to_string(),
                    ip: r.ip,
                });
            }
        }
        if !any {
            out.push(Planned::Failed {
                spec: spec_s.to_string(),
                error: "interface has no usable address".to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, ip: &str) -> IfaceAddr {
        IfaceAddr {
            name: name.into(),
            alt_name: None,
            ip: ip.parse().unwrap(),
        }
    }

    fn table() -> Vec<IfaceAddr> {
        vec![
            row("lo", "127.0.0.1"),
            row("lo", "::1"),
            row("eth0", "192.0.2.10"),
            row("eth0", "fe80::1"),
            row("wg0", "100.64.0.1"),
            row("wg0", "fd7a::1"),
            row("wg0", "fe80::2"),
            row("ll0", "169.254.3.4"),
        ]
    }

    fn specs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn interface_resolves_to_usable_addresses_only() {
        let p = plan(&specs(&["wg0"]), &table());
        assert_eq!(
            p,
            vec![
                Planned::Addr {
                    spec: "wg0".into(),
                    ip: "100.64.0.1".parse().unwrap()
                },
                Planned::Addr {
                    spec: "wg0".into(),
                    ip: "fd7a::1".parse().unwrap()
                },
            ]
        );
    }

    #[test]
    fn missing_interface_is_an_error_not_a_fallback() {
        let p = plan(&specs(&["tun9"]), &table());
        assert_eq!(
            p,
            vec![Planned::Failed {
                spec: "tun9".into(),
                error: "interface not found".into()
            }]
        );
    }

    #[test]
    fn link_local_only_interface_has_no_usable_address() {
        let p = plan(&specs(&["ll0"]), &table());
        assert!(matches!(&p[0], Planned::Failed { error, .. } if error.contains("no usable")));
    }

    #[test]
    fn wildcards_are_refused_and_name_the_exposure() {
        for w in ["0.0.0.0", "::"] {
            let p = plan(&specs(&[w]), &table());
            let Planned::Failed { error, .. } = &p[0] else {
                panic!("wildcard {w} was planned: {p:?}");
            };
            assert!(error.starts_with("wildcard bind refused"), "{error}");
            assert!(error.contains("eth0=192.0.2.10"), "{error}");
            assert!(error.contains("wg0=100.64.0.1"), "{error}");
            assert!(!error.contains("127.0.0.1"), "{error}");
        }
    }

    #[test]
    fn ip_literals_pass_through_and_dedupe() {
        let p = plan(&specs(&["100.64.0.1", "wg0", "203.0.113.7"]), &table());
        let addrs: Vec<String> = p
            .iter()
            .filter_map(|x| match x {
                Planned::Addr { ip, .. } => Some(ip.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(addrs, vec!["100.64.0.1", "fd7a::1", "203.0.113.7"]);
    }

    #[test]
    fn loopback_literal_is_reported_redundant() {
        let p = plan(&specs(&["127.0.0.1"]), &table());
        assert!(matches!(&p[0], Planned::Failed { error, .. } if error.contains("always served")));
    }

    #[test]
    fn an_address_change_changes_the_plan() {
        let before = plan(&specs(&["wg0"]), &table());
        let mut t2 = table();
        t2.retain(|r| r.ip.to_string() != "100.64.0.1");
        t2.push(row("wg0", "100.64.0.9"));
        let after = plan(&specs(&["wg0"]), &t2);
        assert_ne!(before, after);
        assert!(after.contains(&Planned::Addr {
            spec: "wg0".into(),
            ip: "100.64.0.9".parse().unwrap()
        }));
        assert!(!after.contains(&Planned::Addr {
            spec: "wg0".into(),
            ip: "100.64.0.1".parse().unwrap()
        }));
    }

    #[test]
    fn summary_lines() {
        let ok = EndpointStatus {
            spec: "wg0".into(),
            address: Some("fd7a::1".into()),
            bound: true,
            error: None,
        };
        assert_eq!(ok.summary(41419), "wg0 → [fd7a::1]:41419 ✓");
        let bad = EndpointStatus {
            spec: "wg0".into(),
            address: None,
            bound: false,
            error: Some("interface not found".into()),
        };
        assert_eq!(bad.summary(41419), "wg0 ✗ interface not found");
    }
}
