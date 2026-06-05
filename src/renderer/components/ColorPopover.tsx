import { useEffect, useRef } from 'react';
import type { CSSProperties } from 'react';
import { ColorSwatches } from './ColorSwatches';

interface ColorPopoverProps {
  anchor: DOMRect;
  value: string;
  onChange: (color: string) => void;
  onClose: () => void;
}

// Fixed-positioned so it escapes the pane's overflow:hidden clipping.
export function ColorPopover({ anchor, value, onChange, onClose }: ColorPopoverProps) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [onClose]);

  const style: CSSProperties = { position: 'fixed', top: anchor.bottom + 4, left: anchor.left, zIndex: 50 };

  return (
    <div className="hp-popover" style={style} ref={ref} onMouseDown={(e) => e.stopPropagation()}>
      <ColorSwatches value={value} onChange={onChange} />
    </div>
  );
}
