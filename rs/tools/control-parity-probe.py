#!/usr/bin/env python3
"""Control-API parity probe (Wave-2 INTEGRATION track).

Run against an ISOLATED hyperpanes instance (temp config dirs!) on each OS and
diff the outputs: it captures the JSON *shapes* (key paths + value types, values
dropped) of control.json discovery, GET /health, GET /state, a POST /command
round-trip (renamePane), POST /panes/{id}/input, and the WS /events hello +
first output event. Values like port/token/pid/paths differ per OS; structure
must not.

Usage: python3 control-parity-probe.py <path-to-control.json> [out.json]
Stdlib only (urllib + a raw-socket WS client) so it runs unchanged on
Windows / Linux / macOS.
"""
import base64
import json
import os
import socket
import struct
import sys
import time
import urllib.request


def shape(value, prefix="$"):
    """Flatten a JSON value into sorted "path: type" lines (arrays by element 0)."""
    out = []
    if isinstance(value, dict):
        if not value:
            out.append(f"{prefix}: object(empty)")
        for k in sorted(value):
            out.extend(shape(value[k], f"{prefix}.{k}"))
    elif isinstance(value, list):
        if not value:
            out.append(f"{prefix}[]: (empty)")
        else:
            out.extend(shape(value[0], f"{prefix}[]"))
    elif isinstance(value, bool):
        out.append(f"{prefix}: bool")
    elif isinstance(value, (int, float)):
        out.append(f"{prefix}: number")
    elif isinstance(value, str):
        out.append(f"{prefix}: string")
    elif value is None:
        out.append(f"{prefix}: null")
    return out


def http(method, url, token=None, body=None):
    req = urllib.request.Request(url, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, data=data, timeout=10) as r:
        return json.loads(r.read().decode() or "null")


class WsClient:
    """Minimal RFC6455 client: handshake + read text frames (no extensions)."""

    def __init__(self, host, port, path):
        self.sock = socket.create_connection((host, port), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(req.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("WS handshake: connection closed")
            resp += chunk
        status = resp.split(b"\r\n", 1)[0].decode()
        if "101" not in status:
            raise RuntimeError(f"WS handshake failed: {status}")
        self.buf = resp.split(b"\r\n\r\n", 1)[1]

    def _read_exact(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("WS: connection closed mid-frame")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def read_text(self, deadline):
        """Next complete text frame before `deadline` (epoch s), or None."""
        while time.time() < deadline:
            self.sock.settimeout(max(0.1, deadline - time.time()))
            try:
                hdr = self._read_exact(2)
            except (socket.timeout, TimeoutError):
                return None
            opcode = hdr[0] & 0x0F
            ln = hdr[1] & 0x7F
            if ln == 126:
                ln = struct.unpack(">H", self._read_exact(2))[0]
            elif ln == 127:
                ln = struct.unpack(">Q", self._read_exact(8))[0]
            payload = self._read_exact(ln)
            if opcode == 1:  # text
                return payload.decode("utf-8", "replace")
            # ping/binary/close → skip (probe only reads)
        return None


def main():
    control_path = sys.argv[1]
    out_path = sys.argv[2] if len(sys.argv) > 2 else "parity-shapes.json"
    result = {"os": sys.platform}

    control = json.load(open(control_path, encoding="utf-8"))
    result["control_json"] = shape(control)
    port, token = control["port"], control["token"]
    base = f"http://127.0.0.1:{port}"

    result["health"] = shape(http("GET", f"{base}/health"))

    state = http("GET", f"{base}/state", token)
    result["state"] = shape(state)

    # First pane of the first tab of the first window — the isolated app's seed pane.
    pane = state["windows"][0]["tabs"][0]["panes"][0]
    pane_id = pane["paneId"] if "paneId" in pane else pane["id"]

    rename = http("POST", f"{base}/command", token,
                  {"type": "renamePane", "paneId": pane_id, "label": "parity-probe"})
    result["command_renamePane"] = shape(rename)

    # WS hello first, then trigger output so the event stream proves end-to-end.
    ws = WsClient("127.0.0.1", port, f"/events?token={token}")
    hello = ws.read_text(time.time() + 5)
    result["ws_hello"] = shape(json.loads(hello)) if hello else ["<no hello>"]

    inp = http("POST", f"{base}/panes/{pane_id}/input", token,
               {"data": "echo parity-probe-marker", "submit": True})
    result["panes_input"] = shape(inp)

    # Collect events until an output event for our pane (or 10s).
    deadline = time.time() + 10
    seen_types, output_shape = [], None
    while time.time() < deadline:
        msg = ws.read_text(deadline)
        if msg is None:
            break
        evt = json.loads(msg)
        t = evt.get("type") or evt.get("event") or "?"
        if t not in seen_types:
            seen_types.append(t)
        if t == "output" and output_shape is None:
            output_shape = shape(evt)
            break
    result["ws_event_types_seen"] = sorted(seen_types)
    result["ws_output_event"] = output_shape or ["<no output event within 10s>"]

    json.dump(result, open(out_path, "w", encoding="utf-8"), indent=2)
    print(f"wrote {out_path}")
    for k in result:
        v = result[k]
        print(f"  {k}: {len(v) if isinstance(v, list) else v}")


if __name__ == "__main__":
    main()
