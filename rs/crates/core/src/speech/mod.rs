//! Per-pane "talk": local TTS for new Claude assistant replies.
//! Owned modules: [`normalize`] (markdown) and [`engine`] (the
//! serialized playback queue + backend detection). This file holds only the
//! persisted settings shape + load/save.
//!
//! NOTE: a sibling track adds `speech/tailer.rs` (pane-output tailing) in parallel —
//! kept additive-only here so that merge stays a small, conflict-free diff.

pub mod engine;
pub mod normalize;
pub mod tailer;

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Per-installation speech settings, persisted to `speech.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SpeechSettings {
    /// Custom TTS command, e.g. `["say", "{text}"]`. `None` -> auto-detect a backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_template: Option<Vec<String>>,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub focused_only: bool,
}

/// Read settings from `path`, falling back to [`SpeechSettings::default`] on a
/// missing or corrupt file.
pub fn load(path: &Path) -> SpeechSettings {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return SpeechSettings::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist `settings` to `path`, atomically.
pub fn save(path: &Path, settings: &SpeechSettings) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::persistence::paths::write_atomic(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hp-speech-settings-{}-{tag}.json",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_yields_defaults() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load(&p), SpeechSettings::default());
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let p = temp_path("corrupt");
        std::fs::write(&p, b"not json {").unwrap();
        assert_eq!(load(&p), SpeechSettings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_fields_default() {
        let p = temp_path("partial");
        std::fs::write(&p, b"{}").unwrap();
        assert_eq!(load(&p), SpeechSettings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_then_load_round_trips() {
        let p = temp_path("roundtrip");
        let settings = SpeechSettings {
            command_template: Some(vec!["say".into(), "{text}".into()]),
            muted: true,
            focused_only: true,
        };
        save(&p, &settings).unwrap();
        assert_eq!(load(&p), settings);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn command_template_omitted_from_json_when_unset() {
        let p = temp_path("omit");
        save(&p, &SpeechSettings::default()).unwrap();
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert!(!on_disk.contains("commandTemplate"));
        let _ = std::fs::remove_file(&p);
    }
}
