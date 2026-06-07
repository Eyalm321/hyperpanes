//! Port of `src/main/ai/ai-service.ts` — the façade wiring pane_buffer → scheduler →
//! redactor → ollama → a pane subtitle. Default-OFF. Takes its settings + memory file
//! paths as PARAMETERS (dependency injection) — it does NOT call `persistence::paths`, so
//! this track stays independent of `persistence-cli`.
//!
//! There is no `ai-service.test.ts` upstream; the tests here cover the façade's own
//! logic (default-off gating, settings persistence, the summarize job's skip/ok/fail
//! decisions, and context reconciliation).
//!
//! Adaptations from the TS source, all driven by Rust's ownership model:
//!  - The Ollama call is abstracted behind the [`Summarizer`] trait so the façade is
//!    unit-testable without a network (the real [`crate::ai::ollama::OllamaClient`]
//!    implements it).
//!  - The scheduler's async `runJob` promise is modelled with the scheduler's
//!    `InFlight`/`complete` pair: when a uid comes due the scheduler records it (via
//!    [`AiService::next_due`]); the live driver runs [`AiService::run_job`] then reports
//!    the result with [`AiService::complete_job`]. The scheduler's online/offline
//!    transition is forwarded through a shared signal cell.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::ai::ai_store::{AiMemoryStore, PanePatch, ProjectPatch};
use crate::ai::ollama::{OllamaClient, SummarizeInput};
use crate::ai::pane_buffer::PaneTailBuffer;
use crate::ai::redactor::redact;
use crate::ai::scheduler::{JobResult, JobStart, SchedulerConfig, SummaryScheduler};

const SYSTEM_PROMPT: &str = "You label what a developer is doing in one terse present-tense phrase \
(max 8 words). No trailing punctuation. Never include secrets, paths, or code. \
If unclear, answer 'working'.";

/// The Ollama call, abstracted so the façade can be tested without a network.
pub trait Summarizer {
    /// Summarize `prompt` under `system`; `Err` on any failure.
    fn run_summary(
        &self,
        system: &str,
        prompt: &str,
    ) -> impl std::future::Future<Output = Result<String, String>>;
    /// Reachability check; never errors.
    fn check_alive(&self) -> impl std::future::Future<Output = bool>;
    /// Live-update the endpoint/model the next call should use.
    fn configure(&mut self, endpoint: &str, model: &str);
}

impl Summarizer for OllamaClient {
    async fn run_summary(&self, system: &str, prompt: &str) -> Result<String, String> {
        self.summarize(&SummarizeInput {
            system: system.to_string(),
            prompt: prompt.to_string(),
        })
        .await
    }
    async fn check_alive(&self) -> bool {
        self.ping().await
    }
    fn configure(&mut self, endpoint: &str, model: &str) {
        OllamaClient::configure(
            self,
            crate::ai::ollama::OllamaPatch {
                endpoint: Some(endpoint.to_string()),
                model: Some(model.to_string()),
                ..Default::default()
            },
        );
    }
}

/// Persisted config (ai-settings.json). Master enable is OFF by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSettings {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    pub settle_ms: i64,
    pub max_staleness_sec: i64,
    pub concurrency: usize,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:11434".to_string(),
            model: "gemma3:4b".to_string(),
            settle_ms: 1500,
            max_staleness_sec: 180,
            concurrency: 1,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialSettings {
    enabled: Option<bool>,
    endpoint: Option<String>,
    model: Option<String>,
    settle_ms: Option<i64>,
    max_staleness_sec: Option<i64>,
    concurrency: Option<usize>,
}

/// A patch for [`AiService::configure`] — everything except the master enable.
#[derive(Debug, Clone, Default)]
pub struct AiSettingsPatch {
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub settle_ms: Option<i64>,
    pub max_staleness_sec: Option<i64>,
    pub concurrency: Option<usize>,
}

/// A minimal project descriptor passed from the cwd tap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiProjectRef {
    pub path: String,
    pub name: String,
}

/// A window's published view of one watched pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiPanePublish {
    pub pane_id: String,
    pub session_uid: String,
    pub label: String,
    pub muted: bool,
}

/// Status surfaced to the renderer / preferences UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiStatus {
    pub enabled: bool,
    pub online: bool,
    pub endpoint: String,
    pub model: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct PaneCtx {
    pane_id: String,
    window_id: i64,
    label: String,
    cwd: String,
    project_path: Option<String>,
    project_name: String,
    muted: bool,
}

