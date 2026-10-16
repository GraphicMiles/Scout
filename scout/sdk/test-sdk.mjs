/**
 * SDK integration test. Runs against a live Scout instance.
 *
 *   SCOUT_URL=http://127.0.0.1:8203 SCOUT_API_KEY=... node sdk/test-sdk.mjs
 *
 * Exercises the documented public interface, including the example from the
 * brief, so the SDK is verified against the real service rather than a mock.
 */
import { ScoutClient, ScoutApiError } from './index.js';

const baseUrl = process.env.SCOUT_URL ?? 'http://127.0.0.1:8203';
const apiKey = process.env.SCOUT_API_KEY ?? 'live_test_key_0123456789abcdef';
const scout = new ScoutClient({ baseUrl, apiKey });

let passed = 0;
let failed = 0;

async function check(name, fn) {
  try {
    await fn();
    passed++;
    console.log(`  ok   ${name}`);
  } catch (e) {
    failed++;
    console.log(`  FAIL ${name}: ${e.message}`);
  }
}

function assert(cond, msg) {
  if (!cond) throw new Error(msg ?? 'assertion failed');
}

console.log(`Scout SDK test against ${baseUrl}\n`);

await check('health()', async () => {
  const h = await scout.health();
  assert(h.status === 'ok', `status was ${h.status}`);
  assert(typeof h.version === 'string');
});

await check('strategies() reports honest statuses', async () => {
  const s = await scout.strategies();
  assert(Array.isArray(s), 'strategies should be an array');
  const yt = s.find((x) => x.name === 'youtube-channel-videos');
  assert(yt, 'youtube strategy missing');
  assert(yt.status === 'verified-live', `youtube status was ${yt.status}`);
  const tt = s.find((x) => x.name === 'tiktok-profile');
  assert(tt?.status === 'not_implemented', 'tiktok should be not_implemented');
  assert(typeof tt.reason === 'string', 'unimplemented strategy must carry a reason');
});

await check('auth failure raises a typed error', async () => {
  const bad = new ScoutClient({ baseUrl, apiKey: 'wrongkey_0123456789abcdef' });
  try {
    await bad.strategies();
    throw new Error('expected an error');
  } catch (e) {
    assert(e instanceof ScoutApiError, 'should be a ScoutApiError');
    assert(e.status === 401, `status was ${e.status}`);
    assert(e.code === 'unauthorized', `code was ${e.code}`);
  }
});

await check('withSession create/use/destroy', async () => {
  const out = await scout.withSession(async (sessionId) => {
    assert(typeof sessionId === 'string' && sessionId.startsWith('ses_'), 'bad session id');
    const nav = await scout.navigate({ sessionId, url: 'https://example.com/' });
    assert(nav.status === 200, `navigate status ${nav.status}`);
    return nav.title;
  });
  assert(out === 'Example Domain', `title was ${out}`);
});

await check('withSession releases the session afterwards', async () => {
  let sid;
  await scout.withSession(async (id) => {
    sid = id;
  });
  try {
    await scout.extract({ sessionId: sid, strategy: 'generic-page' });
    throw new Error('session should be gone');
  } catch (e) {
    assert(e.code === 'not_found', `code was ${e.code}`);
  }
});

await check('extract with a selector schema', async () => {
  const r = await scout.extract({
    url: 'https://example.com/',
    schema: {
      item: 'a',
      fields: { href: { selector: 'a', type: 'url' } },
    },
  });
  assert(Array.isArray(r.data), 'data should be an array');
  assert(r.meta.strategy === 'selectors', `strategy was ${r.meta.strategy}`);
});

await check('the brief\'s YouTube example works', async () => {
  const result = await scout.extract({
    url: 'https://www.youtube.com/@NASA/videos',
    strategy: 'youtube-channel-videos',
    limit: 3,
  });
  assert(result.data.channel.title === 'NASA', `channel was ${result.data.channel.title}`);
  assert(result.data.videos.length === 3, `got ${result.data.videos.length} videos`);
  const v = result.data.videos[0];
  assert(v.videoId && v.title && v.url, 'video needs id, title and url');
  assert(v.url.startsWith('https://www.youtube.com/watch?v='), `url was ${v.url}`);
});

