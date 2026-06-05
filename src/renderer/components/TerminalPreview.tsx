import { useEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties } from 'react';
import { v4 as uuid } from 'uuid';
import { Terminal as XTerm } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { useSettings } from '../store/useSettings';
import { paletteColors, type PaletteName } from '../theme';
import { DEFAULT_FONT_FAMILY, TERMINAL_THEMES, type TerminalThemeName } from './terminal-themes';
import { runIdleEffect, type IdleEffectName } from './idle-effects';
import { ColorSwatches } from './ColorSwatches';
import { ContextMenu, type MenuItem } from './ContextMenu';

// A live, real mini terminal shown in Preferences → Appearance, wrapped in pane
// chrome (frame color/border, header dot & tint, idle glow). It runs an actual
// pty — a real shell (locked / read-only) — so the chosen terminal theme, font
// and frame styling are shown on genuine output.
//
// Its appearance is driven entirely by PROPS, not the global settings store: the
// dialog feeds it the DRAFT appearance (the in-progress, not-yet-applied values),
// so changing a setting updates only this preview until the user clicks Done. Its
// own throwaway session (a fresh uid), killed when the dialog closes.
//
// Standalone (NOT <Terminal/>), so it never registers in the pane map, the idle
// store, the workspace zoom, or the WebGL budget — DOM-rendered only.

interface TerminalPreviewProps {
  terminalTheme: TerminalThemeName;
  fontFamily: string; // '' = the built-in default stack
  fontSize: number;
  framePalette: PaletteName;
  showFrame: boolean;
  showDot: boolean;
  idleAlert: boolean;
  idleEffect: IdleEffectName;
}

export function TerminalPreview({
  terminalTheme,
  fontFamily: fontFamilyProp,
  fontSize,
  framePalette,
  showFrame,
  showDot,
  idleAlert,
  idleEffect
}: TerminalPreviewProps) {
  const fontFamily = fontFamilyProp || DEFAULT_FONT_FAMILY;

  // The dot just shows a representative color from the (draft) palette — the blue
  // slot reads well on the dark UI and isn't the alarm-ish red. It can be overridden
  // (preview-only) via the right-click menu; a real pane's color is per-pane,
  // assigned when it's created, so there's nothing to persist here.
  const [previewColor, setPreviewColor] = useState<string | null>(null);
  const color = previewColor ?? paletteColors(framePalette)[3];

  // Right-clicking the header opens a cursor-anchored context menu (mirroring a
  // real pane), with Change Color nested as one category so the menu can grow.
  // Built here so the swatches stay reactive to `color` and write the local
  // preview override.
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const menuItems = useMemo<MenuItem[]>(
    () => [
      {
        kind: 'submenu',
        label: 'Change Color',
        items: [{ kind: 'custom', node: <ColorSwatches value={color} onChange={setPreviewColor} /> }]
      }
    ],
    [color]
  );

  const hostRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const uidRef = useRef<string>('');
  const [title, setTitle] = useState('');

  // Build the terminal once and spawn its own throwaway shell.
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const term = new XTerm({
      fontFamily,
      fontSize,
      // Locked: it's a preview, not an interactive shell — block keyboard input so
      // you can't type into it (and so its textarea never swallows the dialog's
      // Escape / keybinding keystrokes). A static cursor matches the read-only feel.
      disableStdin: true,
      cursorBlink: false,
      allowProposedApi: true,
      theme: TERMINAL_THEMES[terminalTheme]
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    termRef.current = term;
    fitRef.current = fit;

    const uid = `preview-${uuid()}`;
    uidRef.current = uid;

    const refit = () => {
      if (host.clientWidth === 0 || host.clientHeight === 0) return;
      try {
        fit.fit();
      } catch {
        /* xterm not ready */
      }
    };
    refit();

    // Read-only: pipe pty output in only — no keystrokes go back out (locked).
    const offData = window.hp.onData((u, data) => {
      if (u === uid) term.write(data);
    });
    const offExit = window.hp.onExit((u, code) => {
      if (u === uid) term.write(`\r\n\x1b[90m[process exited: ${code}]\x1b[0m\r\n`);
    });
    const titleDisposable = term.onTitleChange((t) => setTitle(t));

    let spawned = false;
    void window.hp
      .spawn({
        uid,
        // The shell itself isn't an appearance setting — read it live.
        shell: useSettings.getState().defaultShell || undefined,
        cols: term.cols,
        rows: term.rows
      })
      .then(() => {
        spawned = true;
        window.hp.resize(uid, term.cols, term.rows);
      })
      .catch(() => {
        /* spawn failed — the preview just stays blank */
      });

    const ro = new ResizeObserver(() => {
      refit();
      if (spawned) window.hp.resize(uid, term.cols, term.rows);
    });
    ro.observe(host);

    return () => {
      ro.disconnect();
      offData();
      offExit();
      titleDisposable.dispose();
      window.hp.kill(uid);
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
      uidRef.current = '';
    };
    // Built once; the prop-driven effects below apply later changes live.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Live-apply the color theme (colors only — no refit needed).
  useEffect(() => {
    const term = termRef.current;
    if (term) term.options.theme = TERMINAL_THEMES[terminalTheme];
  }, [terminalTheme]);

  // Live-apply font family & size — cell metrics change, so refit and resync the
  // pty to the new grid.
  useEffect(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    const host = hostRef.current;
    if (!term) return;
    term.options.fontFamily = fontFamily;
    term.options.fontSize = fontSize;
    if (fit && host && host.clientWidth > 0 && host.clientHeight > 0) {
      try {
        fit.fit();
      } catch {
        /* xterm not ready */
      }
      if (uidRef.current) window.hp.resize(uidRef.current, term.cols, term.rows);
    }
  }, [fontFamily, fontSize]);

  // The idle glow, played as a one-shot demo ONLY when the glow effect is toggled
  // or changed (this re-runs on idleAlert/idleEffect change). Deliberately skips
  // the first run, so it does NOT fire just from entering Appearance. Feature off =
  // no glow at all.
  const glowRef = useRef<HTMLDivElement>(null);
  const glowFirstRun = useRef(true);
  useEffect(() => {
    if (glowFirstRun.current) {
      glowFirstRun.current = false;
      return;
    }
    const el = glowRef.current;
    if (!el || !idleAlert) return;
    return runIdleEffect(el, idleEffect, { once: true });
  }, [idleAlert, idleEffect]);

  return (
    <div className="hp-prefs-preview">
      <div
        className="hp-pane"
        style={{ borderColor: showFrame ? color : undefined } as CSSProperties}
      >
        {idleAlert && (
          <div
            className="hp-pane-glow"
            ref={glowRef}
            style={{ ['--hp-idle-c' as string]: color } as CSSProperties}
          />
        )}
        <div
          className="hp-pane-header"
          style={{ background: showFrame ? `${color}1a` : undefined }}
          title="Right-click for options"
          onContextMenu={(e) => {
            e.preventDefault();
            e.stopPropagation();
            setMenu({ x: e.clientX, y: e.clientY });
          }}
        >
          {showDot && <span className="hp-pane-dot" style={{ background: color }} />}
          <span className="hp-pane-titlewrap">
            <span className="hp-pane-label">{title || 'preview'}</span>
          </span>
        </div>
        <div
          className="hp-pane-body"
          style={{ background: TERMINAL_THEMES[terminalTheme].background }}
        >
          <div className="hp-term-wrap">
            <div ref={hostRef} className="hp-term-host" />
          </div>
        </div>
      </div>

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          items={menuItems}
          onClose={() => setMenu(null)}
        />
      )}
    </div>
  );
}
