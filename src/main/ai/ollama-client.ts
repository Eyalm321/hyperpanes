// A tiny HTTP client for a local Ollama server (e.g. Gemma on a Mac mini) that
// turns terminal context into a one-line summary. Uses the global `fetch` +
// `AbortSignal.timeout` (Node 24 / Electron 42) — no HTTP dependency.
//
// No app-state coupling: construct with config, call methods. `summarize`
// throws on any failure (the caller owns retry/backoff); `ping` never throws.

export interface OllamaConfig {
  endpoint: string;
  model: string;
  timeoutMs?: number; // default 12000
}

export interface SummarizeInput {
  system: string;
  prompt: string;
}

const DEFAULT_TIMEOUT_MS = 12000;
const PING_TIMEOUT_MS = 3000;
const MAX_SUMMARY_CHARS = 80;

// Strip a trailing slash so `http://host:11434/` and `http://host:11434`
// behave identically.
function normalizeEndpoint(endpoint: string): string {
  return endpoint.replace(/\/+$/, '');
}

// Take the first non-empty line, collapse internal whitespace, clamp length.
function cleanLine(raw: string): string {
  const line = raw.split('\n').map((l) => l.trim()).find((l) => l.length > 0) ?? '';
  const collapsed = line.replace(/\s+/g, ' ').trim();
  return collapsed.slice(0, MAX_SUMMARY_CHARS);
}

export class OllamaClient {
  private endpoint: string;
  private model: string;
  private timeoutMs: number;

  constructor(cfg: OllamaConfig) {
    this.endpoint = normalizeEndpoint(cfg.endpoint);
    this.model = cfg.model;
    this.timeoutMs = cfg.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  }

  // Live-update endpoint/model/timeout. Untouched fields stay as-is.
  configure(patch: Partial<OllamaConfig>): void {
    if (patch.endpoint !== undefined) this.endpoint = normalizeEndpoint(patch.endpoint);
    if (patch.model !== undefined) this.model = patch.model;
    if (patch.timeoutMs !== undefined) this.timeoutMs = patch.timeoutMs;
  }

  // POST {endpoint}/api/generate and reduce `.response` to a clean one-liner.
  // Throws on network error, timeout/abort, non-2xx, or missing/empty response.
  async summarize(input: SummarizeInput): Promise<string> {
    const res = await fetch(`${this.endpoint}/api/generate`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        model: this.model,
        system: input.system,
        prompt: input.prompt,
        stream: false,
        options: { temperature: 0.2, num_predict: 64, top_p: 0.9 }
      }),
      signal: AbortSignal.timeout(this.timeoutMs)
    });

    if (!res.ok) throw new Error(`ollama ${res.status}`);

    const data = (await res.json()) as { response?: unknown };
    const raw = typeof data.response === 'string' ? data.response : '';
    const summary = cleanLine(raw);
    if (!summary) throw new Error('ollama: empty response');
    return summary;
  }

  // GET {endpoint}/api/tags as a reachability check. Returns a boolean; never throws.
  async ping(): Promise<boolean> {
    try {
      const res = await fetch(`${this.endpoint}/api/tags`, {
        method: 'GET',
        signal: AbortSignal.timeout(PING_TIMEOUT_MS)
      });
      return res.ok;
    } catch {
      return false;
    }
  }
}
