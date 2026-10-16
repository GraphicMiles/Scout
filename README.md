# Scout

Scout is a lightweight browser automation runtime with an authenticated HTTP API, written in Rust. It fetches pages, parses the DOM, runs page JavaScript in a sandboxed engine, fills forms, and extracts structured data. It is not a Chromium replacement: it is one small binary that idles around 5 MB of memory and starts in milliseconds.

## Install

From npm. The postinstall step downloads the release binary for your platform; no compiler needed.

```bash
npm install @graphicmiles/scout
```

Or straight from the repository, which runs the same postinstall:

```bash
npm install github:GraphicMiles/Scout
```

Then drive it like a headless browser:

```js
import { launch } from '@graphicmiles/scout/driver';

const scout = await launch();          // starts the binary on a local port
const page = await scout.newPage();
await page.goto('https://example.com/');
console.log(await page.title());
await scout.close();
```

From source, or for platforms without a release binary:

```bash
cd scout
cargo build --release                 # requires Rust 1.88 or newer
SCOUT_BIN=./target/release/scout npm install github:GraphicMiles/Scout
```

For Rust programs:

```bash
cargo add --git https://github.com/GraphicMiles/Scout scout
```

Docker:

```bash
cd scout
docker build -t scout .
docker run --memory=512m --cpus=0.5 -e SCOUT_API_KEYS=your-key -p 8080:8080 scout
```

## Quick start

```bash
cd scout
cargo build --release
SCOUT_API_KEYS=$(openssl rand -hex 24) ./target/release/scout
curl -s -H "authorization: Bearer $SCOUT_API_KEYS" http://127.0.0.1:8080/health
```

## What it does

- Navigates pages, follows redirects, handles gzip, and keeps cookies per session
- Runs page JavaScript during navigation, including fetch and XMLHttpRequest
- Extracts typed fields: text, URLs, numbers, prices, dates
- Clicks, fills, selects, and submits forms, including multipart forms and file uploads
- Stages files on upload inputs and sends them as multipart/form-data, like a browser
- Keeps localStorage and sessionStorage per session, across navigations
- Scrolls pages and delivers key presses to their scripts, so lazy-loading and search-as-you-type pages work
- Runs ES modules: relative and absolute URL imports are fetched, chained, and cached per page load
- Signs in with credentials you supply and reports the evidence
- Extracts videos from YouTube channels, verified against live pages, and follows continuation pages up to the limit you set
- Queues background tasks with retries, timeouts, and a memory watchdog

## What it does not do

There is no layout, no CSS cascade, no Web Workers, no IndexedDB. Page event listeners live only inside one script pass, so a scroll or key press re-runs the page's own scripts before dispatching; scripts that append unconditionally on load may duplicate content. ES module imports resolve relative and absolute URLs only: there is no package registry, so `import 'react'` fails with a script error rather than a guess. The Facebook and TikTok strategies are not implemented and return `unsupported_capability` rather than guessing. Every strategy reports its verification state at `GET /v1/strategies`.

## API

All `/v1` routes require a bearer token from `SCOUT_API_KEYS`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | Liveness check |
| GET | `/metrics` | Prometheus metrics |
| GET | `/v1/strategies` | Strategies and verification states |
| GET | `/v1/tools` | Tool manifest |
| POST | `/v1/tools/call` | Call a tool |
| POST | `/v1/sessions` | Create a session |
| DELETE | `/v1/sessions/:id` | Destroy a session |
| POST | `/v1/navigate` | Load a page into a session |
| POST | `/v1/request` | Raw HTTP request with session cookies |
| POST | `/v1/extract` | Extract fields from a page |
| POST | `/v1/interact` | Click, fill, select, submit, scroll, press a key, or upload a file |
| POST | `/v1/eval` | Run JavaScript against a page |
| POST | `/v1/tasks` | Submit a task |
| GET | `/v1/tasks/:id` | Task status and result |
| POST | `/v1/tasks/:id/cancel` | Cancel a task |

Example:

```bash
# create a session
curl -s -H "authorization: Bearer $SCOUT_API_KEYS" -H "content-type: application/json" \
  -d '{}' http://127.0.0.1:8080/v1/sessions

# load a page into it
curl -s -H "authorization: Bearer $SCOUT_API_KEYS" -H "content-type: application/json" \
  -d '{"sessionId": "ses_...", "url": "https://example.com"}' \
  http://127.0.0.1:8080/v1/navigate
```

## JavaScript SDK

`scout/sdk/` holds a JavaScript and TypeScript SDK for Node 18+. It is also the package's main entry point: `import { ScoutClient } from '@graphicmiles/scout'`.

```js
import { ScoutClient } from './scout/sdk/index.js';

const scout = new ScoutClient({ baseUrl: 'http://127.0.0.1:8080', apiKey: key });
const session = await scout.createSession();
await scout.navigate({ sessionId: session.id, url: 'https://example.com/' });
const page = await scout.eval({ sessionId: session.id, script: 'return document.title;' });
await scout.destroySession(session.id);
```

## Tests

```bash
cd scout
cargo test                                    # unit and API tests
cargo build --release
node --test conformance/conformance.test.mjs  # conformance suite against the binary
SCOUT_URL=http://127.0.0.1:8080 SCOUT_API_KEY=key node sdk/test-sdk.mjs
cd .. && SCOUT_BIN=scout/target/release/scout node driver/test-driver.mjs  # driver against a live page
cd scout && python3 bench/bench.py            # benchmark
node bench/compare-playwright.mjs             # vs Playwright/Chromium (needs: npm i playwright && npx playwright install chromium)
```

## Configuration

Defaults suit local use. The settings most worth knowing:

| Variable | Default | Controls |
|---|---|---|
| `SCOUT_API_KEYS` | required | Bearer tokens, comma separated |
| `PORT` | 8080 | Listen port |
| `SCOUT_ALLOWED_DOMAINS` | unset | Restrict navigation to these domains |
| `SCOUT_MEM_HARD_MB` | 400 | Hard memory budget |
| `SCOUT_JS_ENABLED` | 1 | Run page scripts during navigation |
| `SCOUT_JS_NETWORK` | 1 | Let page scripts make requests |
| `SCOUT_USER_AGENT` | Chrome desktop | User agent sent to sites |

## Layout

```
scout/src/          Runtime source: server, navigation, JS engine, extraction, tasks
scout/tests/        In-process API tests
scout/conformance/  Conformance suite
scout/sdk/          JavaScript SDK and its integration test
driver/             Node driver: launch the binary, drive pages
postinstall.mjs     Downloads the release binary on npm install
scout/bench/        Benchmark harness
scout/fixtures/     Test fixtures
scout/Dockerfile    Container build
```
