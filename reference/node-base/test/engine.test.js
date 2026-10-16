import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import { makeScout, startFixture, PRODUCTS } from './helpers.js';
import { parseSchema } from '../src/validate.js';
import { parseYoutubeChannel } from '../src/adapters/youtube.js';
import { parseHtml } from '../src/html.js';

let fx;
before(async () => { fx = await startFixture(); });
after(async () => { await fx.close(); });

const O = 'owner-a';
const SCHEMA = parseSchema({ item: 'li.item', fields: { name: '.name', price: { selector: '.price', type: 'price' }, date: { selector: 'time', type: 'date' }, url: { selector: 'a.link', type: 'url' } } });

async function withScout(fn, o) { const s = makeScout(o); try { return await fn(s); } finally { await s.close(); } }
const rejectsCode = (p, code) => assert.rejects(p, (e) => { assert.equal(e.code, code, e.message); return true; });

test('extraction accuracy: selectors schema with price/date/url typing', () => withScout(async (s) => {
  const r = await s.extract(O, { url: fx.url + '/products', schema: SCHEMA });
  assert.equal(r.data.length, 3);
  PRODUCTS.forEach((p, i) => {
    assert.equal(r.data[i].name, p.name);
    assert.equal(r.data[i].price.amount, p.amount);
    assert.equal(r.data[i].date, new Date(p.date).toISOString());
    assert.equal(r.data[i].url, fx.url + p.path);
  });
  assert.equal(r.warnings.length, 0);
  assert.equal(s.sessions.size, 0, 'ephemeral session released');
}));

test('generic-page extraction; schema warnings for fields that matched nothing', () => withScout(async (s) => {
  const g = (await s.extract(O, { url: fx.url + '/products' })).data;
  assert.equal(g.title, 'Shop & Co');
  assert.equal(g.description, 'Test shop');
  assert.equal(g.openGraph.title, 'Shop OG');
  assert.equal(g.canonical, fx.url + '/products');
  assert.equal(g.jsonLd[0].n, 3);
  assert.equal(g.times.length, 3);
  assert.equal(g.links.length, 3);
  const w = await s.extract(O, { url: fx.url + '/products', schema: parseSchema({ item: 'li', fields: { x: '.nope' } }) });
  assert.ok(w.warnings.some((x) => x.code === 'field_empty'));
  const none = await s.extract(O, { url: fx.url + '/products', schema: parseSchema({ item: '.zzz', fields: { x: 'b' } }) });
  assert.deepEqual(none.data, []);
  assert.ok(none.warnings.some((x) => x.code === 'no_match'));
}));

test('navigation: redirects, gzip, JSON, malformed HTML, JS-shell warning', () => withScout(async (s) => {
  const ses = s.sessions.create(O);
  const r = await s.navigate(O, { sessionId: ses.id, url: fx.url + '/redirect' });
  assert.equal(r.redirects.length, 1); assert.equal(r.title, 'Shop & Co');
  assert.equal((await s.navigate(O, { sessionId: ses.id, url: fx.url + '/gzip' })).title, 'Shop & Co');
  const j = await s.extract(O, { sessionId: ses.id, url: fx.url + '/json' });
  assert.deepEqual(j.data.json, { ok: true, n: [1, 2] });
  assert.equal((await s.navigate(O, { sessionId: ses.id, url: fx.url + '/malformed' })).status, 200);
  const shell = await s.navigate(O, { sessionId: ses.id, url: fx.url + '/js-shell' });
  assert.equal(shell.warnings[0].code, 'js_required_likely');
}));

