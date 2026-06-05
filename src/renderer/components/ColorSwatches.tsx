import { paletteColors } from '../theme';
import { useSettings } from '../store/useSettings';

interface ColorSwatchesProps {
  value: string;
  onChange: (color: string) => void;
}

const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

export function ColorSwatches({ value, onChange }: ColorSwatchesProps) {
  // The picker offers the active palette's slots; the per-pane popover follows
  // whatever palette is chosen in Appearance.
  const palette = useSettings((s) => s.framePalette);
  const colors = paletteColors(palette);
  const isPreset = colors.some((c) => eq(c, value));
  return (
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
  );
}
