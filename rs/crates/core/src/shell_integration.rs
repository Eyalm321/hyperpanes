//! Port of `src/main/shell-integration.ts` — the INJECTION side of shell integration:
//! classify the shell, locate the shipped init scripts (hp-init.ps1 / hp-init.sh / cmd
//! PROMPT injection), and build the spawn arguments that turn on OSC-7 / OSC 9;9 cwd
//! reporting. Strictly additive — no-ops (plain shell) when a script is missing or the
//! shell is unknown. NOTE: the cwd PARSER already lives in `session::cwd` (done); this is
//! the injection side only. Mirror the non-cwd-parser cases of `shell-integration.test.ts`.
//!
//! STUB — owned by track `platform`.