test('navigation failures are explicit', () => withScout(async (s) => {
  const ses = s.sessions.create(O);
  const nav = (p) => s.navigate(O, { sessionId: ses.id, url: fx.url + p, timeoutMs: 400 });
  await rejectsCode(nav('/huge'), 'response_too_large');
  await rejectsCode(nav('/image'), 'unsupported_content');
  await rejectsCode(nav('/slow'), 'navigation_timeout');
  const e = await nav('/forbidden').catch((x) => x);
  assert.equal(e.code, 'navigation_failed'); assert.equal(e.details.status, 403); assert.match(e.message, /does not bypass/);
  await rejectsCode(nav('/missing'), 'navigation_failed');
  await rejectsCode(s.navigate(O, { sessionId: ses.id, url: 'http://169.254.169.254/' }), 'blocked_destination');
}, { maxResponseBytes: 100000 }));

test('sessions: cookie isolation between sessions and owners', () => withScout(async (s) => {
  const a = s.sessions.create(O); const b = s.sessions.create(O);
  await s.navigate(O, { sessionId: a.id, url: fx.url + '/cookie-set' });
  const echo = async (id) => (await s.extract(O, { sessionId: id, url: fx.url + '/cookie-echo' })).data.json.cookie;
  assert.equal(await echo(a.id), 'sid=abc123');
  assert.equal(await echo(b.id), '');
  await assert.rejects(s.navigate('owner-b', { sessionId: a.id, url: fx.url + '/' }), (e) => e.code === 'not_found');
  assert.throws(() => s.destroySession('owner-b', a.id), (e) => e.code === 'not_found');
  s.destroySession(O, a.id);
  await rejectsCode(s.navigate(O, { sessionId: a.id, url: fx.url + '/' }), 'not_found');
  assert.equal(a.jar.size, 0); assert.equal(a.page, null);
}));

test('sessions: limit and idle expiry', async () => {
  const s = makeScout({ maxSessions: 2, sessionTtlMs: 60 });
  try {
    const a = s.sessions.create(O); s.sessions.create(O);
    assert.throws(() => s.sessions.create(O), (e) => e.code === 'too_many_sessions');
    await new Promise((r) => setTimeout(r, 120));
    s.sessions.create(O); // sweep frees expired sessions
    assert.equal(a.destroyed, true);
  } finally { await s.close(); }
});

test('interaction: link click, form fill + POST submit, GET form, checkbox, select', () => withScout(async (s) => {
  const ses = s.sessions.create(O);
  const act = (action) => s.interact(O, { sessionId: ses.id, action });
  await s.navigate(O, { sessionId: ses.id, url: fx.url + '/form' });
  await act({ type: 'fill', selector: 'input[name=q]', value: 'hello world' });
  await act({ type: 'fill', selector: 'input[name=pw]', value: 's3cret&=' });
  await act({ type: 'check', selector: 'input[name=agree]' });
  await act({ type: 'select', selector: 'select', value: 'blue' });
  const r = await act({ type: 'click', selector: 'button[name=go]' });
  assert.equal(r.navigated, true);
  const out = (await s.extract(O, { sessionId: ses.id, schema: parseSchema({ fields: { body: '#out', ct: '#ct', ref: '#ref' } }) })).data;
  const params = new URLSearchParams(out.body);
  assert.deepEqual(Object.fromEntries(params), { q: 'hello world', token: 't1', pw: 's3cret&=', agree: 'yes', note: 'hello', color: 'blue', go: '1' });
  assert.equal(out.ct, 'application/x-www-form-urlencoded');
  assert.equal(out.ref, fx.url + '/form');

  await s.navigate(O, { sessionId: ses.id, url: fx.url + '/form' });
  await act({ type: 'fill', selector: 'input[name=s]', value: 'a b' });
  await act({ type: 'press', selector: 'input[name=s]', key: 'Enter' });
  assert.equal((await s.extract(O, { sessionId: ses.id, schema: parseSchema({ fields: { q: '#out' } }) })).data.q, '?s=a+b');

  await s.navigate(O, { sessionId: ses.id, url: fx.url + '/form' });
  const c = await act({ type: 'click', selector: '#next' });
  assert.equal(c.page.url, fx.url + '/p/alpha');
}));

