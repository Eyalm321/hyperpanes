# hyperpanes mobile

iOS/Android client for a host hyperpanes: streams pane output, drives Claude/agent
panes, and manages the workspace over the control API. Everything heavy (ptys, agents,
repos) stays on the host — the phone is a remote head.

Architecture, protocol, and host-side requirements: `docs/mobile-client-plan.md`
(repo root).

## Pair with a host

1. On the host, enable the control API (Preferences → Control API) and add a bind
   address to `control-settings.json` (userData dir):

   ```json
   { "enabled": true, "allowInput": true, "bindAddress": "<tailscale-ip>", "port": 51888 }
   ```

   Prefer the Tailscale IP — WireGuard-encrypted, no open LAN ports.
2. Run `hyperpanes pair` → mints a **per-device token** (the master token never leaves the
   host) and prints `hp://…` URLs + a QR code. Name the device / set an expiry with
   `hyperpanes pair --device "my-iphone" --ttl 30d` (TTL omitted = never expires).
3. In the app: **Scan pairing QR** (or enter host:port + token manually).

Each device is paired individually and revocably: `hyperpanes devices` lists the paired
clients by label, and `hyperpanes revoke "my-iphone"` drops one without disturbing the others.
Device tokens are persisted (`device-tokens.json`), so a phone stays paired across host restarts.

## Develop

```sh
flutter pub get
flutter analyze
flutter test          # protocol, splice-dedupe, model parsing
flutter run           # attached device/emulator
```

## Build

- **Android** (Linux/mac/Windows): `flutter build apk --release`
  → `build/app/outputs/flutter-apk/app-release.apk` (sideload or `adb install`).
- **iOS** (mac only): `flutter build ipa` — needs Xcode + a signing team. For personal
  use, open `ios/Runner.xcworkspace` and run to a device from Xcode.

## Layers (lib/src/)

| Dir | What |
|---|---|
| `api/` | pairing URL parse, `/state` models, `/events` frames, HTTP+WS client (auto-reconnect) |
| `term/` | `PaneSession` — xterm emulator seeded from `GET /output` and spliced onto WS `output` frames by byte cursor; quick-keys map |
| `state/` | `HostSession` (state tree + liveness, output-free), saved hosts (tokens in the platform keystore) |
| `ui/` | connect / dashboard / terminal / composer screens, theme |

Perf invariants (keep these):
- The dashboard never consumes `output` frames — liveness/state only.
- Only the OPEN pane holds an emulator; on open it re-seeds from the host's replay
  buffer instead of the app buffering every pane in the background.
- The emulator is locked to the HOST grid (`autoResize: false` + host cols/rows) —
  a pty stream only renders correctly at the width it was produced for.
