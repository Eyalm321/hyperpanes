# Running app version

Agents can ask which hyperpanes version (and, for non-public builds, which build) is running.

- `GET /state` — a first top-level field `app: {"version": "1.2.3", "build": "private build, abc1234"}`.
  `build` is omitted when the app has no build label (the public app). Present for every token scope.
- `GET /health` (unauthenticated) — the existing `version` field, plus an optional trailing
  `build` (omitted when unset). Build labels are sanitized: only `[A-Za-z0-9 ._,+()-]`, at most
  64 chars, and a label containing `/`, `\`, `@` or `:` is dropped, so no path, host or user leaks.
- Pane env `HYPERPANES_VERSION` — the app version (no build suffix), injected by the GUI when it
  spawns a pane. It is never persisted or restored (`env_persist` runtime key).

**Caveat:** the env var is frozen at pane spawn. After an app upgrade, panes re-attached from the
session daemon keep the old value, so `/health` or `/state` is authoritative for the running app.
