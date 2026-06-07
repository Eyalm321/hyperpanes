import { create } from 'zustand';
import { v4 as uuid } from 'uuid';
import type {
  Direction,
  GroupPayload,
  GroupSpec,
  Layout,
  Pane,
  WorkspaceFile
} from '../types';
import { nextColor, remapColor, type PaletteName } from '../theme';
import { useSettings } from './useSettings';
import { clampFraction, equalSizes, insertSize, isEqualSplit, normalize, removeSize } from '../layout/sizes';
import { computeTiles, effectiveLayout } from '../layout/presets';
import { neighborIndex } from '../layout/navigate';

const VALID_LAYOUTS: Layout[] = ['auto', 'single', 'columns', 'rows', 'grid', 'main-stack'];

export const DEFAULT_FONT_SIZE = 13;
const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 40;
const clampFont = (n: number) => Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, n));

// Session uids currently being moved between groups. A Terminal unmounting for
// one of these detaches instead of killing its pty, so the process survives the
// move (the new mount re-attaches and replays output). See Terminal cleanup.
export const movingSessions = new Set<string>();

// A pane's explicit font-size override (set by zooming), or undefined when the
// pane follows the configurable default. Kept separate from paneFontSize so
// components can resolve the default reactively (see Terminal).
export const paneFontOverride = (s: WorkspaceState, id: string): number | undefined => {
  for (const g of s.groups) {
    const p = g.panes.find((pp) => pp.id === id);
    if (p) return p.fontSize;
  }
  return undefined;
};

// A pane's effective font size: its zoom override, else the default-size setting.
export const paneFontSize = (s: WorkspaceState, id: string): number =>
  paneFontOverride(s, id) ?? useSettings.getState().defaultFontSize;

// A group is a self-contained workspace (one tab): its own panes, layout, focus,
// zoom and sizing. Every group's panes stay mounted (see App), so
// background tabs keep their shells running. The shape is identical to
// `GroupPayload` so a whole group can be torn off to another window verbatim.
export type Group = GroupPayload;

interface ClosedGroup {
  spec: GroupSpec;
  index: number; // where it sat, so reopen restores its position
}

interface WorkspaceState {
  groups: Group[];
  activeId: string;
  closed: ClosedGroup[]; // stack of recently closed tabs (Ctrl+Shift+T)
  groupSeq: number; // monotonic counter for default tab titles

  // ---- tabs ----
  addGroup: () => void;
  closeGroup: (id: string) => void;
  reopenGroup: () => void;
  duplicateGroup: (id: string) => void; // clone a tab (fresh ids/sessions) after it
  closeOthers: (id: string) => void; // close every tab but `id`
  closeToRight: (id: string) => void; // close tabs after `id`
  popOutGroup: (id: string) => void; // detach `id` into a fresh window (non-drag)
  setGroupLayout: (id: string, layout: Layout) => void; // set a specific tab's layout
  setActiveGroup: (id: string) => void;
  cycleGroup: (delta: number) => void; // +1 next tab, -1 previous (wraps)
  renameGroup: (id: string, title: string) => void;
  moveGroupToIndex: (groupId: string, index: number) => void; // reorder a tab in the strip
  movePaneToGroup: (paneId: string, targetGroupId: string, index?: number) => void;
  movePaneToNewGroup: (paneId: string) => void;
  // ---- cross-window tab move (Stage 2) ----
  extractGroup: (groupId: string) => GroupPayload | null; // remove from this window, hand off live
  extractPaneAsGroup: (paneId: string) => GroupPayload | null; // tear one pane out to another window
  injectGroup: (group: GroupPayload, index?: number) => void; // adopt a group at slot `index`
  adoptPaneInto: (pane: Pane, targetGroupId: string, index: number) => void; // stitch a pane in

