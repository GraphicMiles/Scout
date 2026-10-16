# Scout

The Scout runtime: a lightweight browser automation runtime with an authenticated
HTTP API, implemented in Rust.

This directory is **the product**. It began as a port of the Node reference
implementation and is now the only supported runtime. It exposes the same HTTP API,
the same SDK, the same error codes and the same test fixtures — verified against
the Node base rather than described as equivalent.

The Node implementation lives in `../reference/node-base/` and is kept in the tree
as the **contract reference**, not as a supported runtime: `conformance/` runs the
ported test suite against this binary, and every contract question in this file was
settled by running the Node service and reading its source, not by assumption.
Deliberately, the port is ahead of it — it has an embedded JavaScript engine and a
YouTube adapter verified against live pages, neither of which the Node base has.

## Build and run

```bash
cargo build --release                     # target/release/scout
cargo test                                # unit + in-process integration tests
SCOUT_API_KEYS=$(openssl rand -hex 24) ./target/release/scout
curl -s -H "authorization: Bearer $SCOUT_API_KEYS" http://127.0.0.1:8080/health
```

Node 22 is needed only for the JavaScript SDK and the conformance suite
(`sdk/` is the JS SDK, `conformance/` and `bench/` are JavaScript/Python
harnesses). The service itself has no runtime dependency on Node, Playwright,
Puppeteer or Chromium: `Cargo.toml` pulls `reqwest` (rustls), `scraper`/`html5ever`
and `rquickjs` (QuickJS).

Container: `Dockerfile` and `docker-compose.yml` are in this directory. **They were
written but not built or run** — the authoring sandbox has no Docker daemon. The
compose file sizes the cgroup limit above `SCOUT_MEM_HARD_MB` so the runtime's own
watchdog fires before the kernel OOM killer.

## Verification status

