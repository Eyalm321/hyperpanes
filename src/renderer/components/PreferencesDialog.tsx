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
import { IDLE_EFFECT_LABELS, IDLE_EFFECT_NAMES, type IdleEffectName } from './idle-effects';
import { TerminalPreview } from './TerminalPreview';
import type { AiStatus, ControlStatus } from '../types';
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

// The appearance settings, edited as a draft inside the dialog (only the preview
// reflects them) and applied to the real settings on Done. Mirrors the relevant
// fields of Settings.
interface AppearanceDraft {
  framePalette: PaletteName;
  terminalTheme: TerminalThemeName;
  fontFamily: string;
  defaultFontSize: number;
  showFrame: boolean;
  showDot: boolean;
  idleAlert: boolean;
  idleEffect: IdleEffectName;
  idleAlertSeconds: number;
}

// Mirror useSettings' clamps so the draft shows the same bounds the setters apply.
const clampPrefFont = (n: number) => Math.max(6, Math.min(40, Math.round(n)));
const clampPrefSeconds = (n: number) => Math.max(2, Math.min(120, Math.round(n)));

// True when the draft differs from the live settings — i.e. there are un-applied
// appearance edits, so closing should prompt to save or discard.
function isAppearanceDirty(draft: AppearanceDraft | null): boolean {
  if (!draft) return false;
  const s = useSettings.getState();
  return (
    draft.framePalette !== s.framePalette ||
    draft.terminalTheme !== s.terminalTheme ||
    draft.fontFamily !== s.fontFamily ||
    draft.defaultFontSize !== s.defaultFontSize ||
    draft.showFrame !== s.showFrame ||
    draft.showDot !== s.showDot ||
    draft.idleAlert !== s.idleAlert ||
    draft.idleEffect !== s.idleEffect ||
    draft.idleAlertSeconds !== s.idleAlertSeconds
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
  const idleEffect = useSettings((s) => s.idleEffect);
  const setIdleEffect = useSettings((s) => s.setIdleEffect);
  const idleAlertSeconds = useSettings((s) => s.idleAlertSeconds);
  const setIdleAlertSeconds = useSettings((s) => s.setIdleAlertSeconds);

  // Appearance is edited as a DRAFT: the controls write here and only the preview
  // reflects them; nothing touches the real settings (or the actual panes) until
  // Done. `a` is the live view — the draft once the dialog has opened, else the
  // committed values (the one frame before the open effect snapshots them).
  const [draft, setDraft] = useState<AppearanceDraft | null>(null);
  // The "save or discard?" prompt shown when closing with un-applied edits.
  const [confirmClose, setConfirmClose] = useState(false);
  // Mirror the latest draft / confirm state for the window keydown handler, whose
  // closure is pinned to [open] and would otherwise read stale values.
  const draftRef = useRef(draft);
  draftRef.current = draft;
  const confirmCloseRef = useRef(confirmClose);
  confirmCloseRef.current = confirmClose;
  const committedAppearance: AppearanceDraft = {
    framePalette,
    terminalTheme,
    fontFamily,
    defaultFontSize,
    showFrame,
    showDot,
    idleAlert,
    idleEffect,
    idleAlertSeconds
  };
  const a = draft ?? committedAppearance;
  const patchAppearance = (p: Partial<AppearanceDraft>) =>
    setDraft((d) => ({ ...(d ?? committedAppearance), ...p }));

  const snapshotAppearance = (): AppearanceDraft => {
    const s = useSettings.getState();
    return {
      framePalette: s.framePalette,
      terminalTheme: s.terminalTheme,
      fontFamily: s.fontFamily,
      defaultFontSize: s.defaultFontSize,
      showFrame: s.showFrame,
      showDot: s.showDot,
      idleAlert: s.idleAlert,
      idleEffect: s.idleEffect,
      idleAlertSeconds: s.idleAlertSeconds
    };
  };

  // Apply the draft to the real settings (and repaint panes for a palette switch).
  // Only changed fields are written. Called from Done; other closes discard it.
  const commitAppearance = () => {
    const d = draft;
    if (!d) return;
    const s = useSettings.getState();
    if (d.framePalette !== s.framePalette) {
      remapPalette(d.framePalette); // repaint existing panes by slot (custom kept)
      setFramePalette(d.framePalette);
    }
    if (d.terminalTheme !== s.terminalTheme) setTerminalTheme(d.terminalTheme);
    if (d.fontFamily !== s.fontFamily) setFontFamily(d.fontFamily);
    if (d.defaultFontSize !== s.defaultFontSize) setDefaultFontSize(d.defaultFontSize);
    if (d.showFrame !== s.showFrame) setShowFrame(d.showFrame);
    if (d.showDot !== s.showDot) setShowDot(d.showDot);
    if (d.idleAlert !== s.idleAlert) setIdleAlert(d.idleAlert);
    if (d.idleEffect !== s.idleEffect) setIdleEffect(d.idleEffect);
    if (d.idleAlertSeconds !== s.idleAlertSeconds) setIdleAlertSeconds(d.idleAlertSeconds);
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

  // Control API status lives in the main process (loopback server, off by
  // default), so it's fetched on open rather than read from a store.
  const [control, setControl] = useState<ControlStatus | null>(null);
  useEffect(() => {
    if (!open) return;
    void window.hp.control.getStatus().then(setControl);
  }, [open]);
  const toggleControl = () => void window.hp.control.setEnabled(!control?.enabled).then(setControl);
  const toggleAllowInput = () =>
    void window.hp.control.setAllowInput(!control?.allowInput).then(setControl);

  // Ambient AI status also lives in main (ai-settings.json, off by default). Fetch
  // on open; subscribe to live status (online/offline) while the dialog is shown.
  // endpoint/model are local drafts (committed on blur/Enter) so typing isn't
  // overwritten by an incoming status push.
  const [ai, setAi] = useState<AiStatus | null>(null);
  const [aiEndpoint, setAiEndpoint] = useState('');
  const [aiModel, setAiModel] = useState('');
  useEffect(() => {
    if (!open) return;
    void window.hp.ai.getStatus().then((st) => {
      setAi(st);
      setAiEndpoint(st.endpoint);
      setAiModel(st.model);
    });
    return window.hp.ai.onStatus(setAi);
  }, [open]);
  const toggleAi = () => void window.hp.ai.setEnabled(!ai?.enabled).then(setAi);
  const commitAiEndpoint = () => {
    if (ai && aiEndpoint.trim() && aiEndpoint.trim() !== ai.endpoint)
      void window.hp.ai.configure({ endpoint: aiEndpoint.trim() }).then(setAi);
  };
  const commitAiModel = () => {
    if (ai && aiModel.trim() && aiModel.trim() !== ai.model)
      void window.hp.ai.configure({ model: aiModel.trim() }).then(setAi);
  };

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
    setConfirmClose(false);
    setDraft(null); // discard any un-applied appearance edits
    closePreferences();
  };
  // Closing with un-applied appearance edits prompts to save/discard; otherwise it
  // just closes. Used by Escape and a backdrop click.
  const requestClose = () => {
    if (isAppearanceDirty(draft)) setConfirmClose(true);
    else close();
  };

  // While open, Preferences owns the keyboard: it records combos (when a row is
  // armed) and otherwise closes on Escape. App's global shortcuts bail out while
  // preferencesOpen, so nothing else reacts to these keystrokes.
  useEffect(() => {
    if (!open) return;
    setTab('keybindings');
    setDraft(snapshotAppearance()); // start the appearance draft from live settings
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
        // If the prompt is already up, Escape cancels it (keep editing). Otherwise
        // close — but route through the save/discard prompt when there are edits.
        if (confirmCloseRef.current) setConfirmClose(false);
        else if (isAppearanceDirty(draftRef.current)) setConfirmClose(true);
        else close();
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
    <>
    <div
      className="hp-modal-backdrop hp-frosted-backdrop"
      // Only a click on the backdrop itself closes — NOT a click that bubbled up
      // from inside the panel. (We avoid stopPropagation on the panel so in-dialog
      // clicks still reach document, letting popovers like the color picker close
      // on an outside click.)
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) requestClose();
      }}
    >
      <div className="hp-prefs">
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
                <div className="hp-kb-group hp-prefs-preview-group">
                  <div className="hp-kb-group-title">Preview</div>
                  <TerminalPreview
                    terminalTheme={a.terminalTheme}
                    fontFamily={a.fontFamily}
                    fontSize={a.defaultFontSize}
                    framePalette={a.framePalette}
                    showFrame={a.showFrame}
                    showDot={a.showDot}
                    idleAlert={a.idleAlert}
                    idleEffect={a.idleEffect}
                  />
                </div>
                <div className="hp-kb-group">
                  <div className="hp-kb-group-title">Frame color palette</div>
                  <div className="hp-pal-list">
                    {PALETTE_NAMES.map((name) => (
                      <button
                        key={name}
                        type="button"
                        className={`hp-pal-opt${a.framePalette === name ? ' active' : ''}`}
                        onClick={() => patchAppearance({ framePalette: name })}
                      >
                        <span className="hp-pal-name">{PALETTE_LABELS[name]}</span>
                        <span className="hp-pal-chips">
                          {paletteColors(name).map((c) => (
                            <span key={c} className="hp-pal-chip" style={{ background: c }} />
                          ))}
                        </span>
                        <span className="hp-pal-check">{a.framePalette === name ? '✓' : ''}</span>
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
                        on={a.showFrame}
                        onToggle={() => patchAppearance({ showFrame: !a.showFrame })}
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
                        on={a.showDot}
                        onToggle={() => patchAppearance({ showDot: !a.showDot })}
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
                        on={a.idleAlert}
                        onToggle={() => patchAppearance({ idleAlert: !a.idleAlert })}
                        label="Idle glow for AI panes"
                      />
                    </div>
                  </div>
                  {a.idleAlert && (
                    <>
                      <div className="hp-kb-row">
                        <span className="hp-kb-label">
                          Glow effect
                          <em className="hp-kb-hint">how an idle pane catches your eye</em>
                        </span>
                        <div className="hp-kb-right">
                          <select
                            className="hp-select hp-prefs-select"
                            value={a.idleEffect}
                            onChange={(e) =>
                              patchAppearance({ idleEffect: e.target.value as IdleEffectName })
                            }
                          >
                            {IDLE_EFFECT_NAMES.map((n) => (
                              <option key={n} value={n}>
                                {IDLE_EFFECT_LABELS[n]}
                              </option>
                            ))}
                          </select>
                        </div>
                      </div>
                      <div className="hp-kb-row">
                        <span className="hp-kb-label">
                          Idle after
                          <em className="hp-kb-hint">seconds of silence before it glows</em>
                        </span>
                        <div className="hp-kb-right">
                          <button
                            className="hp-kb-act"
                            title="Shorter"
                            onClick={() =>
                              patchAppearance({
                                idleAlertSeconds: clampPrefSeconds(a.idleAlertSeconds - 1)
                              })
                            }
                          >
                            −
                          </button>
                          <span className="hp-kb-combo static">{a.idleAlertSeconds}s</span>
                          <button
                            className="hp-kb-act"
                            title="Longer"
                            onClick={() =>
                              patchAppearance({
                                idleAlertSeconds: clampPrefSeconds(a.idleAlertSeconds + 1)
                              })
                            }
                          >
                            +
                          </button>
                        </div>
                      </div>
                    </>
                  )}
                </div>

                <div className="hp-kb-group">
                  <div className="hp-kb-group-title">Terminal</div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">Color theme</span>
                    <div className="hp-kb-right">
                      <select
                        className="hp-select hp-prefs-select"
                        value={a.terminalTheme}
                        onChange={(e) =>
                          patchAppearance({ terminalTheme: e.target.value as TerminalThemeName })
                        }
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
                      <FontPicker
                        value={a.fontFamily}
                        onChange={(v) => patchAppearance({ fontFamily: v })}
                      />
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
                        onClick={() =>
                          patchAppearance({ defaultFontSize: clampPrefFont(a.defaultFontSize - 1) })
                        }
                      >
                        −
                      </button>
                      <span className="hp-kb-combo static">{a.defaultFontSize}px</span>
                      <button
                        className="hp-kb-act"
                        title="Larger"
                        onClick={() =>
                          patchAppearance({ defaultFontSize: clampPrefFont(a.defaultFontSize + 1) })
                        }
                      >
                        +
                      </button>
                    </div>
                  </div>
                  <div className="hp-kb-row">
                    <span className="hp-kb-label">
                      Font size (focused pane)
                      <em className="hp-kb-hint">zoom of the active pane only</em>
                    </span>
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
                </div>
              </>
            )}

            {tab === 'general' && (
              <div className="hp-kb-group">
                <div className="hp-kb-group-title">Terminal</div>
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

                <div className="hp-kb-group-title">Control API (agents / MCP)</div>
                <div className="hp-kb-row">
                  <span className="hp-kb-label">
                    Allow agent control
                    <em className="hp-kb-hint">
                      local loopback API so an MCP server can read panes &amp; layout — off by
                      default
                    </em>
                  </span>
                  <div className="hp-kb-right">
                    <Toggle
                      on={!!control?.enabled}
                      onToggle={toggleControl}
                      label="Allow agent control"
                    />
                  </div>
                </div>
                {control?.enabled && (
                  <>
                    <div className="hp-kb-row">
                      <span className="hp-kb-label">
                        Allow sending input
                        <em className="hp-kb-hint">
                          lets agents type into live shells — extra risk, off by default
                        </em>
                      </span>
                      <div className="hp-kb-right">
                        <Toggle
                          on={!!control?.allowInput}
                          onToggle={toggleAllowInput}
                          label="Allow sending input"
                        />
                      </div>
                    </div>
                    <div className="hp-kb-hint">
                      {control?.running && control?.port
                        ? `Listening on 127.0.0.1:${control.port} — token in control.json under the app's data folder.`
                        : 'Starting…'}
                    </div>
                  </>
                )}

                <div className="hp-kb-group-title">Local AI (Ollama)</div>
                <div className="hp-kb-row">
                  <span className="hp-kb-label">
                    Ambient pane summaries
                    <em className="hp-kb-hint">
                      a local Gemma watches each pane &amp; writes a high-level subtitle — off by
                      default
                    </em>
                  </span>
                  <div className="hp-kb-right">
                    <Toggle on={!!ai?.enabled} onToggle={toggleAi} label="Ambient pane summaries" />
                  </div>
                </div>
                {ai?.enabled && (
                  <>
                    <div className="hp-kb-row">
                      <span className="hp-kb-label">
                        Ollama endpoint
                        <em className="hp-kb-hint">e.g. your Mac mini over Tailscale</em>
                      </span>
                      <div className="hp-kb-right">
                        <input
                          className="hp-input"
                          type="text"
                          spellCheck={false}
                          placeholder="http://host:11434"
                          style={{ width: 260 }}
                          value={aiEndpoint}
                          onChange={(e) => setAiEndpoint(e.target.value)}
                          onBlur={commitAiEndpoint}
                          onKeyDown={(e) => {
                            if (e.key === 'Enter') commitAiEndpoint();
                          }}
                        />
                      </div>
                    </div>
                    <div className="hp-kb-row">
                      <span className="hp-kb-label">Model</span>
                      <div className="hp-kb-right">
                        <input
                          className="hp-input"
                          type="text"
                          spellCheck={false}
                          placeholder="gemma3:4b"
                          style={{ width: 160 }}
                          value={aiModel}
                          onChange={(e) => setAiModel(e.target.value)}
                          onBlur={commitAiModel}
                          onKeyDown={(e) => {
                            if (e.key === 'Enter') commitAiModel();
                          }}
                        />
                      </div>
                    </div>
                    <div className="hp-kb-hint">
                      {ai?.online
                        ? `Connected to ${ai.endpoint} (${ai.model}).`
                        : `Offline — can't reach ${ai?.endpoint}. Retrying…${
                            ai?.lastError ? ` (${ai.lastError})` : ''
                          }`}
                    </div>
                  </>
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
            <button
              className="hp-btn hp-btn-primary"
              onClick={() => {
                commitAppearance();
                close();
              }}
            >
              Done
            </button>
          </div>
        </div>
      </div>
    </div>

    {confirmClose && (
      <div
        className="hp-modal-backdrop hp-confirm-backdrop"
        onMouseDown={() => setConfirmClose(false)}
      >
        <div className="hp-modal" onMouseDown={(e) => e.stopPropagation()}>
          <div className="hp-modal-title">Unsaved appearance changes</div>
          <p className="hp-modal-text">
            You changed appearance settings but haven&apos;t applied them. Save the changes or
            discard them?
          </p>
          <div className="hp-modal-actions">
            <button className="hp-btn" onClick={() => setConfirmClose(false)}>
              Keep editing
            </button>
            <button className="hp-btn" onClick={close}>
              Discard
            </button>
            <button
              className="hp-btn hp-btn-primary"
              onClick={() => {
                commitAppearance();
                close();
              }}
            >
              Save
            </button>
          </div>
        </div>
      </div>
    )}
    </>
  );
}
