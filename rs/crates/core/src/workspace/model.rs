//! Serde types for the workspace.json format (from `src/main/workspace.ts`):
//! `WorkspaceFile` / `WindowSpec` / `GroupSpec` / `PaneSpec`. Must round-trip
//! byte-identically with existing workspace files. Use
//! `#[serde(skip_serializing_if = "Option::is_none")]` on every optional field so
//! unset fields are OMITTED (not emitted as null) — downstream consumers are strict.
//! Include: GroupSpec `sizes`/`mainFraction`/`focused`/`zoomed`; PaneSpec
//! `label`/`subtitle`/`color`/`command`/`args`/`cwd`/`shell`/`fontSize`/`meta`.
//! Match the JS field names exactly (use `#[serde(rename_all = "camelCase")]` or
//! per-field renames as needed — e.g. `mainFraction`).
//!
//! STUB — owned by track `core-cli`. Replace with the real types + round-trip tests.
