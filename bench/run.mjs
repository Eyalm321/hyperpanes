// Orchestrator: detect terminals, run the selected suites over the eligible ones,
// and emit results/<label>.json + results/report.md.
//
//   node bench/run.mjs [--only=ids] [--suite=throughput,startup,memory]
//                      [--runs=N] [--cases=...] [--label=name]
//                      [--bytes=MB] [--lines=N] [--check-updates]
//
// See bench/README.md for the methodology and fairness caveats.

import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import os from 'node:os';
import { parseArgs, listFlag, numFlag } from './lib/args.mjs';
import { TERMINALS, getTerminal } from './terminals.config.mjs';
import { detectAll, checkUpdates } from './detect.mjs';
import { renderReport } from './lib/report.mjs';
import { summarize, median, sleep } from './measure/timing.mjs';
import { sampleTree } from './measure/proctree.mjs';
import {
  RESULTS_DIR,
  REPO_ROOT,
  ensureResultsDir,
  uid,
  writeCmdWrapper,
  spawnTerminal,
  waitForFile,
  readJsonSafe,
  killTreeAndWait,
  nativeRepoRunning,
  cleanup
} from './lib/launch.mjs';

const WORKLOADS = join(REPO_ROOT, 'bench', 'workloads');
const DEFAULT_CASES = ['dense', 'scrolling', 'scrolling-region', 'alt-screen', 'unicode', 'cursor-motion'];
const SETTLE_MS = 4000; // after ready-marker, before sampling memory
const BARE_STARTUP_MS = 6000; // config-only apps have no ready-marker
const DEFAULT_CPU_MS = 2000; // idle-CPU sample window (0 disables CPU sampling)

const log = (...a) => console.log('[bench]', ...a);

/** One throughput launch: stream a case in the terminal, read its self-timed result. */
async function throughputRun(term, kase, bytes, timeoutMs) {
  const id = uid('thr');
  const out = join(RESULTS_DIR, `${id}.json`);
  const wrapper = writeCmdWrapper(id, join(WORKLOADS, 'throughput.mjs'), ['--case', kase, '--out', out, '--bytes', String(bytes)]);
  const { exe, args, env, temp } = term.launch({ wrapperPath: wrapper, cwd: REPO_ROOT, label: 'bench' });
  const child = spawnTerminal(exe, args, { env });
  let result = null;
  if (await waitForFile(out, timeoutMs)) result = await readJsonSafe(out);
  await killTreeAndWait(child.pid);
  cleanup([wrapper, out, ...(temp || [])]);
  return result && result.mbPerSec != null ? result.mbPerSec : null;
}

async function throughputSuite(term, { cases, runs, bytes, timeout }) {
  const byCase = {};
  for (const kase of cases) {
    const samples = [];
    for (let r = 0; r < runs; r++) {
      const v = await throughputRun(term, kase, bytes, timeout);
      if (v != null) samples.push(v);
    }
    byCase[kase] = samples.length ? median(samples) : null;
    log(`  throughput ${term.id} ${kase}: ${byCase[kase] == null ? 'n/a' : byCase[kase].toFixed(1) + ' MB/s'}`);
  }
  return { byCase };
}

/** One startup launch: delta between harness t0 and the probe's first-execution stamp. */
async function startupRun(term, timeoutMs) {
  const id = uid('start');
  const out = join(RESULTS_DIR, `${id}.json`);
  const probeArgs = ['--out', out];
  if (term.id === 'hyperpanes') probeArgs.push('--hold'); // no auto-exit
  const wrapper = writeCmdWrapper(id, join(WORKLOADS, 'startup-probe.mjs'), probeArgs);
  const { exe, args, env, temp } = term.launch({ wrapperPath: wrapper, cwd: REPO_ROOT, label: 'bench' });
  const t0 = Date.now();
  const child = spawnTerminal(exe, args, { env });
  let delta = null;
  if (await waitForFile(out, timeoutMs)) {
    const res = await readJsonSafe(out);
    if (res && res.probeStart != null) delta = res.probeStart - t0;
  }
  await killTreeAndWait(child.pid);
  cleanup([wrapper, out, ...(temp || [])]);
  return delta;
}

