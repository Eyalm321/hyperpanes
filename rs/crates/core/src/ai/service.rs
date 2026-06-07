//! Port of `src/main/ai/ai-service.ts` — the façade wiring pane_buffer → scheduler →
//! redactor → ollama → a pane subtitle. Default-OFF. Takes its userData base dir as a
//! PARAMETER (dependency injection) — do NOT call `persistence::paths` directly, so this
//! track stays independent of `persistence-cli`.
//!
//! STUB — owned by track `ai`.
