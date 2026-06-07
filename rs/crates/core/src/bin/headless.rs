//! `hyperpanes-headless` — a runnable, GUI-less daemon for parity testing. Starts the app
//! (central SessionManager + control server + `control.json`) so the REAL MCP server at
//! `C:\hyperpanes-mcp` can be pointed at the Rust backend for the acceptance gate.
//! (Auto-discovered bin — no Cargo.toml entry needed.)
//!
//! STUB — owned by track `control-server`.

fn main() {
    // TODO(control-server): call `hyperpanes_core::app::run(...)`. See FANOUT-HANDOFF.md.
    println!("hyperpanes-headless: not yet implemented");
}
