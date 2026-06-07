//! Port of `src/main/control-output.ts` — the read-path pure cores:
//! `waitDecision` / `nextPollDelay` / `sliceSince` / `detectAwaitingInput`
//! (powers `waitForIdle`, the `since` delta cursor, and awaiting-input detection).
//! Mirror every case in `control-output.test.ts` — including the rendered-screen
//! fixtures (trust dialog vs idle box vs working spinner vs menu tail).
//!
//! ⚠ PARITY TRAP: the `since` cursor counts JS string `.length` = UTF-16 code units,
//! and `sliceSince` slices by that count. Rust `String::len()` is UTF-8 bytes — they
//! DIFFER for non-ASCII. MCP clients persist these cursors across reads, so track the
//! replay/cursor in UTF-16 code units (or document the divergence). Add a non-ASCII test.
//!
//! STUB — owned by track `core-io`. Replace with the 1:1 port + `#[test]`s.
