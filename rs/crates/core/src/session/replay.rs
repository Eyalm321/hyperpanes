//! Port of the 128KB rolling replay buffer in `src/main/session.ts` (lets a
//! re-attaching view replay recent output instead of showing a blank pane).
//!
//! ⚠ Track length/slicing in **UTF-16 code units**, not UTF-8 bytes, to match the
//! control-output `since`/`sliceSince` cursor (MCP persists those cursors). See
//! `control::output`. Unit-testable.
//!
//! STUB — owned by track `session-engine`.
