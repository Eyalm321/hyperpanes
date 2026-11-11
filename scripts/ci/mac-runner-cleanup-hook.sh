#!/bin/bash
# Runner job-start/job-completed hook: keeps CI disk bounded.
#  - drop cargo incremental dirs (CI builds don't benefit)
#  - _work capped at WORK_CAP_GB: delete target dirs, largest first, then whole checkouts
#  - cargo registry src/cache wiped when over REG_CAP_GB (re-downloadable)
#  - floor: if free disk < FLOOR_GB, wipe every target dir, the iOS caches below and
#    Xcode's shared module/compilation caches
#  - iOS build (ios-build.yml) caches:
#    - Flutter SDKs in the tool cache: only the newest one is kept
#    - pub cache wiped when over PUB_CAP_GB (re-downloadable)
#    - Xcode DerivedData of CI checkouts wiped when over DD_CAP_GB; only dirs whose
#      workspace lives under _work are touched, never other projects on the host
#    - CocoaPods download cache wiped when over PODS_CAP_GB
#  - runner _diag logs older than 7 days removed
# Never fails the job.
set -u
R="$HOME/actions-runner"; W="$R/_work"
WORK_CAP_GB=${WORK_CAP_GB:-20}; REG_CAP_GB=${REG_CAP_GB:-4}; FLOOR_GB=${FLOOR_GB:-20}
PUB_CAP_GB=${PUB_CAP_GB:-3}; DD_CAP_GB=${DD_CAP_GB:-3}; PODS_CAP_GB=${PODS_CAP_GB:-2}
PUB="${PUB_CACHE:-$HOME/.pub-cache}"; DD="$HOME/Library/Developer/Xcode/DerivedData"
PODS="$HOME/Library/Caches/CocoaPods"
LOG="$R/_diag/cleanup-hook.log"
kb() { local v; v=$(du -sk "$1" 2>/dev/null | awk '{print $1}'); echo "${v:-0}"; }
avail_gb() { df -g / | awk 'NR==2{print $4}'; }
{
echo "$(date '+%F %T') hook start: work=$(( $(kb "$W") / 1048576 ))G free=$(avail_gb)G"
find "$W" -maxdepth 7 -type d -name incremental -path '*/target/*' -prune -exec rm -rf {} + 2>/dev/null
targets() { find "$W" -maxdepth 6 -type d -name target -not -path '*/_actions/*' -not -path '*/_tool/*' -prune 2>/dev/null; }
# DerivedData dirs built from a checkout under _work (info.plist WorkspacePath).
ci_dd() {
  for d in "$DD"/*/; do [ -f "$d/info.plist" ] || continue
    case "$(plutil -extract WorkspacePath raw -o - "$d/info.plist" 2>/dev/null)" in "$W"/*) echo "${d%/}";; esac
  done
}
ci_dd_kb() { local t=0 d; while read -r d; do [ -n "$d" ] && t=$((t + $(kb "$d"))); done < <(ci_dd); echo "$t"; }
if [ "$(avail_gb)" -lt "$FLOOR_GB" ]; then
  echo "free < ${FLOOR_GB}G: wiping all target dirs + iOS caches"; targets | xargs -I{} rm -rf {}
  ci_dd | while read -r d; do rm -rf "$d"; done; rm -rf "$PUB/hosted" "$PUB/git" "$PODS"
  rm -rf "$DD/ModuleCache.noindex" "$DD/CompilationCache.noindex"
fi
# Flutter SDKs (subosito/flutter-action, one dir per version): keep only the newest.
if [ -d "$W/_tool/flutter" ]; then
  ls -1td "$W/_tool/flutter"/*/ 2>/dev/null | tail -n +2 | while read -r d; do echo "old Flutter SDK: rm $d"; rm -rf "$d"; done
fi
if [ -d "$PUB" ] && [ "$(kb "$PUB")" -gt $((PUB_CAP_GB * 1048576)) ]; then echo "pub cache over ${PUB_CAP_GB}G: pruning"; rm -rf "$PUB/hosted" "$PUB/git"; fi
if [ -d "$DD" ] && [ "$(ci_dd_kb)" -gt $((DD_CAP_GB * 1048576)) ]; then
  echo "CI DerivedData over ${DD_CAP_GB}G: wiping"; ci_dd | while read -r d; do rm -rf "$d"; done
fi
if [ -d "$PODS" ] && [ "$(kb "$PODS")" -gt $((PODS_CAP_GB * 1048576)) ]; then echo "CocoaPods cache over ${PODS_CAP_GB}G: wiping"; rm -rf "$PODS"; fi
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
