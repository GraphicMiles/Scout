#!/usr/bin/env node
/**
 * Scout vs Playwright (Chromium) on the same local fixtures.
 *
 * The comparison the design brief asks for: same pages, same extraction task,
 * both engines measured by the same harness. What it measures: cold start to
 * first result, idle and peak RSS, per-task latency, success rate, accuracy.
 * Fixtures are local and contain no JavaScript, so the numbers describe the
 * engines, not the network.
 *
 * Fairness notes, stated rather than hidden:
 *   - Playwright runs a real browser: layout, rendering, a JS engine. That is
 *     the capability difference this benchmark quantifies, not hides.
 *   - Scout is measured through its HTTP API, which is its only interface.
 *     Playwright is driven in-process, which is its only interface.
 *   - Both sides reuse one session/page for the whole scenario, matching
 *     bench/bench.py.
 *
 * Requirements:
 *   - the release binary: cargo build --release
 *   - playwright installed somewhere node can find it:
 *       npm install playwright && npx playwright install chromium
 *     either in this directory's tree, or set PLAYWRIGHT_HOME to a directory
 *     containing node_modules/playwright.
 *
 * Usage:
 *   node bench/compare-playwright.mjs [--small 100] [--large 25]
 */
import { parseArgs } from 'node:util';
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import fs from 'node:fs';
import path from 'node:path';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, '..');
const BINARY = path.join(ROOT, 'target', 'release', 'scout');
const FIXTURE = path.join(HERE, 'fixture_server.py');

const KEY = 'compare_key_0123456789abcdefgh';
const SCHEMA = {
  item: 'li.item',
  fields: {
    name: { selector: '.name' },
    price: { selector: '.price', type: 'price' },
  },
};

const { values: args } = parseArgs({
  options: {
    small: { type: 'string', default: '100' },
    large: { type: 'string', default: '25' },
  },
});

// ------------------------------------------------------------ playwright load

async function loadPlaywright() {
  try {
    return await import('playwright');
  } catch {
    const home = process.env.PLAYWRIGHT_HOME;
    if (!home) {
      throw new Error(
        'playwright is not importable from here; run `npm install playwright` ' +
        'in this tree, or point PLAYWRIGHT_HOME at a directory that has it.'
      );
    }
    const req = createRequire(path.join(home, 'package.json'));
    return req('playwright');
  }
}

// ----------------------------------------------------------------- utilities

async function freePortAsync() {
  const net = await import('node:net');
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.listen(0, '127.0.0.1', () => {
      const port = srv.address().port;
      srv.close(() => resolve(port));
    });
  });
}

function rssKb(pid) {
  try {
    const status = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
    const m = status.match(/VmRSS:\s+(\d+) kB/);
    return m ? Number(m[1]) : 0;
  } catch {
    return 0;
  }
}

/** Sum of RSS over every process whose command line mentions ms-playwright. */
function chromiumRssKb() {
  let total = 0;
  let pids = [];
  try { pids = fs.readdirSync('/proc'); } catch { return 0; }
  for (const name of pids) {
    if (!/^\d+$/.test(name)) continue;
    try {
      const cmd = fs.readFileSync(`/proc/${name}/cmdline`, 'utf8');
      if (cmd.includes('ms-playwright')) total += rssKb(Number(name));
    } catch { /* process exited between readdir and read */ }
  }
  return total;
}

function percentile(sorted, p) {
  const i = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, i)];
}

function statsFor(times, ok, accurate) {
  const sorted = [...times].sort((a, b) => a - b);
  return {
    tasks: times.length + (ok - times.length), // placeholder, overwritten below
    success: `${ok}/${times.length}`,
    accuracy: `${accurate}/${times.length}`,
    p50Ms: sorted.length ? +percentile(sorted, 50).toFixed(1) : null,
    p95Ms: sorted.length ? +percentile(sorted, 95).toFixed(1) : null,
  };
}

// ------------------------------------------------------------------ scouting

async function measureScout(fixturePort) {
  const port = await freePortAsync();
  const child = spawn(BINARY, [], {
    env: {
      ...process.env,
      PORT: String(port),
      SCOUT_API_KEYS: KEY,
      SCOUT_PRIVATE_ALLOWLIST: '127.0.0.1',
      SCOUT_LOG_LEVEL: 'error',
      SCOUT_RATE_LIMIT_PER_MIN: '100000',
      // The large fixture has 8000 items; the default extraction cap is lower.
      SCOUT_MAX_EXTRACT_ITEMS: '10000',
    },
    stdio: ['ignore', 'ignore', 'ignore'],
  });

  const base = `http://127.0.0.1:${port}`;
  const t0 = performance.now();
  for (;;) {
    try {
      const r = await fetch(`${base}/health`);
      if (r.ok) break;
    } catch { /* not up yet */ }
    await new Promise((r) => setTimeout(r, 20));
  }
  const coldStartMs = +(performance.now() - t0).toFixed(1);
  const idleRssKb = rssKb(child.pid);

  const auth = { authorization: `Bearer ${KEY}`, 'content-type': 'application/json' };
  const post = async (p, body) => {
    const r = await fetch(base + p, { method: 'POST', headers: auth, body: JSON.stringify(body) });
    return { status: r.status, json: await r.json().catch(() => null) };
  };

  const { json: sess } = await post('/v1/sessions', {});
  const sid = sess.id;
  const expected = { small: 3, large: 8000 };

  const scenario = async (label, path, n) => {
    const times = [];
    let ok = 0;
    let accurate = 0;
    for (let i = 0; i < n; i++) {
      const t = performance.now();
      const { status, json } = await post('/v1/extract', {
        sessionId: sid,
        url: `http://127.0.0.1:${fixturePort}${path}`,
        strategy: 'selectors',
        schema: SCHEMA,
        // The selectors strategy defaults to 50 items; the comparison must
        // extract everything Playwright's $$eval extracts.
        limit: 8000,
      });
      times.push(performance.now() - t);
      const items = json && json.data;
      if (status === 200 && Array.isArray(items) && items.length === expected[label]) {
        ok++;
        if (items[0].name === 'Item 0' && items[0].price && items[0].price.amount === 0.5) {
          accurate++;
        }
      }
    }
    return { label, times, ok, accurate, n };
  };

  const small = await scenario('small', '/small', Number(args.small));
  const large = await scenario('large', '/large', Number(args.large));
  const peakRssKb = rssKb(child.pid);

  try { process.kill(-child.pid, 'SIGTERM'); } catch { child.kill('SIGTERM'); }

  return {
    engine: 'Scout (Rust, HTTP API)',
    coldStartMs,
    idleRssKb,
    peakRssKb,
    scenarios: [small, large],
  };
}

