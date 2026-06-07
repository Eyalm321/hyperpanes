import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useSettings } from './useSettings';

// In-memory localStorage (the test env is node, where it's otherwise undefined).
const mem: Record<string, string> = {};
const localStorageMock = {
  getItem: (k: string) => (k in mem ? mem[k] : null),
  setItem: (k: string, v: string) => {
    mem[k] = String(v);
  },
  removeItem: (k: string) => {
    delete mem[k];
  },
  clear: () => {
    for (const k of Object.keys(mem)) delete mem[k];
  }
};

beforeEach(() => {
  localStorageMock.clear();
  vi.stubGlobal('localStorage', localStorageMock);
});

const saved = () => JSON.parse(mem['hp.settings.v1']);

describe('useSettings persistence', () => {
  it('a single setter persists the WHOLE settings blob, not just its own field', () => {
    // Regression guard: persist() used to write only { defaultShell }, which would
    // silently drop every other setting.
    useSettings.getState().setDefaultShell('pwsh');
    const s = saved();
    expect(s.defaultShell).toBe('pwsh');
    expect(s).toMatchObject({
      framePalette: expect.any(String),
      terminalTheme: expect.any(String),
      fontFamily: expect.any(String),
      defaultFontSize: expect.any(Number),
      showFrame: expect.any(Boolean),
      showDot: expect.any(Boolean)
    });
  });

  it('clamps the default font size to the allowed range', () => {
    useSettings.getState().setDefaultFontSize(999);
    expect(useSettings.getState().defaultFontSize).toBe(40);
    expect(saved().defaultFontSize).toBe(40);

    useSettings.getState().setDefaultFontSize(1);
    expect(useSettings.getState().defaultFontSize).toBe(6);
  });

  it('persists the appearance toggles', () => {
    useSettings.getState().setShowFrame(false);
    useSettings.getState().setShowDot(false);
    expect(saved().showFrame).toBe(false);
    expect(saved().showDot).toBe(false);
  });

  it('persists the chosen idle glow effect', () => {
    expect(useSettings.getState().idleEffect).toBe('firefly');
    useSettings.getState().setIdleEffect('blink');
    expect(useSettings.getState().idleEffect).toBe('blink');
    expect(saved().idleEffect).toBe('blink');
  });

  it('defaults the sidebar on and persists toggling it off', () => {
    expect(useSettings.getState().showSidebar).toBe(true);
    useSettings.getState().setShowSidebar(false);
    expect(useSettings.getState().showSidebar).toBe(false);
    expect(saved().showSidebar).toBe(false);
  });
});
