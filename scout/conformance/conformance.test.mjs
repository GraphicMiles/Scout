/**
 * Conformance suite: the Node implementation's HTTP-observable tests, run
 * against the Rust runtime.
 *
 * Every assertion talks to a spawned `target/release/scout` process over HTTP,
 * using the shipped SDK as the client and the byte-identical fixture site from
 * `fixture.mjs`. Nothing reaches into Scout's internals, so these tests pass or
 * fail on observable behaviour only.
 *
 * Status codes follow the published contract: 400 invalid_request, 403
 * blocked_destination, 404 not_found, 409 conflict, 413 payload_too_large, 415
 * unsupported_content, 422 compatibility_error/element_not_found, 429
 * rate_limited, 501 unsupported_capability, 502 navigation_failed, 503
 * queue_full/overloaded/too_many_sessions/resource_limit, 504 timeouts.
 *
 *   node --test conformance/conformance.test.mjs
 *
 * Requires Node 22 (global fetch + node:test) and a built release binary.
 */
import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ScoutApiError, ScoutClient } from '../sdk/index.js';
import { KEY, OTHER_KEY, startFixture } from './fixture.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BINARY = process.env.SCOUT_BIN ?? path.join(HERE, '..', 'target', 'release', 'scout');
const METRICS_TOKEN = 'metrics-token-0123456789abcd';

