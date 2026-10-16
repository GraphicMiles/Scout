# Scout

Lightweight browser automation runtime with an authenticated HTTP API and a
JavaScript/TypeScript SDK. It fetches documents over HTTP, parses and queries the
DOM, fills and submits forms, extracts structured data, and runs page JavaScript in
an embedded QuickJS sandbox.

It is not a Chromium replacement, and it does not claim to be. See
[`scout/README.md`](scout/README.md) for the full capability table.

## Layout

| Path | What it is |
|---|---|
| **[`scout/`](scout/README.md)** | **The product.** The Rust runtime, its SDK, tests, benchmarks and deployment config. Start here. |
| `reference/node-base/` | The original Node implementation, kept as the **contract reference** for the conformance suite. Not a supported runtime: no JavaScript engine, and its YouTube adapter does not work against live pages. |

Only `scout/` is maintained. `reference/node-base/` exists so the port can be
verified against an independent implementation of the same contract.

## Quick start

```bash
cd scout
cargo build --release
SCOUT_API_KEYS=$(openssl rand -hex 24) ./target/release/scout
curl -s -H "authorization: Bearer $SCOUT_API_KEYS" http://127.0.0.1:8080/health
```

Container (image not yet built in CI — see the note in `scout/Dockerfile`):

```bash
cd scout
docker build -t scout . && docker run --memory=512m --cpus=0.1 -e SCOUT_API_KEYS=... -p 8080:8080 scout
```

## Verified status

Measured, not assumed — every figure below was reproduced by running the code:

- **Idle RSS 4.86 MB**, **peak 23.1 MB** on a real extraction task.
- **235 tests passing**: 103 Rust unit, 22 Rust API, 47 conformance, 17 SDK
  integration, plus 46 in the Node reference base.
- **YouTube adapter verified against live pages** — the only site adapter that is.
- **Facebook and TikTok are not implemented** and return `unsupported_capability`
  before any network or queue use. No compatibility is claimed for them.
- **Not yet benchmarked against Playwright/Chromium or Lightpanda.**

Unsupported capabilities are reported explicitly rather than returning partial or
fabricated data. `GET /v1/strategies` exposes the verification state of every
strategy.

Gap analysis against the original engineering brief: [`BRIEF_ASSESSMENT.md`](BRIEF_ASSESSMENT.md).
Implementation plan: [`PLAN.md`](PLAN.md).
