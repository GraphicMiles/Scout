// Capture Chromium screenshots of every fixture, so the paint stage has goldens.
//
//   node harness/shots.mjs          # all fixtures
//   node harness/shots.mjs 01 04    # just these
//
// Output: corpus/screenshots/<fixture>.png at the same 800x600 viewport the
// measurement harness uses, with deviceScaleFactor 1.

import { chromium } from 'playwright-core';
import { readdirSync, mkdirSync, readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const fixturesDir = join(root, 'corpus/fixtures');
const outDir = join(root, 'corpus/screenshots');
mkdirSync(outDir, { recursive: true });

const only = process.argv.slice(2);
const files = readdirSync(fixturesDir)
  .filter((f) => f.endsWith('.html'))
  .filter((f) => only.length === 0 || only.some((p) => f.startsWith(p)))
  .sort();

const browser = await chromium.launch({ args: ['--force-color-profile=srgb', '--font-render-hinting=none'] });
const page = await browser.newPage({ viewport: { width: 800, height: 600 }, deviceScaleFactor: 1 });

for (const file of files) {
  const name = file.replace(/\.html$/, '');
  const html = readFileSync(join(fixturesDir, file), 'utf8');
  await page.setContent(html, { waitUntil: 'load' });
  await page.screenshot({ path: join(outDir, `${name}.png`), animations: 'disabled' });
  console.log(`shot ${name}`);
}

await browser.close();
