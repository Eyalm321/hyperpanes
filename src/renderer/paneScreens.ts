// On-demand screen serializers, keyed by paneId. Each mounted Terminal registers
// a closure that renders ITS xterm buffer to clean text (see screen.ts); the
// control bridge (renderer/control.ts) calls it to serve
// `read_pane({ mode: "screen" })`.
//
// Kept in its own tiny module so the control layer can reach a pane's live screen
// WITHOUT importing the xterm-heavy Terminal component (which would drag
// '@xterm/xterm' + its CSS into control.ts and its unit tests). Mirrors how
// `paneTerminals` exposes terminal handles, but with no heavy dependency.
export const paneScreens = new Map<string, () => string>();
