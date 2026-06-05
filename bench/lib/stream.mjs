// Backpressure-honoring stdout helpers, shared by the throughput + fill workloads.
// A terminal applies flow control on its PTY/conpty input: when its buffer is
// full it stops reading, write() returns false, and we await 'drain'. Because the
// write only unblocks once the terminal has consumed (≈ rendered) the bytes,
// elapsed wall-time over a fixed payload is a proxy for render throughput. This is
// the same model vtebench uses.

import { once } from 'node:events';

/** Write a Buffer to stdout in chunks, awaiting 'drain' whenever the kernel buffer fills. */
export async function writeBuffer(buf, chunkSize = 64 * 1024) {
  for (let offset = 0; offset < buf.length; offset += chunkSize) {
    const slice = buf.subarray(offset, Math.min(offset + chunkSize, buf.length));
    if (!process.stdout.write(slice)) {
      await once(process.stdout, 'drain');
    }
  }
}

/** Resolve once everything written so far has been flushed (write ordering guarantees this). */
export function flushStdout() {
  return new Promise((resolve) => process.stdout.write('\x1b[0m', () => resolve()));
}

/** Deterministic LCG so generated payloads are byte-identical across runs. */
export function rng(seed = 0x2545f491) {
  let s = seed >>> 0;
  return () => {
    // numerical recipes LCG
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 0xffffffff;
  };
}
