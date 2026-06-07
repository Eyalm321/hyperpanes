//! Rendered-screen serializer: drive an `alacritty_terminal` `Term` from the pty byte
//! stream and serialize its grid to clean text for control `mode:"screen"` reads
//! (this replaces the renderer's xterm.js serialize — a capability GAIN: screen reads
//! need no GUI). Treat exact text as best-effort parity with xterm (wrapping / wide
//! chars / trailing-space trimming may differ); `detect_awaiting_input` runs on this.
//!
//! STUB — owned by track `session-engine`.
