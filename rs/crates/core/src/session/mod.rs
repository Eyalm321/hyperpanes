//! Session subsystem. The pure `cwd` parser (done, Wave 0) plus the LIVE engine
//! (this wave): `pty` / `spawn` / `batcher` / `replay` / `screen`. Plus the session
//! **daemon** (M0): `proto` (the wire protocol) + `daemon` (a PTY-owning daemon over a
//! UDS / named pipe, with a loopback client) — both headless-testable, no Slint.
pub mod batcher;
pub mod cwd;
pub mod daemon;
pub mod daemon_client;
pub mod env;
pub mod osc133;
pub mod proto;
pub mod pty;
pub mod replay;
pub mod screen;
pub mod spawn;
