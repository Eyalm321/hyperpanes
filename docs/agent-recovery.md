# Agent-Pane API-Error Recovery

Headless goal agents (spec agents, impl agents) run unattended for hours inside hyperpanes
panes. Two failure modes don't show up as a crash: the pane's Claude process hits an
`API Error:` and either auto-retries (fine, still alive) or goes idle forever; or a stray
tool-result record left over from a killed turn **poisons** the transcript so `--resume`
can never recover it through the documented path. Both leave the pane sitting there,
looking exactly like a slow but healthy agent. This doc is the contract for detecting and
recovering from both, implemented in `rs/crates/core/src/claude_recovery.rs`
(`detect_api_error`, `classify_api_error`, `resolve_session_candidates`,
`repair_transcript`) and exposed as the control command `recoverPane`.

## Why detection is exit-based and conservative

There is no structured signal for "this agent died mid-turn." The `SessionStart` hook that
stamps a pane's live-session marker only fires on a *new* session start — if the process
dies before its first turn completes, the marker is never written, so the one documented
recovery path (`restart_pane resume:true`) errors with "no live Claude marker" at exactly
the moment it's needed most. And an orchestrator watchdog that only tracks queue claims or
branch commits sees nothing either: an agent that dies before its first enqueue or first
commit produces **no** queue/branch signal, ever.

The one thing that's always present is the pane's own scrollback. So detection reads the
pane tail and looks for the **last** `API Error:` line — that's the only meaningful
"pane is not working" signal available, and it's read-only and idempotent to check.
Two things follow from that:

- **`retrying: true` means still alive — do not act.** Claude's own retry loop prints
  `API Error:` lines while it backs off and retries automatically. Treat this as *working*,
  not *wedged*; acting on it (restart/repair) races the process's own recovery and can
  compound the problem.
- Detection never guesses from silence alone. A quiet pane mid-compile or mid-tool-call is
  not distinguishable from a dead one by timer; the tail is what disambiguates.

## Session resolution: marker, then scan

To resume or repair a transcript you need its `sessionId` and the `CLAUDE_CONFIG_DIR` it
belongs to (accounts each have their own `projects` store, and resuming under the wrong
account fails). Resolution tries, in order:

1. **Marker** — the pane's live-session marker, if `SessionStart` wrote one. Fast path,
   exact match, `source:"marker"`.
2. **Scan** — if there's no marker (the death-before-first-turn gap above), fall back to
   scanning **every** per-account `$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/` store for
   transcripts matching the pane's cwd, across all accounts in rotation — not just the
   pane's current `CLAUDE_CONFIG_DIR`, since the pane may have started under a different
   account than it's running under now. `source:"scan"` returns `candidates`, newest
   `mtimeMs` first (recency is the only ordering signal available; there's no other way to
   guess which transcript the dead process was writing to).
3. **Explicit override** — an operator-supplied `sessionId` wins over both when given.

**Encoded-cwd rule:** Claude encodes a project's working directory into its store path by
keeping `[A-Za-z0-9]` and replacing every other character with `-`. Scanning must reproduce
this exactly (not a fuzzy match) to land in the right per-account directory.

**Why every candidate carries `configDir`:** a `sessionId` alone is not enough to resume —
`claude --resume <id>` reads from whatever `CLAUDE_CONFIG_DIR` is set in the environment it's
launched with, and the same cwd can have transcripts under more than one account. Without
`configDir` traveling with the candidate, a caller could resume the right session under the
wrong account and get a fresh 401/empty-history instead of the real transcript.

## Repair: surgical, byte-preserving, idempotent

