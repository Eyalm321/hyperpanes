//! `hyperpanes-core` — headless Rust core for the native rewrite (Phase 1).
//!
//! The module map below is **frozen by the fan-out scaffold**. Each leaf module is
//! a 1:1 port of a TypeScript source under `../../../src/main`, owned by exactly one
//! parallel track (see that module's header and your `FANOUT-HANDOFF.md`). Fill in
//! ONLY your owned files — do not touch `lib.rs`, the `mod.rs` files, or `Cargo.toml`
//! (they are shared and would collide across worktrees).

pub mod ansi_strip;
pub mod cli;
pub mod control;
pub mod session;
pub mod workspace;
