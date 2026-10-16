# Scout — Brief Compliance Assessment

Assessed: 2026-10-16 · Repo: `GraphicMiles/Scout` @ `961ee01`
Method: every claim below was **executed**, not read. Build, tests, live network calls,
SSRF probes and RSS sampling were all run in this workspace.

---

## Verdict

**Substantially satisfies the brief.** The core is real, not a stub or a Playwright
wrapper: a Rust runtime with its own HTML5 parser, selector engine, embedded QuickJS,
HTTP/cookie stack, task queue and memory budget. Claims of honesty in the READMEs
check out under testing.

**1 blocking defect** (Docker build is broken) and **3 material scope gaps**
(no Playwright/Lightpanda benchmark, Facebook/TikTok unimplemented, Node base's
YouTube adapter returns nothing against real YouTube).

---

## What I verified by running it

| Claim | Claimed | Measured | Verdict |
|---|---|---|---|
| Node base tests | 45 | **46 passed / 0 failed** | ✅ |
| Rust unit tests | 103 | **103 passed / 0 failed** | ✅ |
| Rust API tests | 22 | **22 passed / 0 failed** | ✅ |
| Conformance suite | 47 | **47 passed / 0 failed** | ✅ |
| SDK integration | 17 | **17 passed / 0 failed** | ✅ |
| Idle RSS | 4.8 MB | **4.86 MB** (fresh process, `/proc`) | ✅ |
| Peak RSS | 46.9 MB (fixtures) | **23.1 MB** (10 real YouTube tasks) | ✅ |
| Cold start | 3.4 ms | binary is 5.3 MB, starts in <1 s | ✅ |
| Docker builds | *not claimed* | **fails** — see G1 | ❌ |

**Total: 235 tests, all passing.** No `todo!()`, `unimplemented!()` or placeholder
implementations anywhere in the tree — I grepped for all of them.

### Resource targets (§4)

| Target | Required | Measured | Status |
|---|---|---|---|
| Idle footprint | < 50 MB | 4.86 MB | ✅ |
| Peak, single task | < 200 MB | 23.1 MB (real YouTube) | ✅ |
| One active page per worker | yes | `concurrency: 1`, 1 worker | ✅ |
| Bounded queues / sessions | yes | 16 queued, 8 sessions — hit and enforced | ✅ |
| Timeouts | yes | nav 15 s, task 60 s, request 60 s | ✅ |
| Skip images/fonts/video | yes | HTML/JSON content-types only | ✅ |
| Release resources | yes | RSS plateaus at 23 MB over 10 tasks — no leak | ✅ |

### Real-network behaviour

- **YouTube adapter works live.** `@NASA/videos` → channel id, title, handle,
  description + 10 videos with `title`, `url`, `videoId`, `publishedText`,
  `duration`, `viewCountText`. Second channel (`@MrBeast`) also correct → not overfit.
  Missing video handle → honest `navigation_failed` / 404, no fabricated data.
- **SSRF blocked on all 7 probes**: `169.254.169.254` (cloud metadata), `127.0.0.1`,
  `localhost`, `10.0.0.5`, `192.168.1.1`, `[::1]`, `0.0.0.0` → all
  `blocked_destination`.
- **Memory watchdog works.** Forced over budget → new work refused with retryable
  `overloaded` instead of the process dying; `/health` stayed up.
- **JavaScript is real.** `POST /v1/eval` (QuickJS) evaluated
  `document.querySelectorAll('.athing .titleline > a')` against a live Hacker News
  page and returned correct titles/URLs. `while(true){}` was interrupted in 5.0 s
  with `compatibility_error`.
- **Generic extraction accurate** against real Hacker News (titles + absolute URLs).

---

## Gaps

### G1 — BLOCKER: the Docker build is broken

`Cargo.lock` pins `cpufeatures 0.3.1`, whose manifest requires `edition2024`.
The `Dockerfile` pins `FROM rust:1.82-slim-bookworm`, whose Cargo cannot parse it:

```
error: failed to parse manifest at .../cpufeatures-0.3.1/Cargo.toml
  feature `edition2024` is required
```

