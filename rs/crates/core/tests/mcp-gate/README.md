# MCP acceptance gate — Rust control server

How to re-run the live acceptance gate that drives the **real** MCP server at `C:\hyperpanes-mcp`
against the Rust `headless` daemon. These `.mjs` harnesses are reference copies of the scripts that
were run; the originals live in `C:\hyperpanes-mcp` (where Node resolves `@modelcontextprotocol/sdk`).

## Steps

1. Build the daemon:
   ```
   cargo build --manifest-path rs/Cargo.toml --bin headless
   ```
2. Build the MCP server (once):
   ```
   cd C:\hyperpanes-mcp && npm install && npm run build
   ```
3. Start the daemon against an **isolated** discovery file (so it never fights the Electron app's
   `%APPDATA%\hyperpanes\control.json`; the daemon's single-instance lock is also headless-salted):
   ```
   set HYPERPANES_CONTROL_FILE=C:\hp-gate\control.json
   set HYPERPANES_ALLOW_INPUT=1
   cargo run --manifest-path rs/Cargo.toml --bin headless
   ```
4. In another shell, run the two harnesses (from the MCP dir so the SDK resolves):
   ```
   cd C:\hyperpanes-mcp
   set HYPERPANES_CONTROL_FILE=C:\hp-gate\control.json
   node gate.mjs        # 17 checks: real MCP server (stdio) → tools → daemon
   node ws_check.mjs     # 8 checks: live /events WS (hello, no-leak, activity flips)
   ```

## What each covers

- **gate.mjs** spawns the REAL hyperpanes MCP server via the MCP SDK stdio client and exercises:
  `control_status`, `open_pane`→paneId, `list_panes` (meta+activity), `read_pane`
  (`mode:"screen"`+`waitForIdle`, `strip=1`), `set_meta` (synchronous merged echo), `whoami`
  (explicit + from `HYPERPANES_PANE_ID` env), durable inbox (`send_message`/`read_messages` +
  global-seq cursor), advisory lock (owner/non-owner/release), `mint_token`, scoped `/state`
  subtree filter, escalation mint → 403, and a scoped token excluding a newly-spawned sibling.
- **ws_check.mjs** verifies the `/events` WebSocket directly: the `hello` greeting, live `output`
  frame delivery on spawn, scope-filtered fan-out (a sibling-scoped stream sees no pane-A frames),
  the busy→idle `activity` flip on master + in-scope streams, and no activity leak to siblings.

Result (2026-06-07): **17/17 + 8/8 green** against the Rust daemon.
