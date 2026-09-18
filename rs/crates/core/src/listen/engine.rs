//! The dictation worker: one recorder child at a time, then one transcriber run,
//! on a private thread. Plain `std::thread` + `std::sync::mpsc`, like
//! [`crate::speech::engine`] — the UI thread only flips a phase and drains text.
//!
//! Recording is a child process writing RAW s16le (see the module header of
//! [`super`]); stopping kills it, wraps whatever bytes exist in a WAV header, and
//! runs the transcriber. Nothing here blocks the caller: `start`/`stop` just post
//! a message.

use super::{clean_transcript, duration_ms, write_wav16, ListenSettings, MIN_RECORDING_MS};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How often the recording loop wakes to check the max-duration cap.
const TICK: Duration = Duration::from_millis(50);

/// What the engine is doing right now. `repr(u8)` so the UI can carry it in an
/// atomic and hand it to Slint as an int.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    Idle = 0,
    Recording = 1,
    Transcribing = 2,
}

impl Phase {
    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Recording,
            2 => Phase::Transcribing,
            _ => Phase::Idle,
        }
    }
}

/// Mic capture backend. Each writes RAW 16 kHz mono s16le to the `{raw}` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorder {
    /// User-supplied command; `{raw}` in any arg is replaced with the capture path.
    Custom(Vec<String>),
    /// PipeWire (`pw-record`) — the modern Fedora/Ubuntu default.
    PwRecord,
    /// ALSA (`arecord`), present on essentially every Linux box.
    Arecord,
    /// `ffmpeg`, used on macOS (avfoundation) / Windows (dshow) where no small
    /// capture CLI ships by default.
    Ffmpeg,
    None,
}

/// Speech-to-text backend. Reads a WAV, prints the transcript on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transcriber {
    /// User-supplied command; `{wav}` in any arg is replaced with the recording path.
    Custom(Vec<String>),
    /// whisper.cpp's `whisper-cli -m <model>`.
    WhisperCli {
        program: String,
        model: String,
    },
    None,
}

impl Recorder {
    pub fn name(&self) -> &'static str {
        match self {
            Recorder::Custom(_) => "custom",
            Recorder::PwRecord => "pw-record",
            Recorder::Arecord => "arecord",
            Recorder::Ffmpeg => "ffmpeg",
            Recorder::None => "none",
        }
    }
}

impl Transcriber {
    pub fn name(&self) -> &'static str {
        match self {
            Transcriber::Custom(_) => "custom",
            Transcriber::WhisperCli { .. } => "whisper-cli",
            Transcriber::None => "none",
        }
    }
}

fn on_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(cmd))
        .find(|p| p.is_file())
}

/// Where a locally built whisper.cpp usually lands, checked when `whisper-cli` is
/// not on `PATH` (the build tree is not normally installed).
fn whisper_fallbacks() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.push(home.join(".local/share/whisper.cpp/build/bin/whisper-cli"));
        out.push(home.join(".local/bin/whisper-cli"));
    }
    out
}

/// Models shipped next to a whisper.cpp checkout, best (largest) first.
fn model_fallbacks() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let models = PathBuf::from(home).join(".local/share/whisper.cpp/models");
        for name in [
            "ggml-medium.en.bin",
            "ggml-small.en.bin",
            "ggml-base.en.bin",
            "ggml-tiny.en.bin",
        ] {
            out.push(models.join(name));
        }
    }
    out
}

/// Pick a recorder: the configured command if present, else the first backend on
/// `PATH` for this platform.
pub fn detect_recorder(settings: &ListenSettings) -> Recorder {
    if let Some(cmd) = &settings.record_command {
        if !cmd.is_empty() {
            return Recorder::Custom(cmd.clone());
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if on_path("pw-record").is_some() {
            return Recorder::PwRecord;
        }
        if on_path("arecord").is_some() {
            return Recorder::Arecord;
        }
    }
    if on_path("ffmpeg").is_some() {
        return Recorder::Ffmpeg;
    }
    Recorder::None
}

/// Pick a transcriber: the configured command, else whisper.cpp (`whisper-cli` on
/// `PATH` or in its usual build location) paired with the configured model — or,
/// failing that, a model sitting in the whisper.cpp checkout.
pub fn detect_transcriber(settings: &ListenSettings) -> Transcriber {
    if let Some(cmd) = &settings.transcribe_command {
        if !cmd.is_empty() {
            return Transcriber::Custom(cmd.clone());
        }
    }
    let program = on_path("whisper-cli")
        .or_else(|| whisper_fallbacks().into_iter().find(|p| p.is_file()))
        .map(|p| p.to_string_lossy().into_owned());
    let model = settings
        .model_path
        .clone()
        .filter(|m| !m.is_empty())
        .filter(|m| PathBuf::from(m).is_file())
        .or_else(|| {
            model_fallbacks()
                .into_iter()
                .find(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
        });
    match (program, model) {
        (Some(program), Some(model)) => Transcriber::WhisperCli { program, model },
        _ => Transcriber::None,
    }
}

/// Threads for whisper.cpp: enough to matter, capped before oversubscription hurts.
fn whisper_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8))
        .unwrap_or(4)
}

