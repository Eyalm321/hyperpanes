//! Port of `src/main/control-input.ts` — input normalization for the control API:
//! `submitNewlines` (Windows CR vs LF), `keyToBytes` / `keysToBytes`, and the
//! `NAMED_KEYS` table (enter, ctrl+c, arrows, … — 40+ keys). Mirror every case in
//! `control-input.test.ts`. Byte-exact: these feed a real PTY.
//!
//! STUB — owned by track `core-io`. Replace with the 1:1 port + `#[test]`s.
