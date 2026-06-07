# hyperpanes — native Rust rewrite (`rs/`)

Native (Slint) rewrite of hyperpanes, Windows-first. See the master plan at
`C:\Users\Admin\.claude\plans\effervescent-giggling-conway.md`.

This tree is being built via **fan-out** (parallel agents, one per track). The
module map in `crates/core/src/lib.rs` and the spike crates under `spikes/` are
**frozen by the scaffold** — each leaf is owned by exactly one track. Do not edit
files outside your track's ownership (see your `FANOUT-HANDOFF.md`).

## Layout

- `crates/core/` — `hyperpanes-core`, the headless Phase-1 core (no GUI). Pure
  modules ported 1:1 from `../src/main`, each with mirrored tests.
- `spikes/terminal-render/` — Phase-0 Spike A: GPU terminal-in-Slint (go/no-go).
- `spikes/tearoff/` — Phase-0 Spike B: cross-window live tear-off (go/no-go).

Parity source of truth: the TypeScript in `../src/main` and its `*.test.ts`.
