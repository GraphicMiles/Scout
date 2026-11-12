/**
 * The Chromium half of the real-site benchmark.
 *
 * For every site in `sites.txt`, load the live page at the benchmark viewport,
 * screenshot it, and measure the boxes of exactly the elements our own snapshot
 * reported — matched by the CSS path our engine prints, which is a valid
 * selector. That makes the comparison a measurement of the same nodes rather
 * than of two different ideas of "the page".
 *
 *   node bench/chrome.mjs            # all sites
 *   node bench/chrome.mjs example    # just these
 *
 * Writes bench/results/<slug>-chrome.png and bench/results/<slug>-chrome.json.
 */

import { chromium } from 'playwright-core';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.dirname(here);
const out = path.join(here, 'results');

const VIEWPORT = { width: 1280, height: 800 };
const USER_AGENT =
  'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36';

const only = process.argv.slice(2);
const sites = readFileSync(path.join(here, 'sites.txt'), 'utf8')
  .split('\n')
  .map((line) => line.trim())
  .filter((line) => line && !line.startsWith('#'))
  .map((line) => {
    const [slug, url] = line.split('\t');
    return { slug, url };
  })
  .filter((s) => only.length === 0 || only.includes(s.slug));

const browser = await chromium.launch({
  args: ['--force-color-profile=srgb', '--font-render-hinting=none', '--disable-lcd-text'],
});

const summary = [];

for (const site of sites) {
  const page = await browser.newPage({
    viewport: VIEWPORT,
    deviceScaleFactor: 1,
    userAgent: USER_AGENT,
    bypassCSP: true,
  });

  const started = process.hrtime.bigint();
  let status = 'ok';
  try {
    // Render the *saved* HTML with a <base> pointing at the live origin, so both
    // renderers see the same bytes and the same stylesheets.
    const saved = path.join(here, 'sites', `${site.slug}.html`);
    const html = readFileSync(saved, 'utf8').replace(
      /<head([^>]*)>/i,
      `<head$1><base href="${site.url}">`,
    );
    await page.setContent(html, { waitUntil: 'load', timeout: 45000 });
    await page.waitForTimeout(600); // let late CSS/fonts settle
    await page.screenshot({ path: path.join(out, `${site.slug}-chrome.png`), animations: 'disabled' });
  } catch (error) {
    status = String(error).split('\n')[0];
  }
  const elapsedMs = Number(process.hrtime.bigint() - started) / 1e6;

  // Measure the boxes for the paths our own snapshot produced. Paths that do
  // not exist in Chromium's DOM are reported as missing, not counted as wrong:
  // a page driven by JavaScript will simply not have them.
  let rects = {};
  const oursPath = path.join(out, `${site.slug}-ours.json`);
  if (status === 'ok' && existsSync(oursPath)) {
    const ours = JSON.parse(readFileSync(oursPath, 'utf8'));
    const paths = ours.elements.map((e) => e.path);
    try {
      rects = await page.evaluate((paths) => {
        const found = {};
        for (const selector of paths) {
          let el = null;
          try {
            el = document.querySelector(selector);
          } catch {
            continue;
          }
          if (!el) continue;
          const r = el.getBoundingClientRect();
          found[selector] = {
            x: +(r.x + window.scrollX).toFixed(2),
            y: +(r.y + window.scrollY).toFixed(2),
            w: +r.width.toFixed(2),
            h: +r.height.toFixed(2),
          };
        }
        return found;
      }, paths);
    } catch (error) {
      status = `measure failed: ${String(error).split('\n')[0]}`;
    }
  }

  const geometry = {
    slug: site.slug,
    url: site.url,
    status,
    elapsed_ms: +elapsedMs.toFixed(1),
    rects,
  };
  writeFileSync(path.join(out, `${site.slug}-chrome.json`), JSON.stringify(geometry, null, 2));
  summary.push(geometry);
  console.log(
    `${site.slug.padEnd(20)} ${status.padEnd(6)} ${elapsedMs.toFixed(0).padStart(6)} ms  ` +
      `${Object.keys(rects).length} boxes matched`,
  );

  await page.close();
}

await browser.close();
writeFileSync(path.join(out, 'chrome-summary.json'), JSON.stringify(summary, null, 2));
