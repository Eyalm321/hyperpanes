// Timing + summary-stat helpers shared across suites.

/** High-resolution elapsed-ms helper. Returns a function that yields ms since the call. */
export function startTimer() {
  const t0 = process.hrtime.bigint();
  return () => Number(process.hrtime.bigint() - t0) / 1e6;
}

/** Sleep without blocking the event loop. */
export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function sorted(nums) {
  return [...nums].sort((a, b) => a - b);
}

export function median(nums) {
  if (!nums.length) return NaN;
  const s = sorted(nums);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
}

export function mean(nums) {
  if (!nums.length) return NaN;
  return nums.reduce((a, b) => a + b, 0) / nums.length;
}

export function stddev(nums) {
  if (nums.length < 2) return 0;
  const m = mean(nums);
  const variance = nums.reduce((a, b) => a + (b - m) ** 2, 0) / (nums.length - 1);
  return Math.sqrt(variance);
}

/** Roll a list of samples into a compact stats object. */
export function summarize(samples) {
  const xs = samples.filter((n) => Number.isFinite(n));
  return {
    runs: xs.length,
    min: xs.length ? Math.min(...xs) : NaN,
    median: median(xs),
    mean: mean(xs),
    stddev: stddev(xs),
    samples: xs
  };
}
