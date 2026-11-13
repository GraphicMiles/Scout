/**
 * Scout driver: launch the Scout binary and drive it like a headless browser.
 *
 *   import { launch } from '@graphicmiles/scout/driver';
 *   const scout = await launch();
 *   const page = await scout.newPage();
 *   await page.goto('https://example.com/');
 *   console.log(await page.title());
 *   await scout.close();
 *
 * Binary resolution order:
 *   1. `executablePath` option
 *   2. `SCOUT_BIN` environment variable
 *   3. the copy postinstall downloaded into this package
 *   4. `scout` on PATH
 */

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { ScoutClient } from '../scout/sdk/index.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));

/** Asset name the release workflow publishes for each platform. */
const PLATFORM_ASSETS = {
  'linux-x64': 'scout-linux-x64',
  'linux-arm64': 'scout-linux-arm64',
  'darwin-x64': 'scout-darwin-x64',
  'darwin-arm64': 'scout-darwin-arm64',
};

export function platformAsset() {
  const key = `${process.platform}-${process.arch}`;
  return PLATFORM_ASSETS[key] ?? null;
}

/** Where postinstall put (or will put) the binary for this platform. */
export function bundledBinaryPath() {
  const asset = platformAsset();
  if (!asset) return null;
  return path.join(HERE, '..', 'bin', asset);
}

