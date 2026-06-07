import { useWorkspace, type Group } from './store/useWorkspace';
import { useIdle } from './store/useIdle';
import { paneScreens } from './paneScreens';
import type { ControlCommand, ControlWindowPayload, GroupSpec, Layout, PaneSpec } from './types';

// Bridge between this window's store and the main-process control API (M2).
// `buildControlPayload` snapshots the window's structure for `GET /state`;
// `applyControlCommand` enacts a forwarded `POST /command` against the store.
// Both are pure of IPC so they can be unit-tested; App wires them to window.hp.

export function buildControlPayload(groups: Group[], activeId: string): ControlWindowPayload {
  // Idle is tracked in a separate store (output quiescence, keyed by paneId).
  // An exited pane is reported 'exited' regardless; otherwise idle→'idle',
  // else 'busy'.
  //
  // HEURISTIC LIMITATION (agent-orchestration B, #15): a pane that has never
  // emitted output reads 'busy' — the 'busy' bucket conflates three states a
  // consumer might want to tell apart: never-started, actively-working, and
  // idle-but-still-streaming. Liveness only becomes reliable once a pane has
  // produced output and then gone quiet (useIdle's quiescence timer fires). An
  // orchestrator should treat 'busy' as "not known-idle", not "definitely working".
  const idle = useIdle.getState().idle;
  return {
    activeTabId: activeId,
    tabs: groups.map((g) => ({
      id: g.id,
      title: g.title,
      layout: g.layout,
      panes: g.panes.map((p) => ({
        id: p.id,
        sessionUid: p.sessionUid,
        label: p.label,
        // Surface subtitle so /state can read back what rename_pane set (it
        // applies live + reports it, but was previously absent from the payload).
        ...(p.subtitle ? { subtitle: p.subtitle } : {}),
        color: p.color,
        command: p.command,
        ...(p.args && p.args.length ? { args: p.args } : {}),
        cwd: p.cwd,
        shell: p.shell,
        status: p.status,
        exitCode: p.exitCode,
        activity: p.status === 'exited' ? 'exited' : idle[p.id] ? 'idle' : 'busy',
        ...(p.meta ? { meta: p.meta } : {})
      }))
    }))
  };
}

const str = (v: unknown): string | undefined => (typeof v === 'string' ? v : undefined);

// Coerce an untrusted value (from an agent over /command) into a non-empty
// string[], dropping non-string entries; undefined for a non-array / empty result.
// Used for a pane's direct-spawn argv (P4a) — same defensive posture as metaRecord.
const strArray = (v: unknown): string[] | undefined => {
  if (!Array.isArray(v)) return undefined;
  const out = v.filter((x): x is string => typeof x === 'string');
  return out.length ? out : undefined;
};

// Coerce an untrusted value (from an agent over /command) into a string→string
// map, dropping non-string values. Returns undefined for a non-object / empty
// result. Used for spawn-time meta/env, where a key has no prior value to clear.
const metaRecord = (v: unknown): Record<string, string> | undefined => {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return undefined;
  const out: Record<string, string> = {};
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    if (typeof val === 'string') out[k] = val;
  }
  return Object.keys(out).length ? out : undefined;
};

// Coerce an untrusted setMeta payload into a merge PATCH: a string value SETS a
// key, an explicit `null` DELETES it (so a stale key — e.g. `task` — can be
// cleared, #6), and any other non-string value is dropped. Returns undefined for
// a non-object / empty result (a no-op patch).
const metaPatch = (v: unknown): Record<string, string | null> | undefined => {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return undefined;
  const out: Record<string, string | null> = {};
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    if (typeof val === 'string') out[k] = val;
    else if (val === null) out[k] = null;
  }
  return Object.keys(out).length ? out : undefined;
};

const num = (v: unknown): number | undefined =>
  typeof v === 'number' && Number.isFinite(v) ? v : undefined;
const bool = (v: unknown): boolean | undefined => (typeof v === 'boolean' ? v : undefined);

// Coerce an untrusted pane spec (from the launch attach path / an agent over
// /command) into a PaneSpec, dropping fields of the wrong type. Same defensive
// posture as metaRecord — groupFromSpec then re-validates layout/sizes downstream.
function coercePaneSpec(v: unknown): PaneSpec {
  const o = (v && typeof v === 'object' ? v : {}) as Record<string, unknown>;
  const spec: PaneSpec = {};
  const set = <K extends keyof PaneSpec>(k: K, val: PaneSpec[K] | undefined) => {
    if (val !== undefined) spec[k] = val;
  };
  set('label', str(o.label));
  set('subtitle', str(o.subtitle));
  set('color', str(o.color));
  set('command', str(o.command));
  set('args', strArray(o.args));
  set('cwd', str(o.cwd));
  set('shell', str(o.shell));
  set('fontSize', num(o.fontSize));
  set('meta', metaRecord(o.meta));
  set('showFrame', bool(o.showFrame));
  set('showDot', bool(o.showDot));
  return spec;
}

