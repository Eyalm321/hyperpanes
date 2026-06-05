import { useState } from 'react';
import { FONT_OPTIONS } from './terminal-themes';

// Sentinel <option> value for "type your own font family".
const CUSTOM = '__custom__';

interface FontPickerProps {
  value: string;
  onChange: (value: string) => void;
}

// Mirrors ShellPicker: a quick-pick <select> plus a custom free-text row for any
// font not in the list. The empty value is the built-in default stack.
export function FontPicker({ value, onChange }: FontPickerProps) {
  const isPreset = FONT_OPTIONS.some((o) => o.value === value);

  const [custom, setCustom] = useState(!isPreset && value !== '');
  const showCustom = custom || (!isPreset && value !== '');

  const onSelect = (v: string) => {
    if (v === CUSTOM) {
      setCustom(true);
      if (isPreset) onChange(''); // start the custom field empty, not on a preset
      return;
    }
    setCustom(false);
    onChange(v);
  };

  return (
    <div className="hp-shell-picker">
      <select
        className="hp-select"
        value={showCustom ? CUSTOM : value}
        onChange={(e) => onSelect(e.target.value)}
      >
        {FONT_OPTIONS.map((o) => (
          <option key={o.value || 'default'} value={o.value}>
            {o.label}
          </option>
        ))}
        <option value={CUSTOM}>Custom…</option>
      </select>
      {showCustom && (
        <input
          className="hp-input"
          type="text"
          autoFocus
          value={value}
          placeholder='e.g. "JetBrains Mono", monospace'
          onChange={(e) => onChange(e.target.value)}
        />
      )}
    </div>
  );
}
