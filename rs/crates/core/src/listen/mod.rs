//! Push-to-talk dictation: record the mic, transcribe it locally, hand the text
//! back so a pane can type it. The mirror image of [`crate::speech`] (which
//! speaks Claude's replies) — same shape: a persisted settings blob here, the
//! backend detection + worker thread in [`engine`].
//!
//! Recording writes RAW 16 kHz mono s16le, never a .wav: stopping means killing
//! the recorder, and a killed `arecord`/`pw-record` leaves a wav header whose
//! length field was never fixed up, which decoders then read as zero samples.
//! We own the header instead — [`write_wav16`] wraps whatever bytes landed on
//! disk, so a mid-sentence stop always yields a valid file.

pub mod engine;

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Sample rate every backend is configured for — what whisper.cpp wants natively.
pub const SAMPLE_RATE: u32 = 16_000;
/// Bytes per sample (s16le, mono).
const BYTES_PER_SAMPLE: u32 = 2;
/// Recordings shorter than this are dropped instead of transcribed: whisper
/// hallucinates confident sentences out of a fraction of a second of silence.
pub const MIN_RECORDING_MS: u64 = 350;

/// Per-installation dictation settings, persisted to `listen.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenSettings {
    /// Custom recorder, e.g. `["arecord", "-t", "raw", "{raw}"]`. `{raw}` is replaced
    /// with the capture path; the command must write RAW s16le 16 kHz mono there and
    /// keep running until killed. `None` -> auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_command: Option<Vec<String>>,
    /// Custom transcriber, e.g. `["whisper-cli", "-m", "…", "-f", "{wav}", "-nt"]`.
    /// `{wav}` is replaced with the finished recording; the transcript is read from
    /// stdout. `None` -> auto-detect (whisper.cpp) using [`Self::model_path`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcribe_command: Option<Vec<String>>,
    /// Whisper model for the auto-detected transcriber (ignored with a custom command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    /// Hard cap on one utterance; recording auto-stops here so a forgotten
    /// push-to-talk can't fill the disk.
    #[serde(default = "default_max_seconds")]
    pub max_seconds: u64,
    /// Press Enter after typing the transcript into the pane. Off by default —
    /// dictation drops text at the prompt, you decide when it is sent.
    #[serde(default)]
    pub auto_submit: bool,
}

fn default_max_seconds() -> u64 {
    60
}

impl Default for ListenSettings {
    fn default() -> Self {
        Self {
            record_command: None,
            transcribe_command: None,
            model_path: None,
            max_seconds: default_max_seconds(),
            auto_submit: false,
        }
    }
}

/// Read settings from `path`, falling back to [`ListenSettings::default`] on a
/// missing or corrupt file.
pub fn load(path: &Path) -> ListenSettings {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return ListenSettings::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Persist `settings` to `path`, atomically.
pub fn save(path: &Path, settings: &ListenSettings) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::persistence::paths::write_atomic(path, json.as_bytes())
}

/// How long `raw_len` bytes of 16 kHz mono s16le audio play for.
pub fn duration_ms(raw_len: usize) -> u64 {
    raw_len as u64 * 1000 / u64::from(SAMPLE_RATE * BYTES_PER_SAMPLE)
}

/// Wrap raw 16 kHz mono s16le `pcm` in a 44-byte canonical WAV header and write it
/// to `path`. Owning the header is what makes a killed recorder safe (see the module
/// header).
pub fn write_wav16(path: &Path, pcm: &[u8]) -> std::io::Result<()> {
    let data_len = pcm.len() as u32;
    let byte_rate = SAMPLE_RATE * BYTES_PER_SAMPLE;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels = mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&(BYTES_PER_SAMPLE as u16).to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    std::fs::write(path, &out)
}

/// Squeeze a transcriber's stdout down to the utterance: whisper.cpp prints
/// `[00:00.000 --> 00:02.000]  text` per segment even with some log flags, and
/// annotates non-speech as `[BLANK_AUDIO]`/`(silence)`. Returns an empty string
/// when nothing was said.
pub fn clean_transcript(raw: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Drop a leading `[hh:mm:ss.mmm --> hh:mm:ss.mmm]` timestamp span.
        let text = match (line.starts_with('['), line.find("]  ")) {
            (true, Some(end)) if line[..end].contains("-->") => line[end + 3..].trim(),
            _ => line,
        };
        if text.is_empty() {
            continue;
        }
        // Non-speech annotations: a bracketed/parenthesised whole line.
        let bracketed = (text.starts_with('[') && text.ends_with(']'))
            || (text.starts_with('(') && text.ends_with(')'));
        if bracketed {
            continue;
        }
        parts.push(text.to_string());
    }
    parts.join(" ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hp-listen-{}-{tag}", std::process::id()))
    }

    #[test]
    fn missing_file_yields_defaults() {
        let p = temp_path("missing.json");
        let _ = std::fs::remove_file(&p);
        let s = load(&p);
        assert_eq!(s, ListenSettings::default());
        assert_eq!(s.max_seconds, 60);
        assert!(!s.auto_submit);
    }

    #[test]
    fn settings_round_trip_and_omit_unset_commands() {
        let p = temp_path("roundtrip.json");
        let settings = ListenSettings {
            model_path: Some("/models/ggml-small.en.bin".into()),
            auto_submit: true,
            ..Default::default()
        };
        save(&p, &settings).unwrap();
        let json = std::fs::read_to_string(&p).unwrap();
        assert!(
            !json.contains("recordCommand"),
            "unset commands must be omitted: {json}"
        );
        assert!(json.contains("modelPath"));
        assert_eq!(load(&p), settings);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults() {
        let p = temp_path("corrupt.json");
        std::fs::write(&p, b"{not json").unwrap();
        assert_eq!(load(&p), ListenSettings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_header_describes_the_payload() {
        let p = temp_path("out.wav");
        let pcm = vec![0u8; 3200]; // 0.1 s at 16 kHz mono s16le
        write_wav16(&p, &pcm).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(bytes.len(), 44 + pcm.len());
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            36 + pcm.len() as u32
        );
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            SAMPLE_RATE
        );
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            pcm.len() as u32
        );
        assert_eq!(duration_ms(pcm.len()), 100);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn clean_transcript_strips_timestamps_and_non_speech() {
        let raw = "\n[00:00:00.000 --> 00:00:02.000]   open the config file\n\
                   [00:00:02.000 --> 00:00:03.000]   and rerun the tests\n";
        assert_eq!(
            clean_transcript(raw),
            "open the config file and rerun the tests"
        );
        assert_eq!(clean_transcript("[BLANK_AUDIO]"), "");
        assert_eq!(clean_transcript("(silence)\n"), "");
        assert_eq!(clean_transcript("  plain text  "), "plain text");
        assert_eq!(clean_transcript(""), "");
    }
}
