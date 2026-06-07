//! Port of `src/main/ai/ai-store.ts` — AI settings + memory persistence
//! (`ai-settings.json` / `ai-memory.json`) with ATOMIC temp-then-rename writes. Takes the
//! directory as a PARAMETER (DI), not `persistence::paths`, to stay decoupled. Mirror
//! `ai-store.test.ts`.
//!
//! STUB — owned by track `ai`.
