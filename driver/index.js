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
import { chmod, copyFile, rename } from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
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

  const child = spawn(executablePath, [], {
    env: {
      ...process.env,
      ...opts.env,
      PORT: String(port),
      SCOUT_API_KEYS: apiKey,
      SCOUT_LOG_LEVEL: opts.logLevel ?? 'error',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  let exited = false;
  let earlyExit = '';
  const logs = [];
  child.stdout.on('data', (d) => logs.push(String(d)));
  child.stderr.on('data', (d) => logs.push(String(d)));
  child.on('exit', (code) => {
    exited = true;
    earlyExit = logs.join('').trim();
    if (code) earlyExit += `\n(exit code ${code})`;
  });

  const baseUrl = `http://127.0.0.1:${port}`;
  const client = new ScoutClient({ baseUrl, apiKey, timeoutMs: opts.timeoutMs ?? 60_000 });

  // Wait for the server to answer. A fast exit means the binary never came up.
  const deadline = Date.now() + (opts.startupTimeoutMs ?? 15_000);
  for (;;) {
    if (exited) {
      throw new Error(`scout exited during startup: ${earlyExit || 'no output'}`);
    }
    try {
      await client.health();
      break;
    } catch {
      if (Date.now() > deadline) {
        child.kill('SIGKILL');
        throw new Error(`scout did not answer at ${baseUrl} within ${opts.startupTimeoutMs}ms`);
      }
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  return {
    client,
    pid: child.pid,
    baseUrl,

    /** A fresh page: its own Scout session. */
    async newPage() {
      const session = await client.createSession();
      return makePage(client, session.id);
    },

    /** Stop the server. Safe to call twice. */
    async close() {
      if (exited) return;
      child.kill('SIGTERM');
      await new Promise((resolve) => {
        child.once('exit', resolve);
        setTimeout(() => {
          if (!exited) child.kill('SIGKILL');
          resolve();
        }, 3000).unref?.();
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
      return client.interact({ sessionId, action: { type: 'scroll', value: px } });
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
