import { useEffect, useRef, useState } from 'react';
import { useUI } from '../store/useUI';
import { activeGroup, DEFAULT_FONT_SIZE, paneFontSize, useWorkspace } from '../store/useWorkspace';
import { useKeybindings } from '../store/useKeybindings';
import { useSettings } from '../store/useSettings';
import { ShellPicker } from './ShellPicker';
import { FontPicker } from './FontPicker';
import { PALETTE_LABELS, PALETTE_NAMES, paletteColors, type PaletteName } from '../theme';
import {
  TERMINAL_THEME_LABELS,
  TERMINAL_THEME_NAMES,
  type TerminalThemeName
} from './terminal-themes';
import {
  BINDING_DEFS,
  CATEGORY_ORDER,
  comboEquals,
  comboFromEvent,
  comboParts,
  type Combo
} from '../keybindings';

type Tab = 'appearance' | 'general' | 'keybindings';

function KeyCombo({ combo }: { combo: Combo }) {
  return (
    <span className="hp-kb-keys">
      {comboParts(combo).map((p, i) => (
        <kbd key={i}>{p}</kbd>
      ))}
    </span>
  );
}

function Toggle({ on, onToggle, label }: { on: boolean; onToggle: () => void; label: string }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      className={`hp-switch${on ? ' on' : ''}`}
      onClick={onToggle}
    >
      <span className="hp-switch-knob" />
    </button>
  );
}

