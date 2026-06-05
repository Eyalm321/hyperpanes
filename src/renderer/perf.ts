// Live WebGL-context accounting (per renderer/window). Each on-screen pane attaches
// one xterm WebGL renderer; Chromium force-loses the oldest once a process holds
// more than ~16, silently dropping that pane to the slower DOM renderer. We now
// attach WebGL only while a pane is actually on screen (see Terminal.tsx), so this
// counter should track the number of *visible* tiles, not the total pane count.
// The "Performance: Dump metrics" command reads it to confirm the cap isn't hit.
let liveWebglContexts = 0;

export function incWebglContexts(): void {
  liveWebglContexts++;
}

export function decWebglContexts(): void {
  liveWebglContexts = Math.max(0, liveWebglContexts - 1);
}

export function getWebglContextCount(): number {
  return liveWebglContexts;
}