// Coerce an untrusted group spec into a GroupSpec, or null if it has no panes.
function coerceGroupSpec(v: unknown): GroupSpec | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const panes = Array.isArray(o.panes) ? o.panes.map(coercePaneSpec) : [];
  if (panes.length === 0) return null;
  const spec: GroupSpec = { panes };
  if (str(o.title) != null) spec.title = str(o.title);
  if (str(o.layout) != null) spec.layout = str(o.layout) as Layout;
  if (Array.isArray(o.sizes)) {
    const sizes = (o.sizes as unknown[]).filter((n): n is number => typeof n === 'number');
    if (sizes.length) spec.sizes = sizes;
  }
  if (num(o.mainFraction) != null) spec.mainFraction = num(o.mainFraction);
  if (num(o.focused) != null) spec.focused = num(o.focused);
  if (num(o.zoomed) != null) spec.zoomed = num(o.zoomed);
  return spec;
}

// Enact a control command. Unknown types and missing targets are no-ops — the
// server already resolved the target window, so this runs in the right renderer.
// Returns a command-specific result (newPane → the new pane id) that App relays
// back to main via the correlationId, or undefined for result-less commands (D).
export function applyControlCommand(cmd: ControlCommand): unknown {
  const ws = useWorkspace.getState();
  const paneId = cmd.paneId;
  switch (cmd.type) {
    case 'focusPane':
      if (paneId) ws.focusPane(paneId);
      break;
    case 'closePane':
      if (paneId) ws.removePane(paneId);
      break;
    case 'restartPane':
      if (paneId) ws.restartPane(paneId);
      break;
    case 'renamePane':
      if (paneId && str(cmd.label) != null) ws.renamePane(paneId, str(cmd.label)!, str(cmd.subtitle));
      break;
    case 'recolorPane':
      if (paneId && str(cmd.color)) ws.recolorPane(paneId, str(cmd.color)!);
      break;
    case 'setMeta': {
      const patch = metaPatch(cmd.meta);
      if (paneId && patch) ws.setPaneMeta(paneId, patch);
      // Echo the TRUE merged meta straight from the store as the command result
      // (mirrors newPane → id, D). The bridge must NOT re-read /state to learn the
      // merge: that read races the renderer's debounced control-publish and returns
      // a pre-merge snapshot (the #7 echo race). `ws` was captured before the
      // mutation, so re-read fresh state. {} (not undefined) for a fully-cleared
      // pane so the echo is always an object; undefined only if the pane is gone.
      if (!paneId) break;
      const pane = useWorkspace.getState().groups.flatMap((g) => g.panes).find((p) => p.id === paneId);
      return pane ? (pane.meta ?? {}) : undefined;
    }
    case 'newPane': {
      const pane = (cmd.pane ?? {}) as Record<string, unknown>;
      // addPane returns the new pane id — relayed back as the command result so a
      // manager can map the spawned worker without a racy list-diff (D). `env`
      // injects extra pty env (e.g. a scoped control token) at spawn (F).
      return ws.addPane({
        label: str(pane.label),
        command: str(pane.command),
        // Verbatim argv → a direct, no-shell spawn of `command` (P4a).
        args: strArray(pane.args),
        cwd: str(pane.cwd),
        shell: str(pane.shell),
        color: str(pane.color),
        meta: metaRecord(pane.meta),
        env: metaRecord(pane.env)
      });
    }
    case 'attach': {
      // Launch attach (a second `hyperpanes …` routed into this window) and the
      // MCP equivalent. `as:'tab'` (default) adds each group as a fresh-shell tab;
      // `as:'panes'` merges all the groups' panes into the active tab. Returns the
      // new tab ids (tab) or new pane ids (panes) so a caller can map them (D).
      const groups = (Array.isArray(cmd.groups) ? cmd.groups : [])
        .map(coerceGroupSpec)
        .filter((g): g is GroupSpec => g != null);
      if (groups.length === 0) return undefined;
      if (cmd.as === 'panes') {
        return groups
          .flatMap((g) => g.panes)
          .map((p) =>
            ws.addPane({
              label: p.label,
              command: p.command,
              args: p.args,
              cwd: p.cwd,
              shell: p.shell,
              color: p.color,
              meta: p.meta
            })
          );
      }
      return ws.appendGroups(groups);
    }
    case 'readScreen': {
      // Rendered-screen read (interactive-pane-driving plan C1): serialize this
      // pane's live xterm buffer to clean text and return it as the command
      // result. Returns undefined if the pane isn't mounted here (no registered
      // serializer) — the server then falls back to the raw replay.
      if (!paneId) return undefined;
      return paneScreens.get(paneId)?.();
    }
    case 'focusTab':
      if (str(cmd.tabId)) ws.setActiveGroup(str(cmd.tabId)!);
      break;
    case 'setLayout': {
      const layout = str(cmd.layout);
      const tabId = str(cmd.tabId) ?? ws.activeId;
      if (layout) ws.setGroupLayout(tabId, layout as Group['layout']);
      break;
    }
    default:
      break;
  }
}

// Reply shape relayed back to main (over the correlationId) and on to the
// `/command` HTTP response: a result on success, or an error string on failure.
export type ControlCommandReply =
  | { ok: true; result?: unknown }
  | { ok: false; error: string };

// Enact a forwarded command and produce the reply main awaits. Wraps
// applyControlCommand so that (a) a throwing store action becomes an error reply
// instead of skipping the reply and hanging the request to its timeout (#3), and
// (b) a result is normalized via `await` in case a future command path returns a
// Promise (else it would be serialized as a pending Promise, #9).
export async function settleControlCommand(cmd: ControlCommand): Promise<ControlCommandReply> {
  try {
    const result = await applyControlCommand(cmd);
    return { ok: true, result };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
}