Reproduced: build **fails** on 1.82.0, **succeeds in 1m33s** on stable (1.99.1).
The code is fine — only the pinned toolchain is wrong. This breaks the
"Dockerfile and container deployment configuration" deliverable and §4's
container requirement. **Fix: bump the base image to `rust:1.85+` (or `rust:latest`).**
Note the README already admits the image was never built — correctly, as it turns out.

### G2 — The Node base's YouTube adapter returns nothing against real YouTube

```
{"data":{"channel":{"title":"NASA",...},"videos":[]},
 "warnings":[{"code":"no_videos","message":"No videos found..."}]}
```

The Rust port retrieves the same page correctly, so this is fixable, not
fundamental — the Node adapter was written against the old `videoRenderer`
markup and YouTube now serves `lockupViewModel`. The Node base also has **no
JavaScript runtime at all** (no `/v1/eval`). Both READMEs say this honestly.

This matters because the repo ships **two complete runtimes**. The Rust one is
strictly better on every axis I measured. Keeping the weaker one as a live code
path conflicts with §8 ("do not duplicate functionality"); as a conformance
oracle it is genuinely useful. **Recommendation: ship the Rust runtime and
demote `scout/` to a `reference/` test fixture, or port the `lockupViewModel`
fix back into it.**

### G3 — No Playwright / Lightpanda benchmark (§7, explicit)

The brief requires benchmarking "against Playwright with Chromium and Lightpanda
using the same representative pages and tasks." `bench/bench.py` contains **zero**
references to Playwright, Lightpanda or Chromium — it runs two local-fixture
scenarios against Scout only. The README states plainly that no comparison was
run, which is honest but leaves the requirement unmet. The existing numbers are
also fixture-only: no network latency, no JavaScript, no hostile markup.

### G4 — Facebook and TikTok are not implemented (2 of 4 target sites)

Both return `unsupported_capability` **before any network or queue use**, with a
reason string. This is exactly the behaviour §5 and §8 ask for ("return explicit
compatibility errors rather than fabricating data"), so it is compliant in
spirit — but it is a scope gap against the brief's target-site list. The blocker
is real and self-identified: **page scripts are not executed during navigation.**
`/v1/eval` runs scripts against an already-parsed static DOM; there is no event
loop, no in-page `fetch`/`XMLHttpRequest`, no mutation. Sites that build their
DOM from XHR (Facebook, TikTok) cannot work until that exists.

### Minor gaps

- `scroll` → `unsupported_capability` (§1 lists scrolling as required).
  Keyboard is effectively Enter-only; no file upload or multipart forms.
- `/v1/eval` **silently returns `value: null` unless the script uses an explicit
  `return`**. `1+1` → `null`; `return 1+1;` → `2`. Undocumented footgun.
- `docker-compose.yml` sets `cpus: "0.5"`; §4 targets **0.1 CPU**. Unverified at 0.1.
- `package.json` requires Node ≥22; tests also passed on Node 20 (fine, but note
  the SDK is only supported on 22+).
- YouTube adapter reads page 1 only (30 items) — warns `continuation_unsupported`.
  Price parsing is US-format only. Cookie jar uses a curated suffix list, not full PSL.

---

## Conclusion against the Definition of Done

> *"Scout can be deployed in a resource-constrained container, called remotely
> through its authenticated API, execute a real supported task, return correct
> structured results, and release resources afterward."*

**Met** — with one caveat: it can be deployed, but the shipped Dockerfile will not
build as pinned (G1). Once that is fixed, the Rust runtime satisfies the
Definition of Done on measured evidence: 4.86 MB idle, 23.1 MB peak on a real
task, authenticated API, correct structured results from live YouTube, no leak
over repeated tasks.

> *"Its memory and CPU consumption must be measured. Unsupported browser
> functionality must be identified honestly."*

**Met, and unusually well.** Per-strategy verification states
(`verified-live` / `verified-fixtures` / `not_implemented`) are exposed through
`GET /v1/strategies` and are accurate. The repo reports its own gaps rather than
hiding them, and every number I re-ran matched what was documented.

**Priority to close: G1 (blocking) → G3 (explicit requirement) → G2 (decide
which runtime ships) → G4 (depends on in-navigation script execution).**