fn substitute(template: &[String], key: &str, value: &str) -> Option<Command> {
    let (program, args) = template.split_first()?;
    let mut c = Command::new(program);
    for arg in args {
        c.arg(arg.replace(key, value));
    }
    Some(c)
}

fn record_command(backend: &Recorder, raw: &str) -> Option<Command> {
    let rate = super::SAMPLE_RATE.to_string();
    match backend {
        Recorder::None => None,
        Recorder::Custom(t) => substitute(t, "{raw}", raw),
        Recorder::PwRecord => {
            let mut c = Command::new("pw-record");
            c.args([
                "--rate",
                &rate,
                "--channels",
                "1",
                "--format",
                "s16",
                "--raw",
                raw,
            ]);
            Some(c)
        }
        Recorder::Arecord => {
            let mut c = Command::new("arecord");
            c.args([
                "-q", "-t", "raw", "-f", "S16_LE", "-r", &rate, "-c", "1", raw,
            ]);
            Some(c)
        }
        Recorder::Ffmpeg => {
            let mut c = Command::new("ffmpeg");
            c.args(["-hide_banner", "-loglevel", "error", "-y"]);
            #[cfg(target_os = "macos")]
            c.args(["-f", "avfoundation", "-i", ":default"]);
            #[cfg(target_os = "windows")]
            c.args(["-f", "dshow", "-i", "audio=default"]);
            #[cfg(all(unix, not(target_os = "macos")))]
            c.args(["-f", "pulse", "-i", "default"]);
            c.args(["-ar", &rate, "-ac", "1", "-f", "s16le", raw]);
            Some(c)
        }
    }
}

fn transcribe_command(backend: &Transcriber, wav: &str) -> Option<Command> {
    match backend {
        Transcriber::None => None,
        Transcriber::Custom(t) => substitute(t, "{wav}", wav),
        Transcriber::WhisperCli { program, model } => {
            let mut c = Command::new(program);
            // `-nt` drops the per-segment timestamps, `-np` the progress chatter, so
            // stdout is (close to) just the words; `clean_transcript` handles the rest.
            //
            // `-bs 1` (greedy instead of the 5-wide beam) and a capped thread count are
            // what make this feel like dictation rather than a batch job: on this box,
            // 8 s of speech through small.en went 6.4 s -> 2.9 s, with no difference in
            // the transcript. Threads are capped at 8 — handing whisper every core was
            // 24 s (memory-bandwidth bound, not compute bound).
            c.args([
                "-m",
                model,
                "-f",
                wav,
                "-nt",
                "-np",
                "-bs",
                "1",
                "-t",
                &whisper_threads().to_string(),
            ]);
            Some(c)
        }
    }
}

/// A point-in-time snapshot of the engine, for status lines and tests.
#[derive(Debug, Clone, PartialEq)]
pub struct ListenStatus {
    pub phase: Phase,
    pub recorder: String,
    pub transcriber: String,
    pub last_error: Option<String>,
}

/// Hands each engine its own capture-file id (see [`capture_path`]).
static NEXT_ENGINE_ID: AtomicU64 = AtomicU64::new(0);

struct Shared {
    /// This engine's capture-file id.
    id: u64,
    phase: AtomicU8,
    recorder: Mutex<Recorder>,
    transcriber: Mutex<Transcriber>,
    child: Mutex<Option<Child>>,
    /// Finished transcripts, drained by the UI thread.
    out: Mutex<VecDeque<String>>,
    last_error: Mutex<Option<String>>,
    max_seconds: AtomicU8,
}

enum Msg {
    Start,
    Stop,
    Cancel,
}

/// Owns the worker thread; cloneable handle used from the UI thread.
pub struct ListenEngine;

impl ListenEngine {
    /// Detect backends from `settings` and start the worker thread.
    pub fn spawn(settings: &ListenSettings) -> ListenHandle {
        spawn_with(
            detect_recorder(settings),
            detect_transcriber(settings),
            settings.max_seconds,
        )
    }
}

