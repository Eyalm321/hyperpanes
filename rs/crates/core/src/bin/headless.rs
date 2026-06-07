//! `hyperpanes-headless` — a runnable, GUI-less daemon for parity testing. Starts the app
//! (central SessionManager + control server + `control.json`) so the REAL MCP server at
//! `C:\hyperpanes-mcp` can be pointed at the Rust backend for the acceptance gate.
//! (Auto-discovered bin — no Cargo.toml entry needed.)
//!
//! Usage (gate): point the MCP at an isolated discovery file so it never fights the Electron app:
//!   set HYPERPANES_CONTROL_FILE=C:\tmp\hp-headless\control.json
//!   set HYPERPANES_ALLOW_INPUT=1
//!   cargo run --bin headless
//! Then run the MCP with the same `HYPERPANES_CONTROL_FILE`.

fn main() {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("hyperpanes-headless: failed to start tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = runtime.block_on(hyperpanes_core::app::run()) {
        eprintln!("hyperpanes-headless: {e}");
        std::process::exit(1);
    }
}