async function startupSuite(term, { runs, timeout }) {
  const samples = [];
  // one warmup, discarded
  await startupRun(term, timeout);
  for (let r = 0; r < runs; r++) {
    const d = await startupRun(term, timeout);
    if (d != null && d >= 0) samples.push(d);
  }
  const s = summarize(samples);
  log(`  startup ${term.id}: ${Number.isNaN(s.median) ? 'n/a' : Math.round(s.median) + ' ms'}`);
  return { medianMs: s.median, stddevMs: s.stddev, runs: s.runs };
}

/** Spawn a workload, wait for its ready-marker, settle, sample the tree, kill. */
async function memorySample(term, workloadFile, workloadArgs, { readyTimeout, cpuMs = 0 }) {
  const id = uid('mem');
  const ready = join(RESULTS_DIR, `${id}.ready.json`);
  const wrapper = writeCmdWrapper(id, join(WORKLOADS, workloadFile), ['--ready', ready, ...workloadArgs]);
  const { exe, args, env, temp } = term.launch({ wrapperPath: wrapper, cwd: REPO_ROOT, label: 'bench' });
  const child = spawnTerminal(exe, args, { env });
  await waitForFile(ready, readyTimeout);
  await sleep(SETTLE_MS);
  const sample = sampleTree(child.pid, { cpuMs });
  await killTreeAndWait(child.pid);
  cleanup([wrapper, ready, ...(temp || [])]);
  return sample;
}

async function memorySuite(term, { lines, idleOnly, cpuMs = 0 }) {
  const row = {
    idleWorkingSetMB: null,
    idlePrivateMB: null,
    loadWorkingSetMB: null,
    idleCpuPct: null,
    procCount: null,
    note: ''
  };

  if (idleOnly) {
    // No run-a-command flag (config-only terminals + the native hyperpanes GUI): launch bare with
    // whatever isolated data dir the entry mints, settle, then sample idle memory + CPU.
    const { exe, args, env, temp } = term.launch({ wrapperPath: null, cwd: REPO_ROOT, label: 'bench' });
    const child = spawnTerminal(exe, args, { env });
    await sleep(BARE_STARTUP_MS);
    const s = sampleTree(child.pid, { cpuMs });
    await killTreeAndWait(child.pid);
    cleanup(temp || []);
    row.idleWorkingSetMB = s.ok ? s.workingSetMB : null;
    row.idlePrivateMB = s.ok ? s.privateBytesMB : null;
    row.idleCpuPct = s.ok ? s.cpuPercent : null;
    row.procCount = s.ok ? s.count : null;
    row.note =
      s.ok && s.count > 0 ? 'idle only (not driven)' : 'idle only; tree empty — app may be a launcher stub that exited';
    log(
      `  memory ${term.id} (idle-only): ${row.idleWorkingSetMB == null ? 'n/a' : row.idleWorkingSetMB + ' MB'}` +
        `${row.idleCpuPct == null ? '' : `, cpu ${row.idleCpuPct}%`}, ${row.procCount ?? '?'} procs`
    );
    return row;
  }

  const idle = await memorySample(term, 'idle.mjs', [], { readyTimeout: 30000, cpuMs });
  row.idleWorkingSetMB = idle.ok ? idle.workingSetMB : null;
  row.idlePrivateMB = idle.ok ? idle.privateBytesMB : null;
  row.idleCpuPct = idle.ok ? idle.cpuPercent : null;
  row.procCount = idle.ok ? idle.count : null;
  if (idle.ok && idle.count === 0) row.note = 'tree empty (launcher stub exited — see WT caveat)';

  const load = await memorySample(term, 'fill-scrollback.mjs', ['--lines', String(lines)], { readyTimeout: 180000 });
  row.loadWorkingSetMB = load.ok ? load.workingSetMB : null;

  log(`  memory ${term.id}: idle ${row.idleWorkingSetMB ?? 'n/a'} MB, load ${row.loadWorkingSetMB ?? 'n/a'} MB`);
  return row;
}

