//! Compile the app's Slint UI, importing the reusable `TerminalPane` from the
//! `hyperpanes-terminal-widget` crate via a Slint *library path*. In `ui/app.slint`
//! that surfaces as `import { TerminalPane, KeyMsg } from "@widgets";`.

use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let widget = manifest.join("../terminal-widget/ui/widget.slint");

    let mut libs: HashMap<String, PathBuf> = HashMap::new();
    libs.insert("widgets".to_string(), widget.clone());

    let cfg = slint_build::CompilerConfiguration::new().with_library_paths(libs);
    slint_build::compile_with_config("ui/app.slint", cfg).expect("slint compile failed");

    for f in [
        "ui/app.slint",
        "ui/topbar.slint",
        "ui/paneview.slint",
        "ui/theme.slint",
        "ui/types.slint",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }
    println!("cargo:rerun-if-changed={}", widget.display());
}
