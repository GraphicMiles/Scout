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
const sleep = (ms, signal) =>
  new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(signal.reason ?? new Error('Aborted'));
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal.reason ?? new Error('Aborted'));
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener?.('abort', onAbort);
      resolve();
    }, ms);
    signal?.addEventListener?.('abort', onAbort, { once: true });
  });

function bytesToBase64(bytes) {
  if (typeof Buffer !== 'undefined') return Buffer.from(bytes).toString('base64');
  let bin = '';
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    bin += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(bin);
}

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
      if (SAFE_RETRY.has(err.code) && attempt < this.#retries) { await sleep(Math.min((retryAfter ?? 1) * 1000, 10000), signal); continue; }
      throw err;
    }
  }

  async #requestRaw(path, body, signal) {
    const timeout = AbortSignal.timeout(this.#timeout);
    const res = await this.#fetch(this.#base + path, {
      method: 'POST',
      headers: { ...(this.#key ? { authorization: `Bearer ${this.#key}` } : {}), 'content-type': 'application/json' },
      body: JSON.stringify(body),
      signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
    });
    if (res.ok) return res;
    const raw = await res.text();
    let json = null;
    try { json = raw ? JSON.parse(raw) : null; } catch { /* non-JSON body from a proxy */ }
    throw new ScoutApiError(res.status, json, Number(res.headers.get('retry-after')) || undefined);
  }

  health() { return this.#request('GET', '/health'); }
  strategies() { return this.#request('GET', '/v1/strategies').then((r) => r.strategies); }
  createSession(opts = {}) { return this.#request('POST', '/v1/sessions', opts ?? {}); }
  async destroySession(id) { await this.#request('DELETE', `/v1/sessions/${encodeURIComponent(id)}`); }
  navigate(params, opts) { return this.#request('POST', '/v1/navigate', params, opts?.signal); }
  extract(params, opts) { return this.#request('POST', '/v1/extract', params, opts?.signal); }
  interact(params, opts) { return this.#request('POST', '/v1/interact', params, opts?.signal); }

  /** Execute JavaScript against the session's loaded page (read-only DOM snapshot). */
  eval(params, opts) { return this.#request('POST', '/v1/eval', params, opts?.signal); }
  submitTask(spec, opts) { return this.#request('POST', '/v1/tasks', spec, opts?.signal); }
  getTask(id, opts) { return this.#request('GET', `/v1/tasks/${encodeURIComponent(id)}`, undefined, opts?.signal); }
  cancelTask(id, opts) { return this.#request('POST', `/v1/tasks/${encodeURIComponent(id)}/cancel`, {}, opts?.signal); }

  /**
   * A request that is not a page load: the session's cookies go out, the SSRF
   * policy applies, and the response comes back unparsed. This is the primitive
   * for reading and writing an API as an authenticated user.
   */
  request(params, opts) { return this.#request('POST', '/v1/request', params, opts?.signal); }

  /**
   * Geometry of the current page, from Scout's own layout engine.
   * Returns { elements: [{ path, tag, rect, text, styles }], document, viewport }.
   */
  layout(sessionId, opts = {}) {
    const { width, height, maxElements, signal } = opts;
    return this.callTool('page.layout', { sessionId, width, height, maxElements }, { signal });
  }

  /**
   * A PNG of the current page, drawn in-process. Resolves to a Buffer (Node) or
   * Uint8Array; pass `{ encoding: 'base64' }` for a string. This is not Chromium:
   * pages that need CSS Grid or multi-column layout will not look right, and
   * `layout()` is the ground truth for what the engine actually placed.
   */
  async screenshot(sessionId, opts = {}) {
    const { width, height, fullPage, encoding = 'buffer', signal } = opts;
    const res = await this.#requestRaw('/v1/screenshot', { sessionId, width, height, fullPage }, signal);
    const bytes = new Uint8Array(await res.arrayBuffer());
    if (encoding === 'base64') return bytesToBase64(bytes);
    return typeof Buffer !== 'undefined' ? Buffer.from(bytes) : bytes;
  }

  /** Discover the agent-facing tool layer. */
  tools() { return this.#request('GET', '/v1/tools').then((r) => r.tools); }

  /** Call one tool by name. */
  callTool(name, args = {}, opts) {
    return this.#request('POST', '/v1/tools/call', { name, arguments: args }, opts?.signal);
  }

  /**
   * Sign in with credentials you are authorised to use, and get back the
   * evidence: which cookies were issued, whether the sign-in form is still on
   * the page, and what the landing page says. `looksAuthenticated` is a
   * conclusion drawn from that evidence, not an assumption.
   */
  login(params, opts) { return this.callTool('auth.login', params, opts); }

  /** Move a signed-in session somewhere it can be resumed. */
  exportSession(sessionId) { return this.callTool('session.export', { sessionId }); }
  importSession(sessionId, url, cookies) { return this.callTool('session.import', { sessionId, url, cookies }); }

  /** Create a session, run fn(sessionId), and always destroy the session afterwards. */
  async withSession(fn, sessionOpts = {}) {
    const s = await this.createSession(sessionOpts);
    try { return await fn(s.id); } finally { await this.destroySession(s.id).catch(() => {}); }
  }

  /** Poll until the task is terminal. Throws ScoutApiError if it did not succeed. */
  async waitForTask(id, { pollMs = 300, timeoutMs = 120000, signal } = {}) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (signal?.aborted) throw signal.reason ?? new Error(`Aborted waiting for task ${id}`);
      const t = await this.getTask(id, { signal });
      if (t.status === 'succeeded') return t;
      if (['failed', 'cancelled', 'timed_out'].includes(t.status)) throw new ScoutApiError(200, { error: t.error ?? { code: t.status, message: `Task ${t.status}` } });
      if (Date.now() > deadline) throw new Error(`Timed out waiting for task ${id}`);
      await sleep(pollMs, signal);
    }
  }

  async runTask(spec, opts) { return this.waitForTask((await this.submitTask(spec, opts)).id, opts); }
}