| Suite | Command | Result |
|---|---|---|
| Rust unit tests | `cargo test --lib` | **103 passed / 0 failed** |
| Rust in-process API tests | `cargo test --test api` | **22 passed / 0 failed** |
| Ported conformance suite (Node's tests, same fixtures) | `node --test conformance/conformance.test.mjs` | **47 passed / 0 failed** |
| JavaScript SDK against the live service | `SCOUT_URL=... node sdk/test-sdk.mjs` | **17 passed / 0 failed** |
| Benchmark | `python3 bench/bench.py` | see below |

The conformance suite starts the release binary on a free port, drives it through
the same fixtures as the Node tests (`conformance/fixture.mjs` is a byte-identical
port of `reference/node-base/test/helpers.js`) and asserts Node's status codes,
error codes and payload shapes. It covers auth and config refusal, unknown-field rejection, all ten
endpoints, redirects/gzip/JSON, malformed markup, sessions and cookie isolation,
the SSRF and allowed-domain policy, rate limiting, payload caps, task queue
behaviour (bounds, cancel, timeout, retries, memory admission), extraction
schemas, form interaction and shutdown.

## Measured benchmark (`bench/results/latest.json`)

100 tasks per scenario, 2 vCPUs, local fixtures, sequential client, with
`--release` and `lto`. Raw JSON: `bench/results/latest.json`.

| Measure | Rust port | Node base (same fixtures) |
|---|---|---|
| Cold start to healthy | **3.4 ms** | ~192 ms |
| Idle RSS | **4.8 MB** | ~64 MB |
| Peak RSS | 46.9 MB | ~117 MB |
| Small page (~1 KB), 100 tasks | 100 % success, 100 % accuracy, p50 0.7 ms, p95 0.8 ms | 100 % / 100 %, p50 4 ms |
| Large page (8000 items, 0.8 MB), 100 tasks | 100 % success, 100 % accuracy, p50 55.1 ms, p95 62.7 ms | 100 % / 100 %, p50 90 ms |

These numbers measure the runtime against local fixtures: no network latency, no
JavaScript, no hostile markup, and no real-site compatibility claim is implied.
Playwright/Chromium and Lightpanda were not benchmarked (no browsers in the
sandbox), so no comparison against them is claimed.

## Compatibility notes

**Capability status is reported, never assumed.** `GET /v1/strategies` returns each
strategy with its verification state: `generic-page` and `selectors` are
`verified-fixtures`, `youtube-channel-videos` is **`verified-live`** (checked
2026-10-16 against `https://www.youtube.com/@NASA/videos`, which served
`lockupViewModel` rather than the older `videoRenderer` shape), and
`facebook-jobs`, `facebook-marketplace` and `tiktok-profile` are `not_implemented`
and refused with `unsupported_capability` *before* any network or queue use. No
Facebook or TikTok compatibility is claimed.

**Deliberate differences from the Node base.** Each was measured, not assumed:

1. `POST /v1/navigate` requires `sessionId`, and a malformed id is a 400
   (`"sessionId" is malformed`) — matching `parseNavigate`. Earlier builds of this
   port accepted a bare `url` and created a temporary session; that divergence is
   gone.
2. A `<select multiple>` given more than one value returns `unsupported_capability`
   instead of silently recording only the first. The Node base supports it; the
   port's form state maps one key to one value, and quietly doing less would submit
   a choice the caller never made.
3. `scroll` returns `unsupported_capability` with an explanation (Node's own
   `interaction.js` throws the same code). Unknown *action types* are a 400
   `invalid_request`, as in Node's `parseAction` — the action list is closed.
4. `GET /v1/eval`-style scripting: the port exposes `POST /v1/eval` (used by the
   SDK's `eval()`) and the Node base has no equivalent endpoint. Additive only.
5. `interact` responses carry the Node fields (`ok`, `type`, `navigated`, `page`)
   plus `effect`/`warnings` for callers that want the port's own detail.

**JavaScript.** An embedded QuickJS engine (`rquickjs`) is part of the runtime, and
page scripts now run **during navigation**:

- Inline and external `<script>` elements execute in document order inside one
  context, so scripts share a global scope the way classic scripts do.
- External scripts are fetched through the same guarded client as the document,
  so the SSRF policy applies to them too.
- The DOM is mutable — `createElement`, `appendChild`, `innerHTML`,
  `setAttribute`, `classList` and friends all work. After the scripts finish,
  the mutated tree is serialised and re-parsed, so extraction sees content the
  scripts built.
- `setTimeout`/`setInterval` run on a virtual clock: callbacks are queued and
  drained synchronously after the scripts, so deferred writes are captured
  without sleeping in real time.
- `fetch`, `XMLHttpRequest`, `Headers`, `Response` and `URLSearchParams` are
  implemented over a host bridge that applies the *same* guarded client, cookie
  jar, SSRF policy and byte caps as navigation. Requests are synchronous — there
  is no event loop to suspend on — so the promise returned is already settled;
  `await` and `.then` chains run because the host drains pending jobs after each
  script, exactly as a browser drains its microtask queue. A scripted request to
  a blocked address is refused (`fetch` rejects with a `TypeError`).
- Bounded by a wall-clock budget, a memory cap, a stack cap, a script-count cap,
  a byte budget for fetched scripts, a request budget (`SCOUT_JS_MAX_FETCHES`,
  default 8) and a promise-job budget (`SCOUT_JS_JOB_BUDGET`, default 5 000).
  A runaway script is interrupted.

Still missing, and reported rather than papered over: no layout, no CSS cascade,
no `IndexedDB`, no Web Workers, no ES module semantics. A page whose content
depends on those still reports `js_required_likely` (or `js_shell_not_rendered`,
when the document arrives as a shell with no links) rather than returning
fabricated data.

Two differences from a browser worth knowing: CORS is **not** enforced — the
request has already been through the SSRF policy and the session's own jar, so
rejecting a cross-origin response would cost compatibility without adding
safety, though `credentials` is still honoured; and response bodies are text, so
an octet stream is decoded as UTF-8 and is lossy rather than faithful.

Measured cost of the JS engine: idle RSS **5.0 MB**, peak **6.5 MB** across 15
navigations that each ran a page script and an authenticated in-page `fetch`,
binary **5.5 MB** (`SCOUT_JS_ENABLED=0` restores the old read-only behaviour;
`SCOUT_JS_NETWORK=0` keeps the engine and refuses only the network).

`POST /v1/eval` runs the script as a **function body**, so a value is returned only
by an explicit `return`; a trailing expression's value is discarded.

```js
{ "script": "1 + 1" }                      // value: null   + warning `no_return_value`
{ "script": "return 1 + 1;" }              // value: 2
{ "script": "return document.title;" }     // value: "Hacker News"
```

When a script yields no value and contains no `return`, the response carries
`warnings: [{ code: "no_return_value", … }]` so the caller is told why, instead of
debugging a silent `null`. The check is deliberately permissive: if `return` appears
anywhere in the source, no warning is raised, because a spurious warning would be
worse than a missed one. A runaway script is an explicit `compatibility_error`
("JavaScript exceeded its time limit"), never a `null`.

**Signed-in sessions.** A session can be authenticated with credentials the
operator already owns, and then behaves like that user:

- `auth.login` (tool) loads a sign-in page, discovers the password field and the
  field above it, fills them, submits, and reports the *evidence* — which cookies
  appeared, whether the sign-in form is still on the page, and what the landing
  page says — rather than guessing that it worked.
- Cookies are read before and written after every hop of a request, so a cookie
  issued by a login `POST` is presented on the redirect that follows it. That is
  what makes signing in possible at all.
- `POST /v1/request` (and the `request` tool) makes a request with the session
  jar and the SSRF policy applied and returns the response unparsed, which is the
  primitive for authenticated reads and writes on a JSON API.
- `session.export` / `session.import` move a signed-in session between sessions,
  so it can be resumed without replaying the login.
- Navigation carries the header set a browser sends — `Sec-Fetch-*`, client hints
  derived from the configured UA, and `Referer`/`Origin` under
  `strict-origin-when-cross-origin`. `SCOUT_BROWSER_HEADERS=0` turns that off and
  `SCOUT_USER_AGENT` declares something else, because transparency about what is
  on the wire matters more than getting in.

None of this defeats a CAPTCHA, a second factor, a rate limit or an access
control. A login a human could not complete with these credentials fails here
too, and the failure is reported with the status and the evidence instead of
being retried in the hope it works the second time.

**Other honest limits.** The YouTube adapter reads the first page of results only
(continuation requests are not made); price parsing handles US-style separators
only; the cookie jar screens a curated public-suffix list rather than the full PSL;
`Dockerfile`/`docker-compose.yml` are unverified; Docker resource limits are
enforced by the runtime's own budget checks plus the compose cgroup settings, and
the compose file has not been exercised.

## Layout

```
src/server.rs      HTTP surface: routing, auth, limits, error mapping
src/scout.rs       orchestration: sessions, strategies, tasks, retries
src/worker.rs      DOM thread affinity (scraper::Html is !Send) and the command loop
src/interaction.rs click/fill/select/check/uncheck/press/submit + form submission
src/extract.rs     typed field extraction (string, url, number, price, date)
src/js.rs          embedded QuickJS: DOM shim, fetch/XHR bridge, limits, console
src/auth.rs        signing in with supplied credentials, and the evidence it returns
src/adapters/       strategy registry (generic-page, selectors, youtube, ...)
src/tasks.rs       bounded queue, concurrency permits, admission, watchdog
src/resources.rs   RSS sampling, soft/hard memory budgets
src/security.rs    URL policy, DNS re-checks, guarded fetch, browser headers
conformance/       port of the Node test suite (fixture.mjs + conformance.test.mjs)
sdk/               JavaScript SDK and its integration test
bench/             benchmark client, fixtures and recorded results
```
