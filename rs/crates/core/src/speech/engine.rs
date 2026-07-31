//! A single global, serialized speech queue: exactly one utterance plays at a time,
//! spoken via whatever backend [`detect`] finds. Plain `std::thread` +
//! `std::sync::mpsc` — no async runtime needed for "run one shell command at a time".

use super::SpeechSettings;
use std::collections::VecDeque;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Queue capacity before the oldest queued (not yet spoken) utterance is dropped.
const QUEUE_CAP: usize = 64;
/// Poll interval while waiting for the in-flight backend process to exit, so
/// [`SpeechHandle::stop_all`] can interrupt it promptly without holding the child
/// lock across a blocking wait.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The TTS backend a pane's speech is rendered through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// User-supplied command; `{text}` in any arg is replaced with the utterance text.
    Custom(Vec<String>),
    SpdSay,
    EspeakNg,
    Say,
    /// No usable backend was found (or none configured) — utterances are dropped.
    None,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Custom(_) => "custom",
            Backend::SpdSay => "spd-say",
            Backend::EspeakNg => "espeak-ng",
            Backend::Say => "say",
            Backend::None => "none",
        }
    }
}

#[cfg(unix)]
fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file()))
        .unwrap_or(false)
}

/// Pick a backend: the configured custom command if present, else the first backend
/// found on `PATH` for the current platform, else [`Backend::None`].
pub fn detect(settings: &SpeechSettings) -> Backend {
    if let Some(cmd) = &settings.command_template {
        if !cmd.is_empty() {
            return Backend::Custom(cmd.clone());
        }
    }
    #[cfg(target_os = "macos")]
    {
        if on_path("say") {
            return Backend::Say;
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if on_path("spd-say") {
            return Backend::SpdSay;
        }
        if on_path("espeak-ng") {
            return Backend::EspeakNg;
        }
    }
    Backend::None
}

fn build_command(backend: &Backend, text: &str) -> Option<Command> {
    match backend {
        Backend::None => None,
        Backend::SpdSay => {
            let mut c = Command::new("spd-say");
            c.arg("--wait").arg(text);
            Some(c)
        }
        Backend::EspeakNg => {
            let mut c = Command::new("espeak-ng");
            c.arg(text);
            Some(c)
        }
        Backend::Say => {
            let mut c = Command::new("say");
            c.arg(text);
            Some(c)
        }
        Backend::Custom(template) => {
            let (program, args) = template.split_first()?;
            let mut c = Command::new(program);
            for arg in args {
                c.arg(arg.replace("{text}", text));
            }
            Some(c)
        }
    }
}

/// One queued item: which pane spoke it, an optional label (e.g. the pane name), and
/// the already-[`normalize`](super::normalize)-d text.
#[derive(Debug, Clone)]
pub struct Utterance {
    pub pane_id: String,
    pub label: String,
    pub text: String,
}

impl Utterance {
    fn spoken_text(&self) -> String {
        if self.label.is_empty() {
            self.text.clone()
        } else {
            format!("{}. {}", self.label, self.text)
        }
    }
}

/// A point-in-time snapshot of the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechStatus {
    pub backend: String,
    pub speaking_pane: Option<String>,
    pub queue_len: usize,
    pub muted: bool,
}

struct Shared {
    queue: Mutex<VecDeque<Utterance>>,
    muted: AtomicBool,
    backend: Mutex<Backend>,
    speaking_pane: Mutex<Option<String>>,
    current_child: Mutex<Option<Child>>,
    /// Bumped by `stop_all` so an in-flight `speak` can notice it was cancelled even
    /// after the child has already been reaped by a race with a fresh enqueue.
    generation: AtomicU64,
}

/// Push `item` onto `queue`, dropping the oldest entry first if already at `cap`.
fn push_bounded(queue: &mut VecDeque<Utterance>, cap: usize, item: Utterance) {
    if queue.len() >= cap {
        queue.pop_front();
    }
    queue.push_back(item);
}

pub struct SpeechEngine;

impl SpeechEngine {
    /// Detect a backend from `settings` and start the engine thread.
    pub fn spawn(settings: SpeechSettings) -> SpeechHandle {
        let backend = detect(&settings);
        spawn_with_backend(backend, settings.muted)
    }
}

fn spawn_with_backend(backend: Backend, muted: bool) -> SpeechHandle {
    let shared = Arc::new(Shared {
        queue: Mutex::new(VecDeque::new()),
        muted: AtomicBool::new(muted),
        backend: Mutex::new(backend),
        speaking_pane: Mutex::new(None),
        current_child: Mutex::new(None),
        generation: AtomicU64::new(0),
    });
    let (wake_tx, wake_rx) = mpsc::channel();
    let worker = Arc::clone(&shared);
    thread::spawn(move || run(worker, wake_rx));
    SpeechHandle { shared, wake_tx }
}

/// A cheap, `Clone + Send` handle to the running speech engine.
#[derive(Clone)]
pub struct SpeechHandle {
    shared: Arc<Shared>,
    wake_tx: mpsc::Sender<()>,
}

