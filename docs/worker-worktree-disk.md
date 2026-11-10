# Worker worktrees on disk: root, orphan sweep, shared build cache, disk guard

`hyperpanes worker --worktree` runs every task in a throwaway git worktree. This page covers
**where** those worktrees live, how a crashed runner's leftovers get cleaned up, how task
worktrees of one repo share a warm build cache, and the free-space check made before each
worktree is created. The fork point (`--base`) is covered in
[`worker-worktree-base.md`](worker-worktree-base.md).

## The defect

Task worktrees used to go to `std::env::temp_dir()/hp-wt-<queue>-<id8>-<pid>`, which is `/tmp`
on Linux. On a machine where `/tmp` is a RAM-backed tmpfs with a per-user quota (Fedora's
default), every Rust impl agent cold-built its own `target/` (about 7.5 GB for the app crate)
inside its worktree:

- Two or three concurrent goals filled the 32 GB quota. Every process of that user writing to
  `/tmp` then failed with `EDQUOT`, including every Claude Code session's Bash tool, which keeps
  its output under `/tmp`. Agents had to delete their own build caches to recover, twice in one
  day.
- Those builds were billed to RAM.
- Every worktree started from a cold `target/`, so each task paid for a full dependency build.

## Where worktrees live now

The **worktree root** resolves in this order:

1. `--worktree-root <dir>`
2. `HYPERPANES_WORKTREE_ROOT` (empty means unset)
3. the default, `<cache dir>/hyperpanes/worktrees`, where the cache dir is
   - Linux: `$XDG_CACHE_HOME` (absolute values only), default `~/.cache`
   - macOS: `~/Library/Caches`
   - Windows: `%LOCALAPPDATA%`

A relative value is resolved against the runner's cwd. The default is never under the system
temp dir.

