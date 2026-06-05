// Idle workload — runs INSIDE the terminal under test (driven memory sample).
//
//   <node> idle.mjs --ready <marker> [--maxalive <ms>]
//
// Writes a ready-marker (so the harness knows the pane is up), then idles to keep
// the process tree alive and stable while the harness walks Win32_Process and sums
// memory. The harness kills the tree when done; the safety timeout prevents an
// orphan if the harness dies.

import { writeFileSync } from 'node:fs';
import { parseArgs, numFlag } from '../lib/args.mjs';

const { flags } = parseArgs();
const ready = flags.ready ? String(flags.ready) : null;
const maxAlive = numFlag(flags.maxalive, 120000);

if (ready) writeFileSync(ready, JSON.stringify({ pid: process.pid, at: Date.now() }));
process.stdout.write('[bench:idle] ready\r\n');

setTimeout(() => process.exit(0), maxAlive);
setInterval(() => {}, 1 << 30);