fn spawn_with(recorder: Recorder, transcriber: Transcriber, max_seconds: u64) -> ListenHandle {
    let shared = Arc::new(Shared {
        id: NEXT_ENGINE_ID.fetch_add(1, Ordering::SeqCst),
        phase: AtomicU8::new(Phase::Idle as u8),
        recorder: Mutex::new(recorder),
        transcriber: Mutex::new(transcriber),
        child: Mutex::new(None),
        out: Mutex::new(VecDeque::new()),
        last_error: Mutex::new(None),
        max_seconds: AtomicU8::new(max_seconds.clamp(1, 255) as u8),
    });
    let (tx, rx) = mpsc::channel();
    let worker = Arc::clone(&shared);
    thread::spawn(move || run(worker, rx));
    ListenHandle { shared, tx }
}

#[derive(Clone)]
pub struct ListenHandle {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Msg>,
}

impl ListenHandle {
    /// Begin recording. No-op unless idle.
    pub fn start(&self) {
        let _ = self.tx.send(Msg::Start);
    }

    /// Stop recording and transcribe what was captured.
    pub fn stop(&self) {
        let _ = self.tx.send(Msg::Stop);
    }

    /// Stop recording and throw the audio away.
    pub fn cancel(&self) {
        let _ = self.tx.send(Msg::Cancel);
    }

    /// Push-to-talk: idle -> record, recording -> stop+transcribe. While a
    /// transcription is already running this does nothing (the text is coming).
    pub fn toggle(&self) {
        match self.phase() {
            Phase::Idle => self.start(),
            Phase::Recording => self.stop(),
            Phase::Transcribing => {}
        }
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.shared.phase.load(Ordering::SeqCst))
    }

    /// Take the next finished transcript, if any. Called on the UI tick.
    pub fn take_transcript(&self) -> Option<String> {
        self.shared
            .out
            .lock()
            .expect("listen out lock poisoned")
            .pop_front()
    }

    /// Take the last failure (backend missing, recorder died, empty audio…), if any.
    pub fn take_error(&self) -> Option<String> {
        self.shared
            .last_error
            .lock()
            .expect("listen error lock poisoned")
            .take()
    }

    /// Re-detect backends after a settings change.
    pub fn reconfigure(&self, settings: &ListenSettings) {
        *self.shared.recorder.lock().expect("recorder lock poisoned") = detect_recorder(settings);
        *self
            .shared
            .transcriber
            .lock()
            .expect("transcriber lock poisoned") = detect_transcriber(settings);
        self.shared
            .max_seconds
            .store(settings.max_seconds.clamp(1, 255) as u8, Ordering::SeqCst);
    }

    /// True when both halves of the pipeline exist — otherwise dictation can only
    /// report how to fix it.
    pub fn ready(&self) -> bool {
        *self.shared.recorder.lock().expect("recorder lock poisoned") != Recorder::None
            && *self
                .shared
                .transcriber
                .lock()
                .expect("transcriber lock poisoned")
                != Transcriber::None
    }

    pub fn status(&self) -> ListenStatus {
        ListenStatus {
            phase: self.phase(),
            recorder: self
                .shared
                .recorder
                .lock()
                .expect("recorder lock poisoned")
                .name()
                .to_string(),
            transcriber: self
                .shared
                .transcriber
                .lock()
                .expect("transcriber lock poisoned")
                .name()
                .to_string(),
            last_error: self
                .shared
                .last_error
                .lock()
                .expect("listen error lock poisoned")
                .clone(),
        }
    }
}

fn set_error(shared: &Arc<Shared>, msg: impl Into<String>) {
    *shared
        .last_error
        .lock()
        .expect("listen error lock poisoned") = Some(msg.into());
}

fn kill_child(shared: &Arc<Shared>) {
    let taken = shared.child.lock().expect("child lock poisoned").take();
    if let Some(mut child) = taken {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Capture files are keyed by engine instance, not just pid: two engines in one
/// process (the test suite, or a second window's) must never share a recording.
fn capture_path(engine_id: u64, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hp-dictate-{}-{engine_id}.{ext}",
        std::process::id()
    ))
}

