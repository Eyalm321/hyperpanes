import { paletteColors } from '../theme';
import { useSettings } from '../store/useSettings';

interface ColorSwatchesProps {
  value: string;
  onChange: (color: string) => void;
  // When the frame/dot toggle pair is supplied (the pane color pickers), the grid
  // gains a leading "no color" swatch (a Photoshop-style red slash): picking it
  // turns the frame AND dot off; picking any color turns them back on. Omit the
  // handlers (e.g. the project color picker) and the none swatch isn't shown.
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
  const hasNone = !!onToggleFrame && !!onToggleDot;
  const noneSelected = hasNone && !frameOn && !dotOn;

  // Picking a color shows it (frame + dot on); picking "none" hides both.
  const pickColor = (c: string) => {
    onChange(c);
    onToggleFrame?.(true);
    onToggleDot?.(true);
  };
  const pickNone = () => {
    onToggleFrame?.(false);
    onToggleDot?.(false);
  };

  return (
    <div className="hp-swatches">
      {hasNone && (
        <button
          type="button"
          className={`hp-swatch hp-swatch-none${noneSelected ? ' selected' : ''}`}
          title="No color — no frame or dot"
          onClick={pickNone}
        />
      )}
      {colors.map((c) => (
        <button
          key={c}
          type="button"
          className={`hp-swatch${!noneSelected && eq(c, value) ? ' selected' : ''}`}
          style={{ background: c }}
          title={c}
          onClick={() => pickColor(c)}
        />
      ))}
      <label
        className={`hp-swatch hp-swatch-custom${!noneSelected && !isPreset ? ' selected' : ''}`}
        title="Custom color"
        style={!noneSelected && !isPreset ? { background: value } : undefined}
      >
        <input type="color" value={value} onChange={(e) => pickColor(e.target.value)} />
      </label>
    </div>
  );
}
