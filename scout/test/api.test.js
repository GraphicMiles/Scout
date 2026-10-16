import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import { startScout, startFixture, KEY, OTHER_KEY, testConfig } from './helpers.js';
import { ScoutClient, ScoutApiError } from '../sdk/index.js';
import { loadConfig } from '../src/config.js';

let fx; let app; let scout;
before(async () => { fx = await startFixture(); app = await startScout({ metricsToken: 'metrics-token-0123456789' }); scout = new ScoutClient({ baseUrl: app.url, apiKey: KEY, maxRetries: 0 }); });
after(async () => { await app.close(); await fx.close(); });

const raw = (path, { method = 'GET', key = KEY, body, headers = {} } = {}) => fetch(app.url + path, { method, headers: { ...(key ? { authorization: `Bearer ${key}` } : {}), ...(body !== undefined ? { 'content-type': 'application/json' } : {}), ...headers }, body: typeof body === 'string' ? body : body === undefined ? undefined : JSON.stringify(body) });
const err = async (p) => p.then(() => assert.fail('expected rejection'), (e) => e);

test('config refuses to start without API keys outside dev mode', () => {
  assert.throws(() => loadConfig({}, {}), /SCOUT_API_KEYS/);
  assert.throws(() => loadConfig({ SCOUT_API_KEYS: 'short' }), /16 characters/);
  assert.doesNotThrow(() => loadConfig({ SCOUT_DEV_MODE: '1' }));
});

test('health is public; everything else needs a valid key', async () => {
  assert.equal((await raw('/health', { key: null })).status, 200);
  for (const [p, m] of [['/v1/strategies', 'GET'], ['/v1/sessions', 'POST'], ['/metrics', 'GET']]) {
    const r = await raw(p, { method: m, key: null, body: m === 'POST' ? {} : undefined });
    assert.equal(r.status, 401, p); assert.equal(r.headers.get('www-authenticate'), 'Bearer');
  }
  assert.equal((await raw('/v1/strategies', { key: 'wrong-key-0123456789abcdef' })).status, 401);
  assert.equal((await raw('/v1/strategies', { key: null, headers: { 'x-api-key': KEY } })).status, 200);
});

test('metrics: protected by its own token; API keys are not accepted when a metrics token is set', async () => {
  assert.equal((await raw('/metrics')).status, 401);
  const r = await raw('/metrics', { key: 'metrics-token-0123456789' });
  assert.equal(r.status, 200);
  const t = await r.text();
  assert.match(t, /scout_rss_bytes \d+/); assert.match(t, /scout_http_requests_total/);
  assert.ok(!t.includes(KEY));
});

test('errors: consistent objects and meaningful status codes', async () => {
  const j = async (r) => (await r.json()).error;
  assert.equal((await raw('/nope')).status, 404);
  const m = await raw('/v1/navigate'); assert.equal(m.status, 405); assert.equal(m.headers.get('allow'), 'POST');
  let r = await raw('/v1/extract', { method: 'POST', body: '{not json' }); assert.equal(r.status, 400); assert.equal((await j(r)).code, 'invalid_request');
  r = await raw('/v1/extract', { method: 'POST', body: { urll: 'x' } }); assert.equal(r.status, 400); assert.match((await j(r)).message, /Unknown field/);
  r = await raw('/v1/extract', { method: 'POST', body: { url: 'https://example.com', schema: { fields: { a: 'li:hover' } } } }); assert.equal((await j(r)).code, 'invalid_request');
  r = await raw('/v1/extract', { method: 'POST', body: { url: 'http://169.254.169.254/latest/meta-data/' } });
  assert.equal(r.status, 403); const e = await j(r); assert.equal(e.code, 'blocked_destination'); assert.equal(e.retryable, false);
  r = await raw('/v1/extract', { method: 'POST', body: { url: fx.url + '/', strategy: 'facebook-jobs' } }); assert.equal(r.status, 501);
  r = await raw('/v1/navigate', { method: 'POST', body: { sessionId: 'ses_' + 'a'.repeat(32), url: fx.url } }); assert.equal(r.status, 404);
  r = await raw('/v1/sessions', { method: 'POST', body: 'x'.repeat(70000) }); assert.equal(r.status, 413);
  r = await raw('/v1/sessions', { method: 'POST', body: '{}', headers: { 'content-type': 'text/plain' } }); assert.ok([200, 201, 400].includes(r.status));
});

