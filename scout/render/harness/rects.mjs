// Print every element's border box for a page, as Chromium sees it.
//   node harness/rects.mjs demo/page.html
import { chromium } from 'playwright-core';
import { readFileSync } from 'node:fs';
const file = process.argv[2];
const html = readFileSync(file, 'utf8');
const b = await chromium.launch();
const p = await b.newPage({ viewport: { width: 800, height: 600 }, deviceScaleFactor: 1 });
await p.setContent(html, { waitUntil: 'load' });
const rects = await p.evaluate(() => {
  const out = [];
  const walk = (el, path) => {
    const r = el.getBoundingClientRect();
    const kids = [...el.children];
    out.push({ path, tag: el.tagName.toLowerCase(), x: +r.x.toFixed(2), y: +r.y.toFixed(2), w: +r.width.toFixed(2), h: +r.height.toFixed(2) });
    kids.forEach((c, i) => walk(c, path + '.' + i));
  };
  walk(document.documentElement, '0');
  return out;
});
console.log(JSON.stringify(rects));
await b.close();
