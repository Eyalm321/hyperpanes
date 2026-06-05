// Throughput workload — runs INSIDE the terminal under test.
//
//   <node> throughput.mjs --case <name> --out <file> [--bytes <MB>]
//
// A Node reimplementation of vtebench's streaming model (vtebench itself is
// Rust+bash and WSL-only on Windows). We build a fixed-size payload for the
// chosen case, stream it to stdout honoring backpressure, time the stream, then
// write {case, bytes, ms, mbPerSec} to --out and exit. The terminal renders as
// fast as it can; backpressure makes elapsed ≈ throughput.
//
// NOT byte-identical to vtebench — a representative, internally-consistent proxy.

import { writeFileSync } from 'node:fs';
import { parseArgs, numFlag } from '../lib/args.mjs';
import { writeBuffer, flushStdout, rng } from '../lib/stream.mjs';
import { startTimer } from '../measure/timing.mjs';

const { flags } = parseArgs();
const kase = String(flags.case || 'dense');
const out = flags.out ? String(flags.out) : null;
const budget = numFlag(flags.bytes, 16) * 1000 * 1000; // payload bytes (~MB, decimal)

const PRINTABLE = '!"#$%&\'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~ ';
const UNICODE = ['日本語', 'çödé', 'áêĩ', '😀🚀✨🔥', 'Ωμέγα', 'मनोज', '汉字宽度'];

// Per-case block builder. Returns a string appended to the payload until the byte
// budget is hit. Escape sequences are part of the streamed bytes.
function makeBlock(kase) {
  const rand = rng();
  let n = 0;
  switch (kase) {
    case 'dense': {
      // Full rows of varied printable cells.
      const width = 200;
      return () => {
        let row = '';
        for (let i = 0; i < width; i++) row += PRINTABLE[(n + i) % PRINTABLE.length];
        n++;
        return row + '\r\n';
      };
    }
    case 'scrolling':
      return () => `${n++}: the quick brown fox jumps over the lazy dog\r\n`;
    case 'scrolling-region':
      // Region is set once in the prologue; blocks just feed lines that scroll in it.
      return () => `region line ${n++} — lorem ipsum dolor sit amet consectetur\r\n`;
    case 'alt-screen':
      // Random positioned writes within the alternate screen buffer.
      return () => {
        const r = 1 + Math.floor(rand() * 40);
        const c = 1 + Math.floor(rand() * 100);
        const txt = PRINTABLE.slice(0, 1 + Math.floor(rand() * 20));
        n++;
        return `\x1b[${r};${c}H${txt}`;
      };
    case 'unicode':
      return () => {
        const parts = [];
        for (let i = 0; i < 8; i++) parts.push(UNICODE[(n + i) % UNICODE.length]);
        n++;
        return parts.join(' ') + '\r\n';
      };
    case 'cursor-motion':
      return () => {
        let s = '';
        for (let i = 0; i < 16; i++) {
          const r = 1 + Math.floor(rand() * 40);
          const c = 1 + Math.floor(rand() * 100);
          s += `\x1b[${r};${c}H*`;
        }
        n++;
        return s;
      };
    default:
      throw new Error(`unknown case: ${kase}`);
  }
}

function prologue(kase) {
  if (kase === 'scrolling-region') return '\x1b[1;20r\x1b[H'; // set scroll region, home
  if (kase === 'alt-screen') return '\x1b[?1049h'; // enter alt buffer
  return '';
}
function epilogue(kase) {
  if (kase === 'scrolling-region') return '\x1b[r'; // reset scroll region
  if (kase === 'alt-screen') return '\x1b[?1049l'; // leave alt buffer
  return '';
}

function buildPayload(kase, budget) {
  const block = makeBlock(kase);
  const parts = [prologue(kase)];
  let total = Buffer.byteLength(parts[0], 'utf8');
  const tail = Buffer.byteLength(epilogue(kase), 'utf8');
  while (total < budget - tail) {
    const s = block();
    parts.push(s);
    total += Buffer.byteLength(s, 'utf8');
  }
  parts.push(epilogue(kase));
  return Buffer.from(parts.join(''), 'utf8');
}

async function main() {
  const payload = buildPayload(kase, budget); // built BEFORE timing
  const timer = startTimer();
  await writeBuffer(payload);
  await flushStdout();
  const ms = timer();

  const result = {
    case: kase,
    bytes: payload.length,
    ms: Math.round(ms * 100) / 100,
    mbPerSec: Math.round((payload.length / (1000 * ms)) * 100) / 100
  };
  if (out) writeFileSync(out, JSON.stringify(result));
  process.stdout.write(
    `\r\n[bench:throughput] ${result.case}: ${result.mbPerSec} MB/s (${result.bytes} bytes / ${result.ms} ms)\r\n`
  );
  process.exit(0);
}

main().catch((err) => {
  if (out) {
    try {
      writeFileSync(out, JSON.stringify({ case: kase, error: String(err) }));
    } catch {
      /* ignore */
    }
  }
  process.stderr.write(`[bench:throughput] error: ${err}\n`);
  process.exit(1);
});
