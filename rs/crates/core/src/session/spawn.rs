//! Port of the spawn-resolution helpers in `src/main/session.ts`:
//! `resolveSpawn` / `buildArgs` / `resolveWindowsCommand` / `defaultShell`, including
//! the PATHEXT/PATH search for a direct, no-shell `args[]` spawn (P4a) and the
//! scoped-control-token env suppression (a scoped child must NOT see
//! `HYPERPANES_CONTROL_FILE`). Pure + unit-testable.
//!
//! STUB — owned by track `session-engine`.
