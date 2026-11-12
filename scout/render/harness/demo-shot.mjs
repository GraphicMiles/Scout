import { chromium } from 'playwright-core';
import { readFileSync } from 'node:fs';
const html = readFileSync('corpus/extra/page.html', 'utf8');
const b = await chromium.launch({ args: ['--force-color-profile=srgb'] });
const p = await b.newPage({ viewport: { width: 800, height: 600 }, deviceScaleFactor: 1 });
await p.setContent(html, { waitUntil: 'load' });
await p.screenshot({ path: 'corpus/extra/page.png', fullPage: true });
await b.close();
