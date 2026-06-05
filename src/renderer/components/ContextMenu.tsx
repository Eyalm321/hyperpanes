import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';

// One row of a context menu. Built as data (see contextMenus.tsx) and rendered by
// <ContextMenu>, so the same component serves the tab, pane-header and taskbar
// menus. 'custom' drops an arbitrary node in (used for the color swatches).
export type MenuItem =
  | {
      kind: 'item';
      label: string;
      shortcut?: string;
      icon?: ReactNode;
      glyph?: string;
      danger?: boolean;
      disabled?: boolean;
      checked?: boolean;
      onSelect: () => void;
    }
  | { kind: 'submenu'; label: string; glyph?: string; icon?: ReactNode; items: MenuItem[] }
  | { kind: 'sep' }
  | { kind: 'custom'; node: ReactNode };

interface ContextMenuProps {
  x: number;
  y: number;
  items: MenuItem[];
  onClose: () => void;
}

// A cursor-anchored menu. Reuses the hamburger menu's .hp-menu styling and the
// CSS-only .hp-submenu flyout; adds viewport clamping (the cursor can be near any
// edge) and the usual dismiss-on-outside/Escape/scroll/blur handling.
export function ContextMenu({ x, y, items, onClose }: ContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x, y });

  // Clamp into the viewport once we can measure the menu.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const { width, height } = el.getBoundingClientRect();
    let nx = x;
    let ny = y;
    if (nx + width > window.innerWidth - 4) nx = Math.max(4, window.innerWidth - width - 4);
    if (ny + height > window.innerHeight - 4) ny = Math.max(4, window.innerHeight - height - 4);
    setPos({ x: nx, y: ny });
  }, [x, y, items]);

  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.stopPropagation();
        onClose();
      }
    };
    document.addEventListener('mousedown', onDown, true);
    document.addEventListener('keydown', onKey, true);
    window.addEventListener('scroll', onClose, true);
    window.addEventListener('resize', onClose);
    window.addEventListener('blur', onClose);
    return () => {
      document.removeEventListener('mousedown', onDown, true);
      document.removeEventListener('keydown', onKey, true);
      window.removeEventListener('scroll', onClose, true);
      window.removeEventListener('resize', onClose);
      window.removeEventListener('blur', onClose);
    };
  }, [onClose]);

  // Flyouts default to opening rightward (left:100%). When the menu sits in the
  // right of the viewport that would overflow, so open them leftward instead.
  const openLeft = pos.x > window.innerWidth * 0.6;

  return (
    <div
      ref={ref}
      className="hp-menu hp-menu--context"
      role="menu"
      style={{ position: 'fixed', left: pos.x, top: pos.y }}
      onContextMenu={(e) => e.preventDefault()}
    >
      {items.map((item, i) => (
        <MenuRow key={i} item={item} onClose={onClose} openLeft={openLeft} />
      ))}
    </div>
  );
}

function MenuRow({
  item,
  onClose,
  openLeft
}: {
  item: MenuItem;
  onClose: () => void;
  openLeft: boolean;
}) {
  if (item.kind === 'sep') return <div className="hp-menu-sep" />;
  if (item.kind === 'custom') return <div className="hp-menu-custom">{item.node}</div>;

  if (item.kind === 'submenu') {
    return (
      <div
        className="hp-menu-item hp-has-submenu"
        role="menuitem"
        aria-haspopup="true"
        tabIndex={0}
      >
        {item.glyph != null && <span className="hp-menu-glyph">{item.glyph}</span>}
        {item.icon}
        <span>{item.label}</span>
        <span className="hp-menu-shortcut">▸</span>
        <div className={`hp-submenu${openLeft ? ' hp-submenu--left' : ''}`} role="menu">
          {item.items.map((sub, i) => (
            <MenuRow key={i} item={sub} onClose={onClose} openLeft={openLeft} />
          ))}
        </div>
      </div>
    );
  }

  return (
    <button
      className={`hp-menu-item${item.checked ? ' active' : ''}${item.danger ? ' hp-danger' : ''}`}
      role="menuitem"
      disabled={item.disabled}
      onClick={() => {
        if (item.disabled) return;
        item.onSelect();
        onClose();
      }}
    >
      {item.glyph != null && <span className="hp-menu-glyph">{item.glyph}</span>}
      {item.icon}
      <span>{item.label}</span>
      {item.shortcut != null && <span className="hp-menu-shortcut">{item.shortcut}</span>}
      {item.checked && <span className="hp-menu-radio">✓</span>}
    </button>
  );
}
