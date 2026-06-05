// Single source of truth for the app's keyboard shortcuts. The App-level key
// handler, the per-pane terminal, and the Preferences > Keybindings editor all
// read combos from here (via the useKeybindings store), so the displayed
// bindings can never drift from the ones that actually fire.

// A key combo. `ctrl` means "Ctrl or Cmd" so the same binding works on Windows
// and macOS (mirroring the original `e.ctrlKey || e.metaKey` checks).
export interface Combo {
  key: string; // normalized e.key: 'p', 'arrowleft', '=', '0', 'f', …
  ctrl?: boolean;
  alt?: boolean;
  shift?: boolean;
}

export interface BindingDef {
  id: string;
  label: string;
  category: string;
  defaultCombo: Combo;
}

// Categories render in this order in the editor.
export const CATEGORY_ORDER = ['General', 'Tabs', 'Panes', 'Zoom'] as const;

export const BINDING_DEFS: BindingDef[] = [
  { id: 'palette.toggle', label: 'Command palette', category: 'General', defaultCombo: { ctrl: true, shift: true, key: 'p' } },
  { id: 'tab.new', label: 'New tab', category: 'Tabs', defaultCombo: { ctrl: true, key: 't' } },
  { id: 'tab.next', label: 'Next tab', category: 'Tabs', defaultCombo: { ctrl: true, key: 'tab' } },
  { id: 'tab.prev', label: 'Previous tab', category: 'Tabs', defaultCombo: { ctrl: true, shift: true, key: 'tab' } },
  { id: 'tab.reopen', label: 'Reopen closed tab', category: 'Tabs', defaultCombo: { ctrl: true, shift: true, key: 't' } },
  { id: 'pane.focusLeft', label: 'Focus pane left', category: 'Panes', defaultCombo: { alt: true, key: 'arrowleft' } },
  { id: 'pane.focusRight', label: 'Focus pane right', category: 'Panes', defaultCombo: { alt: true, key: 'arrowright' } },
  { id: 'pane.focusUp', label: 'Focus pane up', category: 'Panes', defaultCombo: { alt: true, key: 'arrowup' } },
  { id: 'pane.focusDown', label: 'Focus pane down', category: 'Panes', defaultCombo: { alt: true, key: 'arrowdown' } },
  { id: 'pane.toggleZoom', label: 'Zoom / unzoom pane', category: 'Panes', defaultCombo: { alt: true, key: 'z' } },
  { id: 'pane.toggleFullscreen', label: 'Fullscreen pane', category: 'Panes', defaultCombo: { key: 'f11' } },
  { id: 'pane.search', label: 'Search in pane', category: 'Panes', defaultCombo: { ctrl: true, key: 'f' } },
  { id: 'zoom.in', label: 'Zoom in (font)', category: 'Zoom', defaultCombo: { ctrl: true, key: '=' } },
  { id: 'zoom.out', label: 'Zoom out (font)', category: 'Zoom', defaultCombo: { ctrl: true, key: '-' } },
  { id: 'zoom.reset', label: 'Reset zoom (font)', category: 'Zoom', defaultCombo: { ctrl: true, key: '0' } }
];

const MOD_KEYS = new Set(['Control', 'Alt', 'Shift', 'Meta']);

// Normalize a KeyboardEvent's key to a stable token, or null for a bare modifier.
function normalizeKey(e: KeyboardEvent): string | null {
  if (MOD_KEYS.has(e.key)) return null;
  if (e.key === ' ') return 'space';
  return e.key.toLowerCase();
}

// Build a combo from a pressed key, or null if only a modifier was pressed.
export function comboFromEvent(e: KeyboardEvent): Combo | null {
  const key = normalizeKey(e);
  if (key === null) return null;
  return {
    key,
    ctrl: e.ctrlKey || e.metaKey || undefined,
    alt: e.altKey || undefined,
    shift: e.shiftKey || undefined
  };
}

// Does a stored combo match a live keyboard event?
export function comboMatches(combo: Combo | undefined, e: KeyboardEvent): boolean {
  if (!combo) return false;
  const key = normalizeKey(e);
  if (key === null) return false;
  return (
    key === combo.key &&
    !!combo.ctrl === (e.ctrlKey || e.metaKey) &&
    !!combo.alt === e.altKey &&
    !!combo.shift === e.shiftKey
  );
}

export function comboEquals(a: Combo, b: Combo): boolean {
  return a.key === b.key && !!a.ctrl === !!b.ctrl && !!a.alt === !!b.alt && !!a.shift === !!b.shift;
}

const KEY_LABELS: Record<string, string> = {
  arrowleft: '←',
  arrowright: '→',
  arrowup: '↑',
  arrowdown: '↓',
  space: 'Space',
  escape: 'Esc',
  enter: 'Enter',
  tab: 'Tab'
};

function keyLabel(key: string): string {
  if (KEY_LABELS[key]) return KEY_LABELS[key];
  return key.length === 1 ? key.toUpperCase() : key.charAt(0).toUpperCase() + key.slice(1);
}

// The pieces of a combo, e.g. ['Ctrl', 'Shift', 'P'] — handy for <kbd> rendering.
export function comboParts(combo: Combo): string[] {
  const parts: string[] = [];
  if (combo.ctrl) parts.push('Ctrl');
  if (combo.alt) parts.push('Alt');
  if (combo.shift) parts.push('Shift');
  parts.push(keyLabel(combo.key));
  return parts;
}

export function comboLabel(combo: Combo): string {
  return comboParts(combo).join('+');
}
