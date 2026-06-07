//! Port of the cwd parser in `src/main/shell-integration.ts`:
//! `parseOscCwd` / `fileUriToPath` / `oscDataToCwd`. Handles OSC 7 (POSIX file URI)
//! AND OSC 9;9 (cmd / Windows Terminal), MSYS `/c/...` → `C:\...`, remote-authority
//! rejection, dedupe-on-change, and split-across-chunk carry with `OSC_MAX` abandon.
//! Operate on the RAW pty chunk (pre-batch). Do NOT delegate to alacritty's OSC
//! handling — the contract's quirks are tested here. Mirror `shell-integration.test.ts`.
//!
//! STUB — owned by track `core-text`. Replace with the 1:1 port + `#[test]`s.
