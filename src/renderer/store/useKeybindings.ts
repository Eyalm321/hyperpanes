import { create } from 'zustand';
import { BINDING_DEFS, type Combo } from '../keybindings';

const STORAGE_KEY = 'hp.keybindings.v1';

function defaults(): Record<string, Combo> {
  const out: Record<string, Combo> = {};
  for (const d of BINDING_DEFS) out[d.id] = d.defaultCombo;
  return out;
}

// Defaults overlaid with any persisted overrides. Merging onto fresh defaults
// means bindings added in later versions appear even for existing users.
function load(): Record<string, Combo> {
  const base = defaults();
  if (typeof localStorage === 'undefined') return base;
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const saved = JSON.parse(raw) as Record<string, Combo>;
      for (const id in saved) if (id in base) base[id] = saved[id];
    }
  } catch {
    /* corrupt/blocked storage — fall back to defaults */
  }
  return base;
}

function persist(combos: Record<string, Combo>) {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(combos));
  } catch {
    /* storage unavailable — keep in-memory only */
  }
}

interface KeybindingsState {
  combos: Record<string, Combo>;
  setCombo: (id: string, combo: Combo) => void;
  resetCombo: (id: string) => void;
  resetAll: () => void;
}

export const useKeybindings = create<KeybindingsState>((set) => ({
  combos: load(),
  setCombo: (id, combo) =>
    set((s) => {
      const combos = { ...s.combos, [id]: combo };
      persist(combos);
      return { combos };
    }),
  resetCombo: (id) =>
    set((s) => {
      const def = BINDING_DEFS.find((d) => d.id === id);
      if (!def) return s;
      const combos = { ...s.combos, [id]: def.defaultCombo };
      persist(combos);
      return { combos };
    }),
  resetAll: () =>
    set(() => {
      const combos = defaults();
      persist(combos);
      return { combos };
    })
}));
