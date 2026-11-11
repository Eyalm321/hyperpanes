#!/bin/bash
# Runner job-start/job-completed hook: keeps CI disk bounded.
#  - drop cargo incremental dirs (CI builds don't benefit)
#  - _work capped at WORK_CAP_GB: delete target dirs, largest first, then whole checkouts
#  - cargo registry src/cache wiped when over REG_CAP_GB (re-downloadable)
#  - floor: if free disk < FLOOR_GB, wipe every target dir
#  - runner _diag logs older than 7 days removed
# Never fails the job.
set -u
R="$HOME/actions-runner"; W="$R/_work"
WORK_CAP_GB=${WORK_CAP_GB:-20}; REG_CAP_GB=${REG_CAP_GB:-4}; FLOOR_GB=${FLOOR_GB:-20}
LOG="$R/_diag/cleanup-hook.log"
kb() { local v; v=$(du -sk "$1" 2>/dev/null | awk '{print $1}'); echo "${v:-0}"; }
avail_gb() { df -g / | awk 'NR==2{print $4}'; }
{
echo "$(date '+%F %T') hook start: work=$(( $(kb "$W") / 1048576 ))G free=$(avail_gb)G"
find "$W" -maxdepth 7 -type d -name incremental -path '*/target/*' -prune -exec rm -rf {} + 2>/dev/null
targets() { find "$W" -maxdepth 6 -type d -name target -not -path '*/_actions/*' -not -path '*/_tool/*' -prune 2>/dev/null; }
if [ "$(avail_gb)" -lt "$FLOOR_GB" ]; then echo "free < ${FLOOR_GB}G: wiping all target dirs"; targets | xargs -I{} rm -rf {}; fi
cap=$((WORK_CAP_GB * 1048576))
if [ "$(kb "$W")" -gt "$cap" ]; then
  for t in $(targets | while read -r d; do echo "$(kb "$d") $d"; done | sort -rn | awk '{print $2}'); do
    [ "$(kb "$W")" -le "$cap" ] && break; echo "over cap: rm $t"; rm -rf "$t"; done
fi
if [ "$(kb "$W")" -gt "$cap" ]; then
  for d in "$W"/*/; do case "$d" in */_actions/|*/_tool/|*/_temp/) continue;; esac
    [ "$(kb "$W")" -le "$cap" ] && break; echo "still over cap: rm $d"; rm -rf "$d"; done
fi
REG="$HOME/.cargo/registry"
if [ -d "$REG" ] && [ "$(kb "$REG")" -gt $((REG_CAP_GB * 1048576)) ]; then echo "registry over ${REG_CAP_GB}G: pruning src+cache"; rm -rf "$REG/src" "$REG/cache"; fi
find "$R/_diag" -type f -name '*.log' -mtime +7 -delete 2>/dev/null
echo "$(date '+%F %T') hook end: work=$(( $(kb "$W") / 1048576 ))G free=$(avail_gb)G"
} >>"$LOG" 2>&1
exit 0
