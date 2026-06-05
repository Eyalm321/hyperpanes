// ---------------------------------------------------------------------------
// Capability scoping for the control API (agent-orchestration F). A token is
// either the master token (unscoped — the root/CEO, written to control.json) or
// a minted token carrying a Scope that limits which panes/tabs/windows it can
// reach. Scoping is opt-in: the single-orchestrator case ignores it entirely;
// recursive orgs hand each manager a subtree-scoped token.
//
// These are the pure predicates; ControlServer applies them on every route and
// uses the live pane tree to validate sub-scope minting (canMint).
// ---------------------------------------------------------------------------

// A scope names allowed targets at any level. A pane is reachable if it matches
// on ANY level (its own id, its tab, or its window). Empty/absent arrays match
// nothing at that level. A null scope (not represented here) means unscoped.
export interface Scope {
  windowIds?: number[];
  tabIds?: string[];
  paneIds?: string[];
}

// A pane's addressing coordinates, from the server read-model.
export interface PaneCoords {
  paneId: string;
  tabId: string;
  windowId: number;
}

// Whether `scope` (null = unscoped/master) may touch a specific pane.
export function paneInScope(scope: Scope | null, c: PaneCoords): boolean {
  if (!scope) return true;
  return (
    !!scope.paneIds?.includes(c.paneId) ||
    !!scope.tabIds?.includes(c.tabId) ||
    !!scope.windowIds?.includes(c.windowId)
  );
}

// Whether `scope` may act on a whole window (e.g. a window-targeted command).
export function windowInScope(scope: Scope | null, windowId: number): boolean {
  if (!scope) return true;
  return !!scope.windowIds?.includes(windowId);
}

// Whether `scope` may act on a tab (its tab id, or its owning window).
export function tabInScope(scope: Scope | null, tabId: string, windowId: number): boolean {
  if (!scope) return true;
  return !!scope.tabIds?.includes(tabId) || !!scope.windowIds?.includes(windowId);
}

// Validate a requested scope: every named id must resolve to a real target and
// be reachable by the minter's scope (so a parent can only mint NARROWER tokens
// — no privilege escalation). `coordsOf`/`tabWindow` come from the live tree;
// unknown ids are rejected. Returns the first problem, or null if OK.
export function checkMintable(
  parent: Scope | null,
  child: Scope,
  tree: {
    paneCoords: (paneId: string) => PaneCoords | null;
    tabWindow: (tabId: string) => number | null;
    hasWindow: (windowId: number) => boolean;
  }
): string | null {
  for (const w of child.windowIds ?? []) {
    if (!tree.hasWindow(w)) return `unknown windowId ${w}`;
    if (!windowInScope(parent, w)) return `windowId ${w} is outside the minting token's scope`;
  }
  for (const t of child.tabIds ?? []) {
    const win = tree.tabWindow(t);
    if (win == null) return `unknown tabId ${t}`;
    if (!tabInScope(parent, t, win)) return `tabId ${t} is outside the minting token's scope`;
  }
  for (const p of child.paneIds ?? []) {
    const coords = tree.paneCoords(p);
    if (!coords) return `unknown paneId ${p}`;
    if (!paneInScope(parent, coords)) return `paneId ${p} is outside the minting token's scope`;
  }
  if (!(child.windowIds?.length || child.tabIds?.length || child.paneIds?.length)) {
    return 'scope must name at least one windowId, tabId, or paneId';
  }
  return null;
}

// Validate + normalize an untrusted scope payload (from JSON over /tokens).
// Drops non-arrays / wrong element types; returns null if nothing usable.
export function coerceScope(v: unknown): Scope | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const nums = (x: unknown): number[] | undefined =>
    Array.isArray(x) ? x.filter((n): n is number => typeof n === 'number' && Number.isFinite(n)) : undefined;
  const strs = (x: unknown): string[] | undefined =>
    Array.isArray(x) ? x.filter((s): s is string => typeof s === 'string') : undefined;
  const scope: Scope = {};
  const w = nums(o.windowIds);
  const t = strs(o.tabIds);
  const p = strs(o.paneIds);
  if (w && w.length) scope.windowIds = w;
  if (t && t.length) scope.tabIds = t;
  if (p && p.length) scope.paneIds = p;
  return scope.windowIds || scope.tabIds || scope.paneIds ? scope : null;
}
