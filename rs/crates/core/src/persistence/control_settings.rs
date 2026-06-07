//! Control-server settings persistence — `{ enabled, allowInput }` in
//! `control-settings.json` under the userData dir (both default `false`). Atomic write.
//!
//! Port of the `loadSettings` / `saveSettings` pair in `src/main/control-server.ts`.
//! Loading mirrors the TS coercion exactly: any read/parse error (missing or corrupt
//! file) yields the defaults, and each field is `true` ONLY when the JSON value is the
//! boolean `true` (TS `parsed.enabled === true`) — a missing key, `null`, or a
//! non-boolean value all coerce to `false`.

use crate::persistence::paths;
use serde::{Deserialize, Serialize};

/// Control-server settings: whether the local control API is enabled, and whether it
/// accepts input (send_input/send_keys) from clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlSettings {
    pub enabled: bool,
    pub allow_input: bool,
}

impl Default for ControlSettings {
    fn default() -> Self {
        // DEFAULT_SETTINGS in control-server.ts: control API off, input disallowed.
        ControlSettings {
            enabled: false,
            allow_input: false,
        }
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
                allow_input: false
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
            },
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            on_disk,
            "{\n  \"enabled\": false,\n  \"allowInput\": true\n}"
        );
        let _ = std::fs::remove_file(&p);
    }
}
