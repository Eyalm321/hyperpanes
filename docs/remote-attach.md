# Remote attach: use another machine's panes from the desktop app

Remote attach lets one Hyperpanes desktop app (the **client**) show and drive the windows and tabs
of another Hyperpanes instance (the **host**) as ordinary tabs. It reuses the same control API and
pairing flow as the mobile app (see [`mobile-client-plan.md`](mobile-client-plan.md)): the client
connects to the host's control server with a revocable *device token*, reads state over HTTP,
streams live output over `WS /events`, and sends keystrokes to `POST /panes/{id}/input`.

The host needs no new features: any v0.0.34 host that can pair a phone can be attached from a
desktop.

> **Read the [Security](#security) section before exposing a host.** A pairing URL is a full-authority
> password for every pane on the host, and the transport is unencrypted. Use a VPN.

## Host setup

### 1. Bind the control server to your VPN address

By default the control server listens on `127.0.0.1` only. To accept a remote client, set
`bindAddress` (and a fixed `port`, so your pairing URL stays valid across restarts) in
`control-settings.json` in the host's config directory:

| OS | Path |
|---|---|
| Linux | `$XDG_CONFIG_HOME/hyperpanes/control-settings.json` (default `~/.config/hyperpanes/control-settings.json`) |
| macOS | `~/Library/Application Support/hyperpanes/control-settings.json` |
| Windows | `%APPDATA%\hyperpanes\control-settings.json` |

```json
{
  "enabled": true,
  "allowInput": true,
  "bindAddress": "100.x.y.z",
  "port": 41419
}
```

- `bindAddress`: the IP address of the host's **VPN interface** (WireGuard-style; shown here as the
  placeholder `100.x.y.z`). Do not use `0.0.0.0` unless you firewall the port yourself.
- `port`: any free port, 1 to 65535.
- `allowInput`: must be `true` for the client to type. With `false` the client is read-only.
- `enabled`: the control API must be on (Preferences → Control API).

Restart the host app, or toggle Preferences → Control API off and on.

> **Silent fallback.** The server only binds a non-loopback address when `bindAddress` is an IP that
> is **live on a local interface right now**. If the VPN is down at startup, or the address is a
> typo, the server silently falls back to `127.0.0.1` and remote clients see "connection refused".
> Bring the VPN up first, then restart the host app. `hyperpanes pair` prints a warning when no
> `bindAddress` is configured.

### 2. Mint a pairing URL

On the host, run:

```sh
hyperpanes pair --device my-laptop            # never expires
hyperpanes pair --device my-laptop --ttl 12h  # self-revokes after 12 hours (also 30d, 90m, or ms)
```

`--device` names the credential (it defaults to the machine hostname). The command prints one or
more URLs of the form:

```
hp://100.x.y.z:41419/?token=<TOKEN>&v=1
```

and a QR code (for the phone app; ignore it for desktop). Treat the whole URL as a secret.

### 3. Revoke

```sh
hyperpanes devices              # list paired devices (label + expiry; tokens are never shown)
hyperpanes revoke my-laptop     # revoke every device token carrying that label
```

Revocation is immediate. A connected client gets an auth error and stops retrying.

## Client usage

1. Run **Connect to host…** from the command palette or the menu.
2. Paste the pairing URL. Accepted forms: `hp://host:port/?token=T&v=1`, the same with a `#T`
   fragment, an `http://` URL, or a bare `host:port` plus a separate token.
3. The host's windows and tabs appear as **remote tabs**, titled `⇄ <label> · <tab>` (the `⇄`
   prefix is the remote marker) and the first one is activated. The dialog's **Remember this host**
   (default on) saves the host to `remote-hosts.json`; **Connect automatically at startup**
   (default off) opts it into autoconnect.

Command palette entries: **Connect to host…**, **Connect to `<label>`** (one per saved host),
**Reconnect remote host `<label>`**, **Disconnect remote host `<label>`** and **Forget saved host
`<label>`**. The tab context menu of a remote tab also offers *Reconnect Remote Host* and
*Disconnect Remote Host*. Disconnect drops the session and closes its remote tabs; nothing on the
host is touched.

### Sizing

A remote pane renders in the local terminal widget at the **host pane's** columns × rows. The
client never resizes the host's pty. If your local pane is larger the content is padded; if smaller
it is clipped. There is no font auto-fit in v1. To see more, enlarge the pane on the host.

### Closing is detaching

Closing a remote tab or pane only **detaches** it locally. The host pane keeps running; nothing on
the host is killed.

### Disconnects and reconnect