fn run(shared: Arc<Shared>, rx: mpsc::Receiver<Msg>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stop | Msg::Cancel => {} // nothing recording
            Msg::Start => {
                let raw = capture_path(shared.id, "raw");
                let _ = std::fs::remove_file(&raw);
                let backend = shared
                    .recorder
                    .lock()
                    .expect("recorder lock poisoned")
                    .clone();
                let Some(mut cmd) = record_command(&backend, &raw.to_string_lossy()) else {
                    set_error(
                        &shared,
                        "No microphone backend (install pw-record, arecord or ffmpeg)",
                    );
                    continue;
                };
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        set_error(&shared, format!("Recorder failed to start: {e}"));
                        continue;
                    }
                };
                *shared.child.lock().expect("child lock poisoned") = Some(child);
                shared.phase.store(Phase::Recording as u8, Ordering::SeqCst);
                let keep = record_loop(&shared, &rx);
                kill_child(&shared);
                if !keep {
                    shared.phase.store(Phase::Idle as u8, Ordering::SeqCst);
                    let _ = std::fs::remove_file(&raw);
                    continue;
                }
                shared
                    .phase
                    .store(Phase::Transcribing as u8, Ordering::SeqCst);
                transcribe(&shared, &raw);
                shared.phase.store(Phase::Idle as u8, Ordering::SeqCst);
            }
        }
    }
}

/// Block while recording. Returns true when the audio should be transcribed
/// (explicit stop or the duration cap), false when cancelled.
fn record_loop(shared: &Arc<Shared>, rx: &mpsc::Receiver<Msg>) -> bool {
    let cap = Duration::from_secs(u64::from(shared.max_seconds.load(Ordering::SeqCst)));
    let started = Instant::now();
    loop {
        match rx.recv_timeout(TICK) {
            Ok(Msg::Stop) => return true,
            Ok(Msg::Cancel) => return false,
            Ok(Msg::Start) => {} // already recording
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
        if started.elapsed() >= cap {
            return true;
        }
        // The recorder died on its own (no such device, PipeWire restart…).
        let mut guard = shared.child.lock().expect("child lock poisoned");
        if let Some(child) = guard.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_))) {
                drop(guard);
                set_error(shared, "Recorder exited (is a microphone connected?)");
                return true;
            }
        }
    }
}

