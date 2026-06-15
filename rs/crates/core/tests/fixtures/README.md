# Cross-language wire fixtures

## `task-wire.golden.json`

The canonical serialized form of a fully-populated `Task` (see
`rs/crates/core/src/control/work.rs`). It was **derived**, not hand-written:
a fully-populated `Task` (every field set, including the flattened lease fields
`claimedBy` / `fencingToken` / `visibilityDeadline` and a non-null `result`) was
run through `serde_json::to_string_pretty`, and the exact emitted bytes were
captured here.

The Rust serialization is **authoritative**. This file is the contract both the
Rust queue and the TypeScript controller must agree on: camelCase keys,
epoch-ms NUMBER timestamps, and a FLATTENED lease (never a nested `lease` object).

> **MUST stay byte-identical** with its twin in the controller repo:
> `hpmcp-controller/test/fixtures/task-wire.golden.json`.
> Both copies are checked by a contract test in each repo. If you regenerate this
> golden, update BOTH copies in the same change or the contract tests break.
