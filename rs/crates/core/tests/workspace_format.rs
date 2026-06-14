//! Workspace-file format round-trip tests (#15) — exercised through the PUBLIC
//! `workspace::io` + `workspace::model` API only, against the CURRENT bare format
//! (a plain `WorkspaceFile` JSON object; the versioned envelope is a separate,
//! in-flight concern and is deliberately not assumed here).
//!
//! What these lock in, modeled on what mature terminals gate on (Alacritty's
//! ref-tests / WT's parser units, applied to our persistence layer):
//!   * full-field write→read round-trips at every nesting level (panes / groups /
//!     windows, split state, bounds, args, meta);
//!   * serde default-tolerance: an older/partial blob (missing fields) still loads;
//!   * forward-tolerance: unknown JSON keys are ignored, not fatal;
//!   * the no-`null` / omitted-optionals contract a hand-edited file relies on.

use hyperpanes_core::workspace::io::{has_panes, read_workspace, windows_of, write_workspace};
use hyperpanes_core::workspace::model::{
    GroupSpec, PaneSpec, WindowBounds, WindowSpec, WorkspaceFile,
};
use std::collections::BTreeMap;

/// A unique temp path per test so parallel tests never collide.
fn temp_file(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("hp-ws-format-{}-{tag}.json", std::process::id()))
}

/// A `WorkspaceFile` with EVERY field populated at every nesting level.
fn kitchen_sink() -> WorkspaceFile {
    let pane = PaneSpec {
        label: Some("worker".into()),
        color: Some("#e5484d".into()),
        command: Some("claude".into()),
        args: Some(vec!["--model".into(), "opus".into()]),
        cwd: Some(if cfg!(windows) { "C:\\work" } else { "/work" }.into()),
        shell: Some("pwsh".into()),
        font_size: Some(18),
        meta: Some(BTreeMap::from([
            ("ai.subtitle".to_string(), "compiling".to_string()),
            ("role".to_string(), "worker".to_string()),
        ])),
        uid: Some("pane-3".into()),
    };
    let group = GroupSpec {
        title: Some("build".into()),
        layout: Some("main-stack".into()),
        panes: vec![pane.clone(), PaneSpec::default()],
        sizes: Some(vec![0.7, 0.3]),
        main_fraction: Some(0.6),
        focused: Some(1),
        zoomed: Some(0),
    };
    WorkspaceFile {
        name: Some("sink".into()),
        layout: Some("grid".into()),
        panes: Some(vec![pane]),
        groups: Some(vec![group.clone()]),
        active: Some(0),
        windows: Some(vec![WindowSpec {
            title: Some("main".into()),
            active: Some(0),
            bounds: Some(WindowBounds {
                x: Some(-8),
                y: Some(0),
                width: Some(1920),
                height: Some(1080),
                maximized: Some(false),
                fullscreen: Some(true),
            }),
            groups: vec![group],
        }]),
    }
}

#[test]
fn every_field_survives_an_on_disk_round_trip() {
    let path = temp_file("sink");
    let ws = kitchen_sink();
    assert!(write_workspace(&path, &ws));
    let back = read_workspace(&path).expect("kitchen-sink file reads back");
    // All cwds in the fixture are absolute, so resolve_cwds is identity and the read
    // value must equal the written one EXACTLY — any drift is a format regression.
    assert_eq!(back, ws);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn in_memory_serde_round_trip_is_lossless() {
    let ws = kitchen_sink();
    let json = serde_json::to_string_pretty(&ws).unwrap();
    let back: WorkspaceFile = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ws);
    // The bare format is a plain object with camelCase keys.
    assert!(json.starts_with('{'));
    assert!(json.contains("\"fontSize\": 18"));
    assert!(json.contains("\"mainFraction\": 0.6"));
    assert!(!json.contains("null"), "unset optionals must be omitted, never null");
}

#[test]
fn an_old_minimal_blob_still_loads_with_defaults() {
    // The oldest shape in the wild: just panes with commands. Everything else defaults.
    let path = temp_file("minimal");
    std::fs::write(&path, br#"{ "panes": [ { "command": "npm run dev" } ] }"#).unwrap();
    let ws = read_workspace(&path).expect("minimal blob loads");
    let panes = ws.panes.expect("panes present");
    assert_eq!(panes[0].command.as_deref(), Some("npm run dev"));
    assert_eq!(panes[0].label, None);
    assert_eq!(panes[0].font_size, None);
    assert_eq!(ws.name, None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn unknown_keys_are_tolerated_not_fatal() {
    // Forward-tolerance: a file written by a NEWER build (extra keys at any level) must
    // still load in this one — serde's default behavior is to ignore unknown fields,
    // and this test pins that (a future `deny_unknown_fields` would break rollback).
    let path = temp_file("unknown");
    std::fs::write(
        &path,
        br#"{
  "name": "future",
  "someFutureTopLevelKey": { "nested": true },
  "panes": [ { "command": "bash", "someFuturePaneKey": 42 } ]
}"#,
    )
    .unwrap();
    let ws = read_workspace(&path).expect("future-keyed blob still loads");
    assert_eq!(ws.name.as_deref(), Some("future"));
    assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("bash"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn group_split_state_survives_disk_round_trip() {
    // The split state a relaunch restore depends on: sizes / mainFraction / focused /
    // zoomed / per-pane fontSize all survive write→read.
    let path = temp_file("split");
    let ws = WorkspaceFile {
        groups: Some(vec![GroupSpec {
            title: Some("t".into()),
            layout: Some("columns".into()),
            panes: vec![
                PaneSpec { font_size: Some(22), ..Default::default() },
                PaneSpec::default(),
            ],
            sizes: Some(vec![0.25, 0.75]),
            main_fraction: Some(0.55),
            focused: Some(1),
            zoomed: Some(1),
        }]),
        active: Some(0),
        ..Default::default()
    };
    assert!(write_workspace(&path, &ws));
    let back = read_workspace(&path).expect("reads back");
    let g = &back.groups.as_ref().unwrap()[0];
    assert_eq!(g.sizes.as_deref(), Some(&[0.25, 0.75][..]));
    assert_eq!(g.main_fraction, Some(0.55));
    assert_eq!(g.focused, Some(1));
    assert_eq!(g.zoomed, Some(1));
    assert_eq!(g.panes[0].font_size, Some(22));
    assert_eq!(g.panes[1].font_size, None);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn windows_of_normalises_each_round_tripped_shape_identically() {
    // The launcher consumes files through windows_of; a round-trip must not change what
    // it resolves for any of the three top-level shapes.
    let shapes = [
        WorkspaceFile {
            name: Some("p".into()),
            panes: Some(vec![PaneSpec { label: Some("a".into()), ..Default::default() }]),
            ..Default::default()
        },
        WorkspaceFile {
            groups: Some(vec![GroupSpec {
                panes: vec![PaneSpec { label: Some("b".into()), ..Default::default() }],
                ..Default::default()
            }]),
            ..Default::default()
        },
        kitchen_sink(),
    ];
    for (i, ws) in shapes.iter().enumerate() {
        let path = temp_file(&format!("shape{i}"));
        assert!(write_workspace(&path, ws));
        let back = read_workspace(&path).expect("round-trips");
        assert!(has_panes(&back));
        assert_eq!(
            windows_of(Some(&back)),
            windows_of(Some(ws)),
            "shape {i}: windows_of must resolve identically after a round-trip"
        );
        let _ = std::fs::remove_file(&path);
    }
}