impl SpeechHandle {
    /// Enqueue an utterance to be spoken once prior ones finish. Drops the oldest
    /// queued (not-yet-spoken) utterance if the queue is already at capacity.
    pub fn enqueue(&self, utterance: Utterance) {
        {
            let mut q = self
                .shared
                .queue
                .lock()
                .expect("speech queue lock poisoned");
            push_bounded(&mut q, QUEUE_CAP, utterance);
        }
        let _ = self.wake_tx.send(());
    }

    /// Discard the queue and kill whatever is currently speaking. Returns as soon as
    /// the in-flight process has been killed, without waiting out its own duration.
    pub fn stop_all(&self) {
        self.generation_bump();
        self.shared
            .queue
            .lock()
            .expect("speech queue lock poisoned")
            .clear();
        self.kill_current_child();
    }

    fn generation_bump(&self) {
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn kill_current_child(&self) {
        let taken = self
            .shared
            .current_child
            .lock()
            .expect("current-child lock poisoned")
            .take();
        if let Some(mut child) = taken {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::SeqCst);
    }

    /// Re-detect the backend for a new custom command template (`None` re-runs
    /// platform auto-detection).
    pub fn set_template(&self, template: Option<Vec<String>>) {
        let settings = SpeechSettings {
            command_template: template,
            ..Default::default()
        };
        let mut backend = self
            .shared
            .backend
            .lock()
            .expect("speech backend lock poisoned");
        *backend = detect(&settings);
    }

    pub fn status(&self) -> SpeechStatus {
        SpeechStatus {
            backend: self
                .shared
                .backend
                .lock()
                .expect("speech backend lock poisoned")
                .name()
                .to_string(),
            speaking_pane: self
                .shared
                .speaking_pane
                .lock()
                .expect("speaking-pane lock poisoned")
                .clone(),
            queue_len: self
                .shared
                .queue
                .lock()
                .expect("speech queue lock poisoned")
                .len(),
            muted: self.shared.muted.load(Ordering::SeqCst),
        }
    }
}

fn run(shared: Arc<Shared>, wake_rx: mpsc::Receiver<()>) {
    while wake_rx.recv().is_ok() {
        loop {
            let next = shared
                .queue
                .lock()
                .expect("speech queue lock poisoned")
                .pop_front();
            let Some(utterance) = next else { break };
            if shared.muted.load(Ordering::SeqCst) {
                continue;
            }
            let generation = shared.generation.load(Ordering::SeqCst);
            *shared
                .speaking_pane
                .lock()
                .expect("speaking-pane lock poisoned") = Some(utterance.pane_id.clone());
            speak(&shared, &utterance, generation);
            *shared
                .speaking_pane
                .lock()
                .expect("speaking-pane lock poisoned") = None;
        }
    }
}

/// Run one utterance's backend command to completion, polling (rather than
/// blocking) so a concurrent `stop_all` can kill the child without waiting on this
/// thread's lock.
fn speak(shared: &Arc<Shared>, utterance: &Utterance, generation: u64) {
    let backend = shared
        .backend
        .lock()
        .expect("speech backend lock poisoned")
        .clone();
    let text = utterance.spoken_text();
    let Some(mut cmd) = build_command(&backend, &text) else {
        return;
    };
    let Ok(child) = cmd.spawn() else { return };
    *shared
        .current_child
        .lock()
        .expect("current-child lock poisoned") = Some(child);

    loop {
        if shared.generation.load(Ordering::SeqCst) != generation {
            // A stop_all fired: kill it ourselves if it hasn't already been taken.
            let taken = shared
                .current_child
                .lock()
                .expect("current-child lock poisoned")
                .take();
            if let Some(mut child) = taken {
                let _ = child.kill();
                let _ = child.wait();
            }
            return;
        }
        let mut guard = shared
            .current_child
            .lock()
            .expect("current-child lock poisoned");
        match guard.as_mut() {
            None => return, // stop_all already reaped it.
            Some(child) => match child.try_wait() {
                Ok(Some(_status)) => {
                    *guard = None;
                    return;
                }
                Ok(None) => {
                    drop(guard);
                    thread::sleep(POLL_INTERVAL);
                }
                Err(_) => {
                    *guard = None;
                    return;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hp-speech-engine-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A shell script that exclusive-appends `START $1` then, after `sleep_secs`,
    /// `END $1` to `log` — used to observe serialization/interleaving from outside.
    fn make_echo_script(
        dir: &std::path::Path,
        log: &std::path::Path,
        sleep_secs: f64,
    ) -> std::path::PathBuf {
        let script = dir.join("speak.sh");
        let contents = format!(
            "#!/bin/sh\necho \"START $1\" >> {log:?}\nsleep {sleep_secs}\necho \"END $1\" >> {log:?}\n"
        );
        std::fs::write(&script, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        script
    }

    fn custom_template(script: &std::path::Path) -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            script.to_string_lossy().into_owned(),
            "{text}".to_string(),
        ]
    }

    #[test]
    fn detect_prefers_custom_template() {
        let settings = SpeechSettings {
            command_template: Some(vec!["say".into(), "{text}".into()]),
            ..Default::default()
        };
        assert_eq!(
            detect(&settings),
            Backend::Custom(vec!["say".into(), "{text}".into()])
        );
    }

    #[test]
    fn push_bounded_drops_oldest_at_capacity() {
        let mut q = VecDeque::new();
        for i in 0..5 {
            push_bounded(
                &mut q,
                3,
                Utterance {
                    pane_id: "p".into(),
                    label: String::new(),
                    text: i.to_string(),
                },
            );
        }
        let texts: Vec<&str> = q.iter().map(|u| u.text.as_str()).collect();
        assert_eq!(texts, vec!["2", "3", "4"]);
    }

    #[test]
    fn none_backend_enqueue_is_a_noop() {
        let handle = spawn_with_backend(Backend::None, false);
        handle.enqueue(Utterance {
            pane_id: "pane-1".into(),
            label: String::new(),
            text: "hello".into(),
        });
        thread::sleep(Duration::from_millis(150));
        let status = handle.status();
        assert_eq!(status.queue_len, 0);
        assert_eq!(status.speaking_pane, None);
    }

    // The harness backend is a `#!/bin/sh` script, so this exercises the engine only
    // where a POSIX shell exists; the serialization logic it pins is platform-agnostic.
    #[test]
    #[cfg(unix)]
    fn alternating_start_end_serialized_and_fifo_per_producer() {
        let dir = scratch_dir("alt");
        let log = dir.join("log.txt");
        let _ = std::fs::remove_file(&log);
        let script = make_echo_script(&dir, &log, 0.05);
        let handle = spawn_with_backend(Backend::Custom(custom_template(&script)), false);

        let h1 = handle.clone();
        let t1 = thread::spawn(move || {
            for i in 0..3 {
                h1.enqueue(Utterance {
                    pane_id: "pane-a".into(),
                    label: format!("a{i}"),
                    text: "hello".into(),
                });
            }
        });
        let h2 = handle.clone();
        let t2 = thread::spawn(move || {
            for i in 0..3 {
                h2.enqueue(Utterance {
                    pane_id: "pane-b".into(),
                    label: format!("b{i}"),
                    text: "world".into(),
                });
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();

        // 6 utterances * 50ms each, plus generous scheduling headroom.
        thread::sleep(Duration::from_millis(2000));

        let contents = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(
            lines.len(),
            12,
            "expected 6 START/END pairs, got: {lines:?}"
        );

        for pair in lines.chunks(2) {
            let [start, end] = pair else {
                panic!("odd number of log lines: {lines:?}")
            };
            assert!(start.starts_with("START "), "not a START line: {start}");
            assert!(end.starts_with("END "), "not an END line: {end}");
            let start_label = start.trim_start_matches("START ");
            let end_label = end.trim_start_matches("END ");
            assert_eq!(
                start_label, end_label,
                "START/END must pair on the same utterance without interleaving: {lines:?}"
            );
        }

        let starts: Vec<&str> = lines.iter().step_by(2).copied().collect();
        for (prefix, count) in [("START a", 3), ("START b", 3)] {
            let order: Vec<u32> = starts
                .iter()
                .filter(|l| l.starts_with(prefix))
                .map(|l| {
                    l[prefix.len()..]
                        .chars()
                        .next()
                        .and_then(|c| c.to_digit(10))
                        .expect("label digit")
                })
                .collect();
            assert_eq!(order.len(), count as usize);
            assert_eq!(order, (0..count).collect::<Vec<_>>(), "not FIFO: {lines:?}");
        }
    }

    // The harness backend is a `#!/bin/sh` script, so this exercises the engine only
    // where a POSIX shell exists; the serialization logic it pins is platform-agnostic.
    #[test]
    #[cfg(unix)]
    fn stop_all_kills_in_flight_and_discards_queue() {
        let dir = scratch_dir("stop");
        let log = dir.join("log.txt");
        let _ = std::fs::remove_file(&log);
        let script = make_echo_script(&dir, &log, 30.0);
        let handle = spawn_with_backend(Backend::Custom(custom_template(&script)), false);

        handle.enqueue(Utterance {
            pane_id: "pane-1".into(),
            label: String::new(),
            text: "first".into(),
        });
        // Let it actually start (START written, child in flight) before queuing more.
        thread::sleep(Duration::from_millis(300));
        handle.enqueue(Utterance {
            pane_id: "pane-1".into(),
            label: String::new(),
            text: "second".into(),
        });
        assert_eq!(handle.status().queue_len, 1);

        let started = Instant::now();
        handle.stop_all();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "stop_all took {elapsed:?}"
        );

        thread::sleep(Duration::from_millis(300));
        let contents = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(contents.contains("START first"));
        assert!(
            !contents.contains("END"),
            "killed process must never log END: {contents:?}"
        );
        assert_eq!(
            handle.status().queue_len,
            0,
            "queued utterances must be discarded"
        );
    }
}