Laptop sleep/wake, a Wi-Fi change, a VPN drop or a host restart put the remote tabs into a visible
**disconnected** state. The client reconnects automatically with capped exponential backoff and
resumes the output stream from its cursor. You can also choose **Reconnect** explicitly. Remote
tabs are never closed automatically.

A revoked, expired or wrong token shows an **auth error** and stops retrying. Mint a new URL and
reconnect.

If the host has input disabled, typing shows a "host disallows input" toast.

### Saved hosts

Connected hosts can be saved. They live in `remote-hosts.json` in the client's config directory
(same directory table as above) with fields `label`, `host`, `port`, `token`. On Linux and macOS the
file has mode `0600`; on Windows it sits in the per-user profile directory.

- **Forget host** removes the entry (and its token) from the file.
- **Autoconnect at startup** is opt-in per host and **off by default**. An unreachable saved host
  never blocks startup.

## What works, and what does not (v1)

Works:

- Viewing all of the host's windows and tabs as remote tabs, with live output and resume after reconnect.
- Typing and pasting into remote panes (when the host has `allowInput` on).
- Auto-reconnect, manual reconnect, saved hosts, opt-in autoconnect.

Does not (yet):

- Resizing the host pty from the client, or auto-fitting fonts to the host size.
- Creating or closing panes, tabs or windows **on the host** from the client. Close only detaches.
- TLS. The connection is plain HTTP/WS (see Security).
- Per-device scopes: every device token has full authority.

## Troubleshooting

| Symptom | Likely cause and fix |
|---|---|
| **Connection refused** | The host fell back to loopback because `bindAddress` was not a live local IP at startup (VPN down, typo). Bring the VPN up and restart the host app. Also check the port matches `port` in `control-settings.json`, and that no firewall blocks it. On the host, `ss -ltn` should show the VPN address and port, not `127.0.0.1`. |
| **401 / auth error** | The token is bad, was revoked (`hyperpanes revoke`), or expired (`--ttl`). Mint a new pairing URL. The client stops retrying on purpose. |
| **403 on typing / "host disallows input"** | `allowInput` is off on the host. Set `"allowInput": true` in `control-settings.json` (or toggle Allow input in Preferences) and restart or toggle the control API. |
| **Stuck "disconnected"** | The VPN is down or the client's network changed. Bring the VPN back up. The client retries with capped backoff; use **Reconnect** to retry now. If the host app restarted with a different address, update the saved host. |
| **Connects but no tabs** | The host has no windows open, or the token's host is a different instance than you intended. Check `GET /state` on the host. |

## Security

- **Device tokens are full-authority bearer secrets.** They are not scoped. Anyone holding a pairing
  URL can read every pane on the host and, with `allowInput` on, **type into your panes**, which
  means running commands as your user. Never paste a URL into chat, tickets, or screenshots.
- **The transport is plain HTTP and WS.** Nothing is encrypted at the application layer. Only use
  remote attach over a WireGuard-style VPN (or another encrypted tunnel). Never expose the port on an
  untrusted LAN or the internet, and avoid `bindAddress: "0.0.0.0"` without a firewall.
- Tokens are never shown in tab titles or written to logs by the client. The saved-hosts file is
  mode `0600`, but it is still a plaintext secret on disk: protect your client disk accordingly.
- **Lost or stolen laptop?** Run `hyperpanes revoke <label>` on the host immediately. Prefer
  `--ttl` for short-lived access.
- Turn `allowInput` off on hosts where read-only viewing is enough.

## Local control commands (client side)

For scripting and the smoke test, the client's own control API accepts two extra commands. They
need an unscoped token (the client's master token, or a device token paired *to the client*);
scoped agent tokens get `403`. The pairing URL / token is never echoed back.

```sh
# connect exactly like the dialog (not remembered unless "remember": true)
POST /command {"type":"connectRemote","url":"hp://host:port/?token=T&v=1","label":"x"}
  -> {"ok":true,"result":{"hostId":3}}
# every attached remote pane: grid size, connection/exit state and the rendered screen text
POST /command {"type":"remoteScreen"}
  -> {"ok":true,"result":[{"hostId":3,"hostPaneId":"…","cols":120,"rows":30,
                           "connected":true,"exited":false,"screenText":"…"}]}
```

Remote panes are deliberately **not** part of the client's own `/state` (they are not local
panes), are never saved in the session file, and their screen text is never sent to the local AI.

## Repeatable smoke test

`scripts/smoke-remote-attach.sh <path-to-hyperpanes-binary> [evidence-dir]` boots a fully isolated
host and client under Xvfb, attaches, and verifies output and typed input round-trip. It needs
`Xvfb`, `curl`, `jq`, `xdotool` and ImageMagick (`import`). Evidence (logs, screenshot, redacted
state) is written to the evidence dir.
