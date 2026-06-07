//! Port of the PTY layer in `src/main/session.ts` — a `Pty` trait + a `portable-pty`
//! implementation (conpty on Windows): spawn(file, args, cwd, env, cols, rows) → a
//! handle exposing onData / onExit, write, resize, kill. Keep it behind the trait so
//! the conpty backend is swappable (the `portable-pty-psmux` fork adds modern conpty
//! input-mode flags if the default path mis-delivers `submit`/`send_keys`).
//!
//! STUB — owned by track `session-engine`.
