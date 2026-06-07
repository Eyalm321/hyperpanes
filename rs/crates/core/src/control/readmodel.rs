//! The CENTRAL read-model — AUTHORITATIVE here (not an Electron renderer mirror). Holds the
//! windows → tabs → panes tree + reverse indexes (uidToPane / paneIndex / tabToWindow) rebuilt
//! on STRUCTURE change only, a per-window structural fingerprint, and per-pane `activity`
//! (busy/idle/exited) computed centrally from `session_manager` `last_output_at` at the
//! `idleAlertSeconds` threshold. Serializes the EXACT `/state` JSON (PaneState:
//! id/sessionUid/label/color/command?/args?/cwd?/shell?/subtitle?/status/exitCode?/activity/meta?,
//! optionals OMITTED when unset). Scope-filtered per request.
//!
//! STUB — owned by track `control-server`.
