# Scout

Lightweight browser automation runtime with an authenticated HTTP API and a JS/TS SDK. This is a **working base**, not a Chromium replacement: it fetches documents over HTTP, parses and queries the DOM, fills and submits forms, and extracts structured data. It does **not** execute JavaScript yet.

## Status at a glance

| Area | State |
|---|---|
| Fetch, redirects, cookies, sessions, SSRF policy | Working, tested |
| HTML parser, CSS selectors, extraction (text, url, number, price, date) | Working, tested (own implementation, see below) |
| Interactions: click link, fill, select, check, Enter, submit (urlencoded forms) | Working, tested |
| Task queue: bounded, cancel, timeout, retries (navigation only), watchdog | Working, tested |
| HTTP API (all required endpoints), auth, rate limits, body limits, metrics | Working, tested |
| SDK | Working, tested against the real server |
| JavaScript execution / browser APIs | **Not implemented**. Pages that need JS return a `js_required_likely` warning or a `compatibility_error` |
| YouTube `youtube-channel-videos` | Verified against a **synthetic fixture only**; never run against real youtube.com |
| Facebook, TikTok | **Not implemented.** Strategies exist and return `unsupported_capability` before any request is made |
| Scroll, keyboard other than Enter, file upload, multipart forms | Return `unsupported_capability` |
| Playwright / Lightpanda benchmark comparison | **Not run** (see Benchmarks) |

## Run

```bash
SCOUT_API_KEYS=$(openssl rand -hex 24) node src/index.js        # needs Node 22+, zero npm dependencies
SCOUT_DEV_MODE=1 node src/index.js                              # local only: auth off when no keys are set
docker build -t scout . && docker run --memory=512m --cpus=0.1 -e SCOUT_API_KEYS=... -p 8080:8080 scout
npm test            # 45 tests
npm run bench       # Linux only
```

## SDK

```js
import { ScoutClient } from './sdk/index.js';
const scout = new ScoutClient({ baseUrl: process.env.SCOUT_URL, apiKey: process.env.SCOUT_API_KEY });

const result = await scout.extract({ url: 'https://www.youtube.com/@CHANNEL/videos', strategy: 'youtube-channel-videos', limit: 10 });
console.log(result.data, result.warnings);

// Generic extraction
await scout.extract({ url, schema: { item: 'li.item', fields: { name: '.name', price: { selector: '.price', type: 'price' }, link: { selector: 'a', type: 'url' } } } });

// Session + interaction
await scout.withSession(async (sessionId) => {
  await scout.navigate({ sessionId, url });
  await scout.interact({ sessionId, action: { type: 'fill', selector: 'input[name=q]', value: 'term' } });
  await scout.interact({ sessionId, action: { type: 'press', selector: 'input[name=q]', key: 'Enter' } });
  return scout.extract({ sessionId, strategy: 'generic-page' });
});

// Background task (navigation steps retry on 5xx/timeouts; interactions never auto-retry)
const task = await scout.runTask({ steps: [{ op: 'navigate', url }, { op: 'extract', schema }] });
```

## API

`POST /v1/sessions` · `DELETE /v1/sessions/:id` (204) · `POST /v1/navigate` · `POST /v1/extract` · `POST /v1/interact` · `POST /v1/tasks` (202) · `GET /v1/tasks/:id` · `POST /v1/tasks/:id/cancel` · `GET /v1/strategies` · `GET /health` (public) · `GET /metrics` (Prometheus; `SCOUT_METRICS_TOKEN`, else an API key).

Auth: `Authorization: Bearer <key>` or `x-api-key`. Errors always look like `{"error":{"code","message","retryable","details?"}}`. Unknown request fields are rejected. Every response includes `x-request-id`.
Codes: 400 `invalid_request`, 401 `unauthorized`, 403 `blocked_destination`, 404 `not_found`, 409 `conflict`/`cancelled`, 413 `payload_too_large`, 415 `unsupported_content`, 422 `compatibility_error`/`element_not_found`, 429 `rate_limited`, 501 `unsupported_capability`, 502 `navigation_failed`/`response_too_large`, 503 `queue_full`/`overloaded`/`too_many_sessions`/`resource_limit`/`shutting_down`, 504 `navigation_timeout`/`task_timeout`.

## Configuration (env)

