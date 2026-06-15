//! Cross-language wire contract for the queue `Task` record.
//!
//! The Rust `Task` (`control::work`) and the TS controller (`hpmcp-controller`'s
//! `src/worker-pool/kinds.ts`) hand-mirror the SAME wire shape: camelCase keys,
//! epoch-ms NUMBER timestamps, and a FLATTENED lease (`claimedBy` / `fencingToken`
//! / `visibilityDeadline` at the top level — never a nested `lease` object).
//!
//! `tests/fixtures/task-wire.golden.json` is the canonical golden, derived from
//! `serde_json::to_string_pretty` of a fully-populated `Task`. The Rust
//! serialization is authoritative; the TS side holds a byte-identical copy at
//! `hpmcp-controller/test/fixtures/task-wire.golden.json` and asserts the same
//! keys. If this test drifts, the two sides have silently diverged.

use hyperpanes_core::control::work::{Task, TaskState};

const GOLDEN: &str = include_str!("fixtures/task-wire.golden.json");

#[test]
fn golden_deserializes_and_round_trips() {
    // The golden must parse into the real `Task` type...
    let task: Task = serde_json::from_str(GOLDEN).expect("golden must deserialize into Task");

    // ...and a few load-bearing values must survive the trip (proves the camelCase
    // keys actually mapped onto the right fields, not just parsed-and-ignored).
    assert_eq!(task.state, TaskState::Claimed);
    assert_eq!(task.claimed_by.as_deref(), Some("wkr-A"));
    assert_eq!(task.fencing_token, Some(7));
    assert_eq!(task.visibility_deadline, Some(1_700_000_030_000));
    assert_eq!(task.max_attempts, 5);
    assert_eq!(task.available_at, 1_700_000_000_000);
    assert_eq!(task.result.as_deref(), Some("artifact://built/ok"));

    // Re-serializing must round-trip to the SAME JSON value as the golden (ignoring
    // only whitespace — `Value` comparison is structural).
    let from_golden: serde_json::Value = serde_json::from_str(GOLDEN).expect("golden as Value");
    let reserialized: serde_json::Value = serde_json::to_value(&task).expect("Task to Value");
    assert_eq!(
        reserialized, from_golden,
        "re-serializing the parsed Task must reproduce the golden JSON exactly"
    );
}

#[test]
fn golden_uses_exact_camelcase_keys_and_no_snake_case() {
    let v: serde_json::Value = serde_json::from_str(GOLDEN).expect("golden as Value");
    let obj = v.as_object().expect("golden is a JSON object");

    // The load-bearing camelCase keys MUST be present with these exact names.
    for key in [
        "claimedBy",
        "fencingToken",
        "visibilityDeadline",
        "maxAttempts",
        "availableAt",
        "createdAt",
        "updatedAt",
        "dedupeKey",
    ] {
        assert!(
            obj.contains_key(key),
            "golden must contain camelCase key `{key}`"
        );
    }

    // And NO snake_case variant may appear — that would mean the wire drifted.
    for bad in [
        "claimed_by",
        "fencing_token",
        "visibility_deadline",
        "max_attempts",
        "available_at",
        "created_at",
        "updated_at",
        "dedupe_key",
    ] {
        assert!(
            !obj.contains_key(bad),
            "golden must NOT contain snake_case key `{bad}`"
        );
    }

    // Timestamps are NUMBERs (epoch ms), not strings.
    for ts in [
        "availableAt",
        "visibilityDeadline",
        "createdAt",
        "updatedAt",
    ] {
        assert!(
            obj.get(ts).map(|x| x.is_number()).unwrap_or(false),
            "`{ts}` must be a JSON number (epoch ms)"
        );
    }
}
