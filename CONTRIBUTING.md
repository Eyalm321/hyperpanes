# Contributing to Hyperpanes

Thanks for helping. Bug reports, fixes, docs and features are all welcome. Before you start:

- **Bugs and features:** open an [issue](https://github.com/Eyalm321/hyperpanes/issues/new/choose)
  first for anything bigger than a small fix, so we can agree on the approach before you write it.
- **Security problems:** never in a public issue. See [SECURITY.md](SECURITY.md).
- **Conduct:** this project follows the [Code of Conduct](CODE_OF_CONDUCT.md).

By contributing you agree that your contribution is licensed under the [MIT License](LICENSE).

## Repository layout

| Path | What | CI |
|---|---|---|
| `rs/` | Cargo workspace for the headless core (`rs/crates/core`) | test + clippy + rustfmt on Linux, macOS, Windows |
| `rs/crates/terminal-widget/` | Its own workspace: the Slint terminal pane | test + clippy + rustfmt on Linux, macOS, Windows |
| `rs/crates/app/` | Its own workspace: the GUI binary (Slint, wgpu) | `cargo build --locked` on Linux when a PR touches Rust |
| `mobile/hyperpanes_mobile/` | Flutter client for iOS and Android | `flutter analyze`, `flutter test`, debug APK build |
| `rs/packaging/` | Installer and bundle scripts per OS | run at release time |
| `resources/` | Shell integration and bundled agent personas | |
| `bench/` | Node benchmark harness (detect-only) | |

`rs/`, `rs/crates/terminal-widget` and `rs/crates/app` are **independent workspaces**, each with
its own `Cargo.lock`. Run Cargo from inside each one, or pass `--manifest-path`.

## Set up a build machine

You need a stable Rust toolchain ([rustup](https://rustup.rs)) on every OS.

**Linux (Debian/Ubuntu):** the same packages CI installs:

```bash
sudo apt-get install -y --no-install-recommends \
  libfontconfig1-dev libfreetype6-dev libxkbcommon-dev libwayland-dev \
  libxcb-shape0-dev libxcb-xfixes0-dev libssl-dev pkg-config cmake clang
```

On Fedora the equivalents are `fontconfig-devel freetype-devel libxkbcommon-devel
wayland-devel libxcb-devel openssl-devel pkgconf cmake clang`.

**macOS:** Xcode Command Line Tools (`xcode-select --install`). Nothing else.

**Windows:** the MSVC toolchain (Visual Studio Build Tools with the "Desktop development with
C++" workload). The `windows-latest` CI legs use nothing beyond that and stable Rust.

**Mobile:** the Flutter stable channel and Java 21 (Android). iOS builds need a Mac with Xcode.

## Build and test

These are the commands the required `verify` check runs. Run them before you open a PR.

```bash
# Tests (hard gate, every OS)
(cd rs && cargo test --all --no-fail-fast)
(cd rs/crates/terminal-widget && cargo test --all --no-fail-fast)

# Lint (blocking, Linux)
(cd rs && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings)
(cd rs/crates/terminal-widget && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings)

# GUI build (runs when a PR touches rs/crates/** or rs/Cargo.lock)
scripts/check-zbus-no-tokio.sh
(cd rs/crates/app && cargo build --locked)

# Mobile (when you touch mobile/)
(cd mobile/hyperpanes_mobile && flutter pub get && flutter analyze --no-fatal-infos && flutter test && flutter build apk --debug)

# Public-hygiene guard self-test (when you touch .github/public-guard/)
bash .github/public-guard/test.sh
```

Run the app from source with `cargo run --manifest-path rs/crates/app/Cargo.toml`. The release
build is `cargo build --release --manifest-path rs/crates/app/Cargo.toml`. The GUI crate is
heavy: the first build takes a while and several GB of disk.

**Don't test against your everyday instance.** A dev build shares the control file and state
directories with an installed Hyperpanes. Give it its own dirs, as described in
[docs/dev-instance-isolation.md](docs/dev-instance-isolation.md).

## Pull requests

1. Fork, then branch from `main`. Keep one topic per PR.
2. Write commit messages as `area: what changed`, for example `control: newPane keeps
   pane.subtitle` or `fix(core): …`. The body says why.
3. Add or update tests for behavior changes. Keep the build warning-free: clippy runs with
   `-D warnings`.
4. Update the README or `docs/` when you change user-facing behavior.
5. Open the PR against `main` and fill in the template. The **`verify`** check must be green.
   It aggregates tests, lint, the GUI build, the persona check and the public-hygiene guard.
6. Paths listed in [`.github/CODEOWNERS`](.github/CODEOWNERS) (CI, the control-plane auth,
   session spawning, secret redaction, build scripts, dependency manifests) need a maintainer's
   review. Expect closer review there.

### Keep private data out

This repository is public, and its history is permanent. The `public-guard` job fails a PR
whose commits add any of these to a line, a file path or a commit message:

- a home path with a real user name (`/home/<name>`, `/Users/<name>`, `C:\Users\<name>`);
- a private-range IP address (RFC 1918 or the 100.64/10 carrier-grade NAT range);
- anything matching [`.github/public-guard/denylist.txt`](.github/public-guard/denylist.txt).

In examples and tests, use placeholders: `/home/user`, `/Users/you`, `C:\Users\me`, and the
documentation ranges `192.0.2.x`, `198.51.100.x`, `203.0.113.x`. If a flagged value really must
be public, add the exact string to
[`.github/public-guard/allowlist.txt`](.github/public-guard/allowlist.txt) in the same PR and
say why. The guard reads its lists from the base branch, so a new allowlist entry only applies
once a maintainer has merged it.

Never commit tokens, keys, `control.json`, `device-tokens.json`, or Claude transcripts.
