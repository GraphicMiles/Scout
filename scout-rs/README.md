# Scout (Rust)

The Scout runtime, ported from the Node reference implementation to Rust. It is the
same service: the same HTTP API, the same SDK, the same error codes and the same
test fixtures — verified against the Node base rather than described as equivalent.

The Node base in `../scout/` is the reference for the contract. It is kept in the
tree on purpose: `conformance/` runs the ported test suite against this binary, and
every contract question in this file was settled by running the Node service and
reading its source, not by assumption.

## Build and run

```bash
cargo build --release                     # target/release/scout
cargo test                                # unit + in-process integration tests
SCOUT_API_KEYS=$(openssl rand -hex 24) ./target/release/scout
curl -s -H "authorization: Bearer $SCOUT_API_KEYS" http://127.0.0.1:8080/health
```

Node 22 is needed only for the JavaScript SDK and the conformance suite
(`scout-rs/sdk/` is the JS SDK, `conformance/` and `bench/` are JavaScript/Python
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
port of `scout/test/helpers.js`) and asserts Node's status codes, error codes and
payload shapes. It covers auth and config refusal, unknown-field rejection, all ten
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

**JavaScript.** An embedded QuickJS engine (`rquickjs`) is part of the runtime: the
`eval` path runs real scripts against a read-only snapshot of the parsed document
with a memory cap, stack cap and interrupt-based timeout, and captures `console`
output. Page scripts are **not** executed during navigation yet — no event loop, no
in-page `fetch`/`XMLHttpRequest`, no document mutation. A page whose content depends
on those still reports `js_required_likely` rather than returning fabricated data.

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
src/js.rs          embedded QuickJS: DOM snapshot shim, limits, console capture
src/adapters/       strategy registry (generic-page, selectors, youtube, ...)
src/tasks.rs       bounded queue, concurrency permits, admission, watchdog
src/resources.rs   RSS sampling, soft/hard memory budgets
src/security.rs    URL policy, DNS re-checks, guarded fetch
conformance/       port of the Node test suite (fixture.mjs + conformance.test.mjs)
sdk/               JavaScript SDK and its integration test
bench/             benchmark client, fixtures and recorded results
```
