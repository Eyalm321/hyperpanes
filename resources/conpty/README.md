# Bundled ConPTY (OpenConsole) redistributable

`conpty.dll` + `OpenConsole.exe` (x64) from the official
[`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)
NuGet package, version **1.24.260512001** (MIT, © Microsoft Corporation, built from
[microsoft/terminal](https://github.com/microsoft/terminal)).

## Why we ship it

portable-pty's `load_conpty()` prefers a `conpty.dll` found next to the running exe over the
in-box Windows one; that dll spawns its matched `OpenConsole.exe` as the pseudoconsole host.
The in-box conhost re-renders the whole DECSTBM scroll region per scrolled line (child
throughput = ~28 MB/s ÷ rows repainted ≈ 0.4 MB/s) and lacks the ConPTY passthrough refactor.
Measured A/B on the GUI bench (2026-06-09, same binary, 3 runs, only delta = this pair):

| case | in-box | bundled 1.24 |
| --- | ---: | ---: |
| dense | 5.9 MB/s | **98.6 MB/s** |
| scrolling | 7.9 | **65.5** |
| unicode | 8.5 | **58.9** |
| cursor-motion | 14.4 | **33.1** |
| scrolling-region | 0.4 | **17.6** |
| alt-screen | 22.4 | **29.2** |

Full investigation: `docs/conpty-passthrough-investigation.md` (§F).

## Deployment

- **Dev:** `rs/crates/app/build.rs` copies both files next to the built binary.
- **Release:** `rs/packaging/installer.nsi` installs both into `$INSTDIR`.

## Updating

The two files MUST stay a matched pair (same package version — a mismatched dll/host can
desync). To bump: download
`https://www.nuget.org/api/v2/package/Microsoft.Windows.Console.ConPTY/<version>` (a zip),
take `runtimes/win-x64/native/conpty.dll` + `build/native/runtimes/x64/OpenConsole.exe`,
replace both files here, update this README's version, and re-run the bench
(`node bench/run.mjs --only=hyperpanes --suite=throughput`) to confirm no regression.
WezTerm ships the same pair the same way (wezterm#7774).
