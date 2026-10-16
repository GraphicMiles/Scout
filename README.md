# Scout

Scout is a lightweight browser automation runtime with an authenticated HTTP API, written in Rust. It fetches pages, parses the DOM, runs page JavaScript in a sandboxed engine, fills forms, and extracts structured data. It is not a Chromium replacement: it is one small binary that idles around 5 MB of memory and starts in milliseconds.

## Quick start

Requires Rust 1.88 or newer.

```bash
cd scout
cargo build --release
SCOUT_API_KEYS=$(openssl rand -hex 24) ./target/release/scout
curl -s -H "authorization: Bearer $SCOUT_API_KEYS" http://127.0.0.1:8080/health
```

Docker:

```bash
cd scout
docker build -t scout .
docker run --memory=512m --cpus=0.5 -e SCOUT_API_KEYS=your-key -p 8080:8080 scout
```

## What it does

- Navigates pages, follows redirects, handles gzip, and keeps cookies per session
- Runs page JavaScript during navigation, including fetch and XMLHttpRequest
- Extracts typed fields: text, URLs, numbers, prices, dates
- Clicks, fills, selects, and submits forms
- Signs in with credentials you supply and reports the evidence
- Extracts videos from YouTube channels, verified against live pages
- Queues background tasks with retries, timeouts, and a memory watchdog

## What it does not do

There is no layout, no CSS cascade, no Web Workers, no IndexedDB. The Facebook and TikTok strategies are not implemented and return `unsupported_capability` rather than guessing. Every strategy reports its verification state at `GET /v1/strategies`.

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
| POST | `/v1/interact` | Click, fill, or submit |
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

`sdk/` holds a JavaScript and TypeScript SDK for Node 22+.

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
python3 bench/bench.py                        # benchmark
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
scout/bench/        Benchmark harness
scout/fixtures/     Test fixtures
scout/Dockerfile    Container build
```
