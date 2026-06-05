import { useWorkspace, type Group } from './store/useWorkspace';
import { useIdle } from './store/useIdle';
import type { ControlCommand, ControlWindowPayload } from './types';

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
        cwd: str(pane.cwd),
        shell: str(pane.shell),
        color: str(pane.color),
        meta: metaRecord(pane.meta),
        env: metaRecord(pane.env)
      });
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
