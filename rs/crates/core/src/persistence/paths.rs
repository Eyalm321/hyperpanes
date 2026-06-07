//! userData-dir + file-path resolution (replaces Electron `app.getPath('userData')`).
//!
//! ⚠ MUST resolve to the EXACT same Windows folder Electron uses:
//! `%APPDATA%\<productName>` where `<productName>` comes from `package.json`. Read it and
//! pin the literal folder name — otherwise the MCP can't find `control.json` and
//! last-session restore breaks. Provides the canonical file paths: control.json,
//! control-settings.json, last-workspace.json, projects.json, ai-settings.json,
//! ai-memory.json. Add a test that reads a file the Electron app actually wrote.
//!
//! STUB — owned by track `persistence-cli`. (Wave-2 control server consumes this; keep
//! the public fn signatures clear and stable.)