  // ---- panes (act on the active group, or the group owning the pane id) ----
  addPane: (partial?: Partial<Pane>) => string;
  removePane: (id: string) => void;
  renamePane: (id: string, label: string, subtitle?: string) => void; // subtitle: '' clears, undefined leaves as-is
  recolorPane: (id: string, color: string) => void;
  setPaneFrame: (id: string, on: boolean) => void; // per-pane show-frame override (color toggles)
  setPaneDot: (id: string, on: boolean) => void; // per-pane show-dot override (color toggles)
  applyProjectToPane: (id: string, project: { color: string; name: string }) => void; // tint a pane to a detected git project
  setPaneMeta: (id: string, patch: Record<string, string | null>) => void; // merge org metadata; null value deletes a key (agent-orchestration C)
  remapPalette: (to: PaletteName) => void; // re-slot every pane's color into a palette
  restartPane: (id: string) => void;
  zoomPane: (id: string, delta: number) => void;
  resetPaneZoom: (id: string) => void;
  setLayout: (layout: Layout) => void;
  focusPane: (id: string) => void;
  focusIndex: (index: number) => void;
  focusDirection: (dir: Direction) => void;
  toggleZoom: (id?: string) => void;
  setSizes: (sizes: number[]) => void;
  setMainFraction: (fraction: number) => void;
  markExited: (id: string, code: number) => void;
  loadWorkspace: (file: WorkspaceFile) => void; // into the active tab
  loadSession: (file: WorkspaceFile) => void; // replace all tabs
}

// When zoomed, navigation keeps the zoom glued to the newly focused pane.
const follow = (zoomedId: string | null, targetId: string) => (zoomedId ? targetId : zoomedId);

// The currently active group. groups is never empty, so this is always defined.
export const activeGroup = (s: WorkspaceState): Group =>
  s.groups.find((g) => g.id === s.activeId) ?? s.groups[0];

export function specFromGroup(g: Group): GroupSpec {
  // Emit the sizing/focus/zoom fields only when they differ from the defaults,
  // so ordinary tabs stay terse; groupFromSpec restores the defaults otherwise.
  const focusedIndex = g.panes.findIndex((p) => p.id === g.focusedId);
  const zoomedIndex = g.zoomedId ? g.panes.findIndex((p) => p.id === g.zoomedId) : -1;
  return {
    title: g.title,
    layout: g.layout,
    panes: g.panes.map((p) => ({
      label: p.label,
      ...(p.subtitle ? { subtitle: p.subtitle } : {}),
      color: p.color,
      ...(p.showFrame !== undefined ? { showFrame: p.showFrame } : {}),
      ...(p.showDot !== undefined ? { showDot: p.showDot } : {}),
      ...(p.command ? { command: p.command } : {}),
      ...(p.args && p.args.length ? { args: p.args } : {}),
      ...(p.cwd ? { cwd: p.cwd } : {}),
      ...(p.shell ? { shell: p.shell } : {}),
      ...(p.fontSize ? { fontSize: p.fontSize } : {}),
      ...(p.meta && Object.keys(p.meta).length ? { meta: p.meta } : {})
    })),
    ...(g.panes.length > 1 && !isEqualSplit(g.sizes) ? { sizes: g.sizes } : {}),
    ...(g.layout === 'main-stack' && Math.abs(g.mainFraction - 0.6) > 1e-6
      ? { mainFraction: g.mainFraction }
      : {}),
    ...(focusedIndex > 0 ? { focused: focusedIndex } : {}),
    ...(zoomedIndex >= 0 ? { zoomed: zoomedIndex } : {})
  };
}

function emptyGroup(seq: number): Group {
  return {
    id: uuid(),
    title: `workspace ${seq}`,
    panes: [],
    layout: 'auto',
    focusedId: null,
    zoomedId: null,
    sizes: [],
    mainFraction: 0.6,
    seq: 0
  };
}

// A fresh tab seeded with one interactive shell so it's usable immediately.
function seededGroup(seq: number): Group {
  const base = emptyGroup(seq);
  const pane: Pane = {
    id: uuid(),
    sessionUid: uuid(),
    label: 'shell',
    color: nextColor(0, useSettings.getState().framePalette),
    // New panes are clean/uncolored by default (no frame, no dot).
    showFrame: false,
    showDot: false,
    status: 'running'
  };
  return { ...base, panes: [pane], sizes: equalSizes(1), focusedId: pane.id, seq: 1 };
}

// A torn-off pane's `env` (a scoped control token) must not travel into another
// window's store — env is runtime-only and never leaves the window that spawned
// the pane (see Pane.env in types.ts). The live pty keeps its own env and a
// re-attach ignores env anyway, so dropping it from the handed-off record is
// safe and upholds the invariant (#5).
const stripPaneEnv = ({ env: _env, ...pane }: Pane): Pane => pane;

