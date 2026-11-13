use super::*;
use std::cell::RefCell;

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const NOW: i64 = 100 * HOUR;

fn idle_obs() -> SpecPaneObs {
    SpecPaneObs {
        pane_id: "pane-aaaaaaaa-1".into(),
        goal: "g1".into(),
        parent: Some("orch".into()),
        project: Some("/w/proj".into()),
        meta_done: false,
        idle_override: IdleOverride::Default,
        busy: false,
        last_output_at: Some(NOW - 4 * HOUR),
        peer: Some(PeerObs {
            status: PeerStatus::Idle,
            status_updated_at: Some(NOW - 4 * HOUR),
            pid: 10,
            started_at: Some(NOW - 9 * HOUR),
        }),
        asking: false,
        last_report: None,
        last_inbound_ts: None,
        last_parent_msg_ts: None,
        last_message_ts: None,
        claimed_tasks: 0,
        worker_children: 0,
        descendants: Descendants::None,
        pending_nudges: 0,
        report_file: ReportFile::NotSet,
        rss_kb: Some(1000),
        session: Some(SessionInfo {
            session_id: "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a".into(),
            cwd: "/w/proj".into(),
            config_dir: String::new(),
        }),
        cwd: Some("/w/proj".into()),
    }
}

/// Closing config (the shipped default is dry-run; the policy tests exercise real closes).
fn cfg() -> SweepCfg {
    SweepCfg {
        dry_run: false,
        ..SweepCfg::default()
    }
}

#[test]
fn shipped_default_is_dry_run() {
    assert!(SweepCfg::default().dry_run);
    assert!(SweepCfg::default().enabled);
}

fn v(o: &SpecPaneObs) -> Verdict {
    decide(o, &cfg(), NOW)
}

// ---- triggers ----

#[test]
fn idle_trigger_closes_after_three_hours() {
    assert_eq!(v(&idle_obs()), Verdict::Close(CloseReason::Idle));
    let mut o = idle_obs();
    o.last_output_at = Some(NOW - 2 * HOUR);
    o.peer.as_mut().unwrap().status_updated_at = Some(NOW - 2 * HOUR);
    assert_eq!(v(&o), Verdict::Keep(SkipReason::NotIdleLongEnough));
}

#[test]
fn recent_message_resets_idle_since() {
    let mut o = idle_obs();
    o.last_message_ts = Some(NOW - 10 * MIN);
    assert_eq!(v(&o), Verdict::Keep(SkipReason::NotIdleLongEnough));
}

#[test]
fn done_via_meta_closes_after_thirty_minutes() {
    let mut o = idle_obs();
    o.meta_done = true;
    o.last_output_at = Some(NOW - 31 * MIN);
    o.peer.as_mut().unwrap().status_updated_at = Some(NOW - 31 * MIN);
    assert_eq!(v(&o), Verdict::Close(CloseReason::GoalDone));
    o.last_output_at = Some(NOW - 20 * MIN);
    assert_eq!(v(&o), Verdict::Keep(SkipReason::NotIdleLongEnough));
}

#[test]
fn done_via_report_closes_after_thirty_minutes() {
    let mut o = idle_obs();
    o.last_report = Some(Report {
        kind: ReportKind::Done,
        ts: NOW - HOUR,
    });
    o.last_output_at = Some(NOW - 45 * MIN);
    o.peer.as_mut().unwrap().status_updated_at = Some(NOW - 45 * MIN);
    o.last_message_ts = Some(NOW - 45 * MIN);
    assert_eq!(v(&o), Verdict::Close(CloseReason::GoalDone));
}

#[test]
fn idle_close_off_disables_both_triggers() {
    let mut o = idle_obs();
    o.idle_override = IdleOverride::Off;
    assert_eq!(v(&o), Verdict::Keep(SkipReason::IdleCloseOff));
    o.meta_done = true;
    assert_eq!(v(&o), Verdict::Keep(SkipReason::IdleCloseOff));
}

