# macOS packaging — Hyperpanes.app + dmg

`bundle.sh <version>` (version without a leading `v`) builds `rs/crates/app`
in release mode, assembles `Hyperpanes.app`, and emits
`rs/packaging/out/hyperpanes-<version>.dmg` containing the app plus an
`/Applications` symlink. It runs from any cwd and works both on the Mac mini
and on a GitHub `macos-latest` (arm64) runner — only stock macOS tools are
used (`cargo`, `sips`, `iconutil`, `hdiutil`, `plutil`).

## Bundle layout

```
Hyperpanes.app/
  Contents/
    Info.plist                      # com.hyperpanes.app, .hyperpanes doc type + exported UTI
    MacOS/
      hyperpanes                    # the release binary
      resources/shell-integration/  # hp-init.ps1 / hp-init.sh — where the app's
                                    # exe-relative lookup finds them TODAY
    Resources/
      hyperpanes.icns               # generated from build/icon.png via sips + iconutil
      shell-integration/            # duplicate copy at the idiomatic bundle location,
                                    # for a future bundle-aware lookup in core
```

The shell-integration scripts ship twice on purpose:
`core::shell_integration::shell_integration_dir()` resolves
`exe_dir/resources/shell-integration` relative to the running binary, and in a
bundle `exe_dir` is `Contents/MacOS/` — that copy is the one in use. The
`Contents/Resources/shell-integration` copy is inert until core also probes
`exe_dir/../Resources/shell-integration` (a one-line candidate addition);
packaging will not need to change when it does.

## Installing an unsigned build (Gatekeeper)

The dmg is **unsigned and not notarized**, so macOS quarantines it on
download and a plain double-click shows "Hyperpanes is damaged" or
"cannot be opened because the developer cannot be verified".

Either of these gets past it:

- **Right-click → Open**: after copying `Hyperpanes.app` to `/Applications`,
  right-click (or Ctrl-click) the app → **Open** → **Open** in the dialog.
  Only needed once; afterwards it launches normally.
- **Strip the quarantine attribute** (Terminal):

  ```sh
  xattr -dr com.apple.quarantine /Applications/Hyperpanes.app
  ```

On newer macOS the first launch may instead be blocked outright with no Open
override; then use System Settings → Privacy & Security → "Open Anyway", or
the `xattr` command above.

## `.hyperpanes` file association

`Info.plist` declares the `com.hyperpanes.workspace` exported UTI (extension
`.hyperpanes`, conforms to `public.json`) and registers the app as its Owner
editor. LaunchServices picks the declaration up when the app is first copied
into `/Applications` (or launched). Double-clicking a `.hyperpanes` file then
opens it in the app — macOS passes the path as `argv[1]`, which flows through
the CLI's positional-path capture, same as the Windows `"%1"` association.

To verify a registration:

```sh
mdls -name kMDItemContentType some.hyperpanes
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -dump | grep -i hyperpanes
```

## Versioning

`CFBundleShortVersionString` carries the full `<version>` string;
`CFBundleVersion` gets the numeric prefix only (`0.1.0-test` → `0.1.0`)
because Apple requires period-separated numbers there.

The icon source is `build/icon.png` (512×512 — the same art as the Windows
`icon.ico`), so the iconset tops out at 512 px and omits the `512@2x` (1024)
slot; `iconutil` accepts that.
