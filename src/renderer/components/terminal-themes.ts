import type { ITheme } from '@xterm/xterm';

// Terminal color themes (the terminal's OWN background/foreground/cursor and the
// 16 ANSI colors) — distinct from the pane frame palette in theme.ts. The active
// theme is an Appearance setting and is applied live to every open terminal.
// Each theme defines the full ANSI set so program output stays legible (a Light
// theme with the default dark-tuned ANSI colors would be unreadable).

export type TerminalThemeName = 'dark' | 'black' | 'light' | 'highContrast';

export const TERMINAL_THEME_NAMES: TerminalThemeName[] = ['dark', 'black', 'light', 'highContrast'];

export const TERMINAL_THEME_LABELS: Record<TerminalThemeName, string> = {
  dark: 'Dark',
  black: 'Black',
  light: 'Light',
  highContrast: 'High contrast'
};

export const DEFAULT_TERMINAL_THEME: TerminalThemeName = 'dark';

export const TERMINAL_THEMES: Record<TerminalThemeName, ITheme> = {
  // Catppuccin Mocha — the original hardcoded theme.
  dark: {
    background: '#11111b',
    foreground: '#cdd6f4',
    cursor: '#f5e0dc',
    cursorAccent: '#11111b',
    selectionBackground: '#585b70',
    black: '#45475a',
    red: '#f38ba8',
    green: '#a6e3a1',
    yellow: '#f9e2af',
    blue: '#89b4fa',
    magenta: '#f5c2e7',
    cyan: '#94e2d5',
    white: '#bac2de',
    brightBlack: '#585b70',
    brightRed: '#f38ba8',
    brightGreen: '#a6e3a1',
    brightYellow: '#f9e2af',
    brightBlue: '#89b4fa',
    brightMagenta: '#f5c2e7',
    brightCyan: '#94e2d5',
    brightWhite: '#a6adc8'
  },
  // Pure-black background (OLED-friendly).
  black: {
    background: '#000000',
    foreground: '#e6e6e6',
    cursor: '#ffffff',
    cursorAccent: '#000000',
    selectionBackground: '#3a3a3a',
    black: '#5c6370',
    red: '#ff5c57',
    green: '#5af78e',
    yellow: '#f3f99d',
    blue: '#57c7ff',
    magenta: '#ff6ac1',
    cyan: '#9aedfe',
    white: '#f1f1f0',
    brightBlack: '#686868',
    brightRed: '#ff5c57',
    brightGreen: '#5af78e',
    brightYellow: '#f3f99d',
    brightBlue: '#57c7ff',
    brightMagenta: '#ff6ac1',
    brightCyan: '#9aedfe',
    brightWhite: '#ffffff'
  },
  // Catppuccin Latte — light background with light-tuned ANSI colors.
  light: {
    background: '#eff1f5',
    foreground: '#4c4f69',
    cursor: '#dc8a78',
    cursorAccent: '#eff1f5',
    selectionBackground: '#acb0be',
    black: '#5c5f77',
    red: '#d20f39',
    green: '#40a02b',
    yellow: '#df8e1d',
    blue: '#1e66f5',
    magenta: '#ea76cb',
    cyan: '#179299',
    white: '#acb0be',
    brightBlack: '#6c6f85',
    brightRed: '#d20f39',
    brightGreen: '#40a02b',
    brightYellow: '#df8e1d',
    brightBlue: '#1e66f5',
    brightMagenta: '#ea76cb',
    brightCyan: '#179299',
    brightWhite: '#bcc0cc'
  },
  // Maximum-legibility: white on black with vivid, bright ANSI colors.
  highContrast: {
    background: '#000000',
    foreground: '#ffffff',
    cursor: '#ffff00',
    cursorAccent: '#000000',
    selectionBackground: '#5a5a5a',
    black: '#000000',
    red: '#ff5555',
    green: '#00ff00',
    yellow: '#ffff00',
    blue: '#5c5cff',
    magenta: '#ff55ff',
    cyan: '#00ffff',
    white: '#ffffff',
    brightBlack: '#888888',
    brightRed: '#ff5555',
    brightGreen: '#55ff55',
    brightYellow: '#ffff55',
    brightBlue: '#7c7cff',
    brightMagenta: '#ff7cff',
    brightCyan: '#55ffff',
    brightWhite: '#ffffff'
  }
};

// Terminal font family. Empty value = the built-in default stack.
export const DEFAULT_FONT_FAMILY = 'Consolas, Menlo, "DejaVu Sans Mono", monospace';

export interface FontOption {
  value: string;
  label: string;
}

// Quick picks for the font-family selector. Values carry a `monospace` fallback;
// anything not installed on the user's system falls back gracefully. The empty
// value is the built-in default; anything else is entered as Custom.
export const FONT_OPTIONS: FontOption[] = [
  { value: '', label: 'System default (Consolas)' },
  { value: 'Cascadia Code, monospace', label: 'Cascadia Code' },
  { value: 'Cascadia Mono, monospace', label: 'Cascadia Mono' },
  { value: 'Consolas, monospace', label: 'Consolas' },
  { value: '"Courier New", monospace', label: 'Courier New' },
  { value: '"Fira Code", monospace', label: 'Fira Code' },
  { value: '"JetBrains Mono", monospace', label: 'JetBrains Mono' },
  { value: 'Menlo, monospace', label: 'Menlo' }
];
