// Measures Scout's own resident memory, CPU time, latency and success rate against local fixtures.
// Linux only (reads /proc). Usage: node bench/bench.js [--n 100] [--large-items 8000]
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import { startFixture, productsHtml, PRODUCTS } from '../test/helpers.js';
import { ScoutClient } from '../sdk/index.js';

const arg = (k, d) => { const i = process.argv.indexOf('--' + k); return i > 0 ? Number(process.argv[i + 1]) : d; };
const N = arg('n', 100); const LARGE = arg('large-items', 8000);
const KEY = 'bench-key-0123456789abcdef'; const PORT = 20000 + Math.floor(Math.random() * 20000);

async function proc(pid) {
  const st = await fs.readFile(`/proc/${pid}/status`, 'utf8');
  const stat = (await fs.readFile(`/proc/${pid}/stat`, 'utf8')).split(') ')[1].split(' ');
  return { rss: +/VmRSS:\s+(\d+)/.exec(st)[1] * 1024, peak: +/VmHWM:\s+(\d+)/.exec(st)[1] * 1024, cpuSec: (+stat[11] + +stat[12]) / 100 };
}
const pct = (a, p) => [...a].sort((x, y) => x - y)[Math.min(a.length - 1, Math.floor(a.length * p))];
const mb = (b) => +(b / 1048576).toFixed(1);

const fx = await startFixture();
const t0 = performance.now();
const child = spawn(process.execPath, ['--max-old-space-size=192', '--expose-gc', 'src/index.js'], {
  env: { ...process.env, PORT: String(PORT), HOST: '127.0.0.1', SCOUT_API_KEYS: KEY, SCOUT_PRIVATE_ALLOWLIST: '127.0.0.1', SCOUT_LOG_LEVEL: 'silent', SCOUT_RATE_LIMIT_PER_MIN: '1000000', SCOUT_MAX_RESPONSE_BYTES: String(4 * 1048576) },
  stdio: 'ignore',
});
const scout = new ScoutClient({ baseUrl: `http://127.0.0.1:${PORT}`, apiKey: KEY, maxRetries: 0 });
for (;;) { try { await scout.health(); break; } catch { await new Promise((r) => setTimeout(r, 10)); } }
const coldStartMs = Math.round(performance.now() - t0);
await new Promise((r) => setTimeout(r, 1500));
const idle = await proc(child.pid);

const schema = { item: 'li.item', fields: { name: '.name', price: { selector: '.price', type: 'price' } } };
async function scenario(name, path, check) {
  const before = await proc(child.pid);
  const lat = []; let ok = 0; let correct = 0; let firstMs = null; const rssSamples = [];
  for (let i = 0; i < N; i++) {
    const s = performance.now();
    try {
      const r = await scout.extract({ url: fx.url + path, schema, limit: 200 });
      ok++; if (check(r.data)) correct++;
    } catch { /* counted as failure */ }
    const d = performance.now() - s; lat.push(d); if (i === 0) firstMs = d;
    if (i % 10 === 0) rssSamples.push((await proc(child.pid)).rss);
  }
  const after = await proc(child.pid);
  return {
    scenario: name, tasks: N, successRate: ok / N, extractionAccuracy: correct / N,
    firstResultMs: Math.round(firstMs), p50Ms: Math.round(pct(lat, 0.5)), p95Ms: Math.round(pct(lat, 0.95)),
    cpuMsPerTask: +(((after.cpuSec - before.cpuSec) / N) * 1000).toFixed(1),
    rssMbStart: mb(before.rss), rssMbEnd: mb(after.rss), rssMbMaxSampled: mb(Math.max(...rssSamples)), peakRssMbSoFar: mb(after.peak),
  };
}
const results = [];
results.push(await scenario('small-page (3 items, ~1 KB)', '/products', (d) => d.length === 3 && d[0].name === PRODUCTS[0].name && d[0].price.amount === PRODUCTS[0].amount));
results.push(await scenario(`large-page (${LARGE} items, ~${mb(LARGE * 105)} MB html)`, `/large?items=${LARGE}`, (d) => d.length === 200 && d[199].name === 'Item 199'));
await new Promise((r) => setTimeout(r, 500));
const end = await proc(child.pid);
const report = {
  when: new Date().toISOString(), node: process.version, cpus: (await import('node:os')).cpus().length,
  note: 'Scout process only, measured from /proc. Host CPU is not throttled to 0.1; use cpuMsPerTask (CPU time) to estimate behaviour under --cpus=0.1. Playwright/Chromium and Lightpanda were NOT run.',
  coldStartMs, idleRssMb: mb(idle.rss), finalRssMb: mb(end.rss), peakRssMb: mb(end.peak), totalCpuSec: end.cpuSec, results,
};
console.log(JSON.stringify(report, null, 2));
await fs.writeFile(new URL('./results/latest.json', import.meta.url), JSON.stringify(report, null, 2));
child.kill('SIGTERM'); await fx.close(); process.exit(0);
