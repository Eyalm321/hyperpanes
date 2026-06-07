//! In-process command execution — replaces the Electron renderer round-trip + correlationId.
//! POST /command mutates the central `readmodel` directly and returns `{ok, result}`
//! SYNCHRONOUSLY (the set_meta echo race is now structurally impossible). Commands:
//! newPane (→ returns new paneId) / closePane / setLayout / renamePane / recolorPane / setMeta /
//! focusPane / openTab / … PRESERVE the response shapes + status mapping byte-for-byte: keep
//! the 504 string for any command you deliberately make async, 500 on action error, 404
//! window-not-found, 400 missing-type/target, 403 scope error. `readScreen` serializes the
//! central `alacritty_terminal` Term via `session::screen`.
//!
//! STUB — owned by track `control-server`.
