//! On-disk persistence under the Windows userData dir. Frozen map.
//! NOTE: `paths::user_data_dir()` MUST resolve to the EXACT same folder Electron uses
//! (`%APPDATA%\<productName>`), or the MCP can't find `control.json` and last-session
//! restore breaks. See the core handoff.
pub mod paths;
pub mod control_settings;
pub mod projects;
