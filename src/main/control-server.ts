import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { writeFileSync, rmSync, readFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import type { Duplex } from 'node:stream';
import { WebSocketServer, WebSocket } from 'ws';
import { app } from 'electron';
import { MessageInbox } from './control-inbox';
import { PaneLocks } from './control-lock';
import { stripAnsi } from './ansi-strip';
import { submitNewlines } from './control-input';
import {
  paneInScope,
  windowInScope,
  tabInScope,
  checkMintable,
  coerceScope,
  type Scope,
  type PaneCoords
} from './control-scope';

// ---------------------------------------------------------------------------
// Local control API (M2). A loopback HTTP server that lets an external process
// (the MCP server, Phase 2) read pane structure + output and — when explicitly
// allowed — send input and structural commands. Security posture:
//   • bound to 127.0.0.1 only (never a routable interface);
//   • every request needs the per-instance bearer token from control.json;
//   • DISABLED by default — nothing listens until the user turns it on;
//   • `sendInput` is gated a second time behind `allowInput` (also default off),
//     because typing into a live shell is the sharp edge.
// A `/events` WebSocket (same port + token) pushes pane output deltas, exits, and
// structure-change pings so the MCP server can back resource subscriptions; the
// poll route `GET /panes/:id/output` is still there for the initial backlog.
// ---------------------------------------------------------------------------

export interface ControlPaneInfo {
  id: string;
  sessionUid: string;
  label: string;
  color: string;
  command?: string;
  cwd?: string;
  shell?: string;
  status: 'running' | 'exited';
  exitCode?: number;
  // Liveness for orchestration (agent-orchestration B). HEURISTIC: 'idle' = no
  // pty output for the idle threshold (the agent is likely waiting at its
  // prompt / done), 'busy' = recently producing output, 'exited' = process gone.
  // Not a contract that work is complete — see the orchestration plan's risks.
  activity: 'busy' | 'idle' | 'exited';
  // Free-form per-pane metadata. Reserved keys give an agent org its shape:
  // `role`, `parent`, `agentType`, `task` (agent-orchestration C).
  meta?: Record<string, string>;
}

export interface ControlTabInfo {
  id: string;
  title: string;
  layout: string;
  panes: ControlPaneInfo[];
}

// What a renderer publishes about its own window (main stamps the windowId).
export interface ControlWindowPayload {
  activeTabId: string | null;
  tabs: ControlTabInfo[];
}

export interface ControlWindowState extends ControlWindowPayload {
  windowId: number;
}

export interface ControlCommand {
  type: string;
  paneId?: string;
  windowId?: number;
  // Set by main when dispatching, so the renderer can reply with the command's
  // result (e.g. a new pane id) and the HTTP /command response can carry it (D).
  correlationId?: string;
  [key: string]: unknown;
}

// Side-effects the server needs from the rest of main. Kept as plain callbacks so
// the server has no direct dependency on SessionManager / BrowserWindow and stays
// unit-testable.
export interface ControlDeps {
  readOutput: (sessionUid: string) => string | null;
  sendInput: (sessionUid: string, data: string) => void;
  // Forward a structural command to a window's renderer and resolve with its
  // result. `ok:false` ⇒ failure: no such window, or `timedOut` (renderer never
  // replied), or `error` (the store action threw). `result` carries a
  // command-specific value (newPane → the new pane id); undefined for result-less
  // commands (D, #2/#3).
  dispatchCommand: (
    windowId: number,
    command: ControlCommand
  ) => Promise<{ ok: boolean; result?: unknown; timedOut?: boolean; error?: string }>;
  onActiveChange?: (active: boolean) => void;
}

interface ControlSettings {
  enabled: boolean;
  allowInput: boolean;
}

const DEFAULT_SETTINGS: ControlSettings = { enabled: false, allowInput: false };

export interface ControlStatus extends ControlSettings {
  running: boolean;
  port: number | null;
}

// Events pushed over the `/events` WebSocket. `output`/`exit` carry the addressing
// sessionUid plus the paneId when it's resolvable from the read-model; `state` is
// a bare "structure changed, re-fetch /state" ping; `hello` is the greeting;
// `message` is a live nudge for the durable message bus (agent-orchestration E).
// Pane-addressed frames (output/exit/activity/message) are scope-filtered per
// client — a scoped token only sees frames for panes in its scope.
export type ControlEvent =
  | { type: 'hello'; pid: number; version: string }
  | { type: 'output'; sessionUid: string; paneId: string | null; data: string }
  | { type: 'exit'; sessionUid: string; paneId: string | null; code: number }
  | { type: 'activity'; paneId: string; activity: 'busy' | 'idle' | 'exited' }
  | { type: 'message'; to: string; from: string; seq: number; body: string }
  | { type: 'state' };

// A bearer token's authority: null scope = master (unscoped); a Scope limits it.
interface TokenInfo {
  scope: Scope | null;
  expiresAt?: number; // ms epoch; absent = no expiry (the master token)
}

export class ControlServer {
  private windows = new Map<number, ControlWindowState>();
  // Reverse indexes over the read-model, rebuilt only when structure changes
  // (see reindex). This keeps the hot paths — `emitOutput` per pty batch, and
  // per-request pane lookups — O(1) Map gets instead of a windows→tabs→panes walk.
  private uidToPane = new Map<string, string>();
  private paneIndex = new Map<string, { windowId: number; tabId: string; pane: ControlPaneInfo }>();
  // tabId → its owning windowId, for scope checks on tab-targeted commands.
  private tabToWindow = new Map<string, number>();
  // Per-window structural fingerprint (everything EXCEPT each pane's `activity`).
  // Lets setWindowState tell a pure busy⇄idle flip from a real structure change,
  // so a liveness-only publish doesn't force every client to re-GET /state (#13).
  private windowSig = new Map<number, string>();
  private server: Server | null = null;
  private wss: WebSocketServer | null = null;
  private port: number | null = null;
  private token: string | null = null; // master (unscoped) token, written to control.json
  // Minted scoped tokens (agent-orchestration F). The master token above is the
  // unscoped root; these are subtree-limited and may carry a TTL.
  private scopedTokens = new Map<string, TokenInfo>();
  // Per-connection scope, so pane-addressed events are filtered to each client's
  // authority. A master client maps to null (sees everything).
  private wsScopes = new WeakMap<WebSocket, Scope | null>();
  // Durable per-pane message bus + advisory write locks (agent-orchestration E/H).
  private inbox = new MessageInbox();
  private locks = new PaneLocks();
  private settings: ControlSettings;
  private stateTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(private deps: ControlDeps) {
    this.settings = this.loadSettings();
  }

  // Start listening if enabled in persisted settings (called once on launch).
  init(): void {
    if (this.settings.enabled) this.start();
  }

  // ---- read-model maintenance (fed by renderers via control:publishState) ----
  setWindowState(windowId: number, payload: ControlWindowPayload): void {
    // Hold the prior index + structural fingerprint, then rebuild. One pass over
    // the new index does double duty: emit per-pane `activity` events on each flip
    // (busy⇄idle⇄exited — the highest-leverage orchestration signal) AND let the
    // structural diff decide whether a `state` ping is even warranted. (#11/#12/#13)
    const prevIndex = this.paneIndex;
    const prevSig = this.windowSig.get(windowId);
    const sig = this.structuralSig(payload);
    this.windows.set(windowId, { windowId, ...payload });
    this.windowSig.set(windowId, sig);
    this.reindex(); // assigns a fresh paneIndex, leaving prevIndex intact to diff against
    // Per-pane activity events on each flip of a KNOWN pane (new panes ride the
    // `state` ping below). Skipped entirely when nobody is streaming (#12).
    if (this.hasClients()) {
      for (const [paneId, { pane }] of this.paneIndex) {
        const before = prevIndex.get(paneId);
        if (before && before.pane.activity !== pane.activity) {
          this.broadcastForPane(paneId, { type: 'activity', paneId, activity: pane.activity });
        }
      }
    }
    // Only ping `state` on a STRUCTURAL change; a pure busy⇄idle flip is already
    // carried by the activity events above, so it no longer forces every client to
    // re-GET /state (#13).
    if (prevSig !== sig) this.notifyState();
  }

  // A window's structure fingerprint, excluding each pane's `activity` (which
  // flips on liveness, not structure). Cheap stable JSON — same object shape each
  // call, so key order is stable. Used to suppress redundant `state` pings (#13).
  private structuralSig(payload: ControlWindowPayload): string {
    return JSON.stringify({
      activeTabId: payload.activeTabId,
      tabs: payload.tabs.map((t) => ({
        id: t.id,
        title: t.title,
        layout: t.layout,
        panes: t.panes.map((p) => ({ ...p, activity: undefined }))
      }))
    });
  }

  dropWindow(windowId: number): void {
    if (this.windows.delete(windowId)) {
      this.windowSig.delete(windowId);
      this.reindex();
      this.notifyState();
    }
  }

  // Flatten the windows→tabs→panes tree into fresh lookup maps. Runs only on a
  // structure change (a window publishing / closing), never on the output path.
  // Builds new maps and reassigns (rather than clearing in place) so a caller can
  // hold the prior index to diff against (see setWindowState).
  private reindex(): void {
    const uidToPane = new Map<string, string>();
    const paneIndex = new Map<string, { windowId: number; tabId: string; pane: ControlPaneInfo }>();
    const tabToWindow = new Map<string, number>();
    for (const st of this.windows.values()) {
      for (const tab of st.tabs) {
        tabToWindow.set(tab.id, st.windowId);
        for (const pane of tab.panes) {
          uidToPane.set(pane.sessionUid, pane.id);
          paneIndex.set(pane.id, { windowId: st.windowId, tabId: tab.id, pane });
        }
      }
    }
    this.uidToPane = uidToPane;
    this.paneIndex = paneIndex;
    this.tabToWindow = tabToWindow;
  }

  // A pane's addressing coordinates, for scope checks (null if unknown).
  private paneCoords(paneId: string): PaneCoords | null {
    const found = this.paneIndex.get(paneId);
    return found ? { paneId, tabId: found.tabId, windowId: found.windowId } : null;
  }

  getState(): { windows: ControlWindowState[] } {
    return { windows: [...this.windows.values()] };
  }

  // `/state` filtered to a scope: keep only in-scope panes, dropping tabs/windows
  // left empty. A null scope (master) returns everything verbatim.
  private stateForScope(scope: Scope | null): { windows: ControlWindowState[] } {
    if (!scope) return this.getState();
    const windows: ControlWindowState[] = [];
    for (const st of this.windows.values()) {
      const tabs = st.tabs
        .map((tab) => ({
          ...tab,
          panes: tab.panes.filter((p) =>
            paneInScope(scope, { paneId: p.id, tabId: tab.id, windowId: st.windowId })
          )
        }))
        .filter((tab) => tab.panes.length > 0);
      if (tabs.length > 0) windows.push({ ...st, tabs });
    }
    return { windows };
  }

  // ---- event stream (fed from ipc's session handlers) ----
  // True only while at least one /events client is connected, so the hot pty
  // output path can bail before doing any work when nobody is streaming.
  private hasClients(): boolean {
    return !!this.wss && this.wss.clients.size > 0;
  }

  emitOutput(sessionUid: string, data: string): void {
    if (!this.hasClients()) return;
    const paneId = this.paneIdForUid(sessionUid);
    this.broadcastForPane(paneId, { type: 'output', sessionUid, paneId, data });
  }

  emitExit(sessionUid: string, code: number): void {
    if (!this.hasClients()) return;
    const paneId = this.paneIdForUid(sessionUid);
    this.broadcastForPane(paneId, { type: 'exit', sessionUid, paneId, code });
  }

  // ---- message bus (agent-orchestration E) ----
  // Enqueue a message to a pane's durable inbox and nudge live, in-scope clients.
  // Returns the stored message (seq lets a reader advance its cursor).
  postMessage(to: string, from: string, body: string) {
    const msg = this.inbox.post(to, from, body, Date.now());
    this.broadcastForPane(to, { type: 'message', to, from, seq: msg.seq, body });
    return msg;
  }

  readMessages(paneId: string, afterSeq = 0) {
    return {
      messages: this.inbox.read(paneId, afterSeq),
      dropped: this.inbox.droppedCount(paneId),
      latestSeq: this.inbox.latestSeq(paneId)
    };
  }

  // Coalesce structure changes into one "re-fetch /state" ping per tick.
  private notifyState(): void {
    if (!this.hasClients() || this.stateTimer) return;
    this.stateTimer = setTimeout(() => {
      this.stateTimer = null;
      this.broadcast({ type: 'state' });
    }, 100);
  }

  // Send to every client (used for structure-only `state` pings — each client
  // then GETs its own scope-filtered /state). Serialize once, not per client (#14).
  private broadcast(event: ControlEvent): void {
    if (!this.wss) return;
    const json = JSON.stringify(event);
    for (const ws of this.wss.clients) this.send(ws, json);
  }

  // Send a pane-addressed event only to clients whose scope includes that pane.
  // A master client (null scope) always receives it; an unknown pane (no coords)
  // is treated as master-only so a scoped client never sees an unresolvable pane.
  // The frame is serialized once, lazily on the first eligible client (#14).
  private broadcastForPane(paneId: string | null, event: ControlEvent): void {
    if (!this.wss) return;
    const coords = paneId ? this.paneCoords(paneId) : null;
    let json: string | null = null;
    for (const ws of this.wss.clients) {
      const scope = this.wsScopes.get(ws) ?? null;
      if (!scope || (coords && paneInScope(scope, coords))) {
        if (json === null) json = JSON.stringify(event);
        this.send(ws, json);
      }
    }
  }

  private send(ws: WebSocket, event: ControlEvent | string): void {
    if (ws.readyState !== WebSocket.OPEN) return;
    ws.send(typeof event === 'string' ? event : JSON.stringify(event));
  }

  private paneIdForUid(sessionUid: string): string | null {
    return this.uidToPane.get(sessionUid) ?? null;
  }

  // ---- enable / input toggles (persisted; default off) ----
  status(): ControlStatus {
    return { ...this.settings, running: !!this.server, port: this.port };
  }

  setEnabled(enabled: boolean): ControlStatus {
    if (enabled === this.settings.enabled) return this.status();
    this.settings.enabled = enabled;
    this.saveSettings();
    if (enabled) this.start();
    else this.stop();
    this.deps.onActiveChange?.(enabled);
    return this.status();
  }

  setAllowInput(allow: boolean): ControlStatus {
    this.settings.allowInput = allow;
    this.saveSettings();
    return this.status();
  }

  // Tear down on quit so a stale control.json never points at a dead port.
  shutdown(): void {
    this.stop();
  }

  // ---- lifecycle ----
  private start(): void {
    if (this.server) return;
    this.token = randomBytes(32).toString('hex');
    const server = createServer((req, res) => this.handle(req, res));
    server.on('error', (err) => {
      console.error('control server error', err);
      this.stop();
    });

    // `/events` WebSocket on the same port. noServer mode so we authenticate the
    // upgrade ourselves before completing the handshake. Token comes from the
    // `?token=` query (WebSocket clients can't set Authorization reliably) or a
    // Bearer header.
    this.wss = new WebSocketServer({ noServer: true });
    server.on('upgrade', (req, socket: Duplex, head) => {
      const url = new URL(req.url ?? '/', 'http://127.0.0.1');
      if (url.pathname !== '/events') return socket.destroy();
      const token = url.searchParams.get('token') ?? this.bearer(req);
      // Resolve the token's authority up front; a scoped token's stream is then
      // filtered to its panes (broadcastForPane). Unknown/expired ⇒ 401.
      const info = this.resolveToken(token);
      if (!info) {
        socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
        return socket.destroy();
      }
      this.wss!.handleUpgrade(req, socket, head, (ws) => {
        this.wsScopes.set(ws, info.scope);
        this.send(ws, { type: 'hello', pid: process.pid, version: app.getVersion() });
      });
    });

    // Ephemeral port on loopback only.
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address();
      this.port = addr && typeof addr === 'object' ? addr.port : null;
      this.writeDiscovery();
    });
    this.server = server;
  }

  private stop(): void {
    if (this.stateTimer) {
      clearTimeout(this.stateTimer);
      this.stateTimer = null;
    }
    if (this.wss) {
      for (const ws of this.wss.clients) ws.terminate();
      this.wss.close();
      this.wss = null;
    }
    if (this.server) {
      this.server.close();
      this.server = null;
    }
    this.port = null;
    this.token = null;
    // Minted tokens die with the server (a new run mints a fresh master token).
    this.scopedTokens.clear();
    this.removeDiscovery();
  }

  // ---- request handling ----
  private handle(req: IncomingMessage, res: ServerResponse): void {
    const send = (code: number, body: unknown) => {
      const json = JSON.stringify(body);
      res.writeHead(code, { 'content-type': 'application/json' });
      res.end(json);
    };

    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    const path = url.pathname;

    // /health is the only unauthenticated route (discovery handshake).
    if (path === '/health' && req.method === 'GET') {
      return send(200, {
        ok: true,
        app: 'hyperpanes',
        pid: process.pid,
        version: app.getVersion(),
        allowInput: this.settings.allowInput
      });
    }

    // Every other route needs a valid token; a scoped one limits what it reaches.
    const auth = this.resolveToken(this.bearer(req));
    if (!auth) return send(401, { error: 'unauthorized' });
    const scope = auth.scope;

    if (path === '/state' && req.method === 'GET') {
      return send(200, this.stateForScope(scope));
    }

    // Mint a narrower (subtree-scoped) token a parent hands a child via env (F).
    if (path === '/tokens' && req.method === 'POST') {
      return this.readBody(req, (body) => {
        const requested = coerceScope(body?.scope);
        if (!requested) {
          return send(400, { error: 'expected { scope: { windowIds?|tabIds?|paneIds? }, ttlMs? }' });
        }
        const problem = this.canMint(scope, requested);
        if (problem) return send(403, { error: problem });
        const ttlMs = typeof body?.ttlMs === 'number' && body.ttlMs > 0 ? body.ttlMs : undefined;
        const minted = this.mintToken(requested, ttlMs);
        return send(200, {
          ok: true,
          token: minted.token,
          scope: requested,
          expiresAt: minted.expiresAt ?? null,
          port: this.port,
          events: this.port ? `ws://127.0.0.1:${this.port}/events?token=${minted.token}` : null
        });
      });
    }

    // /panes/:id/(output|input|messages|lock)
    const paneMatch = /^\/panes\/([^/]+)\/(output|input|messages|lock)$/.exec(path);
    if (paneMatch) {
      const paneId = decodeURIComponent(paneMatch[1]);
      const found = this.findPane(paneId);
      if (!found) return send(404, { error: 'no such pane', paneId });
      // Scope gate: a scoped token may only act on panes within its scope.
      if (!paneInScope(scope, { paneId, tabId: found.tabId, windowId: found.windowId })) {
        return send(403, { error: 'pane out of scope', paneId });
      }
      const kind = paneMatch[2];

      if (kind === 'output' && req.method === 'GET') {
        const raw = this.deps.readOutput(found.pane.sessionUid) ?? '';
        const strip = url.searchParams.get('strip') === '1';
        const cleaned = strip ? stripAnsi(raw) : raw; // clean output mode (G)
        const tail = Number(url.searchParams.get('tail'));
        const output =
          Number.isFinite(tail) && tail > 0 ? cleaned.split('\n').slice(-tail).join('\n') : cleaned;
        return send(200, { paneId, status: found.pane.status, stripped: strip, output });
      }

      if (kind === 'input' && req.method === 'POST') {
        if (!this.settings.allowInput) return send(403, { error: 'input not allowed' });
        return this.readBody(req, (body) => {
          const data = typeof body?.data === 'string' ? body.data : null;
          if (data == null) return send(400, { error: 'expected { data: string }' });
          // Advisory write lock (H): if someone else holds it, refuse this write.
          const owner = typeof body?.owner === 'string' ? body.owner : null;
          const holder = this.locks.holder(paneId, Date.now());
          if (holder && holder !== owner) return send(423, { error: 'pane locked', owner: holder });
          // On Windows conpty a bare "\n" types but doesn't submit; normalize to CR.
          this.deps.sendInput(found.pane.sessionUid, submitNewlines(data));
          send(200, { ok: true });
        });
      }

      // Durable message bus (E): read past a cursor, or enqueue.
      if (kind === 'messages' && req.method === 'GET') {
        const after = Number(url.searchParams.get('after'));
        return send(200, {
          paneId,
          ...this.readMessages(paneId, Number.isFinite(after) && after > 0 ? after : 0)
        });
      }
      if (kind === 'messages' && req.method === 'POST') {
        return this.readBody(req, (body) => {
          const from = typeof body?.from === 'string' ? body.from : 'unknown';
          const msgBody = typeof body?.body === 'string' ? body.body : null;
          if (msgBody == null) return send(400, { error: 'expected { from?, body: string }' });
          const msg = this.postMessage(paneId, from, msgBody);
          send(200, { ok: true, seq: msg.seq });
        });
      }

      // Advisory lock (H): acquire/renew (POST) or release (DELETE).
      if (kind === 'lock' && req.method === 'POST') {
        return this.readBody(req, (body) => {
          const owner = typeof body?.owner === 'string' ? body.owner : null;
          if (!owner) return send(400, { error: 'expected { owner: string, ttlMs? }' });
          const ttlMs = typeof body?.ttlMs === 'number' && body.ttlMs > 0 ? body.ttlMs : 30_000;
          const r = this.locks.acquire(paneId, owner, Date.now(), ttlMs);
          send(r.ok ? 200 : 423, r.ok ? r : { ...r, error: 'held' });
        });
      }
      if (kind === 'lock' && req.method === 'DELETE') {
        return this.readBody(req, (body) => {
          const owner = typeof body?.owner === 'string' ? body.owner : null;
          if (!owner) return send(400, { error: 'expected { owner: string }' });
          const ok = this.locks.release(paneId, owner, Date.now());
          send(ok ? 200 : 423, ok ? { ok: true } : { ok: false, error: 'not the lock holder' });
        });
      }
      return send(405, { error: 'method not allowed' });
    }

    if (path === '/command' && req.method === 'POST') {
      return this.readBody(req, (body) => {
        const cmd = body as ControlCommand | null;
        if (!cmd || typeof cmd.type !== 'string') {
          return send(400, { error: 'expected { type: string, … }' });
        }
        // Scope gate on the command's target (pane / tab / window).
        const denied = this.commandScopeError(scope, cmd);
        if (denied) return send(403, { error: denied });
        // Resolve a target window: explicit windowId, else the pane's window.
        let windowId = typeof cmd.windowId === 'number' ? cmd.windowId : undefined;
        if (windowId == null && cmd.paneId) windowId = this.findPane(cmd.paneId)?.windowId;
        if (windowId == null) return send(400, { error: 'command needs a paneId or windowId' });
        // Request/response: the renderer replies with the command's result (e.g.
        // a new pane id), which we surface in the HTTP response. A failed dispatch
        // is reported with a distinct status so a caller (e.g. open_pane) sees a
        // real failure instead of a phantom success (D, #2):
        //   • timedOut → 504 (renderer wedged / never replied)
        //   • error    → 500 (the store action threw)
        //   • neither  → 404 (no such window)
        this.deps.dispatchCommand(windowId, cmd).then(
          (r) => {
            if (r.ok) {
              return send(200, { ok: true, ...(r.result !== undefined ? { result: r.result } : {}) });
            }
            if (r.timedOut) return send(504, { error: 'command timed out (no renderer reply)', windowId });
            if (r.error) return send(500, { error: r.error });
            return send(404, { error: 'window not found', windowId });
          },
          () => send(500, { error: 'command dispatch failed' })
        );
      });
    }

    return send(404, { error: 'not found', path });
  }

  // ---- scoping (agent-orchestration F) ----
  // Resolve a presented bearer to its authority: the master token (constant-time
  // compare) → unscoped; a known, unexpired minted token → its scope; else null.
  private resolveToken(presented: string | null): TokenInfo | null {
    if (!presented) return null;
    if (this.tokenMatches(presented)) return { scope: null };
    const info = this.scopedTokens.get(presented);
    if (!info) return null;
    if (info.expiresAt != null && info.expiresAt <= Date.now()) {
      this.scopedTokens.delete(presented);
      return null;
    }
    return info;
  }

  // Mint + register a scoped token, optionally with a TTL.
  private mintToken(scope: Scope, ttlMs?: number): { token: string; expiresAt?: number } {
    const token = randomBytes(32).toString('hex');
    const expiresAt = ttlMs && ttlMs > 0 ? Date.now() + ttlMs : undefined;
    this.scopedTokens.set(token, { scope, expiresAt });
    return { token, expiresAt };
  }

  // A minter may only grant a scope that is itself within its own authority
  // (no escalation). Validated against the live tree. Returns an error or null.
  private canMint(parent: Scope | null, child: Scope): string | null {
    return checkMintable(parent, child, {
      paneCoords: (id) => this.paneCoords(id),
      tabWindow: (id) => this.tabToWindow.get(id) ?? null,
      hasWindow: (id) => this.windows.has(id)
    });
  }

  // Whether a scoped token may run `cmd` against its target (pane > tab > window).
  private commandScopeError(scope: Scope | null, cmd: ControlCommand): string | null {
    if (!scope) return null; // master: anything
    if (typeof cmd.paneId === 'string') {
      const coords = this.paneCoords(cmd.paneId);
      if (!coords) return `unknown paneId ${cmd.paneId}`;
      return paneInScope(scope, coords) ? null : `paneId ${cmd.paneId} is out of scope`;
    }
    if (typeof cmd.tabId === 'string') {
      const win = this.tabToWindow.get(cmd.tabId);
      if (win == null) return `unknown tabId ${cmd.tabId}`;
      return tabInScope(scope, cmd.tabId, win) ? null : `tabId ${cmd.tabId} is out of scope`;
    }
    if (typeof cmd.windowId === 'number') {
      if (windowInScope(scope, cmd.windowId)) return null;
      // newPane (and setLayout without a tabId) act on the window's ACTIVE tab, so
      // a tab-scoped manager may spawn into its own tab when that tab is active.
      const activeTab = this.windows.get(cmd.windowId)?.activeTabId;
      if (activeTab && tabInScope(scope, activeTab, cmd.windowId)) return null;
      return `windowId ${cmd.windowId} is out of scope`;
    }
    return 'a scoped token needs a paneId, tabId, or windowId on the command';
  }

  private bearer(req: IncomingMessage): string | null {
    const header = req.headers['authorization'];
    return typeof header === 'string' && header.startsWith('Bearer ')
      ? header.slice('Bearer '.length)
      : null;
  }

  // Constant-time token compare (length-guarded so timingSafeEqual can't throw).
  private tokenMatches(presented: string | null): boolean {
    if (!this.token || !presented) return false;
    const a = Buffer.from(presented);
    const b = Buffer.from(this.token);
    return a.length === b.length && timingSafeEqual(a, b);
  }

  private readBody(req: IncomingMessage, cb: (body: Record<string, unknown> | null) => void): void {
    let raw = '';
    req.on('data', (chunk) => {
      raw += chunk;
      if (raw.length > 1_000_000) req.destroy(); // crude DoS guard on loopback
    });
    req.on('end', () => {
      if (!raw) return cb(null);
      try {
        cb(JSON.parse(raw));
      } catch {
        cb(null);
      }
    });
    req.on('error', () => cb(null));
  }

  private findPane(
    paneId: string
  ): { windowId: number; tabId: string; pane: ControlPaneInfo } | null {
    return this.paneIndex.get(paneId) ?? null;
  }

  // ---- discovery + settings files (under userData, user-scoped) ----
  private discoveryPath(): string {
    return join(app.getPath('userData'), 'control.json');
  }

  private settingsPath(): string {
    return join(app.getPath('userData'), 'control-settings.json');
  }

  private writeDiscovery(): void {
    if (this.port == null || !this.token) return;
    try {
      writeFileSync(
        this.discoveryPath(),
        JSON.stringify(
          {
            port: this.port,
            token: this.token,
            pid: process.pid,
            version: app.getVersion(),
            // Convenience for clients: full event-stream URL (token included).
            events: `ws://127.0.0.1:${this.port}/events?token=${this.token}`
          },
          null,
          2
        ),
        'utf8'
      );
    } catch (err) {
      console.error('failed to write control.json', err);
    }
  }

  private removeDiscovery(): void {
    try {
      rmSync(this.discoveryPath(), { force: true });
    } catch {
      /* ignore */
    }
  }

  private loadSettings(): ControlSettings {
    try {
      const path = this.settingsPath();
      if (!existsSync(path)) return { ...DEFAULT_SETTINGS };
      const parsed = JSON.parse(readFileSync(path, 'utf8')) as Partial<ControlSettings>;
      return {
        enabled: parsed.enabled === true,
        allowInput: parsed.allowInput === true
      };
    } catch {
      return { ...DEFAULT_SETTINGS };
    }
  }

  private saveSettings(): void {
    try {
      writeFileSync(this.settingsPath(), JSON.stringify(this.settings, null, 2), 'utf8');
    } catch (err) {
      console.error('failed to write control-settings.json', err);
    }
  }
}
