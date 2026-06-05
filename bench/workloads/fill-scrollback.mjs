// Fill-scrollback workload — runs INSIDE the terminal under test (memory-under-load).
//
//   <node> fill-scrollback.mjs --ready <marker> [--lines <N>] [--maxalive <ms>]
//
// Prints N lines to grow the terminal's scrollback buffer (the "after-load" memory
// phase), writes the ready-marker AFTER filling so the harness samples a settled
// post-fill tree, then idles like idle.mjs until the harness kills the tree.

import { writeFileSync } from 'node:fs';
import { parseArgs, numFlag } from '../lib/args.mjs';
import { writeBuffer, flushStdout } from '../lib/stream.mjs';

const { flags } = parseArgs();
const ready = flags.ready ? String(flags.ready) : null;
const lines = numFlag(flags.lines, 200000);
const maxAlive = numFlag(flags.maxalive, 120000);

function buildLines(count) {
  const parts = [];
  for (let i = 0; i < count; i++) {
    parts.push(`scrollback ${i}: the quick brown fox jumps over the lazy dog 0123456789\r\n`);
  }
  return Buffer.from(parts.join(''), 'utf8');
}

async function main() {
  await writeBuffer(buildLines(lines));
  await flushStdout();
  if (ready) writeFileSync(ready, JSON.stringify({ pid: process.pid, at: Date.now(), lines }));
  process.stdout.write(`\r\n[bench:fill] filled ${lines} lines\r\n`);

  setTimeout(() => process.exit(0), maxAlive);
  setInterval(() => {}, 1 << 30);
}

main().catch((err) => {
  process.stderr.write(`[bench:fill] error: ${err}\n`);
  process.exit(1);
});
