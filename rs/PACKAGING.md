# Packaging — native Rust `hyperpanes`

Produces a **per-user Windows NSIS installer** for the native Rust app
(`rs/crates/app`, binary `hyperpanes`). It is the Rust equivalent of the Electron
`electron-builder` NSIS setup (`electron-builder.yml` + `build/installer.nsh`).

## Build it locally

```powershell
# From the repo root. Builds release, embeds the icon, runs makensis.
pwsh rs/packaging/build-installer.ps1
```

Output: `rs/packaging/dist/Hyperpanes-<version>-setup.exe`

Useful flags:

```powershell
pwsh rs/packaging/build-installer.ps1 -SkipBuild              # reuse an existing release build
pwsh rs/packaging/build-installer.ps1 -Version 0.1.0          # override the version stamp
```

### Prerequisites

- **Rust** (stable) — `cargo build --release` for the app crate.
- **NSIS** (`makensis`) — install with `choco install nsis -y`. The script also
  finds it under `C:\Program Files (x86)\NSIS\` if it's not on `PATH`.
- **rcedit** — *optional*. Used only to embed the icon/version into the bare
  `.exe`. If missing, the script downloads it to `rs/packaging/.tools/`; if that
  fails too, the installer still builds (the icon is wired into the shortcuts and
  Add/Remove Programs regardless — see note below).

## What the installer does (parity with the Electron build)

| electron-builder.yml / installer.nsh | This installer (`installer.nsi`) |
| --- | --- |
| `oneClick: false` | Assisted MUI2 installer (Welcome → Directory → Install → Finish) |
| `perMachine: false` | Per-user, **no elevation** — installs to `%LOCALAPPDATA%\Programs\Hyperpanes`, registry under `HKCU` |
| `allowToChangeInstallationDirectory: true` | Directory page |
| `artifactName: Hyperpanes-<version>-setup.exe` | `Hyperpanes-<version>-setup.exe` |
| `installer.nsh` PATH add/remove | `AddToUserPath` / `RemoveFromUserPath` — **verbatim port** of the fail-safe PowerShell `[Environment]::SetEnvironmentVariable(..., 'User')` approach |
| app icon | `build/icon.ico` → installer/uninstaller UI, Start-Menu + Desktop shortcuts, and Add/Remove Programs |

After install, `hyperpanes` resolves from any **new** terminal (PATH is updated for
the current user; already-open shells won't see it until restarted).

## Files

- `rs/packaging/installer.nsi` — the NSIS script (per-user, MUI2, PATH integration).
- `rs/packaging/build-installer.ps1` — build → icon-embed → `makensis` driver.
- `.github/workflows/release-rust.yml` — CI: builds + publishes on a `rust-v*` tag.

## Releasing via CI

Push a `v*` tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The workflow builds the installer on `windows-latest` and attaches
`Hyperpanes-0.1.0-setup.exe` to the GitHub Release. It can also be run manually
via **workflow_dispatch** with an explicit version.

> **Note:** the legacy Electron `release.yml` also triggers on `v*` and produces
> an identically-named `Hyperpanes-<ver>-setup.exe`. Retire it (archive the
> Electron `main`) before tagging, otherwise the two releases collide.

## Known follow-up (needs an app-crate change — out of packaging scope)

The bare `hyperpanes.exe`'s **own** embedded icon (shown in Explorer / when the
binary is launched directly) is set best-effort via `rcedit` at package time. The
"proper" fix is a `build.rs`/`winres` icon resource in `rs/crates/app`, which is
app-source territory and intentionally **not** changed here. Shortcuts and
Add/Remove Programs already show the icon via `icon.ico` either way.
