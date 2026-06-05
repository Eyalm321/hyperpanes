import { useEffect, useRef, useState } from 'react';
import type { SearchAddon } from '@xterm/addon-search';

interface SearchBoxProps {
  search: SearchAddon | null;
  onClose: () => void;
}

const DECORATIONS = {
  matchOverviewRuler: '#89b4fa',
  activeMatchColorOverviewRuler: '#f5e0dc'
};

export function SearchBox({ search, onClose }: SearchBoxProps) {
  const [query, setQuery] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const find = (back: boolean) => {
    if (!search || !query) return;
    const opts = { decorations: DECORATIONS };
    if (back) search.findPrevious(query, opts);
    else search.findNext(query, opts);
  };

  return (
    <div className="hp-search" onMouseDown={(e) => e.stopPropagation()}>
      <input
        ref={inputRef}
        className="hp-search-input"
        placeholder="Find"
        value={query}
        onChange={(e) => {
          const v = e.target.value;
          setQuery(v);
          if (!search) return;
          if (v) search.findNext(v, { incremental: true, decorations: DECORATIONS });
          else search.clearDecorations();
        }}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === 'Enter') find(e.shiftKey);
          else if (e.key === 'Escape') {
            search?.clearDecorations();
            onClose();
          }
        }}
      />
      <button className="hp-search-btn" onMouseDown={(e) => e.stopPropagation()} onClick={() => find(true)} title="Previous (Shift+Enter)">
        ↑
      </button>
      <button className="hp-search-btn" onMouseDown={(e) => e.stopPropagation()} onClick={() => find(false)} title="Next (Enter)">
        ↓
      </button>
      <button
        className="hp-search-btn"
        onMouseDown={(e) => e.stopPropagation()}
        onClick={() => {
          search?.clearDecorations();
          onClose();
        }}
        title="Close (Esc)"
      >
        ×
      </button>
    </div>
  );
}
