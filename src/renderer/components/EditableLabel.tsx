import { forwardRef, useEffect, useImperativeHandle, useRef, useState } from 'react';

// Reveal text one character at a time when it CHANGES (never on first mount), so a
// fresh ambient-AI summary "types" itself in. Returns the progressively-revealed
// string plus whether it's mid-type (drives the caret).
function useTypewriter(text: string | undefined): { text: string; typing: boolean } {
  const [shown, setShown] = useState(text ?? '');
  const [typing, setTyping] = useState(false);
  const prev = useRef(text ?? '');
  useEffect(() => {
    const target = text ?? '';
    if (target === prev.current) return; // unchanged → leave it (no mount animation)
    prev.current = target;
    if (!target) {
      setShown('');
      setTyping(false);
      return;
    }
    let i = 0;
    setShown('');
    setTyping(true);
    const id = setInterval(() => {
      i += 1;
      setShown(target.slice(0, i));
      if (i >= target.length) {
        clearInterval(id);
        setTyping(false);
      }
    }, 26);
    return () => clearInterval(id);
  }, [text]);
  return { text: shown, typing };
}

interface EditableLabelProps {
  value: string;
  subtitle?: string; // user-set subtitle: editable, always wins, seeds the editor
  aiSubtitle?: string; // ambient-AI line: shown (dimmed, marked) ONLY when no user subtitle
  shellTitle?: string; // the shell's reported title (label stays locked; shown only as tooltip)
  onCommit: (value: string, subtitle: string) => void;
}

// Lets a parent (PaneFrame) start editing imperatively — e.g. the pane menu's
// "Rename…" — instead of only via double-click.
export interface EditableLabelHandle {
  start: () => void;
}

export const EditableLabel = forwardRef<EditableLabelHandle, EditableLabelProps>(function EditableLabel(
  { value, subtitle, aiSubtitle, shellTitle, onCommit },
  ref
) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const [subDraft, setSubDraft] = useState(subtitle ?? '');
  const [showSub, setShowSub] = useState(!!subtitle); // subtitle textbox visible while editing
  const titleRef = useRef<HTMLInputElement>(null);
  const subRef = useRef<HTMLInputElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (editing) titleRef.current?.select();
  }, [editing]);

  // The AI line types itself in on each update — but only when no manual subtitle
  // is set (a manual subtitle wins and shows instantly, no animation).
  const { text: aiShown, typing: aiTyping } = useTypewriter(subtitle ? undefined : aiSubtitle);

  const start = () => {
    setDraft(value);
    setSubDraft(subtitle ?? '');
    setShowSub(!!subtitle);
    setEditing(true);
  };
  // No deps array → the exposed `start` always closes over the latest value/subtitle.
  useImperativeHandle(ref, () => ({ start }));
  const commit = () => {
    const v = draft.trim();
    // Titles can't be blank — keep the prior one if cleared. Subtitle may clear.
    onCommit(v || value, subDraft.trim());
    setEditing(false);
  };
  const cancel = () => setEditing(false);

  const addSubtitle = () => {
    setShowSub(true);
    requestAnimationFrame(() => subRef.current?.focus());
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    e.stopPropagation();
    if (e.key === 'Enter') commit();
    else if (e.key === 'Escape') cancel();
  };

  if (editing) {
    return (
      <div
        ref={wrapRef}
        className="hp-label-edit"
        onMouseDown={(e) => e.stopPropagation()}
        // Commit when focus leaves the whole editor — not when hopping between
        // the title input, subtitle input, and the "+" button.
        onBlur={(e) => {
          if (!wrapRef.current?.contains(e.relatedTarget as Node)) commit();
        }}
      >
        <input
          ref={titleRef}
          className="hp-label-input"
          value={draft}
          autoFocus
          placeholder="title"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKeyDown}
        />
        {showSub ? (
          <input
            ref={subRef}
            className="hp-label-input hp-subtitle-input"
            value={subDraft}
            placeholder="subtitle"
            onChange={(e) => setSubDraft(e.target.value)}
            onKeyDown={onKeyDown}
          />
        ) : (
          <button
            type="button"
            className="hp-label-add"
            title="Add subtitle"
            onClick={addSubtitle}
          >
            ＋
          </button>
        )}
      </div>
    );
  }

  return (
    <span
      className="hp-pane-titlewrap"
      title={shellTitle ? `${value}  ·  shell: ${shellTitle}` : value}
      onMouseDown={(e) => e.stopPropagation()}
      onDoubleClick={start}
    >
      <span className="hp-pane-label">{value}</span>
      {subtitle ? (
        <span className="hp-pane-subtitle">{subtitle}</span>
      ) : (
        aiSubtitle && (
          <span
            className={`hp-pane-subtitle hp-pane-subtitle-ai${aiTyping ? ' typing' : ''}`}
            title="AI summary of this pane"
          >
            ✦ {aiShown}
          </span>
        )
      )}
    </span>
  );
});