test('SDK: example usage from the spec works end to end', async () => {
  const result = await scout.extract({ url: fx.url + '/@testchan/videos', strategy: 'youtube-channel-videos', limit: 2 });
  assert.equal(result.data.videos.length, 2);
  assert.equal(result.data.channel.title, 'Test Channel');
});

test('SDK: session flow with navigate, interact, extract, destroy', async () => {
  await scout.withSession(async (sessionId) => {
    const nav = await scout.navigate({ sessionId, url: fx.url + '/form' });
    assert.equal(nav.title, 'Form');
    await scout.interact({ sessionId, action: { type: 'fill', selector: 'input[name=s]', value: 'zed' } });
    const r = await scout.interact({ sessionId, action: { type: 'press', selector: 'input[name=s]', key: 'Enter' } });
    assert.equal(r.navigated, true);
    const x = await scout.extract({ sessionId, schema: { fields: { q: '#out' } } });
    assert.equal(x.data.q, '?s=zed');
  });
  const s = await scout.createSession();
  await scout.destroySession(s.id);
  const e = await err(scout.navigate({ sessionId: s.id, url: fx.url }));
  assert.ok(e instanceof ScoutApiError); assert.equal(e.code, 'not_found'); assert.equal(e.status, 404);
  assert.equal(app.scout.sessions.size, 0);
});

test('callers cannot touch each other\'s sessions or tasks', async () => {
  const other = new ScoutClient({ baseUrl: app.url, apiKey: OTHER_KEY, maxRetries: 0 });
  const s = await scout.createSession();
  assert.equal((await err(other.navigate({ sessionId: s.id, url: fx.url }))).code, 'not_found');
  assert.equal((await err(other.destroySession(s.id))).code, 'not_found');
  const t = await scout.submitTask({ steps: [{ op: 'navigate', url: fx.url + '/' }] });
  assert.equal((await err(other.getTask(t.id))).code, 'not_found');
  assert.equal((await err(other.cancelTask(t.id))).code, 'not_found');
  await scout.destroySession(s.id);
  await scout.waitForTask(t.id);
});

test('SDK: tasks run, report failures, and cancel', async () => {
  const t = await scout.runTask({ steps: [{ op: 'navigate', url: fx.url + '/products' }, { op: 'extract', schema: { item: 'li.item', fields: { name: '.name' } } }] });
  assert.equal(t.result.data.length, 3);
  const bad = await err(scout.runTask({ steps: [{ op: 'navigate', url: fx.url + '/forbidden' }], retries: 0 }));
  assert.equal(bad.code, 'navigation_failed');
  const slow = await scout.submitTask({ steps: [{ op: 'navigate', url: fx.url + '/slow', timeoutMs: 20000 }], timeoutMs: 20000 });
  await new Promise((r) => setTimeout(r, 150));
  assert.equal((await scout.cancelTask(slow.id)).status, 'cancelled');
  assert.equal((await err(scout.waitForTask(slow.id))).code, 'cancelled');
  await new Promise((r) => setTimeout(r, 100));
  assert.deepEqual([app.scout.tasks.stats().running, app.scout.sessions.size], [0, 0]);
});

test('client disconnect aborts in-flight work and releases the slot', async () => {
  const ctl = new AbortController();
  const p = err(scout.extract({ url: fx.url + '/slow', timeoutMs: 20000 }, { signal: ctl.signal }));
  await new Promise((r) => setTimeout(r, 200));
  assert.equal(app.scout.tasks.stats().running, 1);
  ctl.abort(); await p;
  await new Promise((r) => setTimeout(r, 200));
  assert.deepEqual([app.scout.tasks.stats().running, app.scout.sessions.size], [0, 0]);
});