`SCOUT_API_KEYS` (required, 16+ chars each, comma-separated) · `SCOUT_METRICS_TOKEN` · `SCOUT_CONCURRENCY` (1) · `SCOUT_MAX_QUEUED` (16) · `SCOUT_MAX_SESSIONS` (8) · `SCOUT_NAV_TIMEOUT_MS` (15000) · `SCOUT_TASK_TIMEOUT_MS` (60000) · `SCOUT_MAX_RESPONSE_BYTES` (2 MB) · `SCOUT_MAX_DOM_NODES` (150000) · `SCOUT_RATE_LIMIT_PER_MIN` (120 per key) · `SCOUT_MEM_SOFT_MB` (300, reject new work) · `SCOUT_MEM_HARD_MB` (400, abort running task) · `SCOUT_ALLOWED_DOMAINS` · `SCOUT_PRIVATE_ALLOWLIST` (explicit opt-in for internal hosts) · `SCOUT_LOG_LEVEL`.

## Security model

- Every URL and every redirect hop is validated: http/https only, no embedded credentials, ports 80/443 only, no localhost/internal suffixes/single-label hosts, no private, loopback, link-local, metadata, CGNAT, multicast, mapped or NAT64 ranges, decimal/hex IP forms normalized. Resolved DNS addresses are checked again at connect time (rebinding).
- Only textual document types are downloaded; images, fonts and media are refused by content type. No subresources are ever fetched.
- Sessions and tasks are bound to the creating API key; other keys get `not_found`. Cookie jars are per session, scoped by domain/path/Secure, and cleared on destroy. Typed form values live in memory only and are dropped on navigation. Logs redact auth, cookie, password and token fields and never contain request bodies.
- Keys compared via SHA-256 + `timingSafeEqual`; failed-auth attempts are throttled per address; per-key rate limits; 64 KB body limit.
- Scout identifies itself honestly (`Scout/0.1`). It does not forge consent, solve CAPTCHAs, rotate identities or evade anti-bot systems; 401/403/429 and consent interstitials surface as errors.

## Known gaps (plain observations, for the implementing agent)

- No JavaScript runtime. `src/navigation.js` only emits a heuristic `js_required_likely` warning.
- `src/html.js` is a tolerant tokenizer/tree builder, not an HTML5-compliant parser (no foster parenting or adoption agency; `<td>` inside unclosed inline elements nests incorrectly).
- `src/cookies.js` has no public-suffix list; it only rejects bare-TLD `Domain=` values. Cookies are session-scoped, so impact is limited to one session.
- Price parsing handles US-style separators only.
- Sessions on one key share a global session cap (`SCOUT_MAX_SESSIONS`), not a per-key cap.
- YouTube: first page only (about 30 videos); continuation requests are not made. Unverified against the live site, which may serve consent or bot-check pages to this client.
- Docker image and compose file were written but not built or run (no Docker in the authoring sandbox).
- Heap does not shrink after large pages: RSS stayed ~113 MB after the large-page scenario.

## Benchmarks (measured here: Node 22.22, Linux, 1 vCPU, not throttled to 0.1)

Scout child process with `--max-old-space-size=192 --expose-gc`, sequential tasks against local fixtures via the SDK. Raw output: `bench/results/latest.json`.

| Measure | Result | Target |
|---|---|---|
| Cold start to healthy | ~190 ms | none |
| Idle RSS | **~64 MB** | <50 MB: **not met** (bare Node baseline is ~63 MB) |
| Peak RSS, 3-item page, 100 tasks | ~72 MB | <200 MB: met |
| Peak RSS, 8000-item ~0.8 MB page, 100 tasks | ~117 MB | <200 MB: met |
| Small page: success / accuracy / p50 / p95 / CPU per task | 100% / 100% / 4 ms / 12 ms / 2.3 ms | none |
| Large page: success / accuracy / p50 / p95 / CPU per task | 100% / 100% / 90 ms / 133 ms / 92 ms | none |

At `--cpus=0.1`, CPU-bound work takes roughly 10x its CPU time in wall time (about 1 s for the large page); this is an estimate and has not been measured under a real 0.1 CPU cap. Local fixtures have no network latency, JavaScript, or hostile markup, so these numbers say nothing about real-site compatibility. Playwright/Chromium and Lightpanda were not benchmarked: the authoring sandbox had no network or browsers. Add them as additional runners in `bench/` using the same fixtures.

## Next steps, in order

1. Run the YouTube strategy against real channels from the target deployment; record what breaks.
2. Rust port of the hot path (idle memory is bounded by the Node runtime). Candidate crates to evaluate: `html5ever`/`scraper` or `lol_html` (parsing), `reqwest`/`hyper` + `cookie_store` (HTTP), `url`, `rquickjs` (QuickJS) vs `boa` (JS), `axum`. Keep the HTTP contract, SDK, tests and fixtures unchanged and use them as the conformance suite.
3. Add an embedded JS runtime and the minimum DOM/`fetch`/`XMLHttpRequest` surface for pages that need it, driven by measured failures.
4. Facebook/TikTok adapters only after real-page verification, within access rules; optional separate browser fallback for incompatible sites.