#[test]
fn idle_close_number_overrides_hours() {
    let mut o = idle_obs(); // idle 4h
    o.idle_override = IdleOverride::Hours(6.0);
    assert_eq!(v(&o), Verdict::Keep(SkipReason::NotIdleLongEnough));
    o.idle_override = IdleOverride::Hours(1.0);
    assert_eq!(v(&o), Verdict::Close(CloseReason::Idle));
}

#[test]
fn idle_close_meta_parsing() {
    let m = |v: &str| {
        let mut b = BTreeMap::new();
        b.insert(META_IDLE_CLOSE.to_string(), v.to_string());
        IdleOverride::from_meta(&b)
    };
    assert_eq!(m("off"), IdleOverride::Off);
    assert_eq!(m("OFF"), IdleOverride::Off);
    assert_eq!(m("6"), IdleOverride::Hours(6.0));
    assert_eq!(m("1.5"), IdleOverride::Hours(1.5));
    assert_eq!(m("junk"), IdleOverride::Default);
    assert_eq!(
        IdleOverride::from_meta(&BTreeMap::new()),
        IdleOverride::Default
    );
}

// ---- skip rules ----

#[test]
fn each_skip_rule_blocks_the_close() {
    type Case = (&'static str, Box<dyn Fn(&mut SpecPaneObs)>, SkipReason);
    let cases: Vec<Case> = vec![
        ("busy", Box::new(|o| o.busy = true), SkipReason::Busy),
        ("no peer", Box::new(|o| o.peer = None), SkipReason::NoPeer),
        (
            "peer busy",
            Box::new(|o| o.peer.as_mut().unwrap().status = PeerStatus::Busy),
            SkipReason::PeerNotIdle,
        ),
        (
            "peer shell",
            Box::new(|o| o.peer.as_mut().unwrap().status = PeerStatus::Shell),
            SkipReason::PeerNotIdle,
        ),
        (
            "peer unknown",
            Box::new(|o| o.peer.as_mut().unwrap().status = PeerStatus::Unknown),
            SkipReason::PeerNotIdle,
        ),
        ("asking", Box::new(|o| o.asking = true), SkipReason::Asking),
        (
            "claimed",
            Box::new(|o| o.claimed_tasks = 1),
            SkipReason::ClaimedTask,
        ),
        (
            "worker",
            Box::new(|o| o.worker_children = 1),
            SkipReason::WorkerRunning,
        ),
        (
            "descendant",
            Box::new(|o| o.descendants = Descendants::Some),
            SkipReason::LiveDescendant,
        ),
        (
            "descendant unknown",
            Box::new(|o| o.descendants = Descendants::Unknown),
            SkipReason::LiveDescendant,
        ),
        (
            "nudge",
            Box::new(|o| o.pending_nudges = 2),
            SkipReason::PendingNudge,
        ),
        (
            "report missing",
            Box::new(|o| o.report_file = ReportFile::Missing),
            SkipReason::ReportMissing,
        ),
        (
            "no goal",
            Box::new(|o| o.goal = String::new()),
            SkipReason::NotGoalPane,
        ),
        (
            "no parent",
            Box::new(|o| o.parent = None),
            SkipReason::NotGoalPane,
        ),
        (
            "no session",
            Box::new(|o| o.session = None),
            SkipReason::NoResume,
        ),
        (
            "bad session id",
            Box::new(|o| o.session.as_mut().unwrap().session_id = "$(boom)".into()),
            SkipReason::NoResume,
        ),
        (
            "nothing known",
            Box::new(|o| {
                o.last_output_at = None;
                o.peer.as_mut().unwrap().status_updated_at = None;
            }),
            SkipReason::UnknownIdle,
        ),
    ];
    for (name, f, want) in cases {
        let mut o = idle_obs();
        f(&mut o);
        assert_eq!(v(&o), Verdict::Keep(want), "{name}");
    }
    // A ready report file does not block.
    let mut o = idle_obs();
    o.report_file = ReportFile::Ready;
    assert_eq!(v(&o), Verdict::Close(CloseReason::Idle));
}

#[test]
fn skip_rules_also_block_trigger_a() {
    let mut o = idle_obs();
    o.meta_done = true;
    o.claimed_tasks = 2;
    assert_eq!(v(&o), Verdict::Keep(SkipReason::ClaimedTask));
}

#[test]
fn disabled_config_keeps_everything() {
    let c = SweepCfg {
        enabled: false,
        ..cfg()
    };
    assert_eq!(
        decide(&idle_obs(), &c, NOW),
        Verdict::Keep(SkipReason::Disabled)
    );
}

#[test]
fn unanswered_needs_decision_blocks_only_trigger_b() {
    let mut o = idle_obs();
    for kind in [ReportKind::NeedsDecision, ReportKind::Blocked] {
        o.last_report = Some(Report {
            kind,
            ts: NOW - 5 * HOUR,
        });
        assert_eq!(v(&o), Verdict::Keep(SkipReason::OpenQuestion), "{kind:?}");
        // A message from anyone else (a worker report, a broadcast) is no answer.
        o.last_inbound_ts = Some(NOW - 4 * HOUR - 1);
        assert_eq!(v(&o), Verdict::Keep(SkipReason::OpenQuestion), "{kind:?}");
        // The parent answered since: closable.
        o.last_parent_msg_ts = Some(NOW - 4 * HOUR - 1);
        assert_eq!(v(&o), Verdict::Close(CloseReason::Idle));
        // A parent message from BEFORE the question is no answer either.
        o.last_parent_msg_ts = Some(NOW - 6 * HOUR);
        assert_eq!(v(&o), Verdict::Keep(SkipReason::OpenQuestion), "{kind:?}");
        o.last_inbound_ts = None;
        o.last_parent_msg_ts = None;
    }
    // Trigger A (done via meta) ignores the open question.
    o.meta_done = true;
    o.last_output_at = Some(NOW - HOUR);
    o.peer.as_mut().unwrap().status_updated_at = Some(NOW - HOUR);
    assert_eq!(v(&o), Verdict::Close(CloseReason::GoalDone));
}

#[test]
fn parent_rework_after_done_cancels_the_done_trigger() {
    let mut o = idle_obs();
    o.last_report = Some(Report {
        kind: ReportKind::Done,
        ts: NOW - 2 * HOUR,
    });
    o.last_output_at = Some(NOW - HOUR);
    o.peer.as_mut().unwrap().status_updated_at = Some(NOW - HOUR);
    assert_eq!(v(&o), Verdict::Close(CloseReason::GoalDone));
    // Someone else writing does not reopen it.
    o.last_inbound_ts = Some(NOW - 90 * MIN);
    assert_eq!(v(&o), Verdict::Close(CloseReason::GoalDone));
    // The parent writing after the done (rework) does: back to the 3 h idle rule.
    o.last_parent_msg_ts = Some(NOW - 90 * MIN);
    assert_eq!(v(&o), Verdict::Keep(SkipReason::NotIdleLongEnough));
    // meta hp.goalState=done still counts.
    o.meta_done = true;
    assert_eq!(v(&o), Verdict::Close(CloseReason::GoalDone));
}

#[test]
fn ledger_tracks_messages_per_sender_and_apply_keeps_non_spec_panes() {
    let mut l = SweepLedger::new();
    l.record_post("S", Some("W"), false, 1, 100, "worker note");
    l.record_post("S", Some("O"), false, 2, 200, "answer: B");
    l.record_post("S", Some("W"), false, 3, 300, "worker note 2");
    assert_eq!(l.last_inbound_from("S", "O"), Some(200));
    assert_eq!(l.last_inbound_from("S", "W"), Some(300));
    assert_eq!(l.last_inbound_ts("S"), Some(300));
    // The orchestrator's open consult survives a pass that only observed spec panes.
    l.record_post("O", Some("S"), true, 4, 400, "needs-decision: A or B?");
    let _ = apply_pass(&mut l, vec![idle_obs()], &cfg(), NOW);
    assert_eq!(l.open_consults("O").len(), 1);
}

// ---- screen ----

#[test]
fn screen_asking_detection() {
    assert!(screen_asking(
        "Pick one\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel",
        None
    ));
    assert!(screen_asking("Do you want to proceed?", None));
    assert!(screen_asking("x\n↑/↓ to navigate", None));
    // Plain-text fallback: text after the prompt is a draft.
    assert!(screen_asking("done\n❯ half typed", None));
    assert!(!screen_asking("done\n❯ ", None));
    assert!(!screen_asking("done\n│ ❯                │", None));
    assert!(screen_asking("done\n│ ❯ draft          │", None));
    // Structured read distinguishes typed from ghost.
    assert!(screen_asking(
        "❯ hello",
        Some((InputLineKind::Typed, "hello"))
    ));
    assert!(!screen_asking(
        "❯ run tests",
        Some((InputLineKind::Suggestion, "run tests"))
    ));
    assert!(!screen_asking("❯", Some((InputLineKind::Empty, ""))));
    // Markers beat the structured read.
    assert!(screen_asking(
        "Esc to cancel",
        Some((InputLineKind::Empty, ""))
    ));
}

// ---- hysteresis ----

#[test]
fn hysteresis_needs_a_standing_close_and_resets_on_keep() {
    let mut l = SweepLedger::new();
    assert!(!l.note_verdict("p", true, 0)); // first close verdict
    assert!(!l.note_verdict("p", true, 59_000)); // too soon
    assert!(l.note_verdict("p", true, 60_000)); // >= 60s after the first
                                                // A keep resets the run.
    assert!(!l.note_verdict("p", false, 70_000));
    assert!(!l.note_verdict("p", true, 80_000));
    assert!(!l.note_verdict("p", true, 130_000));
    assert!(l.note_verdict("p", true, 140_000));
}

// ---- ledger ----

#[test]
fn ledger_records_reports_inbound_and_open_consults() {
    let mut l = SweepLedger::new();
    // spec S asks orch O.
    l.record_post("O", Some("S"), true, 5, 1000, "needs-decision: A or B?");
    assert_eq!(l.last_report("S").unwrap().kind, ReportKind::NeedsDecision);
    assert_eq!(l.last_inbound_ts("O"), Some(1000));
    assert_eq!(l.last_sent_ts("S"), Some(1000));
    let open = l.open_consults("O");
    assert_eq!(
        open,
        vec![OpenConsult {
            from: "S".into(),
            seq: 5,
            ts: 1000
        }]
    );
    // Orchestrator answers S: closed.
    l.record_post("S", Some("O"), false, 6, 2000, "go with A");
    assert!(l.open_consults("O").is_empty());
    assert_eq!(l.last_inbound_ts("S"), Some(2000));
    // A later consult is open again; progress reports are not consults.
    l.record_post("O", Some("S"), true, 7, 3000, "progress: working");
    assert!(l.open_consults("O").is_empty());
    l.record_post("O", Some("S"), true, 8, 4000, "Needs-Decision: C?");
    assert_eq!(l.open_consults("O").len(), 1);
    // Non-spec senders are not consults.
    l.record_post("O", Some("X"), false, 9, 5000, "needs-decision: x");
    assert_eq!(l.open_consults("O").len(), 1);
    assert!(l.open_consults("nobody").is_empty());
}

#[test]
fn report_kind_parsing() {
    assert_eq!(parse_report_kind("done"), Some(ReportKind::Done));
    assert_eq!(parse_report_kind("Done: shipped"), Some(ReportKind::Done));
    assert_eq!(
        parse_report_kind("blocked - need token"),
        Some(ReportKind::Blocked)
    );
    assert_eq!(parse_report_kind("failed."), Some(ReportKind::Failed));
    assert_eq!(parse_report_kind("hello there"), None);
    assert_eq!(parse_report_kind(""), None);
}

// ---- /proc parsing ----

#[test]
fn stat_parser_handles_tricky_comm_and_start_time() {
    // comm has a space and a closing paren; starttime is field 22.
    let line = "4242 (my (weird) proc) S 100 4242 4242 0 -1 4194560 1 0 0 0 5 6 0 0 20 0 1 0 987654 1000 200 18446744073709551615";
    let s = parse_stat(line).unwrap();
    assert_eq!(s.state, 'S');
    assert_eq!(s.ppid, 100);
    assert_eq!(s.start_ticks, 987654);
    assert!(parse_stat("garbage").is_none());
    assert!(parse_stat("1 (x) S 1").is_none());
    assert_eq!(start_ms(250, 1_000, 100), 1_000_000 + 2_500);
}

#[test]
fn descendant_scan_ignores_launch_helpers_and_zombies() {
    let st = |state, ppid, start_ticks| StatInfo {
        state,
        ppid,
        start_ticks,
    };
    // root 10 starts at tick 0; clk 100 => 1 tick = 10ms. grace 60s = 6000 ticks.
    let mut procs = vec![
        (10, st('S', 1, 0)),
        (11, st('S', 10, 100)),   // launch helper
        (12, st('S', 11, 200)),   // grandchild helper
        (99, st('S', 1, 50_000)), // unrelated, late
    ];
    assert_eq!(
        descendants_beyond_grace(&procs, 10, 0, 100, 60_000),
        Descendants::None
    );
    procs.push((13, st('S', 12, 7_000))); // grandchild started after grace
    assert_eq!(
        descendants_beyond_grace(&procs, 10, 0, 100, 60_000),
        Descendants::Some
    );
    procs.last_mut().unwrap().1.state = 'Z';
    assert_eq!(
        descendants_beyond_grace(&procs, 10, 0, 100, 60_000),
        Descendants::None
    );
    // Unknown root.
    assert_eq!(
        descendants_beyond_grace(&procs, 777, 0, 100, 60_000),
        Descendants::Unknown
    );
}

// ---- record + log ----

fn session() -> SessionInfo {
    SessionInfo {
        session_id: "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a".into(),
        cwd: "/w/proj".into(),
        config_dir: "/home/u/.claude-alt".into(),
    }
}

#[test]
fn resume_command_gates_and_config_dir() {
    let c = resume_command(&session(), None).unwrap();
    assert_eq!(
        c,
        "cd '/w/proj' && CLAUDE_CONFIG_DIR='/home/u/.claude-alt' claude --resume 0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a"
    );
    let mut s = session();
    s.config_dir.clear();
    assert_eq!(
        resume_command(&s, None).unwrap(),
        "cd '/w/proj' && claude --resume 0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a"
    );
    s.session_id = "x; rm -rf /".into();
    assert!(resume_command(&s, None).is_none());
    let mut s = session();
    s.cwd = "/bad'quote".into();
    assert!(resume_command(&s, None).is_none());
    // Marker cwd empty: falls back to the pane's.
    s.cwd.clear();
    assert!(resume_command(&s, Some("/w/other"))
        .unwrap()
        .contains("cd '/w/other'"));
    assert!(resume_command(&s, None).is_none());
}

fn scratch() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "hp-spec-sweep-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn record() -> CloseRecord {
    CloseRecord {
        goal: "g1".into(),
        reason: "idle".into(),
        pane_id: "abcdef012345".into(),
        parent: Some("orch".into()),
        session: session(),
        cwd: "/w/proj".into(),
        idle_min: 241,
        rss_kb: Some(2048),
        resume: resume_command(&session(), None),
        reports: vec!["[1] progress: a".into(), "[2] done: b".into()],
        screen_tail: "last line".into(),
    }
}

