//! Single-instance gate + argv hand-off (replaces Electron `requestSingleInstanceLock`).
//!
//! - Detector: a named **mutex** (`CreateMutexW`; `GetLastError == ERROR_ALREADY_EXISTS`
//!   ⇒ a primary already runs). Pipe-connect must NOT be the detector — it races at startup.
//! - Hand-off: a named **pipe**. The primary runs a pipe server; a secondary connects and
//!   sends `{ argv, cwd }` as JSON; the primary routes it via `crate::cli::routing`.
//!
//! Use the `windows` crate for the mutex; tokio's named-pipe (or `windows`) for the pipe.
//! Pipe/mutex names derive from a stable per-user salt.
//!
//! STUB — owned by track `platform`.
