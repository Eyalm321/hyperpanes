#!/usr/bin/env bash
# Read-only: show whether process <pid> is on the D-Bus session bus and registered with the
# AT-SPI registry (i.e. visible to screen readers). Only issues introspection reads (busctl list,
# GetAddress, GetChildren, the Name property); it never calls a method that changes state.
# Usage: scripts/atspi-check.sh <pid>    (or $GUI_PID, as set by repro-no-session-bus.sh --then)
set -u
PID=${1:-${GUI_PID:-}}
[ -n "$PID" ] || { echo "usage: $0 <pid>" >&2; exit 2; }

echo "session-bus connections owned by pid $PID:"
busctl --user --no-pager list 2>/dev/null | awk -v p="$PID" '$2==p {print "  " $1}'

A11Y=$(busctl --user call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress 2>/dev/null |
  sed -E 's/^s "(.*)"$/\1/')
EN=$(busctl --user get-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled 2>/dev/null)
echo "a11y bus: ${A11Y:-<none>} (IsEnabled: ${EN:-?})"
[ -n "$A11Y" ] || exit 0

NAMES=$(busctl --address="$A11Y" --no-pager list 2>/dev/null | awk -v p="$PID" '$2==p {print $1}')
echo "a11y-bus connections owned by pid $PID:"
for n in $NAMES; do echo "  $n"; done

REG=$(busctl --address="$A11Y" call org.a11y.atspi.Registry /org/a11y/atspi/accessible/root \
  org.a11y.atspi.Accessible GetChildren 2>/dev/null)
FOUND=0
for n in $NAMES; do
  if printf '%s' "$REG" | grep -q "\"$n\""; then
    APP=$(busctl --address="$A11Y" get-property "$n" /org/a11y/atspi/accessible/root \
      org.a11y.atspi.Accessible Name 2>/dev/null)
    echo "registered with the AT-SPI registry: $n (application Name: ${APP:-?})"
    FOUND=1
  fi
done
[ "$FOUND" = 1 ] || echo "NOT registered with the AT-SPI registry"
