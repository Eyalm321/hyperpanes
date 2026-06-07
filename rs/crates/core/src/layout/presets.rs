//! Port of `src/renderer/layout/presets.ts` — the 5 layout presets (single / columns / rows /
//! grid / main-stack) + auto-resolution (1→single, 2-3→columns, 4+→grid). Computes tile rects
//! as fractions 0..1 from (preset, pane count, sizes, mainFraction):
//! `compute_tiles(...) -> Vec<Rect>` where `Rect { x, y, w, h }`. Mirror `presets.test.ts`.
//!
//! STUB — owned by track `layout-engine`.
