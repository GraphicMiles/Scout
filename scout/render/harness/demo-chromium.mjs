// Render a local page with Chromium at the same viewport as our screenshot,
// for a side-by-side.   node harness/demo-chromium.mjs demo/capabilities.html out.png 1100 900
import { chromium } from 'playwright-core';
import path from 'node:path';
const [file, out, width = '1100', height = '900'] = process.argv.slice(2);
const browser = await chromium.launch({ args: ['--force-color-profile=srgb', '--font-render-hinting=none', '--disable-lcd-text'] });
const page = await browser.newPage({ viewport: { width: +width, height: +height }, deviceScaleFactor: 1 });
await page.goto('file://' + path.resolve(file), { waitUntil: 'load' });
await page.screenshot({ path: out, animations: 'disabled' });
await browser.close();
console.log('wrote', out);