// Coerce a spec's direct-spawn args (P4a) into a clean string[] | undefined: specs
// come from disk / an agent, so keep only string entries and drop an empty result.
// Mirrors control.ts's strArray so both spawn paths validate identically.
const argvOf = (v: unknown): string[] | undefined => {
  if (!Array.isArray(v)) return undefined;
  const out = v.filter((x): x is string => typeof x === 'string');
  return out.length ? out : undefined;
};

// Remove a pane from a group, fixing up sizes/focus/zoom (no session kill).
function withoutPane(g: Group, paneId: string): Group {
  const index = g.panes.findIndex((p) => p.id === paneId);
  if (index < 0) return g;
  const panes = g.panes.filter((p) => p.id !== paneId);
  // Auto layouts are always equal splits — recompute rather than rebalance (Q7).
  const sizes = g.layout === 'auto' ? equalSizes(panes.length) : removeSize(g.sizes, index);
  let focusedId = g.focusedId;
  if (focusedId === paneId) {
    const next = panes[Math.min(index, panes.length - 1)];
    focusedId = next ? next.id : null;
  }
  const zoomedId = g.zoomedId === paneId ? null : g.zoomedId;
  return { ...g, panes, sizes, focusedId, zoomedId };
}

function groupFromSpec(spec: GroupSpec, fallbackTitle: string): Group {
  const palette = useSettings.getState().framePalette;
  let seq = 0;
  const panes: Pane[] = (spec.panes ?? []).map((p) => {
    seq += 1;
    return {
      id: uuid(),
      sessionUid: uuid(),
      label: p.label || `pane ${seq}`,
      subtitle: p.subtitle || undefined,
      // Heal a color saved under an older palette definition to the active
      // palette's current value for its slot; custom colors pass through.
      color: p.color ? remapColor(p.color, palette) : nextColor(seq - 1, palette),
      showFrame: p.showFrame,
      showDot: p.showDot,
      command: p.command || undefined,
      args: argvOf(p.args), // direct-spawn argv (P4a), defensively coerced
      cwd: p.cwd || undefined,
      shell: p.shell || undefined,
      status: 'running' as const,
      fontSize: p.fontSize || undefined,
      meta: p.meta && Object.keys(p.meta).length ? p.meta : undefined
    };
  });
  // A spec with no (or invalid) layout defaults to auto; an explicit saved
  // layout — including 'columns' — passes through untouched (no migration, Q6).
  const layout = spec.layout && VALID_LAYOUTS.includes(spec.layout) ? spec.layout : 'auto';

  // Optional sizing / focus / zoom, all defensively validated (specs come from
  // disk or an agent). A bad / mismatched value silently falls back to default.
  const n = panes.length;
  const validIndex = (i: unknown): i is number =>
    typeof i === 'number' && Number.isInteger(i) && i >= 0 && i < n;
  const sizesOk =
    Array.isArray(spec.sizes) &&
    spec.sizes.length === n &&
    spec.sizes.every((s) => typeof s === 'number' && Number.isFinite(s) && s > 0);
  // Auto layout is always an equal split (see withoutPane), so custom sizes only
  // apply to an explicit layout.
  const sizes = layout !== 'auto' && sizesOk ? normalize(spec.sizes!) : equalSizes(n);
  const mainFraction =
    typeof spec.mainFraction === 'number' && Number.isFinite(spec.mainFraction)
      ? clampFraction(spec.mainFraction)
      : 0.6;
  const focusedId = validIndex(spec.focused) ? panes[spec.focused].id : (panes[0]?.id ?? null);
  const zoomedId = validIndex(spec.zoomed) ? panes[spec.zoomed].id : null;

  return {
    id: uuid(),
    title: spec.title || fallbackTitle,
    panes,
    layout,
    focusedId,
    zoomedId,
    sizes,
    mainFraction,
    seq
  };
}

const initial = emptyGroup(1);

// Close this renderer's window (sessions it owns are reaped by main on close).
function closeWindow() {
  if (typeof window !== 'undefined') window.hp?.win?.close();
}

