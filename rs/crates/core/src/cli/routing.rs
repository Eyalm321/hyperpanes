//! Port of the second-instance launch-routing in `src/main/workspace.ts`:
//! `resolveSecondInstanceWindows` (a 2nd `hyperpanes …` invocation's argv + cwd → the
//! window specs + routing to apply). REUSE the routing enums already defined in
//! `crate::cli::parse` (`RoutingTarget` / `AttachAs` / `LaunchRouting`) and `parse_cli` —
//! do NOT redefine them. The Electron-specific `routeLaunch` (BrowserWindow placement)
//! is NOT ported here (it's UI). Mirror the routing cases in `workspace.test.ts`.
//!
//! STUB — owned by track `persistence-cli`.
