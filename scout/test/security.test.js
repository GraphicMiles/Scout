import test from 'node:test';
import assert from 'node:assert/strict';
import { validateUrl, makeLookup, guardedFetch, isBlockedIp } from '../src/security.js';
import { CookieJar } from '../src/cookies.js';
import { startFixture, testConfig } from './helpers.js';

const strict = { privateAllowlist: [], allowedDomains: [], allowedPorts: [80, 443] };

test('SSRF: blocked destinations', () => {
  for (const u of ['http://127.0.0.1/', 'http://localhost/', 'http://foo.localhost/', 'http://[::1]/', 'http://169.254.169.254/latest/meta-data/',
    'http://2130706433/', 'http://0x7f.0.0.1/', 'http://0177.0.0.1/', 'http://[::ffff:127.0.0.1]/', 'http://10.1.1.1/', 'http://172.16.0.1/', 'http://192.168.1.1/',
    'http://100.64.0.1/', 'http://metadata.google.internal/', 'http://intranet/', 'http://[fd00::1]/', 'http://[fe80::1]/',
    'file:///etc/passwd', 'ftp://example.com/', 'gopher://example.com/', 'http://user:pw@example.com/', 'http://example.com:22/', 'http://example.com:6379/']) {
    assert.throws(() => validateUrl(u, strict), (e) => e.code === 'blocked_destination', u);
  }
});
test('SSRF: public destinations are allowed; garbage is invalid_request', () => {
  for (const u of ['https://example.com/', 'http://example.com:80/x?y=1', 'https://sub.example.co.uk/']) assert.doesNotThrow(() => validateUrl(u, strict), u);
  assert.throws(() => validateUrl('not a url', strict), (e) => e.code === 'invalid_request');
});
test('allowedDomains restricts everything else', () => {
  const cfg = { ...strict, allowedDomains: ['example.com'] };
  assert.doesNotThrow(() => validateUrl('https://www.example.com/', cfg));
  assert.throws(() => validateUrl('https://evil.com/', cfg), (e) => e.code === 'blocked_destination');
  assert.throws(() => validateUrl('https://notexample.com/', cfg), (e) => e.code === 'blocked_destination');
});
test('IP range checks', () => {
  assert.equal(isBlockedIp('8.8.8.8'), false);
  assert.equal(isBlockedIp('2606:4700:4700::1111'), false);
  assert.equal(isBlockedIp('::ffff:8.8.8.8'), true); // mapped addresses are never legitimate DNS answers
  assert.equal(isBlockedIp('not-an-ip'), true);
});
test('DNS rebinding: resolved addresses are re-validated at connect time', async () => {
  const resolver = (host, opts, cb) => cb(null, host === 'evil.test' ? [{ address: '10.0.0.5', family: 4 }] : host === 'mixed.test' ? [{ address: '93.184.216.34', family: 4 }, { address: '127.0.0.1', family: 4 }] : [{ address: '93.184.216.34', family: 4 }]);
  const look = makeLookup({ ...strict, resolver });
  const run = (h, o = {}) => new Promise((res) => look(h, o, (err, a) => res({ err, a })));
  assert.equal((await run('evil.test')).err?.code, 'blocked_destination');
  assert.equal((await run('mixed.test', { all: true })).err?.code, 'blocked_destination');
  assert.equal((await run('ok.test', { all: true })).a[0].address, '93.184.216.34');
  assert.equal((await run('ok.test')).a, '93.184.216.34');
});
test('redirects are validated hop by hop', async () => {
  const fx = await startFixture();
  const cfg = testConfig();
  try {
    for (const p of ['/redirect-meta', '/redirect-private', '/redirect-file']) {
      await assert.rejects(guardedFetch(cfg, { url: fx.url + p }), (e) => e.code === 'blocked_destination' && /Redirect to/.test(e.message), p);
    }
    await assert.rejects(guardedFetch(cfg, { url: fx.url + '/redirect-loop' }), (e) => e.code === 'navigation_failed' && /Too many redirects/.test(e.message));
    const ok = await guardedFetch(cfg, { url: fx.url + '/redirect' });
    assert.equal(ok.url, fx.url + '/products');
    assert.equal(ok.redirects.length, 1);
  } finally { await fx.close(); }
});
test('a host that is not allowlisted is refused even if reachable', async () => {
  const fx = await startFixture();
  try { await assert.rejects(guardedFetch(testConfig({ privateAllowlist: [] }), { url: fx.url + '/' }), (e) => e.code === 'blocked_destination'); }
  finally { await fx.close(); }
});

test('cookie jar: scoping, expiry, secure, rejection of cross-domain and TLD cookies', () => {
  let now = 1_000_000;
  const j = new CookieJar(() => now);
  const u = new URL('https://shop.example.com/a/b');
  j.store(u, ['a=1; Path=/', 'b=2; Domain=example.com', 'c=3; Domain=other.com', 'd=4; Domain=com', 'e=5; Max-Age=10', 'f=6; Secure; Path=/a', 'g=7; Path=/zzz']);
  assert.equal(j.header(u).split('; ').sort().join(';'), 'a=1;b=2;e=5;f=6');
  assert.equal(j.header('https://example.com/a/x'), 'b=2'); // domain cookie, default path /a
  assert.ok(!j.header('http://shop.example.com/a/b').includes('f=6'));
  assert.equal(j.header('https://evil.com/'), '');
  now += 11_000;
  assert.ok(!j.header(u).includes('e=5'));
  j.store(u, 'a=; Max-Age=0; Path=/');
  assert.ok(!j.header(u).includes('a=1'));
  j.clear();
  assert.equal(j.size, 0);
});
