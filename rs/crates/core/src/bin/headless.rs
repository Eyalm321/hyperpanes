//! `hyperpanes-headless` — a runnable, GUI-less daemon for parity testing. Starts the app
//! (central SessionManager + control server + `control.json`) so the REAL MCP server at
//! `C:\hyperpanes-mcp` can be pointed at the Rust backend for the acceptance gate.
//! (Auto-discovered bin — no Cargo.toml entry needed.)
//!
//! Usage (gate): point the MCP at an isolated discovery file AND data dir so it never fights the
//! live app or attaches its work queue (a partly isolated boot refuses to start, see
//! `persistence::isolation` / docs/dev-instance-isolation.md):
//!   Windows:  set APPDATA=C:\tmp\hp-headless
//!             set HYPERPANES_CONTROL_FILE=C:\tmp\hp-headless\control.json
//!   Linux:    D=$(mktemp -d); HYPERPANES_CONTROL_FILE=$D/state/hyperpanes/control.json \
//!             XDG_STATE_HOME=$D/state XDG_DATA_HOME=$D/data XDG_CONFIG_HOME=$D/config
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
    if let Err(e) = runtime.block_on(hyperpanes_core::app::run(env!("CARGO_PKG_VERSION"))) {
        eprintln!("hyperpanes-headless: {e}");
        std::process::exit(1);
    }
}