/**
 * Soft preflight. The native GUI has no single-instance lock and every measured hyperpanes
 * (native or Electron) is launched with an isolated data dir, so a running instance does NOT
 * capture the harness's launch — there is nothing to hard-skip. We only surface an informational
 * note when a repo-built native instance is already up (it shares the machine's CPU/RAM, which can
 * nudge the idle numbers). Returns null (never blocks).
 */
function preflight(term) {
  if (term.id === 'hyperpanes' && nativeRepoRunning()) {
    log('  note: a repo-built native hyperpanes is already running — measuring a separate fresh (isolated-APPDATA) instance; close it for the cleanest idle numbers.');
  }
  return null;
}

async function main() {
  const { flags } = parseArgs();
  const onlyIds = listFlag(flags.only);
  const suitesReq = listFlag(flags.suite) || ['throughput', 'startup', 'memory'];
  const runs = numFlag(flags.runs, 5);
  const cases = listFlag(flags.cases) || DEFAULT_CASES;
  const bytes = numFlag(flags.bytes, 16);
  const lines = numFlag(flags.lines, 200000);
  const tpTimeout = numFlag(flags.timeout, 60000);
  const label = (flags.label && String(flags.label)) || 'report';
  // `--idle-bare` forces EVERY targeted terminal through the bare idle sample (no in-pane
  // workload), so a native-vs-Electron-vs-WT memory/CPU comparison is identical methodology
  // for all of them. `--cpu-ms=0` disables idle-CPU sampling.
  const idleBare = !!flags['idle-bare'];
  const cpuMs = numFlag(flags['cpu-ms'], DEFAULT_CPU_MS);

  ensureResultsDir();
  log('detecting terminals…');
  const detect = detectAll();
  if (flags['check-updates']) checkUpdates(detect);

  const errors = [];
  const installed = new Map(detect.filter((d) => d.installed).map((d) => [d.id, d]));
  let targets = TERMINALS.filter((t) => installed.has(t.id) && t.suites?.length);
  if (onlyIds) {
    targets = onlyIds.map((id) => getTerminal(id)).filter(Boolean).filter((t) => {
      if (!installed.has(t.id)) {
        errors.push(`${t.id}: not installed — skipped`);
        return false;
      }
      return true;
    });
  }

  const suites = { throughput: { cases, rows: [] }, startup: { rows: [] }, memory: { rows: [] } };

  for (const term of targets) {
    const applicable = term.suites.filter((s) => suitesReq.includes(s));
    if (!applicable.length) continue;
    log(`=== ${term.name} (${applicable.join(', ')}) ===`);

    const block = preflight(term);
    if (block) {
      errors.push(`${term.id}: ${block}`);
      log(`  SKIP: ${block}`);
      continue;
    }

    const base = { id: term.id, name: term.name };
    try {
      if (applicable.includes('throughput')) {
        const r = await throughputSuite(term, { cases, runs, bytes, timeout: tpTimeout });
        suites.throughput.rows.push({ ...base, byCase: r.byCase });
      }
      if (applicable.includes('startup')) {
        const r = await startupSuite(term, { runs, timeout: 30000 });
        suites.startup.rows.push({ ...base, ...r });
      }
      if (applicable.includes('memory')) {
        const r = await memorySuite(term, { lines, idleOnly: idleBare || !!term.memoryIdleOnly, cpuMs });
        suites.memory.rows.push({ ...base, ...r });
      }
    } catch (err) {
      errors.push(`${term.id}: run error — ${err}`);
      log(`  ERROR: ${err}`);
    }
  }

  const data = {
    label,
    date: new Date().toISOString(),
    machine: `${os.hostname()} ${os.type()} ${os.release()} (${os.cpus()[0]?.model || '?'}, ${os.cpus().length} cores)`,
    node: process.version,
    runs,
    detect,
    suites,
    errors
  };

  const jsonPath = join(RESULTS_DIR, `${label}.json`);
  const reportPath = join(RESULTS_DIR, 'report.md');
  writeFileSync(jsonPath, JSON.stringify(data, null, 2));
  writeFileSync(reportPath, renderReport(data));
  log(`wrote ${jsonPath}`);
  log(`wrote ${reportPath}`);
}

main().catch((err) => {
  console.error('[bench] fatal:', err);
  process.exit(1);
});
