// Render collected results into Markdown: detection table, one table per suite
// (hyperpanes highlighted), and the fairness caveats footer.

export const CAVEATS = [
  'Throughput is measured by PTY backpressure (vtebench’s model): a terminal that buffers a large PTY read can ack bytes before rendering them, which under-reports its render cost. It’s the accepted proxy, not a pixel-accurate render timer.',
  'Startup is "process launch → command running in a pane" and includes a constant Node-start cost (cancels across terminals). hyperpanes additionally pays a `shell -c` spawn inside the pane — a small asymmetry.',
  'Memory is a Win32_Process tree-walk (Working Set + Private Bytes) from the spawned root PID. It can miss a reused host process (Windows Terminal shares one host across windows, and `wt.exe` is a launcher stub that may exit) or include unrelated windows. hyperpanes is cross-checked against its own in-app `metrics().totalMemoryMB`.',
  'Tabby, Hyper and Wave have no run-a-command CLI flag, so they are launched bare and report **idle memory only** — clearly labeled "not driven".',
  'Input latency is NOT automated here. Use the manual Typometer procedure in the README for that.',
  'Kitty and Ghostty have no Windows build and are excluded.',
  'Run on AC power with other apps closed; results are medians over multiple runs but still machine- and load-dependent.'
];

const fmt = (n, digits = 1) =>
  n == null || Number.isNaN(n) ? '—' : typeof n === 'number' ? n.toFixed(digits) : String(n);

function table(headers, rows) {
  const head = `| ${headers.join(' | ')} |`;
  const sep = `| ${headers.map(() => '---').join(' | ')} |`;
  const body = rows.map((r) => `| ${r.join(' | ')} |`).join('\n');
  return [head, sep, body].join('\n');
}

const label = (row) => (row.id === 'hyperpanes' ? `**${row.name}**` : row.name);

function detectSection(detect = []) {
  const rows = detect.map((d) => [
    label(d),
    d.installed ? 'yes' : d.note?.includes('platform') ? 'n/a' : 'no',
    d.version || (d.installed ? '?' : '—'),
    d.note || ''
  ]);
  return `## Detected terminals\n\n${table(['Terminal', 'Installed', 'Version', 'Note'], rows)}`;
}

function throughputSection(thr) {
  if (!thr || !thr.rows?.length) return '';
  const cases = thr.cases || [];
  const headers = ['Terminal', ...cases.map((c) => `${c} (MB/s)`)];
  const rows = thr.rows.map((r) => [label(r), ...cases.map((c) => fmt(r.byCase?.[c]))]);
  return `## Throughput (median MB/s, higher is better)\n\n${table(headers, rows)}`;
}

function startupSection(st) {
  if (!st || !st.rows?.length) return '';
  const hasHf = st.rows.some((r) => r.hyperfine);
  const headers = ['Terminal', 'Median (ms)', 'Stddev (ms)', ...(hasHf ? ['hyperfine mean (ms)'] : [])];
  const rows = st.rows.map((r) => [
    label(r),
    fmt(r.medianMs),
    fmt(r.stddevMs),
    ...(hasHf ? [r.hyperfine ? `${fmt(r.hyperfine.meanMs)} ± ${fmt(r.hyperfine.stddevMs)}` : '—'] : [])
  ]);
  return `## Startup (launch → command in pane, lower is better)\n\n${table(headers, rows)}`;
}

function memorySection(mem) {
  if (!mem || !mem.rows?.length) return '';
  const headers = ['Terminal', 'Idle WS (MB)', 'Idle Private (MB)', 'Load WS (MB)', 'Procs', 'Note'];
  const rows = mem.rows.map((r) => [
    label(r),
    fmt(r.idleWorkingSetMB),
    fmt(r.idlePrivateMB),
    fmt(r.loadWorkingSetMB),
    r.procCount == null ? '—' : String(r.procCount),
    [r.note, r.metricsCrossCheck != null ? `metrics(): ${fmt(r.metricsCrossCheck)} MB` : '']
      .filter(Boolean)
      .join('; ')
  ]);
  return `## Memory (Win32_Process tree, lower is better)\n\n${table(headers, rows)}`;
}

export function renderReport(data) {
  const parts = [];
  parts.push(`# hyperpanes terminal benchmark — ${data.label || 'report'}`);
  const meta = [
    data.date && `Date: ${data.date}`,
    data.machine && `Machine: ${data.machine}`,
    data.node && `Node: ${data.node}`,
    data.runs != null && `Runs: ${data.runs}`
  ].filter(Boolean);
  if (meta.length) parts.push(meta.join(' • '));

  parts.push(detectSection(data.detect));
  const thr = throughputSection(data.suites?.throughput);
  const st = startupSection(data.suites?.startup);
  const mem = memorySection(data.suites?.memory);
  for (const s of [thr, st, mem]) if (s) parts.push(s);

  if (data.errors?.length) {
    parts.push(`## Notes & skipped runs\n\n${data.errors.map((e) => `- ${e}`).join('\n')}`);
  }

  parts.push(`## Fairness caveats\n\n${CAVEATS.map((c, i) => `${i + 1}. ${c}`).join('\n')}`);
  return parts.join('\n\n') + '\n';
}
