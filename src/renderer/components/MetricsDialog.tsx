import { useEffect, useState } from 'react';
import { useUI } from '../store/useUI';

// Visible counterpart to the "Performance: Dump metrics" command. The command also
// logs the full snapshot to the console, but this panel surfaces the numbers in the
// app (the console is invisible without DevTools). "Copy JSON" puts the snapshot on
// the clipboard — handy for pasting into the bench harness memory cross-check.
export function MetricsDialog() {
  const data = useUI((s) => s.metricsData);
  const close = useUI((s) => s.closeMetrics);
  const [copied, setCopied] = useState(false);

  useEffect(() => setCopied(false), [data]);

  useEffect(() => {
    if (!data) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        close();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [data, close]);

  if (!data) return null;
  const { snap, liveCtx, paneCount } = data;
  const startup = Object.entries(snap.startupMs);

  const copy = () => {
    const payload = { ...snap, liveWebglContexts: liveCtx, panesMounted: paneCount };
    navigator.clipboard
      .writeText(JSON.stringify(payload, null, 2))
      .then(() => setCopied(true))
      .catch(() => {});
  };

  return (
    <div className="hp-modal-backdrop hp-frosted-backdrop" onMouseDown={close}>
      <div className="hp-modal hp-metrics-modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="hp-modal-title">Performance metrics</div>

        <div className="hp-metrics-summary">
          <span>
            <strong>{snap.totalMemoryMB}</strong> MB total
          </span>
          <span>
            <strong>{snap.processes.length}</strong> processes
          </span>
          <span>
            <strong>{snap.windows}</strong> window{snap.windows === 1 ? '' : 's'}
          </span>
          <span>
            WebGL <strong>{liveCtx}</strong> live / {paneCount} pane{paneCount === 1 ? '' : 's'}
          </span>
        </div>

        {startup.length > 0 && (
          <div className="hp-metrics-block">
            <div className="hp-metrics-h">Startup (ms since process start)</div>
            <div className="hp-metrics-marks">
              {startup.map(([k, v]) => (
                <span key={k}>
                  {k}: <strong>{v}</strong>
                </span>
              ))}
            </div>
          </div>
        )}

        <div className="hp-metrics-block">
          <div className="hp-metrics-h">Memory by process type</div>
          <table className="hp-metrics-table">
            <thead>
              <tr>
                <th>Type</th>
                <th>Count</th>
                <th>MB</th>
              </tr>
            </thead>
            <tbody>
              {snap.byType.map((t) => (
                <tr key={t.type}>
                  <td>{t.type}</td>
                  <td>{t.count}</td>
                  <td>{t.memoryMB}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        <div className="hp-metrics-block">
          <div className="hp-metrics-h">Processes</div>
          <div className="hp-metrics-scroll">
            <table className="hp-metrics-table">
              <thead>
                <tr>
                  <th>Type</th>
                  <th>PID</th>
                  <th>MB</th>
                  <th>CPU%</th>
                </tr>
              </thead>
              <tbody>
                {snap.processes.map((p) => (
                  <tr key={p.pid}>
                    <td>{p.type}</td>
                    <td>{p.pid}</td>
                    <td>{p.memoryMB}</td>
                    <td>{p.cpu}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>

        <div className="hp-modal-actions">
          <button className="hp-btn" onClick={copy}>
            {copied ? 'Copied ✓' : 'Copy JSON'}
          </button>
          <button className="hp-btn hp-btn-primary" onClick={close}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