async function resolveBinary(executablePath) {
  if (executablePath) return executablePath;
  if (process.env.SCOUT_BIN) return process.env.SCOUT_BIN;
  const bundled = bundledBinaryPath();
  if (bundled && existsSync(bundled)) return bundled;
  return 'scout'; // PATH
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

/**
 * Start a Scout server and return a handle to it.
 *
 * Options:
 *   executablePath  path to the scout binary
 *   apiKey          key the server requires (generated when omitted)
 *   port            port to listen on (a free one when omitted)
 *   env             extra environment variables for the child
 */
export async function launch(opts = {}) {
  const executablePath = await resolveBinary(opts.executablePath);
  const apiKey = opts.apiKey ?? crypto.randomBytes(24).toString('hex');
  const port = opts.port ?? (await freePort());
  const host = opts.host ?? '127.0.0.1';
  const startupTimeoutMs = opts.startupTimeoutMs ?? 15_000;

  const child = spawn(executablePath, [], {
    env: {
      ...process.env,
      ...opts.env,
      HOST: host,
      PORT: String(port),
      SCOUT_API_KEYS: apiKey,
      SCOUT_LOG_LEVEL: opts.logLevel ?? 'error',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  let exited = false;
  let spawnError = null;
  let earlyExit = '';
  let logBytes = 0;
  const MAX_STARTUP_LOG_BYTES = 32 * 1024;
  const logs = [];
  const onData = (d) => {
    if (logBytes >= MAX_STARTUP_LOG_BYTES) return;
    const chunk = String(d);
    const remaining = MAX_STARTUP_LOG_BYTES - logBytes;
    logs.push(chunk.slice(0, remaining));
    logBytes += Math.min(chunk.length, remaining);
  };
  child.stdout?.on('data', onData);
  child.stderr?.on('data', onData);
  // Drain pipes without retaining output after startup completes so child stdio never blocks or leaks memory.
  const noopDrain = () => {};
  child.on('error', (err) => {
    spawnError = err;
    exited = true;
  });
  child.on('exit', (code, signal) => {
    exited = true;
    earlyExit = logs.join('').trim();
    if (code) earlyExit += `\n(exit code ${code})`;
    else if (signal) earlyExit += `\n(signal ${signal})`;
  });

  const baseUrl = `http://${host === '0.0.0.0' ? '127.0.0.1' : host}:${port}`;
  const client = new ScoutClient({ baseUrl, apiKey, timeoutMs: opts.timeoutMs ?? 60_000 });

  // Wait for the server to answer. A fast exit or spawn error means the binary never came up.
  const deadline = Date.now() + startupTimeoutMs;
  for (;;) {
    if (spawnError) {
      throw new Error(`failed to spawn scout (${executablePath}): ${spawnError.message}`);
    }
    if (exited) {
      throw new Error(`scout exited during startup: ${earlyExit || 'no output'}`);
    }
    try {
      await client.health();
      break;
    } catch {
      if (Date.now() > deadline) {
        try { child.kill('SIGKILL'); } catch { /* already gone */ }
        throw new Error(`scout did not answer at ${baseUrl} within ${startupTimeoutMs}ms`);
      }
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  // Stop buffering logs in memory once startup succeeds while keeping pipes drained.
  child.stdout?.off('data', onData);
  child.stderr?.off('data', onData);
  child.stdout?.on('data', noopDrain);
  child.stderr?.on('data', noopDrain);
  logs.length = 0;

  return {
    client,
    pid: child.pid,
    baseUrl,

    /** A fresh page: its own Scout session. */
    async newPage(sessionOpts = {}) {
      const session = await client.createSession(sessionOpts);
      return makePage(client, session.id);
    },

    /** Stop the server. Safe to call twice. */
    async close() {
      if (exited) return;
      try { child.kill('SIGTERM'); } catch { return; }
      await new Promise((resolve) => {
        const timer = setTimeout(() => {
          if (!exited) {
            try { child.kill('SIGKILL'); } catch { /* already gone */ }
          }
          resolve();
        }, 3000);
        timer.unref?.();
        child.once('exit', () => {
          clearTimeout(timer);
          resolve();
        });
      });
    },
  };
}

function makePage(client, sessionId) {
  const page = {
    id: sessionId,

    /** Load a URL into the page. Returns the navigation summary. */
    goto(url, opts = {}) {
      return client.navigate({ sessionId, url, timeoutMs: opts.timeoutMs });
    },

    /** The page's current URL. */
    async url() {
      const r = await client.eval({ sessionId, script: 'return location.href;' });
      return r.value;
    },

    /** The page's title. */
    async title() {
      const r = await client.eval({ sessionId, script: 'return document.title;' });
      return r.value;
    },

    /** Visible text of the whole page, or of one selector. */
    async text(selector) {
      const r = await client.callTool('page.text', { sessionId, selector });
      return r.text ?? null;
    },

    /** The page's HTML. */
    async content() {
      const r = await client.eval({
        sessionId,
        script: 'return document.documentElement.outerHTML;',
      });
      return r.value;
    },

    /**
     * Every element's box, from Scout's own layout engine — the Playwright-shaped
     * way to ask "where is this on the page" without a browser.
     */
    layout(opts = {}) {
      return client.layout(sessionId, opts);
    },

    /** A PNG of the page, as a Buffer. `opts: { width, height, fullPage }` */
    screenshot(opts = {}) {
      return client.screenshot(sessionId, opts);
    },

    click(selector) {
      return client.interact({ sessionId, action: { type: 'click', selector } });
    },

    fill(selector, value) {
      return client.interact({ sessionId, action: { type: 'fill', selector, value } });
    },

    select(selector, value) {
      return client.interact({ sessionId, action: { type: 'select', selector, value } });
    },

    press(selector, key) {
      return client.interact({ sessionId, action: { type: 'press', selector, key } });
    },

    scroll(px) {
      const action = { type: 'scroll' };
      if (px !== undefined && px !== null) action.value = String(px);
      return client.interact({ sessionId, action });
    },

    /** Stage a file on an input[type=file]; submit sends it as multipart. */
    upload(selector, filename, contentBase64) {
      return client.interact({
        sessionId,
        action: { type: 'upload', selector, value: filename, contentBase64 },
      });
    },

    submit(selector = 'form') {
      return client.interact({ sessionId, action: { type: 'submit', selector } });
    },

    /** Run JavaScript in the page and return its value. */
    eval(script) {
      return client.eval({ sessionId, script });
    },

    /** Run an extraction strategy against the loaded page. */
    extract(params = {}) {
      return client.extract({ sessionId, ...params });
    },

    /** Destroy this page's session. */
    close() {
      return client.destroySession(sessionId).catch(() => {});
    },
  };
  return page;
}
