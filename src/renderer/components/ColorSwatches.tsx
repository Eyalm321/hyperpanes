import { paletteColors } from '../theme';
import { useSettings } from '../store/useSettings';

interface ColorSwatchesProps {
  value: string;
  onChange: (color: string) => void;
  // Optional per-pane frame/dot toggles. When the on/toggle pairs are supplied
  // (the post-creation popover and right-click color menu) two checkboxes render
  // below the swatches so the pane's frame and dot can be turned on/off here too.
  frameOn?: boolean;
  dotOn?: boolean;
  onToggleFrame?: (on: boolean) => void;
  onToggleDot?: (on: boolean) => void;
}

const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

export function ColorSwatches({
  value,
  onChange,
  frameOn,
  dotOn,
  onToggleFrame,
  onToggleDot
}: ColorSwatchesProps) {
  // The picker offers the active palette's slots; the per-pane popover follows
  // whatever palette is chosen in Appearance.
  const palette = useSettings((s) => s.framePalette);
  const colors = paletteColors(palette);
  const isPreset = colors.some((c) => eq(c, value));
  const showToggles = !!onToggleFrame || !!onToggleDot;
  return (
    <div className="hp-swatches-wrap">
      <div className="hp-swatches">
        {colors.map((c) => (
          <button
            key={c}
            type="button"
            className={`hp-swatch${eq(c, value) ? ' selected' : ''}`}
            style={{ background: c }}
            title={c}
            onClick={() => onChange(c)}
          />
        ))}
        <label
          className={`hp-swatch hp-swatch-custom${!isPreset ? ' selected' : ''}`}
          title="Custom color"
          style={!isPreset ? { background: value } : undefined}
        >
          <input type="color" value={value} onChange={(e) => onChange(e.target.value)} />
        </label>
      </div>
      {showToggles && (
        <div style={{ display: 'flex', gap: '14px', marginTop: '8px', fontSize: '12px' }}>
          {onToggleFrame && (
            <label style={{ display: 'flex', alignItems: 'center', gap: '5px', cursor: 'pointer' }}>
              <input
                type="checkbox"
                checked={!!frameOn}
                onChange={(e) => onToggleFrame(e.target.checked)}
              />
              Frame
            </label>
          )}
          {onToggleDot && (
            <label style={{ display: 'flex', alignItems: 'center', gap: '5px', cursor: 'pointer' }}>
              <input type="checkbox" checked={!!dotOn} onChange={(e) => onToggleDot(e.target.checked)} />
              Dot
            </label>
          )}
        </div>
      )}
    </div>
  );
}
