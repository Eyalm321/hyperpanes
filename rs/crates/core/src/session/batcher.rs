//! Port of `DataBatcher` in `src/main/session.ts` — coalesce pty output and flush on
//! 16ms OR 200KB, whichever comes first. Unit-testable with an injected clock.
//!
//! STUB — owned by track `session-engine`.
