//! The headless app wiring — the one place every subsystem meets. Build the central
//! `SessionManager`; the `AiService` (default-off, settings/memory paths from
//! `persistence::paths`); and the `ControlServer` (gated by `persistence::control_settings`).
//! Run the `single_instance` gate (mutex + named-pipe) and route a second-instance argv via
//! `cli::routing`; resolve the launch workspace via `workspace::launch`. Expose `run()` for
//! the headless daemon bin (and, later, for the Slint app to embed).
//!
//! STUB — owned by track `control-server`.
