//! Session subsystem. Phase 1 / this fan-out lands ONLY the pure `cwd` parser; the
//! live PTY / term / batcher / replay modules come in a later phase. Frozen map.
pub mod cwd;
