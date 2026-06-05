import { describe, expect, it } from 'vitest';
import { PaneLocks } from './control-lock';

describe('PaneLocks (advisory)', () => {
  it('an unlocked pane has no holder', () => {
    const locks = new PaneLocks();
    expect(locks.holder('p', 1000)).toBeNull();
  });

  it('acquire blocks a different owner until expiry', () => {
    const locks = new PaneLocks();
    const a = locks.acquire('p', 'mgrA', 1000, 5000); // holds until 6000
    expect(a).toMatchObject({ ok: true, owner: 'mgrA', expiresAt: 6000 });
    expect(locks.holder('p', 2000)).toBe('mgrA');

    // A different owner is refused while the lock is live, told who blocks.
    const b = locks.acquire('p', 'mgrB', 2000, 5000);
    expect(b).toMatchObject({ ok: false, owner: 'mgrA' });

    // After expiry the pane is free; mgrB can take it.
    expect(locks.holder('p', 7000)).toBeNull();
    expect(locks.acquire('p', 'mgrB', 7000, 1000).ok).toBe(true);
  });

  it('the holder may renew its own lock', () => {
    const locks = new PaneLocks();
    locks.acquire('p', 'mgr', 1000, 1000); // expires 2000
    const renew = locks.acquire('p', 'mgr', 1500, 1000); // extend to 2500
    expect(renew).toMatchObject({ ok: true, owner: 'mgr', expiresAt: 2500 });
  });

  it('only the holder may release; expired/absent counts as freed', () => {
    const locks = new PaneLocks();
    locks.acquire('p', 'mgr', 1000, 5000);
    expect(locks.release('p', 'intruder', 2000)).toBe(false);
    expect(locks.release('p', 'mgr', 2000)).toBe(true);
    expect(locks.holder('p', 2000)).toBeNull();
    // Releasing a free pane is a no-op success.
    expect(locks.release('free', 'anyone', 0)).toBe(true);
  });
});