/** Ask the OS for a free port, then release it. */
function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.on('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

/**
 * Boot a Scout instance. Sections that need different limits spin up their own,
 * mirroring the Node harness's `testConfig(overrides)`.
 */
export async function startScout({ env = {}, keys = [KEY, OTHER_KEY], wait = 15000 } = {}) {
  const port = await freePort();
  const child = spawn(BINARY, [], {
    env: {
      ...process.env,
      PORT: String(port),
      SCOUT_API_KEYS: keys.join(','),
      SCOUT_PRIVATE_ALLOWLIST: '127.0.0.1',
      SCOUT_YOUTUBE_HOSTS: '127.0.0.1',
      SCOUT_NAV_TIMEOUT_MS: '8000',
      SCOUT_REQUEST_TIMEOUT_MS: '8000',
      SCOUT_TASK_TIMEOUT_MS: '8000',
      SCOUT_LOG_LEVEL: 'error',
      SCOUT_METRICS_TOKEN: METRICS_TOKEN,
      SCOUT_RATE_LIMIT_PER_MIN: '1000',
      ...env,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const logs = [];
  child.stdout.on('data', (d) => logs.push(String(d)));
  child.stderr.on('data', (d) => logs.push(String(d)));

  const baseUrl = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + wait;
  for (;;) {
    if (child.exitCode !== null) {
      throw new Error(`scout exited early (code ${child.exitCode}):\n${logs.join('')}`);
    }
    try {
      const res = await fetch(`${baseUrl}/health`);
      if (res.ok) break;
    } catch { /* not up yet */ }
    if (Date.now() > deadline) throw new Error(`scout did not start:\n${logs.join('')}`);
    await new Promise((r) => setTimeout(r, 50));
  }
  return {
    baseUrl,
    port,
    logs,
    client: (key = KEY) => new ScoutClient({ baseUrl, apiKey: key, maxRetries: 0 }),
    stop: () =>
      new Promise((r) => {
        if (child.exitCode !== null) return r(child.exitCode);
        child.on('exit', (code) => r(code));
        child.kill('SIGTERM');
      }),
    kill: () => child.kill('SIGKILL'),
  };
}

/** Raw request that never retries, for status-code assertions. */
async function raw(baseUrl, method, pathname, { key = KEY, body, headers = {} } = {}) {
  const res = await fetch(baseUrl + pathname, {
    method,
    headers: {
      ...(key ? { authorization: `Bearer ${key}` } : {}),
      ...(body !== undefined ? { 'content-type': 'application/json' } : {}),
      ...headers,
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* non-JSON body */ }
  return { status: res.status, headers: res.headers, json, text };
}

let fixture;
let scout;
let fx;
/** POST with a JSON body — `raw` takes {body} as an option, so wrap it once. */
const post = (baseUrl, pathname, body, opts) => raw(baseUrl, 'POST', pathname, { ...opts, body });

/** Node's `parseNavigate` requires a session, so a test that only wants to
 * fetch a page creates one first. */
const sessionCache = new Map();
async function sessionFor(baseUrl, key = KEY) {
  const cacheKey = `${baseUrl}|${key}`;
  if (sessionCache.has(cacheKey)) return sessionCache.get(cacheKey);
  const r = await post(baseUrl, '/v1/sessions', {}, { key });
  assert.equal(r.status, 201, `session creation failed: ${r.status} ${r.text.slice(0, 160)}`);
  sessionCache.set(cacheKey, r.json.id);
  return r.json.id;
}

before(async () => {
  fixture = await startFixture();
  fx = fixture.url;
  scout = await startScout();
});

after(async () => {
  await scout?.stop();
  await fixture?.close();
});

// ---------------------------------------------------------------- api.test.js

describe('api: configuration and authentication', () => {
  // Node: "config refuses to start without API keys outside dev mode"
  it('refuses to start without API keys outside dev mode', async () => {
    const port = await freePort();
    const child = spawn(BINARY, [], {
      env: { ...process.env, PORT: String(port), SCOUT_API_KEYS: '', SCOUT_DEV_MODE: '' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (out += d));
    const code = await new Promise((r) => child.on('exit', r));
    assert.notEqual(code, 0, 'should exit non-zero without keys');
    assert.match(out, /SCOUT_API_KEYS/, `expected a message naming SCOUT_API_KEYS, got: ${out}`);
  });

  it('refuses an API key shorter than 16 characters', async () => {
    const port = await freePort();
    const child = spawn(BINARY, [], {
      env: { ...process.env, PORT: String(port), SCOUT_API_KEYS: 'short' },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (out += d));
    const code = await new Promise((r) => child.on('exit', r));
    assert.notEqual(code, 0);
    assert.match(out, /16/, `expected a minimum-length message, got: ${out}`);
  });

  // Node: "health is public; everything else needs a valid key"
  it('health is public; everything else needs a valid key', async () => {
    const h = await raw(scout.baseUrl, 'GET', '/health', { key: null });
    assert.equal(h.status, 200);
    assert.equal(h.json.status, 'ok');

    for (const p of ['/v1/strategies', '/metrics']) {
      const anon = await raw(scout.baseUrl, 'GET', p, { key: null });
      assert.equal(anon.status, 401, `${p} should require a key`);
      assert.equal(anon.json.error.code, 'unauthorized');
      assert.equal(anon.json.error.retryable, false);
      assert.ok(anon.headers.get('www-authenticate'), 'WWW-Authenticate must be present');
    }

    const good = await raw(scout.baseUrl, 'GET', '/v1/strategies', { key: null, headers: { 'x-api-key': KEY } });
    assert.equal(good.status, 200, 'x-api-key must be accepted as an alternative to bearer');
  });

  // Node: "metrics: protected by its own token; API keys are not accepted when
  // a metrics token is set"
  it('metrics requires its own token, not an API key', async () => {
    const withApiKey = await raw(scout.baseUrl, 'GET', '/metrics');
    assert.equal(withApiKey.status, 401, 'an API key must not open /metrics when a token is set');

    const withToken = await raw(scout.baseUrl, 'GET', '/metrics', { key: METRICS_TOKEN });
    assert.equal(withToken.status, 200);
    assert.match(withToken.text, /scout_/, 'metrics output should carry scout_ series');
  });

  // Node: "errors: consistent objects and meaningful status codes"
  it('errors are consistent objects with meaningful status codes', async () => {
    const sid = await sessionFor(scout.baseUrl);
    const cases = [
      ['GET', '/v1/nope', undefined, 404, 'not_found'],
      ['POST', '/v1/extract', { url: `${fx}/`, strategy: 'does-not-exist' }, 400, 'invalid_request'],
      ['POST', '/v1/navigate', { sessionId: 'ses_x', url: `${fx}/` }, 400, 'invalid_request'],
      ['POST', '/v1/navigate', { sessionId: `ses_${'a'.repeat(32)}`, url: `${fx}/` }, 404, 'not_found'],
      ['POST', '/v1/navigate', { sessionId: sid, url: 'http://169.254.169.254/' }, 403, 'blocked_destination'],
      ['POST', '/v1/extract', { url: `${fx}/`, strategy: 'tiktok-profile' }, 501, 'unsupported_capability'],
      ['POST', '/v1/extract', { url: `${fx}/`, strategy: 'selectors' }, 400, 'invalid_request'],
    ];
    for (const [method, p, body, status, code] of cases) {
      const r = await raw(scout.baseUrl, method, p, { body });
      assert.equal(r.status, status, `${p} → ${r.status} (${r.text.slice(0, 140)})`);
      assert.ok(r.json?.error, `${p} should return {"error": {...}}`);
      assert.equal(r.json.error.code, code, `${p} code was ${r.json.error.code}`);
      assert.equal(typeof r.json.error.message, 'string');
      assert.equal(typeof r.json.error.retryable, 'boolean');
      assert.ok(r.headers.get('x-request-id'), 'every response carries a request id');
      assert.equal(r.headers.get('cache-control'), 'no-store');
    }
  });

  it('rejects unknown request fields by name', async () => {
    const r = await post(scout.baseUrl, '/v1/navigate', {
      sessionId: 'ses_x',
      url: `${fx}/`,
      bogusField: 1,
    });
    assert.equal(r.status, 400);
    assert.equal(r.json.error.code, 'invalid_request');
    assert.match(r.json.error.message, /bogusField/);
    assert.ok(r.json.error.details.allowed, 'details should name the allowed fields');
  });

  it('rejects a wrong method with 405 and an Allow header', async () => {
    const r = await raw(scout.baseUrl, 'GET', '/v1/navigate');
    assert.equal(r.status, 405, `got ${r.status}: ${r.text.slice(0, 120)}`);
    assert.ok(r.headers.get('allow'), '405 must advertise the allowed methods');
  });

  // Node: "SDK: example usage from the spec works end to end"
  it('SDK example usage works end to end', async () => {
    const result = await scout.client().extract({
      url: `${fx}/@testchan/videos`,
      strategy: 'youtube-channel-videos',
      limit: 3,
    });
    assert.equal(result.data.channel.title, 'Test Channel');
    assert.equal(result.data.channel.id, 'UCabc123');
    assert.equal(result.data.videos.length, 3);
    assert.equal(result.data.videos[0].title, 'Video 0');
    assert.equal(result.data.videos[0].url, 'https://www.youtube.com/watch?v=vid0');
    assert.ok(result.meta.durationMs >= 0);
    assert.equal(result.meta.status, 200);
  });

  // Node: "SDK: session flow with navigate, interact, extract, destroy"
  it('SDK session flow: navigate, interact, extract, destroy', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    assert.match(id, /^ses_/, 'session ids carry a prefix');
    try {
      const nav = await client.navigate({ sessionId: id, url: `${fx}/products` });
      assert.equal(nav.status, 200);
      assert.equal(nav.title, 'Shop & Co');

      const ex = await client.extract({
        sessionId: id,
        schema: { item: 'li.item', fields: { name: '.name', link: { selector: 'a.link', type: 'url' } } },
      });
      assert.equal(ex.data.length, 3);
      assert.equal(ex.data[0].name, 'Alpha Lamp');
      assert.equal(ex.data[0].link, `${fx}/p/alpha`);

      // Node's contract: the click reports `navigated` and the landing page.
      const click = await client.interact({ sessionId: id, action: { type: 'click', selector: 'a.link' } });
      assert.equal(click.navigated, true);
      assert.equal(click.type, 'click');
      assert.equal(click.navigation.status, 200);
      assert.equal(click.page.status, 200);
    } finally {
      await client.destroySession(id);
    }
    await assert.rejects(
      () => client.extract({ sessionId: id, strategy: 'generic-page' }),
      (e) => e instanceof ScoutApiError && e.code === 'not_found',
    );
  });

  // Node: "callers cannot touch each other's ..."
  it("callers cannot touch each other's sessions or tasks", async () => {
    const mine = scout.client(KEY);
    const theirs = scout.client(OTHER_KEY);

    const { id } = await mine.createSession();
    await assert.rejects(
      () => theirs.extract({ sessionId: id, strategy: 'generic-page' }),
      (e) => e.code === 'not_found',
      'another owner must not see the session',
    );
    await assert.rejects(() => theirs.destroySession(id), (e) => e.code === 'not_found');
    // The owner can still use it: a cross-owner delete must not remove it.
    const nav = await mine.navigate({ sessionId: id, url: `${fx}/` });
    assert.equal(nav.status, 200);

    const task = await mine.submitTask({ steps: [{ op: 'navigate', url: `${fx}/` }] });
    await assert.rejects(
      () => theirs.getTask(task.id),
      (e) => e.code === 'not_found',
      'another owner must not read the task',
    );
    await mine.destroySession(id);
  });

  // Node: "SDK: tasks run, report failures, and cancel"
  it('SDK tasks run, report failures, and cancel', async () => {
    const client = scout.client();

    const ok = await client.runTask({
      steps: [
        { op: 'navigate', url: `${fx}/products` },
        { op: 'extract', schema: { item: 'li.item', fields: { name: '.name' } } },
      ],
    });
    assert.equal(ok.status, 'succeeded');
    assert.equal(ok.result.steps.length, 2);
    assert.equal(ok.result.data.length, 3);
    assert.equal(ok.result.steps[0].result.status, 200, 'the navigate step reports its HTTP status');

    // Node: "tasks ... report failures" — element_not_found on step 1.
    const bad = await client.submitTask({
      steps: [
        { op: 'navigate', url: `${fx}/products` },
        { op: 'interact', action: { type: 'click', selector: '#none' } },
      ],
    });
    const failed = await client.waitForTask(bad.id, { timeoutMs: 30000 }).catch((e) => e);
    assert.ok(failed instanceof ScoutApiError, `expected a failure, got ${JSON.stringify(failed)}`);
    assert.equal(failed.code, 'element_not_found');
    const detail = await client.getTask(bad.id);
    assert.equal(detail.status, 'failed');
    assert.equal(detail.error.details.failedStep, 1, 'the failing step index is reported');
    assert.equal(detail.result, null, 'a failed task must not carry a result');
  });

  // Node: "concurrent requests: bounded queue sheds load with 503 queue_full;
  // no result is wrong"
  it('bounded queue sheds load and never returns a wrong result', async () => {
    const s = await startScout({ env: { SCOUT_CONCURRENCY: '1', SCOUT_MAX_QUEUED: '1', SCOUT_MAX_SESSIONS: '64' } });
    try {
      const client = s.client();
      // A serial request on an idle instance must succeed.
      const first = await client.extract({ url: `${fx}/products`, schema: { item: 'li.item', fields: { name: '.name' } } });
      assert.equal(first.data.length, 3);

      const results = await Promise.all(
        Array.from({ length: 12 }, (_, i) =>
          client
            .extract({ url: `${fx}/products#${i}`, schema: { item: 'li.item', fields: { name: '.name' } } })
            .then((r) => ({ ok: true, r }))
            .catch((e) => ({ ok: false, code: e.code, status: e.status })),
        ),
      );
      const shed = results.filter((x) => !x.ok);
      assert.ok(shed.length >= 1, 'excess load should be shed rather than queued without bound');
      for (const x of shed) {
        assert.equal(x.status, 503, `shed with ${x.status}`);
        assert.ok(['queue_full', 'overloaded'].includes(x.code), `shed code was ${x.code}`);
      }
      // Nothing that reported success may be wrong.
      for (const x of results.filter((r) => r.ok)) {
        assert.equal(x.r.data.length, 3);
        assert.equal(x.r.data[0].name, 'Alpha Lamp');
      }
    } finally {
      await s.stop();
    }
  });

  // Node: "rate limiting per API key"
  it('rate limits per API key', async () => {
    const limitedKey = 'ratelimited-key-0123456789';
    const s = await startScout({ keys: [limitedKey], env: { SCOUT_RATE_LIMIT_PER_MIN: '5' } });
    try {
      let sawRateLimit = false;
      for (let i = 0; i < 20 && !sawRateLimit; i++) {
        const res = await raw(s.baseUrl, 'GET', '/v1/strategies', { key: limitedKey });
        if (res.status === 429) {
          sawRateLimit = true;
          assert.equal(res.json.error.code, 'rate_limited');
          assert.ok(res.headers.get('retry-after'), 'a 429 must say when to retry');
        } else {
          assert.equal(res.status, 200);
        }
      }
      assert.ok(sawRateLimit, 'exceeding the per-key limit must produce 429 rate_limited');
    } finally {
      await s.stop();
    }
  });

  // Node: "repeated failed authentication is throttled per address"
  it('repeated failed authentication is throttled', async () => {
    const s = await startScout();
    try {
      let throttled = false;
      for (let i = 0; i < 40 && !throttled; i++) {
        const r = await raw(s.baseUrl, 'GET', '/v1/strategies', { key: 'wrong-key-000000000000' });
        if (r.status === 429) throttled = true;
        else assert.equal(r.status, 401);
      }
      assert.ok(throttled, 'repeated bad keys should eventually be throttled');
    } finally {
      await s.stop();
    }
  });

  // Node: "secrets never reach logs"
  it('secrets never reach logs', async () => {
    const s = await startScout();
    try {
      await raw(s.baseUrl, 'GET', '/v1/strategies', { key: 'supersecret-0123456789abc' });
      await raw(s.baseUrl, 'GET', '/metrics', { key: METRICS_TOKEN });
      await s.stop();
      const logs = s.logs.join('');
      assert.ok(!logs.includes('supersecret-0123456789abc'), 'an API key must never be logged');
      assert.ok(!logs.includes(METRICS_TOKEN), 'the metrics token must never be logged');
    } finally {
      await s.kill();
    }
  });

  // Node: "client disconnect aborts in-flight work and releases the slot"
  it('a client disconnect releases the worker slot', async () => {
    const s = await startScout({ env: { SCOUT_CONCURRENCY: '1' } });
    try {
      const sid = await sessionFor(s.baseUrl);
      await new Promise((resolve) => {
        const body = JSON.stringify({ sessionId: sid, url: `${fx}/slow` });
        const sock = net.connect(s.port, '127.0.0.1', () => {
          sock.write(
            `POST /v1/navigate HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer ${KEY}\r\n` +
              `Content-Type: application/json\r\nContent-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`,
          );
          setTimeout(() => { sock.destroy(); resolve(); }, 250);
        });
        sock.on('error', () => resolve());
      });

      const started = Date.now();
      const r = await post(s.baseUrl, '/v1/navigate', { sessionId: sid, url: `${fx}/products` });
      assert.equal(r.status, 200, `the slot should be reusable, got ${r.status}: ${r.text.slice(0, 140)}`);
      assert.ok(Date.now() - started < 5000, 'the slot was held after the caller left');
    } finally {
      await s.stop();
    }
  });
});

// ------------------------------------------------------------- engine.test.js

describe('engine: navigation', () => {
  it('follows redirects, decodes gzip, and passes JSON through', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    const redir = await client.navigate({ sessionId: id, url: `${fx}/redirect` });
    assert.equal(redir.status, 200);
    assert.equal(redir.redirects.length, 1);
    assert.equal(redir.redirects[0].status, 302);

    const gz = await client.navigate({ sessionId: id, url: `${fx}/gzip` });
    assert.equal(gz.status, 200);
    assert.equal(gz.title, 'Shop & Co', 'gzip must be decoded');

    const json = await client.navigate({ sessionId: id, url: `${fx}/json` });
    assert.equal(json.status, 200);
    assert.match(json.contentType, /application\/json/);
  });

  it('parses malformed HTML and warns about a JS shell', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    const malformed = await client.navigate({ sessionId: id, url: `${fx}/malformed` });
    assert.equal(malformed.status, 200, 'a tolerant parse must not fail the navigation');

    const shell = await client.navigate({ sessionId: id, url: `${fx}/js-shell` });
    assert.ok(
      shell.warnings.some((w) => w.code === 'js_required_likely'),
      `expected js_required_likely, got ${JSON.stringify(shell.warnings)}`,
    );
  });

  it('navigation failures are explicit', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    const nav = (p, timeoutMs) => client.navigate({ sessionId: id, url: `${fx}${p}`, ...(timeoutMs ? { timeoutMs } : {}) });
    try {
      // The published contract throws for an error status rather than
      // returning the page, and says plainly that it will not bypass a block.
      const forbidden = await nav('/forbidden').catch((e) => e);
      assert.equal(forbidden.code, 'navigation_failed', `got ${JSON.stringify(forbidden)}`);
      assert.equal(forbidden.details.status, 403);
      assert.match(forbidden.message, /does not bypass/);
      assert.equal(forbidden.retryable, false, '403 is not retryable');

      await assert.rejects(() => nav('/missing'), (e) => e.code === 'navigation_failed');

      const image = await nav('/image').catch((e) => e);
      assert.equal(image.code, 'unsupported_content', 'a non-HTML body is refused');

      const loop = await nav('/redirect-loop').catch((e) => e);
      assert.equal(loop.code, 'navigation_failed', `redirect loop: ${JSON.stringify(loop)}`);

      const slow = await nav('/slow', 400).catch((e) => e);
      assert.equal(slow.code, 'navigation_timeout', `slow page: ${JSON.stringify(slow)}`);
    } finally {
      await client.destroySession(id);
    }
  });

  it('sessions isolate cookies from each other', async () => {
    const client = scout.client();
    const a = await client.createSession();
    const b = await client.createSession();
    try {
      await client.navigate({ sessionId: a.id, url: `${fx}/cookie-set` });
      await client.navigate({ sessionId: b.id, url: `${fx}/cookie-set` });
      await client.navigate({ sessionId: a.id, url: `${fx}/cookie-echo` });
      const echoed = fixture.hits.cookieEcho.at(-1);
      assert.match(echoed, /sid=abc123/, 'the session cookie should be sent back');
      assert.ok(!/evil=1/.test(echoed), 'a cross-domain cookie must not be sent to 127.0.0.1');
      assert.ok(!/(^|;\s*)tld=1/.test(echoed), 'a bare-TLD cookie must be rejected outright');
    } finally {
      await client.destroySession(a.id);
      await client.destroySession(b.id);
    }
  });

  it('enforces the session limit', async () => {
    const s = await startScout({ env: { SCOUT_MAX_SESSIONS: '2' } });
    try {
      const client = s.client();
      const one = await client.createSession();
      const two = await client.createSession();
      await assert.rejects(
        () => client.createSession(),
        (e) => e.code === 'too_many_sessions' && e.status === 503,
        'the third session must be refused while two are live',
      );
      await client.destroySession(one.id);
      const three = await client.createSession();
      assert.ok(three.id, 'the slot must be reusable');
      await client.destroySession(two.id);
      await client.destroySession(three.id);
    } finally {
      await s.stop();
    }
  });

  it('expires idle sessions', async () => {
    const s = await startScout({ env: { SCOUT_SESSION_TTL_MS: '400' } });
    try {
      const client = s.client();
      const { id } = await client.createSession();
      await new Promise((r) => setTimeout(r, 1200));
      await assert.rejects(
        () => client.navigate({ sessionId: id, url: `${fx}/` }),
        (e) => e.code === 'not_found',
        'an idle session should be gone after its TTL',
      );
    } finally {
      await s.stop();
    }
  });
});

describe('engine: extraction', () => {
  // Node: "extraction accuracy: selectors schema with price/date/url typing"
  it('extracts with a selectors schema, typing price/date/url', async () => {
    const r = await scout.client().extract({
      url: `${fx}/products`,
      schema: {
        item: 'li.item',
        fields: {
          name: '.name',
          price: { selector: '.price', type: 'price' },
          date: { selector: 'time', type: 'date' },
          url: { selector: 'a.link', type: 'url' },
        },
      },
    });
    assert.equal(r.data.length, 3);
    assert.equal(r.data[0].name, 'Alpha Lamp');
    assert.equal(r.data[0].price.amount, 1299.5);
    assert.equal(r.data[1].price.amount, 45);
    assert.equal(r.data[2].price.amount, 15000);
    assert.equal(r.data[0].date, '2026-03-01T00:00:00.000Z');
    assert.equal(r.data[1].url, `${fx}/p/beta`);
    assert.equal(r.meta.strategy, 'selectors');
  });

  it('extracts a single record when the schema has no item selector', async () => {
    const r = await scout.client().extract({
      url: `${fx}/form`,
      schema: { fields: { title: 'title' } },
    });
    assert.equal(r.data.title, 'Form', 'without `item`, data is a single object');
  });

  // Node: "generic-page extraction; schema warnings for fields that matched nothing"
  it('generic-page extraction matches the documented shape', async () => {
    const g = (await scout.client().extract({ url: `${fx}/products`, strategy: 'generic-page' })).data;
    assert.equal(g.title, 'Shop & Co');
    assert.equal(g.description, 'Test shop');
    assert.equal(g.openGraph.title, 'Shop OG');
    assert.equal(g.canonical, `${fx}/products`);
    assert.equal(g.jsonLd[0].n, 3);
    assert.equal(g.times.length, 3);
    assert.equal(g.links.length, 3);
  });

  it('warns for fields that matched nothing, and returns [] for no items', async () => {
    const client = scout.client();
    const w = await client.extract({ url: `${fx}/products`, schema: { item: 'li', fields: { x: '.nope' } } });
    assert.ok(w.warnings.some((x) => x.code === 'field_empty'), JSON.stringify(w.warnings));

    const none = await client.extract({ url: `${fx}/products`, schema: { item: '.zzz', fields: { x: 'b' } } });
    assert.deepEqual(none.data, []);
    assert.ok(none.warnings.some((x) => x.code === 'no_match'));
  });

  it('refuses strategies that are not implemented, before any network use', async () => {
    for (const strategy of ['facebook-jobs', 'facebook-marketplace', 'tiktok-profile']) {
      const r = await post(scout.baseUrl, '/v1/extract', { url: `${fx}/cookie-echo`, strategy });
      assert.equal(r.status, 501, `${strategy} should be refused`);
      assert.equal(r.json.error.code, 'unsupported_capability');
      assert.equal(r.json.error.retryable, false);
      assert.ok(r.json.error.message.length > 20, 'the reason must be explained in prose');
    }
  });

  it('reports every strategy status honestly', async () => {
    const list = await scout.client().strategies();
    const byName = Object.fromEntries(list.map((s) => [s.name, s]));
    assert.equal(byName['generic-page'].status, 'verified-fixtures');
    assert.equal(byName.selectors.status, 'verified-fixtures');
    assert.equal(byName['youtube-channel-videos'].status, 'verified-live');
    for (const n of ['tiktok-profile', 'facebook-jobs', 'facebook-marketplace']) {
      assert.equal(byName[n].status, 'not_implemented');
      assert.ok(byName[n].reason, `${n} must state why`);
    }
  });

  // Node: "YouTube adapter parses fixture-shaped ytInitialData (brace-safe),
  // reports gaps"
  it('reads the YouTube fixture shape and reports what is missing', async () => {
    const client = scout.client();
    const yt = await client.extract({ url: `${fx}/@testchan/videos`, strategy: 'youtube-channel-videos', limit: 10 });
    assert.equal(yt.data.channel.title, 'Test Channel');
    assert.equal(yt.data.channel.id, 'UCabc123');
    assert.equal(yt.data.videos.length, 5);
    assert.equal(yt.data.videos[4].title, 'Video 4');
    assert.equal(yt.data.videos[4].publishedText, '5 days ago');

    const wrongHost = await post(scout.baseUrl, '/v1/extract', {
      url: 'https://example.com/@someone',
      strategy: 'youtube-channel-videos',
    });
    assert.equal(wrongHost.status, 400, 'the strategy requires a YouTube host');
    assert.match(wrongHost.json.error.message, /youtube\.com/);

    const empty = await client.extract({ url: `${fx}/@empty/videos`, strategy: 'youtube-channel-videos', limit: 10 });
    assert.equal(empty.data.videos.length, 0);
    assert.equal(empty.data.channel.title ?? null, null, 'a missing channel must not be invented');
    assert.ok(empty.warnings.some((w) => w.code === 'no_videos'), JSON.stringify(empty.warnings));

    // A channel root is aimed at its Videos tab by the strategy, so the
    // fixture's /@testchan/videos is what gets read.
    const root = await client.extract({ url: `${fx}/@testchan`, strategy: 'youtube-channel-videos', limit: 2 });
    assert.equal(root.data.videos.length, 2, 'a channel root is prepared before the fetch');
    assert.equal(root.data.channel.title, 'Test Channel');
  });
});

describe('engine: interaction', () => {
  // Node: "interaction: link click, form fill + POST submit, GET form,
  // checkbox, select"
  it('clicks a link and reports the landing page', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    try {
      await client.navigate({ sessionId: id, url: `${fx}/` });
      const r = await client.interact({ sessionId: id, action: { type: 'click', selector: 'a' } });
      assert.equal(r.navigated, true);
      assert.equal(r.navigation.url, `${fx}/products`);
      assert.equal(r.page.title, 'Shop & Co');
    } finally {
      await client.destroySession(id);
    }
  });

  it('fills a form and submits it with POST', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    try {
      await client.navigate({ sessionId: id, url: `${fx}/form` });
      await client.interact({ sessionId: id, action: { type: 'fill', selector: 'input[name=q]', value: 'hello world' } });
      await client.interact({ sessionId: id, action: { type: 'fill', selector: 'input[name=pw]', value: 's3cret&=' } });
      await client.interact({ sessionId: id, action: { type: 'check', selector: 'input[name=agree]' } });
      await client.interact({ sessionId: id, action: { type: 'select', selector: 'select', value: 'blue' } });

      const r = await client.interact({ sessionId: id, action: { type: 'click', selector: 'button[name=go]' } });
      assert.equal(r.navigated, true);

      const out = (
        await client.extract({ sessionId: id, schema: { fields: { body: '#out', ct: '#ct' } } })
      ).data;
      assert.match(out.ct, /application\/x-www-form-urlencoded/);
      // Everything the form carries reached the server...
      assert.match(out.body, /q=hello(\+|%20)world/, `q not submitted: ${out.body}`);
      assert.match(out.body, /token=t1/);
      assert.match(out.body, /pw=s3cret%26%3D/, 'the value is form-encoded');
      assert.match(out.body, /agree=yes/, 'the checked box is included');
      assert.match(out.body, /color=blue/, 'the selected option is included');
      assert.match(out.body, /go=1/, "the activated submit button's name/value is included");
      assert.ok(/note=hello|¬e=hello/.test(out.body), `the textarea value is included: ${out.body}`);
      // NOTE: the fixture echoes the raw body into <pre>, and an HTML5 parser
      // reads the `&not` of `&note=hello` as the legacy character reference
      // `&not` + "e=hello" (browsers do the same). The submission itself is
      // unaffected; only the echoed copy of the text differs from the Node
      // tokenizer, which passed the ampersand through. See conformance/README.md.
    } finally {
      await client.destroySession(id);
    }
  });

  it('submits a GET form as query parameters', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    try {
      await client.navigate({ sessionId: id, url: `${fx}/form` });
      await client.interact({ sessionId: id, action: { type: 'fill', selector: 'input[name=s]', value: 'a b' } });
      const r = await client.interact({ sessionId: id, action: { type: 'press', selector: 'input[name=s]', key: 'Enter' } });
      assert.equal(r.navigated, true);
      assert.equal(r.navigation.url, `${fx}/echo-get?s=a+b`);
    } finally {
      await client.destroySession(id);
    }
  });

  it('interaction failures are explicit, never silent', async () => {
    const client = scout.client();
    const act = (sessionId, action) => post(scout.baseUrl, '/v1/interact', { sessionId, action });

    // Node: "click a with no page yet" → conflict
    const blank = await client.createSession();
    try {
      const noPage = await act(blank.id, { type: 'click', selector: 'a' });
      assert.equal(noPage.status, 409, `expected 409, got ${noPage.status}: ${noPage.text.slice(0, 120)}`);
      assert.equal(noPage.json.error.code, 'conflict');
    } finally {
      await client.destroySession(blank.id);
    }

    const { id } = await client.createSession();
    try {
      await client.navigate({ sessionId: id, url: `${fx}/form` });
      const cases = [
        [{ type: 'click', selector: '#nope' }, 'element_not_found', 422],
        [{ type: 'fill', selector: 'input[type=file]', value: 'x' }, 'unsupported_capability', 501],
        [{ type: 'select', selector: 'select', value: 'green' }, 'element_not_found', 422],
        // Beyond the Node list, same contract:
        [{ type: 'uncheck', selector: 'select' }, 'invalid_request', 400],
        [{ type: 'click', selector: 'input[name=q]', index: 9 }, 'element_not_found', 422],
      ];
      for (const [action, code, status] of cases) {
        const r = await act(id, action);
        assert.equal(r.status, status, `${JSON.stringify(action)} → ${r.status} ${r.text.slice(0, 140)}`);
        assert.equal(r.json.error.code, code, `${JSON.stringify(action)} code was ${r.json.error.code}`);
      }
      // A click with no semantic behaviour is delivered to the page's
      // scripts instead of refused: a plain button and a javascript: link.
      for (const sel of ['#js', '#js-link']) {
        const r = await act(id, { type: 'click', selector: sel });
        assert.equal(r.status, 200, `${sel} → ${r.status} ${r.text.slice(0, 140)}`);
        assert.equal(r.json.ok, true, `${sel} body was ${r.text.slice(0, 140)}`);
        assert.equal(r.json.type, 'click');
      }

      // Wait times out with its own code when nothing matches.
      const wait = await act(id, { type: 'wait', selector: '#never', timeoutMs: 250 });
      assert.equal(wait.status, 504, `wait → ${wait.status} ${wait.text.slice(0, 140)}`);
      assert.equal(wait.json.error.code, 'wait_timeout');
      const waitOk = await act(id, { type: 'wait', value: 'Go', timeoutMs: 2000 });
      assert.equal(waitOk.status, 200, `wait ok → ${waitOk.status} ${waitOk.text.slice(0, 140)}`);

      // History: nothing behind the first page, so back is a clean 404.
      const back = await act(id, { type: 'back' });
      assert.equal(back.status, 404, `back → ${back.status} ${back.text.slice(0, 140)}`);
      assert.equal(back.json.error.code, 'not_found');

      // The enctype form used to be refused; multipart submits now. Its file
      // input has nothing staged, so it sends an empty file part, like a browser.
      const mp = await act(id, { type: 'submit', selector: 'form[enctype] input' });
      assert.equal(mp.status, 200, `multipart submit → ${mp.status} ${mp.text.slice(0, 140)}`);
      assert.equal(mp.json.effect, 'navigate', `multipart submit effect was ${mp.json.effect}`);
    } finally {
      await client.destroySession(id);
    }
  });

  // Node refused scroll and non-Enter keys outright. The Rust runtime delivers
  // them to the page's scripts: the page's own scripts re-run (so listeners
  // exist again in the fresh context), the event is dispatched, and the DOM is
  // rebuilt from the result.
  it('scroll and non-Enter key presses are delivered as events', async () => {
    const client = scout.client();
    const act = (sessionId, action) => post(scout.baseUrl, '/v1/interact', { sessionId, action });
    const { id } = await client.createSession();
    try {
      await client.navigate({ sessionId: id, url: `${fx}/form` });
      const scroll = await act(id, { type: 'scroll', value: 600 });
      assert.equal(scroll.status, 200, scroll.text.slice(0, 140));
      assert.equal(scroll.json.effect, 'events');
      assert.equal(scroll.json.detail.event.kind, 'scroll');
      assert.equal(scroll.json.detail.event.scrollPx, 600);

      const press = await act(id, { type: 'press', selector: 'input[name=q]', key: 'Tab' });
      assert.equal(press.status, 200, press.text.slice(0, 140));
      assert.equal(press.json.effect, 'events');
      assert.equal(press.json.detail.event.key, 'Tab');
    } finally {
      await client.destroySession(id);
    }
  });

  it('index selects among matches', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    try {
      await client.navigate({ sessionId: id, url: `${fx}/products` });
      const r = await client.interact({ sessionId: id, action: { type: 'click', selector: 'a.link', index: 1 } });
      assert.equal(r.navigated, true);
      assert.equal(r.navigation.url, `${fx}/p/beta`, 'index 1 is the second link');
    } finally {
      await client.destroySession(id);
    }
  });

  it('check then uncheck both round-trip into a submission', async () => {
    const client = scout.client();
    const { id } = await client.createSession();
    try {
      await client.navigate({ sessionId: id, url: `${fx}/form` });
      await client.interact({ sessionId: id, action: { type: 'check', selector: 'input[name=agree]' } });
      const checked = await client.interact({ sessionId: id, action: { type: 'click', selector: 'button[name=go]' } });
      assert.equal(checked.navigated, true);
      let body = (
        await client.extract({ sessionId: id, schema: { fields: { body: '#out' } } })
      ).data.body;
      assert.match(body, /agree=yes/);

      await client.navigate({ sessionId: id, url: `${fx}/form` });
      await client.interact({ sessionId: id, action: { type: 'check', selector: 'input[name=agree]' } });
      await client.interact({ sessionId: id, action: { type: 'uncheck', selector: 'input[name=agree]' } });
      await client.interact({ sessionId: id, action: { type: 'click', selector: 'button[name=go]' } });
      body = (await client.extract({ sessionId: id, schema: { fields: { body: '#out' } } })).data.body;
      assert.ok(!/agree=yes/.test(body), 'an unchecked box is omitted from the submission');
    } finally {
      await client.destroySession(id);
    }
  });
});

describe('engine: tasks', () => {
  // Node: "tasks: multi-step success, partial-failure reporting, unsupported
  // strategies fail before network"
  it('runs multi-step tasks and reports partial failures', async () => {
    const client = scout.client();
    const ok = await client.runTask({
      steps: [
        { op: 'navigate', url: `${fx}/` },
        { op: 'extract', strategy: 'generic-page' },
        { op: 'navigate', url: `${fx}/products` },
        { op: 'extract', schema: { item: 'li.item', fields: { name: '.name' } } },
      ],
    });
    assert.equal(ok.status, 'succeeded');
    assert.equal(ok.result.steps.length, 4);
    assert.equal(ok.result.data.length, 3);

    // Node asserts this shape explicitly.
    const bad = await client.submitTask({
      steps: [
        { op: 'navigate', url: `${fx}/products` },
        { op: 'interact', action: { type: 'click', selector: '#none' } },
      ],
    });
    let view = await client.getTask(bad.id);
    const deadline = Date.now() + 30000;
    while (!['succeeded', 'failed', 'cancelled', 'timed_out'].includes(view.status) && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      view = await client.getTask(bad.id);
    }
    assert.equal(view.status, 'failed');
    assert.equal(view.error.code, 'element_not_found');
    assert.equal(view.error.details.failedStep, 1);
    assert.equal(view.result, null);

    // An unimplemented strategy fails before any network use.
    const r = await post(scout.baseUrl, '/v1/tasks', {
      steps: [{ op: 'navigate', url: `${fx}/cookie-echo` }, { op: 'extract', strategy: 'tiktok-profile' }],
    });
    assert.equal(r.status, 501);
    assert.equal(r.json.error.code, 'unsupported_capability');

    const empty = await post(scout.baseUrl, '/v1/tasks', {
      steps: [
        { op: 'navigate', url: `${fx}/cookie-echo` },
        { op: 'extract', url: `${fx}/cookie-echo`, strategy: 'facebook-jobs' },
      ],
    });
    assert.equal(empty.status, 501, 'a strategy on the step itself is refused too');
  });

  it('validates task specs', async () => {
    const cases = [
      [{ steps: [] }, 400, '1 to 20 steps'],
      [{ steps: [{ op: 'nope' }] }, 400, 'steps\\[0\\]\\.op must be'],
      [{ steps: Array.from({ length: 21 }, () => ({ op: 'navigate', url: `${fx}/` })) }, 400, '1 to 20 steps'],
      [{ steps: [{ op: 'navigate', url: `${fx}/` }], retries: 9 }, 400, 'retries'],
      [{ steps: [{ op: 'navigate', url: `${fx}/` }], timeoutMs: 500 }, 400, 'timeoutMs'],
    ];
    for (const [spec, status, needle] of cases) {
      const r = await post(scout.baseUrl, '/v1/tasks', spec);
      assert.equal(r.status, status, `${JSON.stringify(spec).slice(0, 50)} → ${r.status} ${r.text.slice(0, 120)}`);
      assert.match(r.json.error.message, new RegExp(needle), `message was: ${r.json.error.message}`);
    }
  });

  // Node: "tasks: navigation retries on 503 and succeeds; with retries=0 it fails"
  it('retries a flaky destination and succeeds', async () => {
    const client = scout.client();
    const id = `${Date.now()}`;
    const task = await client.runTask({
      steps: [{ op: 'navigate', url: `${fx}/flaky?id=${id}&n=2`, timeoutMs: 5000 }],
      retries: 2,
    });
    assert.equal(task.status, 'succeeded');
    assert.equal(task.result.steps[0].result.status, 200);

    const noRetry = await client.submitTask({
      steps: [{ op: 'navigate', url: `${fx}/flaky?id=${id}-b&n=2` }],
      retries: 0,
    });
    let view = await client.getTask(noRetry.id);
    const deadline = Date.now() + 20000;
    while (!['succeeded', 'failed'].includes(view.status) && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      view = await client.getTask(noRetry.id);
    }
    assert.equal(view.status, 'failed', 'with retries disabled a 503 is final');
  });

  // Node: "tasks: cancel running and queued, timeout, queue bound"
  it('cancels a running task', async () => {
    const client = scout.client();
    const task = await client.submitTask({ steps: [{ op: 'navigate', url: `${fx}/slow`, timeoutMs: 6000 }] });
    await new Promise((r) => setTimeout(r, 200));
    await client.cancelTask(task.id);
    let view = await client.getTask(task.id);
    const deadline = Date.now() + 15000;
    while (!['cancelled', 'failed', 'succeeded'].includes(view.status) && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      view = await client.getTask(task.id);
    }
    assert.equal(view.status, 'cancelled');
    assert.equal(view.result, null, 'a cancelled task carries no result');
  });

  it('fails a task that exceeds its timeout', async () => {
    const client = scout.client();
    const task = await client.submitTask({
      steps: [{ op: 'navigate', url: `${fx}/slow`, timeoutMs: 6000 }],
      timeoutMs: 1000,
    });
    let view = await client.getTask(task.id);
    const deadline = Date.now() + 20000;
    while (!['failed', 'timed_out', 'succeeded'].includes(view.status) && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
      view = await client.getTask(task.id);
    }
    assert.ok(['failed', 'timed_out'].includes(view.status), `status was ${view.status}`);
    assert.ok(view.error, 'a timed-out task must explain itself');
  });
});

describe('engine: security', () => {
  // Node: "SSRF: blocked destinations"
  it('blocks metadata, private and non-HTTP destinations', async () => {
    const cases = [
      'http://169.254.169.254/latest/meta-data/',
      'http://metadata.google.internal/',
      'http://10.0.0.5/admin',
      'http://192.168.1.1/',
      'http://localhost:8080/',
      'file:///etc/passwd',
      'http://user:pass@example.com/',
      'http://example.com:8443/',
    ];
    for (const url of cases) {
      const r = await post(scout.baseUrl, '/v1/navigate', { sessionId: await sessionFor(scout.baseUrl), url });
      assert.ok([400, 403].includes(r.status), `${url} → ${r.status} ${r.text.slice(0, 100)}`);
      assert.ok(
        ['blocked_destination', 'invalid_request'].includes(r.json.error.code),
        `${url} code was ${r.json.error.code}`,
      );
    }
  });

  // Node: "redirects are validated hop by hop"
  it('re-validates every redirect hop', async () => {
    const sid = await sessionFor(scout.baseUrl);
    for (const p of ['/redirect-meta', '/redirect-private', '/redirect-file']) {
      const r = await post(scout.baseUrl, '/v1/navigate', { sessionId: sid, url: fx + p });
      assert.equal(r.status, 403, `${p} must be blocked, got ${r.status} ${r.text.slice(0, 100)}`);
      assert.equal(r.json.error.code, 'blocked_destination');
    }
  });

  // Node: "allowedDomains restricts everything else"
  it('enforces an allowed-domain list when configured', async () => {
    const s = await startScout({ env: { SCOUT_ALLOWED_DOMAINS: 'example.com' } });
    try {
      const sid = await sessionFor(s.baseUrl);
      const allowed = await post(s.baseUrl, '/v1/navigate', { sessionId: sid, url: 'https://example.com/' });
      assert.equal(allowed.status, 200, allowed.text.slice(0, 120));

      const denied = await post(s.baseUrl, '/v1/navigate', { sessionId: sid, url: `${fx}/` });
      assert.equal(denied.status, 403);
      assert.equal(denied.json.error.code, 'blocked_destination');
    } finally {
      await s.stop();
    }
  });

  it('refuses an oversized response body', async () => {
    const s = await startScout({ env: { SCOUT_MAX_RESPONSE_BYTES: '262144' } });
    try {
      const r = await post(s.baseUrl, '/v1/navigate', { sessionId: await sessionFor(s.baseUrl), url: `${fx}/huge` });
      assert.equal(r.status, 502, `expected a size failure, got ${r.status}: ${r.text.slice(0, 100)}`);
      assert.equal(r.json.error.code, 'response_too_large');
    } finally {
      await s.stop();
    }
  });

  it('refuses an oversized request body', async () => {
    const s = await startScout({ env: { SCOUT_MAX_BODY_BYTES: '512' } });
    try {
      const r = await post(s.baseUrl, '/v1/extract', {
        url: `${fx}/`,
        schema: { item: 'li', fields: { a: { selector: 'x'.repeat(2000) } } },
      });
      assert.equal(r.status, 413, `${r.status}: ${r.text.slice(0, 120)}`);
      assert.equal(r.json.error.code, 'payload_too_large');
    } finally {
      await s.stop();
    }
  });

  it('refuses new work when memory is above the soft limit', async () => {
    // A 1 MB soft limit is below Scout's own baseline, so admission control must
    // refuse new work rather than pretend the limit is not there. It guards task
    // submission, as in the reference runtime.
    const s = await startScout({ env: { SCOUT_MEM_SOFT_MB: '1' } });
    try {
      const r = await post(s.baseUrl, '/v1/tasks', { steps: [{ op: 'navigate', url: `${fx}/` }] });
      assert.equal(r.status, 503, `expected shed load, got ${r.status}: ${r.text.slice(0, 100)}`);
      assert.equal(r.json.error.code, 'overloaded');
      assert.equal(r.json.error.retryable, true);
    } finally {
      await s.stop();
    }
  });

  // Node: "graceful shutdown: in-flight work is aborted and resources released
  // after the grace period"
  it('shuts down gracefully', async () => {
    const s = await startScout({ env: { SCOUT_SHUTDOWN_GRACE_MS: '1500' } });
    const code = await s.stop();
    assert.equal(code, 0, `expected a clean exit, got ${code}`);
  });
});
