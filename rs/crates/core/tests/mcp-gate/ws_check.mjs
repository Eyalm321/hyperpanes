// Live WS /events verification for the Rust control server: hello greeting, scope-filtered
// fan-out (no sibling leak), and a busy→idle `activity` flip. Uses Node globals (fetch + WebSocket).
import { readFileSync } from 'node:fs';

const CONTROL_FILE = process.env.HYPERPANES_CONTROL_FILE || 'C:\\hp-gate\\control.json';
const d = JSON.parse(readFileSync(CONTROL_FILE, 'utf8'));
const BASE = `http://127.0.0.1:${d.port}`;
const AUTH = { authorization: `Bearer ${d.token}`, 'content-type': 'application/json' };

let pass = 0,
  fail = 0;
const assert = (cond, name, detail) => {
  console.log(`${cond ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
  cond ? pass++ : fail++;
};

const api = (method, path, body) =>
  fetch(BASE + path, { method, headers: AUTH, body: body ? JSON.stringify(body) : undefined }).then(async (r) => ({
    status: r.status,
    body: await r.json().catch(() => ({})),
  }));

// Collect frames from a WS until `ms` elapses.
function listen(url, ms) {
  return new Promise((resolve) => {
    const frames = [];
    const ws = new WebSocket(url);
    ws.onmessage = (e) => frames.push(JSON.parse(e.data));
    setTimeout(() => {
      try {
        ws.close();
      } catch {}
      resolve(frames);
    }, ms);
  });
}

function open(url) {
  return new Promise((resolve, reject) => {
    const frames = [];
    const ws = new WebSocket(url);
    ws.onmessage = (e) => frames.push(JSON.parse(e.data));
    ws.onopen = () => resolve({ ws, frames });
    ws.onerror = (e) => reject(e);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  // A pre-existing pane to scope a "sibling" token to (its banner already flushed pre-connect).
  const b = await api('POST', '/command', { type: 'newPane', windowId: 1, pane: { command: 'cmd', label: 'wsB' } });
  const paneB = b.body.result;
  const mintB = await api('POST', '/tokens', { scope: { paneIds: [paneB] } });
  const tokenB = mintB.body.token;
  assert(typeof paneB === 'string' && tokenB?.length === 64, 'spawned sibling pane B + scoped-B token');

  // 1. hello greeting on the master stream.
  const master = await open(d.events);
  await sleep(150);
  assert(master.frames.some((f) => f.type === 'hello' && f.pid === d.pid), 'master WS hello frame', JSON.stringify(master.frames[0]));

  // 2. a sibling-scoped stream (pane B) — receives its own hello.
  const scopedB = await open(`ws://127.0.0.1:${d.port}/events?token=${tokenB}`);
  await sleep(150);
  assert(scopedB.frames.some((f) => f.type === 'hello'), 'scoped (pane-B) WS hello frame');
  master.frames.length = 0;
  scopedB.frames.length = 0;

  // 3. Spawn pane A WHILE the streams are connected → its shell banner output flushes live, so we
  //    can verify output-frame delivery + scope filtering without depending on conpty input echo.
  const a = await api('POST', '/command', { type: 'newPane', windowId: 1, pane: { command: 'cmd', label: 'wsA' } });
  const paneA = a.body.result;
  const mintA = await api('POST', '/tokens', { scope: { paneIds: [paneA] } });
  const scopedA = await open(`ws://127.0.0.1:${d.port}/events?token=${mintA.body.token}`);
  await sleep(2500);

  const masterSawA = master.frames.some((f) => f.type === 'output' && f.paneId === paneA);
  const scopedBSawA = scopedB.frames.some((f) => f.paneId === paneA);
  assert(masterSawA, 'master stream got pane-A output frames (live spawn)', `masterFrames=${master.frames.length}`);
  assert(!scopedBSawA, 'sibling-scoped (pane-B) stream got NO pane-A frames (no leak)', `scopedBFrames=${scopedB.frames.length}`);

  // 4. activity busy→idle flip (idle threshold 10s): pane A's spawn output goes quiet → idle.
  await sleep(11000);
  const masterIdle = master.frames.find((f) => f.type === 'activity' && f.paneId === paneA && f.activity === 'idle');
  const scopedAIdle = scopedA.frames.find((f) => f.type === 'activity' && f.paneId === paneA && f.activity === 'idle');
  const scopedBIdle = scopedB.frames.find((f) => f.type === 'activity' && f.paneId === paneA);
  assert(!!masterIdle, 'activity busy→idle flip for pane A on master stream', JSON.stringify(masterIdle));
  assert(!!scopedAIdle, 'pane-A-scoped stream received its own activity flip');
  assert(!scopedBIdle, 'sibling-scoped stream got NO pane-A activity frame (no leak)');

  try {
    master.ws.close();
    scopedA.ws.close();
    scopedB.ws.close();
  } catch {}
  await api('POST', '/command', { type: 'closePane', paneId: paneA });
  await api('POST', '/command', { type: 'closePane', paneId: paneB });

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error('WS CHECK ERROR:', e);
  process.exit(2);
});
