import { app, BrowserWindow } from 'electron';
import type { MetricsSnapshot } from '../renderer/types';

// Cold-start instrumentation. `t0` is captured when this module is first imported
// — done at the very top of main/index.ts, so it's ~process start. `mark()` records
// named milestones (whenReady, first window shown / renderer loaded) once each, so
// "Performance: Dump metrics" can report startup cost without an external profiler.
const t0 = process.hrtime.bigint();
const marks: Record<string, number> = {};

export function mark(name: string): void {
  if (marks[name] == null) {
    marks[name] = Math.round(Number(process.hrtime.bigint() - t0) / 1e6);
  }
}

// A point-in-time memory/process snapshot from app.getAppMetrics(). workingSetSize
// is reported in KB; we roll it up per process type (Browser / GPU / Tab / Utility)
// and overall so the dump shows where Electron's footprint actually goes.
export function collectMetrics(): MetricsSnapshot {
  const metrics = app.getAppMetrics();
  const byType = new Map<string, { count: number; memoryKB: number }>();
  let totalKB = 0;

  const processes = metrics.map((m) => {
    const memoryKB = m.memory.workingSetSize; // KB
    totalKB += memoryKB;
    const agg = byType.get(m.type) ?? { count: 0, memoryKB: 0 };
    agg.count += 1;
    agg.memoryKB += memoryKB;
    byType.set(m.type, agg);
    return {
      type: m.type,
      pid: m.pid,
      memoryMB: Math.round(memoryKB / 1024),
      cpu: Math.round((m.cpu?.percentCPUUsage ?? 0) * 10) / 10
    };
  });

  return {
    startupMs: { ...marks },
    windows: BrowserWindow.getAllWindows().filter((w) => !w.isDestroyed()).length,
    totalMemoryMB: Math.round(totalKB / 1024),
    byType: [...byType]
      .map(([type, v]) => ({ type, count: v.count, memoryMB: Math.round(v.memoryKB / 1024) }))
      .sort((a, b) => b.memoryMB - a.memoryMB),
    processes: processes.sort((a, b) => b.memoryMB - a.memoryMB)
  };
}