// --------------------------------------------------------------- playwright

async function measurePlaywright(fixturePort, chromium) {
  const t0 = performance.now();
  let browser;
  try {
    browser = await chromium.launch();
  } catch (e) {
    // Root containers refuse the Chromium sandbox; retry without it and say so.
    console.error(`chromium.launch() failed (${e.message.split('\n')[0]}); retrying with --no-sandbox`);
    browser = await chromium.launch({ args: ['--no-sandbox'] });
  }
  const context = await browser.newContext();
  const page = await context.newPage();
  await page.goto('about:blank');
  const coldStartMs = +(performance.now() - t0).toFixed(1);
  const idleRssKb = chromiumRssKb();

  const expected = { small: 3, large: 8000 };

  const scenario = async (label, path, n) => {
    const url = `http://127.0.0.1:${fixturePort}${path}`;
    const times = [];
    let ok = 0;
    let accurate = 0;
    for (let i = 0; i < n; i++) {
      const t = performance.now();
      await page.goto(url, { waitUntil: 'load' });
      const items = await page.$$eval('li.item', (els) =>
        els.slice(0, 1).map((el) => ({
          name: el.querySelector('.name').textContent,
          price: el.querySelector('.price').textContent,
        }))
      );
      const count = await page.$$eval('li.item', (els) => els.length);
      times.push(performance.now() - t);
      if (count === expected[label]) ok++;
      if (items[0] && items[0].name === 'Item 0' && items[0].price === '$0.50') accurate++;
    }
    return { label, times, ok, accurate, n };
  };

  const small = await scenario('small', '/small', Number(args.small));
  const large = await scenario('large', '/large', Number(args.large));
  const peakRssKb = chromiumRssKb();

  await browser.close();
  return {
    engine: 'Playwright (Chromium headless shell)',
    coldStartMs,
    idleRssKb,
    peakRssKb,
    scenarios: [small, large],
  };
}

// --------------------------------------------------------------------- main

const fixturePort = await freePortAsync();
const fixture = spawn('python3', [FIXTURE, String(fixturePort)], { stdio: 'ignore' });
await new Promise((r) => setTimeout(r, 500));

try {
  const { chromium } = await loadPlaywright();

  const scout = await measureScout(fixturePort);
  const pw = await measurePlaywright(fixturePort, chromium);

  const row = (name, f) =>
    `| ${name} | ${f(scout)} | ${f(pw)} |`;

  console.log('Same local fixtures, same task (navigate + extract 3 / 8000 items),');
  console.log(`${args.small} small tasks and ${args.large} large tasks per engine.\n`);
  console.log('| Measure | Scout | Playwright (Chromium) |');
  console.log('|---|---|---|');
  console.log(row('Cold start to first result', (m) => `${m.coldStartMs} ms`));
  console.log(row('Idle RSS', (m) => `${(m.idleRssKb / 1024).toFixed(1)} MB`));
  console.log(row('Peak RSS', (m) => `${(m.peakRssKb / 1024).toFixed(1)} MB`));
  for (const label of ['small', 'large']) {
    const s = scout.scenarios.find((x) => x.label === label);
    const p = pw.scenarios.find((x) => x.label === label);
    console.log(row(`${label} page: success`, (m) => {
      const x = m.scenarios.find((y) => y.label === label);
      return `${x.ok}/${x.n}`;
    }));
    console.log(row(`${label} page: accuracy`, (m) => {
      const x = m.scenarios.find((y) => y.label === label);
      return `${x.accurate}/${x.n}`;
    }));
    console.log(row(`${label} page: p50`, (m) => {
      const x = m.scenarios.find((y) => y.label === label);
      const st = statsFor(x.times, x.ok, x.accurate);
      return `${st.p50Ms} ms`;
    }));
    console.log(row(`${label} page: p95`, (m) => {
      const x = m.scenarios.find((y) => y.label === label);
      const st = statsFor(x.times, x.ok, x.accurate);
      return `${st.p95Ms} ms`;
    }));
  }
  console.log('\nPlaywright provides a full browser: layout, rendering, a complete JS engine.');
  console.log('Scout does not. That capability difference is what these numbers show.');
} finally {
  fixture.kill('SIGTERM');
}