export const useWorkspace = create<WorkspaceState>((set, get) => {
  // Replace the active group with a transformed copy.
  const mapActive = (s: WorkspaceState, fn: (g: Group) => Group): Group[] =>
    s.groups.map((g) => (g.id === s.activeId ? fn(g) : g));

  // Replace whichever group owns `paneId` (works for background tabs too).
  const mapPaneGroup = (s: WorkspaceState, paneId: string, fn: (g: Group) => Group): Group[] =>
    s.groups.map((g) => (g.panes.some((p) => p.id === paneId) ? fn(g) : g));

  return {
    groups: [initial],
    activeId: initial.id,
    closed: [],
    groupSeq: 1,

    addGroup: () =>
      set((s) => {
        const groupSeq = s.groupSeq + 1;
        const g = seededGroup(groupSeq);
        return { groups: [...s.groups, g], activeId: g.id, groupSeq };
      }),

    closeGroup: (id) => {
      const s = get();
      if (!s.groups.some((g) => g.id === id)) return;
      // Closing the last tab closes the window (matches a browser).
      if (s.groups.length === 1) {
        closeWindow();
        return;
      }
      set((st) => {
        const index = st.groups.findIndex((g) => g.id === id);
        if (index < 0) return st;
        const spec = specFromGroup(st.groups[index]);
        const groups = st.groups.filter((g) => g.id !== id);
        let activeId = st.activeId;
        if (activeId === id) activeId = groups[Math.min(index, groups.length - 1)].id;
        return { groups, activeId, closed: [...st.closed, { spec, index }] };
      });
    },

    reopenGroup: () =>
      set((s) => {
        if (s.closed.length === 0) return s;
        const closed = s.closed.slice();
        const last = closed.pop()!;
        const groupSeq = s.groupSeq + 1;
        const g = groupFromSpec(last.spec, `workspace ${groupSeq}`);
        const groups = s.groups.slice();
        groups.splice(Math.min(Math.max(last.index, 0), groups.length), 0, g);
        return { groups, activeId: g.id, closed, groupSeq };
      }),

    // Clone a tab right after it. groupFromSpec (the same builder reopenGroup uses)
    // mints fresh pane ids + sessionUids, so the copy gets new shells — command
    // panes re-run, interactive shells start clean. The clone becomes active.
    duplicateGroup: (id) =>
      set((s) => {
        const i = s.groups.findIndex((g) => g.id === id);
        if (i < 0) return s;
        const groupSeq = s.groupSeq + 1;
        const g = groupFromSpec(specFromGroup(s.groups[i]), `workspace ${groupSeq}`);
        const groups = s.groups.slice();
        groups.splice(i + 1, 0, g);
        return { groups, activeId: g.id, groupSeq };
      }),

    // Close every tab except `id`, pushing each removed tab onto the reopen stack
    // (each individually reopenable). Their ptys die when the panes unmount, exactly
    // as closeGroup relies on — no extra kill logic here.
    closeOthers: (id) =>
      set((s) => {
        const keep = s.groups.find((g) => g.id === id);
        if (!keep || s.groups.length < 2) return s;
        const closed = [...s.closed];
        s.groups.forEach((g, index) => {
          if (g.id !== id) closed.push({ spec: specFromGroup(g), index });
        });
        return { groups: [keep], activeId: id, closed };
      }),

    // Close the tabs to the right of `id` (same reopen-stack handling as closeOthers).
    closeToRight: (id) =>
      set((s) => {
        const i = s.groups.findIndex((g) => g.id === id);
        if (i < 0 || i === s.groups.length - 1) return s;
        const closed = [...s.closed];
        s.groups.slice(i + 1).forEach((g, k) => closed.push({ spec: specFromGroup(g), index: i + 1 + k }));
        const groups = s.groups.slice(0, i + 1);
        const activeId = groups.some((g) => g.id === s.activeId) ? s.activeId : id;
        return { groups, activeId, closed };
      }),

    // Detach a tab into a fresh window without a drag. extractGroup flags its
    // sessions "moving" (so their Terminals detach, not die) and the new window
    // re-attaches to the live ptys. No-op for the sole tab (nothing to pop out).
    popOutGroup: (id) => {
      if (get().groups.length < 2) return;
      const group = get().extractGroup(id);
      if (group) void window.hp?.win?.spawnGroupWindow(group);
    },

    // Set a specific tab's layout without activating it (the menu acts on the
    // clicked tab). Mirrors setLayout, which only ever targets the active group.
    setGroupLayout: (id, layout) =>
      set((s) => ({ groups: s.groups.map((g) => (g.id === id ? { ...g, layout } : g)) })),

    setActiveGroup: (id) => set((s) => (s.groups.some((g) => g.id === id) ? { activeId: id } : s)),

    cycleGroup: (delta) =>
      set((s) => {
        if (s.groups.length < 2) return s;
        const i = s.groups.findIndex((g) => g.id === s.activeId);
        const next = (i + delta + s.groups.length) % s.groups.length;
        return { activeId: s.groups[next].id };
      }),

    renameGroup: (id, title) =>
      set((s) => ({ groups: s.groups.map((g) => (g.id === id ? { ...g, title } : g)) })),

    // Reorder a tab live during an on-strip drag. `index` is the slot AMONG THE
    // OTHER tabs (the dragged one removed first), which is jitter-free: it's
    // computed from the non-dragged tabs' centers, so moving the dragged tab can't
    // shift its own reference points.
    moveGroupToIndex: (groupId, index) =>
      set((s) => {
        const from = s.groups.findIndex((g) => g.id === groupId);
        if (from < 0) return s;
        const rest = s.groups.slice();
        const [g] = rest.splice(from, 1);
        const at = Math.max(0, Math.min(index, rest.length));
        rest.splice(at, 0, g);
        if (rest.every((gg, i) => gg.id === s.groups[i].id)) return s; // unchanged
        return { groups: rest };
      }),

    movePaneToGroup: (paneId, targetGroupId, index) =>
      set((s) => {
        const src = s.groups.find((g) => g.panes.some((p) => p.id === paneId));
        if (!src) return s;
        if (!s.groups.some((g) => g.id === targetGroupId)) return s;
        // Reorder within the same group (drop onto a sibling). Move the pane — and
        // its size slot — to the new index; the pane stays mounted (keyed by id), so
        // no session churn. `index` is in the original order incl. the dragged pane.
        if (src.id === targetGroupId) {
          const from = src.panes.findIndex((p) => p.id === paneId);
          let to = index == null ? src.panes.length : Math.max(0, Math.min(index, src.panes.length));
          if (to > from) to -= 1; // removal shifts the target left by one
          if (from < 0 || to === from) return s;
          const panes = src.panes.slice();
          const [moved] = panes.splice(from, 1);
          panes.splice(to, 0, moved);
          let sizes = src.sizes;
          if (src.layout === 'auto' || src.sizes.length !== src.panes.length) {
            sizes = equalSizes(panes.length);
          } else {
            sizes = src.sizes.slice();
            const [movedSize] = sizes.splice(from, 1);
            sizes.splice(to, 0, movedSize);
          }
          return { groups: s.groups.map((g) => (g.id === src.id ? { ...g, panes, sizes } : g)) };
        }
        const pane = src.panes.find((p) => p.id === paneId)!;
        movingSessions.add(pane.sessionUid); // Terminal will detach, not kill
        const trimmed = withoutPane(src, paneId);
        return {
          groups: s.groups.map((g) => {
            if (g.id === src.id) return trimmed;
            if (g.id === targetGroupId) {
              // Insert at `index` (the drop slot in the target's layout), else append.
              const at = index == null ? g.panes.length : Math.max(0, Math.min(index, g.panes.length));
              const panes = [...g.panes.slice(0, at), pane, ...g.panes.slice(at)];
              return {
                ...g,
                panes,
                sizes: g.layout === 'auto' ? equalSizes(panes.length) : insertSize(g.sizes, at),
                focusedId: pane.id
              };
            }
            return g;
          })
        };
      }),

    movePaneToNewGroup: (paneId) =>
      set((s) => {
        const src = s.groups.find((g) => g.panes.some((p) => p.id === paneId));
        if (!src || src.panes.length < 2) return s; // already alone — nothing to split
        const pane = src.panes.find((p) => p.id === paneId)!;
        movingSessions.add(pane.sessionUid);
        const trimmed = withoutPane(src, paneId);
        const groupSeq = s.groupSeq + 1;
        const newGroup: Group = {
          ...emptyGroup(groupSeq),
          panes: [pane],
          sizes: equalSizes(1),
          focusedId: pane.id
        };
        return {
          groups: s.groups.map((g) => (g.id === src.id ? trimmed : g)).concat(newGroup),
          activeId: newGroup.id,
          groupSeq
        };
      }),

    // Tear a single pane out to another window: wrap it in a fresh one-pane group
    // (carrying its live sessionUid), remove it from its source tab, and hand the
    // group back for IPC. The session is flagged `moving` so its Terminal detaches
    // (pty stays alive) and the receiving window re-attaches. If that empties the
    // source tab, drop the tab — reseeding a shell only if it was the last one.
    extractPaneAsGroup: (paneId) => {
      let payload: GroupPayload | null = null;
      set((s) => {
        const src = s.groups.find((g) => g.panes.some((p) => p.id === paneId));
        if (!src) return s;
        const pane = src.panes.find((p) => p.id === paneId)!;
        movingSessions.add(pane.sessionUid);
        let groupSeq = s.groupSeq + 1;
        payload = {
          ...emptyGroup(groupSeq),
          title: pane.label,
          panes: [stripPaneEnv(pane)], // env stays in this window (#5)
          sizes: equalSizes(1),
          focusedId: pane.id
        };
        const trimmed = withoutPane(src, paneId);
        let groups: Group[];
        let activeId = s.activeId;
        if (trimmed.panes.length === 0) {
          groups = s.groups.filter((g) => g.id !== src.id);
          if (groups.length === 0) {
            groupSeq += 1;
            const seeded = seededGroup(groupSeq);
            groups = [seeded];
            activeId = seeded.id;
          } else if (activeId === src.id) {
            activeId = groups[0].id;
          }
        } else {
          groups = s.groups.map((g) => (g.id === src.id ? trimmed : g));
        }
        return { groups, activeId, groupSeq };
      });
      return payload;
    },

    // Hand a whole tab off to another window. Every session in it is flagged
    // `moving` so its Terminal detaches (keeps the pty alive) on unmount; the
    // receiving window re-attaches by sessionUid. Returns the live group so the
    // caller can ship it over IPC. Never leaves the window with zero tabs.
    extractGroup: (groupId) => {
      let payload: GroupPayload | null = null;
      set((s) => {
        const index = s.groups.findIndex((g) => g.id === groupId);
        if (index < 0) return s;
        const src = s.groups[index];
        src.panes.forEach((p) => movingSessions.add(p.sessionUid));
        // Hand off the live group with each pane's env stripped (#5).
        payload = { ...src, panes: src.panes.map(stripPaneEnv) };
        let groups = s.groups.filter((g) => g.id !== groupId);
        let groupSeq = s.groupSeq;
        let activeId = s.activeId;
        if (groups.length === 0) {
          groupSeq += 1;
          const seeded = seededGroup(groupSeq);
          groups = [seeded];
          activeId = seeded.id;
        } else if (activeId === groupId) {
          activeId = groups[Math.min(index, groups.length - 1)].id;
        }
        return { groups, activeId, groupSeq };
      });
      return payload;
    },

    // Adopt a group docked in from another window, inserting it at slot `index`
    // (the cursor's drop position; defaults to the end). A pristine window still
    // shows its lone empty starter tab — replace it rather than leaving a blank
    // tab beside the adopted one. Idempotent if the same group is delivered twice.
    injectGroup: (group, index) =>
      set((s) => {
        if (s.groups.some((g) => g.id === group.id)) return { activeId: group.id };
        const pristine = s.groups.length === 1 && s.groups[0].panes.length === 0;
        if (pristine) return { groups: [group], activeId: group.id };
        const at = index == null ? s.groups.length : Math.max(0, Math.min(index, s.groups.length));
        const groups = [...s.groups.slice(0, at), group, ...s.groups.slice(at)];
        return { groups, activeId: group.id };
      }),

    // Stitch a pane handed over from ANOTHER window into `targetGroupId` at slot
    // `index` (cross-window pane → layout). The pane keeps its live sessionUid, so
    // its Terminal re-attaches on mount; idempotent if delivered twice.
    adoptPaneInto: (pane, targetGroupId, index) =>
      set((s) => {
        const target = s.groups.find((g) => g.id === targetGroupId);
        if (!target) return s;
        if (target.panes.some((p) => p.id === pane.id)) return { activeId: targetGroupId };
        const at = Math.max(0, Math.min(index, target.panes.length));
        const panes = [...target.panes.slice(0, at), pane, ...target.panes.slice(at)];
        const sizes = target.layout === 'auto' ? equalSizes(panes.length) : insertSize(target.sizes, at);
        return {
          groups: s.groups.map((g) =>
            g.id === targetGroupId ? { ...g, panes, sizes, focusedId: pane.id } : g
          ),
          activeId: targetGroupId
        };
      }),

    addPane: (partial) => {
      const id = uuid();
      set((s) => ({
        groups: mapActive(s, (g) => {
          const seq = g.seq + 1;
          const pane: Pane = {
            id,
            sessionUid: uuid(),
            label: partial?.label ?? `pane ${seq}`,
            subtitle: partial?.subtitle,
            color: partial?.color ?? nextColor(seq - 1, useSettings.getState().framePalette),
            // Clean/uncolored by default; a caller (e.g. opening a project) can
            // override, and a git-project tint flips these on later.
            showFrame: partial?.showFrame ?? false,
            showDot: partial?.showDot ?? false,
            command: partial?.command,
            args: partial?.args,
            cwd: partial?.cwd,
            shell: partial?.shell,
            status: 'running',
            meta: partial?.meta,
            env: partial?.env
          };
          return {
            ...g,
            panes: [...g.panes, pane],
            // Auto stays equal-split; concrete layouts rebalance around the new pane.
            sizes:
              g.layout === 'auto'
                ? equalSizes(g.panes.length + 1)
                : insertSize(g.sizes, g.panes.length),
            focusedId: id,
            seq
          };
        })
      }));
      return id;
    },

    removePane: (id) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => {
          const index = g.panes.findIndex((p) => p.id === id);
          if (index < 0) return g;
          const panes = g.panes.filter((p) => p.id !== id);
          const sizes = g.layout === 'auto' ? equalSizes(panes.length) : removeSize(g.sizes, index);
          let focusedId = g.focusedId;
          if (focusedId === id) {
            const next = panes[Math.min(index, panes.length - 1)];
            focusedId = next ? next.id : null;
          }
          const zoomedId = g.zoomedId === id ? null : g.zoomedId;
          return { ...g, panes, sizes, focusedId, zoomedId };
        })
      })),

    renamePane: (id, label, subtitle) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) =>
            p.id === id
              ? {
                  ...p,
                  label,
                  // undefined arg leaves the subtitle untouched; '' clears it.
                  ...(subtitle === undefined ? {} : { subtitle: subtitle.trim() || undefined })
                }
              : p
          )
        }))
      })),

    recolorPane: (id, color) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) => (p.id === id ? { ...p, color } : p))
        }))
      })),

    // Per-pane override of the global show-frame toggle (undefined inherits it).
    setPaneFrame: (id, on) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) => (p.id === id ? { ...p, showFrame: on } : p))
        }))
      })),

    // Per-pane override of the global show-dot toggle (undefined inherits it).
    setPaneDot: (id, on) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) => (p.id === id ? { ...p, showDot: on } : p))
        }))
      })),

    // Tint a pane to a detected git project: adopt its color, turn the frame and
    // dot on, and show the repo name as the subtitle. (projects feature seam)
    applyProjectToPane: (id, project) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) =>
            p.id === id
              ? { ...p, color: project.color, showFrame: true, showDot: true, subtitle: project.name }
              : p
          )
        }))
      })),

    // Merge a free-form metadata PATCH onto a pane (agent-orchestration C): a
    // string value sets/overwrites a key, an explicit `null` deletes it (so a
    // stale key — e.g. `task` — can be cleared, #6); untouched keys are kept. The
    // meta object is dropped entirely once empty, so a cleared pane matches a
    // never-set one (no `meta` in the control payload). Surfaced to the control
    // plane via buildControlPayload so an orchestrator can read the org's shape.
    setPaneMeta: (id, patch) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) => {
            if (p.id !== id) return p;
            const meta = { ...(p.meta ?? {}) };
            for (const [k, v] of Object.entries(patch)) {
              if (v === null) delete meta[k];
              else meta[k] = v;
            }
            return { ...p, meta: Object.keys(meta).length ? meta : undefined };
          })
        }))
      })),

    // Switch palettes: re-slot every pane's color across all tabs into `to`.
    // Custom (non-slot) colors are left untouched (see theme.remapColor).
    remapPalette: (to) =>
      set((s) => ({
        groups: s.groups.map((g) => ({
          ...g,
          panes: g.panes.map((p) => ({ ...p, color: remapColor(p.color, to) }))
        }))
      })),

    restartPane: (id) =>
      set((s) => ({
        // New sessionUid re-keys the Terminal effect: it kills the old pty (if
        // any) and spawns a fresh one running the same command/cwd.
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) =>
            p.id === id ? { ...p, sessionUid: uuid(), status: 'running', exitCode: undefined } : p
          )
        }))
      })),

    zoomPane: (id, delta) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) =>
            p.id === id
              ? {
                  ...p,
                  fontSize: clampFont((p.fontSize ?? useSettings.getState().defaultFontSize) + delta)
                }
              : p
          )
        }))
      })),

    // Clear the per-pane override so the pane follows the default-size setting
    // again (and tracks future changes to it).
    resetPaneZoom: (id) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) => (p.id === id ? { ...p, fontSize: undefined } : p))
        }))
      })),

    setLayout: (layout) => set((s) => ({ groups: mapActive(s, (g) => ({ ...g, layout })) })),

    focusPane: (id) =>
      set((s) => ({ groups: mapPaneGroup(s, id, (g) => ({ ...g, focusedId: id })) })),

    focusIndex: (index) =>
      set((s) => ({
        groups: mapActive(s, (g) => {
          const target = g.panes[index];
          if (!target) return g;
          return { ...g, focusedId: target.id, zoomedId: follow(g.zoomedId, target.id) };
        })
      })),

    focusDirection: (dir) =>
      set((s) => ({
        groups: mapActive(s, (g) => {
          const n = g.panes.length;
          if (n < 2) return g;
          const fromIndex = Math.max(
            0,
            g.panes.findIndex((p) => p.id === g.focusedId)
          );

          // Resolve auto to a concrete preset so the geometry below — and the
          // single-pane cycle check — never see 'auto'.
          const eff = effectiveLayout(g.layout, n);
          let targetIndex: number | null;
          if (g.zoomedId || eff === 'single') {
            // Only one pane is visible — cycle through the order instead.
            const delta = dir === 'right' || dir === 'down' ? 1 : -1;
            targetIndex = (fromIndex + delta + n) % n;
          } else {
            const tiles = computeTiles(eff, n, g.sizes, g.mainFraction, fromIndex);
            targetIndex = neighborIndex(tiles, fromIndex, dir);
          }

          if (targetIndex === null) return g;
          const target = g.panes[targetIndex];
          return { ...g, focusedId: target.id, zoomedId: follow(g.zoomedId, target.id) };
        })
      })),

    toggleZoom: (id) =>
      set((s) => {
        if (id) {
          return {
            groups: mapPaneGroup(s, id, (g) => ({
              ...g,
              zoomedId: g.zoomedId === id ? null : id,
              focusedId: id
            }))
          };
        }
        return {
          groups: mapActive(s, (g) => {
            const target = g.focusedId;
            if (!target) return g;
            return { ...g, zoomedId: g.zoomedId === target ? null : target, focusedId: target };
          })
        };
      }),

    setSizes: (sizes) => set((s) => ({ groups: mapActive(s, (g) => ({ ...g, sizes })) })),
    setMainFraction: (mainFraction) =>
      set((s) => ({ groups: mapActive(s, (g) => ({ ...g, mainFraction })) })),

    markExited: (id, code) =>
      set((s) => ({
        groups: mapPaneGroup(s, id, (g) => ({
          ...g,
          panes: g.panes.map((p) => (p.id === id ? { ...p, status: 'exited', exitCode: code } : p))
        }))
      })),

    loadWorkspace: (file) =>
      set((s) => {
        const built = groupFromSpec(
          { title: file.name, layout: file.layout, panes: file.panes ?? [] },
          activeGroup(s).title
        );
        // Keep the active group's id so it stays the same tab.
        const replaced: Group = { ...built, id: s.activeId };
        return { groups: s.groups.map((g) => (g.id === s.activeId ? replaced : g)) };
      }),

    loadSession: (file) =>
      set((s) => {
        const specs: GroupSpec[] =
          file.groups && file.groups.length > 0
            ? file.groups
            : [{ title: file.name, layout: file.layout, panes: file.panes ?? [] }];
        let groupSeq = 0;
        const groups = specs.map((spec) => {
          groupSeq += 1;
          return groupFromSpec(spec, `workspace ${groupSeq}`);
        });
        const idx =
          file.active != null && file.active >= 0 && file.active < groups.length ? file.active : 0;
        return { groups, activeId: groups[idx].id, closed: s.closed, groupSeq };
      })
  };
});
