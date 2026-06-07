//! Port of `parseCli` in `src/main/workspace.ts` — the positional/stateful CLI
//! grammar (window → tab → pane state machine). Preserve exactly:
//!  - attach-to-most-recent-`-c` for `-l/--color/--cwd/--shell/--font`
//!  - pre-`-c` `--cwd/--shell` as launch-wide defaults; `--layout` as pendingLayout
//!  - `--name` header-scope rules; `--window`/`--tab` separators
//!  - `--new-window`/`--attach[=…]`/`--into-current`/`--as tab|panes` routing precedence
//!  - legacy single-window output shape when no separators
//!  - label default = `command.trim().split(/\s+/)[0] || "shell"`
//!  - positional `.json` only when it ends `.json` AND exists
//! Use a HAND-ROLLED state machine (not clap). Mirror all 16 `workspace.test.ts` cases.
//! Consumes the serde types from `crate::workspace::model`.
//!
//! STUB — owned by track `core-cli`. Replace with the 1:1 port + `#[test]`s.