Each repo gets its own directory under the root, keyed by the repo's git **common dir**, so every
linked worktree of one repo (for example a goal's private integration worktree) shares it:

```text
<root>/<repo>-<hash8>/
  repo                         # the repo's git common dir, for humans
  wt/<queue>-<id8>-<pid>/      # a task worktree (branch worker/<queue>/<id8>)
  wt/<queue>-<id8>-<pid>.lock  # its owner lock
  cache/slot-<k>/              # build-cache slot k  → HP_WORKTREE_CACHE_DIR
  cache/slot-<k>.lock          # its lock; mtime = last release
  cache/slot-<k>.size          # last measured size in bytes (see "Size limits")
  evidence/<queue>-<id8>-<pid>.patch  # uncommitted work saved before removal (see below)
```

From an MCP client, `spawn_workers` carries the override through its existing `env` passthrough,
so the bridge needs no change: `spawn_workers {…, isolation:"worktree", base:"main",
env:{HYPERPANES_WORKTREE_ROOT:"/data/hp-worktrees"}}`.

## Auto-removal and the orphan sweep

A finished task's worktree is removed as before (`git worktree remove --force`, then whatever is
left), and its branch stays. Before it is removed, the runner checks the task's result and saves
any uncommitted work (see [Task results and evidence patches](#task-results-and-evidence-patches)).

Each task worktree also has an **owner lock**, an OS file lock (`flock` on unix, `LockFileEx` on
Windows) on `wt/<name>.lock`. The runner takes it *before* creating the worktree and holds it
until the worktree is gone. On unix the lock's file descriptor is also inherited by that task's
child process, and only that one. So the lock stays held while *any* process of the task tree is
alive: a runner that dies while its `claude` child keeps working does not get the child's tree
swept out from under it.

When a `--worktree` runner starts, it sweeps its repo's directory:

- A `wt/<name>` whose lock file is missing, or whose lock it can take, is an **orphan** (its
  owner is dead). The runner saves the orphan's uncommitted changes to an evidence patch, logs
  its branch, the change count and the patch path, removes it (`git worktree remove --force`,
  falling back to deleting the directory), then deletes the lock file.
- A `wt/<name>` whose lock is held is live and left alone.
- A lone lock file older than 24 hours with a free lock is deleted.
- Then `git worktree prune` drops stale admin entries.
- Evidence patches older than 14 days are deleted.

The OS releases a dead process's locks, so a SIGKILLed or OOM-killed runner's worktree is
reclaimed by the next runner that starts for the same repo. Pid reuse can't fool the check the
way a pid-liveness test could. Branches are never deleted, so commits made under the
commit-first protocol survive.

Worktrees left in `/tmp/hp-wt-*` by older builds aren't swept. They're on tmpfs and disappear at
reboot, and `git worktree prune` clears their admin entries.

## Task results and evidence patches

A `--worktree` task's result is its commits. The runner used to ack every exit 0 and then delete
the worktree, so a child that exited cleanly without committing (#86) counted as done and its
uncommitted work disappeared with the directory. Now, after the child exits and **before** the
worktree is removed, the runner:

1. Counts `git rev-list --count <fork-sha>..worker/<queue>/<id8>`. `<fork-sha>` is the exact
   commit the worktree was created from, resolved at creation, not the `--base` ref. That ref can
   advance mid-task (a dependent wave's integration branch), and counting against it would hide
   or invent commits.
2. If the tree is dirty, saves the uncommitted work (tracked changes plus untracked files that
   aren't gitignored, so `target/` and other build output stay out) as a binary-safe patch
   against the worktree's HEAD, at `<root>/<repo>-<hash8>/evidence/<queue>-<id8>-<pid>.patch`.
   To recover it, run `git apply <patch>` on a checkout of the task branch. The save stages into
   the dying worktree's index but never commits, and it runs after the commit count. A patch can
   contain anything the agent left lying around, such as a stray `.env` that isn't gitignored, so
   the `evidence/` dir is `0700`, each patch is `0600`, and the runner logs only the path, never
   the contents.

Then:

| Child exit | Commits beyond the fork sha | Outcome |
|---|---|---|
| 0 | ≥ 1 | ack. If uncommitted work was also left, the patch is saved and a warning logged. |
| 0 | 0 | **failed**: nack with `requeue: false`, reason `no commit on worker/<queue>/<id8> beyond base <sha12> (child exited 0); uncommitted changes saved to <patch>`, or `…; worktree was clean, nothing saved`. |
| 0 | unknown (git failed) | retryable nack, `cannot verify commits on <branch>: <error>`. An unverified result is never acked. |
| non-zero / runner error | any | the usual retryable nack (backoff, `max_attempts`), with `; uncommitted changes saved to <patch>` appended when there was work to save. |

An empty result is failed outright rather than retried. A task that reported success without
committing would most likely do the same thing again, and every retry would burn a full agent run.
A failed task isn't claimed, so it never shows up as stranded.

If the patch can't be written, the reason says so and the worktree directory is **kept**: its
owner lock is released, so the next runner's startup sweep treats it as an orphan, retries the
save, and then removes it.

**Retention.** Patches are kept 14 days (the same as idle cache slots) and deleted by the startup
sweep. They're small, since ignored build output is excluded, so there's no size cap. Cache slots
are untouched by any of this.

**Empty results that are legitimate.** Some queues produce no commit by design, such as read-only
checks or reports delivered over the message bus. Run those runners with `--allow-empty-result`
(or env `HYPERPANES_WORKER_ALLOW_EMPTY_RESULT=1`; `true`/`yes`/`on` also work), and an exit 0 acks
as before. Patches are still saved. From MCP, pass it through `spawn_workers`' `env`:
`env:{HYPERPANES_WORKER_ALLOW_EMPTY_RESULT:"1"}`. The flag only matters with `--worktree`;
without a worktree there is no branch to check, and exit 0 still acks.

The goal personas already tell impl agents to commit before they exit. This makes the runner
enforce it.

## Shared warm build cache: per-repo slots

Every task gets `HP_WORKTREE_CACHE_DIR`, a persistent **cache slot** for its repo, next to the
existing `HP_WORKTREE`. The runner takes the lowest-numbered slot whose lock is free and holds
that lock exclusively for the whole task:

- **Concurrent workers never share a slot**, so N workers can never corrupt each other's build
  state or queue behind each other's build locks.
- **A slot is reused serially**, so a worker's next task, or the next runner on the same repo
  (another goal, a later wave), starts warm.
- It is plain directories plus locks, so it works on any filesystem.

The runner stays language-agnostic: it only exports the directory. Projects decide what to put
there. The goal personas point cargo at it in the impl-agent spawn command:

```sh
env ${HP_WORKTREE_CACHE_DIR:+"CARGO_TARGET_DIR=$HP_WORKTREE_CACHE_DIR/cargo-target"} claude …
```

`CARGO_TARGET_DIR` is set only when the runner exported a slot, so older runners and non-worktree
runs are unaffected. Build outputs then land in `$CARGO_TARGET_DIR/debug/…`, not
`<worktree>/target/`.

The slot's target dir was last used by a worktree at a different path. Cargo hashes workspace
path packages relative to the workspace root, so the expensive part (registry and git
dependencies) stays fresh. Only the workspace's own crates rebuild, because the fresh checkout's
files are newer than their artifacts. Measured numbers are in the PR that introduced this.

**Bounding disk.** A slot persists after its task, so a concurrency peak would otherwise pin its
disk:

- **Eviction under disk pressure.** When the disk guard (below) finds the root short of space,
  it first deletes idle slots, those whose lock it can take, least recently used first, across
  every repo under the root, until the minimum is met. Only then does it give up on the claim.
- **Age GC.** The startup sweep deletes idle slots unused for 14 days.
- **Optional cap.** `--max-cache-slots <n>` / `HYPERPANES_WORKTREE_MAX_SLOTS` (default `0`, no
  cap). A task that finds every slot under the cap busy gets an uncached `wt/<name>.cache` dir:
  a cold build, owned by its task lock and deleted with the worktree. It doesn't block, and it
  never shares a slot. There's no cap by default because eviction already reclaims idle slots
  on demand, and at peak a busy slot holds exactly what a per-worktree `target/` would have held
  anyway.

### Size limits: per-slot cap, root budget, soft floor

Age GC and pressure eviction don't bound a slot's size *while it is reused*. Every version bump,
`Cargo.lock` change, feature set or branch adds new artifact variants to the same
`cargo-target/`, plus `incremental/` dirs, so a single slot once grew to 56 GB and filled the
disk. Three knobs bound it. Each takes a size (bytes or `K/M/G/T`), and **`0` turns it off**.
Precedence is flag > env > default.

| Knob | Flag | Env | Default |
| --- | --- | --- | --- |
| Per-slot cap | `--slot-max-size <size>` | `HYPERPANES_WORKTREE_SLOT_MAX` | 20G |
| Root-wide budget | `--cache-max-size <size>` | `HYPERPANES_WORKTREE_CACHE_MAX` | 100G |
| Proactive free-space floor | `--soft-free <size>` | `HYPERPANES_WORKTREE_SOFT_FREE` | min(40G, 10% of the disk) |

- **Per-slot cap.** Enforced while holding the slot's lock, at two points: when the task's child
  exits (before the slot is released), and right after a slot is acquired (a backstop for slots
  left by older runners). An over-cap slot is pruned in two stages: first every `incremental/` dir
  under `cargo-target` is deleted; if the slot is still over the cap, its contents are deleted
  (the slot dir and its lock file are kept).
- **Soft floor default.** Without a flag or env value, the soft floor is the smaller of 40G and a
  tenth of the total size of the filesystem holding the worktree root: 10G on a 100G disk, 40G on
  anything from 400G up. A flat 40G on a small disk (a laptop SSD, a CI runner) would evict every
  idle warm slot on every claim and the cache would never stay warm. An explicit value always wins,
  and `0` turns the floor off.
- **Root budget.** The sum of all slot sizes across every repo under the root (they share the
  filesystem) is checked at runner start and between claims. Over budget, idle slots are evicted
  least recently used first until the total fits. Busy slots count toward the total but can't be
  evicted.
- **Soft floor.** Between claims, if free space on the root is below the floor, idle slots are
  evicted LRU until it is met. It sits above the hard `--min-free` refusal, which is unchanged: it
  keeps the disk from ever reaching that refusal.

**Size sidecar.** Walking a large cache is slow, so a release writes the measured size to
`cache/slot-<k>.size` (decimal bytes, written under the lock). The release stamps the lock file's
mtime first and writes the sidecar second, so a sidecar whose mtime is at least the lock's was
measured after the last release and is fresh. A fresh sidecar lets acquire and the budget check skip
the walk; otherwise the slot is walked. Sizes are on-disk usage (like `du`), symlinks are never
followed, and every walk is logged with its duration (`slot-3: measured 31.0 GiB in 2.4s`).

Every action logs one line, for example:

```text
slot-3: pruned incremental 11.2 GiB (31.0 GiB > cap 20.0 GiB)
evicted idle myapp-1a2b3c4d/slot-1 (57.0 GiB; cache 160.0 GiB > budget 100.0 GiB)
evicted idle myapp-1a2b3c4d/slot-2 (free 35.0 GiB < soft floor 40.0 GiB)
```

**Manual prune.** `hyperpanes worker --prune-cache [--dry-run]` applies the same rules once. It
resolves the root like the worker (`--worktree-root`, env, default), needs no queue and doesn't
touch the control plane. For every idle slot it applies the per-slot cap, then the root budget and
floor, and prints one line per slot with the action and before/after size. Busy slots print
`busy, skipped`. `--dry-run` prints the plan and deletes nothing. The three size flags above apply
to it too.

Slot lock files are never deleted, because deleting one races a concurrent acquirer. To reclaim
everything for a repo by hand, stop its runners and `rm -rf <root>/<repo>-<hash8>/cache`.

### Alternatives considered

- **One shared `CARGO_TARGET_DIR` per repo or base.** Cargo takes an exclusive lock on the
  target dir, so N concurrent workers run their builds one at a time. Worktrees at different
  paths also keep invalidating each other's workspace crates.
- **sccache.** It isn't installed by default and can't cache bin, dylib or proc-macro crates
  (the final link and slint's macros). Keying across different absolute paths needs extra setup.
  It stays compatible: an agent can still set `RUSTC_WRAPPER` itself.
- **A reflink copy of a warm seed per task.** It needs a copy-on-write filesystem (it degrades to
  a full copy on ext4), plus seed-promotion races and locking. Under the lowest-free-slot policy,
  a new slot is only created when every existing slot is busy, so a seed only beats slots when
  concurrency exceeds the previous peak.

## Disk guard

After claiming a task and **before** creating its worktree, the runner checks free space on the
worktree root's filesystem against a minimum:

- `--min-free <size>` > `HYPERPANES_WORKTREE_MIN_FREE` > default **10G**. Sizes are bytes or
  `K/M/G/T` (binary units, optional `B`/`iB`). `0` disables the check. An invalid env value is a
  startup error.
- If space is short, idle cache slots are evicted first (LRU, see above). If that doesn't free
  enough, nothing is created and the child is not run. The task is nacked with
  `insufficient disk space for a worktree on <root>: <avail> free < <min> required (set
  HYPERPANES_WORKTREE_MIN_FREE / --min-free; 0 disables)` and a 60 s backoff (`--nack-delay`
  overrides it). Then **that worker stops**: low disk is machine-wide, so claiming more tasks
  would only burn their attempts. The reason shows in the task's last error, so whoever watches
  the queue sees why and can free space and respawn.

The check reads filesystem free space (`statvfs` / `GetDiskFreeSpaceExW`). It doesn't see
per-user quotas, which is one more reason not to point the root at a quota'd tmpfs.
