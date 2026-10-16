# Scout — Next Plan & Implementation

Ordered by the brief's own priority: **Correctness → resource efficiency →
security → reliability → website compatibility → additional features.**
Everything below is scoped against findings in `BRIEF_ASSESSMENT.md`.

---

## P0 — Docker toolchain pin ✅ DONE

**The defect:** `Cargo.lock` resolves `rand 0.10.3` / `cpufeatures 0.3.1`
(need `edition2024`) and `cookie_store 0.22.1` / `time 0.3.55` / `icu_* 2.3.0`
(declare `rust-version = "1.88"`). The Dockerfile pinned `rust:1.82`.

**Investigation worth recording:** the obvious fix (bump to 1.85, the edition-2024
release) is **wrong**. Measured:

| Toolchain | Result |
|---|---|
| 1.82.0 (current pin) | ❌ `feature edition2024 is required` |
| **1.85.0** | ❌ `cookie_store@0.22.1 requires rustc 1.88.0` (+ `time`, `icu_*`) |
| **1.88.0** | ✅ `Finished release profile in 1m49s` |

**1.88 is the minimum.** Applied in two places, because the crate's own MSRV field
was also stale:

- `scout-rs/Cargo.toml` — `rust-version = "1.82"` → `"1.88"`
- `scout-rs/Dockerfile` — `FROM rust:1.82-slim-bookworm` → `rust:1.88-slim-bookworm`
  (tag confirmed to exist on Docker Hub, last updated 2025-07-23)

Also rewrote the Dockerfile's "never built" note to state precisely what *was*
verified (the compile step, at the locked dependency set) versus what was not (the
image).

**Residual risk:** there is no Docker daemon in this sandbox, so the image itself
remains unbuilt. The first `docker build` is still untested — most likely
survivors are CA-certificate or `cargo build --locked` network behaviour inside
the builder stage.

> **Next:** run `docker build -t scout . && docker run --memory=512m --cpus=0.1 …`
> on a machine with Docker and report the result. That closes the last blocker on
> the Definition of Done.

---

## P1 — Declare one runtime; demote the Node base

**Why:** the repo ships two complete runtimes. Measured, the Rust port is strictly
better on every axis — it has a real JS engine and a working live YouTube adapter;
the Node base has neither. Its YouTube adapter returns `videos: []` against real
YouTube because it only walks `videoRenderer ?? gridVideoRenderer` while YouTube
now serves `lockupViewModel`.

This is also a direct conflict with §8: *"Do not overengineer, duplicate
functionality…"*

**Two options — this is a decision for you:**

| Option | Work | Outcome |
|---|---|---|
| **A. Ship Rust only** (recommended) | Move `scout/` → `reference/node-base/`, keep it as the conformance oracle. Rust becomes `scout-rs/` → `scout/`. No new code. | G2 evaporates. One runtime to secure, benchmark and document. |
| **B. Keep both, fix Node** | Port the `lockupViewModel` walker from `scout-rs/src/adapters/youtube.rs` into `scout/src/adapters/youtube.js`, plus a live test. | Two runtimes to maintain; Node still has no JS engine, so it can never reach parity. |

**Recommendation: A.** B only makes sense if something external depends on the
Node build. Say the word and I'll do B instead — the raw fix is ~40 lines in
`parseYoutubeChannel`, and I can verify it live against `@NASA/videos` the same
way I verified the Rust one.

---

## P2 — Playwright / Lightpanda benchmark harness

**Why:** §7 explicitly requires benchmarking "against Playwright with Chromium and
Lightpanda using the same representative pages and tasks." Today `bench/bench.py`
contains **zero** references to either — it runs two local-fixture scenarios
against Scout alone. The existing numbers are also fixture-only: no network
latency, no JavaScript, no hostile markup.

**Implementation:**

1. Add `bench/compare.py` — one harness, three engines, identical scenarios:
   - `small-page` (1 KB list), `large-page` (8000 items, 0.8 MB),
   - `js-rendered` (DOM built by inline script — the case that actually
     differentiates the engines),
   - `form-interaction` (fill + submit),
   - one real public page.
2. Per engine, measure what §7 lists: idle RSS, peak RSS (sampled from `/proc`),
   CPU time, cold-start latency, time to first result, extraction accuracy,
   task completion rate, RSS across 100 repeated tasks.