pub type PushMeta = Box<dyn FnMut(i64, &str, Vec<(String, String)>)>;
pub type OnStatus = Box<dyn FnMut(&AiStatus)>;

// Cheap, stable content fingerprint so we don't re-summarize unchanged output.
fn fingerprint(text: &str) -> String {
    let mut h: i32 = 5381;
    let mut len = 0usize;
    for c in text.chars() {
        h = h
            .wrapping_shl(5)
            .wrapping_add(h)
            .wrapping_add(c as i32);
        len += 1;
    }
    format!("{len}:{h}")
}

fn basename(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

pub struct AiService<S: Summarizer> {
    settings_path: PathBuf,
    push_meta: PushMeta,
    on_status: OnStatus,

    buffer: PaneTailBuffer,
    client: S,
    store: AiMemoryStore,
    scheduler: SummaryScheduler,

    ctx_by_uid: HashMap<String, PaneCtx>,
    last_hash: HashMap<String, String>,
    published_by_window: HashMap<i64, HashSet<String>>,

    settings: AiSettings,
    online: bool,
    last_error: Option<String>,

    // wiring between the embedded scheduler's callbacks and this façade
    ready: Rc<RefCell<VecDeque<String>>>,
    status_signal: Rc<RefCell<Option<(bool, Option<String>)>>>,
}

impl<S: Summarizer> AiService<S> {
    pub fn new(
        settings_path: PathBuf,
        memory_path: PathBuf,
        client: S,
        push_meta: PushMeta,
        on_status: OnStatus,
    ) -> Self {
        let defaults = AiSettings::default();
        let ready: Rc<RefCell<VecDeque<String>>> = Rc::new(RefCell::new(VecDeque::new()));
        let status_signal: Rc<RefCell<Option<(bool, Option<String>)>>> =
            Rc::new(RefCell::new(None));

        let ready_cb = ready.clone();
        let sig_cb = status_signal.clone();
        let scheduler = SummaryScheduler::new(
            SchedulerConfig {
                settle_ms: defaults.settle_ms,
                max_staleness_sec: defaults.max_staleness_sec,
                concurrency: defaults.concurrency,
            },
            move |uid| {
                ready_cb.borrow_mut().push_back(uid.to_string());
                JobStart::InFlight
            },
            Some(Box::new(move |online, err| {
                *sig_cb.borrow_mut() = Some((online, err.map(str::to_string)));
            })),
        );

        Self {
            settings_path,
            push_meta,
            on_status,
            buffer: PaneTailBuffer::new(),
            client,
            store: AiMemoryStore::new(memory_path),
            scheduler,
            ctx_by_uid: HashMap::new(),
            last_hash: HashMap::new(),
            published_by_window: HashMap::new(),
            settings: defaults,
            online: false,
            last_error: None,
            ready,
            status_signal,
        }
    }

    /// Load persisted settings + memory and start if it was left enabled.
    pub fn init(&mut self) {
        self.settings = self.load_settings();
        self.store.load();
        self.client_configure();
        self.scheduler.set_config(self.sched_config());
        if self.settings.enabled {
            self.scheduler.start();
        }
        self.emit_status();
    }

    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    pub fn status(&self) -> AiStatus {
        AiStatus {
            enabled: self.settings.enabled,
            online: self.online,
            endpoint: self.settings.endpoint.clone(),
            model: self.settings.model.clone(),
            last_error: self.last_error.clone(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.settings.enabled == enabled {
            return;
        }
        self.settings.enabled = enabled;
        self.save_settings();
        if enabled {
            self.scheduler.start();
        } else {
            self.scheduler.stop();
            self.online = false;
        }
        self.emit_status();
    }

    /// Live-update endpoint/model/cadence from Preferences.
    pub fn configure(&mut self, patch: AiSettingsPatch) {
        if let Some(endpoint) = patch.endpoint {
            self.settings.endpoint = endpoint;
        }
        if let Some(model) = patch.model {
            self.settings.model = model;
        }
        if let Some(settle_ms) = patch.settle_ms {
            self.settings.settle_ms = settle_ms;
        }
        if let Some(max_staleness_sec) = patch.max_staleness_sec {
            self.settings.max_staleness_sec = max_staleness_sec;
        }
        if let Some(concurrency) = patch.concurrency {
            self.settings.concurrency = concurrency;
        }
        self.save_settings();
        self.client_configure();
        self.scheduler.set_config(self.sched_config());
        self.emit_status();
    }

    // ---- taps from the session output ----
    pub fn on_data(&mut self, uid: &str, data: &str) {
        if !self.settings.enabled {
            return;
        }
        match self.ctx_by_uid.get(uid) {
            Some(ctx) if !ctx.muted => {}
            _ => return,
        }
        self.buffer.append(uid, data);
        self.scheduler.note_output(uid);
    }

    pub fn on_cwd(&mut self, uid: &str, cwd: &str, project: Option<AiProjectRef>) {
        let Some(ctx) = self.ctx_by_uid.get_mut(uid) else {
            return;
        };
        ctx.cwd = cwd.to_string();
        if let Some(project) = project {
            ctx.project_path = Some(project.path);
            ctx.project_name = project.name;
        }
    }

    pub fn on_session_exit(&mut self, uid: &str) {
        self.buffer.clear(uid);
        self.scheduler.forget(uid);
        self.ctx_by_uid.remove(uid);
        self.last_hash.remove(uid);
    }

    /// A window publishes its live set of watched panes. We reconcile our context
    /// map for THAT window only, then prune the store against the union of all
    /// windows' published panes.
    pub fn on_pane_context(&mut self, window_id: i64, panes: &[AiPanePublish]) {
        let mut seen = HashSet::new();
        for p in panes {
            seen.insert(p.session_uid.clone());
            let prev = self.ctx_by_uid.get(&p.session_uid);
            let was_muted = prev.map(|c| c.muted).unwrap_or(false);
            let (cwd, project_path, project_name) = prev
                .map(|c| (c.cwd.clone(), c.project_path.clone(), c.project_name.clone()))
                .unwrap_or_default();
            self.ctx_by_uid.insert(
                p.session_uid.clone(),
                PaneCtx {
                    pane_id: p.pane_id.clone(),
                    window_id,
                    label: p.label.clone(),
                    cwd,
                    project_path,
                    project_name,
                    muted: p.muted,
                },
            );
            if p.muted && !was_muted {
                (self.push_meta)(
                    window_id,
                    &p.pane_id,
                    vec![("ai.subtitle".to_string(), String::new())],
                );
                self.scheduler.forget(&p.session_uid);
                self.last_hash.remove(&p.session_uid);
            }
        }
        // Drop context for this window's panes that are no longer published.
        let to_remove: Vec<String> = self
            .ctx_by_uid
            .iter()
            .filter(|(uid, ctx)| ctx.window_id == window_id && !seen.contains(*uid))
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in to_remove {
            self.ctx_by_uid.remove(&uid);
            self.buffer.clear(&uid);
            self.scheduler.forget(&uid);
            self.last_hash.remove(&uid);
        }
        self.published_by_window
            .insert(window_id, panes.iter().map(|p| p.pane_id.clone()).collect());
        self.prune_panes();
    }

    /// A window closed — forget its panes and re-prune.
    pub fn drop_window(&mut self, window_id: i64) {
        self.published_by_window.remove(&window_id);
        let to_remove: Vec<String> = self
            .ctx_by_uid
            .iter()
            .filter(|(_, ctx)| ctx.window_id == window_id)
            .map(|(uid, _)| uid.clone())
            .collect();
        for uid in to_remove {
            self.ctx_by_uid.remove(&uid);
            self.buffer.clear(&uid);
            self.scheduler.forget(&uid);
            self.last_hash.remove(&uid);
        }
        self.prune_panes();
    }

    // Prune store pane records to the union of all windows' published panes. Only
    // prunes once at least one window has published, so an early/empty state can't
    // wipe persisted memory.
    fn prune_panes(&mut self) {
        let mut all: Vec<String> = Vec::new();
        for set in self.published_by_window.values() {
            for id in set {
                all.push(id.clone());
            }
        }
        if !all.is_empty() {
            self.store.prune_panes_except(&all);
        }
    }

    pub fn shutdown(&mut self) {
        self.scheduler.stop();
        self.store.flush();
    }

    // ---- scheduler <-> live-driver bridge ----

    /// Advance the scheduler's virtual clock (the live driver ticks real elapsed ms).
    pub fn tick(&mut self, ms: i64) {
        self.scheduler.advance(ms);
        self.drain_status_signal();
    }

    /// Pop the next uid the scheduler has dispatched, if any.
    pub fn next_due(&mut self) -> Option<String> {
        self.ready.borrow_mut().pop_front()
    }

    /// Report a finished [`run_job`] back to the scheduler (online/offline tracking,
    /// backoff, re-queue), forwarding any status transition to `on_status`.
    pub fn complete_job(&mut self, uid: &str, result: JobResult) {
        self.scheduler.complete(uid, result);
        self.drain_status_signal();
    }

    fn drain_status_signal(&mut self) {
        let signal = self.status_signal.borrow_mut().take();
        if let Some((online, err)) = signal {
            self.online = online;
            if let Some(e) = err {
                self.last_error = Some(e);
            }
            self.emit_status();
        }
    }

    // ---- the job the scheduler dispatches ----
    pub async fn run_job(&mut self, uid: &str) -> JobResult {
        if !self.settings.enabled {
            return JobResult::Skip;
        }
        let ctx = match self.ctx_by_uid.get(uid) {
            Some(c) if !c.muted => c.clone(),
            _ => return JobResult::Skip,
        };
        let snap = self.buffer.snapshot(uid);
        if snap.alt_screen {
            return JobResult::Skip; // full-screen TUI: raw tail is redraw noise
        }
        let text = snap.text.trim();
        if text.chars().count() < 3 {
            return JobResult::Skip;
        }
        let hash = fingerprint(text);
        if self.last_hash.get(uid) == Some(&hash) {
            return JobResult::Skip; // nothing new since last summary
        }

        let prior = self
            .store
            .get_pane(&ctx.pane_id)
            .map(|p| p.summary.clone())
            .unwrap_or_default();
        let prompt = build_prompt(&ctx, &prior, &redact(&snap.text));

        let line = match self.client.run_summary(SYSTEM_PROMPT, &prompt).await {
            Ok(line) => line,
            Err(err) => {
                self.last_error = Some(err);
                return JobResult::Fail;
            }
        };

        let line = redact(&line);
        self.last_hash.insert(uid.to_string(), hash);
        (self.push_meta)(
            ctx.window_id,
            &ctx.pane_id,
            vec![("ai.subtitle".to_string(), line.clone())],
        );
        self.store.upsert_pane(
            &ctx.pane_id,
            PanePatch {
                project_path: Some(ctx.project_path.clone()),
                label: Some(ctx.label.clone()),
                summary: Some(line.clone()),
                last_cwd: Some(ctx.cwd.clone()),
                ..Default::default()
            },
        );
        if let Some(project_path) = &ctx.project_path {
            self.store.upsert_project(
                project_path,
                ProjectPatch {
                    name: Some(ctx.project_name.clone()),
                    summary: Some(line.clone()),
                    ..Default::default()
                },
            );
        }
        JobResult::Ok
    }

    /// Ping the server and update online state (the live layer calls this on
    /// enable / configure). Mirrors TS `refreshStatus`.
    pub async fn refresh_status(&mut self) {
        let ok = self.client.check_alive().await;
        self.online = ok;
        if ok {
            self.last_error = None;
        }
        self.emit_status();
    }

    fn sched_config(&self) -> SchedulerConfig {
        SchedulerConfig {
            settle_ms: self.settings.settle_ms,
            max_staleness_sec: self.settings.max_staleness_sec,
            concurrency: self.settings.concurrency,
        }
    }

    fn client_configure(&mut self) {
        let endpoint = self.settings.endpoint.clone();
        let model = self.settings.model.clone();
        self.client.configure(&endpoint, &model);
    }

    fn emit_status(&mut self) {
        let st = self.status();
        (self.on_status)(&st);
    }

    fn load_settings(&self) -> AiSettings {
        let mut settings = AiSettings::default();
        if let Ok(text) = std::fs::read_to_string(&self.settings_path) {
            if let Ok(parsed) = serde_json::from_str::<PartialSettings>(&text) {
                if let Some(v) = parsed.enabled {
                    settings.enabled = v;
                }
                if let Some(v) = parsed.endpoint {
                    settings.endpoint = v;
                }
                if let Some(v) = parsed.model {
                    settings.model = v;
                }
                if let Some(v) = parsed.settle_ms {
                    settings.settle_ms = v;
                }
                if let Some(v) = parsed.max_staleness_sec {
                    settings.max_staleness_sec = v;
                }
                if let Some(v) = parsed.concurrency {
                    settings.concurrency = v;
                }
            }
        }
        settings
    }

    fn save_settings(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.settings) {
            if let Err(err) = std::fs::write(&self.settings_path, json) {
                eprintln!("failed to write ai-settings.json: {err}");
            }
        }
    }
}

fn build_prompt(ctx: &PaneCtx, prior: &str, redacted_tail: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !ctx.label.is_empty() {
        lines.push(format!("Pane label: {}", ctx.label));
    }
    if !ctx.cwd.is_empty() {
        lines.push(format!("Directory: {}", basename(&ctx.cwd)));
    }
    if !prior.is_empty() {
        lines.push(format!("Previous summary: {prior}"));
    }
    lines.push(String::new());
    lines.push("Recent terminal output:".to_string());
    lines.push(redacted_tail.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fake summarizer with a canned response, plus a record of how it was called.
    #[derive(Clone)]
    struct Fake {
        resp: Result<String, String>,
        alive: bool,
        calls: Rc<RefCell<Vec<(String, String)>>>, // (system, prompt)
    }
    impl Fake {
        fn ok(line: &str) -> Self {
            Self {
                resp: Ok(line.to_string()),
                alive: true,
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }
        fn err(msg: &str) -> Self {
            Self {
                resp: Err(msg.to_string()),
                alive: false,
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }
    impl Summarizer for Fake {
        async fn run_summary(&self, system: &str, prompt: &str) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((system.to_string(), prompt.to_string()));
            self.resp.clone()
        }
        async fn check_alive(&self) -> bool {
            self.alive
        }
        fn configure(&mut self, _endpoint: &str, _model: &str) {}
    }

    type Meta = Rc<RefCell<Vec<(i64, String, Vec<(String, String)>)>>>;

    struct TempPaths {
        dir: PathBuf,
    }
    impl TempPaths {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("ai-svc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
        fn settings(&self) -> PathBuf {
            self.dir.join("ai-settings.json")
        }
        fn memory(&self) -> PathBuf {
            self.dir.join("ai-memory.json")
        }
    }
    impl Drop for TempPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn build(tp: &TempPaths, client: Fake) -> (AiService<Fake>, Meta) {
        let meta: Meta = Rc::new(RefCell::new(Vec::new()));
        let meta_cb = meta.clone();
        let svc = AiService::new(
            tp.settings(),
            tp.memory(),
            client,
            Box::new(move |win, pane, m| meta_cb.borrow_mut().push((win, pane.to_string(), m))),
            Box::new(|_status| {}),
        );
        (svc, meta)
    }

    fn publish(svc: &mut AiService<Fake>, window: i64, uid: &str, pane: &str, muted: bool) {
        svc.on_pane_context(
            window,
            &[AiPanePublish {
                pane_id: pane.to_string(),
                session_uid: uid.to_string(),
                label: "shell".to_string(),
                muted,
            }],
        );
    }

    #[test]
    fn defaults_to_disabled_and_ignores_output_when_off() {
        let tp = TempPaths::new();
        let (mut svc, _meta) = build(&tp, Fake::ok("x"));
        svc.init();
        assert!(!svc.enabled());
        assert!(!svc.status().enabled);
        // off: on_data is a no-op even if a pane is published
        publish(&mut svc, 1, "u1", "p1", false);
        svc.on_data("u1", "lots of output here\n"); // must not panic / buffer
    }

    #[test]
    fn set_enabled_persists_and_reflects_in_status() {
        let tp = TempPaths::new();
        {
            let (mut svc, _meta) = build(&tp, Fake::ok("x"));
            svc.init();
            svc.set_enabled(true);
            assert!(svc.enabled());
        }
        // a fresh service loads the persisted enabled flag
        let (mut svc2, _meta2) = build(&tp, Fake::ok("x"));
        svc2.init();
        assert!(svc2.enabled());
    }

    #[test]
    fn configure_persists_endpoint_and_model() {
        let tp = TempPaths::new();
        let (mut svc, _meta) = build(&tp, Fake::ok("x"));
        svc.init();
        svc.configure(AiSettingsPatch {
            endpoint: Some("http://192.168.0.11:11434".to_string()),
            model: Some("gemma3:4b".to_string()),
            ..Default::default()
        });
        let st = svc.status();
        assert_eq!(st.endpoint, "http://192.168.0.11:11434");
        assert_eq!(st.model, "gemma3:4b");

        let (mut svc2, _m) = build(&tp, Fake::ok("x"));
        svc2.init();
        assert_eq!(svc2.status().endpoint, "http://192.168.0.11:11434");
    }

    #[tokio::test]
    async fn run_job_skips_when_disabled() {
        let tp = TempPaths::new();
        let (mut svc, _meta) = build(&tp, Fake::ok("did things"));
        svc.init(); // disabled by default
        assert_eq!(svc.run_job("u1").await, JobResult::Skip);
    }

    #[tokio::test]
    async fn run_job_skips_short_unchanged_and_muted() {
        let tp = TempPaths::new();
        let (mut svc, _meta) = build(&tp, Fake::ok("did things"));
        svc.init();
        svc.set_enabled(true);

        // no context yet -> skip
        assert_eq!(svc.run_job("u1").await, JobResult::Skip);

        publish(&mut svc, 1, "u1", "p1", false);
        // too little text -> skip
        svc.on_data("u1", "hi\n");
        assert_eq!(svc.run_job("u1").await, JobResult::Skip);

        // enough text -> ok once, then skip (unchanged hash)
        svc.on_data("u1", "running the build now\n");
        assert_eq!(svc.run_job("u1").await, JobResult::Ok);
        assert_eq!(svc.run_job("u1").await, JobResult::Skip);

        // muted pane -> skip
        publish(&mut svc, 1, "u2", "p2", true);
        svc.on_data("u2", "anything at all here\n");
        assert_eq!(svc.run_job("u2").await, JobResult::Skip);
    }

    #[tokio::test]
    async fn run_job_happy_path_pushes_subtitle_and_persists() {
        let tp = TempPaths::new();
        let fake = Fake::ok("building the project");
        let calls = fake.calls.clone();
        let (mut svc, meta) = build(&tp, fake);
        svc.init();
        svc.set_enabled(true);
        publish(&mut svc, 7, "u1", "p1", false);
        svc.on_cwd(
            "u1",
            "/home/dev/myrepo",
            Some(AiProjectRef {
                path: "/home/dev/myrepo".to_string(),
                name: "myrepo".to_string(),
            }),
        );
        svc.on_data("u1", "cargo build --release\ncompiling...\n");

        assert_eq!(svc.run_job("u1").await, JobResult::Ok);

        // subtitle pushed to the right window/pane
        let m = meta.borrow();
        let last = m.last().unwrap();
        assert_eq!(last.0, 7);
        assert_eq!(last.1, "p1");
        assert_eq!(last.2, vec![("ai.subtitle".to_string(), "building the project".to_string())]);

        // prompt was built with label + directory basename
        let c = calls.borrow();
        let (_system, prompt) = c.last().unwrap();
        assert!(prompt.contains("Pane label: shell"));
        assert!(prompt.contains("Directory: myrepo"));
        assert!(prompt.contains("Recent terminal output:"));

        // persisted into pane + project memory
        assert_eq!(svc.store.get_pane("p1").unwrap().summary, "building the project");
        assert_eq!(
            svc.store.get_project("/home/dev/myrepo").unwrap().summary,
            "building the project"
        );
    }

    #[tokio::test]
    async fn run_job_returns_fail_and_records_error_on_summarizer_error() {
        let tp = TempPaths::new();
        let (mut svc, _meta) = build(&tp, Fake::err("connection refused"));
        svc.init();
        svc.set_enabled(true);
        publish(&mut svc, 1, "u1", "p1", false);
        svc.on_data("u1", "some real output here\n");
        assert_eq!(svc.run_job("u1").await, JobResult::Fail);
        assert_eq!(svc.status().last_error.as_deref(), Some("connection refused"));
    }

    #[tokio::test]
    async fn run_job_redacts_secrets_in_the_subtitle() {
        let tp = TempPaths::new();
        // The model echoes a secret; the façade must scrub it before display.
        let (mut svc, meta) = build(&tp, Fake::ok("leaked SECRET=hunter2"));
        svc.init();
        svc.set_enabled(true);
        publish(&mut svc, 1, "u1", "p1", false);
        svc.on_data("u1", "exporting some variables now\n");
        assert_eq!(svc.run_job("u1").await, JobResult::Ok);
        let m = meta.borrow();
        let (_w, _p, kv) = m.last().unwrap();
        assert_eq!(kv[0].1, "leaked SECRET=[REDACTED]");
    }

    #[test]
    fn on_session_exit_and_pruning_clear_state() {
        let tp = TempPaths::new();
        let (mut svc, _meta) = build(&tp, Fake::ok("x"));
        svc.init();
        svc.set_enabled(true);
        publish(&mut svc, 1, "u1", "p1", false);
        svc.store.upsert_pane("p1", PanePatch::default());
        svc.on_session_exit("u1");
        assert!(svc.ctx_by_uid.get("u1").is_none());
        // a later publish with a different pane prunes the now-orphaned p1 record
        publish(&mut svc, 1, "u2", "p2", false);
        assert!(svc.store.get_pane("p1").is_none());
    }
}