test('concurrent requests: bounded queue sheds load with 503 queue_full; no result is wrong', async () => {
  const a = await startScout({ concurrency: 1, maxQueued: 2 });
  try {
    const c = new ScoutClient({ baseUrl: a.url, apiKey: KEY, maxRetries: 0 });
    const results = await Promise.allSettled(Array.from({ length: 8 }, () => c.extract({ url: fx.url + '/products', schema: { item: 'li.item', fields: { name: '.name' } } })));
    const ok = results.filter((r) => r.status === 'fulfilled'); const rej = results.filter((r) => r.status === 'rejected');
    assert.ok(ok.length >= 3, `ok=${ok.length}`);
    assert.ok(rej.length >= 1);
    for (const r of rej) { assert.equal(r.reason.code, 'queue_full'); assert.equal(r.reason.status, 503); assert.equal(r.reason.retryable, true); }
    for (const r of ok) assert.deepEqual(r.value.data.map((x) => x.name), ['Alpha Lamp', 'Beta Desk', 'Gamma Chair']);
    assert.equal(a.scout.sessions.size, 0);
    // SDK retries queue_full when asked to
    const patient = new ScoutClient({ baseUrl: a.url, apiKey: KEY, maxRetries: 3 });
    const again = await Promise.all(Array.from({ length: 6 }, () => patient.extract({ url: fx.url + '/products' })));
    assert.equal(again.length, 6);
  } finally { await a.close(); }
});

test('rate limiting per API key', async () => {
  const a = await startScout({ rateLimitPerMin: 5 });
  try {
    const codes = []; for (let i = 0; i < 8; i++) codes.push((await fetch(a.url + '/v1/strategies', { headers: { authorization: `Bearer ${KEY}` } })).status);
    assert.deepEqual(codes.filter((c) => c === 200).length, 5); assert.equal(codes.at(-1), 429);
    const other = await fetch(a.url + '/v1/strategies', { headers: { authorization: `Bearer ${OTHER_KEY}` } });
    assert.equal(other.status, 200, 'limits are per key');
    const limited = await fetch(a.url + '/v1/strategies', { headers: { authorization: `Bearer ${KEY}` } });
    assert.ok(Number(limited.headers.get('retry-after')) >= 1);
  } finally { await a.close(); }
});

test('repeated failed authentication is throttled per address', async () => {
  const a = await startScout();
  try {
    const codes = []; for (let i = 0; i < 25; i++) codes.push((await fetch(a.url + '/v1/strategies', { headers: { authorization: 'Bearer nope-nope-nope-nope-nope' } })).status);
    assert.equal(codes[0], 401); assert.equal(codes.at(-1), 429);
  } finally { await a.close(); }
});

test('graceful shutdown: in-flight work is aborted and resources released after the grace period', async () => {
  const a = await startScout({ shutdownGraceMs: 100 });
  const c = new ScoutClient({ baseUrl: a.url, apiKey: KEY, maxRetries: 0 });
  const inflight = err(c.extract({ url: fx.url + '/slow', timeoutMs: 20000 }));
  await new Promise((r) => setTimeout(r, 150));
  await a.close();
  const e = await inflight;
  assert.ok(e instanceof Error);
  assert.deepEqual([a.scout.tasks.stats().running, a.scout.sessions.size], [0, 0]);
});

test('secrets never reach logs: redaction helper', async () => {
  const { redact } = await import('../src/logger.js');
  const out = JSON.stringify(redact({ authorization: 'Bearer x', headers: { Cookie: 'sid=1', 'x-api-key': 'k' }, password: 'p', note: 'ok' }));
  assert.ok(!/Bearer x|sid=1|"k"|"p"/.test(out)); assert.match(out, /ok/);
});
