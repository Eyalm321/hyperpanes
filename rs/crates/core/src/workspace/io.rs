//! Port of the workspace-file I/O in `src/main/workspace.ts`: `readWorkspace` /
//! `writeWorkspace` / `resolveCwds` (walk all three nesting levels — panes/groups/windows —
//! resolving relative cwds against the file's directory) / `windowsOf` / `hasPanes`.
//! Uses `crate::workspace::model`. Mirror the file-I/O cases in `workspace.test.ts`.
//!
//! STUB — owned by track `persistence-cli`.
