#!/usr/bin/env bash
# Self-test for the public-guard: builds a throwaway repo, makes one branch per case, and
# checks the guard's verdict. Run: bash .github/public-guard/test.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
cfg=$(mktemp -d)
trap 'rm -rf "$tmp" "$cfg"' EXIT
cd "$tmp"
git init -q -b main
git config user.name guard-test
git config user.email guard-test@example.invalid
echo base > README
git add README && git commit -qm base

# The guard's lists plus one test-only denylist entry, so the denylist cases need no real
# private name in the public tree. Kept outside the test repo so no case commits it.
cp "$here"/guard.py "$here"/denylist.txt "$here"/allowlist.txt "$cfg/"
printf '%s\n' 'example[-_]?private' >> "$cfg/denylist.txt"

fail=0
# case <name> <expected-exit> <file> <content> [commit-message] [env-denylist]
case_() {
  local name=$1 want=$2 file=$3 content=$4 msg=${5:-"add $3"} envdeny=${6:-}
  git checkout -q -B "case/$name" main
  mkdir -p "$(dirname "$file")"
  printf '%s\n' "$content" > "$file"
  git add -A && git commit -qm "$msg"
  local got=0 out
  out=$(PUBLIC_GUARD_DENYLIST="$envdeny" python3 "$here/guard.py" main HEAD --config-dir "$cfg" 2>&1) || got=$?
  if [ "$got" = "$want" ]; then
    echo "ok   $name (exit $got)"
  else
    echo "FAIL $name: want exit $want, got $got"; echo "$out" | sed 's/^/     /'; fail=1
  fi
  if [ -n "$envdeny" ] && grep -qiF -- "$envdeny" <<<"$out"; then
    echo "FAIL $name: a PUBLIC_GUARD_DENYLIST entry was echoed"; fail=1
  fi
}

case_ clean               0 src/a.rs   'let x = "hello";'
case_ placeholder-home    0 docs/a.md  'cd /home/user/dev && ls /Users/you C:\Users\me'
case_ doc-ip              0 docs/b.md  'connect to 192.0.2.10 or 203.0.113.7'
case_ cidr                0 docs/c.md  'tailnets use 100.64.0.0/10; LANs use 10.0.0.0/8'
case_ documented-ts-ip    0 docs/d.md  'MagicDNS at 100.100.100.100, node 100.64.0.1'
case_ allowlisted         0 app/e.dart 'const host = "10.0.2.2";'
case_ home-linux          1 src/b.rs   'let p = "/home/jdoe/dev/thing";'
case_ home-mac            1 src/c.rs   'let p = "/Users/jdoe/Library";'
case_ home-windows        1 src/d.rs   'let p = r"C:\Users\jdoe\AppData";'
case_ rfc1918             1 src/e.rs   'let h = "192.168.7.20:11434";'
case_ cgnat               1 src/f.rs   'let h = "100.71.2.9";'
case_ denylist-word       1 docs/e.md  'see the Example-Private plugin'
case_ denylist-path       1 example_private/notes.md 'notes'
case_ commit-message      1 src/g.rs   'fn g() {}' 'fix: pairing on 10.1.2.3'
case_ env-denylist        1 docs/f.md  'ported from secretproject-sync' 'add f' 'secretproject-sync'
case_ env-denylist-miss   0 docs/g.md  'nothing private here' 'add g' 'secretproject-sync'

# A value added in one commit and removed in the next is still published by a merge commit.
git checkout -q -B case/add-then-remove main
echo 'host = "10.9.8.7"' > h.txt && git add -A && git commit -qm 'add h'
echo 'host = "192.0.2.1"' > h.txt && git add -A && git commit -qm 'scrub h'
got=0; python3 "$here/guard.py" main HEAD --config-dir "$cfg" >/dev/null 2>&1 || got=$?
if [ "$got" = 1 ]; then echo "ok   add-then-remove (exit 1)"; else echo "FAIL add-then-remove: got $got"; fail=1; fi

[ "$fail" = 0 ] && echo "public-guard self-test: all cases passed"
exit "$fail"
