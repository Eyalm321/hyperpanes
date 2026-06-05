import type { CSSProperties, MouseEvent as ReactMouseEvent, RefObject } from 'react';
import { activeGroup, useWorkspace } from '../store/useWorkspace';
import { clampFraction, resizeAt } from '../layout/sizes';
import { effectiveLayout, type DividerDesc } from '../layout/presets';

interface DividerProps {
  desc: DividerDesc;
  containerRef: RefObject<HTMLDivElement>;
}

export function Divider({ desc, containerRef }: DividerProps) {
  const onMouseDown = (e: ReactMouseEvent) => {
    e.preventDefault();
    const container = containerRef.current;
    if (!container) return;
    const bounds = container.getBoundingClientRect();
    const size = desc.orientation === 'vertical' ? bounds.width : bounds.height;
    if (size === 0) return;

    const move = (ev: MouseEvent) => {
      const px = desc.orientation === 'vertical' ? ev.movementX : ev.movementY;
      const delta = px / size;
      if (delta === 0) return;
      const st = useWorkspace.getState();
      const g = activeGroup(st);
      // Resizing in auto promotes it to the concrete preset it was showing, so
      // the dragged sizes stick instead of being reset to equal on next add (Q7).
      if (g.layout === 'auto') st.setLayout(effectiveLayout(g.layout, g.panes.length));
      if (desc.kind === 'main') {
        st.setMainFraction(clampFraction(g.mainFraction + delta));
      } else {
        st.setSizes(resizeAt(g.sizes, desc.index, delta));
      }
    };
    const up = () => {
      window.removeEventListener('mousemove', move);
      window.removeEventListener('mouseup', up);
      document.body.style.userSelect = '';
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', up);
    document.body.style.userSelect = 'none';
  };

  const style: CSSProperties =
    desc.orientation === 'vertical'
      ? { left: `${desc.at * 100}%`, top: 0, height: '100%', width: 10, transform: 'translateX(-50%)' }
      : { top: `${desc.at * 100}%`, left: 0, width: '100%', height: 10, transform: 'translateY(-50%)' };

  return (
    <div
      className={`hp-divider hp-divider-${desc.orientation}`}
      style={style}
      onMouseDown={onMouseDown}
    />
  );
}
