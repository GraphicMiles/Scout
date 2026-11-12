/**
 * Record the ground truth: load every fixture in Chromium and write, for each
 * element, its index path, box and computed styles.
 *
 *   node harness/measure.mjs            # all fixtures
 *   node harness/measure.mjs 01 02      # matching fixtures only
 *
 * The output in corpus/goldens/ is committed. The Rust engine is tested against
 * it, so the test suite needs no browser.
 */

import { chromium } from 'playwright-core';
import { readdir, readFile, writeFile, mkdir } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const FIXTURES = path.join(ROOT, 'corpus/fixtures');
const GOLDENS = path.join(ROOT, 'corpus/goldens');

const VIEWPORT = { width: 800, height: 600 };
const FONT = 'DejaVu Sans';

/** Computed properties recorded per element. Keep in sync with src/style.rs. */
const STYLE_PROPS = [
  'display', 'position', 'visibility',
  'font-family', 'font-size', 'font-weight', 'font-style', 'line-height',
  'letter-spacing', 'text-align', 'text-transform', 'white-space',
  'color', 'background-color',
  'margin-top', 'margin-right', 'margin-bottom', 'margin-left',
  'padding-top', 'padding-right', 'padding-bottom', 'padding-left',
  'border-top-width', 'border-right-width', 'border-bottom-width', 'border-left-width',
  'width', 'height', 'box-sizing', 'overflow',
];

const filtered = process.argv.slice(2);

const browser = await chromium.launch();
await mkdir(GOLDENS, { recursive: true });

const files = (await readdir(FIXTURES))
  .filter((f) => f.endsWith('.html'))
  .filter((f) => filtered.length === 0 || filtered.some((p) => f.startsWith(p)))
  .sort();

let totalNodes = 0;

for (const file of files) {
  const page = await browser.newPage({ viewport: VIEWPORT, deviceScaleFactor: 1 });
  const html = await readFile(path.join(FIXTURES, file), 'utf8');
  await page.setContent(html, { waitUntil: 'load' });
  await page.evaluate(() => document.fonts.ready);

  const tree = await page.evaluate((props) => {
    const kids = (el) => [...el.children];

    function walk(el, indexPath) {
      const rect = el.getBoundingClientRect();
      const cs = getComputedStyle(el);
      const styles = {};
      for (const p of props) styles[p] = cs.getPropertyValue(p);

      const node = {
        i: indexPath,
        tag: el.tagName.toLowerCase(),
        rect: [
          Math.round(rect.x * 100) / 100,
          Math.round(rect.y * 100) / 100,
          Math.round(rect.width * 100) / 100,
          Math.round(rect.height * 100) / 100,
        ],
        styles,
        children: kids(el).map((c, k) => walk(c, [...indexPath, k])),
      };
      return node;
    }

    return {
      root: walk(document.documentElement, [0]),
      textWidth: document.body.scrollWidth,
      docHeight: document.documentElement.scrollHeight,
    };
  }, STYLE_PROPS);

  const count = (n) => 1 + n.children.reduce((a, c) => a + count(c), 0);
  totalNodes += count(tree.root);

  const golden = {
    fixture: file.replace(/\.html$/, ''),
    viewport: VIEWPORT,
    font: FONT,
    document: { width: tree.textWidth, height: tree.docHeight },
    props: STYLE_PROPS,
    root: tree.root,
  };

  await writeFile(path.join(GOLDENS, golden.fixture + '.json'), JSON.stringify(golden, null, 1));
  console.log(`${golden.fixture.padEnd(24)} ${String(count(tree.root)).padStart(4)} nodes  ` +
    `doc ${golden.document.width}x${golden.document.height}`);
  await page.close();
}

await browser.close();
console.log(`\n${files.length} fixtures, ${totalNodes} element nodes recorded`);
