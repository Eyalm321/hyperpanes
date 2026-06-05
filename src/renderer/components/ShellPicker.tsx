import { useState } from 'react';

// Sentinel <option> value for "type your own shell path".
const CUSTOM = '__custom__';

interface Preset {
  value: string;
  label: string;
}

// Platform-appropriate quick picks. The empty value is the caller-supplied
// "use the default" option; anything not listed here is entered as Custom.
function presetsFor(platform: string, defaultLabel: string): Preset[] {
  const head: Preset = { value: '', label: defaultLabel };
  if (platform === 'win32') {
    return [
      head,
      { value: 'pwsh', label: 'PowerShell 7 (pwsh)' },
      { value: 'powershell', label: 'Windows PowerShell' },
      { value: 'cmd', label: 'Command Prompt (cmd)' }
    ];
  }
  const posix: Preset[] = [
    { value: 'zsh', label: 'zsh' },
    { value: 'bash', label: 'bash' },
    { value: 'fish', label: 'fish' },
    { value: 'pwsh', label: 'PowerShell (pwsh)' }
  ];
  return [head, ...posix];
}

interface ShellPickerProps {
  value: string;
  onChange: (value: string) => void;
  /** Label for the empty option (e.g. "System default" / "Use default shell"). */
  defaultLabel: string;
}

export function ShellPicker({ value, onChange, defaultLabel }: ShellPickerProps) {
  const platform = (typeof window !== 'undefined' && window.hp?.platform) || 'win32';
  const presets = presetsFor(platform, defaultLabel);
  const isPreset = presets.some((p) => p.value === value);

  // A non-empty value that isn't a preset is a custom shell. The flag lets the
  // custom row stick even while its text is momentarily empty.
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
        {presets.map((p) => (
          <option key={p.value || 'default'} value={p.value}>
            {p.label}
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
          placeholder={platform === 'win32' ? 'e.g. C:\\path\\to\\shell.exe' : 'e.g. /usr/bin/fish'}
          onChange={(e) => onChange(e.target.value)}
        />
      )}
    </div>
  );
}
