//! Control-server settings persistence — `{ enabled, allowInput }` in
//! `control-settings.json` under the userData dir (both default `false`). Atomic write.
//!
//! Port of the `loadSettings` / `saveSettings` pair in `src/main/control-server.ts`.
//! Loading mirrors the TS coercion exactly: any read/parse error (missing or corrupt
//! file) yields the defaults, and each field is `true` ONLY when the JSON value is the
//! boolean `true` (TS `parsed.enabled === true`) — a missing key, `null`, or a
//! non-boolean value all coerce to `false`.
//!
//! Remote-access additions (mobile client): optional `bindAddress` + `port`. Both are
//! omitted from the file when unset, so a default config stays byte-identical to the
//! legacy `{ enabled, allowInput }` shape. Coercion is equally forgiving: `bindAddress`
//! must be a non-empty string that parses as an IP address, `port` an integer in
//! 1..=65535 — anything else coerces to "unset" (loopback / ephemeral).

use crate::persistence::paths;
use serde::{Deserialize, Serialize};

/// Control-server settings: whether the local control API is enabled, whether it
/// accepts input (send_input/send_keys) from clients, and — for remote clients like
/// the mobile app — which address/port to bind (`None` = `127.0.0.1` / ephemeral).
///
/// `Default` (DEFAULT_SETTINGS in control-server.ts): control API off, input disallowed,
/// loopback-only on an ephemeral port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ControlSettings {
    pub enabled: bool,
    pub allow_input: bool,
    /// Bind address for the control server. `None` → `127.0.0.1`. Set to the host's
    /// Tailscale/LAN IP (or `0.0.0.0`) to allow remote clients.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,
    /// Fixed listen port. `None` → ephemeral (OS-assigned), the legacy behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// Read the settings from the canonical `control-settings.json`.
pub fn load() -> ControlSettings {
    load_from(&paths::control_settings_json())
}

/// Read the settings from `path`, returning the defaults on any error — exactly the
/// TS `try { … } catch { return { ...DEFAULT_SETTINGS } }` behaviour.
pub fn load_from(path: &std::path::Path) -> ControlSettings {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return ControlSettings::default();
    };
    // Parse to a generic Value so a non-boolean field coerces to `false` (matching
    // `=== true`) rather than failing the whole parse the way a typed bool would.
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return ControlSettings::default();
    };
    ControlSettings {
        enabled: value.get("enabled").and_then(|v| v.as_bool()) == Some(true),
        allow_input: value.get("allowInput").and_then(|v| v.as_bool()) == Some(true),
        bind_address: value
            .get("bindAddress")
            .and_then(|v| v.as_str())
            .filter(|s| s.parse::<std::net::IpAddr>().is_ok())
            .map(str::to_string),
        port: value
            .get("port")
            .and_then(|v| v.as_u64())
            .filter(|&p| (1..=65535).contains(&p))
            .map(|p| p as u16),
    }
}

/// Persist the settings to the canonical `control-settings.json` (atomic).
pub fn save(settings: &ControlSettings) -> std::io::Result<()> {
    save_to(&paths::control_settings_json(), settings)
}

/// Persist the settings to `path`, atomically. The on-disk shape matches
/// `JSON.stringify(this.settings, null, 2)`: `{ "enabled": …, "allowInput": … }`,
/// pretty-printed with 2-space indent.
pub fn save_to(path: &std::path::Path, settings: &ControlSettings) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    paths::write_atomic(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hp-control-settings-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_yields_defaults() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_from(&p), ControlSettings::default());
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let p = temp_path("corrupt");
        std::fs::write(&p, b"not json {").unwrap();
        assert_eq!(load_from(&p), ControlSettings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn only_boolean_true_enables() {
        let p = temp_path("coerce");
        // Non-boolean / null / missing all coerce to false; only literal true counts.
        std::fs::write(&p, br#"{ "enabled": "yes", "allowInput": null }"#).unwrap();
        assert_eq!(load_from(&p), ControlSettings::default());
        std::fs::write(&p, br#"{ "enabled": true }"#).unwrap();
        assert_eq!(
            load_from(&p),
            ControlSettings {
                enabled: true,
                ..Default::default()
            }
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_then_load_round_trips() {
        let p = temp_path("roundtrip");
        let settings = ControlSettings {
            enabled: true,
            allow_input: true,
            ..Default::default()
        };
        save_to(&p, &settings).unwrap();
        assert_eq!(load_from(&p), settings);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn saved_shape_is_camel_case_pretty() {
        let p = temp_path("shape");
        save_to(
            &p,
            &ControlSettings {
                enabled: false,
                allow_input: true,
                ..Default::default()
            },
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(&p).unwrap();
        // Unset bind/port are OMITTED — the legacy two-field shape byte-for-byte.
        assert_eq!(
            on_disk,
            "{\n  \"enabled\": false,\n  \"allowInput\": true\n}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bind_and_port_round_trip() {
        let p = temp_path("bind-roundtrip");
        let settings = ControlSettings {
            enabled: true,
            allow_input: true,
            bind_address: Some("100.71.2.9".into()),
            port: Some(51888),
        };
        save_to(&p, &settings).unwrap();
        assert_eq!(load_from(&p), settings);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn invalid_bind_or_port_coerce_to_unset() {
        let p = temp_path("bind-coerce");
        // Not an IP / out-of-range port / wrong types → both unset.
        std::fs::write(
            &p,
            br#"{ "enabled": true, "bindAddress": "example.com", "port": 700000 }"#,
        )
        .unwrap();
        let s = load_from(&p);
        assert_eq!(s.bind_address, None);
        assert_eq!(s.port, None);
        std::fs::write(&p, br#"{ "bindAddress": "", "port": "8080" }"#).unwrap();
        let s = load_from(&p);
        assert_eq!(s.bind_address, None);
        assert_eq!(s.port, None);
        // Port 0 means "ephemeral" and is treated as unset, not kept as literal 0.
        std::fs::write(&p, br#"{ "bindAddress": "0.0.0.0", "port": 0 }"#).unwrap();
        let s = load_from(&p);
        assert_eq!(s.bind_address.as_deref(), Some("0.0.0.0"));
        assert_eq!(s.port, None);
        let _ = std::fs::remove_file(&p);
    }
}