await check('unsupported strategy raises unsupported_capability', async () => {
  try {
    await scout.extract({ url: 'https://www.facebook.com/', strategy: 'facebook-jobs' });
    throw new Error('expected an error');
  } catch (e) {
    assert(e.code === 'unsupported_capability', `code was ${e.code}`);
    assert(e.retryable === false, 'should not be retryable');
  }
});

await check('SSRF is blocked with a typed error', async () => {
  try {
    await scout.extract({ url: 'http://169.254.169.254/latest/meta-data/' });
    throw new Error('expected an error');
  } catch (e) {
    assert(e.status === 403, `status was ${e.status}`);
    assert(e.code === 'blocked_destination', `code was ${e.code}`);
  }
});

await check('background task lifecycle', async () => {
  const task = await scout.submitTask({
    steps: [
      { op: 'navigate', url: 'https://example.com/' },
      { op: 'extract', strategy: 'generic-page' },
    ],
  });
  assert(task.status === 'queued', `initial status ${task.status}`);
  const done = await scout.waitForTask(task.id, { timeoutMs: 60000 });
  assert(done.status === 'succeeded', `final status ${done.status}: ${JSON.stringify(done.error)}`);
  assert(done.result.steps.length === 2, 'expected two step results');
  assert(done.result.steps[0].result.status === 200, 'the navigate step reports its HTTP status');
  assert(done.result.data.title === 'Example Domain', `title was ${done.result.data.title}`);
});

await check('task cancellation', async () => {
  const task = await scout.submitTask({
    steps: [{ op: 'navigate', url: 'https://example.com/', timeoutMs: 5000 }],
  });
  await scout.cancelTask(task.id);
  // waitForTask deliberately rejects for non-succeeded terminal states, so poll.
  let done = await scout.getTask(task.id);
  const deadline = Date.now() + 30000;
  while (!['cancelled', 'failed', 'succeeded'].includes(done.status) && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 200));
    done = await scout.getTask(task.id);
  }
  assert(done.status === 'cancelled', `status was ${done.status}`);
  assert(done.result == null, 'a cancelled task must not expose a result');
});

await check('waitForTask rejects a cancelled task', async () => {
  const task = await scout.submitTask({
    steps: [{ op: 'navigate', url: 'https://example.com/', timeoutMs: 5000 }],
  });
  await scout.cancelTask(task.id);
  try {
    await scout.waitForTask(task.id, { timeoutMs: 30000 });
    throw new Error('expected waitForTask to reject');
  } catch (e) {
    assert(e instanceof ScoutApiError, `expected ScoutApiError, got ${e.name}`);
    assert(e.retryable === false, 'a cancellation must not be retryable');
  }
});

await check('unknown route is a typed not_found', async () => {
  try {
    await scout._nope?.();
    // No such method; exercise the raw path instead.
    const res = await fetch(`${baseUrl}/v1/nope`, { headers: { authorization: `Bearer ${apiKey}` } });
    assert(res.status === 404, `status was ${res.status}`);
  } catch (e) {
    throw e;
  }
});

await check('unknown request field is rejected by name', async () => {
  try {
    await scout.navigate({ sessionId: 'ses_x', url: 'https://example.com/', typoField: 1 });
    throw new Error('expected an error');
  } catch (e) {
    assert(e.code === 'invalid_request', `code was ${e.code}`);
    assert(e.message.includes('typoField'), `message was ${e.message}`);
  }
});

await check('eval executes JavaScript against a loaded page', async () => {
  await scout.withSession(async (sessionId) => {
    await scout.navigate({ sessionId, url: 'https://example.com/' });
    // example.com's markup is not stable, so assert against the parsed document
    // rather than a hardcoded selector.
    const r = await scout.eval({ sessionId, script: "return document.title;" });
    assert(r.value === 'Example Domain', `title was ${JSON.stringify(r.value)}`);
    const t = await scout.eval({
      sessionId,
      script: "return document.body.textContent.trim().slice(0, 30);",
    });
    assert(String(t.value).startsWith('This domain is for use'), `text was ${JSON.stringify(t.value)}`);
    const n = await scout.eval({ sessionId, script: "return document.querySelectorAll('p').length;" });
    assert(n.value >= 1, `p count was ${n.value}`);
    assert(t.logs.length === 0 && t.timedOut === false, 'unexpected logs or timeout');
  });
});

