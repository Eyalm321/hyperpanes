// Detect which terminals are installed and their versions. Detect-only: never
// installs or mutates anything. `--check-updates` additionally reports (does not
// apply) `winget upgrade` status. Writes results/terminals.json and prints a table.
//
//   node bench/detect.mjs [--check-updates]

import { spawnSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import os from 'node:os';
import { TERMINALS } from './terminals.config.mjs';
import { ensureResultsDir, RESULTS_DIR } from './lib/launch.mjs';
import { parseArgs } from './lib/args.mjs';

function readVersion(t, exePath) {
  if (t.fixedVersion) return t.fixedVersion();
  if (!t.versionArgs || !exePath) return null;
  const res = spawnSync(exePath, t.versionArgs, { encoding: 'utf8', timeout: 8000 });
  const pick = (s) =>
    (s || '')
      .split(/\r?\n/)
      .map((x) => x.trim())
      .filter(Boolean)[0] || null;
  return pick(res.stdout) || pick(res.stderr);
}

export function detectAll() {
  return TERMINALS.map((t) => {
    if (t.platformUnavailable) {
      return { id: t.id, name: t.name, installed: false, exePath: null, version: null, note: 'n/a (no Windows build)' };
    }
    const exePath = t.resolveExe();
    let installed = !!exePath;
    let note = t.driven ? '' : 'config-only (idle memory only)';
    if (t.id === 'hyperpanes' && !exePath && t.isAvailable?.()) {
      installed = true;
      note = 'dev build (out/main/index.js)';
    }
    const version = installed ? readVersion(t, exePath) : null;
    return { id: t.id, name: t.name, installed, exePath, version, note, wingetId: t.wingetId };
  });
}

export function checkUpdates(detected) {
  const res = spawnSync('winget', ['upgrade'], { encoding: 'utf8', timeout: 90000 });
  const text = res.stdout || '';
  for (const d of detected) {
    if (d.installed && d.wingetId) d.updateAvailable = text.toLowerCase().includes(d.wingetId.toLowerCase());
  }
  return res.status === 0;
}

export function printTable(detected) {
  const rows = detected.map((d) => ({
    Terminal: d.name,
    Installed: d.installed ? 'yes' : d.note?.includes('n/a') ? 'n/a' : 'no',
    Version: d.version || (d.installed ? '?' : '—'),
    Update: d.updateAvailable == null ? '' : d.updateAvailable ? 'available' : 'current',
    Note: d.note || ''
  }));
  const cols = ['Terminal', 'Installed', 'Version', 'Update', 'Note'];
  const width = (c) => Math.max(c.length, ...rows.map((r) => String(r[c]).length));
  const widths = Object.fromEntries(cols.map((c) => [c, width(c)]));
  const fmtRow = (r) => cols.map((c) => String(r[c]).padEnd(widths[c])).join('  ');
  console.log(fmtRow(Object.fromEntries(cols.map((c) => [c, c]))));
  console.log(cols.map((c) => '-'.repeat(widths[c])).join('  '));
  for (const r of rows) console.log(fmtRow(r));
}

export function writeTerminalsJson(detected) {
  ensureResultsDir();
  const out = {
    date: new Date().toISOString(),
    machine: `${os.hostname()} ${os.type()} ${os.release()} (${os.cpus()[0]?.model || '?'}, ${os.cpus().length} cores)`,
    node: process.version,
    terminals: detected
  };
  const path = join(RESULTS_DIR, 'terminals.json');
  writeFileSync(path, JSON.stringify(out, null, 2));
  return path;
}

function main() {
  const { flags } = parseArgs();
  const detected = detectAll();
  if (flags['check-updates']) {
    console.log('Checking winget upgrade status (report-only)…');
    if (!checkUpdates(detected)) console.log('(winget unavailable or returned no data)');
  }
  printTable(detected);
  const path = writeTerminalsJson(detected);
  console.log(`\nWrote ${path}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}
