//! Port of `src/main/paths.ts` — clickable terminal paths: take a candidate path token + a
//! pane's cwd, resolve to an absolute path, verify it on disk (exists / is-dir / is-exe), and
//! open it (in an editor with optional line:col, or via the OS default handler). The grid-side
//! extraction (which tokens look like paths) is ported from `src/renderer/components/pathLinks.ts`
//! and lives in the terminal-widget (it has the cell grid); it calls into THIS for resolve+open.
//! Keep resolution pure/testable; opening shells out (editor command or OS open).
//!
//! STUB — owned by track `clickable-paths`.
