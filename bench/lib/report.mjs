// Render collected results into Markdown: detection table, one table per suite
// (hyperpanes highlighted), and the fairness caveats footer.

export const CAVEATS = [
  'The NATIVE hyperpanes (the Rust rewrite) is benchmarked **idle only**: its GUI binary (v0.0.1) ignores CLI argv and has no run-a-command flag, so the harness cannot inject an in-pane workload — only idle memory/CPU of a fresh instance (one default-shell pane) are measured. Throughput and startup-in-pane are therefore n/a for native until the GUI wires CLI launch (the parser + single-instance gate already exist in core + the headless daemon).',
  'Each measured hyperpanes (native or Electron) is launched with an ISOLATED data dir — native via a throwaway `%APPDATA%`, Electron via `--user-data-dir <temp>` — so it starts as a clean fresh instance and does not hand off to a running copy. The Electron baseline is the INSTALLED app; it is also measured idle-only so the comparison is apples-to-apples (fresh instance, one default pane, no workload).',
  'Memory is a Win32_Process tree-walk (Working Set + Private Bytes) summed from the spawned root PID — this captures Electron’s multi-process tree (main + GPU + renderer + utility helpers) and the native app’s single process. It can miss a reused host process (Windows Terminal shares one host across windows, and `wt.exe` is a launcher stub that may exit) or include unrelated windows.',
  'Idle CPU is sampled by diffing each process’s total processor time over a fixed window (default 2 s) and summing the tree; it is expressed as percent of one core (so it can exceed 100). It is a short idle snapshot, sensitive to background animation/rendering, not a sustained average.',
  'Throughput (PTY backpressure, vtebench’s model) and startup ("process launch → command running in a pane") apply only to terminals with a run-a-command CLI (the Electron build, Windows Terminal, …). A terminal that buffers a large PTY read can ack bytes before rendering, under-reporting render cost; it is the accepted proxy, not a pixel-accurate timer.',
  'Config-only terminals (Tabby, Hyper, Wave) and the native hyperpanes have no run-a-command flag, so they are launched bare and report **idle memory/CPU only**.',
  'Input latency is NOT automated here. Use the manual Typometer procedure in the README for that.',
  'Kitty and Ghostty have no Windows build and are excluded.',
  'Run on AC power with other apps closed; results are medians/single idle snapshots but still machine- and load-dependent.'
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
  const hasCpu = mem.rows.some((r) => r.idleCpuPct != null);
  const headers = [
    'Terminal',
    'Idle WS (MB)',
    'Idle Private (MB)',
    'Load WS (MB)',
    ...(hasCpu ? ['Idle CPU (%)'] : []),
    'Procs',
    'Note'
  ];
  const rows = mem.rows.map((r) => [
    label(r),
    fmt(r.idleWorkingSetMB),
    fmt(r.idlePrivateMB),
    fmt(r.loadWorkingSetMB),
    ...(hasCpu ? [fmt(r.idleCpuPct)] : []),
    r.procCount == null ? '—' : String(r.procCount),
    r.note || ''
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
