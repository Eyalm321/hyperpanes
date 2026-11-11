# Self-hosted macOS runners

macOS CI jobs run on two self-hosted Mac minis registered on this repo, and fall back to
GitHub-hosted `macos-latest` when the minis can't or mustn't be used.

| runner        | labels                                        | preferred for                     |
|---------------|-----------------------------------------------|-----------------------------------|
| `mac-mini-16` | `self-hosted, macOS, ARM64, mac-mini-16`      | `rs` tests, release DMG build     |
| `mac-mini-8`  | `self-hosted, macOS, ARM64, mac-mini-8`       | `rs/crates/terminal-widget` tests |

Either mini may take any macOS job.

## Routing

`.github/workflows/mac-route.yml` is a reusable workflow. `test.yml`, `verify.yml` and
`release-rust.yml` call it as job `route` and set each macOS job's `runs-on` from its output.
Matrix values stay `macos-latest`, so check names don't change. The route job:

1. sends everything to `macos-latest` when repo variable `MAC_RUNNERS` is `off` (kill switch);
2. sends everything to `macos-latest` unless the code is trusted: `push`, `workflow_dispatch`,
   or a `pull_request` from a branch of this repo. Fork PRs and `merge_group` never run on the
   minis;
3. sends everything to `macos-latest` when secret `MAC_RUNNERS_TOKEN` is missing or the runner
   API call fails;
4. otherwise pins a job to its preferred mini when that mini is online and idle. If it's busy
   or offline, the job gets the generic `[self-hosted, macOS, ARM64]` label. If neither mini
   is online, the job goes to `macos-latest`.

`build-ios` always stays on `macos-latest`: `flutter build ios` needs full Xcode, and the minis
have only the Command Line Tools.

## Owner setup

- **Secret `MAC_RUNNERS_TOKEN`**: a fine-grained personal access token, repository access =
  this repo only, permission **Administration: read-only**, nothing else. `GITHUB_TOKEN` can't
  list self-hosted runners. Until this secret exists every macOS job runs on `macos-latest`,
  the same as before the minis.
- **Variable `MAC_RUNNERS`** (optional): set it to `off` to force `macos-latest`.

## Runner hosts

Each mini runs the Actions runner from `~/actions-runner` as a launchd service (`svc.sh`),
and the runner's `.env` sets:

```
CARGO_INCREMENTAL=0
WORK_CAP_GB=20
REG_CAP_GB=4
FLOOR_GB=20
ACTIONS_RUNNER_HOOK_JOB_STARTED=<runner dir>/cleanup-hook.sh
ACTIONS_RUNNER_HOOK_JOB_COMPLETED=<runner dir>/cleanup-hook.sh
```

The hook is `scripts/ci/mac-runner-cleanup-hook.sh`. It runs before and after every job and:

- drops `target/*/incremental`;
- keeps `_work` under `WORK_CAP_GB`, deleting the largest `target` dirs first, then whole
  checkouts;
- wipes the cargo registry `src` and `cache` once they pass `REG_CAP_GB`;
- wipes every `target` dir when free disk falls below `FLOOR_GB`;
- removes runner `_diag` logs older than 7 days.

It logs to `_diag/cleanup-hook.log` and never fails a job.

`CARGO_TARGET_DIR` is deliberately left unset: `rs/packaging/macos/bundle.sh` expects the
release binary inside the checkout.
