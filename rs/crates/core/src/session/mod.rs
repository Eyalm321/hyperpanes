//! Session subsystem. The pure `cwd` parser (done, Wave 0) plus the LIVE engine
//! (this wave): `pty` / `spawn` / `batcher` / `replay` / `screen`. Frozen map.
pub mod cwd;
pub mod pty;
pub mod spawn;
pub mod batcher;
pub mod replay;
pub mod screen;