A poisoned transcript is a JSONL file where a tool-call record's matching tool-result was
never written (the process died between the call and the result — often triggered by a
tool the harness can't answer, e.g. a first-turn `ToolSearch` call that never resolves).
`--resume` on a transcript like that fails deterministically, every time, forever — it's
not a race, so retrying without repairing just reproduces the same failure.

Repair is deliberately narrow:

- **Surgical** — it drops *only* the orphaned tool-call/tool-result record(s); every other
  line is untouched.
- **Byte-preserving** — lines that are kept are copied verbatim, not re-serialized. This
  avoids incidental reformatting (key order, whitespace, float formatting) turning into a
  diff noise or a subtly different transcript than the one Claude wrote.
- **Idempotent** — running repair on an already-healthy transcript is a no-op (`dropped: []`,
  no backup written). Running it twice on the same poisoned transcript is safe: the second
  run finds nothing left to drop.
- **Backed up first** — before any mutation, the original is copied to a timestamped `.bak`
  path alongside it. Repair never overwrites without a recovery copy behind it.

See `rs/crates/core/tests/fixtures/g2-poisoned-transcript.jsonl` for a worked example: a
33-line transcript where the orphaned tool-result record sits at line index **9** (0-based)
— an unanswered tool-call with no matching result before the transcript ends.

## The `recoverPane` control command

```
POST /command
{
  "type": "recoverPane",
  "paneId": "<id>",
  "action": "inspect" | "repair" | "resume",
  "sessionId"?: "<uuid override>",
  "force"?: bool
}
```

### `action: "inspect"` (read-only)

```json
{
  "activity": "...",
  "apiError": { "code": "...", "detail": "...", "retrying": true } ,
  "class": "transient" | "account-limit" | "poisoned" | "unknown" | null,
  "session": { "source": "marker", "sessionId": "...", "configDir": "...", "cwd": "..." }
}
```

or, when there's no marker:

```json
{ "session": { "candidates": [ { "sessionId": "...", "configDir": "...", "path": "...", "mtimeMs": 0 } ] } }
```

`apiError` is `null` when the last meaningful pane activity wasn't an API error. Always
inspect before acting — never repair or resume on a guess.

### `action: "repair"`

Resolves the transcript (marker session, else the newest same-cwd candidate across all
per-account stores, else an explicit `sessionId` override), and if `class == "poisoned"`
(or `force: true`), writes the `.bak` and excises only the orphaned record(s):

```json
{ "sessionId": "...", "path": "...", "dropped": [9], "backup": "<path>.bak" }
```

A healthy transcript returns `dropped: []` and `backup: null` — repair is safe to call
speculatively.

### `action: "resume"`

Restarts the pane with the resolved (and, if needed, just-repaired) session — the
`restart_pane`-with-resume path, so the conversation comes back intact. **Never call
`resume` on a `poisoned` transcript without repairing it first** — resuming a poisoned
transcript directly reproduces the exact same wedge. For `account-limit`, rotate
`CLAUDE_CONFIG_DIR` to a different, non-exhausted account *before* resuming (same account
just re-hits the same limit); for `transient`, resume under the same account.

### Class policy

| `class`         | Meaning                                              | Action                                   |
|-----------------|-------------------------------------------------------|-------------------------------------------|
| `transient`     | Retryable API blip, process is otherwise fine          | `resume`, same account                    |
| `account-limit` | Rate/weekly limit hit on this account                  | rotate `CLAUDE_CONFIG_DIR`, then `resume` |
| `poisoned`      | Orphaned tool-result record wedges `--resume` forever  | `repair`, then `resume` — never resume raw |
| `unknown`       | Doesn't match a known pattern                          | escalate to the human/orchestrator — never thrash restarts |

## Watchdog guidance (orchestrator & spec agent)

`resources/claude/goal-orchestrator/SKILL.md` and `SPEC.md` both carry a watchdog addition
built on this contract: watch pane **activity and API-error state**, not only queue/branch
movement — an agent that dies before its first enqueue or commit is invisible to queue and
branch signals alike, and the tail's `API Error:` line is the only tell. On a pane idle
longer than its work should take, run `recoverPane action:"inspect"` and apply the class
policy above. Both personas also warn every spawned agent off calling `ToolSearch`/deferred
tool loading — a first-turn `ToolSearch` call is exactly the kind of unanswered tool call
that produces the poisoned-transcript pattern this document describes.
