#!/usr/bin/env python3
"""Public-hygiene guard: fail when a change ADDS private material to this public repo.

Usage: guard.py <base> <head> [--config-dir DIR]

Scans everything the commits in base..head add, commit by commit (merges skipped):
  * added lines,
  * added/renamed file paths,
  * commit messages.

It flags:
  * home paths      /home/<user>, /Users/<user>, C:\\Users\\<user>  (placeholder users allowed)
  * private IPs     RFC 1918 (10/8, 172.16/12, 192.168/16) and CGNAT 100.64/10
  * denylist        each regex in <config-dir>/denylist.txt, plus each line of the
                    PUBLIC_GUARD_DENYLIST env var (a repository variable, so private names
                    never have to be committed to the public tree)

Exceptions: exact strings in <config-dir>/allowlist.txt. For examples in docs and tests,
use the documentation ranges (192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24) or a
placeholder user (/home/user, /Users/you, C:\\Users\\me). Those pass without an entry.

Exit 0 = clean, 1 = findings, 2 = usage/git error. Findings print as GitHub annotations.
Matched values are printed for the built-in classes (the PR already shows them); entries
from PUBLIC_GUARD_DENYLIST are never echoed, only "private denylist entry".
"""
import ipaddress
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))

# Placeholder user names that are clearly not a real account. Keep this list generic.
PLACEHOLDER_USERS = {
    "user", "username", "you", "me", "u", "a", "x", "name", "someone", "example", "test",
    "runner", "dev", "alice", "bob", "admin", "my", "foo", "bar", "ubuntu", "linuxbrew",
    "shared", "public", "default", "guest", "root", "library",
}
HOME_RE = re.compile(
    r"(?P<prefix>/home/|/Users/|[A-Za-z]:(?:\\\\|\\|/)Users(?:\\\\|\\|/))(?P<user>[A-Za-z][A-Za-z0-9._-]*)"
)
IPV4_RE = re.compile(r"(?<![\w.])(\d{1,3}(?:\.\d{1,3}){3})(?!\d)(?!\.\d)")
PRIVATE_NETS = [ipaddress.ip_network(n) for n in
                ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10")]
# Documented examples (Tailscale's well-known addresses) that may appear in docs.
BUILTIN_ALLOWED_IPS = {"100.64.0.1", "100.100.100.100"}
# The guard's own config holds the patterns it hunts for; never scan it.
SELF_PREFIX = ".github/public-guard/"


def git(*args):
    r = subprocess.run(["git", *args], capture_output=True, text=True, errors="replace")
    if r.returncode != 0:
        sys.stderr.write(f"git {' '.join(args)} failed: {r.stderr.strip()}\n")
        sys.exit(2)
    return r.stdout


def read_list(path):
    out = []
    try:
        with open(path, encoding="utf-8") as fh:
            for raw in fh:
                line = raw.rstrip("\n")
                if line.strip() and not line.lstrip().startswith("#"):
                    out.append(line.strip())
    except FileNotFoundError:
        pass
    return out


def compile_denylist(entries, source):
    pats = []
    for e in entries:
        try:
            pats.append((re.compile(e, re.IGNORECASE), source, e))
        except re.error as err:
            sys.stderr.write(f"public-guard: bad regex in {source}: {err}\n")
            sys.exit(2)
    return pats


def scan_text(text, allow, deny):
    """Yield (class, shown_value) for each problem in one line of text."""
    for m in HOME_RE.finditer(text):
        user = m.group("user").rstrip(".-")
        full = m.group("prefix") + user
        if user.lower() in PLACEHOLDER_USERS or full in allow:
            continue
        yield "home path", full
    for m in IPV4_RE.finditer(text):
        s = m.group(1)
        try:
            ip = ipaddress.ip_address(s)
        except ValueError:
            continue
        if s in BUILTIN_ALLOWED_IPS or s in allow:
            continue
        if re.match(r"/\d{1,2}\b", text[m.end():]):
            continue  # a CIDR range ("10.0.0.0/8") names a network, not a host
        if any(ip in n for n in PRIVATE_NETS):
            yield "private IP", s
    for pat, source, raw in deny:
        m = pat.search(text)
        if m and m.group(0) not in allow:
            shown = m.group(0) if source == "denylist.txt" else "(private denylist entry)"
            yield "denylist", shown


def annotate(path, line, cls, value):
    loc = f"file={path},line={line}" if path else ""
    print(f"::error {loc}::public-guard: {cls} {value!s}".replace("\n", " "))


def main(argv):
    args = [a for a in argv if not a.startswith("--")]
    cfg = HERE
    if "--config-dir" in argv:
        cfg = argv[argv.index("--config-dir") + 1]
        args.remove(cfg)
    if len(args) != 2:
        sys.stderr.write(__doc__)
        return 2
    base, head = args
    allow = set(read_list(os.path.join(cfg, "allowlist.txt")))
    deny = compile_denylist(read_list(os.path.join(cfg, "denylist.txt")), "denylist.txt")
    env_deny = [l.strip() for l in os.environ.get("PUBLIC_GUARD_DENYLIST", "").splitlines()
                if l.strip() and not l.strip().startswith("#")]
    deny += compile_denylist(env_deny, "PUBLIC_GUARD_DENYLIST")
    if not env_deny:
        print("::notice::public-guard: PUBLIC_GUARD_DENYLIST is empty; only the in-repo denylist applies")

    head_sha = git("rev-parse", "--verify", head + "^{commit}").strip()
    git("rev-parse", "--verify", base + "^{commit}")
    findings = 0

    # Every non-merge commit in base..head, one by one: a merge commit keeps the branch's
    # history, so a value added in one commit and deleted in the next is still published.
    # Merges themselves are skipped (merging main into the branch adds nothing new).
    log = git("log", "--no-merges", "--no-color", "--no-ext-diff", "-p", "-U0", "-M",
              "--format=%x00COMMIT %H%n%B%x00END", f"{base}..{head_sha}")
    sha, path, lineno, in_msg = None, None, 0, False
    for raw in log.splitlines():
        if raw.startswith("\x00COMMIT "):
            sha, path, in_msg = raw[8:].strip(), None, True
            continue
        if in_msg:
            if raw.startswith("\x00END"):
                in_msg = False
                continue
            for cls, val in scan_text(raw, allow, deny):
                print(f"::error::public-guard: {cls} in the message of commit {sha[:10]}: {val}")
                findings += 1
            continue
        if raw.startswith("+++ "):
            path = raw[6:] if raw.startswith("+++ b/") else None
            if path and not path.startswith(SELF_PREFIX):
                for cls, val in scan_text(path, allow, deny):
                    annotate(path, 1, f"{cls} in file path (commit {sha[:10]}):", val)
                    findings += 1
            continue
        if raw.startswith("@@"):
            m = re.search(r"\+(\d+)", raw)
            lineno = int(m.group(1)) if m else 0
            continue
        if raw.startswith("+") and path and not path.startswith(SELF_PREFIX):
            for cls, val in scan_text(raw[1:], allow, deny):
                annotate(path, lineno, f"{cls} (commit {sha[:10]}):", val)
                findings += 1
            lineno += 1

    if findings:
        print(f"public-guard: {findings} finding(s). This repo is public: remove the private "
              "material, use a placeholder (/home/user, 192.0.2.10), or, for a deliberate public "
              "value, add the exact string to .github/public-guard/allowlist.txt.")
        return 1
    print(f"public-guard: clean ({base}..{head_sha[:10]})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