test('interaction failures are explicit, never silent', () => withScout(async (s) => {
  const ses = s.sessions.create(O);
  const act = (action) => s.interact(O, { sessionId: ses.id, action });
  await rejectsCode(act({ type: 'click', selector: 'a' }), 'conflict'); // no page yet
  await s.navigate(O, { sessionId: ses.id, url: fx.url + '/form' });
  await rejectsCode(act({ type: 'click', selector: '#nope' }), 'element_not_found');
  await rejectsCode(act({ type: 'click', selector: '#js-link' }), 'unsupported_capability');
  await rejectsCode(act({ type: 'click', selector: '#onclick' }), 'unsupported_capability');
  await rejectsCode(act({ type: 'click', selector: '#js' }), 'unsupported_capability');
  await rejectsCode(act({ type: 'scroll' }), 'unsupported_capability');
  await rejectsCode(act({ type: 'press', selector: 'input[name=q]', key: 'Tab' }), 'unsupported_capability');
  await rejectsCode(act({ type: 'fill', selector: 'input[type=file]', value: 'x' }), 'unsupported_capability');
  await rejectsCode(act({ type: 'select', selector: 'select', value: 'green' }), 'element_not_found');
  await rejectsCode(act({ type: 'submit', selector: 'form[enctype] input' }), 'unsupported_capability');
}));

test('tasks: multi-step success, partial-failure reporting, unsupported strategies fail before network', () => withScout(async (s) => {
  const wait = async (id) => { for (;;) { const t = s.getTask(O, id); if (['succeeded', 'failed', 'cancelled', 'timed_out'].includes(t.status)) return t; await new Promise((r) => setTimeout(r, 20)); } };
  const spec = (steps, o = {}) => ({ steps, retries: 1, ...o });
  const ok = await wait(s.submitTask(O, spec([{ op: 'navigate', url: fx.url + '/products' }, { op: 'extract', schema: SCHEMA }])).id);
  assert.equal(ok.status, 'succeeded'); assert.equal(ok.result.data.length, 3); assert.equal(ok.result.steps.length, 2);
  const bad = await wait(s.submitTask(O, spec([{ op: 'navigate', url: fx.url + '/products' }, { op: 'interact', action: { type: 'click', selector: '#none' } }])).id);
  assert.equal(bad.status, 'failed'); assert.equal(bad.error.code, 'element_not_found'); assert.equal(bad.error.details.failedStep, 1); assert.equal(bad.result, null);
  const hits = fx.hits.cookieEcho.length;
  for (const strategy of ['facebook-jobs', 'facebook-marketplace', 'tiktok-profile']) {
    assert.throws(() => s.submitTask(O, spec([{ op: 'extract', url: fx.url + '/cookie-echo', strategy }])), (e) => e.code === 'unsupported_capability');
  }
  assert.equal(fx.hits.cookieEcho.length, hits);
  assert.equal(s.sessions.size, 0, 'task sessions released on success and failure');
}));

test('tasks: navigation retries on 503 and succeeds; with retries=0 it fails', () => withScout(async (s) => {
  const run = async (id, retries) => { const t = s.submitTask(O, { steps: [{ op: 'navigate', url: `${fx.url}/flaky?id=${id}&n=2` }], retries }); for (;;) { const x = s.getTask(O, t.id); if (!['queued', 'running'].includes(x.status)) return x; await new Promise((r) => setTimeout(r, 20)); } };
  assert.equal((await run('a', 2)).status, 'succeeded');
  const f = await run('b', 0);
  assert.equal(f.status, 'failed'); assert.equal(f.error.details.status, 503);
}, { maxRetries: 2 }));

