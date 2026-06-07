//! Port of `src/main/session-manager.ts` — the CENTRAL owner of live sessions
//! (`Map<uid, Session>`): create / get / write / resize / kill / killAll. In one Rust
//! process all PTYs live here and windows/panes just reference `uid`s (no Electron
//! broadcast-to-all-windows model — this is what simplifies multi-window re-attach).
//!
//! A `Session` ties pty → cwd-sniff (`session::cwd`, on the RAW chunk pre-batch) →
//! `batcher` → `replay`, emitting Data / Cwd / Exit, and tracks `last_output_at` +
//! a monotonic `output_bytes` cursor (UTF-16 units) for the control read-path.
//!
//! This is the API Wave-2's control server consumes — keep it clean and documented.
//! STUB — owned by track `session-engine`.
