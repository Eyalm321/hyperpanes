#!/usr/bin/env bash
# Fail if any Rust workspace builds zbus with its `tokio` feature.
#
# zbus is compiled once per dependency graph with the union of its users' features. With `tokio`
# on, its executor is bare tokio::spawn/spawn_blocking, which panics on any thread outside a Tokio
# runtime. accesskit_unix's AT-SPI thread (and ksni, slint's xdg-settings watcher) call zbus from
# threads they own, so a single dep enabling zbus/tokio (rfd's `tokio` feature did, via ashpd)
# panicked the GUI on every Linux launch. See rs/crates/app/src/dbus.rs and the rfd note in
# rs/crates/app/Cargo.toml. This is the cheap CI tripwire: it resolves features only and compiles
# nothing. The dbus.rs tests catch the same regression at runtime.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
fail=0
for ws in rs/crates/app rs rs/crates/terminal-widget; do
  if ! out=$(cd "$ROOT/$ws" && cargo tree --locked -e features -i zbus --target all 2>&1); then
    if grep -q "did not match any packages" <<<"$out"; then
      echo "ok   $ws: no zbus in the graph"
      continue
    fi
    echo "error resolving $ws:"; echo "$out"; exit 2
  fi
  if grep -q 'zbus feature "tokio"' <<<"$out"; then
    echo "FAIL $ws: zbus is built with its tokio feature. Enabled by:"
    grep -A8 'zbus feature "tokio"' <<<"$out" | sed 's/^/    /'
    fail=1
  else
    echo "ok   $ws: zbus without tokio ($(grep -cE '^(├|└)── zbus feature' <<<"$out") features, all targets)"
  fi
done
exit $fail
