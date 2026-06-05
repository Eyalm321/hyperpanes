import { create } from 'zustand';
import { DEFAULT_PALETTE, type PaletteName } from '../theme';
import { DEFAULT_TERMINAL_THEME, type TerminalThemeName } from '../components/terminal-themes';
import { DEFAULT_IDLE_EFFECT, type IdleEffectName } from '../components/idle-effects';

const STORAGE_KEY = 'hp.settings.v1';

// Default terminal font size for un-zoomed panes. Mirrors useWorkspace's
// DEFAULT_FONT_SIZE; duplicated as a literal here to avoid a store import cycle
// (useWorkspace imports this module).
const DEFAULT_FONT_SIZE = 13;
const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 40;

// Persisted app-wide preferences (distinct from useUI's transient modal state).
// Kept deliberately small; each field has a "use the built-in default" empty
// value so absence never breaks an older saved blob.
export interface Settings {
  // Default pty shell for new panes that don't set their own (e.g. 'pwsh',
  // 'powershell', 'cmd', '/bin/zsh'). Empty = the system default (COMSPEC /
  // $SHELL, resolved in the main process).
  defaultShell: string;
  // Active frame-color palette for pane dots/borders. Switching it remaps
  // existing panes by slot (see useWorkspace.remapPalette).
  framePalette: PaletteName;
  // Active terminal color theme (the terminal's own bg/fg/ANSI colors).
  terminalTheme: TerminalThemeName;
  // Terminal font family. Empty = the built-in default stack.
  fontFamily: string;
  // Default terminal font size for panes that haven't been individually zoomed.
  defaultFontSize: number;
  // Whether each pane shows its colored frame border + header tint.
  showFrame: boolean;
  // Whether each pane shows its color dot (which is also the color picker).
  showDot: boolean;
  // Whether file paths in terminal output are clickable (click opens, Ctrl+click
  // copies the resolved absolute path).
  clickablePaths: boolean;
  // Command template used to open a clicked path. Empty = auto-detect VS Code,
  // else the OS default handler. Placeholders: {path} {line} {col}.
  editorCommand: string;
  // Whether AI/agent panes glow (a soft pulse on the frame) once they've been
  // quiet for idleAlertSeconds — i.e. the agent finished and is waiting.
  idleAlert: boolean;
  // Which glow effect plays when a pane goes idle (firefly / pulse / blink /
  // solid) — see components/idle-effects.
  idleEffect: IdleEffectName;
  // Seconds of pty silence before an AI pane is considered idle and starts to
  // glow. A bit longer avoids false alarms while the agent briefly pauses.
  idleAlertSeconds: number;
}

const DEFAULTS: Settings = {
  defaultShell: '',
  framePalette: DEFAULT_PALETTE,
  terminalTheme: DEFAULT_TERMINAL_THEME,
  fontFamily: '',
  defaultFontSize: DEFAULT_FONT_SIZE,
  showFrame: true,
  showDot: true,
  clickablePaths: true,
  editorCommand: '',
  idleAlert: true,
  idleEffect: DEFAULT_IDLE_EFFECT,
  idleAlertSeconds: 10
};

function load(): Settings {
  if (typeof localStorage === 'undefined') return { ...DEFAULTS };
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) return { ...DEFAULTS, ...(JSON.parse(raw) as Partial<Settings>) };
  } catch {
    /* corrupt/blocked storage — fall back to defaults */
  }
  return { ...DEFAULTS };
}

function persist(settings: Settings) {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
  } catch {
    /* storage unavailable — keep in-memory only */
  }
}

// Strip the action functions off the store state, leaving only the persistable
// settings fields (so every setter writes the WHOLE blob, not just its field).
const pick = (s: SettingsState): Settings => ({
  defaultShell: s.defaultShell,
  framePalette: s.framePalette,
  terminalTheme: s.terminalTheme,
  fontFamily: s.fontFamily,
  defaultFontSize: s.defaultFontSize,
  showFrame: s.showFrame,
  showDot: s.showDot,
  clickablePaths: s.clickablePaths,
  editorCommand: s.editorCommand,
  idleAlert: s.idleAlert,
  idleEffect: s.idleEffect,
  idleAlertSeconds: s.idleAlertSeconds
});

const clampFont = (n: number) => Math.max(MIN_FONT_SIZE, Math.min(MAX_FONT_SIZE, Math.round(n)));

interface SettingsState extends Settings {
  setDefaultShell: (shell: string) => void;
  setFramePalette: (palette: PaletteName) => void;
  setTerminalTheme: (theme: TerminalThemeName) => void;
  setFontFamily: (family: string) => void;
  setDefaultFontSize: (size: number) => void;
  setShowFrame: (show: boolean) => void;
  setShowDot: (show: boolean) => void;
  setClickablePaths: (on: boolean) => void;
  setEditorCommand: (cmd: string) => void;
  setIdleAlert: (on: boolean) => void;
  setIdleEffect: (effect: IdleEffectName) => void;
  setIdleAlertSeconds: (seconds: number) => void;
}

export const useSettings = create<SettingsState>((set) => ({
  ...load(),
  setDefaultShell: (defaultShell) =>
    set((s) => {
      persist(pick({ ...s, defaultShell }));
      return { defaultShell };
    }),
  setFramePalette: (framePalette) =>
    set((s) => {
      persist(pick({ ...s, framePalette }));
      return { framePalette };
    }),
  setTerminalTheme: (terminalTheme) =>
    set((s) => {
      persist(pick({ ...s, terminalTheme }));
      return { terminalTheme };
    }),
  setFontFamily: (fontFamily) =>
    set((s) => {
      persist(pick({ ...s, fontFamily }));
      return { fontFamily };
    }),
  setDefaultFontSize: (size) =>
    set((s) => {
      const defaultFontSize = clampFont(size);
      persist(pick({ ...s, defaultFontSize }));
      return { defaultFontSize };
    }),
  setShowFrame: (showFrame) =>
    set((s) => {
      persist(pick({ ...s, showFrame }));
      return { showFrame };
    }),
  setShowDot: (showDot) =>
    set((s) => {
      persist(pick({ ...s, showDot }));
      return { showDot };
    }),
  setClickablePaths: (clickablePaths) =>
    set((s) => {
      persist(pick({ ...s, clickablePaths }));
      return { clickablePaths };
    }),
  setEditorCommand: (editorCommand) =>
    set((s) => {
      persist(pick({ ...s, editorCommand }));
      return { editorCommand };
    }),
  setIdleAlert: (idleAlert) =>
    set((s) => {
      persist(pick({ ...s, idleAlert }));
      return { idleAlert };
    }),
  setIdleEffect: (idleEffect) =>
    set((s) => {
      persist(pick({ ...s, idleEffect }));
      return { idleEffect };
    }),
  setIdleAlertSeconds: (seconds) =>
    set((s) => {
      const idleAlertSeconds = Math.max(2, Math.min(120, Math.round(seconds)));
      persist(pick({ ...s, idleAlertSeconds }));
      return { idleAlertSeconds };
    })
}));
