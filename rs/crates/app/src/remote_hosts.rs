//! Saved remote hosts: `<config dir>/remote-hosts.json`.
//!
//! Entries `{label, host, port, token, autoconnect}`. The file holds bearer tokens, so it is
//! written atomically with owner-only permissions (`write_atomic_private`: 0600 on unix). The
//! token never leaves this module except inside a [`RemoteTarget`] (whose `Debug`/`Display`
//! redact it); [`SavedHost`]'s own `Debug` is redacted too.

use std::fmt;
use std::path::Path;

use hyperpanes_core::persistence::paths;
use hyperpanes_core::remote::{PairingError, RemoteTarget};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedHost {
    pub label: String,
    pub host: String,
    pub port: u16,
    pub token: String,
    /// Opt-in: connect when the app starts. Default off.
    #[serde(default)]
    pub autoconnect: bool,
}

impl fmt::Debug for SavedHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SavedHost")
            .field("label", &self.label)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &"***")
            .field("autoconnect", &self.autoconnect)
            .finish()
    }
}

impl SavedHost {
    pub fn from_target(label: &str, target: &RemoteTarget, autoconnect: bool) -> Self {
        SavedHost {
            label: label.to_string(),
            host: target.host.clone(),
            port: target.port,
            token: target.token().to_string(),
            autoconnect,
        }
    }

    pub fn target(&self) -> Result<RemoteTarget, PairingError> {
        RemoteTarget::new(self.host.clone(), self.port, self.token.clone())
    }
}

/// A sensible default label for a host with none given.
pub fn default_label(target: &RemoteTarget) -> String {
    target.host.clone()
}

pub fn load() -> Vec<SavedHost> {
    load_from(&paths::remote_hosts_json())
}

pub fn load_from(path: &Path) -> Vec<SavedHost> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<SavedHost>>(&b).ok())
        .unwrap_or_default()
}

pub fn save_to(path: &Path, hosts: &[SavedHost]) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(hosts)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    paths::write_atomic_private(path, &json)
}

/// Insert or replace (by label) and persist.
pub fn remember(host: SavedHost) -> std::io::Result<()> {
    remember_at(&paths::remote_hosts_json(), host)
}

pub fn remember_at(path: &Path, host: SavedHost) -> std::io::Result<()> {
    let mut all = load_from(path);
    match all.iter_mut().find(|h| h.label == host.label) {
        Some(slot) => *slot = host,
        None => all.push(host),
    }
    save_to(path, &all)
}

/// Remove the entry labelled `label` (and its token) from the file.
pub fn forget(label: &str) -> std::io::Result<bool> {
    forget_at(&paths::remote_hosts_json(), label)
}

pub fn forget_at(path: &Path, label: &str) -> std::io::Result<bool> {
    let mut all = load_from(path);
    let before = all.len();
    all.retain(|h| h.label != label);
    let removed = all.len() != before;
    if removed {
        save_to(path, &all)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hp-remote-hosts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("remote-hosts.json")
    }

    fn host(label: &str, auto: bool) -> SavedHost {
        SavedHost {
            label: label.into(),
            host: "example-host".into(),
            port: 41419,
            token: "SECRETTOKEN".into(),
            autoconnect: auto,
        }
    }

    #[test]
    fn save_load_roundtrip_and_replace_by_label() {
        let p = scratch("rt");
        assert!(load_from(&p).is_empty());
        remember_at(&p, host("a", false)).unwrap();
        remember_at(&p, host("b", true)).unwrap();
        remember_at(
            &p,
            SavedHost {
                port: 1,
                ..host("a", true)
            },
        )
        .unwrap();
        let all = load_from(&p);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].port, 1);
        assert!(all[0].autoconnect && all[1].autoconnect);
    }

    #[test]
    fn autoconnect_defaults_off_when_absent() {
        let p = scratch("def");
        std::fs::write(&p, r#"[{"label":"x","host":"h","port":2,"token":"t"}]"#).unwrap();
        assert!(!load_from(&p)[0].autoconnect);
    }

    #[test]
    fn forget_removes_entry_and_token_from_file() {
        let p = scratch("forget");
        remember_at(&p, host("a", false)).unwrap();
        remember_at(&p, host("b", false)).unwrap();
        assert!(forget_at(&p, "a").unwrap());
        assert!(!forget_at(&p, "a").unwrap());
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"b\"") && !raw.contains("\"a\""));
        forget_at(&p, "b").unwrap();
        assert!(load_from(&p).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let p = scratch("mode");
        remember_at(&p, host("a", false)).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        remember_at(&p, host("b", false)).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn token_not_in_debug_and_target_roundtrips() {
        let h = host("a", false);
        assert!(!format!("{h:?}").contains("SECRETTOKEN"));
        let t = h.target().unwrap();
        assert!(!format!("{t:?}").contains("SECRETTOKEN"));
        assert_eq!(t.token(), "SECRETTOKEN");
        assert_eq!(SavedHost::from_target("a", &t, false), h);
    }
}