#[test]
fn record_and_log_contents_land_in_the_given_state_dir() {
    let d = scratch();
    let r = record();
    let path = write_record(&d, &r, 1234).unwrap();
    assert_eq!(path, d.join("closed-spec-panes").join("g1-abcdef01.md"));
    let md = std::fs::read_to_string(&path).unwrap();
    for needle in [
        "goal g1",
        "reason: idle",
        "pane: abcdef012345",
        "parent: orch",
        "session: 0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a",
        "configDir: /home/u/.claude-alt",
        "cwd: /w/proj",
        "cd '/w/proj' && CLAUDE_CONFIG_DIR='/home/u/.claude-alt' claude --resume",
        "done: b",
        "last line",
    ] {
        assert!(md.contains(needle), "missing {needle:?} in:\n{md}");
    }
    append_log(&d, &r, Some(&path), false, 1234).unwrap();
    append_log(&d, &r, None, true, 1300).unwrap();
    let log = std::fs::read_to_string(d.join("spec-sweep.log")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.join("spec-sweep.log"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the log holds session ids");
    }
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    let e = &lines[0];
    assert_eq!(e["ts"], 1234);
    assert_eq!(e["paneId"], "abcdef012345");
    assert_eq!(e["goal"], "g1");
    assert_eq!(e["parent"], "orch");
    assert_eq!(e["reason"], "idle");
    assert_eq!(e["sessionId"], "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a");
    assert_eq!(e["configDir"], "/home/u/.claude-alt");
    assert_eq!(e["cwd"], "/w/proj");
    assert_eq!(e["idleMin"], 241);
    assert_eq!(e["rssKb"], 2048);
    assert!(e["resume"].as_str().unwrap().contains("claude --resume"));
    assert_eq!(e["record"], path.display().to_string());
    assert!(e.get("wouldClose").is_none());
    assert_eq!(lines[1]["wouldClose"], true);
    let msg = parent_message(&r, &path);
    assert!(msg.starts_with("spec pane closed: goal g1 (idle, idle 241m). Resume: cd '/w/proj'"));
    assert!(msg.contains("Record: "));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn record_path_sanitizes_names() {
    let p = record_path(Path::new("/s"), "../evil goal", "pane/../x");
    assert_eq!(
        p,
        PathBuf::from("/s/closed-spec-panes/.._evil_goal-pane_.._.md")
    );
}

// ---- pass with a fake env ----

struct Fake {
    now: RefCell<i64>,
    obs: SpecPaneObs,
    meta: BTreeMap<String, String>,
}

impl Fake {
    fn new() -> Self {
        let mut meta = BTreeMap::new();
        meta.insert("role".into(), "spec".into());
        meta.insert("goal".into(), "g1".into());
        meta.insert("parent".into(), "orch".into());
        meta.insert("project".into(), "/w/proj".into());
        Self {
            now: RefCell::new(NOW),
            obs: idle_obs(),
            meta,
        }
    }
}

impl SweepEnv for Fake {
    fn now_ms(&self) -> i64 {
        *self.now.borrow()
    }
    fn spec_panes(&self) -> Vec<SpecPane> {
        vec![SpecPane {
            pane_id: self.obs.pane_id.clone(),
            uid: "u".into(),
            meta: self.meta.clone(),
            cwd: self.obs.cwd.clone(),
        }]
    }
    fn is_busy(&self, _: &SpecPane) -> bool {
        self.obs.busy
    }
    fn last_output_at(&self, _: &SpecPane) -> Option<i64> {
        self.obs.last_output_at
    }
    fn session(&self, _: &SpecPane) -> Option<SessionInfo> {
        self.obs.session.clone()
    }
    fn peer(&self, _: &str) -> Option<PeerObs> {
        self.obs.peer
    }
    fn claimed_tasks(&self, queues: &[String]) -> usize {
        assert_eq!(queues, ["g1".to_string(), "proj-g1".to_string()]);
        self.obs.claimed_tasks
    }
    fn worker_children(&self, _: &str) -> usize {
        self.obs.worker_children
    }
    fn descendants(&self, _: u32, _: i64) -> Descendants {
        self.obs.descendants
    }
    fn screen(&self, _: &SpecPane) -> Option<ScreenObs> {
        Some(ScreenObs {
            text: "all quiet\n❯ ".into(),
            input: Some((InputLineKind::Empty, String::new())),
        })
    }
    fn rss_kb(&self, _: u32) -> Option<u64> {
        self.obs.rss_kb
    }
    fn pending_nudges(&self, _: &str) -> usize {
        self.obs.pending_nudges
    }
    fn report_file(&self, _: &str) -> ReportFile {
        ReportFile::Missing
    }
    fn inbox_last_to(&self, _: &str) -> Option<i64> {
        None
    }
    fn inbox_last_from(&self, _: &str, _: &str) -> Option<i64> {
        None
    }
    fn reports_from(&self, _: &str, _: &str) -> Vec<(String, i64)> {
        vec![
            ("progress: x".into(), NOW - 5 * HOUR),
            ("done: y".into(), NOW - 5 * HOUR + 1),
        ]
    }
}

#[test]
fn pass_applies_hysteresis_then_emits_one_close_action() {
    let env = Fake::new();
    let mut l = SweepLedger::new();
    assert!(sweep_pass(&env, &mut l, &cfg()).is_empty());
    *env.now.borrow_mut() += MIN;
    let acts = sweep_pass(&env, &mut l, &cfg());
    assert_eq!(acts.len(), 1);
    // done report found in the parent's inbox, idle well past 30 min => goal-done.
    assert_eq!(acts[0].reason, CloseReason::GoalDone);
    assert_eq!(acts[0].idle_ms, 4 * HOUR + MIN);
    let rec = build_record(&env, &env.spec_panes()[0], &acts[0]);
    assert_eq!(rec.reports.len(), 2);
    assert!(rec.screen_tail.contains("all quiet"));
    assert!(rec.resume.is_some());
}

#[test]
fn pass_dry_run_reports_once_per_run_and_a_keep_resets() {
    let mut env = Fake::new();
    let c = SweepCfg {
        dry_run: true,
        ..cfg()
    };
    let mut l = SweepLedger::new();
    assert!(sweep_pass(&env, &mut l, &c).is_empty());
    *env.now.borrow_mut() += MIN;
    let a = sweep_pass(&env, &mut l, &c);
    assert_eq!(a.len(), 1);
    assert!(a[0].dry_run);
    *env.now.borrow_mut() += MIN;
    assert!(sweep_pass(&env, &mut l, &c).is_empty(), "already logged");
    env.obs.busy = true;
    *env.now.borrow_mut() += MIN;
    assert!(sweep_pass(&env, &mut l, &c).is_empty());
    env.obs.busy = false;
    *env.now.borrow_mut() += MIN;
    assert!(sweep_pass(&env, &mut l, &c).is_empty(), "run restarted");
    *env.now.borrow_mut() += MIN;
    assert_eq!(sweep_pass(&env, &mut l, &c).len(), 1);
}

#[test]
fn queue_name_candidates() {
    let mut m = BTreeMap::new();
    m.insert("project".to_string(), "/home/u/dev/my-app/".to_string());
    assert_eq!(queue_names(&m, "g7"), ["g7", "my-app-g7"]);
    m.insert(META_QUEUE.to_string(), "custom-q".to_string());
    assert_eq!(queue_names(&m, "g7"), ["custom-q"]);
    assert!(queue_names(&BTreeMap::new(), "").is_empty());
}

#[test]
fn env_switch_disables() {
    let s = SpecSweepSettings::default();
    for off in ["0", "false", "off"] {
        assert!(!SweepCfg::from_settings(&s, Some(off)).enabled);
    }
    assert!(SweepCfg::from_settings(&s, Some("1")).enabled);
    assert!(SweepCfg::from_settings(&s, None).enabled);
    let c = SweepCfg::from_settings(
        &SpecSweepSettings {
            idle_close_hours: 5,
            done_close_minutes: 10,
            ..s
        },
        None,
    );
    assert_eq!((c.idle_close_ms, c.done_close_ms), (5 * HOUR, 10 * MIN));
}