3. Write `bench/results/comparison.json` + a table in the README, with the
   environment recorded (2 vCPU, no Docker, sandbox networking).
4. Keep the existing `bench.py` intact — it stays the Scout-only regression bench.

**Constraints I have to flag honestly:**
- Chromium (~400 MB) and the Lightpanda binary must be installed **outside**
  `/home/user` (this sandbox's `/tmp` is a 992 MB tmpfs and already filled once;
  I've moved toolchains to `/var/tmp`). Zero snapshot-budget impact.
- Numbers will be sandbox-specific. I'll document the host rather than let them
  read as universal.
- If Lightpanda can't run here, I'll ship the Playwright comparison and record
  Lightpanda as "not run, and why" — consistent with the repo's existing honesty.

---

## P3 — In-navigation JavaScript execution (unlocks Facebook / TikTok)

**Why:** this is the root dependency behind G4. `POST /v1/eval` runs scripts
against an *already-parsed* DOM (`src/scout.rs:478`). Nothing runs at navigation
time — no event loop, no in-page `fetch`/`XMLHttpRequest`, no mutation
(`src/js.rs:18`). Facebook and TikTok build their DOM from XHR, so they cannot
work until this exists. `src/worker.rs:264` currently emits `js_required_likely`
for exactly these pages.

**Implementation, in measured increments — each independently testable:**

1. **Script execution at navigation.** Collect inline + external `<script>` nodes,
   fetch external ones through the existing guarded client (SSRF policy already
   applies), execute in document order inside the QuickJS sandbox. Cap: wall-clock
   budget, memory budget, script count, total bytes.
2. **Timers.** `setTimeout`/`setInterval` currently *throw*. Implement a virtual
   clock driven off the navigation budget so deferred DOM writes are observed
   without blocking on real time.
3. **Network from inside the sandbox.** `fetch`/`XMLHttpRequest` backed by the
   guarded HTTP client, sharing the session cookie jar and the same SSRF checks.
   This is what pages actually use to load their content.
4. **DOM mutation writeback.** Today the snapshot is read-only. Make script
   mutations reflect into the DOM the extraction engine then queries.
5. **Only then** attempt `facebook-jobs`, `facebook-marketplace`, `tiktok-profile`
   — verifying each against real pages before flipping its status from
   `not_implemented` to `verified-live`.

**Hard constraint, restated because it shapes everything:** do not bypass
authentication, CAPTCHAs or anti-bot protections. If a site can't be served
legitimately, the adapter stays `not_implemented` with an honest reason. Success
here is measured, not assumed — a plausible outcome is that step 5 still returns
explicit compatibility errors, and that is an acceptable, honest result.

---

## P4 — Minor defects (cheap, fold into any of the above)

- **`/v1/eval` silently returns `value: null`** unless the script uses an explicit
  `return` — `1+1` → `null`, `return 1+1;` → `2`. Document the contract *and*
  return a warning when a non-`return` script yields `null`, so callers aren't
  debugging silence.
- `docker-compose.yml` sets `cpus: "0.5"`; §4 targets **0.1 CPU**. Measure at 0.1
  and either adjust the compose file or record the measured floor.
- `scroll` returns `unsupported_capability` though §1 lists scrolling; keyboard is
  Enter-only; no file upload or multipart forms. Keep them declared-unsupported,
  but make sure each is covered by a test asserting the explicit error.
- `scout.zip` at the repo root is a stale duplicate of the tree — delete it or
  generate it in CI.

---

## Suggested sequencing

| # | Item | Effort | Unblocks |
|---|---|---|---|
| P0 | Docker pin | **done** | Definition of Done (needs your `docker build` to confirm) |
| P1 | One runtime (needs your A/B call) | ~30 min | Removes duplication + the broken adapter |
| P4 | Minor defects | ~1 hr | Closes the small correctness gaps |
| P2 | Benchmark harness | ~half a day | The last explicit §7 requirement |
| P3 | In-navigation JS → FB/TikTok | multi-day, incremental | The two remaining target sites |

**My recommendation:** confirm P1 (A or B), then let me execute P1 + P4 together —
that clears every *correctness* gap except the Docker confirmation only you can
run. After that, P2 is the last explicit brief requirement that is currently
unmet, and P3 is the long pole.