export function PreferencesDialog() {
  const open = useUI((s) => s.preferencesOpen);
  const closePreferences = useUI((s) => s.closePreferences);

  // Font size acts on the active tab's focused pane (zoom is per-pane).
  const focusedId = useWorkspace((s) => activeGroup(s).focusedId);
  const fontSize = useWorkspace((s) => (focusedId ? paneFontSize(s, focusedId) : DEFAULT_FONT_SIZE));
  const zoomPane = useWorkspace((s) => s.zoomPane);
  const resetPaneZoom = useWorkspace((s) => s.resetPaneZoom);

  const defaultShell = useSettings((s) => s.defaultShell);
  const setDefaultShell = useSettings((s) => s.setDefaultShell);
  const clickablePaths = useSettings((s) => s.clickablePaths);
  const setClickablePaths = useSettings((s) => s.setClickablePaths);
  const editorCommand = useSettings((s) => s.editorCommand);
  const setEditorCommand = useSettings((s) => s.setEditorCommand);

  // Appearance settings.
  const framePalette = useSettings((s) => s.framePalette);
  const setFramePalette = useSettings((s) => s.setFramePalette);
  const remapPalette = useWorkspace((s) => s.remapPalette);
  const terminalTheme = useSettings((s) => s.terminalTheme);
  const setTerminalTheme = useSettings((s) => s.setTerminalTheme);
  const fontFamily = useSettings((s) => s.fontFamily);
  const setFontFamily = useSettings((s) => s.setFontFamily);
  const defaultFontSize = useSettings((s) => s.defaultFontSize);
  const setDefaultFontSize = useSettings((s) => s.setDefaultFontSize);
  const showFrame = useSettings((s) => s.showFrame);
  const setShowFrame = useSettings((s) => s.setShowFrame);
  const showDot = useSettings((s) => s.showDot);
  const setShowDot = useSettings((s) => s.setShowDot);
  const idleAlert = useSettings((s) => s.idleAlert);
  const setIdleAlert = useSettings((s) => s.setIdleAlert);
  const idleAlertSeconds = useSettings((s) => s.idleAlertSeconds);
  const setIdleAlertSeconds = useSettings((s) => s.setIdleAlertSeconds);

  // Switch palettes and repaint existing panes by slot (custom colors are kept).
  // Re-selecting the active palette is allowed: it re-applies, healing any color
  // saved under an older palette definition.
  const choosePalette = (name: PaletteName) => {
    remapPalette(name);
    setFramePalette(name);
  };

  const platform = (typeof window !== 'undefined' && window.hp?.platform) || 'win32';
  const defaultShellLabel =
    platform === 'win32' ? 'System default (cmd)' : 'System default ($SHELL)';

  const combos = useKeybindings((s) => s.combos);
  const resetCombo = useKeybindings((s) => s.resetCombo);
  const resetAll = useKeybindings((s) => s.resetAll);

  const [tab, setTab] = useState<Tab>('keybindings');
  const [recordingId, setRecordingId] = useState<string | null>(null);
  const [conflict, setConflict] = useState<string | null>(null);
  const recordingRef = useRef<string | null>(null);

  const stopRecording = () => {
    recordingRef.current = null;
    setRecordingId(null);
    setConflict(null);
  };
  const startRecording = (id: string) => {
    recordingRef.current = id;
    setRecordingId(id);
    setConflict(null);
  };
  const close = () => {
    stopRecording();
    closePreferences();
  };

  // While open, Preferences owns the keyboard: it records combos (when a row is
  // armed) and otherwise closes on Escape. App's global shortcuts bail out while
  // preferencesOpen, so nothing else reacts to these keystrokes.
  useEffect(() => {
    if (!open) return;
    setTab('keybindings');
    const onKey = (e: KeyboardEvent) => {
      const recId = recordingRef.current;
      if (recId) {
        e.preventDefault();
        e.stopPropagation();
        if (e.key === 'Escape') {
          stopRecording();
          return;
        }
        const combo = comboFromEvent(e);
        if (!combo) return; // bare modifier — keep waiting
        const current = useKeybindings.getState().combos;
        const clash = BINDING_DEFS.find((d) => d.id !== recId && comboEquals(current[d.id], combo));
        if (clash) {
          setConflict(clash.label);
          return;
        }
        useKeybindings.getState().setCombo(recId, combo);
        stopRecording();
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        close();
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  if (!open) return null;

  const renderRow = (id: string, label: string) => {
    const isRecording = recordingId === id;
    return (
      <div className="hp-kb-row" key={id}>
        <span className="hp-kb-label">{label}</span>
        <div className="hp-kb-right">
          {isRecording ? (
            <span className="hp-kb-combo recording">
              {conflict ? `Used by ${conflict}` : 'Press keys…'}
            </span>
          ) : (
            <button
              className="hp-kb-combo"
              title="Click to rebind"
              onClick={() => startRecording(id)}
            >
              <KeyCombo combo={combos[id]} />
            </button>
          )}
          <button
            className="hp-kb-act"
            title={isRecording ? 'Stop recording (Esc)' : 'Rebind'}
            onClick={() => (isRecording ? stopRecording() : startRecording(id))}
          >
            {isRecording ? '×' : '✎'}
          </button>
          <button className="hp-kb-act" title="Reset to default" onClick={() => resetCombo(id)}>
            ↺
          </button>
        </div>
      </div>
    );
  };

  return (
    <div className="hp-modal-backdrop" onMouseDown={close}>
      <div className="hp-prefs" onMouseDown={(e) => e.stopPropagation()}>
        <div className="hp-prefs-tabs">
          <div className="hp-prefs-title">Preferences</div>
          <button
            className={`hp-prefs-tab${tab === 'general' ? ' active' : ''}`}
            onClick={() => setTab('general')}
          >
            General
          </button>
          <button
            className={`hp-prefs-tab${tab === 'keybindings' ? ' active' : ''}`}
            onClick={() => setTab('keybindings')}
          >
            Keybindings
          </button>
          <button
            className={`hp-prefs-tab${tab === 'appearance' ? ' active' : ''}`}
            onClick={() => setTab('appearance')}
          >
            Appearance
          </button>
        </div>

        <div className="hp-prefs-main">
          <div className="hp-prefs-body">
            {tab === 'appearance' && (
              <>
                <div className="hp-kb-group">
                  <div className="hp-kb-group-title">Frame color palette</div>
                  <div className="hp-pal-list">
                    {PALETTE_NAMES.map((name) => (
                      <button
                        key={name}
                        type="button"
                        className={`hp-pal-opt${framePalette === name ? ' active' : ''}`}
                        onClick={() => choosePalette(name)}
                      >
                        <span className="hp-pal-name">{PALETTE_LABELS[name]}</span>
                        <span className="hp-pal-chips">
                          {paletteColors(name).map((c) => (
                            <span key={c} className="hp-pal-chip" style={{ background: c }} />
                          ))}
                        </span>
                        <span className="hp-pal-check">{framePalette === name ? '✓' : ''}</span>
                      </button>
                    ))}
                  </div>
                  <div className="hp-kb-hint">
                    The dot &amp; frame color of each pane. Switching repaints existing panes by
                    slot; hand-picked custom colors are kept.
                  </div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">
                      Show pane frame
                      <em className="hp-kb-hint">colored border &amp; header tint</em>
                    </span>
                    <div className="hp-kb-right">
                      <Toggle
                        on={showFrame}
                        onToggle={() => setShowFrame(!showFrame)}
                        label="Show pane frame"
                      />
                    </div>
                  </div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">
                      Show color dot
                      <em className="hp-kb-hint">header dot — also the color picker</em>
                    </span>
                    <div className="hp-kb-right">
                      <Toggle
                        on={showDot}
                        onToggle={() => setShowDot(!showDot)}
                        label="Show color dot"
                      />
                    </div>
                  </div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">
                      Idle glow for AI panes
                      <em className="hp-kb-hint">
                        soft pulse when an agent (claude, etc.) goes quiet
                      </em>
                    </span>
                    <div className="hp-kb-right">
                      <Toggle
                        on={idleAlert}
                        onToggle={() => setIdleAlert(!idleAlert)}
                        label="Idle glow for AI panes"
                      />
                    </div>
                  </div>
                  {idleAlert && (
                    <div className="hp-kb-row">
                      <span className="hp-kb-label">
                        Idle after
                        <em className="hp-kb-hint">seconds of silence before it glows</em>
                      </span>
                      <div className="hp-kb-right">
                        <button
                          className="hp-kb-act"
                          title="Shorter"
                          onClick={() => setIdleAlertSeconds(idleAlertSeconds - 1)}
                        >
                          −
                        </button>
                        <span className="hp-kb-combo static">{idleAlertSeconds}s</span>
                        <button
                          className="hp-kb-act"
                          title="Longer"
                          onClick={() => setIdleAlertSeconds(idleAlertSeconds + 1)}
                        >
                          +
                        </button>
                      </div>
                    </div>
                  )}
                </div>

                <div className="hp-kb-group">
                  <div className="hp-kb-group-title">Terminal</div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">Color theme</span>
                    <div className="hp-kb-right">
                      <select
                        className="hp-select hp-prefs-select"
                        value={terminalTheme}
                        onChange={(e) => setTerminalTheme(e.target.value as TerminalThemeName)}
                      >
                        {TERMINAL_THEME_NAMES.map((n) => (
                          <option key={n} value={n}>
                            {TERMINAL_THEME_LABELS[n]}
                          </option>
                        ))}
                      </select>
                    </div>
                  </div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">
                      Font family
                      <em className="hp-kb-hint">applies to all panes</em>
                    </span>
                    <div className="hp-kb-right">
                      <FontPicker value={fontFamily} onChange={setFontFamily} />
                    </div>
                  </div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">
                      Default font size
                      <em className="hp-kb-hint">new &amp; un-zoomed panes</em>
                    </span>
                    <div className="hp-kb-right">
                      <button
                        className="hp-kb-act"
                        title="Smaller"
                        onClick={() => setDefaultFontSize(defaultFontSize - 1)}
                      >
                        −
                      </button>
                      <span className="hp-kb-combo static">{defaultFontSize}px</span>
                      <button
                        className="hp-kb-act"
                        title="Larger"
                        onClick={() => setDefaultFontSize(defaultFontSize + 1)}
                      >
                        +
                      </button>
                    </div>
                  </div>
                </div>
              </>
            )}

            {tab === 'general' && (
              <div className="hp-kb-group">
                <div className="hp-kb-group-title">Terminal</div>
                <div className="hp-kb-row">
                  <span className="hp-kb-label">Font size (focused pane)</span>
                  <div className="hp-kb-right">
                    <button
                      className="hp-kb-act"
                      title="Smaller"
                      disabled={!focusedId}
                      onClick={() => focusedId && zoomPane(focusedId, -1)}
                    >
                      −
                    </button>
                    <span className="hp-kb-combo static">{fontSize}px</span>
                    <button
                      className="hp-kb-act"
                      title="Larger"
                      disabled={!focusedId}
                      onClick={() => focusedId && zoomPane(focusedId, 1)}
                    >
                      +
                    </button>
                    <button
                      className="hp-kb-act"
                      title="Reset to default"
                      disabled={!focusedId}
                      onClick={() => focusedId && resetPaneZoom(focusedId)}
                    >
                      ↺
                    </button>
                  </div>
                </div>
                <div className="hp-kb-row">
                  <span className="hp-kb-label">
                    Default shell
                    <em className="hp-kb-hint">applies to new panes &amp; restarts</em>
                  </span>
                  <div className="hp-kb-right">
                    <ShellPicker
                      value={defaultShell}
                      onChange={setDefaultShell}
                      defaultLabel={defaultShellLabel}
                    />
                  </div>
                </div>
                <div className="hp-kb-group-title">Clickable paths</div>
                <div className="hp-kb-row">
                  <span className="hp-kb-label">
                    Clickable file paths
                    <em className="hp-kb-hint">click a path in output to open · Ctrl+click to copy</em>
                  </span>
                  <div className="hp-kb-right">
                    <Toggle
                      on={clickablePaths}
                      onToggle={() => setClickablePaths(!clickablePaths)}
                      label="Clickable file paths"
                    />
                  </div>
                </div>
                {clickablePaths && (
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">
                      Editor command
                      <em className="hp-kb-hint">blank = auto-detect VS Code, else OS default</em>
                    </span>
                    <div className="hp-kb-right">
                      <input
                        className="hp-input"
                        type="text"
                        spellCheck={false}
                        placeholder="code -g {path}:{line}:{col}"
                        style={{ width: 260 }}
                        value={editorCommand}
                        onChange={(e) => setEditorCommand(e.target.value)}
                      />
                    </div>
                  </div>
                )}
              </div>
            )}

            {tab === 'keybindings' &&
              CATEGORY_ORDER.map((cat) => {
                const rows = BINDING_DEFS.filter((d) => d.category === cat);
                if (rows.length === 0) return null;
                return (
                  <div className="hp-kb-group" key={cat}>
                    <div className="hp-kb-group-title">{cat}</div>
                    {rows.map((d) => renderRow(d.id, d.label))}
                    {cat === 'Panes' && (
                      <div className="hp-kb-row">
                        <span className="hp-kb-label">Focus pane by number</span>
                        <div className="hp-kb-right">
                          <span className="hp-kb-combo static">
                            <kbd>Alt</kbd>
                            <kbd>1</kbd>…<kbd>9</kbd>
                          </span>
                        </div>
                      </div>
                    )}
                  </div>
                );
              })}
          </div>

          <div className="hp-prefs-footer">
            {tab === 'keybindings' && (
              <button className="hp-btn" onClick={resetAll}>
                Reset all to defaults
              </button>
            )}
            <span className="hp-spacer" />
            <button className="hp-btn hp-btn-primary" onClick={close}>
              Done
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