test('tasks: cancel running and queued, timeout, queue bound', () => withScout(async (s) => {
  const slow = { steps: [{ op: 'navigate', url: fx.url + '/slow', timeoutMs: 20000 }], timeoutMs: 20000 };
  const t1 = s.submitTask(O, slow); const t2 = s.submitTask(O, slow); const t3 = s.submitTask(O, slow);
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(s.getTask(O, t1.id).status, 'running'); assert.equal(s.getTask(O, t2.id).status, 'queued');
  assert.throws(() => s.submitTask(O, slow), (e) => e.code === 'queue_full' && e.retryable);
  assert.equal(s.cancelTask(O, t2.id).status, 'cancelled');
  assert.equal(s.cancelTask(O, t1.id).status, 'cancelled');
  assert.equal(s.cancelTask(O, t1.id).status, 'cancelled', 'cancel is idempotent');
  assert.throws(() => s.cancelTask('owner-b', t1.id), (e) => e.code === 'not_found');
  await new Promise((r) => setTimeout(r, 150)); // t3 now running; t1's session must have been released
  assert.equal(s.getTask(O, t3.id).status, 'running');
  s.cancelTask(O, t3.id);
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(s.sessions.size, 0); assert.deepEqual(s.tasks.stats(), { queued: 0, running: 0, retained: 3 });
  const to = s.submitTask(O, { steps: [{ op: 'navigate', url: fx.url + '/slow', timeoutMs: 20000 }], timeoutMs: 1000 });
  await new Promise((r) => setTimeout(r, 1300));
  const t = s.getTask(O, to.id);
  assert.equal(t.status, 'timed_out'); assert.equal(t.error.code, 'task_timeout'); assert.equal(t.result, null);
  assert.equal(s.sessions.size, 0);
}, { maxQueued: 2 }));

test('memory watchdog aborts an in-flight task and releases it', () => withScout(async (s) => {
  const t = s.submitTask(O, { steps: [{ op: 'navigate', url: fx.url + '/slow', timeoutMs: 20000 }], timeoutMs: 20000 });
  await new Promise((r) => setTimeout(r, 500));
  const x = s.getTask(O, t.id);
  assert.equal(x.status, 'failed'); assert.equal(x.error.code, 'resource_limit'); assert.equal(s.sessions.size, 0);
}, { memoryHardLimitBytes: 1 }));

test('admission control rejects new work above the soft memory limit', () => withScout(async (s) => {
  assert.throws(() => s.submitTask(O, { steps: [{ op: 'navigate', url: fx.url + '/' }] }), (e) => e.code === 'overloaded' && e.retryable);
}, { memorySoftLimitBytes: 1 }));

test('YouTube adapter parses fixture-shaped ytInitialData (brace-safe), reports gaps', () => withScout(async (s) => {
  const r = await s.extract(O, { url: fx.url + '/@testchan', strategy: 'youtube-channel-videos', limit: 3 });
  assert.equal(r.data.channel.id, 'UCabc123'); assert.equal(r.data.channel.handle, '@testchan'); assert.match(r.data.channel.description, /} brace {/);
  assert.equal(r.data.videos.length, 3);
  assert.deepEqual(r.data.videos[0], { videoId: 'vid0', title: 'Video 0', url: 'https://www.youtube.com/watch?v=vid0', publishedText: '1 days ago', viewCountText: '10 views', duration: '1:00' });
  assert.equal(r.warnings.length, 0);
  const more = await s.extract(O, { url: fx.url + '/@testchan/videos', strategy: 'youtube-channel-videos', limit: 20 });
  assert.equal(more.data.videos.length, 5);
  assert.equal(more.warnings[0].code, 'continuation_unsupported');
  await rejectsCode(s.extract(O, { url: fx.url + '/@empty/videos', strategy: 'youtube-channel-videos' }), 'compatibility_error');
  await rejectsCode(s.extract(O, { url: 'https://example.com/@x', strategy: 'youtube-channel-videos' }), 'invalid_request');
  assert.equal(parseYoutubeChannel(parseHtml('<script>var ytInitialData = {"a":1};</script>')).warnings.map((w) => w.code).join(), 'channel_missing,no_videos');
}));
