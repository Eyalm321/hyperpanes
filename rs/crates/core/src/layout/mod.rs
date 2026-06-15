//! Tiling layout math — port of `src/renderer/layout/{presets,sizes,navigate}.ts` (pure,
//! NO Slint dep, so it lives in core and is unit-testable). Frozen map; the layout-engine
//! track owns the leaf files.
pub mod navigate;
pub mod presets;
pub mod sizes;
