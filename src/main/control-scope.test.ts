import { describe, expect, it } from 'vitest';
import {
  paneInScope,
  windowInScope,
  tabInScope,
  checkMintable,
  coerceScope,
  type Scope
} from './control-scope';

const coords = (paneId: string, tabId: string, windowId: number) => ({ paneId, tabId, windowId });

describe('scope predicates', () => {
  it('null scope (master) reaches everything', () => {
    expect(paneInScope(null, coords('p', 't', 1))).toBe(true);
    expect(windowInScope(null, 99)).toBe(true);
    expect(tabInScope(null, 't', 1)).toBe(true);
  });

  it('paneInScope matches on pane, tab, or window level', () => {
    expect(paneInScope({ paneIds: ['p1'] }, coords('p1', 't', 1))).toBe(true);
    expect(paneInScope({ tabIds: ['t1'] }, coords('p9', 't1', 1))).toBe(true);
    expect(paneInScope({ windowIds: [2] }, coords('p9', 't9', 2))).toBe(true);
    expect(paneInScope({ paneIds: ['p1'] }, coords('p2', 't', 1))).toBe(false);
    expect(paneInScope({ tabIds: ['t1'] }, coords('p', 't2', 1))).toBe(false);
  });

  it('window/tab predicates', () => {
    expect(windowInScope({ windowIds: [1] }, 1)).toBe(true);
    expect(windowInScope({ tabIds: ['t'] }, 1)).toBe(false); // a tab scope grants no whole window
    expect(tabInScope({ tabIds: ['t1'] }, 't1', 5)).toBe(true);
    expect(tabInScope({ windowIds: [5] }, 't1', 5)).toBe(true); // window scope covers its tabs
  });
});

describe('checkMintable (no privilege escalation)', () => {
  const tree = {
    paneCoords: (id: string) =>
      ({ p1: coords('p1', 't1', 1), p2: coords('p2', 't2', 2) })[id] ?? null,
    tabWindow: (id: string) => ({ t1: 1, t2: 2 })[id] ?? null,
    hasWindow: (id: number) => id === 1 || id === 2
  };

  it('master mints any real sub-scope', () => {
    expect(checkMintable(null, { paneIds: ['p1'] }, tree)).toBeNull();
    expect(checkMintable(null, { windowIds: [2] }, tree)).toBeNull();
  });

  it('rejects unknown ids', () => {
    expect(checkMintable(null, { paneIds: ['ghost'] }, tree)).toMatch(/unknown paneId/);
    expect(checkMintable(null, { tabIds: ['nope'] }, tree)).toMatch(/unknown tabId/);
    expect(checkMintable(null, { windowIds: [9] }, tree)).toMatch(/unknown windowId/);
  });

  it('rejects an empty scope', () => {
    expect(checkMintable(null, {}, tree)).toMatch(/at least one/);
  });

  it('a window-scoped parent may mint a pane in that window, but not in another', () => {
    const parent: Scope = { windowIds: [1] };
    expect(checkMintable(parent, { paneIds: ['p1'] }, tree)).toBeNull(); // p1 ∈ window 1
    expect(checkMintable(parent, { paneIds: ['p2'] }, tree)).toMatch(/outside/); // p2 ∈ window 2
    expect(checkMintable(parent, { windowIds: [2] }, tree)).toMatch(/outside/);
  });
});

describe('coerceScope', () => {
  it('keeps well-typed arrays, drops junk, returns null when empty', () => {
    expect(coerceScope({ paneIds: ['a', 1, 'b'], tabIds: 'x', windowIds: [2, '3'] })).toEqual({
      paneIds: ['a', 'b'],
      windowIds: [2]
    });
    expect(coerceScope({ paneIds: [] })).toBeNull();
    expect(coerceScope(null)).toBeNull();
    expect(coerceScope('nope')).toBeNull();
  });
});
