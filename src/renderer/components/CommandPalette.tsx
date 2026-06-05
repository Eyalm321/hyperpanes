import { useEffect, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';
import { useUI } from '../store/useUI';
import { buildCommands, type Command } from '../commands/registry';
import { fuzzyScore } from '../commands/fuzzy';

export function CommandPalette() {
  const open = useUI((s) => s.paletteOpen);
  const close = useUI((s) => s.closePalette);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  // Snapshot commands when the palette opens (fresh pane/layout state).
  const commands = useMemo<Command[]>(() => (open ? buildCommands() : []), [open]);

  const results = useMemo<{ c: Command; score: number }[]>(() => {
    if (!query) return commands.map((c) => ({ c, score: 0 }));
    const out: { c: Command; score: number }[] = [];
    for (const c of commands) {
      const score = fuzzyScore(query, `${c.title} ${c.keywords ?? ''}`);
      if (score !== null) out.push({ c, score });
    }
    out.sort((a, b) => b.score - a.score);
    return out;
  }, [commands, query]);

  useEffect(() => {
    if (open) setQuery('');
  }, [open]);

  useEffect(() => {
    setActive(0);
  }, [query]);

  useEffect(() => {
    listRef.current?.querySelector('.hp-palette-item.active')?.scrollIntoView({ block: 'nearest' });
  }, [active, results]);

  if (!open) return null;

  const run = (i: number) => {
    const r = results[i];
    if (r) {
      r.c.run();
      close();
    }
  };

  const onKeyDown = (e: ReactKeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setActive((a) => Math.min(a + 1, results.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setActive((a) => Math.max(a - 1, 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      run(active);
    } else if (e.key === 'Escape') {
      e.preventDefault();
      close();
    }
  };

  return (
    <div className="hp-modal-backdrop hp-palette-backdrop" onMouseDown={close}>
      <div className="hp-palette" onMouseDown={(e) => e.stopPropagation()}>
        <input
          className="hp-palette-input"
          autoFocus
          placeholder="Type a command…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className="hp-palette-list" ref={listRef}>
          {results.length === 0 && <div className="hp-palette-empty">No matching commands</div>}
          {results.map((r, i) => (
            <div
              key={r.c.id}
              className={`hp-palette-item${i === active ? ' active' : ''}`}
              onMouseMove={() => setActive(i)}
              onMouseDown={(e) => {
                e.preventDefault();
                run(i);
              }}
            >
              <span className="hp-palette-title">{r.c.title}</span>
              {r.c.subtitle && <span className="hp-palette-subtitle">{r.c.subtitle}</span>}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
