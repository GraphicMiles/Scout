/** Scout JavaScript/TypeScript SDK. Zero dependencies; uses the global fetch (Node 18+, browsers, Deno, Bun). */

export class ScoutApiError extends Error {
  constructor(status, body, retryAfterSec) {
    const e = body?.error;
    super(e?.message ?? `HTTP ${status}`);
    this.name = 'ScoutApiError';
    this.status = status;
    this.code = e?.code ?? 'unknown';
    this.retryable = !!e?.retryable;
    this.details = e?.details;
    this.retryAfterSec = retryAfterSec;
  }
}

// Only errors raised before any work started are retried automatically (never mid-operation failures).
const SAFE_RETRY = new Set(['rate_limited', 'queue_full', 'overloaded', 'too_many_sessions']);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export class ScoutClient {
  #base; #key; #fetch; #timeout; #retries;

  constructor({ baseUrl, apiKey, timeoutMs = 70000, maxRetries = 2, fetch: f = globalThis.fetch } = {}) {
    if (!baseUrl) throw new TypeError('ScoutClient: baseUrl is required');
    this.#base = String(baseUrl).replace(/\/+$/, '');
    this.#key = apiKey;
    this.#timeout = timeoutMs;
    this.#retries = maxRetries;
    this.#fetch = f;
  }

  async #request(method, path, body, signal) {
    for (let attempt = 0; ; attempt++) {
      const timeout = AbortSignal.timeout(this.#timeout);
      const res = await this.#fetch(this.#base + path, {
        method,
        headers: { ...(this.#key ? { authorization: `Bearer ${this.#key}` } : {}), ...(body !== undefined ? { 'content-type': 'application/json' } : {}) },
        body: body !== undefined ? JSON.stringify(body) : undefined,
        signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
      });
      if (res.status === 204) return null;
      const raw = await res.text();
      let json = null;
      try { json = raw ? JSON.parse(raw) : null; } catch { /* non-JSON error page from a proxy */ }
      if (res.ok) return json;
      const retryAfter = Number(res.headers.get('retry-after')) || undefined;
      const err = new ScoutApiError(res.status, json, retryAfter);
      if (SAFE_RETRY.has(err.code) && attempt < this.#retries) { await sleep(Math.min((retryAfter ?? 1) * 1000, 10000)); continue; }
      throw err;
    }
  }

  health() { return this.#request('GET', '/health'); }
  strategies() { return this.#request('GET', '/v1/strategies').then((r) => r.strategies); }
  createSession() { return this.#request('POST', '/v1/sessions', {}); }
  async destroySession(id) { await this.#request('DELETE', `/v1/sessions/${id}`); }
  navigate(params, opts) { return this.#request('POST', '/v1/navigate', params, opts?.signal); }
  extract(params, opts) { return this.#request('POST', '/v1/extract', params, opts?.signal); }
  interact(params, opts) { return this.#request('POST', '/v1/interact', params, opts?.signal); }
  submitTask(spec) { return this.#request('POST', '/v1/tasks', spec); }
  getTask(id) { return this.#request('GET', `/v1/tasks/${id}`); }
  cancelTask(id) { return this.#request('POST', `/v1/tasks/${id}/cancel`, {}); }

  /** Create a session, run fn(sessionId), and always destroy the session afterwards. */
  async withSession(fn) {
    const s = await this.createSession();
    try { return await fn(s.id); } finally { await this.destroySession(s.id).catch(() => {}); }
  }

  /** Poll until the task is terminal. Throws ScoutApiError if it did not succeed. */
  async waitForTask(id, { pollMs = 300, timeoutMs = 120000, signal } = {}) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const t = await this.getTask(id);
      if (t.status === 'succeeded') return t;
      if (['failed', 'cancelled', 'timed_out'].includes(t.status)) throw new ScoutApiError(200, { error: t.error ?? { code: t.status, message: `Task ${t.status}` } });
      if (signal?.aborted || Date.now() > deadline) throw new Error(`Timed out waiting for task ${id}`);
      await sleep(pollMs);
    }
  }

  async runTask(spec, opts) { return this.waitForTask((await this.submitTask(spec)).id, opts); }
}
