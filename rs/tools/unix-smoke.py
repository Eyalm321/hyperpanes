#!/usr/bin/env python3
"""Unix GUI-smoke driver (Wave-2 INTEGRATION track) — control-API-driven checks
against a running ISOLATED instance:

  1. zsh pane spawns with the bundled-ZDOTDIR integration and reports OSC-7 cwd
  2. vim opens + renders in a pane (proves no DSR interception) and quits clean
  3. (optional, app binary path given) .hyperpanes file open via a SECOND
     instance hands off to the primary (single-instance socket) — the file's
     tab appears in the primary's /state

Usage: python3 unix-smoke.py <control.json> [app-binary]
"""
import json
import subprocess
import sys
import time
import urllib.request

PASS, FAIL = "PASS", "FAIL"
results = []


def check(ok, name, detail=""):
    results.append((ok, name, detail))
    print(f"{PASS if ok else FAIL}  {name}{('  — ' + detail) if detail else ''}")


def http(method, url, token=None, body=None):
    req = urllib.request.Request(url, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, data=data, timeout=15) as r:
        return json.loads(r.read().decode() or "null")


def find_pane(state, pane_id):
    for w in state["windows"]:
        for t in w["tabs"]:
            for p in t["panes"]:
                if p["id"] == pane_id:
                    return p
    return None


def main():
    control = json.load(open(sys.argv[1], encoding="utf-8"))
    app_bin = sys.argv[2] if len(sys.argv) > 2 else None
    base = f"http://127.0.0.1:{control['port']}"
    token = control["token"]

    state = http("GET", f"{base}/state", token)
    window_id = state["windows"][0]["windowId"]

    # ---- 1. zsh pane + OSC-7 cwd (the B1/B2 ZDOTDIR wiring, live) ----
    res = http("POST", f"{base}/command", token,
               {"type": "newPane", "windowId": window_id,
                "pane": {"shell": "zsh", "label": "zsh-smoke"}})
    zsh_id = res.get("result")
    check(isinstance(zsh_id, str), "newPane(shell=zsh)", f"paneId={zsh_id}")
    cwd = None
    deadline = time.time() + 20
    while time.time() < deadline and not cwd:
        time.sleep(1)
        p = find_pane(http("GET", f"{base}/state", token), zsh_id)
        cwd = (p or {}).get("cwd")
    check(bool(cwd), "zsh OSC-7 cwd reported (bundled ZDOTDIR chain)", f"cwd={cwd}")

    # ---- 2. vim renders + quits (no DSR interception) ----
    http("POST", f"{base}/panes/{zsh_id}/input", token, {"data": "vim", "submit": True})
    time.sleep(3)
    out = http("GET", f"{base}/panes/{zsh_id}/output?mode=screen", token)
    screen = out.get("data") or out.get("text") or ""
    vim_up = "VIM" in screen or screen.count("~") >= 5
    check(vim_up, "vim opened + rendered (alt screen)",
          f"screen has {screen.count('~')} tildes")
    http("POST", f"{base}/panes/{zsh_id}/input", token, {"data": ":q!", "submit": True})
    time.sleep(2)
    out2 = http("GET", f"{base}/panes/{zsh_id}/output?mode=screen", token)
    screen2 = out2.get("data") or out2.get("text") or ""
    check(screen2.count("~") < 5, "vim quit back to shell", "")

    # ---- 3. .hyperpanes open via second-instance handoff ----
    if app_bin:
        ws_file = "/tmp/hp-smoke-open.hyperpanes"
        payload = {
            "format": "hyperpanes",
            "version": 1,
            "workspace": {
                "name": "smoke-open",
                "groups": [{"title": "smoke-open",
                            "panes": [{"label": "smoked"}]}],
            },
        }
        json.dump(payload, open(ws_file, "w"))
        before = sum(len(w["tabs"]) for w in http("GET", f"{base}/state", token)["windows"])
        proc = subprocess.run([app_bin, ws_file], capture_output=True, timeout=30)
        time.sleep(4)
        after_state = http("GET", f"{base}/state", token)
        after = sum(len(w["tabs"]) for w in after_state["windows"])
        titles = [t["title"] for w in after_state["windows"] for t in w["tabs"]]
        check(proc.returncode == 0, "second instance exits 0 (handoff, no second window)",
              f"rc={proc.returncode}")
        check(after == before + 1 and "smoke-open" in titles,
              ".hyperpanes argv landed in the PRIMARY instance",
              f"tabs {before}->{after} titles={titles}")

    bad = [r for r in results if not r[0]]
    print(f"\n{len(results) - len(bad)}/{len(results)} passed")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
