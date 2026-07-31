//! The default-off "talk" background loop: every [`POLL_INTERVAL`], scan the read-model for
//! panes with talk enabled, tail each one's live Claude transcript for NEW assistant replies
//! (never history), normalize them, and enqueue onto the shared [`SpeechEngine`]. The engine
//! itself is spawned lazily on first use — an install with no pane ever talking starts no
//! playback thread and does no polling work beyond the (empty) read-model scan.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::control::server::Shared;
use crate::speech::engine::{SpeechEngine, SpeechHandle, Utterance};
use crate::speech::normalize::normalize_for_speech;
use crate::speech::tailer::{transcript_path, TranscriptTail};
use crate::speech::{self, SpeechSettings};

/// How often the background loop scans talking panes for new transcript text.
pub const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// A point-in-time snapshot for `/state`'s `speech` field. `backend` is reported even
/// before the engine has been lazily spawned — [`speech::engine::detect`] is pure.
pub struct SpeechServiceStatus {
    pub muted: bool,
    pub focused_only: bool,
    pub backend: String,
    pub speaking_pane: Option<String>,
}

/// Owns persisted [`SpeechSettings`], the lazily-spawned engine, and each talking pane's
/// [`TranscriptTail`]. Cheap to construct — no engine thread starts until a pane actually
/// talks.
pub struct SpeechService {
    settings_path: PathBuf,
    settings: Mutex<SpeechSettings>,
    engine: Mutex<Option<SpeechHandle>>,
    tails: Mutex<HashMap<String, TranscriptTail>>,
}

impl SpeechService {
    pub fn new(settings_path: PathBuf) -> Self {
        let settings = speech::load(&settings_path);
        SpeechService {
            settings_path,
            settings: Mutex::new(settings),
            engine: Mutex::new(None),
            tails: Mutex::new(HashMap::new()),
        }
    }

    fn settings_snapshot(&self) -> SpeechSettings {
        self.settings
            .lock()
            .expect("speech settings lock poisoned")
            .clone()
    }

    pub fn set_muted(&self, muted: bool) {
        let settings = {
            let mut s = self.settings.lock().expect("speech settings lock poisoned");
            s.muted = muted;
            s.clone()
        };
        let _ = speech::save(&self.settings_path, &settings);
        if let Some(handle) = self
            .engine
            .lock()
            .expect("speech engine lock poisoned")
            .as_ref()
        {
            handle.set_muted(muted);
        }
    }

    /// Kill any in-flight utterance and discard the queued backlog, without changing the
    /// persisted mute setting — a one-shot "shut up now" rather than a mode switch. A
    /// never-spawned engine has nothing to stop, so this is a no-op then.
    pub fn stop_all(&self) {
        if let Some(handle) = self
            .engine
            .lock()
            .expect("speech engine lock poisoned")
            .as_ref()
        {
            handle.stop_all();
        }
    }

    pub fn set_focused_only(&self, focused_only: bool) {
        let settings = {
            let mut s = self.settings.lock().expect("speech settings lock poisoned");
            s.focused_only = focused_only;
            s.clone()
        };
        let _ = speech::save(&self.settings_path, &settings);
    }

    pub fn status(&self) -> SpeechServiceStatus {
        let settings = self.settings_snapshot();
        let engine = self.engine.lock().expect("speech engine lock poisoned");
        let (backend, speaking_pane) = match engine.as_ref() {
            Some(h) => {
                let st = h.status();
                (st.backend, st.speaking_pane)
            }
            None => (speech::engine::detect(&settings).name().to_string(), None),
        };
        SpeechServiceStatus {
            muted: settings.muted,
            focused_only: settings.focused_only,
            backend,
            speaking_pane,
        }
    }

    /// Lazily spawn the engine on first use, returning a cheap clone of the handle.
    fn ensure_engine(&self) -> SpeechHandle {
        let mut engine = self.engine.lock().expect("speech engine lock poisoned");
        if let Some(h) = engine.as_ref() {
            return h.clone();
        }
        let settings = self.settings_snapshot();
        let handle = SpeechEngine::spawn(settings);
        *engine = Some(handle.clone());
        handle
    }

