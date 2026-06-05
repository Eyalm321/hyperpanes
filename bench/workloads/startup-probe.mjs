// Startup probe — runs INSIDE the terminal under test.
//
//   <node> startup-probe.mjs --out <file> [--hold]
//
// Uniform startup metric that works even for terminals with no auto-exit
// (hyperpanes). The harness records t0 (Date.now) immediately before spawning the
// terminal; this probe stamps Date.now() the instant it executes and writes it to
// --out. delta = probeStart - t0 = "process launch -> your command is running in a
// pane". The constant Node-start cost cancels when comparing terminals.
//
// Auto-exit terminals: omit --hold so the process exits and the window closes.
// hyperpanes: pass --hold so the pane stays alive until the harness kills the tree.

import { writeFileSync } from 'node:fs';
import { parseArgs } from '../lib/args.mjs';

const { flags } = parseArgs();
const out = flags.out ? String(flags.out) : null;
const probeStart = Date.now();

if (out) writeFileSync(out, JSON.stringify({ probeStart }));

if (flags.hold) {
  // Keep the pane open for the harness to detect the file and kill the tree.
  // Safety auto-exit so a crashed harness never leaves an orphan.
  setTimeout(() => process.exit(0), 120000);
  setInterval(() => {}, 1 << 30);
} else {
  process.exit(0);
}
