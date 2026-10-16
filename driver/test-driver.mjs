/**
 * Driver integration test: launch the binary, drive a page, shut down.
 *
 *   SCOUT_BIN=scout/target/release/scout node driver/test-driver.mjs
 *
 * Hits a live page (example.com) unless FIXTURE_URL is provided.
 */
import assert from 'node:assert/strict';
import { launch } from './index.js';

const bin = process.env.SCOUT_BIN;
const target = process.env.FIXTURE_URL ?? 'https://example.com/';

let passed = 0;
let failed = 0;
function check(name, fn) {
  return Promise.resolve()
    .then(fn)
    .then(
      () => {
        passed++;
        console.log(`  ok   ${name}`);
      },
      (e) => {
        failed++;
        console.log(`  FAIL ${name}: ${e.message}`);
      }
    );
}

const scout = await launch(bin ? { executablePath: bin } : {});

try {
  await check('server answers /health', async () => {
    const h = await scout.client.health();
    assert.ok(h, 'health returned nothing');
  });

  const page = await scout.newPage();

  await check('goto loads a page', async () => {
    const nav = await page.goto(target);
    assert.equal(nav.status, 200, `status was ${nav.status}`);
  });

  await check('title() reads the document', async () => {
    const title = await page.title();
    assert.equal(title, 'Example Domain');
  });

  await check('text() reads the body', async () => {
    const text = await page.text();
    assert.match(text, /documentation examples/i);
  });

  await check('url() reports the page', async () => {
    assert.equal(await page.url(), 'https://example.com/');
  });

  await check('eval() runs in the page', async () => {
    const r = await page.eval('return document.querySelectorAll("a").length;');
    assert.equal(r.value, 1);
  });

  await check('second page is independent', async () => {
    const other = await scout.newPage();
    assert.notEqual(other.id, page.id);
    await other.close();
  });

  await check('page.close() destroys the session', async () => {
    await page.close();
    await assert.rejects(() => page.title());
  });
} finally {
  await scout.close();
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
