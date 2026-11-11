# Self-hosted macOS runners

macOS CI jobs run on two self-hosted Mac minis registered on this repo, and fall back to
GitHub-hosted `macos-latest` when the minis can't or mustn't be used.

| runner        | labels                                          | preferred for                                |
|---------------|-------------------------------------------------|----------------------------------------------|
| `mac-mini-16` | `self-hosted, macOS, ARM64, mac-mini-16, xcode` | `rs` tests, release DMG build                |
| `mac-mini-8`  | `self-hosted, macOS, ARM64, mac-mini-8, xcode`  | `rs/crates/terminal-widget` tests, iOS build |

Either mini may take any macOS job. Both have full Xcode in `/Applications/Xcode.app` and
carry the label `xcode`. The iOS build runs only on runners with that label.

## Routing

`.github/workflows/mac-route.yml` is a reusable workflow. `test.yml`, `verify.yml`,
`release-rust.yml` and `ios-build.yml` call it as job `route` and set each macOS job's
`runs-on` from its output.
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
5. routes the iOS build (output `ios`) among runners labelled `xcode` only. It pins the job
   to an idle one, `mac-mini-8` first, so that in a release it runs beside the DMG build on
   `mac-mini-16`. If every `xcode` runner is busy, the job gets the generic
   `[self-hosted, macOS, ARM64, xcode]` label and waits for whichever frees up first. If none
   is online, it goes to `macos-latest`.

## iOS build

`.github/workflows/ios-build.yml` holds the only copy of the iOS build (`flutter build ios
--release --no-codesign`, then the zipped `Runner.app`). It runs:

- from `release-rust.yml` job `build-ios` (`upload: true`): attaches
  `hyperpanes-mobile-ios-<version>-unsigned.zip` to the release, as before;
- by hand (`workflow_dispatch`): builds, no upload;
- on pull requests that touch `mobile/hyperpanes_mobile/{lib,ios}/**`, its `pubspec.*`,
  `ios-build.yml` or `mac-route.yml`: builds, no upload.

One build runs per ref; a newer push cancels the running one. A release upload is never
cancelled.

On a self-hosted runner the job sets `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`,
so it doesn't depend on `xcode-select`, which on a mini may still point at the Command Line
Tools (switching it needs sudo). Dart native-asset build hooks run with a scrubbed
environment (only `HOME`, `PATH` and `TMPDIR`), so `DEVELOPER_DIR` never reaches the
`xcrun --show-sdk-path --sdk iphoneos` they call. The job therefore puts a two-line `xcrun`
wrapper first on `PATH` (in `$RUNNER_TEMP`) that re-adds it. Hosted runners skip all of this.

The app's iOS plugins resolve through Swift Package Manager, so the build doesn't need
CocoaPods. On a self-hosted runner `subosito/flutter-action` keeps the Flutter SDK in the
runner's tool cache (`_work/_tool/flutter/<channel>-<version>-<arch>`, about 4 GB with the iOS
engine artifacts) and skips its `actions/cache` round trip.

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

For the iOS build it also:

- keeps only the newest Flutter SDK in `_work/_tool/flutter` (each new stable release adds
  a ~4 GB directory);
- wipes the pub cache (`$PUB_CACHE`, default `~/.pub-cache`) once it passes `PUB_CAP_GB`
  (default 3);
- wipes Xcode DerivedData built from CI checkouts once those dirs pass `DD_CAP_GB`
  (default 3). It picks them by the `WorkspacePath` in each dir's `info.plist`, so other
  projects on the host are never touched;
- wipes the CocoaPods download cache once it passes `PODS_CAP_GB` (default 2). Nothing uses
  it today; it's there in case a future plugin needs CocoaPods.

Below `FLOOR_GB` it wipes those caches too, along with Xcode's shared `ModuleCache.noindex`
and `CompilationCache.noindex`. The Flutter SDK lives under `_work`, so it counts toward
`WORK_CAP_GB`, but the cap never deletes it: it just leaves less room for checkouts.

It logs to `_diag/cleanup-hook.log` and never fails a job.

`CARGO_TARGET_DIR` is deliberately left unset: `rs/packaging/macos/bundle.sh` expects the
release binary inside the checkout.