    /// One tick of the background loop, given a snapshot of currently-talking panes
    /// (`(pane id, label)`) already read from the read-model. No talkers takes the
    /// fast path — clear any stale tails and return — without spawning the engine or
    /// touching the filesystem, so a default-off install costs nothing ongoing.
    pub fn tick(&self, talking: &[(String, String)], focused_pane: Option<&str>) {
        if talking.is_empty() {
            self.tails
                .lock()
                .expect("speech tails lock poisoned")
                .clear();
            return;
        }
        let settings = self.settings_snapshot();
        let handle = self.ensure_engine();
        let mut tails = self.tails.lock().expect("speech tails lock poisoned");
        poll_tick(
            talking,
            &settings,
            focused_pane,
            &mut tails,
            &handle,
            resolve_transcript,
        );
    }
}

/// Poll [`Shared::model`] for talking panes and [`SpeechService::tick`] once. Runs forever
/// on [`POLL_INTERVAL`] until its task is aborted; the model lock is held only long enough
/// to snapshot talking panes + the focused pane, never across the tail/engine I/O below.
pub async fn run_ticker(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    loop {
        interval.tick().await;
        let (talking, focused) = {
            let m = shared.model.lock().expect("model lock poisoned");
            (m.talking_panes(), m.focused_pane().map(str::to_string))
        };
        shared.speech.tick(&talking, focused.as_deref());
    }
}

/// A talking pane's live transcript path, resolved from its Claude session marker.
/// `None` when the pane has no live marker (not a Claude pane, or the conversation
/// already ended).
fn resolve_transcript(pane_id: &str) -> Option<PathBuf> {
    let marker = crate::claude_panes::read_pane_session(pane_id)?;
    transcript_path(&marker)
}

