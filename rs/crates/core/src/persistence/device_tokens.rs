//! Paired-device token persistence — `device-tokens.json` in the state dir, beside
//! `control.json`. Each record is a full-authority (unscoped) bearer token handed to one mobile
//! client by `hyperpanes pair`, tagged with a human `label` and an optional expiry.
//!
//! Unlike scoped tokens (in-memory, cleared on stop), device tokens must survive a host restart
//! so a phone paired once stays paired — the same guarantee the master token gets from its own
//! `control-token` file. The running server loads this table on start into its `TokenStore` and
//! rewrites it whenever `pair`/`revoke` mint or drop a device (via the control API).
//!
//! Loading is forgiving, matching the rest of `persistence/`: any read/parse error yields an
//! empty table rather than failing. The file is written `0600` — it holds live credentials.

use crate::persistence::paths;
use serde::{Deserialize, Serialize};

/// One paired device: its bearer `token`, a human `label` (`hyperpanes devices` shows it,
/// `hyperpanes revoke <label>` drops it), and an optional ms-epoch `expires_at` (`None` = never).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub label: String,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<i64>,
}

/// On-disk shape: `{ "devices": [ { label, token, expiresAt? }, … ] }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeviceFile {
    #[serde(default)]
    devices: Vec<DeviceRecord>,
}

/// Read the device table from the canonical `device-tokens.json` (empty on any error).
pub fn load() -> Vec<DeviceRecord> {
    load_from(&paths::device_tokens_json())
}

/// Read the device table from `path`, returning an empty vec on any read/parse error.
pub fn load_from(path: &std::path::Path) -> Vec<DeviceRecord> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<DeviceFile>(&raw)
        .map(|f| f.devices)
        .unwrap_or_default()
}

/// Persist the device table to the canonical `device-tokens.json` (atomic, `0600`).
pub fn save(devices: &[DeviceRecord]) -> std::io::Result<()> {
    save_to(&paths::device_tokens_json(), devices)
}

/// Persist the device table to `path` (atomic), then tighten to `0600` on Unix — the file holds
/// full-authority tokens, so it gets the same permissions as the master `control-token` file.
pub fn save_to(path: &std::path::Path, devices: &[DeviceRecord]) -> std::io::Result<()> {
    let file = DeviceFile {
        devices: devices.to_vec(),
    };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    paths::write_atomic(path, json.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hp-device-tokens-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_yields_empty() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert!(load_from(&p).is_empty());
    }

    #[test]
    fn corrupt_file_yields_empty() {
        let p = temp_path("corrupt");
        std::fs::write(&p, b"{ not json").unwrap();
        assert!(load_from(&p).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn round_trips_records_and_omits_null_expiry() {
        let p = temp_path("roundtrip");
        let devices = vec![
            DeviceRecord {
                label: "eyal-iphone".into(),
                token: "a".repeat(64),
                expires_at: None,
            },
            DeviceRecord {
                label: "ipad".into(),
                token: "b".repeat(64),
                expires_at: Some(1_800_000),
            },
        ];
        save_to(&p, &devices).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        // No-expiry records omit the field (keeps the file tidy, like control-settings does).
        assert!(!raw.contains("\"expiresAt\": null"));
        assert!(raw.contains("\"expiresAt\": 1800000"));
        assert_eq!(load_from(&p), devices);
        let _ = std::fs::remove_file(&p);
    }
}
