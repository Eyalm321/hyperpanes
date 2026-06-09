// Node wrapper around proctree.ps1: invoke it for a root PID and sum the tree.
// Memory is reported in MiB (bytes / 1048576) to line up with hyperpanes'
// metrics().totalMemoryMB (which is workingSetSize KB / 1024).

import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const PS1 = join(here, 'proctree.ps1');
const MIB = 1048576;

const round1 = (n) => Math.round(n * 10) / 10;

/**
 * Sample the process tree rooted at rootPid.
 * @param {number} rootPid
 * @param {{ cpuMs?: number }} [opts] cpuMs>0 → also measure idle CPU over that window (ms).
 * @returns {{ ok:boolean, error?:string, count:number, workingSetMB:number, privateBytesMB:number, cpuPercent:number|null, processes:any[] }}
 */
export function sampleTree(rootPid, { cpuMs = 0 } = {}) {
  const psArgs = ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', PS1, '-RootPid', String(rootPid)];
  if (cpuMs > 0) psArgs.push('-CpuMs', String(cpuMs));
  const res = spawnSync('powershell.exe', psArgs, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });

  const empty = { ok: false, count: 0, workingSetMB: 0, privateBytesMB: 0, cpuPercent: null, processes: [] };
  if (res.error) return { ...empty, error: String(res.error) };
  if (res.status !== 0) return { ...empty, error: (res.stderr || `exit ${res.status}`).trim() };

  let parsed;
  try {
    parsed = JSON.parse(res.stdout || '{}');
  } catch (e) {
    return { ...empty, error: `parse: ${e}` };
  }

  let procs = parsed.processes ?? parsed;
  procs = Array.isArray(procs) ? procs : procs ? [procs] : [];

  const ws = procs.reduce((a, p) => a + (Number(p.workingSet) || 0), 0);
  const pb = procs.reduce((a, p) => a + (Number(p.privateBytes) || 0), 0);
  const cpu = cpuMs > 0 ? procs.reduce((a, p) => a + (Number(p.cpuPercent) || 0), 0) : null;
  return {
    ok: true,
    count: procs.length,
    workingSetMB: round1(ws / MIB),
    privateBytesMB: round1(pb / MIB),
    cpuPercent: cpu == null ? null : round1(cpu),
    processes: procs
  };
}
