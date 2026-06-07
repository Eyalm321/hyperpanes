import { useEffect, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';
import { useUI } from '../store/useUI';
import { activeGroup, useWorkspace } from '../store/useWorkspace';
import { nextColor } from '../theme';
import { ColorSwatches } from './ColorSwatches';
import { ShellPicker } from './ShellPicker';

export function NewPaneDialog() {
  const open = useUI((s) => s.newPaneOpen);
  const close = useUI((s) => s.closeNewPane);
  const addPane = useWorkspace((s) => s.addPane);
  const seq = useWorkspace((s) => activeGroup(s).seq);

  const [label, setLabel] = useState('');
  const [color, setColor] = useState(nextColor(seq));
  const [showFrame, setShowFrame] = useState(false);
  const [showDot, setShowDot] = useState(false);
  const [command, setCommand] = useState('');
  const [cwd, setCwd] = useState('');
  const [shell, setShell] = useState('');

  // Reset the form each time it opens, defaulting the color to the next in rotation.
  // Frame and dot both start off — a fresh pane is clean unless you opt in.
  useEffect(() => {
    if (open) {
      setLabel('');
      setColor(nextColor(activeGroup(useWorkspace.getState()).seq));
      setShowFrame(false);
      setShowDot(false);
      setCommand('');
      setCwd('');
      setShell('');
    }
  }, [open]);

  if (!open) return null;

  const submit = () => {
    addPane({
      label: label.trim() || `pane ${activeGroup(useWorkspace.getState()).seq + 1}`,
      color,
      showFrame,
      showDot,
      command: command.trim() || undefined,
      cwd: cwd.trim() || undefined,
      shell: shell.trim() || undefined
    });
    close();
  };

  const onKeyDown = (e: ReactKeyboardEvent) => {
    if (e.key === 'Enter') submit();
    else if (e.key === 'Escape') close();
  };

  return (
    <div className="hp-modal-backdrop" onMouseDown={close}>
      <div className="hp-modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="hp-modal-title">New pane</div>

        <label className="hp-field">
          <span>Label</span>
          <input
            autoFocus
            value={label}
            placeholder={`pane ${seq + 1}`}
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>

        <div className="hp-field">
          <span>Color</span>
          <ColorSwatches value={color} onChange={setColor} />
        </div>

        <div className="hp-field">
          <span>Show</span>
          <div style={{ display: 'flex', gap: '16px' }}>
            <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer' }}>
              <input
                type="checkbox"
                checked={showFrame}
                onChange={(e) => setShowFrame(e.target.checked)}
              />
              Frame color
            </label>
            <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer' }}>
              <input type="checkbox" checked={showDot} onChange={(e) => setShowDot(e.target.checked)} />
              Color dot
            </label>
          </div>
        </div>

        <label className="hp-field">
          <span>
            Command <em>(optional)</em>
          </span>
          <input
            value={command}
            placeholder="leave empty for an interactive shell"
            onChange={(e) => setCommand(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>

        <label className="hp-field">
          <span>
            Working directory <em>(optional)</em>
          </span>
          <input
            value={cwd}
            placeholder="defaults to home"
            onChange={(e) => setCwd(e.target.value)}
            onKeyDown={onKeyDown}
          />
        </label>

        <div className="hp-field">
          <span>
            Shell <em>(optional)</em>
          </span>
          <ShellPicker value={shell} onChange={setShell} defaultLabel="Use default shell" />
        </div>

        <div className="hp-modal-actions">
          <button className="hp-btn" onClick={close}>
            Cancel
          </button>
          <button className="hp-btn hp-btn-primary" onClick={submit}>
            Create pane
          </button>
        </div>
      </div>
    </div>
  );
}
