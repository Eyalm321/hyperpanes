import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { OllamaClient } from './ollama-client';

// A minimal Response-like stub good enough for the client (which only reads
// `ok`, `status`, and `json()`). Keeps the tests free of any real network.
function jsonResponse(body: unknown, init?: { ok?: boolean; status?: number }) {
  const status = init?.status ?? 200;
  return {
    ok: init?.ok ?? (status >= 200 && status < 300),
    status,
    json: async () => body
  } as unknown as Response;
}

const ENDPOINT = 'http://mac-mini:11434';
const MODEL = 'gemma';

describe('OllamaClient.summarize', () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('parses .response, trims it, and returns a clean single line', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ response: '  rebased onto main, 3 commits  ' }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    expect(await client.summarize({ system: 'sys', prompt: 'p' })).toBe('rebased onto main, 3 commits');
  });

  it('takes the first non-empty line and collapses internal whitespace', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse({ response: '\n\n   running   tests\twith   gaps\nsecond line\nthird' })
    );
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    expect(await client.summarize({ system: 'sys', prompt: 'p' })).toBe('running tests with gaps');
  });

  it('clamps a long line to ~80 chars', async () => {
    const long = 'x'.repeat(200);
    fetchMock.mockResolvedValue(jsonResponse({ response: long }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    const out = await client.summarize({ system: 'sys', prompt: 'p' });
    expect(out.length).toBeLessThanOrEqual(80);
  });

  it('POSTs to {endpoint}/api/generate with the frozen body + options shape', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ response: 'ok' }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    await client.summarize({ system: 'you are terse', prompt: 'summarize this' });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://mac-mini:11434/api/generate');
    expect(init.method).toBe('POST');
    expect(init.headers['content-type']).toBe('application/json');
    expect(init.signal).toBeInstanceOf(AbortSignal);
    expect(JSON.parse(init.body)).toEqual({
      model: MODEL,
      system: 'you are terse',
      prompt: 'summarize this',
      stream: false,
      options: { temperature: 0.2, num_predict: 64, top_p: 0.9 }
    });
  });

  it('throws on a non-2xx response', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ error: 'boom' }, { status: 500 }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    await expect(client.summarize({ system: 'sys', prompt: 'p' })).rejects.toThrow('ollama 500');
  });

  it('throws when fetch rejects (network error / abort / timeout)', async () => {
    fetchMock.mockRejectedValue(new DOMException('aborted', 'AbortError'));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    await expect(client.summarize({ system: 'sys', prompt: 'p' })).rejects.toThrow();
  });

  it('throws when .response is missing', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ done: true }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    await expect(client.summarize({ system: 'sys', prompt: 'p' })).rejects.toThrow();
  });

  it('throws when .response is empty or whitespace-only', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ response: '   \n\t ' }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    await expect(client.summarize({ system: 'sys', prompt: 'p' })).rejects.toThrow();
  });

  it('normalizes a trailing slash on the endpoint', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ response: 'ok' }));
    const client = new OllamaClient({ endpoint: 'http://mac-mini:11434/', model: MODEL });
    await client.summarize({ system: 'sys', prompt: 'p' });
    expect(fetchMock.mock.calls[0][0]).toBe('http://mac-mini:11434/api/generate');
  });

  it('honors the timeoutMs default of 12000 when constructing the signal', async () => {
    const spy = vi.spyOn(AbortSignal, 'timeout');
    fetchMock.mockResolvedValue(jsonResponse({ response: 'ok' }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    await client.summarize({ system: 'sys', prompt: 'p' });
    expect(spy).toHaveBeenCalledWith(12000);
  });

  it('uses a custom timeoutMs when provided', async () => {
    const spy = vi.spyOn(AbortSignal, 'timeout');
    fetchMock.mockResolvedValue(jsonResponse({ response: 'ok' }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL, timeoutMs: 5000 });
    await client.summarize({ system: 'sys', prompt: 'p' });
    expect(spy).toHaveBeenCalledWith(5000);
  });
});

describe('OllamaClient.configure', () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('live-updates endpoint and model used by the next summarize', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ response: 'ok' }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    client.configure({ endpoint: 'http://other:1234/', model: 'llama3' });
    await client.summarize({ system: 'sys', prompt: 'p' });

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://other:1234/api/generate');
    expect(JSON.parse(init.body).model).toBe('llama3');
  });

  it('leaves untouched fields intact when patching partially', async () => {
    const spy = vi.spyOn(AbortSignal, 'timeout');
    fetchMock.mockResolvedValue(jsonResponse({ response: 'ok' }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL, timeoutMs: 8000 });
    client.configure({ model: 'llama3' });
    await client.summarize({ system: 'sys', prompt: 'p' });

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://mac-mini:11434/api/generate');
    expect(JSON.parse(init.body).model).toBe('llama3');
    expect(spy).toHaveBeenCalledWith(8000);
  });
});

describe('OllamaClient.ping', () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('GETs {endpoint}/api/tags and returns true on a 2xx', async () => {
    fetchMock.mockResolvedValue(jsonResponse({ models: [] }));
    const client = new OllamaClient({ endpoint: 'http://mac-mini:11434/', model: MODEL });
    expect(await client.ping()).toBe(true);

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://mac-mini:11434/api/tags');
    expect(init.method ?? 'GET').toBe('GET');
    expect(init.signal).toBeInstanceOf(AbortSignal);
  });

  it('returns false on a non-2xx without throwing', async () => {
    fetchMock.mockResolvedValue(jsonResponse({}, { status: 503 }));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    expect(await client.ping()).toBe(false);
  });

  it('returns false (never throws) when fetch rejects', async () => {
    fetchMock.mockRejectedValue(new Error('ECONNREFUSED'));
    const client = new OllamaClient({ endpoint: ENDPOINT, model: MODEL });
    expect(await client.ping()).toBe(false);
  });
});
