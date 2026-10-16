/**
 * Download the Scout binary for this platform from GitHub releases.
 *
 * Skipped (with a note, never an error) when:
 *   - SCOUT_BIN is set: the user supplies their own binary
 *   - SCOUT_SKIP_DOWNLOAD is set: no download wanted
 *   - the platform has no release asset: use SCOUT_BIN or build from source
 *
 * Downloaded to node_modules/@graphicmiles/scout/bin/, next to this file.
 */

import { createWriteStream, existsSync, mkdirSync, chmodSync, renameSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { pipeline } from 'node:stream/promises';
import { Readable } from 'node:stream';

const HERE = dirname(fileURLToPath(import.meta.url));

const PLATFORM_ASSETS = {
  'linux-x64': 'scout-linux-x64',
  'linux-arm64': 'scout-linux-arm64',
  'darwin-x64': 'scout-darwin-x64',
  'darwin-arm64': 'scout-darwin-arm64',
};

const REPO = 'GraphicMiles/Scout';

function log(msg) {
  console.log(`[scout] ${msg}`);
}

async function main() {
  if (process.env.SCOUT_BIN) {
    log(`SCOUT_BIN is set (${process.env.SCOUT_BIN}); skipping download.`);
    return;
  }
  if (process.env.SCOUT_SKIP_DOWNLOAD || process.env.npm_config_scout_skip_download) {
    log('Download skipped by request.');
    return;
  }

  const key = `${process.platform}-${process.arch}`;
  const asset = PLATFORM_ASSETS[key];
  if (!asset) {
    log(
      `No release binary for ${key}. Build one from source:` +
        `  git clone https://github.com/${REPO} && cd scout && cargo build --release` +
        `, then point SCOUT_BIN at it.`
    );
    return;
  }

  const binDir = join(HERE, 'bin');
  const dest = join(binDir, asset);
  if (existsSync(dest)) {
    log(`Binary already present at ${dest}.`);
    return;
  }

  // Allow a pinned version: SCOUT_VERSION=v0.1.0
  const tag = process.env.SCOUT_VERSION ?? 'latest';
  const url =
    tag === 'latest'
      ? `https://github.com/${REPO}/releases/latest/download/${asset}`
      : `https://github.com/${REPO}/releases/download/${tag}/${asset}`;

  mkdirSync(binDir, { recursive: true });
  log(`Downloading ${url} ...`);
  const tmp = `${dest}.part`;
  try {
    const res = await fetch(url, { redirect: 'follow' });
    if (!res.ok || !res.body) {
      throw new Error(`HTTP ${res.status} for ${url}`);
    }
    await pipeline(Readable.fromWeb(res.body), createWriteStream(tmp));
    chmodSync(tmp, 0o755);
    renameSync(tmp, dest);
    log(`Installed ${dest}`);
  } catch (err) {
    // A failed download must not break npm install: the package still works
    // with a binary from SCOUT_BIN or PATH.
    console.warn(
      `[scout] Could not download the binary (${err.message}).` +
        ` Set SCOUT_BIN to a local scout binary and reinstall.`
    );
  }
}

main();
