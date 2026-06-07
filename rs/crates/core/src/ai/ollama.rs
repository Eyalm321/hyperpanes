//! Port of `src/main/ai/ollama-client.ts` — a minimal Ollama HTTP client via `reqwest`:
//! per-request timeout, `cleanLine` (80-char clamp), `normalizeEndpoint`, and `ping` that
//! NEVER throws (returns a bool, swallows errors). Mirror `ollama-client.test.ts`.
//! (User's Ollama lives at 192.168.0.11 / a gemma model — keep endpoint + model configurable.)
//!
//! STUB — owned by track `ai`.
