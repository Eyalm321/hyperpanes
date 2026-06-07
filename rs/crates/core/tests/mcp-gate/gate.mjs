// MCP acceptance gate for the Rust control server.
// Spawns the REAL hyperpanes MCP server (dist/index.js) via the MCP SDK stdio client, pointed at
// the Rust headless daemon through HYPERPANES_CONTROL_FILE, and drives the documented smoke
// sequence. Any route/JSON/event/discovery drift makes a tool throw or return wrong data → FAIL.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';

const CONTROL_FILE = process.env.HYPERPANES_CONTROL_FILE || 'C:\\hp-gate\\control.json';
const SERVER = 'C:\\hyperpanes-mcp\\dist\\index.js';

let pass = 0,
  fail = 0;
const log = (ok, name, detail) => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
  ok ? pass++ : fail++;
};
const assert = (cond, name, detail) => log(!!cond, name, detail);

function mkClient(extraEnv = {}) {
  const transport = new StdioClientTransport({
    command: process.execPath,
    args: [SERVER],
    env: {
      ...process.env,
      HYPERPANES_CONTROL_FILE: CONTROL_FILE,
      HYPERPANES_ALLOW_INPUT: '1',
      ...extraEnv,
    },
  });
  const client = new Client({ name: 'gate', version: '1.0.0' }, { capabilities: {} });
  return { client, transport };
}

async function call(client, name, args = {}) {
  const res = await client.callTool({ name, arguments: args });
  const text = res.content?.[0]?.text ?? '{}';
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    data = { _raw: text };
  }
  return { data, isError: !!res.isError };
}

