//! `ControlEvent` enum + scope-filtered fan-out for the GET /events WebSocket:
//! hello / output / exit / activity / message / state.
//!
//! Ordering rules that MUST hold (MCP depends on them): `note_output` (byte cursor +
//! last_output_at) updates UNCONDITIONALLY before any subscriber guard, so `since`/`waitForIdle`
//! work with zero clients; output/exit/message/activity are pane-addressed (broadcast_for_pane
//! scope-filter); pure `state` is a coalesced (~100ms) broadcast; a busy⇄idle flip emits
//! `activity` but NOT a `state` ping (the structural-fingerprint diff).
//!
//! STUB — owned by track `control-server`.
