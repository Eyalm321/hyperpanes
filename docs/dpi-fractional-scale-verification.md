# DPI sharpness at fractional scales — verification note (#11)

**Date:** 2026-06-10 · **Build:** release `hyperpanes.exe` (branch `fanout/wave3-tests-polish`,
includes the be55b25 1:1 terminal-surface fix) · **Verdict: already sharp — no fix needed.**

## What was checked

The be55b25 fix pinned the *terminal surface* to 1:1 device pixels (no `image-fit: fill`
sub-pixel stretch). This pass verified the **whole window** — chrome (tab strip, pane
header, buttons), sidebar rail glyphs, and terminal text — at fractional scale factors.

Method: launched the release build in isolation (temp `APPDATA`, control-file env cleared)
with `SLINT_SCALE_FACTOR` = `1.0` / `1.25` / `1.5`, captured each window by PID, and
inspected 4× nearest-neighbor magnifications of the prompt text and the tab label.

## Findings

* **The Slint/winit backend honors `SLINT_SCALE_FACTOR` exactly — no rounding, no
  letterbox.** Window device size scaled 1296×839 → 1616×1039 → 1936×1239 for the same
  logical size; content fills the window at every scale.
* **Terminal text is sharp at 1.25 and 1.5.** Glyphs are *rasterized at* `font_px × scale`
  (the pump reloads fonts on a scale change — `paneview::pump` → `State::reload_font`) and
  the surface maps 1:1 to device pixels, so fractional scales produce larger native glyphs,
  never a resampled stretch. Magnified crops show clean rasterizer anti-aliasing only — no
  doubled/smeared pixel rows, no warp.
* **Chrome is sharp.** Tab labels, the ✕/+ glyphs, and the rounded tab outline are
  femtovg-vector-rendered at the device scale; magnified crops show no blur beyond normal
  AA at 1.25/1.5. Sidebar rail glyphs likewise.

## Notes for future passes

* `SLINT_SCALE_FACTOR` is the cheap way to A/B fractional DPI without touching Windows
  display settings; the capture harness lives at `%TEMP%\hp-dpi\shot.ps1` (temp-APPDATA
  isolation + by-PID window capture, per the screenshot-harness playbook).
* A one-pixel noise band can appear at the captured window's very top edge at any scale —
  a BitBlt/frameless-edge capture artifact, not app rendering (verify against the live
  window before chasing it).
