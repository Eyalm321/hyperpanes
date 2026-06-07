//! Port of `src/main/ai/pane-buffer.ts` — the per-pane rolling, ANSI-stripped tail used as
//! model context (uses `crate::ansi_strip`), with alt-screen / clear-screen / CR handling.
//! Bounded ring. Mirror `pane-buffer.test.ts`.
//!
//! STUB — owned by track `ai`.
