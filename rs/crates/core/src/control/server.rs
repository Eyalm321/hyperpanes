//! The control server: bind `127.0.0.1` on an EPHEMERAL port, build the axum router (routes +
//! the `/events` WS upgrade with token auth via header or `?token=`), and write the `control.json`
//! discovery file under `persistence::paths` — exact fields + 2-space pretty JSON:
//! `{ port, token, pid, version, events: "ws://127.0.0.1:<port>/events?token=<token>" }` —
//! removing it on shutdown. Owns/holds the `SessionManager` + `readmodel` + `tokens` + inbox +
//! locks. Toggled by control-settings (enabled / allowInput), default OFF.
//!
//! STUB — owned by track `control-server`.
