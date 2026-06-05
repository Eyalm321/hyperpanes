// Tiny CLI flag parser for the bench scripts. No deps.
// Supports `--flag=value`, `--flag value`, and bare `--flag` (boolean true).
// Repeated/comma-joined list flags are split on commas by the helpers below.

export function parseArgs(argv = process.argv.slice(2)) {
  /** @type {Record<string, string | boolean>} */
  const flags = {};
  const positionals = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) {
      const eq = a.indexOf('=');
      if (eq !== -1) {
        flags[a.slice(2, eq)] = a.slice(eq + 1);
      } else {
        const next = argv[i + 1];
        if (next != null && !next.startsWith('--')) {
          flags[a.slice(2)] = next;
          i++;
        } else {
          flags[a.slice(2)] = true;
        }
      }
    } else {
      positionals.push(a);
    }
  }
  return { flags, positionals };
}

/** Comma-separated list flag -> string[] (empty/absent -> undefined). */
export function listFlag(value) {
  if (value == null || value === true || value === '') return undefined;
  return String(value)
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean);
}

/** Numeric flag with default. */
export function numFlag(value, fallback) {
  const n = Number.parseInt(String(value), 10);
  return Number.isFinite(n) ? n : fallback;
}
