# Linux packaging (AppImage)

Built by `rs/packaging/appimage.sh <version>` (contract frozen in
`docs/ports-seams.md` §3):

```
rs/packaging/appimage.sh 0.0.6
→ rs/packaging/out/hyperpanes-0.0.6-x86_64.AppImage
```

The script runs from any cwd, release-builds `rs/crates/app` (a non-member
crate — built via `--manifest-path`), assembles an AppDir, and packs it with a
pinned **appimagetool 13** (AppImageKit release), downloaded into
`${XDG_CACHE_HOME:-~/.cache}/hyperpanes-packaging/` on first use. The tool is
invoked with `--appimage-extract-and-run`, so neither the build host nor CI
needs FUSE.

## AppDir layout

```
AppDir/
├── AppRun                          # sh shim → exec usr/bin/hyperpanes
├── hyperpanes.desktop              # also at usr/share/applications/
├── hyperpanes.png  + .DirIcon      # 512×512
└── usr/
    ├── bin/hyperpanes
    ├── bin/resources/shell-integration/   # hp-init.sh + hp-init.ps1
    ├── share/icons/hicolor/<N>x<N>/apps/hyperpanes.png   # 16…512
    └── share/mime/packages/hyperpanes.xml  # application/x-hyperpanes, *.hyperpanes
```

- **shell-integration** sits next to the binary because the app resolves
  `exe_dir/resources/shell-integration` (same layout `installer.nsi` ships on
  Windows; the ConPTY redistributable pair is Windows-only and is skipped).
  `AppRun` is a shim (not a symlink) so `std::env::current_exe()` resolves to
  `usr/bin/hyperpanes` and that relative lookup works from the squashfs mount.
- **MIME**: `usr/share/mime/packages/hyperpanes.xml` declares
  `application/x-hyperpanes` with a `*.hyperpanes` glob (sub-class of JSON).
  AppImages are not "installed", so registration happens only when an
  integrator (appimaged / AppImageLauncher) or a downstream package runs
  `update-mime-database`; the file is in the standard location for that.
- **Icons**: pre-derived under `rs/packaging/linux/icons/hicolor/` —
  16/32/48/64/128/256/512 px PNGs resized (HighQualityBicubic) from the
  canonical `build/icon.png` (512×512 RGBA, the same asset the Windows
  `icon.ico` derives from). Regenerate from `build/icon.png` if the logo
  changes; they are committed so the build needs no image tooling.

## Runtime dependencies (require, not bundle)

The binary links the Slint/winit/wgpu native stack dynamically. We **require**
these at runtime rather than bundling them — they are present on any stock
desktop distribution, and bundling libGL/Mesa or libX11 into an AppImage is the
classic way to break GPU drivers on the host:

| Library | Package (Debian/Ubuntu) | Notes |
|---|---|---|
| libc / libm / libgcc_s | (base) | glibc-based distros; built on the CI baseline (ubuntu-latest), so older glibc than the builder's is not supported |
| libfontconfig | `libfontconfig1` | font discovery |
| libfreetype (via fontconfig) | `libfreetype6` | glyph rasterization |
| libxkbcommon | `libxkbcommon0` | keymaps (X11 **and** Wayland) |
| libxkbcommon-x11 | `libxkbcommon-x11-0` | **needed at runtime on the X11/XWayland path**; absent on some minimal/Wayland-only installs — install it if launch fails with an xkbcommon-x11 load error |
| libX11 + xcb stack | `libx11-6`, `libxcb1` | X11 backend (winit loads at runtime) |
| libwayland-client | `libwayland-client0` | Wayland backend (runtime-loaded) |
| Mesa / GPU drivers | `libgl1`, `libegl1` | wgpu picks Vulkan/GL at runtime; never bundle |

One-liner for a minimal headless-ish Ubuntu/WSL:

```
sudo apt-get install -y libfontconfig1 libxkbcommon0 libxkbcommon-x11-0 libgl1 libegl1
```

Build-time (CI) additionally needs the dev headers:
`libfontconfig1-dev libxkbcommon-dev` (plus the standard Rust toolchain;
Slint's femtovg/wgpu renderers need no further -dev packages).

## File ownership (Wave-1 track T5)

This directory + `rs/packaging/appimage.sh` belong to the packaging-linux
track. `installer.nsi` / `build-installer.ps1` (Windows) and the workflows are
owned elsewhere — keep them untouched from here.
