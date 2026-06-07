//! Port of `src/main/control-inbox.ts` — the durable per-pane message bus:
//! bounded ring buffer, monotonic `seq`, `dropped` accounting, at-least-once
//! read-after cursor. Mirror every case in `control-inbox.test.ts`.
//!
//! STUB — owned by track `core-control-state`. Replace with the 1:1 port + `#[test]`s.