fn transcribe(shared: &Arc<Shared>, raw: &PathBuf) {
    let pcm = std::fs::read(raw).unwrap_or_default();
    let _ = std::fs::remove_file(raw);
    if duration_ms(pcm.len()) < MIN_RECORDING_MS {
        set_error(shared, "Nothing recorded");
        return;
    }
    let wav = capture_path(shared.id, "wav");
    if let Err(e) = write_wav16(&wav, &pcm) {
        set_error(shared, format!("Could not write the recording: {e}"));
        return;
    }
    let backend = shared
        .transcriber
        .lock()
        .expect("transcriber lock poisoned")
        .clone();
    let Some(mut cmd) = transcribe_command(&backend, &wav.to_string_lossy()) else {
        set_error(
            shared,
            "No transcriber (set listen.json transcribeCommand or install whisper.cpp)",
        );
        let _ = std::fs::remove_file(&wav);
        return;
    };
    cmd.stdin(Stdio::null()).stderr(Stdio::null());
    let output = cmd.output();
    let _ = std::fs::remove_file(&wav);
    match output {
        Ok(out) if out.status.success() => {
            let text = clean_transcript(&String::from_utf8_lossy(&out.stdout));
            if text.is_empty() {
                set_error(shared, "Nothing was said");
            } else {
                shared
                    .out
                    .lock()
                    .expect("listen out lock poisoned")
                    .push_back(text);
            }
        }
        Ok(out) => set_error(shared, format!("Transcriber failed ({})", out.status)),
        Err(e) => set_error(shared, format!("Transcriber failed to start: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(mut pred: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if pred() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn detect_prefers_configured_commands() {
        let settings = ListenSettings {
            record_command: Some(vec!["rec".into(), "{raw}".into()]),
            transcribe_command: Some(vec!["stt".into(), "{wav}".into()]),
            ..Default::default()
        };
        assert_eq!(
            detect_recorder(&settings),
            Recorder::Custom(vec!["rec".into(), "{raw}".into()])
        );
        assert_eq!(
            detect_transcriber(&settings),
            Transcriber::Custom(vec!["stt".into(), "{wav}".into()])
        );
    }

    #[test]
    fn transcriber_needs_both_program_and_model() {
        let settings = ListenSettings {
            model_path: Some("/definitely/not/here.bin".into()),
            ..Default::default()
        };
        // No model on disk -> may still find a checkout model, but never a
        // half-configured backend.
        match detect_transcriber(&settings) {
            Transcriber::WhisperCli { program, model } => {
                assert!(PathBuf::from(&program).is_file());
                assert!(PathBuf::from(&model).is_file());
            }
            other => assert_eq!(other, Transcriber::None),
        }
    }

    #[test]
    fn substitutes_only_its_own_placeholder() {
        let cmd = record_command(
            &Recorder::Custom(vec!["rec".into(), "-o".into(), "{raw}".into()]),
            "/tmp/a.raw",
        )
        .unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["-o", "/tmp/a.raw"]);
    }

    /// The latency flags are part of the contract: without greedy decode this is a
    /// batch transcriber, not dictation.
    #[test]
    fn whisper_runs_greedy_and_thread_capped() {
        let cmd = transcribe_command(
            &Transcriber::WhisperCli {
                program: "whisper-cli".into(),
                model: "/m.bin".into(),
            },
            "/tmp/a.wav",
        )
        .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2).any(|w| w == ["-bs", "1"]),
            "greedy: {args:?}"
        );
        let threads: usize = args
            .iter()
            .position(|a| a == "-t")
            .and_then(|i| args.get(i + 1))
            .and_then(|t| t.parse().ok())
            .expect("a thread count");
        assert!(
            (1..=8).contains(&threads),
            "threads out of range: {threads}"
        );
    }

    #[test]
    fn missing_backends_report_instead_of_hanging() {
        let handle = spawn_with(Recorder::None, Transcriber::None, 60);
        assert!(!handle.ready());
        handle.start();
        wait_for(
            || handle.take_error().is_some(),
            "an error for the missing recorder",
        );
        assert_eq!(handle.phase(), Phase::Idle);
    }

    // Drives the whole record -> wav -> transcribe path with two stub commands, so it
    // needs a POSIX shell but no microphone and no model.
    #[test]
    #[cfg(unix)]
    fn records_then_transcribes_and_yields_text() {
        // "Recorder": append 1 s of (silent) 16 kHz mono s16le, then sit until killed.
        let recorder = Recorder::Custom(vec![
            "/bin/sh".into(),
            "-c".into(),
            "head -c 32000 /dev/zero > \"$1\"; sleep 30".into(),
            "sh".into(),
            "{raw}".into(),
        ]);
        // "Transcriber": ignore the wav, print a whisper-shaped line.
        let transcriber = Transcriber::Custom(vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo '[00:00:00.000 --> 00:00:01.000]   hello from the mic'".into(),
            "sh".into(),
            "{wav}".into(),
        ]);
        let handle = spawn_with(recorder, transcriber, 60);
        assert!(handle.ready());
        handle.start();
        wait_for(|| handle.phase() == Phase::Recording, "recording to start");
        handle.stop();
        wait_for(
            || handle.take_transcript().as_deref() == Some("hello from the mic"),
            "the transcript",
        );
        wait_for(|| handle.phase() == Phase::Idle, "the engine to go idle");
    }

    #[test]
    #[cfg(unix)]
    fn cancel_discards_the_audio() {
        let recorder = Recorder::Custom(vec![
            "/bin/sh".into(),
            "-c".into(),
            "head -c 32000 /dev/zero > \"$1\"; sleep 30".into(),
            "sh".into(),
            "{raw}".into(),
        ]);
        let transcriber =
            Transcriber::Custom(vec!["/bin/sh".into(), "-c".into(), "echo nope".into()]);
        let handle = spawn_with(recorder, transcriber, 60);
        handle.start();
        wait_for(|| handle.phase() == Phase::Recording, "recording to start");
        handle.cancel();
        wait_for(|| handle.phase() == Phase::Idle, "the engine to go idle");
        assert_eq!(
            handle.take_transcript(),
            None,
            "cancelled audio must not transcribe"
        );
    }

    #[test]
    #[cfg(unix)]
    fn too_short_a_recording_is_dropped() {
        let recorder = Recorder::Custom(vec![
            "/bin/sh".into(),
            "-c".into(),
            // 0.05 s of audio — below MIN_RECORDING_MS.
            "head -c 1600 /dev/zero > \"$1\"; sleep 30".into(),
            "sh".into(),
            "{raw}".into(),
        ]);
        let transcriber = Transcriber::Custom(vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo hallucination".into(),
        ]);
        let handle = spawn_with(recorder, transcriber, 60);
        handle.start();
        wait_for(|| handle.phase() == Phase::Recording, "recording to start");
        handle.stop();
        wait_for(|| handle.take_error().is_some(), "the too-short error");
        assert_eq!(handle.take_transcript(), None);
    }
}
