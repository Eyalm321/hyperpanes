//! Regression tests for the g4 pane-vanish bug: the GUI host's per-tick wholesale republish
//! (`ReadModel::publish_replace`, extracted from `ControlHost::publish`) racing a `/command
//! newPane` that inserted a pane into the model after the host's snapshot. On the buggy code
//! the republish destroys the just-inserted pane — its PTY session stays alive, but the pane
//! is gone from `/state` / `list_panes` forever (unwatchdoggable, unrestartable). These tests
//! drive the exact publish cycle through the public API.

use std::collections::{BTreeMap, HashSet};

use hyperpanes_core::control::readmodel::{PaneInfo, PaneStatus, ReadModel, TabInfo, WindowInfo};

fn pane(id: &str, uid: &str) -> PaneInfo {
    PaneInfo {
        id: id.to_string(),
        session_uid: uid.to_string(),
        label: "shell".to_string(),
        subtitle: None,
        color: "#888888".to_string(),
        command: None,
        args: None,
        cwd: None,
        shell: None,
        status: PaneStatus::Running,
        exit_code: None,
        meta: None,
    }
}

/// The GUI tree as `ControlHost::publish` rebuilds it: one window (id 1) with one positional
/// tab (`"1:0"`) holding `panes`.
fn gui_window(panes: Vec<PaneInfo>) -> WindowInfo {
    WindowInfo {
        window_id: 1,
        active_tab_id: Some("1:0".to_string()),
        tabs: vec![TabInfo {
            id: "1:0".to_string(),
            title: "Tab 1".to_string(),
            layout: "auto".to_string(),
            panes,
        }],
    }
}

/// A model as it stands right after a publish: window 1 hosting the GUI pane `u-gui`.
fn published_model() -> (ReadModel, HashSet<String>) {
    let mut m = ReadModel::new();
    m.publish_replace(
        &[],
        vec![gui_window(vec![pane("u-gui", "u-gui")])],
        &HashSet::new(),
    );
    let last_published: HashSet<String> = ["u-gui".to_string()].into();
    (m, last_published)
}

/// THE g4 bug: a control-spawned pane inserted between the host's snapshot and its republish
/// must survive the republish. Its session is alive and the GUI simply hasn't adopted it yet;
/// destroying it leaves a live PTY permanently invisible to the whole orchestration plane.
#[test]
fn pane_inserted_between_snapshot_and_publish_survives_the_republish() {
    let (mut m, last_published) = published_model();

    // `/command newPane` (dispatch, off-thread): spawn + insert into window 1's active tab.
    let mut meta = BTreeMap::new();
    meta.insert("role".to_string(), "worker".to_string());
    let mut worker = pane("ctl-worker", "u-worker");
    worker.meta = Some(meta.clone());
    assert!(m.insert_pane(1, worker));

    // The host's republish, rebuilt from a GUI snapshot that predates the insert.
    m.publish_replace(
        &[1],
        vec![gui_window(vec![pane("u-gui", "u-gui")])],
        &last_published,
    );

    let p = m
        .pane("ctl-worker")
        .expect("control pane inserted during the publish cycle must survive the republish");
    assert_eq!(p.session_uid, "u-worker");
    assert_eq!(
        p.meta.as_ref().and_then(|m| m.get("role")).unwrap(),
        "worker"
    );
    // Re-homed somewhere addressable (window 1 still exists → stays in window 1).
    assert_eq!(m.coords_of("ctl-worker").unwrap().window_id, 1);
    // The GUI pane published normally is untouched.
    assert!(m.pane("u-gui").is_some());
}

/// The carry-over must NOT resurrect panes the GUI deliberately dropped: a uid in
/// `last_published` (the GUI hosted it last tick) that is absent from the new tree was closed
/// in the GUI (possibly parked in the closed-tab undo buffer, session still alive) — it must
/// leave the model.
#[test]
fn gui_closed_pane_is_not_resurrected() {
    let mut m = ReadModel::new();
    m.publish_replace(
        &[],
        vec![gui_window(vec![
            pane("u-gui", "u-gui"),
            pane("u-closed", "u-closed"),
        ])],
        &HashSet::new(),
    );
    let last_published: HashSet<String> = ["u-gui".to_string(), "u-closed".to_string()].into();

    // GUI closed `u-closed`; the republished tree no longer contains it.
    m.publish_replace(
        &[1],
        vec![gui_window(vec![pane("u-gui", "u-gui")])],
        &last_published,
    );

    assert!(
        m.pane("u-closed").is_none(),
        "GUI-closed pane must not be resurrected"
    );
    assert!(m.pane("u-gui").is_some());
}

/// A carried-over pane whose original (control-created) tab id no longer exists lands in its
/// window's active tab; a pane adopted into the GUI within the same cycle is not duplicated.
#[test]
fn carryover_rehomes_into_active_tab_and_never_duplicates() {
    let (mut m, last_published) = published_model();

    // A control pane living in a control-minted tab (an `attach as:tab` group).
    assert!(m.insert_tab(
        1,
        TabInfo {
            id: "ctl-tab".to_string(),
            title: "grp".to_string(),
            layout: "auto".to_string(),
            panes: vec![pane("ctl-a", "u-a")],
        },
    ));

    // Republish: the GUI tree has neither `ctl-tab` nor the pane → re-homed to "1:0".
    m.publish_replace(
        &[1],
        vec![gui_window(vec![pane("u-gui", "u-gui")])],
        &last_published,
    );
    assert_eq!(m.coords_of("ctl-a").expect("carried over").tab_id, "1:0");

    // Next cycle the GUI HAS adopted it (it appears in the tree under its own id): no dupe.
    let last2: HashSet<String> = ["u-gui".to_string(), "u-a".to_string()].into();
    m.publish_replace(
        &[1],
        vec![gui_window(vec![
            pane("u-gui", "u-gui"),
            pane("ctl-a", "u-a"),
        ])],
        &last2,
    );
    assert_eq!(
        m.panes().iter().filter(|p| p.session_uid == "u-a").count(),
        1
    );
    assert_eq!(m.coords_of("ctl-a").unwrap().tab_id, "1:0");
}

/// An exited pane is NOT carried over — carry-over exists for live sessions only.
#[test]
fn exited_pane_is_not_carried_over() {
    let (mut m, last_published) = published_model();
    let mut dead = pane("ctl-dead", "u-dead");
    dead.status = PaneStatus::Exited;
    dead.exit_code = Some(1);
    assert!(m.insert_pane(1, dead));

    m.publish_replace(
        &[1],
        vec![gui_window(vec![pane("u-gui", "u-gui")])],
        &last_published,
    );
    assert!(m.pane("ctl-dead").is_none());
}
