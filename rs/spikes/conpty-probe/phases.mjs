// phases.mjs — phased workload for flush-timing diagnosis.
// Phase 1: ~2 MB of scroll-region lines. Then 3s idle. Then a marker line,
// 1s idle, exit. If the master sees bytes during the idle, the host flushes
// on idle; if bytes only arrive at exit, the host flushes at teardown.
// Ignores --case/--bytes args the probe passes.
const out = process.stdout;
const write = (s) => new Promise((r) => out.write(s, r));

await write('\x1b[1;20r\x1b[H');
let total = 0;
let n = 0;
while (total < 2_000_000) {
  const s = `phase1 line ${n++} — lorem ipsum dolor sit amet consectetur\r\n`;
  total += s.length;
  await write(s);
}
await new Promise((r) => setTimeout(r, 3000));
await write('MARKER-AFTER-IDLE\r\n');
await new Promise((r) => setTimeout(r, 1000));
await write('\x1b[r');
process.exit(0);