/// The loop body proper, pure aside from the injected `resolve_transcript` (so tests can
/// point it at a tempdir instead of the real per-account transcript store): for each
/// talking pane, (re)create its tail if the transcript path is new or changed, poll it for
/// new assistant text, normalize, and enqueue. A pane no longer in `talking` (talk turned
/// off, or the pane is gone) has its tail dropped. `label` is only attached to the
/// utterance when more than one pane is talking, so a lone talker stays terse.
fn poll_tick(
    talking: &[(String, String)],
    settings: &SpeechSettings,
    focused_pane: Option<&str>,
    tails: &mut HashMap<String, TranscriptTail>,
    handle: &SpeechHandle,
    resolve_transcript: impl Fn(&str) -> Option<PathBuf>,
) {
    let live: std::collections::HashSet<&str> = talking.iter().map(|(id, _)| id.as_str()).collect();
    tails.retain(|id, _| live.contains(id.as_str()));

    let multi = talking.len() >= 2;
    for (pane_id, label) in talking {
        if settings.focused_only {
            if let Some(focused) = focused_pane {
                if focused != pane_id {
                    continue;
                }
            }
        }
        let Some(path) = resolve_transcript(pane_id) else {
            tails.remove(pane_id);
            continue;
        };
        let tail = tails
            .entry(pane_id.clone())
            .or_insert_with(|| TranscriptTail::start_at_end(path.clone()));
        if tail.path() != path {
            *tail = TranscriptTail::start_at_end(path);
        }
        for text in tail.poll() {
            let normalized = normalize_for_speech(&text);
            if normalized.trim().is_empty() {
                continue;
            }
            handle.enqueue(Utterance {
                pane_id: pane_id.clone(),
                label: if multi { label.clone() } else { String::new() },
                text: normalized,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hp-speech-service-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A shell script that appends its arg to `log` — the "custom command template" backend,
    /// so tests can observe exactly what text reached the engine.
    fn echo_template(dir: &std::path::Path) -> (Vec<String>, PathBuf) {
        let log = dir.join("spoken.log");
        let script = dir.join("speak.sh");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$1\" >> {log:?}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        (
            vec![
                "/bin/sh".to_string(),
                script.to_string_lossy().into_owned(),
                "{text}".to_string(),
            ],
            log,
        )
    }

    fn assistant_line(text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}\n",
            serde_json::to_string(text).unwrap()
        )
    }

    fn wait_for(log: &std::path::Path, needle: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let contents = std::fs::read_to_string(log).unwrap_or_default();
            if contents.contains(needle) || std::time::Instant::now() >= deadline {
                return contents;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)] // fake TTS backend is a /bin/sh script
    #[test]
    fn enabled_pane_speaks_new_assistant_text_normalized() {
        let dir = scratch_dir("single");
        let (template, log) = echo_template(&dir);
        let handle = SpeechEngine::spawn(SpeechSettings {
            command_template: Some(template),
            muted: false,
            focused_only: false,
        });
        let transcript = dir.join("t.jsonl");
        std::fs::write(&transcript, "").unwrap();

        let mut tails = HashMap::new();
        let talking = vec![("pane-1".to_string(), "Worker".to_string())];
        let resolve = |_: &str| Some(transcript.clone());
        // First tick establishes the tail at EOF — pre-existing content never spoken.
        poll_tick(
            &talking,
            &SpeechSettings::default(),
            None,
            &mut tails,
            &handle,
            resolve,
        );

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        f.write_all(assistant_line("# Hello\nworld").as_bytes())
            .unwrap();
        drop(f);

        poll_tick(
            &talking,
            &SpeechSettings::default(),
            None,
            &mut tails,
            &handle,
            resolve,
        );

        let spoken = wait_for(&log, "Hello world");
        assert!(spoken.contains("Hello world"), "got: {spoken:?}");
        assert!(
            !spoken.contains('#'),
            "markdown must be normalized: {spoken:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)] // fake TTS backend is a /bin/sh script
    #[test]
    fn two_talking_panes_get_label_prefixed() {
        let dir = scratch_dir("multi");
        let (template, log) = echo_template(&dir);
        let handle = SpeechEngine::spawn(SpeechSettings {
            command_template: Some(template),
            muted: false,
            focused_only: false,
        });
        let t1 = dir.join("a.jsonl");
        let t2 = dir.join("b.jsonl");
        std::fs::write(&t1, "").unwrap();
        std::fs::write(&t2, "").unwrap();

        let mut tails = HashMap::new();
        let talking = vec![
            ("pane-a".to_string(), "Alpha".to_string()),
            ("pane-b".to_string(), "Beta".to_string()),
        ];
        let t1c = t1.clone();
        let t2c = t2.clone();
        let resolve = move |id: &str| match id {
            "pane-a" => Some(t1c.clone()),
            "pane-b" => Some(t2c.clone()),
            _ => None,
        };
        poll_tick(
            &talking,
            &SpeechSettings::default(),
            None,
            &mut tails,
            &handle,
            resolve.clone(),
        );

        std::fs::OpenOptions::new()
            .append(true)
            .open(&t1)
            .unwrap()
            .write_all(assistant_line("one").as_bytes())
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&t2)
            .unwrap()
            .write_all(assistant_line("two").as_bytes())
            .unwrap();

        poll_tick(
            &talking,
            &SpeechSettings::default(),
            None,
            &mut tails,
            &handle,
            resolve,
        );

        let spoken = wait_for(&log, "Beta. two");
        assert!(spoken.contains("Alpha. one"), "got: {spoken:?}");
        assert!(spoken.contains("Beta. two"), "got: {spoken:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_talkers_clears_tails_and_never_resolves_a_transcript() {
        let mut tails = HashMap::new();
        tails.insert(
            "stale".to_string(),
            TranscriptTail::start_at_end(PathBuf::from("/nonexistent")),
        );
        let calls = AtomicUsize::new(0);
        let handle = SpeechEngine::spawn(SpeechSettings::default());
        poll_tick(
            &[],
            &SpeechSettings::default(),
            None,
            &mut tails,
            &handle,
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            },
        );
        assert!(tails.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn service_tick_fast_path_clears_tails_without_spawning_engine() {
        let dir = scratch_dir("service-idle");
        let service = SpeechService::new(dir.join("speech.json"));
        service.tick(&[], None);
        assert!(
            service.engine.lock().unwrap().is_none(),
            "no pane talking must never spawn the engine"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