async function main() {
  const { client, transport } = mkClient();
  await client.connect(transport);

  // 0. control_status — reachability + health/state round-trip.
  {
    const { data } = await call(client, 'control_status');
    assert(data.available === true && data.appAllowsInput === true, 'control_status reachable + allowInput', `port=${data.port} pid=${data.pid} v=${data.version}`);
  }

  // 1. open_pane → returns a real paneId (the /command round-trip + waitForPane).
  let paneId;
  {
    const { data, isError } = await call(client, 'open_pane', {
      command: 'cmd',
      label: 'gate-worker',
      meta: { role: 'worker' },
    });
    paneId = data.paneId;
    assert(!isError && typeof paneId === 'string' && data.ready === true, 'open_pane → paneId', `paneId=${paneId} ready=${data.ready}`);
  }

  // 2. list_panes shows the new pane with its activity + meta.
  {
    const { data } = await call(client, 'list_panes');
    const p = data.panes?.find((x) => x.paneId === paneId);
    assert(p && p.label === 'gate-worker' && p.meta?.role === 'worker' && ['busy', 'idle', 'exited'].includes(p.activity), 'list_panes (meta + activity)', `activity=${p?.activity}`);
  }

  // 3. read_pane mode:"screen" + waitForIdle — rendered transcript, settles on quiescence.
  {
    const { data, isError } = await call(client, 'read_pane', { paneId, mode: 'screen', waitForIdle: true, settleMs: 400, timeoutMs: 8000 });
    assert(!isError && data.mode === 'screen' && data.waited === true && typeof data.cursor === 'number', 'read_pane screen+waitForIdle', `settled=${data.settled} awaitingInput=${data.awaitingInput} cursorType=${typeof data.cursor}`);
  }

  // 4. read_pane ?strip=1 — clean (de-ANSI'd) raw output with a byte cursor.
  {
    const { data, isError } = await call(client, 'read_pane', { paneId, strip: true });
    assert(!isError && data.stripped === true && typeof data.cursor === 'number', 'read_pane strip=1', `cursor=${data.cursor}`);
  }

  // 5. set_meta — synchronous TRUE merged echo (no /state re-read race).
  {
    const { data } = await call(client, 'set_meta', { paneId, meta: { task: 'gate', role: null } });
    assert(data.meta?.task === 'gate' && data.meta?.role === undefined, 'set_meta merged echo (set + delete)', JSON.stringify(data.meta));
  }

  // 6. whoami via explicit paneId — resolveWhoami over /state.
  {
    const { data } = await call(client, 'whoami', { paneId });
    assert(data.ok === true && data.paneId === paneId && data.task === 'gate', 'whoami(paneId) self-description', `window=${data.windowId} tab=${data.tabId}`);
  }

  // 7. durable inbox — send_message then read_messages (cursor + dropped accounting).
  {
    const sent1 = await call(client, 'send_message', { to: paneId, from: 'orchestrator', body: 'first' });
    const sent2 = await call(client, 'send_message', { to: paneId, from: 'orchestrator', body: 'second' });
    const seq1 = sent1.data.seq; // seq is GLOBAL monotonic, not per-pane (durable-inbox contract)
    const seq2 = sent2.data.seq;
    const { data } = await call(client, 'read_messages', { paneId });
    const after = await call(client, 'read_messages', { paneId, after: seq1 });
    assert(
      data.messages?.length === 2 && data.latestSeq === seq2 && after.data.messages?.length === 1 && after.data.messages[0].body === 'second',
      'durable inbox send/read + cursor',
      `latestSeq=${data.latestSeq} afterCursorReturns=${after.data.messages?.length}`
    );
  }

  // 8. advisory lock — owner writes, non-owner is refused, release frees it.
  {
    const lock = await call(client, 'lock_pane', { paneId, owner: 'mgrA', ttlMs: 60000 });
    assert(lock.data.ok === true && lock.data.owner === 'mgrA', 'lock_pane acquire', `expiresAt=${lock.data.expiresAt}`);
    // Non-owner send_input → refused with a lock error (423 surfaced as a thrown tool error).
    const blocked = await call(client, 'send_input', { paneId, data: 'x', confirm: true, owner: 'mgrB' });
    assert(blocked.isError && /locked/i.test(blocked.data.error || ''), 'send_input refused for non-owner', blocked.data.error);
    // Owner send_input → allowed.
    const owned = await call(client, 'send_input', { paneId, data: '', confirm: true, owner: 'mgrA' });
    assert(owned.data.ok === true, 'send_input allowed for lock owner', JSON.stringify(owned.data));
    const unlock = await call(client, 'unlock_pane', { paneId, owner: 'mgrA' });
    assert(unlock.data.ok === true, 'unlock_pane', JSON.stringify(unlock.data));
  }

  // 9. scoping — mint a pane-scoped token; escalation to the window is refused (no-escalation).
  let scopedToken, port;
  {
    const minted = await call(client, 'mint_token', { paneIds: [paneId] });
    scopedToken = minted.data.token;
    port = minted.data.port;
    assert(minted.data.ok === true && typeof scopedToken === 'string' && scopedToken.length === 64 && /\/events\?token=/.test(minted.data.events || ''), 'mint_token (pane scope)', `port=${port}`);
  }

  // 10. scoped /state subtree filter + escalation 403, driven through a SCOPED bridge (env token).
  {
    const { client: sc, transport: st } = mkClient({
      HYPERPANES_CONTROL_FILE: '', // force env-token discovery (no master control.json)
      HYPERPANES_CONTROL_TOKEN: scopedToken,
      HYPERPANES_CONTROL_PORT: String(port),
      HYPERPANES_PANE_ID: paneId,
    });
    await sc.connect(st);
    // The scoped bridge sees ONLY its pane in /state.
    const list = await call(sc, 'list_panes');
    const ids = (list.data.panes || []).map((p) => p.paneId);
    assert(ids.length === 1 && ids[0] === paneId, 'scoped /state subtree (no sibling leak)', `panes=${ids.length}`);
    // whoami-from-env: the scoped bridge identifies its own pane from HYPERPANES_PANE_ID.
    const who = await call(sc, 'whoami');
    assert(who.data.ok === true && who.data.paneId === paneId, 'whoami-from-env (HYPERPANES_PANE_ID)', who.data.paneId);
    // Escalation: minting a window-scoped token from the scoped bridge → 403.
    const esc = await call(sc, 'mint_token', { windowIds: [1] });
    assert(esc.isError && /scope|403|outside/i.test(esc.data.error || ''), 'escalation mint → 403', esc.data.error);
    await sc.close();
  }

  // 11. open a sibling pane, then confirm the FIRST scoped token still cannot see it (no leak).
  {
    const sib = await call(client, 'open_pane', { command: 'cmd', label: 'sibling' });
    const sibId = sib.data.paneId;
    const { client: sc, transport: st } = mkClient({
      HYPERPANES_CONTROL_FILE: '',
      HYPERPANES_CONTROL_TOKEN: scopedToken,
      HYPERPANES_CONTROL_PORT: String(port),
    });
    await sc.connect(st);
    const list = await call(sc, 'list_panes');
    const ids = (list.data.panes || []).map((p) => p.paneId);
    assert(!ids.includes(sibId) && ids.length === 1, 'scoped token excludes a newly-spawned sibling', `sees=${ids.length}`);
    await sc.close();
    await call(client, 'close_pane', { paneId: sibId });
  }

  // Clean up the worker pane.
  await call(client, 'close_pane', { paneId });
  await client.close();

  console.log(`\n${pass} passed, ${fail} failed`);
  process.exit(fail === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error('GATE HARNESS ERROR:', e);
  process.exit(2);
});