await check('eval captures console output', async () => {
  await scout.withSession(async (sessionId) => {
    await scout.navigate({ sessionId, url: 'https://example.com/' });
    const r = await scout.eval({ sessionId, script: "console.log('hi', 1+1); return null;" });
    assert(r.logs.length === 1, `logs were ${JSON.stringify(r.logs)}`);
    assert(r.logs[0].includes('hi 2'), `log was ${r.logs[0]}`);
    assert(r.value === null, 'value should be null');
  });
});

await check('eval stops a runaway script', async () => {
  await scout.withSession(async (sessionId) => {
    await scout.navigate({ sessionId, url: 'https://example.com/' });
    try {
      await scout.eval({ sessionId, script: 'while(true){}' });
      throw new Error('expected the script to be interrupted');
    } catch (e) {
      assert(e.status === 422, `status was ${e.status}`);
      assert(['compatibility_error', 'timeout', 'resource_limit'].includes(e.code), `code was ${e.code}`);
    }
  });
});

await check('tools() lists what an agent can call', async () => {
  const tools = await scout.tools();
  assert(Array.isArray(tools) && tools.length > 0, 'no tools');
  const names = tools.map((t) => t.name);
  for (const n of ['navigate', 'extract', 'request', 'auth.login', 'session.export']) {
    assert(names.includes(n), `missing tool ${n}`);
  }
  for (const t of tools) assert(t.inputSchema?.type === 'object', `${t.name} has no input schema`);
});

await check('request() returns the response unparsed', async () => {
  await scout.withSession(async (sessionId) => {
    const r = await scout.request({ sessionId, url: 'https://example.com/' });
    assert(r.status === 200, `status was ${r.status}`);
    assert(r.body.includes('Example Domain'), 'body did not come back verbatim');
    assert(typeof r.headers['content-type'] === 'string', 'headers missing');
  });
});

// Signing in is only exercised against a fixture the operator points at: a
// public site's login form is not ours to test against.
const fixture = process.env.SCOUT_FIXTURE_URL;
if (fixture) {
  await check('login() signs in and returns the evidence', async () => {
    await scout.withSession(async (sessionId) => {
      const r = await scout.login({
        sessionId,
        url: `${fixture}/login`,
        username: 'tester@example.com',
        password: 'hunter2',
      });
      assert(r.looksAuthenticated === true, `looksAuthenticated was ${r.looksAuthenticated}: ${r.reason}`);
      assert(r.cookies.added.includes('sid'), `cookies were ${JSON.stringify(r.cookies)}`);
      assert(r.passwordFieldStillPresent === false, 'the sign-in form is still there');
    });
  });

  await check('login() refuses wrong credentials instead of guessing', async () => {
    await scout.withSession(async (sessionId) => {
      const r = await scout.login({
        sessionId,
        url: `${fixture}/login`,
        username: 'tester@example.com',
        password: 'wrong',
      });
      assert(r.looksAuthenticated === false, 'a wrong password must not authenticate');
      assert(r.cookies.added.length === 0, 'no session should have been issued');
    });
  });

  await check('a signed-in session can be exported and resumed', async () => {
    const s1 = await scout.createSession();
    const s2 = await scout.createSession();
    try {
      const login = await scout.login({
        sessionId: s1.id,
        url: `${fixture}/login`,
        username: 'tester@example.com',
        password: 'hunter2',
      });
      assert(login.looksAuthenticated === true, login.reason);
      const exported = await scout.exportSession(s1.id);
      await scout.importSession(s2.id, `${fixture}/`, exported.cookies);
      const nav = await scout.navigate({ sessionId: s2.id, url: `${fixture}/account` });
      assert(nav.status === 200, `status was ${nav.status}`);
      const text = JSON.stringify(await scout.callTool('page.text', { sessionId: s2.id }));
      assert(text.includes('tester@example.com'), `the resumed session is not signed in: ${text}`);
    } finally {
      await scout.destroySession(s1.id).catch(() => {});
      await scout.destroySession(s2.id).catch(() => {});
    }
  });
} else {
  console.log('  skip login() — set SCOUT_FIXTURE_URL to exercise the sign-in flow');
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
