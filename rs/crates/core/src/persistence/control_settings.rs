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
//!
//! #131: optional `remoteEndpoints: [string]` — interface names (a VPN interface) or specific
//! IPs, each served on its own socket IN ADDITION to loopback, which is always served. Legacy
//! `bindAddress` is folded in as one more endpoint (see [`ControlSettings::remote_specs`]).
//! Wildcards are kept here (so the server can REPORT the refusal) but never bound.

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
    /// LEGACY single bind address. Now treated as one remote endpoint (merged into
    /// [`remote_specs`](Self::remote_specs)); prefer `remote_endpoints`. Loopback is always
    /// served regardless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,
    /// Fixed listen port. `None` → ephemeral (OS-assigned), the legacy behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Remote endpoints (#131): interface names or specific IPs, each its own socket beside
    /// loopback. Empty = loopback only. Omitted from the file when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remote_endpoints: Vec<String>,
    /// Spec-pane sweeper settings (`specSweep`). `None` = not configured (all defaults);
    /// omitted from the file when unset so the legacy shape stays byte-identical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_sweep: Option<SpecSweepSettings>,
}

/// `specSweep` block of `control-settings.json`: the app-side sweeper that closes idle or
/// finished goal spec-agent panes (see `control::spec_sweep`). Every field is optional on
/// disk; a missing or garbage value takes its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecSweepSettings {
    /// Master switch (default on).
    pub enabled: bool,
    /// Close a spec pane idle this many hours with nothing in flight (default 3).
    pub idle_close_hours: u64,
    /// Close a spec pane whose goal is done once idle this many minutes (default 30).
    pub done_close_minutes: u64,
    /// Log what would be closed, close nothing (default off).
    pub dry_run: bool,
}

impl Default for SpecSweepSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_close_hours: 3,
            done_close_minutes: 30,
            dry_run: false,
        }
    }
}

impl SpecSweepSettings {
    /// Tolerant parse of the `specSweep` JSON value: anything that is not the expected type
    /// (or a non-positive number) falls back to the default for that field.
    pub fn from_value(v: Option<&serde_json::Value>) -> Self {
        let d = Self::default();
        let Some(v) = v else { return d };
        let num = |k: &str, dflt: u64| {
            v.get(k)
                .and_then(|x| x.as_u64())
                .filter(|&n| n > 0)
                .unwrap_or(dflt)
        };
        let flag = |k: &str, dflt: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(dflt);
        Self {
            enabled: flag("enabled", d.enabled),
            idle_close_hours: num("idleCloseHours", d.idle_close_hours),
            done_close_minutes: num("doneCloseMinutes", d.done_close_minutes),
            dry_run: flag("dryRun", d.dry_run),
        }
    }
}

impl ControlSettings {
    /// Every configured remote endpoint spec: `remote_endpoints`, then the legacy
    /// `bind_address` unless it's loopback, deduped (first wins). Wildcards stay in the list
    /// so the server reports their refusal instead of silently ignoring them.
    pub fn remote_specs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let legacy = self
            .bind_address
            .iter()
            .filter(|a| a.as_str() != "127.0.0.1" && a.as_str() != "::1");
        for spec in self.remote_endpoints.iter().chain(legacy) {
            let spec = spec.trim();
            if !spec.is_empty() && !out.iter().any(|s| s == spec) {
                out.push(spec.to_string());
            }
        }
        out
    }
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
        remote_endpoints: value
            .get("remoteEndpoints")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str())
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        spec_sweep: value
            .get("specSweep")
            .filter(|v| v.is_object())
            .map(|v| SpecSweepSettings::from_value(Some(v))),
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
            bind_address: Some("100.64.0.1".into()),
            port: Some(51888),
            remote_endpoints: vec!["wg0".into(), "fd7a::1".into()],
            spec_sweep: None,
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

    #[test]
    fn remote_endpoints_coerce_forgivingly() {
        let p = temp_path("remote-coerce");
        std::fs::write(
            &p,
            br#"{ "remoteEndpoints": ["wg0", 7, "", "  ", null, "Ethernet 2", "0.0.0.0"] }"#,
        )
        .unwrap();
        let s = load_from(&p);
        // Non-strings and blanks dropped; names with spaces and wildcards kept (the server
        // reports a wildcard's refusal).
        assert_eq!(s.remote_endpoints, vec!["wg0", "Ethernet 2", "0.0.0.0"]);
        std::fs::write(&p, br#"{ "remoteEndpoints": "wg0" }"#).unwrap();
        assert!(load_from(&p).remote_endpoints.is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remote_specs_merge_legacy_bind_address() {
        let mut s = ControlSettings {
            remote_endpoints: vec!["wg0".into(), "100.64.0.1".into()],
            bind_address: Some("100.64.0.1".into()),
            ..Default::default()
        };
        assert_eq!(s.remote_specs(), vec!["wg0", "100.64.0.1"]);
        s.remote_endpoints.clear();
        assert_eq!(s.remote_specs(), vec!["100.64.0.1"]);
        // A legacy wildcard is kept so it gets refused loudly, not silently ignored.
        s.bind_address = Some("0.0.0.0".into());
        assert_eq!(s.remote_specs(), vec!["0.0.0.0"]);
        // Loopback legacy value = nothing remote.
        s.bind_address = Some("127.0.0.1".into());
        assert!(s.remote_specs().is_empty());
        s.bind_address = None;
        assert!(s.remote_specs().is_empty());
    }

    #[test]
    fn spec_sweep_defaults_on_missing_or_garbage() {
        let d = SpecSweepSettings::default();
        assert!(d.enabled && !d.dry_run);
        assert_eq!((d.idle_close_hours, d.done_close_minutes), (3, 30));
        assert_eq!(SpecSweepSettings::from_value(None), d);
        let garbage = serde_json::json!({"enabled": "no", "idleCloseHours": -2,
            "doneCloseMinutes": "x", "dryRun": 7});
        assert_eq!(SpecSweepSettings::from_value(Some(&garbage)), d);
        let zero = serde_json::json!({"idleCloseHours": 0});
        assert_eq!(SpecSweepSettings::from_value(Some(&zero)), d);
    }

    #[test]
    fn spec_sweep_parses_and_round_trips_through_the_file() {
        let p = std::env::temp_dir().join(format!("hp-ss-{}.json", std::process::id()));
        std::fs::write(
            &p,
            r#"{"enabled":true,"specSweep":{"enabled":false,"idleCloseHours":5,"doneCloseMinutes":10,"dryRun":true}}"#,
        )
        .unwrap();
        let s = load_from(&p);
        let ss = s.spec_sweep.clone().unwrap();
        assert!(!ss.enabled && ss.dry_run);
        assert_eq!((ss.idle_close_hours, ss.done_close_minutes), (5, 10));
        save_to(&p, &s).unwrap();
        assert_eq!(load_from(&p), s);
        // A non-object block is ignored; absent block stays None.
        std::fs::write(&p, r#"{"specSweep":"junk"}"#).unwrap();
        assert!(load_from(&p).spec_sweep.is_none());
        let _ = std::fs::remove_file(&p);
    }
}
